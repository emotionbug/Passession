use crate::{Result, anyhow, bail};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use super::{AcpTransformer, VendorUpdate};

/// Session-local reduction only: transport and notification ordering belong to the runtime.
#[derive(Default)]
pub(super) struct Mapping {
    pub modes: Value,
    pub options: Vec<Value>,
    pub replay: Option<bool>,
    pub config_transaction: bool,
    pub staged_config: Option<Value>,
    pub commands_received: bool,
    messages: BTreeMap<String, String>,
    fallback_chunk: Option<(&'static str, String)>,
    message_sequence: u64,
    tool_calls: BTreeMap<String, Value>,
    vendor_tool_calls: BTreeMap<String, Value>,
    pending_compactions: BTreeSet<String>,
}

impl Mapping {
    pub fn update(
        &mut self,
        update: &Value,
        session_id: &str,
        transformers: &[Arc<dyn AcpTransformer>],
    ) -> Result<Vec<Value>> {
        if self.replay == Some(false) {
            return Ok(Vec::new());
        }
        let kind = text(update, "sessionUpdate")?;
        let event = match kind {
            "agent_message_chunk" | "agent_thought_chunk" | "user_message_chunk" => {
                if kind == "user_message_chunk" && self.replay != Some(true) {
                    self.fallback_chunk = None;
                    return Ok(Vec::new());
                }
                let (kind, item_type) = match kind {
                    "agent_message_chunk" => ("agent_message_chunk", "assistant_message"),
                    "agent_thought_chunk" => ("agent_thought_chunk", "reasoning"),
                    _ => ("user_message_chunk", "user_message"),
                };
                // Even a non-text chunk ends the previous kind's fallback item.
                if self
                    .fallback_chunk
                    .as_ref()
                    .is_some_and(|(open, _)| *open != kind)
                {
                    self.fallback_chunk = None;
                }
                if update["content"]["type"] != "text" {
                    return Ok(Vec::new());
                }
                let chunk = text(&update["content"], "text")?;
                let explicit_id = update.get("messageId").and_then(Value::as_str);
                let id = if let Some(id) = explicit_id.filter(|id| !id.is_empty()) {
                    self.fallback_chunk = None;
                    id.to_owned()
                } else {
                    let (_, id) = self.fallback_chunk.get_or_insert_with(|| {
                        self.message_sequence += 1;
                        (kind, format!("{kind}:{}", self.message_sequence))
                    });
                    id.clone()
                };
                let content = self.messages.entry(id.clone()).or_default();
                content.push_str(chunk);
                let mut item = json!({"type":item_type,"id":id,"text":content});
                if let Some(id) =
                    explicit_id.filter(|id| item_type == "assistant_message" || !id.is_empty())
                {
                    if item_type != "reasoning" {
                        item["messageId"] = json!(id);
                    }
                }
                event("timeline.item", session_id, "item", item)
            }
            "tool_call" | "tool_call_update" => {
                if kind == "tool_call" {
                    self.fallback_chunk = None;
                }
                let id = text(update, "toolCallId")?;
                let mut snapshot = merge_tool_call(update, self.tool_calls.get(id));
                for transformer in transformers {
                    snapshot = transformer.tool_call(snapshot, session_id)?;
                }
                let id = text(&snapshot, "id")?.to_owned();
                let item = tool_item(&snapshot);
                self.tool_calls.insert(id, snapshot);
                event("timeline.item", session_id, "item", item)
            }
            "plan" => {
                self.fallback_chunk = None;
                let entries = array(update, "entries")?;
                let items: Vec<_> = entries
                    .iter()
                    .enumerate()
                    .map(|(index, entry)| {
                        json!({"id":index.to_string(),"text":entry["content"],
                        "completed":entry["status"] == "completed","status":entry["status"]})
                    })
                    .collect();
                event(
                    "timeline.item",
                    session_id,
                    "item",
                    json!({"type":"todo","id":"acp-plan","items":items}),
                )
            }
            "available_commands_update" => {
                let commands: Vec<_> = array(update, "availableCommands")?
                    .iter()
                    .map(|command| {
                        let mut value =
                            json!({"name":command["name"],"description":command["description"]});
                        copy_present(&mut value, "argumentHint", command["input"].get("hint"));
                        value
                    })
                    .collect();
                self.commands_received = true;
                event("session.commands", session_id, "commands", json!(commands))
            }
            "current_mode_update" => {
                if let Some(modes) = self.modes.as_object_mut() {
                    if let Some(mode) = update.get("currentModeId") {
                        modes.insert("currentModeId".into(), mode.clone());
                    }
                }
                return Ok(self.config_event(session_id));
            }
            "config_option_update" => {
                self.options = array(update, "configOptions")?.to_vec();
                return Ok(self.config_event(session_id));
            }
            "usage_update" => {
                let mut usage = json!({"contextWindowUsedTokens":update["used"],"contextWindowMaxTokens":update["size"]});
                if update["cost"]["currency"] == "USD" {
                    copy_present(&mut usage, "totalCostUsd", update["cost"].get("amount"));
                }
                event("session.usage", session_id, "usage", usage)
            }
            "compaction_update" => {
                let id = text(update, "compactionId")?;
                let completed = update["status"] == "completed";
                if completed {
                    self.pending_compactions.remove(id);
                } else {
                    self.pending_compactions.insert(id.to_owned());
                }
                event(
                    "timeline.item",
                    session_id,
                    "item",
                    json!({"type":"compaction","id":id,"status":if completed {"completed"} else {"loading"}}),
                )
            }
            _ => return Ok(Vec::new()),
        };
        Ok(vec![event])
    }

    pub fn vendor(&mut self, update: VendorUpdate, session_id: &str) -> Vec<Value> {
        let event = match update {
            VendorUpdate::Commands(commands) => {
                self.commands_received = true;
                event("session.commands", session_id, "commands", commands)
            }
            VendorUpdate::Config(config) => {
                if self.config_transaction {
                    self.staged_config = Some(config);
                    return Vec::new();
                }
                event("session.config", session_id, "config", config)
            }
            VendorUpdate::Timeline(item) => {
                if item["type"] == "tool_call" {
                    if let Some(id) = item["id"].as_str() {
                        if item["status"] == "running" {
                            self.vendor_tool_calls.insert(id.to_owned(), item.clone());
                        } else {
                            self.vendor_tool_calls.remove(id);
                        }
                    }
                }
                event("timeline.item", session_id, "item", item)
            }
            VendorUpdate::Notice(notice) => event("session.notice", session_id, "notice", notice),
        };
        vec![event]
    }

    pub fn finish_turn(&mut self, state: &str, session_id: &str) -> Vec<Value> {
        self.fallback_chunk = None;
        let mut events = Vec::new();
        for snapshot in self.tool_calls.values_mut() {
            if snapshot["status"] != "pending" && snapshot["status"] != "in_progress" {
                continue;
            }
            let item = terminal_tool_item(tool_item(snapshot), state);
            snapshot["status"] = json!(if state == "completed" {
                "completed"
            } else {
                "failed"
            });
            events.push(event("timeline.item", session_id, "item", item));
        }
        for (_, item) in std::mem::take(&mut self.vendor_tool_calls) {
            events.push(event(
                "timeline.item",
                session_id,
                "item",
                terminal_tool_item(item, state),
            ));
        }
        for id in std::mem::take(&mut self.pending_compactions) {
            events.push(event(
                "timeline.item",
                session_id,
                "item",
                json!({"type":"compaction","id":id,"status":"completed"}),
            ));
        }
        events
    }

    pub fn begin_turn(&mut self) {
        self.fallback_chunk = None;
    }

    pub fn config(&self) -> Value {
        let model = self
            .options
            .iter()
            .find(|option| option["category"] == "model");
        let thinking = self
            .options
            .iter()
            .find(|option| option["category"] == "thought_level");
        let modes: Vec<_> = self.modes["availableModes"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|mode| {
                let mut value = json!({"id":mode["id"],"label":mode["name"]});
                copy_present(
                    &mut value,
                    "description",
                    mode.get("description").filter(|v| !v.is_null()),
                );
                value
            })
            .collect();
        let settings: Vec<_> = self
            .options
            .iter()
            .filter(|option| option["category"] != "model" && option["category"] != "thought_level")
            .map(setting)
            .collect();
        let mut value = json!({"models":catalog_options(model),"modes":modes,
            "thinkingOptions":catalog_options(thinking),"settings":settings});
        copy_present(&mut value, "model", selected_value(model));
        copy_present(&mut value, "mode", self.modes.get("currentModeId"));
        copy_present(&mut value, "thinkingOption", selected_value(thinking));
        value
    }

    pub fn catalog(&self) -> Value {
        let Value::Object(mut config) = self.config() else {
            unreachable!()
        };
        config.remove("settings");
        for (source, target) in [
            ("model", "defaultModel"),
            ("mode", "defaultMode"),
            ("thinkingOption", "defaultThinkingOption"),
        ] {
            if let Some(value) = config.remove(source) {
                config.insert(target.into(), value);
            }
        }
        Value::Object(config)
    }

    pub fn values(&self) -> Value {
        Value::Object(
            self.options
                .iter()
                .filter_map(|option| {
                    Some((
                        option["id"].as_str()?.to_owned(),
                        option.get("currentValue")?.clone(),
                    ))
                })
                .collect(),
        )
    }

    fn config_event(&self, session_id: &str) -> Vec<Value> {
        if self.config_transaction {
            Vec::new()
        } else {
            vec![event("session.config", session_id, "config", self.config())]
        }
    }
}

fn event(kind: &str, session_id: &str, key: &str, payload: Value) -> Value {
    let mut value = json!({"type":kind,"sessionId":session_id});
    value[key] = payload;
    value
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("ACP {key} must be a string"))
}

fn array<'a>(value: &'a Value, key: &str) -> Result<&'a Vec<Value>> {
    value
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("ACP {key} must be an array"))
}

fn copy_present(target: &mut Value, key: &str, value: Option<&Value>) {
    if let Some(value) = value {
        target[key] = value.clone();
    }
}

fn first_text<'a>(values: impl IntoIterator<Item = Option<&'a Value>>) -> Option<&'a Value> {
    values.into_iter().flatten().find(|value| value.is_string())
}

fn merge_tool_call(update: &Value, current: Option<&Value>) -> Value {
    let previous = |key| current.and_then(|current| current.get(key));
    let mut value = json!({
        "id":update["toolCallId"],
        "title":first_text([update.get("title"), previous("title"), update.get("name")]).cloned().unwrap_or_else(|| json!("Tool call")),
        "status":update.get("status").or_else(|| previous("status")).filter(|v| !v.is_null()).cloned().unwrap_or_else(|| json!("pending")),
        // Explicit null clears input/output; only an absent key preserves the snapshot.
        "input":update.get("rawInput").or_else(|| previous("input")).unwrap_or(&Value::Null),
        "output":update.get("rawOutput").or_else(|| previous("output")).unwrap_or(&Value::Null),
        "locations":update.get("locations").and_then(Value::as_array).map(|locations| {
            Value::Array(locations.iter().map(|location| location["path"].clone()).collect())
        }).or_else(|| previous("locations").cloned()).unwrap_or_else(|| json!([])),
    });
    copy_present(
        &mut value,
        "name",
        first_text([update.get("name"), previous("name")]),
    );
    copy_present(
        &mut value,
        "kind",
        first_text([update.get("kind"), previous("kind")]),
    );
    value
}

fn tool_item(snapshot: &Value) -> Value {
    let input = &snapshot["input"];
    let edit = snapshot["kind"] == "edit"
        || snapshot["name"].as_str().is_some_and(|name| {
            name.as_bytes()
                .windows(4)
                .any(|part| part.eq_ignore_ascii_case(b"edit"))
        });
    let detail = if edit {
        let path = input
            .get("filePath")
            .filter(|value| value.is_string())
            .or_else(|| {
                snapshot["locations"]
                    .as_array()
                    .and_then(|locations| locations.first())
            });
        let mut detail =
            json!({"type":"edit","filePath":path.cloned().unwrap_or_else(|| json!(""))});
        for key in ["oldString", "newString", "unifiedDiff"] {
            copy_present(
                &mut detail,
                key,
                input.get(key).filter(|value| value.is_string()),
            );
        }
        detail
    } else {
        json!({"type":"unknown","input":snapshot["input"],"output":snapshot["output"]})
    };
    let status = match snapshot["status"].as_str() {
        Some("in_progress" | "pending") => "running",
        Some(status) => status,
        None => "running",
    };
    json!({"type":"tool_call","id":snapshot["id"],"callId":snapshot["id"],
        "name":snapshot.get("name").filter(|v| !v.is_null()).unwrap_or(&snapshot["title"]),
        "detail":detail,"status":status,"error":if status == "failed" { &snapshot["output"] } else { &Value::Null }})
}

fn terminal_tool_item(mut item: Value, state: &str) -> Value {
    item["status"] = json!(state);
    if state == "failed" {
        if item.get("error").is_none_or(Value::is_null) {
            item["error"] = json!({"message":"Turn failed"});
        }
    } else {
        item["error"] = Value::Null;
    }
    item
}

fn selected_value(option: Option<&Value>) -> Option<&Value> {
    option
        .filter(|option| option["type"] != "boolean")
        .and_then(|option| option.get("currentValue"))
}

fn select_options(option: Option<&Value>) -> Vec<Value> {
    let Some(option) = option.filter(|option| option["type"] != "boolean") else {
        return Vec::new();
    };
    let mut options = Vec::new();
    for candidate in option["options"].as_array().into_iter().flatten() {
        if candidate.get("group").is_some() {
            for nested in candidate["options"].as_array().into_iter().flatten() {
                options.push(json!({"label":nested["name"],"value":nested["value"]}));
            }
        } else {
            options.push(json!({"label":candidate["name"],"value":candidate["value"]}));
        }
    }
    options
}

fn catalog_options(option: Option<&Value>) -> Vec<Value> {
    select_options(option)
        .into_iter()
        .map(|mut option| {
            let value = option.as_object_mut().unwrap().remove("value").unwrap();
            option["id"] = value;
            option
        })
        .collect()
}

fn setting(option: &Value) -> Value {
    let boolean = option["type"] == "boolean";
    let mut value = json!({"type":if boolean {"toggle"} else {"select"},"id":option["id"],"label":option["name"]});
    copy_present(
        &mut value,
        "description",
        option.get("description").filter(|value| !value.is_null()),
    );
    copy_present(&mut value, "value", option.get("currentValue"));
    if !boolean {
        value["options"] = json!(select_options(Some(option)));
    }
    value
}

pub(super) fn prompt(prompt: &Value) -> Result<Vec<Value>> {
    let input = &prompt["input"];
    match text(input, "type")? {
        "command" => {
            let name = text(input, "name")?;
            let arguments = input.get("arguments").and_then(Value::as_str).unwrap_or("");
            Ok(vec![
                json!({"type":"text","text":if arguments.is_empty() {format!("/{name}")} else {format!("/{name} {arguments}")}}),
            ])
        }
        "message" => array(input, "content")?
            .iter()
            .map(|content| {
                if content["type"] == "text" || content["type"] == "image" {
                    Ok(content.clone())
                } else {
                    Ok(json!({"type":"text","text":serde_json::to_string(content)?}))
                }
            })
            .collect(),
        kind => bail!("Unsupported provider prompt input: {kind}"),
    }
}

pub(super) fn mcp_servers(config: &Value) -> Result<Vec<Value>> {
    let Some(servers) = config.get("mcpServers") else {
        return Ok(Vec::new());
    };
    let servers = servers
        .as_object()
        .ok_or_else(|| anyhow!("MCP servers must be an object"))?;
    servers.iter().map(|(name, server)| {
        if server["type"] == "stdio" {
            Ok(json!({"name":name,"command":text(server,"command")?,
                "args":server.get("args").filter(|value| !value.is_null()).cloned().unwrap_or_else(|| json!([])),
                "env":named_entries(server.get("env"))?}))
        } else {
            Ok(json!({"type":text(server,"type")?,"name":name,"url":text(server,"url")?,
                "headers":named_entries(server.get("headers"))?}))
        }
    }).collect()
}

fn named_entries(value: Option<&Value>) -> Result<Vec<Value>> {
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Object(values)) => Ok(values
            .iter()
            .map(|(name, value)| json!({"name":name,"value":value}))
            .collect()),
        Some(_) => bail!("MCP environment/headers must be an object"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(kind: &str, value: &str) -> Value {
        json!({"sessionUpdate":kind,"content":{"type":"text","text":value}})
    }

    fn reduce(mapping: &mut Mapping, update: Value) -> Value {
        mapping
            .update(&update, "boundary", &[])
            .unwrap()
            .pop()
            .unwrap()["item"]
            .clone()
    }

    #[test]
    fn fallback_chunks_accumulate_until_a_boundary() {
        let mut mapping = Mapping::default();
        let first = reduce(&mut mapping, chunk("agent_message_chunk", "hello"));
        let second = reduce(&mut mapping, chunk("agent_message_chunk", " world"));
        assert_eq!(first["id"], second["id"]);
        assert_eq!(second["text"], "hello world");
        assert!(second.get("messageId").is_none());
        reduce(&mut mapping, chunk("agent_thought_chunk", "thinking"));
        let third = reduce(&mut mapping, chunk("agent_message_chunk", "next"));
        assert_ne!(third["id"], first["id"]);
        reduce(
            &mut mapping,
            json!({"sessionUpdate":"tool_call","toolCallId":"tool","title":"Read"}),
        );
        let fourth = reduce(&mut mapping, chunk("agent_message_chunk", "after tool"));
        assert_ne!(fourth["id"], third["id"]);
        reduce(&mut mapping, json!({"sessionUpdate":"plan","entries":[]}));
        let fifth = reduce(&mut mapping, chunk("agent_message_chunk", "after plan"));
        assert_ne!(fifth["id"], fourth["id"]);
        mapping.begin_turn();
        let sixth = reduce(&mut mapping, chunk("agent_message_chunk", "new turn"));
        assert_ne!(sixth["id"], fifth["id"]);
        mapping.finish_turn("completed", "boundary");
        assert_ne!(
            reduce(&mut mapping, chunk("agent_message_chunk", "done"))["id"],
            sixth["id"]
        );
    }

    #[test]
    fn explicit_ids_and_non_text_chunks_end_fallback_items() {
        let mut mapping = Mapping::default();
        let first = reduce(&mut mapping, chunk("agent_message_chunk", "first"));
        let mut explicit = chunk("agent_message_chunk", "named");
        explicit["messageId"] = json!("native");
        let item = reduce(&mut mapping, explicit);
        assert_eq!(item["id"], "native");
        assert_eq!(item["messageId"], "native");
        let next = reduce(&mut mapping, chunk("agent_message_chunk", "next"));
        assert_ne!(first["id"], next["id"]);
        assert!(
            mapping
                .update(
                    &json!({"sessionUpdate":"agent_thought_chunk","content":{"type":"image"}}),
                    "boundary",
                    &[]
                )
                .unwrap()
                .is_empty()
        );
        assert_ne!(
            reduce(&mut mapping, chunk("agent_message_chunk", "last"))["id"],
            next["id"]
        );
    }

    #[test]
    fn only_replay_emits_historical_users_and_skip_ignores_everything() {
        let mut mapping = Mapping::default();
        assert!(
            mapping
                .update(&chunk("user_message_chunk", "live"), "boundary", &[])
                .unwrap()
                .is_empty()
        );
        mapping.replay = Some(true);
        let first = reduce(&mut mapping, chunk("user_message_chunk", "old"));
        let second = reduce(&mut mapping, chunk("user_message_chunk", " prompt"));
        assert_eq!(first["id"], second["id"]);
        assert_eq!(second["text"], "old prompt");
        assert_eq!(second["type"], "user_message");
        mapping.replay = Some(false);
        assert!(mapping.update(&json!({"sessionUpdate":"config_option_update","configOptions":[{"id":"ignored"}]}), "boundary", &[]).unwrap().is_empty());
        assert!(mapping.options.is_empty());
        assert!(
            mapping
                .update(&chunk("agent_message_chunk", "ignored"), "boundary", &[])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn partial_tool_updates_distinguish_missing_and_null() {
        let mut mapping = Mapping::default();
        reduce(
            &mut mapping,
            json!({"sessionUpdate":"tool_call","toolCallId":"t","name":"lookup","rawInput":{"query":"x"},"rawOutput":"old"}),
        );
        let item = reduce(
            &mut mapping,
            json!({"sessionUpdate":"tool_call_update","toolCallId":"t","status":"in_progress","rawOutput":null}),
        );
        assert_eq!(item["name"], "lookup");
        assert_eq!(item["detail"]["input"], json!({"query":"x"}));
        assert!(item["detail"]["output"].is_null());
        assert_eq!(item["status"], "running");
        let item = reduce(
            &mut mapping,
            json!({"sessionUpdate":"tool_call_update","toolCallId":"t","rawInput":null,"rawOutput":{"message":"bad"},"status":"failed"}),
        );
        assert!(item["detail"]["input"].is_null());
        assert_eq!(item["error"], json!({"message":"bad"}));
        assert!(mapping.finish_turn("completed", "boundary").is_empty());
    }

    struct EditTransformer;
    impl AcpTransformer for EditTransformer {
        fn tool_call(&self, mut snapshot: Value, session_id: &str) -> Result<Value> {
            assert_eq!(session_id, "boundary");
            snapshot["kind"] = json!("edit");
            snapshot["name"] = json!("vendor edit");
            Ok(snapshot)
        }
    }

    #[test]
    fn transformers_receive_merged_snapshots_and_edit_fields_are_optional() {
        let mut mapping = Mapping::default();
        let transformers: Vec<Arc<dyn AcpTransformer>> = vec![Arc::new(EditTransformer)];
        let event = mapping.update(&json!({"sessionUpdate":"tool_call","toolCallId":"edit","locations":[{"path":"file.rs"}],"rawInput":{"oldString":"before"}}), "boundary", &transformers).unwrap().remove(0);
        assert_eq!(event["sessionId"], "boundary");
        assert_eq!(event["item"]["name"], "vendor edit");
        assert_eq!(
            event["item"]["detail"],
            json!({"type":"edit","filePath":"file.rs","oldString":"before"})
        );
        let item = reduce(
            &mut mapping,
            json!({"sessionUpdate":"tool_call_update","toolCallId":"edit","rawInput":{"filePath":"other.rs","newString":"after"}}),
        );
        assert_eq!(
            item["detail"],
            json!({"type":"edit","filePath":"other.rs","newString":"after"})
        );
    }

    #[test]
    fn terminalization_closes_base_vendor_tools_and_compactions_once() {
        for state in ["completed", "failed", "canceled"] {
            let mut mapping = Mapping::default();
            reduce(
                &mut mapping,
                json!({"sessionUpdate":"tool_call","toolCallId":"base"}),
            );
            mapping.vendor(
                VendorUpdate::Timeline(
                    json!({"type":"tool_call","id":"vendor","status":"running","error":null}),
                ),
                "boundary",
            );
            reduce(
                &mut mapping,
                json!({"sessionUpdate":"compaction_update","compactionId":"compact","status":"started"}),
            );
            let events = mapping.finish_turn(state, "boundary");
            assert_eq!(events.len(), 3);
            for event in &events[..2] {
                assert_eq!(event["item"]["status"], state);
                assert_eq!(event["item"]["error"].is_null(), state != "failed");
            }
            assert_eq!(events[2]["item"]["status"], "completed");
            assert!(mapping.finish_turn(state, "boundary").is_empty());
        }
    }

    #[test]
    fn catalog_config_and_values_preserve_grouped_boolean_and_nullable_settings() {
        let mut mapping = Mapping::default();
        assert!(mapping.config().get("model").is_none());
        assert!(mapping.catalog().get("defaultMode").is_none());
        mapping.modes = json!({"currentModeId":"code","availableModes":[{"id":"code","name":"Code","description":null}]});
        mapping.options = vec![
            json!({"id":"model","name":"Model","category":"model","type":"select","currentValue":"m","options":[{"group":"g","options":[{"name":"Model M","value":"m"}]}]}),
            json!({"id":"thinking","name":"Thinking","category":"thought_level","type":"select","currentValue":"deep","options":[{"name":"Deep","value":"deep"}]}),
            json!({"id":"enabled","name":"Enabled","type":"boolean","currentValue":false,"description":null}),
            json!({"id":"optional","name":"Optional","type":"select","currentValue":null,"options":[]}),
        ];
        let config = mapping.config();
        assert_eq!(config["models"], json!([{"id":"m","label":"Model M"}]));
        assert_eq!(config["thinkingOption"], "deep");
        assert_eq!(
            config["settings"][0],
            json!({"type":"toggle","id":"enabled","label":"Enabled","value":false})
        );
        assert!(config["modes"][0].get("description").is_none());
        assert_eq!(config["settings"][1]["value"], Value::Null);
        assert_eq!(mapping.catalog()["defaultModel"], "m");
        assert!(mapping.catalog().get("settings").is_none());
        assert_eq!(
            mapping.values(),
            json!({"model":"m","thinking":"deep","enabled":false,"optional":null})
        );
    }

    #[test]
    fn config_transactions_stage_vendor_snapshots_without_emitting() {
        let mut mapping = Mapping::default();
        mapping.config_transaction = true;
        assert!(mapping.update(&json!({"sessionUpdate":"config_option_update","configOptions":[{"id":"x","type":"boolean","name":"X","currentValue":true}]}), "boundary", &[]).unwrap().is_empty());
        assert_eq!(mapping.values()["x"], true);
        let vendor = json!({"models":[],"modes":[],"settings":[],"thinkingOptions":[]});
        assert!(
            mapping
                .vendor(VendorUpdate::Config(vendor.clone()), "boundary")
                .is_empty()
        );
        assert_eq!(mapping.staged_config, Some(vendor.clone()));
        mapping.config_transaction = false;
        assert_eq!(
            mapping.vendor(VendorUpdate::Config(vendor.clone()), "boundary")[0]["config"],
            vendor
        );
        mapping.modes = json!({"currentModeId":"old","availableModes":[]});
        let events = mapping
            .update(
                &json!({"sessionUpdate":"current_mode_update","currentModeId":"new"}),
                "boundary",
                &[],
            )
            .unwrap();
        assert_eq!(events[0]["config"]["mode"], "new");
    }

    #[test]
    fn commands_usage_and_plans_use_provider_shapes() {
        let mut mapping = Mapping::default();
        let commands = mapping.update(&json!({"sessionUpdate":"available_commands_update","availableCommands":[{"name":"help","description":"Help"},{"name":"search","description":"Find","input":{"hint":"query"}}]}), "boundary", &[]).unwrap();
        assert!(mapping.commands_received);
        assert!(commands[0]["commands"][0].get("argumentHint").is_none());
        assert_eq!(commands[0]["commands"][1]["argumentHint"], "query");
        for currency in ["USD", "EUR"] {
            let usage = mapping.update(&json!({"sessionUpdate":"usage_update","used":12,"size":100,"cost":{"currency":currency,"amount":0.5}}), "boundary", &[]).unwrap();
            assert_eq!(usage[0]["usage"]["contextWindowUsedTokens"], 12);
            assert_eq!(
                usage[0]["usage"].get("totalCostUsd").is_some(),
                currency == "USD"
            );
        }
        let plan = reduce(
            &mut mapping,
            json!({"sessionUpdate":"plan","entries":[{"content":"done","status":"completed"},{"content":"next","status":"in_progress"}]}),
        );
        assert_eq!(
            plan["items"],
            json!([{"id":"0","text":"done","completed":true,"status":"completed"},{"id":"1","text":"next","completed":false,"status":"in_progress"}])
        );
    }

    #[test]
    fn prompts_and_mcp_servers_keep_native_wire_shapes() {
        assert_eq!(
            prompt(&json!({"input":{"type":"command","name":"help","arguments":"topic"}})).unwrap(),
            vec![json!({"type":"text","text":"/help topic"})]
        );
        assert_eq!(
            prompt(&json!({"input":{"type":"command","name":"help","arguments":""}})).unwrap(),
            vec![json!({"type":"text","text":"/help"})]
        );
        let image = json!({"type":"image","mimeType":"image/png","data":"AA=="});
        let attachment = json!({"type":"attachment","id":"a"});
        let contents = prompt(&json!({"input":{"type":"message","content":[{"type":"text","text":"Hi"},image,attachment]}})).unwrap();
        assert_eq!(contents[1], image);
        assert_eq!(
            serde_json::from_str::<Value>(contents[2]["text"].as_str().unwrap()).unwrap(),
            attachment
        );
        let servers = mcp_servers(&json!({"mcpServers":{
            "local":{"type":"stdio","command":"tool","env":{"TOKEN":"secret"}},
            "remote":{"type":"http","url":"https://example.test/mcp","headers":{"Authorization":"Bearer token"}}
        }})).unwrap();
        assert_eq!(
            servers[0],
            json!({"name":"local","command":"tool","args":[],"env":[{"name":"TOKEN","value":"secret"}]})
        );
        assert_eq!(
            servers[1],
            json!({"type":"http","name":"remote","url":"https://example.test/mcp","headers":[{"name":"Authorization","value":"Bearer token"}]})
        );
    }
}
