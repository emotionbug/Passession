use crate::providers::mcp_elicitation::{self, Answer, Elicitation};
use anyhow::{Result, bail, ensure};
use serde_json::{Map, Value, json};
use std::{fmt::Write as _, sync::Arc};

#[derive(Clone, Debug)]
pub struct PendingPermission {
    pub id: String,
    pub native_id: Option<Value>,
    pub method: String,
    pub params: Value,
    pub request: Value,
    pub(super) elicitation: Option<Arc<Elicitation>>,
}

fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing Codex permission field {key}"))
}
fn nonempty(value: &Value) -> Option<&str> {
    value.as_str().map(str::trim).filter(|s| !s.is_empty())
}

pub fn normalize_questions(raw: &Value) -> Vec<Value> {
    raw.as_array()
        .into_iter()
        .flatten()
        .filter_map(|q| {
            let (id, header, question) = (
                nonempty(&q["id"])?,
                nonempty(&q["header"])?,
                nonempty(&q["question"])?,
            );
            let options: Vec<Value> = q["options"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|o| {
                    let mut option = json!({"label":nonempty(&o["label"])?});
                    if nonempty(&o["description"]).is_some() {
                        option["description"] = o["description"].clone();
                    }
                    Some(option)
                })
                .collect();
            let mut result = json!({"id":id,"header":header,"question":question,"options":options});
            for key in ["multiSelect", "isOther", "isSecret"] {
                if q[key] == true {
                    result[key] = json!(true);
                }
            }
            Some(result)
        })
        .collect()
}
fn format_questions(questions: &[Value]) -> String {
    questions
        .iter()
        .map(|q| {
            let mut text = format!(
                "{}: {}",
                q["header"].as_str().unwrap_or(""),
                q["question"].as_str().unwrap_or("")
            );
            let options: Vec<_> = q["options"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|o| o["label"].as_str())
                .collect();
            if !options.is_empty() {
                text.push_str(&format!("\nOptions: {}", options.join(", ")));
            }
            text
        })
        .collect::<Vec<_>>()
        .join("\n\n")
        .trim()
        .to_owned()
}

fn permission_path(path: &Value) -> Option<&str> {
    match path["type"].as_str()? {
        "path" => nonempty(&path["path"]),
        "glob_pattern" => nonempty(&path["pattern"]),
        "special" => match path["value"]["kind"].as_str()? {
            "root" => Some("filesystem root"),
            "minimal" => Some("minimal defaults"),
            "project_roots" => Some("project roots"),
            "tmpdir" => Some("system temporary directory"),
            "slash_tmp" => Some("/tmp"),
            "unknown" => nonempty(&path["value"]["path"]),
            _ => None,
        },
        _ => None,
    }
}

fn permission_profile_text(params: &Value) -> String {
    let profile = &params["permissions"];
    let mut text = String::new();
    if let Some(cwd) = nonempty(&params["cwd"]) {
        let _ = writeln!(text, "Working directory: {cwd}");
    }
    let start = text.len();
    if let Some(enabled) = profile["network"]["enabled"].as_bool() {
        let access = if enabled { "enabled" } else { "disabled" };
        let _ = writeln!(text, "Network access: {access}");
    }
    let fs = &profile["fileSystem"];
    for (key, label) in [("read", "Read"), ("write", "Write")] {
        for path in fs[key]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(nonempty)
        {
            let _ = writeln!(text, "{label}: {path}");
        }
    }
    for entry in fs["entries"].as_array().into_iter().flatten() {
        let path = &entry["path"];
        let (Some(location), Some(access)) = (permission_path(path), entry["access"].as_str())
        else {
            continue;
        };
        let label = match access {
            "read" => "Read",
            "write" => "Write",
            "deny" => "Deny",
            other => other,
        };
        if let Some(subpath) = nonempty(&path["value"]["subpath"]) {
            let _ = writeln!(text, "{label}: {location}/{subpath}");
        } else {
            let _ = writeln!(text, "{label}: {location}");
        }
    }
    if let Some(depth) = fs["globScanMaxDepth"].as_u64() {
        let _ = writeln!(text, "File glob scan depth: {depth}");
    }
    if text.len() == start {
        text.push_str("No additional permissions.");
    }
    text.truncate(text.trim_end().len());
    text
}

pub fn request(
    provider: &str,
    native_id: Value,
    method: &str,
    params: Value,
) -> Result<PendingPermission> {
    let elicitation;
    let mut req;
    let id;
    if method == "mcpServer/elicitation/request" {
        mcp_elicitation::required_text(&params["threadId"], "Codex thread ID")?;
        ensure!(
            params["turnId"].is_null() || params["turnId"].is_string(),
            "Invalid Codex MCP turn ID"
        );
        let server = mcp_elicitation::required_text(&params["serverName"], "server name")?;
        let message = string(&params, "message")?;
        let parsed = match string(&params, "mode")? {
            mode @ ("form" | "openaiForm" | "openai/form") => {
                ensure!(params["url"].is_null() && params["elicitationId"].is_null(), "Unexpected URL fields in Codex MCP form");
                Elicitation::form(&params["requestedSchema"], &Value::Null, mode != "form")?
            }
            "url" => {
                ensure!(params["requestedSchema"].is_null(), "Unexpected schema in Codex MCP URL request");
                Elicitation::url(&params["url"], &params["elicitationId"])?
            }
            _ => bail!("Unsupported Codex MCP elicitation mode"),
        };
        id = format!("permission-{}", uuid::Uuid::new_v4());
        req = parsed.request(server, message);
        if params["_meta"]["codex_approval_kind"] == "mcp_tool_call" {
            let tool = mcp_elicitation::required_text(&params["_meta"]["tool_name"], "Codex tool name")?;
            let arguments = &params["_meta"]["tool_params"];
            ensure!(arguments.is_object(), "Missing Codex MCP tool arguments for approval");
            let description = format!("{message}\n\nTool: {tool}\nArguments: {arguments}");
            req["description"] = json!(description);
        }
        req["metadata"]["threadId"] = params["threadId"].clone();
        req["metadata"]["turnId"] = params["turnId"].clone();
        elicitation = Some(Arc::new(parsed));
    } else {
        elicitation = None;
        let item = string(&params, "itemId")?;
        string(&params, "threadId")?;
        string(&params, "turnId")?;
        id = format!(
            "permission-{}",
            if method == "item/commandExecution/requestApproval" {
                params["approvalId"].as_str().unwrap_or(item)
            } else {
                item
            }
        );
        let metadata =
            json!({"itemId":item,"threadId":params["threadId"],"turnId":params["turnId"]});
        req = match method {
            "item/commandExecution/requestApproval" => {
                let mut input = Map::new();
                for key in ["command", "cwd"] {
                    if let Some(s) = params[key].as_str() {
                        input.insert(key.into(), json!(s));
                    }
                }
                let command = params["command"].as_str().unwrap_or("");
                json!({"name":"CodexBash","kind":"tool","title":if command.is_empty() { "Run command".to_owned() } else { format!("Run command: {command}") },"input":input,"detail":{"type":"unknown","input":{"command":params["command"],"cwd":params["cwd"]},"output":null},"metadata":metadata})
            }
            "item/fileChange/requestApproval" => {
                json!({"name":"CodexFileChange","kind":"tool","title":"Apply file changes","detail":{"type":"unknown","input":{"reason":params["reason"]},"output":null},"metadata":metadata})
            }
            "item/permissions/requestApproval" => {
                ensure!(
                    params["permissions"].is_object(),
                    "Missing Codex permission profile"
                );
                json!({"name":"CodexPermissions","kind":"tool","title":"Grant additional permissions",
                    "input":{"permissions":params["permissions"],"cwd":params["cwd"]},"metadata":metadata,
                    "detail":{"type":"plain_text","label":"Requested permissions","text":permission_profile_text(&params)}})
            }
            "item/tool/requestUserInput" | "tool/requestUserInput" => {
                ensure!(params["questions"].is_array(), "Missing Codex questions");
                let questions = normalize_questions(&params["questions"]);
                let mut result = json!({"name":"request_user_input","kind":"question","title":"Question","detail":{"type":"plain_text","text":format_questions(&questions),"icon":"brain"},"input":{"questions":questions},"metadata":metadata});
                result["metadata"]["questions"] = result["input"]["questions"].clone();
                result
            }
            _ => bail!("Unknown Codex permission method {method}"),
        };
        if method == "item/commandExecution/requestApproval" {
            let mut item =
                json!({"type":"commandExecution","id":params["itemId"],"status":"inProgress"});
            for key in ["command", "cwd"] {
                if let Some(value) = params[key].as_str() {
                    item[key] = json!(value);
                }
            }
            if let Some(mapped) = super::projection::item(&item, false, params["cwd"].as_str()) {
                req["detail"] = mapped["detail"].clone();
            }
        }
        let reason = nonempty(&params["reason"]);
        let context = &params["networkApprovalContext"];
        if method == "item/commandExecution/requestApproval"
            && let (Some(host), Some(protocol)) =
                (nonempty(&context["host"]), nonempty(&context["protocol"]))
        {
            let destination = format!("Network destination: {host} ({protocol})");
            req["description"] = json!(match reason {
                Some(reason) => format!("{reason}\n{destination}"),
                None => destination,
            });
        } else if let Some(reason) = reason {
            req["description"] = json!(reason);
        }
        if let Some(actions) = approval_actions(method, &params) {
            req["actions"] = actions;
        }
    }
    req["id"] = json!(id);
    req["provider"] = json!(provider);
    let params = if elicitation.is_some() {
        // Keep correlation, not the original schema or its embedded preview assets.
        json!({"threadId":params["threadId"],"turnId":params["turnId"]})
    } else {
        params
    };
    Ok(PendingPermission {
        id,
        native_id: Some(native_id),
        method: method.into(),
        params,
        request: req,
        elicitation,
    })
}

/// Offer the session grant and any persistent rules proposed by Codex.
fn approval_actions(method: &str, params: &Value) -> Option<Value> {
    let session = match method {
        "item/commandExecution/requestApproval" => "Allow this command for this session",
        "item/fileChange/requestApproval" => "Allow changes to these files for this session",
        "item/permissions/requestApproval" => "Allow these permissions for this session",
        _ => return None,
    };
    let mut actions = vec![
        json!({"id":"decline","label":"Deny","behavior":"deny","variant":"danger","intent":"dismiss"}),
        json!({"id":"accept","label":"Allow","behavior":"allow","variant":"primary"}),
        json!({"id":"accept_session","label":session,"behavior":"allow","variant":"secondary"}),
    ];
    if method == "item/commandExecution/requestApproval" {
        if let Some(prefix) = execpolicy_amendment(params) {
            actions.push(json!({"id":"accept_always","label":format!("Always allow commands starting with `{}`", prefix.join(" ")),"behavior":"allow","variant":"secondary"}));
        }
        if let Some(amendments) = params["proposedNetworkPolicyAmendments"].as_array() {
            for (index, amendment) in amendments.iter().enumerate() {
                if let Some((action, host)) = network_amendment(amendment) {
                    actions.push(json!({
                        "id":format!("network_policy_{index}"),
                        "label":format!("Always {action} network access to {host}"),
                        "behavior":action,"variant":"secondary",
                    }));
                }
            }
        }
    }
    Some(json!(actions))
}
fn execpolicy_amendment(params: &Value) -> Option<Vec<&str>> {
    let prefix: Vec<&str> = params["proposedExecpolicyAmendment"]
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .collect();
    (!prefix.is_empty()).then_some(prefix)
}

fn network_amendment(amendment: &Value) -> Option<(&str, &str)> {
    let action = amendment["action"].as_str()?;
    if !matches!(action, "allow" | "deny") {
        return None;
    }
    Some((action, nonempty(&amendment["host"])?))
}

pub fn reply(pending: &PendingPermission, response: &Value) -> Result<Value> {
    let allow = response["behavior"] == "allow";
    let decision = if allow {
        "accept"
    } else if response["interrupt"] == true {
        "cancel"
    } else {
        "decline"
    };
    match pending.method.as_str() {
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
            if pending.method == "item/commandExecution/requestApproval"
                && response["interrupt"] != true
                && let Some(index) = response["selectedActionId"]
                    .as_str()
                    .and_then(|id| id.strip_prefix("network_policy_"))
                    .and_then(|index| index.parse::<usize>().ok())
                && let Some(amendment) = pending.params["proposedNetworkPolicyAmendments"].get(index)
                && let Some((action, _)) = network_amendment(amendment)
                && response["behavior"] == action
            {
                return Ok(json!({"decision":{"applyNetworkPolicyAmendment":{
                    "network_policy_amendment":amendment
                }}}));
            }
            let decision = match response["selectedActionId"].as_str().filter(|_| allow) {
                Some("accept_session") => json!("acceptForSession"),
                Some("accept_always") => match execpolicy_amendment(&pending.params) {
                    Some(prefix) => json!({"acceptWithExecpolicyAmendment":{"execpolicy_amendment":prefix}}),
                    None => json!(decision),
                },
                _ => json!(decision),
            };
            Ok(json!({ "decision": decision }))
        }
        "item/permissions/requestApproval" => Ok(if allow {
            let scope = if response["selectedActionId"] == "accept_session" {
                "session"
            } else {
                "turn"
            };
            json!({"permissions":pending.params["permissions"],"scope":scope})
        } else {
            deny(pending)
        }),
        "mcpServer/elicitation/request" => pending
            .elicitation
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Missing Codex MCP elicitation state"))?
            .answer(response)
            .map(|answer| answer.native(true)),
        "plan" => Ok(if allow { json!({"followUpPrompt":plan_prompt(pending.params["planText"].as_str().unwrap_or(""))}) } else { Value::Null }),
        "item/tool/requestUserInput" | "tool/requestUserInput" => {
            let questions = normalize_questions(&pending.params["questions"]);
            let mut answers = Map::new();
            if allow {
                for q in &questions {
                    let header = q["header"].as_str().unwrap_or("");
                    let labels: Vec<&str> = q["options"].as_array().into_iter().flatten().filter_map(|o| o["label"].as_str()).collect();
                    if let Some(answer) = crate::providers::question_answers::read(response, header, &labels, q["multiSelect"] == true) {
                        let values: Vec<String> = answer.options.into_iter().chain(answer.other).collect();
                        if !values.is_empty() { answers.insert(q["id"].as_str().unwrap_or("").into(),json!({"answers":values})); }
                    }
                }
            }
            Ok(json!({"answers":answers}))
        },
        _ => bail!("Unknown Codex permission method {}",pending.method),
    }
}

pub fn deny(pending: &PendingPermission) -> Value {
    match pending.method.as_str() {
        "mcpServer/elicitation/request" => Answer::cancel().native(true),
        "item/permissions/requestApproval" => json!({"permissions":{},"scope":"turn"}),
        "item/tool/requestUserInput" | "tool/requestUserInput" => json!({"answers":{}}),
        "plan" => Value::Null,
        _ => json!({"decision":"decline"}),
    }
}

pub fn plan_actions(resume_label: Option<&str>) -> Value {
    let mut actions = vec![
        json!({"id":"dismiss","label":"Dismiss","behavior":"deny","variant":"danger","intent":"dismiss"}),
        json!({"id":"implement","label":"Implement","behavior":"allow","variant":"primary","intent":"implement"}),
    ];
    if let Some(label) = resume_label.filter(|s| !s.is_empty()) {
        actions.push(json!({"id":"implement_resume","label":label,"behavior":"allow","variant":"secondary","intent":"implement_resume"}));
    }
    json!(actions)
}
pub fn plan(provider: &str, id: String, text: &str, turn_id: Option<&str>) -> PendingPermission {
    let request = json!({"id":id,"provider":provider,"name":"CodexPlanApproval","kind":"plan","title":"Plan","description":"Review the proposed plan before implementation starts.","input":{"plan":text},"actions":plan_actions(None),"metadata":{"planText":text,"source":"codex_plan_approval"}});
    PendingPermission {
        id,
        native_id: None,
        method: "plan".into(),
        params: json!({"planText":text,"turnId":turn_id}),
        request,
        elicitation: None,
    }
}
pub fn plan_prompt(text: &str) -> String {
    let normalized = text
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n");
    let normalized = normalized.trim();
    let prefix = "The user approved the plan. Implement it now. Do not restate or revise the plan unless blocked.";
    if normalized.is_empty() {
        format!("{prefix} Make the required code changes and verify them.")
    } else {
        format!(
            "{prefix}\n\nApproved plan:\n\n{normalized}\n\nCarry out the work, make the necessary code changes, and verify the result."
        )
    }
}

pub fn question_timeline(pending: &PendingPermission, response: Option<&Value>) -> Result<Value> {
    let questions = normalize_questions(&pending.params["questions"]);
    let mut text = format_questions(&questions);
    let mut metadata = json!({"questions":questions});
    let mut error = Value::Null;
    let status = if let Some(response) = response {
        if response["behavior"] == "allow" {
            let native = reply(pending, response)?;
            let answers: Map<String, Value> = native["answers"]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(k, v)| (k.clone(), v["answers"].clone()))
                .collect();
            if !answers.is_empty() {
                let formatted = answers
                    .iter()
                    .map(|(id, values)| {
                        format!(
                            "{id}: {}",
                            values
                                .as_array()
                                .into_iter()
                                .flatten()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                text = format!("{text}\n\nAnswers:\n\n{formatted}");
            }
            metadata["answers"] = json!(answers);
            "completed"
        } else if response["interrupt"] == true {
            "canceled"
        } else {
            error = json!({"message":response["message"].as_str().unwrap_or("Question dismissed")});
            "failed"
        }
    } else {
        "running"
    };
    Ok(
        json!({"type":"tool_call","callId":pending.params["itemId"],"name":"request_user_input","status":status,"error":error,"detail":{"type":"plain_text","text":text,"icon":"brain"},"metadata":metadata}),
    )
}

fn async_item(item: &Value) -> Option<Value> {
    if item["type"] != "agentMessage"
        || item["delivery"] != "async"
        || item["id"].as_str()?.is_empty()
    {
        return None;
    }
    let raw = item["questions"].as_array()?;
    if raw.is_empty() {
        return None;
    }
    let mut questions = Vec::new();
    for q in raw {
        let title = nonempty(&q["title"])?;
        let mut question = json!({"title":title});
        if let Some(options) = q.get("options") {
            if !options.is_null()
                && !options
                    .as_array()?
                    .iter()
                    .all(|s| s.as_str().is_some_and(|s| !s.is_empty()))
            {
                return None;
            }
            question["options"] = options.clone();
        }
        questions.push(question);
    }
    Some(json!({"type":"agentMessage","id":item["id"],"delivery":"async","questions":questions}))
}
fn async_permission(provider: &str, record: &Value) -> Value {
    let questions: Vec<_> = record["item"]["questions"].as_array().into_iter().flatten().enumerate().map(|(i,q)|json!({"id":i.to_string(),"header":format!("Question {}",i+1),"question":q["title"],"options":q["options"].as_array().into_iter().flatten().map(|label|json!({"label":label})).collect::<Vec<_>>(),"isOther":true})).collect();
    json!({"id":format!("permission-{}",record["item"]["id"].as_str().unwrap_or("")),"provider":provider,"name":"request_user_input_async","kind":"question","title":"Question","input":{"questions":questions}})
}
fn async_timeline(record: &Value) -> Value {
    let answers = record["resolution"].as_array();
    let mut text = record["item"]["questions"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(i, q)| {
            let answer = if let Some(answers) = answers {
                answers
                    .get(i)
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned()
            } else {
                q["options"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let title = q["title"].as_str().unwrap_or("");
            if answer.is_empty() {
                title.to_owned()
            } else {
                format!("{title}\n{answer}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    if record["resolution"] == "dismissed" {
        text.push_str("\n\nDismissed");
    }
    json!({"type":"tool_call","callId":record["item"]["id"],"name":"request_user_input_async","status":"completed","error":null,"detail":{"type":"plain_text","icon":"brain","text":text}})
}
#[derive(Clone, Debug, Default)]
pub struct AsyncQuestions {
    records: Vec<Value>,
}
#[derive(Clone, Debug)]
pub struct PreparedAnswer {
    pub id: String,
    pub prompt: Option<String>,
    pub resolution: Value,
}
impl AsyncQuestions {
    pub fn new(saved: &Value) -> Self {
        let mut records = Vec::new();
        let Some(raw) = saved.as_array() else {
            return Self::default();
        };
        for record in raw {
            let Some(item) = async_item(&record["item"]) else {
                return Self::default();
            };
            let mut parsed = json!({"item":item});
            if let Some(resolution) = record.get("resolution") {
                if resolution != "dismissed"
                    && !resolution
                        .as_array()
                        .is_some_and(|a| a.iter().all(Value::is_string))
                {
                    return Self::default();
                }
                parsed["resolution"] = resolution.clone();
            }
            if let Some(existing) = records
                .iter_mut()
                .find(|r: &&mut Value| r["item"]["id"] == parsed["item"]["id"])
            {
                *existing = parsed;
            } else {
                records.push(parsed);
            }
        }
        Self { records }
    }
    pub fn receive(&mut self, provider: &str, item: &Value) -> Option<Value> {
        let item = async_item(item)?;
        if self.records.iter().any(|r| r["item"]["id"] == item["id"]) {
            return None;
        }
        let record = json!({"item":item});
        let request = async_permission(provider, &record);
        self.records.push(record);
        Some(request)
    }
    pub fn pending(&self, provider: &str) -> Vec<Value> {
        self.records
            .iter()
            .filter(|r| r.get("resolution").is_none())
            .map(|r| async_permission(provider, r))
            .collect()
    }
    fn find(&self, id: &str) -> Option<&Value> {
        let item_id = id.strip_prefix("permission-")?;
        self.records.iter().find(|r| r["item"]["id"] == item_id)
    }
    pub fn has_pending(&self, id: &str) -> bool {
        self.find(id).is_some_and(|r| r.get("resolution").is_none())
    }
    pub fn prepare_response(&self, id: &str, response: &Value) -> Result<PreparedAnswer> {
        let record = self
            .find(id)
            .filter(|r| r.get("resolution").is_none())
            .ok_or_else(|| anyhow::anyhow!("Question is no longer pending"))?;
        let mut resolution = json!("dismissed");
        let mut prompt = None;
        if response["behavior"] == "allow" {
            let answers = response["updatedInput"]["answers"]
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("Expected question answers"))?;
            ensure!(
                answers.values().all(Value::is_string),
                "Expected string question answers"
            );
            let mut values = Vec::new();
            let mut sections = Vec::new();
            for (i, q) in record["item"]["questions"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
            {
                let key = format!("Question {}", i + 1);
                let answer = answers
                    .get(&key)
                    .and_then(nonempty)
                    .ok_or_else(|| anyhow::anyhow!("Answer {key} before submitting"))?;
                sections.push(format!("{}\n{answer}", q["title"].as_str().unwrap_or("")));
                values.push(answer);
            }
            resolution = json!(values);
            prompt = Some(format!(
                "Answers to your questions:\n\n{}",
                sections.join("\n\n")
            ));
        }
        Ok(PreparedAnswer {
            id: id.into(),
            prompt,
            resolution,
        })
    }
    /// Commit only after the active-turn steer succeeds; failed steering leaves it pending.
    pub fn complete(&mut self, prepared: PreparedAnswer) -> Result<Value> {
        ensure!(
            self.has_pending(&prepared.id),
            "Question is no longer pending"
        );
        let item_id = prepared.id.strip_prefix("permission-").unwrap_or("");
        let record = self
            .records
            .iter_mut()
            .find(|r| r["item"]["id"] == item_id)
            .ok_or_else(|| anyhow::anyhow!("Question is no longer pending"))?;
        record["resolution"] = prepared.resolution;
        Ok(async_timeline(record))
    }
    pub fn timeline(&self, item_id: &str) -> Option<Value> {
        self.records
            .iter()
            .find(|r| r["item"]["id"] == item_id)
            .map(async_timeline)
    }
    pub fn retain(&mut self, item_ids: &std::collections::HashSet<String>) -> Vec<String> {
        let mut removed = Vec::new();
        self.records.retain(|r| {
            let id = r["item"]["id"].as_str().unwrap_or("");
            if item_ids.contains(id) {
                return true;
            }
            if r.get("resolution").is_none() {
                removed.push(format!("permission-{id}"));
            }
            false
        });
        removed
    }
    pub fn serialize(&self) -> Value {
        json!(self.records)
    }
}

/// `outcome` is absent for the proposal, otherwise approved/rejected/canceled.
pub fn plan_timeline(pending: &PendingPermission, outcome: Option<&str>) -> Value {
    let call_id = pending.params["itemId"].as_str().unwrap_or(&pending.id);
    let mut item = json!({"type":"tool_call","callId":call_id,"name":"plan_approval","status":match outcome { None => "running", Some("canceled") => "canceled", _ => "completed" },"error":null,"detail":{"type":"plan","text":pending.params["planText"]}});
    if let Some(outcome) = outcome {
        item["metadata"] = if outcome == "canceled" {
            json!({})
        } else {
            json!({"approved":outcome == "approved"})
        };
    }
    item
}
