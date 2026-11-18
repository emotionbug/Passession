use super::tools::{content_text, text};
use super::{array, first, put, string};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Default)]
pub struct Extensions {
    agents: HashMap<String, (String, bool)>,
    runs: HashMap<String, String>,
    async_runs: HashMap<String, AsyncRun>,
    seen_files: HashSet<String>,
    pending_files: HashMap<String, String>,
    children: Vec<Value>,
    announced_children: usize,
    states: HashMap<String, Value>,
    cards: HashMap<String, BTreeMap<usize, Card>>,
}

struct AsyncRun {
    owner: String,
    status_path: PathBuf,
    revision: Option<(SystemTime, u64)>,
    finished: bool,
    multi: bool,
}

impl AsyncRun {
    fn new(dir: &str, owner: &str) -> Self {
        Self {
            owner: owner.into(),
            status_path: Path::new(dir).join("status.json"),
            revision: None,
            finished: false,
            multi: false,
        }
    }

    fn changed_status(&mut self) -> Option<Value> {
        let metadata = std::fs::metadata(&self.status_path).ok()?;
        let revision = metadata.modified().ok().map(|time| (time, metadata.len()));
        if revision.is_some() && self.revision == revision {
            return None;
        }
        let bytes = std::fs::read(&self.status_path).ok()?;
        self.revision = revision;
        serde_json::from_slice(&bytes).ok()
    }
}

#[derive(Default)]
struct Card {
    description: Option<String>,
    session: Option<String>,
    lines: Vec<(String, String)>,
}
impl Extensions {
    pub fn drain_children(&mut self) -> Vec<Value> {
        self.announced_children = 0;
        std::mem::take(&mut self.children)
    }
    /// Publish a discovered transcript locator with the descriptor rather than
    /// emitting a second upsert for the same change when the watcher is attached.
    pub fn attach_child_sources(&mut self, events: &mut Vec<Value>) {
        for child in &self.children[self.announced_children..] {
            let id = &child["id"];
            let source = json!({"file":child["file"]});
            if let Some(event) = events
                .iter_mut()
                .rev()
                .find(|event| event["type"] == "upsert" && event["id"] == *id)
            {
                event["source"] = source;
            } else {
                events.push(json!({"type":"upsert","id":id,"source":source}));
            }
        }
        self.announced_children = self.children.len();
    }
    fn child(&mut self, id: &str, file: Option<&str>) {
        register_child(&mut self.seen_files, &mut self.children, id, file);
    }
    pub fn tool(
        &mut self,
        id: &str,
        name: &str,
        args: &Value,
        status: &str,
        result: &Value,
    ) -> Vec<Value> {
        let mut out = Vec::new();
        let d = &result["details"];
        if matches!(name, "Agent" | "subagent")
            && first(&[&args["subagent_type"]]).is_some()
            && first(&[&args["prompt"]]).is_some()
        {
            let tintin = name == "Agent";
            let title = string(&args["subagent_type"]);
            let description = args["description"]
                .as_str()
                .unwrap_or(string(&args["prompt"]));
            if status == "running" {
                out.push(json!({"type":"upsert","id":id,"title":title,"description":description,"toolCallId":id,"status":"running"}));
            } else if let (Some(agent), Some(state)) =
                (first(&[&d["agentId"]]), d["status"].as_str())
            {
                self.agents.insert(agent.into(), (id.into(), tintin));
                let file = output_file(result).or_else(|| self.pending_files.remove(agent));
                self.child(id, file.as_deref());
                out.push(json!({"type":"upsert","id":id,"title":title,"description":description,"toolCallId":id,"status":agent_status(state,tintin)}));
            } else if matches!(status, "failed" | "canceled") {
                out.push(json!({"type":"upsert","id":id,"status":status}));
            } else if !tintin {
                out.push(json!({"type":"remove","id":id}));
            }
        } else if name == "subagent"
            && args["action"].is_null()
            && first(&[&args["agent"]]).is_some()
            && first(&[&args["task"]]).is_some()
        {
            let base = json!({"type":"upsert","id":id,"title":args["agent"],"description":args["task"],"toolCallId":id,"status":"running"});
            if status == "running" {
                out.push(base);
            } else if d["mode"].is_string() && d["results"].is_array() && d["mode"] != "management"
            {
                if let Some(run) = d["runId"].as_str() {
                    self.runs.insert(run.into(), id.into());
                }
                if array(&d["results"]).is_empty()
                    && (d["asyncId"].is_string() || args["async"] == true)
                {
                    let mut v = base;
                    v["status"] = json!(if status == "failed" {
                        "failed"
                    } else if let Some(dir) = first(&[&d["asyncDir"]]) {
                        self.async_runs
                            .entry(dir.into())
                            .or_insert_with(|| AsyncRun::new(dir, id));
                        "running"
                    } else {
                        "canceled"
                    });
                    out.push(v);
                } else if array(&d["results"]).is_empty() {
                    out.push(json!({"type":"upsert","id":id,"status":if status == "failed" {"failed"} else {"canceled"}}));
                } else {
                    out.extend(self.rows(
                        id,
                        array(&d["results"]),
                        args["task"].as_str(),
                        status == "failed",
                    ));
                }
            } else if matches!(status, "failed" | "canceled") {
                out.push(json!({"type":"upsert","id":id,"status":status}));
            } else {
                out.push(json!({"type":"upsert","id":id,"status":"canceled"}));
            }
        } else if matches!(name, "get_subagent_result" | "steer_subagent") && status != "running" {
            if let Some((owner, tintin)) = self.agents.get(string(&args["agent_id"])).cloned()
                && let Some(state) = d["status"].as_str().filter(|_| d["agentId"].is_string())
            {
                if !tintin {
                    self.child(&owner, d["transcriptPath"].as_str());
                }
                out.push(json!({"type":"upsert","id":owner,"status":agent_status(state,tintin)}));
            }
        } else if name == "bg_wait" && status != "running" {
            for c in array(&d["completions"]) {
                if let Some(owner) = self.runs.get(string(&c["runId"])).cloned() {
                    out.extend(self.rows(&owner, array(&c["results"]), None, false));
                }
            }
        }
        out
    }
    pub fn followup_detail(&self, name: &str, args: &Value, result: &Value) -> Option<Value> {
        if !matches!(name, "get_subagent_result" | "steer_subagent")
            || !self.agents.contains_key(string(&args["agent_id"]))
        {
            return None;
        }
        let mut d = json!({"type":"sub_agent","log":text(result).unwrap_or_default().trim()});
        put(
            &mut d,
            "description",
            result["details"]["description"].as_str(),
        );
        Some(d)
    }
    fn rows(
        &mut self,
        owner: &str,
        rows: &[Value],
        description: Option<&str>,
        failed: bool,
    ) -> Vec<Value> {
        let mut out = Vec::new();
        if rows.len() > 1 {
            out.push(json!({"type":"remove","id":owner}));
        }
        for (index, row) in rows.iter().enumerate() {
            if first(&[&row["agent"]]).is_none() {
                continue;
            }
            let id = if rows.len() == 1 {
                owner.into()
            } else {
                format!("{owner}:{}", row["index"].as_u64().unwrap_or(index as u64))
            };
            let mut v = json!({"type":"upsert","id":id,"title":row["agent"],"toolCallId":owner,"status":if failed||row["success"]==false||(row["success"]!=true&&row["exitCode"]!=0){"failed"}else{"completed"}});
            put(&mut v, "description", description);
            self.child(&id, row["sessionFile"].as_str());
            out.push(v);
        }
        out
    }
    pub fn custom(&mut self, message: &Value) -> Vec<Value> {
        let mut out = Vec::new();
        let kind = string(&message["customType"]);
        if matches!(kind, "subagent-notification" | "subagent-update") {
            let d = &message["details"];
            let rows = std::iter::once(d).chain(array(&d["others"]));
            for row in rows {
                if let Some((owner, tintin)) = self.agents.get(string(&row["id"])).cloned() {
                    self.child(&owner, row["outputFile"].as_str());
                    let mut v = json!({"type":"upsert","id":owner,"status":agent_status(string(&row["status"]),tintin)});
                    put(&mut v, "description", row["description"].as_str());
                    out.push(v);
                }
            }
        } else if kind == "subagent-notify" {
            let body = content_text(&message["content"]);
            let header = body.lines().next().unwrap_or("");
            let status = if cached_regex!(r"^Background tasks completed \(\d+\):").is_match(header)
            {
                Some("completed")
            } else {
                cached_regex!(r"^(?:Background task|Detached foreground task) (completed|failed|stopped): \*\*").captures(header).map(|c|if &c[1]=="stopped"{"canceled"}else if &c[1]=="failed"{"failed"}else{"completed"})
            };
            if let Some(status) = status {
                // Prefer the per-step outcome and file, including the first
                // poll of a very short run that notified before the timer.
                out.extend(self.poll());
                let mut owner = None;
                for line in body.lines() {
                    if let Some(dir) = line.strip_prefix("Retention-managed async directory: ") {
                        owner = self.async_runs.get_mut(dir).and_then(|run| {
                            if run.multi {
                                return None;
                            }
                            if !run.finished {
                                out.push(json!({"type":"upsert","id":run.owner,"status":status}));
                                run.finished = true;
                            }
                            Some(run.owner.clone())
                        });
                    } else if let Some(file) = line.strip_prefix("Session file: ")
                        && let Some(id) = owner.take()
                    {
                        self.child(&id, Some(file));
                    }
                }
            }
        }
        out
    }
    pub fn notification(&mut self, message: &str) -> Option<Vec<Value>> {
        let raw = message.strip_prefix("PASEO_GOTGENES_CHILD_SESSION ")?;
        let Ok(v) = serde_json::from_str::<Value>(raw) else {
            return Some(Vec::new());
        };
        let (Some(agent), Some(file)) = (first(&[&v["agentId"]]), first(&[&v["file"]])) else {
            return Some(Vec::new());
        };
        if let Some((id, _)) = self.agents.get(agent).cloned() {
            self.child(&id, Some(file));
            Some(vec![json!({"type":"upsert","id":id,"status":"running"})])
        } else {
            self.pending_files.insert(agent.into(), file.into());
            Some(Vec::new())
        }
    }
    pub fn poll(&mut self) -> Vec<Value> {
        let mut out = Vec::new();
        for run in self.async_runs.values_mut().filter(|run| !run.finished) {
            let Some(v) = run.changed_status() else {
                continue;
            };
            if !v["state"].is_string() || !v["steps"].is_array() {
                continue;
            }
            let state = string(&v["state"]);
            let terminal = matches!(state, "complete" | "failed" | "stopped");
            let steps = array(&v["steps"]);
            let owner = &run.owner;
            if steps.len() > 1 && !run.multi {
                run.multi = true;
                out.push(json!({"type":"remove","id":owner}));
            }
            for (i, step) in steps.iter().enumerate() {
                if first(&[&step["agent"]]).is_none() {
                    continue;
                }
                let id = if !run.multi {
                    owner.clone()
                } else {
                    format!("{owner}:{i}")
                };
                let status = match string(&step["status"]) {
                    "stopped" | "canceled" => "canceled",
                    "failed" | "error" => "failed",
                    "complete" | "completed" => "completed",
                    _ if state == "stopped" => "canceled",
                    _ if state == "failed" => "failed",
                    _ if terminal => "completed",
                    _ => "running",
                };
                out.push(json!({"type":"upsert","id":id,"title":step["agent"],"toolCallId":owner,"status":status}));
                register_child(
                    &mut self.seen_files,
                    &mut self.children,
                    &id,
                    step["sessionFile"].as_str(),
                );
            }
            if terminal && steps.is_empty() && !run.multi {
                let status = match state {
                    "complete" => "completed",
                    "stopped" => "canceled",
                    _ => "failed",
                };
                out.push(json!({"type":"upsert","id":owner,"status":status}));
            }
            run.finished = terminal;
        }
        out
    }
    pub fn clear_async_runs(&mut self) {
        self.async_runs.clear();
    }
    pub fn omp_task(&mut self, owner: &str, result: &Value) -> Vec<Value> {
        let mut out = Vec::new();
        let details = &result["details"];
        for (rows, terminal) in [(&details["results"], true), (&details["progress"], false)] {
            for row in array(rows) {
                let Some(id) = first(&[&row["id"]]) else {
                    continue;
                };
                // A queued tool result may arrive after a newer native lifecycle event.
                // It discovers missing children, but cannot regress those already observed.
                if let Some(state) = self.states.get(id) {
                    if state["toolCallId"].as_str().is_some_and(|id| id != owner) {
                        self.states.remove(id);
                    } else if !terminal
                        || !matches!(state["status"].as_str(), Some("running" | "unobserved"))
                    {
                        continue;
                    }
                }
                let payload = json!({
                    "id":id,
                    "agent":first(&[&row["agent"]]).unwrap_or("OMP subagent"),
                    "parentToolCallId":owner,
                    "index":row["index"],
                    "description":row["description"],
                    "sessionFile":row["sessionFile"],
                    "status":super::omp_task_status(row, terminal)
                });
                if let Some(event) = self.omp_event("subagent_lifecycle", &payload) {
                    out.push(event);
                }
            }
        }
        out
    }
    pub fn omp_custom(&mut self, message: &Value) -> Vec<Value> {
        let mut out = Vec::new();
        for (id, status) in super::omp_task_completions(message) {
            let Some(state) = self.states.get_mut(id) else {
                continue;
            };
            // Native lifecycle is authoritative once terminal; a late delivery is
            // only needed when that lifecycle event was missed.
            if !matches!(state["status"].as_str(), Some("running" | "unobserved")) {
                continue;
            }
            state["status"] = json!(status);
            out.push(json!({"type":"upsert","id":id,"status":status}));
        }
        out
    }
    pub fn running_omp_children(&self) -> Vec<String> {
        self.states
            .iter()
            .filter(|(_, state)| state["status"] == "running")
            .map(|(id, _)| id.clone())
            .collect()
    }
    pub fn settle_missing_omp_children(
        &mut self,
        candidates: &[String],
        active: &HashSet<&str>,
    ) -> Vec<Value> {
        let mut out = Vec::new();
        for id in candidates.iter().filter(|id| !active.contains(id.as_str())) {
            if let Some(state) = self.states.get_mut(id)
                && state["status"] == "running"
            {
                // A late native result can still refine the unknown outcome.
                state["status"] = json!("unobserved");
                out.push(json!({"type":"upsert","id":id,"status":"canceled"}));
            }
        }
        out
    }
    pub fn omp_event(&mut self, kind: &str, payload: &Value) -> Option<Value> {
        let progress = kind == "subagent_progress";
        let id = if progress {
            string(&payload["progress"]["id"])
        } else {
            string(&payload["id"])
        };
        if id.is_empty() {
            return None;
        }
        // Queued progress cannot reopen a finished run. A fresh lifecycle
        // start can, including when OMP revives an existing subagent ID.
        if progress
            && super::omp_task_status(&payload["progress"], false) == "running"
            && self
                .states
                .get(id)
                .is_some_and(|state| state["status"] != "running")
        {
            return None;
        }
        let state=self.states.entry(id.into()).or_insert_with(||json!({"type":"upsert","id":id,"title":"OMP subagent","description":null,"toolCallId":null,"status":"running"}));
        if let Some(title) = first(&[&payload["agent"]]) {
            state["title"] = json!(title);
        }
        let data = if progress {
            &payload["progress"]
        } else {
            payload
        };
        if let Some(v) = data
            .get("description")
            .or_else(|| payload.get("assignment"))
        {
            state["description"] = v.clone();
        }
        if let Some(v) = payload.get("parentToolCallId") {
            state["toolCallId"] = v.clone();
        }
        let status = string(&data["status"]);
        state["status"] = json!(super::omp_task_status(data, false));
        if let Some(model) = first(&[&data["resolvedModel"]]) {
            state["resolvedModel"] = json!(model);
        }
        if let Some(file) = first(&[&payload["sessionFile"]]) {
            let title = state["title"].clone();
            state["source"] = json!({"path":file,"title":title});
        }
        let mut event = state.clone();
        if let Some(model) = first(&[&state["resolvedModel"]]) {
            let model = model.trim();
            let model = model
                .split_once('/')
                .filter(|(p, m)| !p.is_empty() && !m.is_empty())
                .map(|(p, m)| format!("{m} ({p})"))
                .unwrap_or_else(|| model.into());
            event["title"] = json!(format!("{} · {}", string(&state["title"]).trim(), model));
        }
        event.as_object_mut().unwrap().remove("resolvedModel");
        if let Some(parent) = first(&[&payload["parentToolCallId"]]) {
            let i = payload["index"].as_u64().unwrap_or(0) as usize;
            let card = self
                .cards
                .entry(parent.into())
                .or_default()
                .entry(i)
                .or_default();
            if let Some(s) = first(&[&data["description"]]) {
                card.description = Some(s.into());
            }
            if let Some(s) = first(&[&payload["sessionFile"]]) {
                card.session = Some(s.into());
            }
            if !progress {
                append(
                    card,
                    format!("lifecycle:{status}:{id}"),
                    format!("{id} {status}"),
                );
            } else {
                for (prefix, tool) in std::iter::once(("current", &data["currentTool"]))
                    .chain(array(&data["recentTools"]).iter().map(|v| ("tool", v)))
                {
                    if let Some(name) = first(&[&tool["tool"], &tool["name"]]) {
                        let args = first(&[&tool["args"]])
                            .map(str::to_owned)
                            .or_else(|| {
                                (!tool["input"].is_null()).then(|| {
                                    tool["input"]
                                        .as_str()
                                        .map(str::to_owned)
                                        .unwrap_or_else(|| tool["input"].to_string())
                                })
                            })
                            .unwrap_or_default();
                        append(
                            card,
                            format!(
                                "{prefix}:{name}:{args}{}",
                                tool.get("endMs")
                                    .map(|v| format!(":{v}"))
                                    .unwrap_or_default()
                            ),
                            if args.is_empty() {
                                format!("[{name}]")
                            } else {
                                format!("[{name}] {args}")
                            },
                        );
                    }
                }
                for line in array(&data["recentOutput"]) {
                    if let Some(s) =
                        first(&[line, &line["text"], &line["output"], &line["content"]])
                    {
                        append(card, format!("output:{s}"), s.into());
                    }
                }
                if status != "running" {
                    append(
                        card,
                        format!("status:{status}:{id}"),
                        format!("{id} {status}"),
                    );
                }
            }
        }
        Some(event)
    }
    pub fn card(&self, id: &str, detail: &mut Value) {
        if detail["type"] != "sub_agent" {
            return;
        }
        let Some(cards) = self.cards.get(id) else {
            return;
        };
        let Some((_, first)) = cards.first_key_value() else {
            return;
        };
        if detail["description"].is_null() {
            put(detail, "description", first.description.as_deref());
        }
        put(detail, "childSessionId", first.session.as_deref());
        let count = cards.keys().max().copied().unwrap_or(0) + 1;
        let lines: Vec<String> = cards
            .iter()
            .flat_map(|(i, c)| {
                c.lines.iter().map(move |(_, s)| {
                    if count > 1 {
                        format!("[{}/{}] {s}", i + 1, count)
                    } else {
                        s.clone()
                    }
                })
            })
            .collect();
        if !lines.is_empty() {
            detail["log"] = json!(lines.join("\n"));
        }
    }
    pub fn finish_card(&mut self, id: &str) {
        if !self
            .states
            .values()
            .any(|state| state["toolCallId"] == id && state["status"] == "running")
        {
            self.cards.remove(id);
        }
    }
    pub fn cancel(&mut self) -> Vec<Value> {
        let mut out = Vec::new();
        for state in self.states.values_mut() {
            if state["status"] == "running" {
                state["status"] = json!("canceled");
                let mut v = state.clone();
                v.as_object_mut().unwrap().remove("resolvedModel");
                out.push(v);
            }
        }
        self.cards.clear();
        out
    }
}

fn register_child(
    seen_files: &mut HashSet<String>,
    children: &mut Vec<Value>,
    id: &str,
    file: Option<&str>,
) {
    if let Some(file) = file.filter(|s| !s.trim().is_empty())
        && !seen_files.contains(file)
    {
        seen_files.insert(file.into());
        children.push(json!({"id":id,"file":file}));
    }
}
fn append(card: &mut Card, key: String, text: String) {
    if card.lines.iter().any(|(k, _)| k == &key) {
        return;
    }
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return;
    }
    let text = if text.chars().count() > 240 {
        format!("{}...", text.chars().take(240).collect::<String>())
    } else {
        text
    };
    card.lines.push((key, text));
    if card.lines.len() > 200 {
        card.lines.remove(0);
    }
}
fn agent_status(s: &str, tintin: bool) -> &str {
    match s {
        "completed" => "completed",
        "steered" if tintin => "completed",
        "error" => "failed",
        "aborted" | "stopped" => "canceled",
        _ => "running",
    }
}
fn output_file(result: &Value) -> Option<String> {
    text(result)?.lines().find_map(|l| {
        l.strip_prefix("Output file:")
            .map(str::trim)
            .filter(|s| !s.is_empty() && !s.chars().any(char::is_whitespace))
            .map(str::to_owned)
    })
}
pub fn todo(family: &str, name: &str, result: &Value) -> Option<Value> {
    let d = &result["details"];
    if family == "omp" {
        if name != "todo" {
            return None;
        }
        let phases = d["phases"].as_array()?;
        let tasks: Vec<Value> = phases
            .iter()
            .flat_map(|p| array(&p["tasks"]).iter().cloned())
            .collect();
        return omp_todos(&tasks);
    }
    if !d["error"].is_null() {
        return None;
    }
    if name == "todo" {
        if let Some(tasks) = d["tasks"].as_array() {
            let items:Vec<Value>=tasks.iter().filter(|t|t["status"]!="deleted").map(|t|{let mut item=json!({"id":t["id"].as_str().map(str::to_owned).unwrap_or_else(||t["id"].to_string()),"text":t["subject"],"status":t["status"],"completed":t["status"]=="completed"});put(&mut item,"activeForm",first(&[&t["activeForm"]]));item}).collect();
            return Some(json!({"type":"todo","items":items}));
        }
        if let Some(tasks) = d["todos"].as_array() {
            return Some(
                json!({"type":"todo","items":tasks.iter().map(|t|json!({"id":t["id"].to_string(),"text":t["text"],"status":if t["done"]==true{"completed"}else{"pending"},"completed":t["done"]})).collect::<Vec<_>>()}),
            );
        }
    }
    if matches!(
        name,
        "create_goal" | "get_goal" | "update_goal" | "set_goal_tasks" | "update_goal_task"
    ) && d["version"] == 3
        && d["goal"]["taskList"]["tasks"].is_array()
    {
        let mut items = Vec::new();
        goal_tasks(
            array(&d["goal"]["taskList"]["tasks"]),
            string(&d["goal"]["currentTaskId"]),
            &mut items,
        );
        return Some(json!({"type":"todo","items":items}));
    }
    None
}
fn goal_tasks(tasks: &[Value], current: &str, out: &mut Vec<Value>) {
    for t in tasks {
        out.push(json!({"id":t["id"],"text":t["title"],"status":if t["status"]!="pending"{"completed"}else if t["id"]==current{"in_progress"}else{"pending"},"completed":t["status"]!="pending"}));
        goal_tasks(array(&t["subtasks"]), current, out);
    }
}
pub fn omp_todos(tasks: &[Value]) -> Option<Value> {
    if tasks.is_empty() {
        return None;
    }
    Some(
        json!({"type":"todo","items":tasks.iter().filter(|t|t["status"]!="abandoned").map(|t|{let status=string(&t["status"]);let content=string(&t["content"]);json!({"text":if status=="blocked"{format!("{} ({})",content,first(&[&t["blocker"]]).map(|s|format!("blocked: {s}")).unwrap_or_else(||"blocked".into()))}else{content.into()},"status":if matches!(status,"completed"|"in_progress"){status}else{"pending"},"completed":status=="completed"})}).collect::<Vec<_>>()}),
    )
}
