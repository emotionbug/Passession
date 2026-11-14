//! Ports of `stream/tool_call_display.dart` (protocol tool-call-display.ts,
//! paseo-tool-presentation/*, tool-call icon/file-path utils,
//! tool-calls/presentation.ts) and the data side of `tool_call_details.dart`.

use super::i18n::{t, ta};
use super::json::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;

// ---- summary-text.ts ----

const MAX_SUMMARY_CHARS: usize = 120;
const SCAN_WINDOW_CHARS: usize = MAX_SUMMARY_CHARS * 4;

/// Collapses whitespace and caps at 120 characters with a trailing ellipsis.
pub fn to_summary_text(value: &str) -> Option<String> {
    let chars: Vec<char> = value.chars().collect();
    let partial = chars.len() > SCAN_WINDOW_CHARS;
    let window: String = if partial { chars[..SCAN_WINDOW_CHARS].iter().collect() } else { value.to_owned() };
    let collapsed = window.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return None;
    }
    let count = collapsed.chars().count();
    if count <= MAX_SUMMARY_CHARS && !partial {
        return Some(collapsed);
    }
    let kept: String = collapsed.chars().take(MAX_SUMMARY_CHARS - 1).collect();
    Some(format!("{}…", kept.trim_end()))
}

// ---- path-utils.ts ----

pub fn strip_cwd_prefix(file_path: &str, cwd: Option<&str>) -> String {
    paseo_protocol::helpers::strip_cwd_prefix(file_path, cwd)
}

// ---- tool-name-normalization.ts ----

fn normalize_tool_name(name: &str) -> String {
    name.trim().to_lowercase()
}

fn is_speak_tool_name(name: &str) -> bool {
    let normalized = normalize_tool_name(name);
    let tokens: Vec<&str> = normalized
        .split(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit()))
        .filter(|t| !t.is_empty())
        .collect();
    tokens.last() == Some(&"speak")
}

pub fn is_paseo_tool_name(name: &str) -> bool {
    paseo_tool_leaf_name(name).is_some()
}

pub fn paseo_tool_leaf_name(name: &str) -> Option<String> {
    let normalized = normalize_tool_name(name);
    if is_speak_tool_name(&normalized) {
        return None;
    }
    if normalized.contains("__") {
        let segments: Vec<&str> = normalized.split("__").filter(|s| !s.is_empty()).collect();
        if segments.len() >= 3 && segments[0] == "mcp" && (segments[1] == "paseo" || segments[1].starts_with("paseo_")) {
            return Some(segments[2..].join("__"));
        }
        return None;
    }
    if normalized.contains('.') {
        let parts: Vec<&str> = normalized.split('.').collect();
        if parts[0] == "paseo" || parts[0].starts_with("paseo_") {
            return Some(parts[1..].join("."));
        }
    }
    None
}

// ---- paseo-tool-presentation/catalog.ts ----

#[derive(Clone, Copy)]
enum Context {
    None,
    Field(&'static [&'static str]),
    TargetAgent,
    FieldOrTargetAgent(&'static str),
    Workspace,
    Viewport,
}

struct PaseoEntry {
    context: Context,
    prompt_field: Option<&'static str>,
    input_order: &'static [&'static str],
    output_fields: Option<&'static [&'static str]>,
}

const WORKSPACE_FIELDS: &[&str] = &["title", "workspaceId", "projectId", "isolation", "path", "mode", "worktreeSlug", "branchName", "baseBranch", "branch", "prNumber", "forge"];
const AGENT_FIELDS: &[&str] = &["title", "agentId", "provider", "workspaceId", "cwd", "sessionMode", "modeId", "background", "notifyOnFinish", "internal", "settings", "labels"];
const AUTOMATION_FIELDS: &[&str] = &["name", "id", "cron", "timezone", "provider", "cwd", "isolation", "maxRuns", "expiresIn", "clearExpires"];
const BROWSER_FIELDS: &[&str] = &["browserId", "url", "ref", "sourceRef", "targetRef", "value", "text", "key", "button", "doubleClick", "modifiers", "filePaths", "fullPage", "maxEntries", "timeoutMs", "deltaX", "deltaY", "width", "height", "function"];
const AUTOMATION_OUTPUT: &[&str] = &["id", "status", "nextRunAt", "expiresAt"];

/// A catalogued Paseo tool; its title is `agentStream.paseoTools.<leaf>`.
const fn entry(context: Context, prompt_field: Option<&'static str>, input_order: &'static [&'static str], output_fields: Option<&'static [&'static str]>) -> PaseoEntry {
    PaseoEntry { context, prompt_field, input_order, output_fields }
}

const fn browser(context: Context) -> PaseoEntry {
    entry(context, None, BROWSER_FIELDS, None)
}

fn paseo_catalog(leaf: &str) -> Option<PaseoEntry> {
    use Context::*;
    Some(match leaf {
        "create_agent" => entry(Field(&["title", "initialPrompt"]), Some("initialPrompt"), AGENT_FIELDS, Some(&["agentId", "status", "currentModeId", "cwd"])),
        "send_agent_prompt" => entry(Field(&["prompt"]), Some("prompt"), AGENT_FIELDS, Some(&["status", "lastMessage", "permission"])),
        "get_agent_status" => entry(TargetAgent, Option::None, &["agentId"], Option::None),
        "get_agent_activity" => entry(TargetAgent, Option::None, &["agentId", "limit"], Option::None),
        "list_agents" => entry(None, Option::None, &["cwd", "statuses", "sinceHours", "limit", "includeArchived"], Option::None),
        "update_agent" => entry(FieldOrTargetAgent("name"), Option::None, AGENT_FIELDS, Option::None),
        "set_agent_mode" => entry(Field(&["modeId"]), Option::None, &["agentId", "modeId"], Option::None),
        "cancel_agent" => entry(TargetAgent, Option::None, &["agentId"], Option::None),
        "archive_agent" => entry(TargetAgent, Option::None, &["agentId"], Option::None),
        "kill_agent" => entry(TargetAgent, Option::None, &["agentId"], Option::None),
        "list_pending_permissions" => entry(None, Option::None, &[], Option::None),
        "respond_to_permission" => entry(TargetAgent, Option::None, &["agentId", "requestId", "response"], Option::None),
        "create_workspace" => entry(Workspace, Option::None, WORKSPACE_FIELDS, Some(&["workspaceId", "projectId"])),
        "list_hosts" => entry(None, Option::None, &[], Option::None),
        "list_workspaces" => entry(None, Option::None, &[], Option::None),
        "rename_workspace" => entry(Field(&["title"]), Option::None, &["title", "workspaceId"], Option::None),
        "archive_workspace" => entry(Field(&["workspaceId"]), Option::None, &["workspaceId"], Some(&["workspaceId", "archivedAgentIds", "removedDirectory"])),
        "list_workspace_scripts" => entry(None, Option::None, &["workspaceId"], Option::None),
        "start_workspace_script" => entry(Field(&["scriptName"]), Option::None, &["workspaceId", "scriptName"], Option::None),
        "stop_workspace_script" => entry(Field(&["scriptName"]), Option::None, &["workspaceId", "scriptName"], Option::None),
        "create_terminal" => entry(Field(&["name", "command"]), Option::None, &["cwd", "workspaceId", "name", "command"], Option::None),
        "list_terminals" => entry(None, Option::None, &["cwd", "all"], Option::None),
        "capture_terminal" => entry(Field(&["terminalId"]), Option::None, &["terminalId", "lines"], Option::None),
        "send_terminal_keys" => entry(Field(&["keys"]), Option::None, &["terminalId", "keys", "literal"], Option::None),
        "kill_terminal" => entry(Field(&["terminalId"]), Option::None, &["terminalId"], Option::None),
        "create_schedule" => entry(Field(&["name", "cron"]), Some("prompt"), AUTOMATION_FIELDS, Some(AUTOMATION_OUTPUT)),
        "create_heartbeat" => entry(Field(&["name", "cron"]), Some("prompt"), AUTOMATION_FIELDS, Some(AUTOMATION_OUTPUT)),
        "update_schedule" => entry(Field(&["name", "id"]), Some("prompt"), AUTOMATION_FIELDS, Option::None),
        "list_schedules" => entry(None, Option::None, &[], Option::None),
        "inspect_schedule" => entry(Field(&["id"]), Option::None, &["id"], Option::None),
        "schedule_logs" => entry(Field(&["id"]), Option::None, &["id"], Option::None),
        "run_schedule_once" => entry(Field(&["id"]), Option::None, &["id"], Option::None),
        "pause_schedule" => entry(Field(&["id"]), Option::None, &["id"], Option::None),
        "resume_schedule" => entry(Field(&["id"]), Option::None, &["id"], Option::None),
        "delete_schedule" => entry(Field(&["id"]), Option::None, &["id"], Option::None),
        "delete_heartbeat" => entry(Field(&["id"]), Option::None, &["id"], Option::None),
        "list_providers" => entry(None, Option::None, &[], Option::None),
        "list_models" => entry(Field(&["provider"]), Option::None, &["provider"], Option::None),
        "list_profiles" => entry(None, Option::None, &[], Option::None),
        "inspect_provider" => entry(Field(&["provider"]), Option::None, &["provider", "cwd", "settings"], Option::None),
        "browser_list_tabs" => browser(None),
        "browser_new_tab" => browser(Field(&["url"])),
        "browser_close_tab" => browser(Field(&["browserId"])),
        "browser_navigate" => browser(Field(&["url"])),
        "browser_back" => browser(None),
        "browser_forward" => browser(None),
        "browser_reload" => browser(None),
        "browser_snapshot" => browser(None),
        "browser_screenshot" => browser(None),
        "browser_click" => browser(Field(&["ref"])),
        "browser_hover" => browser(Field(&["ref"])),
        "browser_drag" => browser(Field(&["sourceRef"])),
        "browser_fill" => browser(Field(&["ref"])),
        "browser_select" => browser(Field(&["value", "ref"])),
        "browser_type" => browser(Field(&["text"])),
        "browser_keypress" => browser(Field(&["key"])),
        "browser_upload" => browser(Field(&["filePaths"])),
        "browser_wait" => browser(Field(&["text", "url"])),
        "browser_scroll" => browser(None),
        "browser_resize" => browser(Viewport),
        "browser_logs" => browser(None),
        "browser_evaluate" => browser(Field(&["function"])),
        _ => return Option::None,
    })
}

fn is_present(value: &Value) -> bool {
    match value {
        Value::String(s) => !s.trim().is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Number(_) => true,
        _ => false,
    }
}

fn apply_context(context: Context, input: &Value, target: Option<&str>) -> Value {
    match context {
        Context::None => Value::Null,
        Context::Field(keys) => keys.iter().map(|k| get(input, k)).find(|v| is_present(v)).cloned().unwrap_or(Value::Null),
        Context::TargetAgent => target.map(|t| json!(t)).unwrap_or(Value::Null),
        Context::FieldOrTargetAgent(key) => {
            if is_present(get(input, key)) {
                get(input, key).clone()
            } else {
                target.map(|t| json!(t)).unwrap_or(Value::Null)
            }
        }
        Context::Workspace => {
            let base = get(input, "path").as_str().map(|path| {
                path.split(['\\', '/']).rfind(|s| !s.is_empty()).unwrap_or("").to_owned()
            });
            let base_value = base.map(Value::String).unwrap_or(Value::Null);
            [get(input, "title"), get(input, "branchName"), get(input, "branch"), &base_value]
                .into_iter()
                .find(|v| is_present(v))
                .cloned()
                .unwrap_or(Value::Null)
        }
        Context::Viewport => match (get(input, "width"), get(input, "height")) {
            (Value::Number(w), Value::Number(h)) => json!(format!("{}×{}", num_text(w), num_text(h))),
            _ => Value::Null,
        },
    }
}

fn num_text(n: &serde_json::Number) -> String {
    if let Some(i) = n.as_i64() {
        i.to_string()
    } else {
        n.to_string()
    }
}

fn read_tool_input(detail: &Value) -> Value {
    if get(detail, "type") != "unknown" {
        return json!({});
    }
    let input = get(detail, "input");
    if input.is_object() { input.clone() } else { json!({}) }
}

fn read_string(value: &Value) -> Option<String> {
    non_empty_str(value).map(str::to_owned)
}

fn format_context(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => to_summary_text(s),
        Value::Number(n) => Some(num_text(n)),
        Value::Array(items) if !items.is_empty() => {
            let head = items[0].as_str().and_then(to_summary_text)?;
            Some(if items.len() > 1 { format!("{head} +{}", items.len() - 1) } else { head })
        }
        _ => None,
    }
}

fn capitalize(sentence: &str) -> String {
    let mut chars = sentence.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn split_separators(value: &str) -> String {
    value
        .split(['.', '_', '-'])
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn humanize_leaf_name(leaf: &str) -> String {
    capitalize(split_separators(leaf).trim().to_lowercase().as_str())
}

/// The agent a Paseo call acts on, so callers can resolve its title.
pub fn paseo_tool_target_agent_id(tool_name: &str, detail: &Value) -> Option<String> {
    paseo_tool_leaf_name(tool_name)?;
    read_string(get(&read_tool_input(detail), "agentId"))
}

fn paseo_tool_display(tool_name: &str, detail: &Value, target_agent_title: Option<&str>) -> Option<(String, Option<String>)> {
    let leaf = paseo_tool_leaf_name(tool_name)?;
    let Some(entry) = paseo_catalog(&leaf) else {
        return Some((humanize_leaf_name(&leaf), None));
    };
    let input = read_tool_input(detail);
    let target = target_agent_title.map(str::to_owned).or_else(|| read_string(get(&input, "agentId")));
    let context = apply_context(entry.context, &input, target.as_deref());
    Some((t(&format!("agentStream.paseoTools.{leaf}")), format_context(&context)))
}

// ---- paseo-tool-presentation/agent-message.ts ----

pub fn build_agent_message_display(message: &Value) -> (String, Option<String>, Value) {
    let sender = get(message, "sender");
    let sender_id = str_of(get(sender, "id"));
    let event = match str_of(get(message, "event")) {
        "finished" => "finished",
        "errored" => "errored",
        "permission-required" => "permissionRequired",
        "closed" => "closed",
        _ => "message",
    };
    let display = t(&format!("agentStream.agentMessage.{event}"));
    let title = str_or(get(sender, "title"), sender_id);
    (
        display,
        to_summary_text(title),
        json!({
            "type": "plain_text",
            "text": format!("{}\n\n{}", ta("agentStream.agentMessage.from", &[("agent", sender_id)]), str_of(get(message, "text"))),
        }),
    )
}

// ---- paseo-tool-presentation/detail-sections.ts ----

#[derive(Clone, Debug, PartialEq)]
pub struct PaseoDetailSection {
    pub title: String,
    pub text: Option<String>,
    pub fields: Vec<(String, String)>,
}

/// Field names with a catalogued label (`agentStream.toolFields.<name>`).
const LABELLED_FIELDS: &[&str] = &[
    "agentId",
    "archivedAgentIds",
    "baseBranch",
    "branchName",
    "browserId",
    "clearExpires",
    "currentModeId",
    "cwd",
    "deltaX",
    "deltaY",
    "doubleClick",
    "expiresIn",
    "expiresAt",
    "filePaths",
    "fullPage",
    "initialPrompt",
    "id",
    "lastMessage",
    "maxEntries",
    "maxRuns",
    "modeId",
    "newMode",
    "nextRunAt",
    "notifyOnFinish",
    "internal",
    "prNumber",
    "projectId",
    "removedDirectory",
    "requestId",
    "scriptName",
    "sessionMode",
    "sinceHours",
    "sourceRef",
    "targetRef",
    "terminalId",
    "thinkingOptionId",
    "timeoutMs",
    "updateCount",
    "workspaceId",
    "worktreeSlug",
];

fn field_label(key: &str) -> Option<String> {
    LABELLED_FIELDS.contains(&key).then(|| t(&format!("agentStream.toolFields.{key}")))
}

fn humanize_key(key: &str) -> String {
    if let Some(known) = field_label(key) {
        return known;
    }
    let mut spaced = String::with_capacity(key.len() + 4);
    let chars: Vec<char> = key.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        if i > 0 && c.is_ascii_uppercase() && (chars[i - 1].is_ascii_lowercase() || chars[i - 1].is_ascii_digit()) {
            spaced.push(' ');
        }
        spaced.push(*c);
    }
    let sentence = spaced
        .split(['.', '_', '-', ' '])
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    capitalize(&sentence)
}

fn indent(value: &str, spaces: usize) -> String {
    value.replace('\n', &format!("\n{}", " ".repeat(spaces)))
}

fn format_value(value: &Value) -> String {
    match value {
        Value::Null => t("agentStream.toolValues.none"),
        Value::Bool(b) => t(if *b { "agentStream.toolValues.yes" } else { "agentStream.toolValues.no" }),
        Value::String(s) => s.clone(),
        Value::Number(n) => num_text(n),
        Value::Array(items) => {
            if items.is_empty() {
                return t("agentStream.toolValues.none");
            }
            items.iter().map(|item| format!("• {}", indent(&format_value(item), 2))).collect::<Vec<_>>().join("\n")
        }
        Value::Object(map) => {
            if map.is_empty() {
                return t("agentStream.toolValues.none");
            }
            map.iter()
                .map(|(k, v)| format!("{}: {}", humanize_key(k), indent(&format_value(v), 2)))
                .collect::<Vec<_>>()
                .join("\n")
        }
    }
}

fn fields_from_value(value: &Value, order: &[&str], omitted: Option<&str>, included: Option<&[&str]>) -> Vec<(String, String)> {
    if value.is_null() {
        return Vec::new();
    }
    let Some(object) = value.as_object() else {
        let formatted = format_value(value);
        return if formatted.is_empty() { Vec::new() } else { vec![(t("agentStream.toolValues.value"), formatted)] };
    };
    let mut keys: Vec<&String> = object
        .keys()
        .filter(|k| Some(k.as_str()) != omitted && included.is_none_or(|inc| inc.contains(&k.as_str())))
        .collect();
    let rank: BTreeMap<&str, usize> = order.iter().enumerate().map(|(i, k)| (*k, i)).collect();
    keys.sort_by(|a, b| {
        let ra = rank.get(a.as_str()).copied().unwrap_or(1 << 30);
        let rb = rank.get(b.as_str()).copied().unwrap_or(1 << 30);
        ra.cmp(&rb).then_with(|| a.cmp(b))
    });
    keys.into_iter().map(|k| (humanize_key(k), format_value(&object[k]))).collect()
}

fn unwrap_mcp_result(output: &Value) -> Value {
    let Some(map) = output.as_object() else { return output.clone() };
    if map.get("structuredContent").is_some_and(|v| !v.is_null()) {
        return map["structuredContent"].clone();
    }
    if let Some(content) = map.get("content").and_then(Value::as_array)
        && content.len() == 1
    {
        let item = &content[0];
        if get(item, "type") == "text" {
            let text = get(item, "text");
            if let Some(text) = text.as_str() {
                return serde_json::from_str(text).unwrap_or_else(|_| json!(text));
            }
            return text.clone();
        }
    }
    output.clone()
}

/// Prompt/Details/Result sections for Paseo MCP tools; None for other tools.
pub fn build_paseo_tool_detail_sections(tool_name: &str, input: &Value, output: &Value) -> Option<Vec<PaseoDetailSection>> {
    let leaf = paseo_tool_leaf_name(tool_name)?;
    let spec = paseo_catalog(&leaf);
    let mut sections = Vec::new();
    let prompt_field = spec.as_ref().and_then(|s| s.prompt_field);
    if let (Some(field), true) = (prompt_field, input.is_object())
        && let Some(prompt) = non_empty_str(get(input, field))
    {
        sections.push(PaseoDetailSection { title: t("agentStream.toolSections.prompt"), text: Some(prompt.to_owned()), fields: Vec::new() });
    }
    let input_fields = fields_from_value(input, spec.as_ref().map(|s| s.input_order).unwrap_or(&[]), prompt_field, None);
    if !input_fields.is_empty() {
        sections.push(PaseoDetailSection { title: t("agentStream.toolSections.details"), text: None, fields: input_fields });
    }
    let outputs = spec.as_ref().and_then(|s| s.output_fields);
    let output_fields = fields_from_value(&unwrap_mcp_result(output), outputs.unwrap_or(&[]), None, outputs);
    if !output_fields.is_empty() {
        sections.push(PaseoDetailSection { title: t("agentStream.toolSections.result"), text: None, fields: output_fields });
    }
    Some(sections)
}

// ---- tool-call-display.ts ----

pub fn humanize_tool_name(name: &str) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return name.to_owned();
    }
    if trimmed.contains([':', '.', '/']) || trimmed.contains("__") {
        return trimmed.to_owned();
    }
    capitalize(&split_separators(trimmed).to_lowercase())
}

fn canonical_display(detail: &Value, cwd: Option<&str>) -> (String, Option<String>) {
    let s = |key: &str| Some(str_of(get(detail, key)).to_owned());
    match str_of(get(detail, "type")) {
        "shell" => ("Shell".into(), s("command")),
        "read" => ("Read".into(), Some(strip_cwd_prefix(str_of(get(detail, "filePath")), cwd))),
        "edit" => ("Edit".into(), Some(strip_cwd_prefix(str_of(get(detail, "filePath")), cwd))),
        "write" => ("Write".into(), Some(strip_cwd_prefix(str_of(get(detail, "filePath")), cwd))),
        "search" => ("Search".into(), s("query")),
        "fetch" => ("Fetch".into(), s("url")),
        "worktree_setup" => ("Worktree setup".into(), s("branchName")),
        "sub_agent" => (
            read_string(get(detail, "subAgentType")).unwrap_or_else(|| "Task".into()),
            read_string(get(detail, "description")),
        ),
        "plain_text" => (String::new(), s("label")),
        "plan" => ("Plan".into(), None),
        _ => (String::new(), None),
    }
}

/// A tool row glyph (utils/tool-call-icon-name.ts).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolGlyph {
    SquareTerminal,
    Eye,
    Pencil,
    Search,
    Bot,
    Sparkles,
    Brain,
    MicVocal,
    Paseo,
    Wrench,
    Plus,
    CircleDot,
    Check,
    CheckSquare,
}

fn icon_by_name(name: &str) -> ToolGlyph {
    match name {
        "square_terminal" => ToolGlyph::SquareTerminal,
        "eye" => ToolGlyph::Eye,
        "pencil" => ToolGlyph::Pencil,
        "search" => ToolGlyph::Search,
        "bot" => ToolGlyph::Bot,
        "sparkles" => ToolGlyph::Sparkles,
        "brain" => ToolGlyph::Brain,
        "mic_vocal" => ToolGlyph::MicVocal,
        "paseo" => ToolGlyph::Paseo,
        _ => ToolGlyph::Wrench,
    }
}

pub fn resolve_tool_call_icon(tool_name: &str, detail: Option<&Value>, agent_message: bool) -> ToolGlyph {
    if agent_message {
        return ToolGlyph::Paseo;
    }
    let lower = tool_name.trim().to_lowercase();
    if let Some(detail) = detail
        && get(detail, "type") == "plain_text"
        && let Some(icon) = get(detail, "icon").as_str()
    {
        return icon_by_name(icon);
    }
    if lower == "thinking" && detail.is_none_or(|d| get(d, "type") == "unknown") {
        return ToolGlyph::Brain;
    }
    if lower == "speak" {
        return ToolGlyph::MicVocal;
    }
    if is_paseo_tool_name(&lower) {
        return ToolGlyph::Paseo;
    }
    if lower == "task" {
        return ToolGlyph::Bot;
    }
    let Some(detail) = detail else { return ToolGlyph::Wrench };
    match str_of(get(detail, "type")) {
        "shell" | "worktree_setup" => ToolGlyph::SquareTerminal,
        "read" => ToolGlyph::Eye,
        "edit" | "write" => ToolGlyph::Pencil,
        "search" | "fetch" => ToolGlyph::Search,
        "sub_agent" => ToolGlyph::Bot,
        "plan" => ToolGlyph::Brain,
        _ => ToolGlyph::Wrench,
    }
}

// ---- utils/extract-tool-call-file-path.ts ----

const SHELL_FILE_COMMANDS: &[&str] = &["cat", "bat", "less", "more", "head", "tail", "wc", "nl", "tac", "od", "xxd", "file", "stat", "column", "md5", "md5sum", "sha1sum", "sha256sum", "shasum"];

fn path_from_shell_command(command: &str) -> Option<String> {
    let trimmed = command.trim();
    if trimmed.is_empty() || trimmed.contains(['|', '>', '<', '&', ';', '`', '$', '(', ')']) {
        return None;
    }
    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    if tokens.len() < 2 || !SHELL_FILE_COMMANDS.contains(&tokens[0]) {
        return None;
    }
    let last = *tokens.last()?;
    if last.starts_with('-') {
        return None;
    }
    if tokens.len() > 2 {
        let previous = tokens[tokens.len() - 2];
        if !previous.starts_with('-') {
            let before = tokens[tokens.len() - 3];
            let short_flag = before.len() == 2 && before.starts_with('-') && before.as_bytes()[1].is_ascii_alphabetic();
            if !short_flag {
                return None;
            }
        }
    }
    Some(last.to_owned())
}

pub fn extract_tool_call_file_path(detail: &Value) -> Option<String> {
    match str_of(get(detail, "type")) {
        "read" | "edit" | "write" => read_string(get(detail, "filePath")),
        "shell" => path_from_shell_command(str_of(get(detail, "command"))),
        _ => None,
    }
}

// ---- tool-call-detail-state.ts ----

fn meaningful_value(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(s) => !s.trim().is_empty(),
        Value::Number(_) | Value::Bool(_) => true,
        Value::Array(items) => items.iter().any(meaningful_value),
        Value::Object(map) => map.values().any(meaningful_value),
    }
}

fn present(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

pub fn has_meaningful_tool_call_detail(detail: &Value) -> bool {
    let d = |k: &str| get(detail, k);
    match str_of(d("type")) {
        "shell" => true,
        "read" | "write" => present(d("filePath")) || present(d("content")),
        "edit" => present(d("filePath")) || present(d("unifiedDiff")) || present(d("oldString")) || present(d("newString")),
        "search" => {
            !str_of(d("query")).trim().is_empty()
                || present(d("content"))
                || !list(d("filePaths")).is_empty()
                || !list(d("webResults")).is_empty()
                || !list(d("annotations")).is_empty()
        }
        "fetch" => present(d("url")) || present(d("result")) || present(d("codeText")),
        "sub_agent" => present(d("subAgentType")) || present(d("description")) || present(d("log")),
        "plain_text" => present(d("label")) || present(d("text")),
        "plan" => !str_of(d("text")).trim().is_empty(),
        "unknown" => meaningful_value(d("input")) || meaningful_value(d("output")),
        _ => false,
    }
}

pub fn is_pending_tool_call_detail(detail: &Value, status: &str, error: &Value) -> bool {
    get(detail, "type") != "worktree_setup"
        && (status == "running" || status == "executing")
        && error.is_null()
        && !has_meaningful_tool_call_detail(detail)
}

/// protocol tool-call-display.ts formatErrorText (failed calls only).
pub fn tool_call_error_text(status: &str, error: &Value) -> Option<String> {
    if status != "failed" || error.is_null() {
        return None;
    }
    if let Some(s) = error.as_str() {
        return Some(s.to_owned());
    }
    if let Some(content) = get(error, "content").as_str() {
        return Some(content.to_owned());
    }
    Some(pretty(error))
}

fn non_empty_error(error: &Value) -> bool {
    !(error.is_null() || error == &Value::Bool(false) || error == "" || error.as_i64() == Some(0))
}

// ---- tool-calls/presentation.ts ----

#[derive(Clone, Debug)]
pub struct ToolCallPresentation {
    pub tool_name: String,
    pub detail: Value,
    pub display_name: String,
    pub summary: Option<String>,
    pub error_text: Option<String>,
    pub icon: ToolGlyph,
    pub status: String,
    pub is_loading_details: bool,
    pub can_open_details: bool,
    pub open_file_path: Option<String>,
    /// plan-card.tsx PlanOutcome: pending / approved / rejected / canceled.
    pub plan_outcome: Option<&'static str>,
}

impl ToolCallPresentation {
    pub fn is_plan(&self) -> bool {
        get(&self.detail, "type") == "plan"
    }
    pub fn is_loading(&self) -> bool {
        self.status == "running" || self.status == "executing"
    }
    pub fn is_error(&self) -> bool {
        self.status == "failed"
    }
}

pub fn build_tool_call_presentation(item: &Value, cwd: Option<&str>, target_agent_title: Option<&str>) -> ToolCallPresentation {
    let name = str_of(get(item, "name")).to_owned();
    let status = str_or(get(item, "status"), "completed").to_owned();
    let error = get(item, "error");
    let metadata = get(item, "metadata");
    let raw_detail = if get(item, "detail").is_object() {
        get(item, "detail").clone()
    } else {
        json!({"type": "unknown", "input": null, "output": null})
    };
    let agent_message = get(item, "agentMessage");
    let agent_display = agent_message.is_object().then(|| build_agent_message_display(agent_message));
    let detail = agent_display.as_ref().map(|d| d.2.clone()).unwrap_or_else(|| raw_detail.clone());
    let error_text = tool_call_error_text(&status, error);

    let paseo = match &agent_display {
        Some((display, summary, _)) => Some((display.clone(), summary.clone())),
        None => paseo_tool_display(&name, &raw_detail, target_agent_title),
    };
    let (display_name, mut summary) = match paseo {
        Some(p) => p,
        None => {
            let (canonical_name, canonical_summary) = canonical_display(&raw_detail, cwd);
            let lower = name.trim().to_lowercase();
            let unknown = get(&raw_detail, "type") == "unknown";
            let (override_name, override_summary): (Option<&str>, Option<String>) = if unknown && lower == "task" {
                (Some("Task"), read_string(get(metadata, "subAgentActivity")))
            } else if unknown && lower == "thinking" {
                (Some("Thinking"), None)
            } else if lower == "terminal" {
                let label = if get(&raw_detail, "type") == "plain_text" { read_string(get(&raw_detail, "label")) } else { None };
                (Some("Terminal"), label)
            } else {
                (None, None)
            };
            let display = override_name.map(|label| {
                if label == "Thinking" { super::i18n::t("agentStream.thinking") } else { label.to_owned() }
            }).unwrap_or_else(|| {
                if canonical_name.is_empty() { humanize_tool_name(&name) } else { canonical_name }
            });
            let summary = if override_name.is_some() { override_summary } else { canonical_summary };
            (display, summary)
        }
    };
    if summary.as_deref() == Some("") {
        summary = None;
    }
    let loading_details = is_pending_tool_call_detail(&detail, &status, error);
    let has_details = non_empty_error(error) || has_meaningful_tool_call_detail(&detail);
    let plan_outcome = if get(&raw_detail, "type") != "plan" {
        None
    } else if status == "canceled" {
        Some("canceled")
    } else if get(metadata, "approved") == &Value::Bool(false) {
        Some("rejected")
    } else if get(metadata, "approved") == &Value::Bool(true) {
        Some("approved")
    } else if status == "running" || status == "executing" {
        Some("pending")
    } else {
        None
    };
    ToolCallPresentation {
        icon: resolve_tool_call_icon(&name, Some(&detail), agent_message.is_object()),
        open_file_path: extract_tool_call_file_path(&detail),
        tool_name: name,
        detail,
        display_name,
        summary,
        error_text,
        status,
        is_loading_details: loading_details,
        can_open_details: has_details || loading_details,
        plan_outcome,
    }
}

// ---- tool-calls/detail-level/overview/model.ts ----

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct OverviewSummary {
    pub edited_file_count: usize,
    pub command_count: usize,
    pub read_file_count: usize,
    pub search_count: usize,
    pub other_tool_count: usize,
    pub paseo_call_count: usize,
}

fn is_search_tool_name(normalized: &str) -> bool {
    for suffix in ["web_search", "llm_context"] {
        if let Some(prefix) = normalized.strip_suffix(suffix)
            && (prefix.is_empty() || prefix.ends_with(['_', '.', ':', '/']))
        {
            return true;
        }
    }
    false
}

pub fn build_overview_summary(calls: &[std::rc::Rc<Value>]) -> (OverviewSummary, bool) {
    let mut edited = std::collections::HashSet::new();
    let mut read = std::collections::HashSet::new();
    let mut summary = OverviewSummary::default();
    let mut loading = false;
    for call in calls {
        let item = get(call, "item");
        let name = str_of(get(item, "name"));
        let normalized = name.trim().to_lowercase();
        let detail = get(item, "detail");
        let status = str_of(get(item, "status"));
        loading = loading || status == "running" || status == "executing";
        let kind = str_of(get(detail, "type"));
        if is_paseo_tool_name(name) || normalized.starts_with("paseo_") {
            summary.paseo_call_count += 1;
        } else if kind == "edit" || kind == "write" {
            edited.insert(str_of(get(detail, "filePath")).to_owned());
        } else if kind == "shell" {
            summary.command_count += 1;
        } else if kind == "read" {
            read.insert(str_of(get(detail, "filePath")).to_owned());
        } else if kind == "search" || is_search_tool_name(&normalized) {
            summary.search_count += 1;
        } else {
            summary.other_tool_count += 1;
        }
    }
    summary.edited_file_count = edited.len();
    summary.read_file_count = read.len();
    (summary, loading)
}

// ---- tool_call_details.dart (data) ----

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffKind {
    Context,
    Header,
    Add,
    Remove,
}

fn split_lines(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let normalized = text.replace("\r\n", "\n");
    let mut lines: Vec<String> = normalized.split('\n').map(str::to_owned).collect();
    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

/// tool-call-parsers.ts parseUnifiedDiff.
pub fn parse_unified_diff(text: &str) -> Vec<(DiffKind, String)> {
    let mut out = Vec::new();
    for line in split_lines(text) {
        if line.is_empty() {
            out.push((DiffKind::Context, line));
        } else if line.starts_with("@@") {
            out.push((DiffKind::Header, line));
        } else if line.starts_with('+') {
            if !line.starts_with("+++") {
                out.push((DiffKind::Add, line));
            }
        } else if line.starts_with('-') {
            if !line.starts_with("---") {
                out.push((DiffKind::Remove, line));
            }
        } else if line.starts_with("diff --git") || line.starts_with("index ") {
            continue;
        } else if line.starts_with("\\ No newline") {
            out.push((DiffKind::Header, line));
        } else {
            out.push((DiffKind::Context, line));
        }
    }
    out
}

/// tool-call-parsers.ts buildLineDiff (LCS line diff).
pub fn build_line_diff(original: &str, updated: &str) -> Vec<(DiffKind, String)> {
    let a = split_lines(original);
    let b = split_lines(updated);
    if a.is_empty() && b.is_empty() {
        return Vec::new();
    }
    let (m, n) = (a.len(), b.len());
    let mut dp = vec![vec![0usize; n + 1]; m + 1];
    for i in (0..m).rev() {
        for j in (0..n).rev() {
            dp[i][j] = if a[i] == b[j] { dp[i + 1][j + 1] + 1 } else { dp[i + 1][j].max(dp[i][j + 1]) };
        }
    }
    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < m && j < n {
        if a[i] == b[j] {
            out.push((DiffKind::Context, format!(" {}", a[i])));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            out.push((DiffKind::Remove, format!("-{}", a[i])));
            i += 1;
        } else {
            out.push((DiffKind::Add, format!("+{}", b[j])));
            j += 1;
        }
    }
    while i < m {
        out.push((DiffKind::Remove, format!("-{}", a[i])));
        i += 1;
    }
    while j < n {
        out.push((DiffKind::Add, format!("+{}", b[j])));
        j += 1;
    }
    out
}

/// One section of ToolCallDetailsContent, rendered by `rows::draw_details`.
#[derive(Clone, Debug)]
pub enum DetailBlock {
    /// Full-bleed surface1 mono block (shell: `$ ` muted prefix).
    Shell { command: String, output: String },
    SubAgentLog { session: Option<String>, actions: Vec<(String, Option<String>)>, log: String, fallback: String },
    Diff(Vec<(DiffKind, String)>),
    /// Read/write content: highlighted by path, gutter when start line known.
    Source { content: String, path: String, start_line: Option<i64> },
    /// Bordered surface2 scroll area with plain code text.
    Code(String),
    /// Bare code text (search file paths, web results, annotations).
    BareCode(String),
    PlainText(String),
    GroupHeader(String),
    JsonBox(String),
    Paseo(PaseoDetailSection),
    Error(String),
}

fn serialize(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => pretty(other),
    }
}

fn trim_end_newlines(s: &str) -> &str {
    s.trim_end_matches('\n')
}

fn trim_start_newlines(s: &str) -> &str {
    s.trim_start_matches('\n')
}

fn tool_name_label(name: &str) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return name.to_owned();
    }
    trimmed
        .split(['.', '_', '-', ' '])
        .filter(|s| !s.is_empty())
        .map(capitalize)
        .collect::<Vec<_>>()
        .join(" ")
}

/// `ToolCallDetailsContent._sections` (+ the trailing error section).
/// `i18n` maps catalog keys (toolCallDetails.input/output/subAgentActivity).
pub fn detail_blocks(detail: &Value, tool_name: Option<&str>, error_text: Option<&str>, i18n: &dyn Fn(&str) -> String) -> Vec<DetailBlock> {
    let d = |k: &str| get(detail, k);
    let mut blocks = Vec::new();
    match str_of(d("type")) {
        "shell" => blocks.push(DetailBlock::Shell {
            command: trim_end_newlines(str_of(d("command"))).to_owned(),
            output: trim_start_newlines(str_of(d("output"))).to_owned(),
        }),
        "sub_agent" => {
            let mut actions = Vec::new();
            let mut remaining = Vec::new();
            for line in trim_start_newlines(str_of(d("log"))).split('\n') {
                let normalized = line.trim();
                if normalized.is_empty() {
                    continue;
                }
                let parsed = normalized.strip_prefix('[').and_then(|rest| {
                    let close = rest.find(']')?;
                    let tool = rest[..close].trim();
                    let after = &rest[close + 1..];
                    if !after.is_empty() && !after.starts_with(char::is_whitespace) {
                        return None;
                    }
                    (!tool.is_empty()).then(|| (tool.to_owned(), after.trim().to_owned()))
                });
                match parsed {
                    Some((tool, summary)) => actions.push((tool_name_label(&tool), (!summary.is_empty()).then_some(summary))),
                    None => remaining.push(line.to_owned()),
                }
            }
            let log = trim_start_newlines(&remaining.join("\n")).to_owned();
            let ty = str_of(d("subAgentType"));
            let description = str_of(d("description"));
            let fallback = match (ty.is_empty(), description.is_empty()) {
                (false, false) => format!("{ty}: {description}"),
                (false, true) => ty.to_owned(),
                (true, false) => description.to_owned(),
                (true, true) => i18n("toolCallDetails.subAgentActivity"),
            };
            let session = non_empty_str(d("childSessionId")).map(str::to_owned);
            blocks.push(DetailBlock::SubAgentLog { session, actions, log, fallback });
        }
        "edit" => {
            let lines = if !d("unifiedDiff").is_null() {
                parse_unified_diff(str_of(d("unifiedDiff")))
            } else {
                build_line_diff(str_of(d("oldString")), str_of(d("newString")))
            };
            blocks.push(DetailBlock::Diff(lines));
        }
        "write" => {
            if present(d("content")) {
                blocks.push(DetailBlock::Source { content: str_of(d("content")).to_owned(), path: str_of(d("filePath")).to_owned(), start_line: None });
            }
        }
        "read" => {
            if present(d("content")) {
                let start = d("offset").as_f64().map(|v| v as i64).unwrap_or(1);
                blocks.push(DetailBlock::Source { content: str_of(d("content")).to_owned(), path: str_of(d("filePath")).to_owned(), start_line: Some(start) });
            }
        }
        "search" => {
            if present(d("content")) {
                blocks.push(DetailBlock::Code(str_of(d("content")).to_owned()));
            }
            if !list(d("filePaths")).is_empty() {
                blocks.push(DetailBlock::BareCode(list(d("filePaths")).iter().map(|v| str_of(v).to_owned()).collect::<Vec<_>>().join("\n")));
            }
            if !list(d("webResults")).is_empty() {
                blocks.push(DetailBlock::BareCode(
                    list(d("webResults"))
                        .iter()
                        .map(|r| format!("{}\n{}", str_of(get(r, "title")), str_of(get(r, "url"))))
                        .collect::<Vec<_>>()
                        .join("\n\n"),
                ));
            }
            if !list(d("annotations")).is_empty() {
                blocks.push(DetailBlock::BareCode(list(d("annotations")).iter().map(|v| str_of(v).to_owned()).collect::<Vec<_>>().join("\n\n")));
            }
        }
        "fetch" => {
            let url = str_of(d("url"));
            let result = str_of(d("result"));
            blocks.push(DetailBlock::Code(if result.is_empty() { url.to_owned() } else { format!("{url}\n\n{result}") }));
        }
        "plain_text" => {
            let text = str_of(d("text"));
            // Some tools publish their command or wait target only as a label.
            let content = if text.trim().is_empty() { str_of(d("label")) } else { text };
            if !content.trim().is_empty() {
                blocks.push(DetailBlock::PlainText(content.to_owned()));
            }
        }
        "unknown" => {
            let input = d("input");
            let output = d("output");
            if let Some(sections) = tool_name.and_then(|name| build_paseo_tool_detail_sections(name, input, output)) {
                blocks.extend(sections.into_iter().map(DetailBlock::Paseo));
            } else if input.is_string() && output.is_null() {
                blocks.push(DetailBlock::PlainText(str_of(input).to_owned()));
            } else {
                for (key, value) in [("toolCallDetails.input", input), ("toolCallDetails.output", output)] {
                    if !meaningful_value(value) {
                        continue;
                    }
                    let text = serialize(value);
                    if text.is_empty() {
                        continue;
                    }
                    blocks.push(DetailBlock::GroupHeader(i18n(key)));
                    blocks.push(DetailBlock::JsonBox(text));
                }
            }
        }
        _ => {}
    }
    if let Some(error) = error_text.filter(|e| !e.is_empty()) {
        blocks.push(DetailBlock::Error(error.to_owned()));
    }
    blocks
}

/// Full-bleed detail types sit 8 apart, the rest 16.
pub fn detail_is_full_bleed(detail: &Value) -> bool {
    matches!(str_of(get(detail, "type")), "edit" | "shell" | "write")
}

#[cfg(test)]
mod catalog_tests {
    use super::*;

    fn catalogs() -> [Value; 2] {
        [
            serde_json::from_str(include_str!("../../../resources/locales/en.json")).unwrap(),
            serde_json::from_str(include_str!("../../../resources/locales/ko.json")).unwrap(),
        ]
    }

    #[test]
    fn every_labelled_field_and_paseo_tool_has_both_translations() {
        for catalog in catalogs() {
            let stream = &catalog["agentStream"];
            for field in LABELLED_FIELDS {
                assert!(stream["toolFields"][field].is_string(), "agentStream.toolFields.{field}");
            }
            let tools = stream["paseoTools"].as_object().unwrap();
            for leaf in tools.keys() {
                assert!(paseo_catalog(leaf).is_some(), "agentStream.paseoTools.{leaf} has no catalog entry");
            }
            for key in ["agentMessage", "toolSections", "toolValues"] {
                assert!(stream[key].is_object(), "agentStream.{key}");
            }
        }
        let [en, ko] = catalogs();
        let keys = |c: &Value| c["agentStream"]["paseoTools"].as_object().unwrap().keys().cloned().collect::<Vec<_>>();
        assert_eq!(keys(&en), keys(&ko));
        assert_eq!(keys(&en).len(), 62);
    }
}
