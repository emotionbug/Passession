use anyhow::{Result, anyhow, bail, ensure};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

pub(super) fn properties() -> Value {
    json!({
        "provider":{"type":"string"},
        "query":{"type":"string","description":"Case-insensitive substring of model ID or label. Cannot be combined with modelId."},
        "limit":{"type":"integer","minimum":1,"maximum":100,"default":20},
        "cursor":{"type":"object","properties":{
            "revision":{"type":"string"},
            "offset":{"type":"integer","minimum":0}
        },"required":["revision","offset"],"additionalProperties":false,"description":"Use nextCursor with the same provider/query. Cannot be combined with modelId."},
        "modelId":{"type":"string","description":"Exact case-sensitive selectable model ID. Returns one detailed row automatically; cannot be combined with query or cursor."},
        "details":{"type":"boolean","default":false,"description":"Include all model fields, sharing thinkingOptions through zero-based thinkingOptionsRef indexes into response-local thinkingOptionSets. Otherwise return only id and label."}
    })
}

struct Cursor<'a> {
    revision: &'a str,
    offset: usize,
}

pub(super) struct Request<'a> {
    pub(super) provider: &'a str,
    query: Option<&'a str>,
    limit: usize,
    cursor: Option<Cursor<'a>>,
    model_id: Option<&'a str>,
    details: bool,
}

impl<'a> Request<'a> {
    pub(super) fn parse(input: &'a Value) -> Result<Self> {
        let provider = super::string(input, "provider")?;
        let query = input
            .get("query")
            .map(|_| super::string(input, "query"))
            .transpose()?;
        let model_id = input
            .get("modelId")
            .map(|_| super::string(input, "modelId"))
            .transpose()?;
        ensure!(
            model_id.is_none() || (query.is_none() && input.get("cursor").is_none()),
            "modelId cannot be combined with query or cursor"
        );
        let limit = match input.get("limit") {
            None => 20,
            Some(value) => value
                .as_u64()
                .filter(|limit| (1..=100).contains(limit))
                .ok_or_else(|| anyhow!("limit must be an integer from 1 to 100"))?
                as usize,
        };
        let details = match input.get("details") {
            None => false,
            Some(value) => value
                .as_bool()
                .ok_or_else(|| anyhow!("details must be a boolean"))?,
        };
        let cursor = input
            .get("cursor")
            .map(|value| {
                super::strict_keys(value, &["revision", "offset"])?;
                let revision = super::string(value, "revision")?;
                let offset = value
                    .get("offset")
                    .and_then(Value::as_u64)
                    .and_then(|offset| usize::try_from(offset).ok())
                    .ok_or_else(|| anyhow!("cursor.offset must be a nonnegative integer"))?;
                Ok::<_, anyhow::Error>(Cursor { revision, offset })
            })
            .transpose()?;
        Ok(Self {
            provider,
            query,
            limit,
            cursor,
            model_id,
            details,
        })
    }

    pub(super) fn project(&self, entry: &Value, cancel: &CancellationToken) -> Result<Value> {
        let catalog = entry
            .get("models")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let query = self.query.map(str::to_lowercase);
        let offset = self.cursor.as_ref().map_or(0, |cursor| cursor.offset);
        let detailed = self.details || self.model_id.is_some();
        let page_limit = if self.model_id.is_some() {
            1
        } else {
            self.limit
        };
        let mut page = Vec::with_capacity(page_limit.min(catalog.len()));
        let mut total = 0;
        let mut hash = self.model_id.is_none().then(Sha256::new);
        if let Some(hash) = &mut hash {
            hash.update(b"paseo:model-catalog:v1");
            serde_json::to_writer(hash, &(self.provider, self.query))?;
        }
        for model in catalog {
            ensure!(!cancel.is_cancelled(), "Tool execution cancelled");
            if model.get("isSelectable") == Some(&Value::Bool(false)) {
                continue;
            }
            if let Some(id) = self.model_id {
                if model.get("id").and_then(Value::as_str) == Some(id) {
                    page.push(model);
                    total = 1;
                    break;
                }
                continue;
            }
            if let Some(query) = &query {
                let matches = ["id", "label"].iter().any(|key| {
                    model
                        .get(*key)
                        .and_then(Value::as_str)
                        .is_some_and(|text| contains_query(text, query))
                });
                if !matches {
                    continue;
                }
            }
            // Stream the matched catalog into the digest without cloning or buffering it.
            // Details and limit do not affect the revision, so pages may change their view/size.
            if let Some(hash) = &mut hash {
                serde_json::to_writer(hash, model)?;
            }
            if total >= offset && page.len() < self.limit {
                page.push(model);
            }
            total += 1;
        }
        if let Some(id) = self.model_id {
            ensure!(
                total == 1,
                "Model '{id}' is unavailable or not selectable for provider '{}'",
                self.provider
            );
        }
        let revision = hash.map(|hash| format!("{:x}", hash.finalize()));
        if let Some(cursor) = &self.cursor {
            ensure!(
                Some(cursor.revision) == revision.as_deref(),
                "Model catalog or provider/query changed; restart without cursor"
            );
            ensure!(
                offset <= total,
                "cursor.offset exceeds the matched model count"
            );
        }
        let next_offset = offset + page.len();
        let next_cursor = if next_offset < total {
            json!({"revision":revision,"offset":next_offset})
        } else {
            Value::Null
        };
        let mut sets: Vec<&Value> = Vec::new();
        let mut models = Vec::with_capacity(page.len());
        for model in page {
            ensure!(!cancel.is_cancelled(), "Tool execution cancelled");
            let object = model
                .as_object()
                .ok_or_else(|| anyhow!("Model catalog row must be an object"))?;
            let mut projected = Map::new();
            for (key, value) in object {
                if detailed {
                    if key == "thinkingOptions" {
                        // Equality, not a hash, determines sharing; null and [] remain distinct.
                        let index = match sets.iter().position(|existing| *existing == value) {
                            Some(index) => index,
                            None => {
                                sets.push(value);
                                sets.len() - 1
                            }
                        };
                        projected.insert("thinkingOptionsRef".to_owned(), json!(index));
                    } else {
                        projected.insert(key.clone(), value.clone());
                    }
                } else if key == "id" || key == "label" {
                    projected.insert(key.clone(), value.clone());
                }
            }
            models.push(Value::Object(projected));
        }
        if cancel.is_cancelled() {
            bail!("Tool execution cancelled");
        }
        let mut response = json!({"provider":self.provider,"total":total});
        response["models"] = Value::Array(models);
        response["nextCursor"] = next_cursor;
        if detailed {
            response["thinkingOptionSets"] = json!(sets);
        }
        Ok(response)
    }
}

fn contains_query(text: &str, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    if text.is_ascii() && query.is_ascii() {
        return text
            .as_bytes()
            .windows(query.len())
            .any(|window| window.eq_ignore_ascii_case(query.as_bytes()));
    }
    text.to_lowercase().contains(query)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(catalog: &Value, input: Value) -> Result<Value> {
        Request::parse(&input)?.project(catalog, &CancellationToken::new())
    }

    fn reconstruct(response: &Value) -> Vec<Value> {
        response["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|model| {
                let mut model = model.clone();
                if let Some(index) = model.as_object_mut().unwrap().remove("thinkingOptionsRef") {
                    model["thinkingOptions"] =
                        response["thinkingOptionSets"][index.as_u64().unwrap() as usize].clone();
                }
                model
            })
            .collect()
    }

    #[test]
    fn model_catalog_pages_reconstruct_all_fields_and_shared_options() {
        let models: Vec<Value> = (0..137).map(|index| json!({
            "id":format!("model-{index}"), "label":format!("Model {index}"),
            "isSelectable":true, "contextWindow":32768 + index,
            "thinkingOptions":[{"id":"off","label":"Off","metadata":null},{"id":"on","budget":index % 7}],
            "defaultThinkingOptionId":if index % 2 == 0 { "off" } else { "on" },
            "extension":{"nested":[null, true, {"number":index}]}
        })).collect();
        let catalog = json!({"models":models});
        let discovery = list(&catalog, json!({"provider":"omp"})).unwrap();
        assert!(discovery.get("thinkingOptionSets").is_none());
        let mut input = json!({"provider":"omp","details":true,"limit":13});
        let mut restored = Vec::new();
        loop {
            let response = list(&catalog, input.clone()).unwrap();
            assert_eq!(response["total"], models.len());
            assert!(response["models"].as_array().unwrap().len() <= 13);
            assert!(response["thinkingOptionSets"].as_array().unwrap().len() <= 7);
            restored.extend(reconstruct(&response));
            if response["nextCursor"].is_null() {
                break;
            }
            input["cursor"] = response["nextCursor"].clone();
        }
        assert_eq!(restored, models);

        let response = list(
            &catalog,
            json!({"provider":"omp","limit":100,"details":true}),
        )
        .unwrap();
        assert_eq!(response["models"].as_array().unwrap().len(), 100);
        assert_eq!(response["thinkingOptionSets"].as_array().unwrap().len(), 7);
        let second = list(
            &catalog,
            json!({"provider":"omp","limit":100,"details":true,"cursor":response["nextCursor"]}),
        )
        .unwrap();
        assert_eq!(second["models"].as_array().unwrap().len(), 37);
        assert!(second["nextCursor"].is_null());
    }

    #[test]
    fn model_catalog_preserves_absent_null_empty_and_defaults() {
        let catalog = json!({"models":[
            {"id":"absent","label":"Absent"},
            {"id":"null","label":"Null","thinkingOptions":null,"defaultThinkingOptionId":null},
            {"id":"empty","label":"Empty","thinkingOptions":[],"defaultThinkingOptionId":""},
            {"id":"empty-again","label":"Empty again","thinkingOptions":[]},
            {"id":"enabled","label":"Enabled","thinkingOptions":[{"id":"high","extra":{"a":1,"b":2}}],"defaultThinkingOptionId":"high"},
            {"id":"equal","label":"Equal","thinkingOptions":[{"extra":{"b":2,"a":1},"id":"high"}],"defaultThinkingOptionId":"high"}
        ]});
        let response = list(&catalog, json!({"provider":"omp","details":true})).unwrap();
        assert_eq!(json!(reconstruct(&response)), catalog["models"]);
        assert_eq!(response["thinkingOptionSets"].as_array().unwrap().len(), 3);
        assert!(response["models"][0].get("thinkingOptionsRef").is_none());
        assert_ne!(
            response["models"][1]["thinkingOptionsRef"],
            response["models"][2]["thinkingOptionsRef"]
        );
        assert_eq!(
            response["models"][2]["thinkingOptionsRef"],
            response["models"][3]["thinkingOptionsRef"]
        );
        assert_eq!(
            response["models"][4]["thinkingOptionsRef"],
            response["models"][5]["thinkingOptionsRef"]
        );
    }

    #[test]
    fn model_catalog_filters_and_exact_selection_precede_projection() {
        let catalog = json!({"models":[
            {"id":"Vendor/Alpha","label":"Fast","thinkingOptions":[{"id":"high"}],"defaultThinkingOptionId":"high"},
            {"id":"beta","label":"ALPHA label","thinkingOptions":[]},
            {"id":"hidden","label":"Alpha hidden","isSelectable":false},
            {"id":"unicode","label":"ÉCLAIR"}
        ]});
        let default = list(&catalog, json!({"provider":"omp"})).unwrap();
        assert_eq!(default["total"], 3);
        assert!(default.get("thinkingOptionSets").is_none());
        assert_eq!(
            default["models"][0],
            json!({"id":"Vendor/Alpha","label":"Fast"})
        );
        let first = list(
            &catalog,
            json!({"provider":"omp","query":"aLpHa","limit":1}),
        )
        .unwrap();
        assert_eq!(first["total"], 2);
        assert_eq!(
            first["models"],
            json!([{"id":"Vendor/Alpha","label":"Fast"}])
        );
        let last = list(
            &catalog,
            json!({"provider":"omp","query":"aLpHa","limit":1,"cursor":first["nextCursor"]}),
        )
        .unwrap();
        assert_eq!(last["models"], json!([{"id":"beta","label":"ALPHA label"}]));
        assert!(last["nextCursor"].is_null());
        let exact = list(
            &catalog,
            json!({"provider":"omp","modelId":"Vendor/Alpha","details":false}),
        )
        .unwrap();
        assert_eq!(exact["total"], 1);
        assert!(exact["nextCursor"].is_null());
        assert_eq!(reconstruct(&exact), vec![catalog["models"][0].clone()]);
        for id in ["vendor/alpha", "Vendor", "hidden", "missing"] {
            assert!(list(&catalog, json!({"provider":"omp","modelId":id})).is_err());
        }
        for conflict in [
            json!({"query":""}),
            json!({"cursor":{"revision":"x","offset":0}}),
        ] {
            let mut input = json!({"provider":"omp","modelId":"beta"});
            input
                .as_object_mut()
                .unwrap()
                .extend(conflict.as_object().unwrap().clone());
            assert!(list(&catalog, input).is_err());
        }
        let unicode = list(&catalog, json!({"provider":"omp","query":"écl"})).unwrap();
        assert_eq!(unicode["models"][0]["id"], "unicode");
        let no_matches = list(
            &catalog,
            json!({"provider":"omp","query":"does not exist","details":true}),
        )
        .unwrap();
        assert_eq!(no_matches["total"], 0);
        assert_eq!(no_matches["models"], json!([]));
        assert_eq!(no_matches["thinkingOptionSets"], json!([]));
        assert!(no_matches["nextCursor"].is_null());
    }

    #[test]
    fn model_catalog_cursor_pins_provider_query_contents_and_order() {
        let catalog = json!({"models":[
            {"id":"a","label":"A","thinkingOptions":[],"defaultThinkingOptionId":null},
            {"id":"b","label":"B","thinkingOptions":[]},
            {"id":"c","label":"C","thinkingOptions":[]}
        ]});
        let first = list(&catalog, json!({"provider":"omp","limit":1})).unwrap();
        let cursor = &first["nextCursor"];
        let input = json!({"provider":"omp","cursor":cursor,"limit":2,"details":true});
        let rest = list(&catalog, input.clone()).unwrap();
        assert_eq!(
            reconstruct(&rest),
            catalog["models"].as_array().unwrap()[1..]
        );
        assert!(rest["nextCursor"].is_null());
        for change in [
            json!({"provider":"pi"}),
            json!({"query":""}),
            json!({"query":"a"}),
        ] {
            let mut changed_input = input.clone();
            changed_input
                .as_object_mut()
                .unwrap()
                .extend(change.as_object().unwrap().clone());
            assert!(list(&catalog, changed_input).is_err());
        }
        let mut variants = Vec::new();
        let mut reordered = catalog.clone();
        reordered["models"].as_array_mut().unwrap().swap(0, 1);
        variants.push(reordered);
        for (key, value) in [
            ("label", json!("Renamed")),
            ("thinkingOptions", json!([{"id":"high"}])),
            ("defaultThinkingOptionId", json!("high")),
            ("isSelectable", json!(false)),
        ] {
            let mut changed = catalog.clone();
            changed["models"][0][key] = value;
            variants.push(changed);
        }
        let mut added = catalog.clone();
        added["models"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id":"d","label":"D"}));
        variants.push(added);
        for changed in variants {
            assert!(list(&changed, input.clone()).is_err());
        }
        let mut end = input.clone();
        end["cursor"]["offset"] = json!(3);
        assert_eq!(list(&catalog, end.clone()).unwrap()["models"], json!([]));
        end["cursor"]["offset"] = json!(4);
        assert!(list(&catalog, end).is_err());
    }

    #[test]
    fn model_catalog_validates_boundaries_and_cancellation() {
        let catalog = json!({"models":[]});
        for invalid in [
            json!({}),
            json!({"provider":false}),
            json!({"provider":"omp","limit":0}),
            json!({"provider":"omp","limit":101}),
            json!({"provider":"omp","limit":1.5}),
            json!({"provider":"omp","limit":null}),
            json!({"provider":"omp","query":false}),
            json!({"provider":"omp","modelId":null}),
            json!({"provider":"omp","details":"true"}),
            json!({"provider":"omp","cursor":null}),
            json!({"provider":"omp","cursor":{"offset":0}}),
            json!({"provider":"omp","cursor":{"revision":"x","offset":-1}}),
            json!({"provider":"omp","cursor":{"revision":"x","offset":0.5}}),
        ] {
            assert!(list(&catalog, invalid).is_err());
        }
        for limit in [1, 100] {
            let result = list(&catalog, json!({"provider":"omp","limit":limit})).unwrap();
            assert_eq!(result["total"], 0);
            assert!(result["nextCursor"].is_null());
        }
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(
            Request::parse(&json!({"provider":"omp"}))
                .unwrap()
                .project(&catalog, &cancel)
                .is_err()
        );
    }
}
