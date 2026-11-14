use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MAX_EXPLICIT_AGENT_TITLE_CHARS: usize = 200;
pub const MAX_SLUG_LENGTH: usize = 50;

/// JS-compatible string conversion for JSON error values. Actual Rust errors use
/// their Display implementation; arbitrary objects are not trusted as Errors.
pub fn error_message(error: &Value) -> String {
    match error {
        Value::String(s) => s.clone(),
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Object(_) => "[object Object]".into(),
        Value::Array(items) => items
            .iter()
            .map(|v| {
                if v.is_null() {
                    String::new()
                } else {
                    error_message(v)
                }
            })
            .collect::<Vec<_>>()
            .join(","),
    }
}
pub fn error_message_or<'a>(error: &'a Value, fallback: &'a str) -> &'a str {
    error.as_str().filter(|s| !s.is_empty()).unwrap_or(fallback)
}
pub fn normalize_tool_name(name: &str) -> String {
    name.trim().to_lowercase()
}
pub fn tokenize_tool_name(name: &str) -> Vec<String> {
    normalize_tool_name(name)
        .split(|c: char| !c.is_ascii_lowercase() && !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}
pub fn tool_leaf_name(name: &str) -> Option<String> {
    tokenize_tool_name(name).pop()
}
pub fn is_likely_namespaced_tool_name(name: &str) -> bool {
    let name = normalize_tool_name(name);
    if name.contains(['.', ':', '/']) {
        return true;
    }
    let mut segments = name.split("__").filter(|s| !s.is_empty());
    let first = segments.next();
    let second = segments.next();
    first.is_some() && second.is_some_and(|s| s.contains('_') || segments.next().is_some())
}
pub fn paseo_tool_leaf_name(name: &str) -> Option<String> {
    let name = normalize_tool_name(name);
    if name.contains("__") {
        let segments: Vec<_> = name.split("__").filter(|s| !s.is_empty()).collect();
        if segments.len() >= 3
            && segments[0] == "mcp"
            && (segments[1] == "paseo" || segments[1].starts_with("paseo_"))
        {
            return Some(segments[2..].join("__"));
        }
    } else if let Some((prefix, leaf)) = name.split_once('.')
        && (prefix == "paseo" || prefix.starts_with("paseo_")) {
            return Some(leaf.to_owned());
        }
    None
}
pub fn strip_cwd_prefix(file_path: &str, cwd: Option<&str>) -> String {
    let Some(cwd) = cwd.filter(|s| !s.is_empty()) else {
        return file_path.to_owned();
    };
    if file_path.is_empty() {
        return String::new();
    }
    let cwd = cwd.replace('\\', "/");
    let cwd = cwd.trim_end_matches('/');
    let path = file_path.replace('\\', "/");
    if path == cwd {
        return ".".into();
    }
    path.strip_prefix(cwd)
        .and_then(|p| p.strip_prefix('/'))
        .map_or_else(|| file_path.to_owned(), str::to_owned)
}
pub fn validate_branch_slug(slug: &str) -> Result<(), &'static str> {
    if slug.is_empty() {
        return Err("Branch name cannot be empty");
    }
    if slug.len() > 100 {
        return Err("Branch name too long (max 100 characters)");
    }
    if !slug
        .bytes()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'/')
    {
        return Err(
            "Branch name must contain only lowercase letters, numbers, hyphens, and forward slashes",
        );
    }
    if slug.starts_with('-') || slug.ends_with('-') {
        return Err("Branch name cannot start or end with a hyphen");
    }
    if slug.contains("--") {
        return Err("Branch name cannot have consecutive hyphens");
    }
    Ok(())
}
pub fn slugify(input: &str) -> String {
    let input = input.to_lowercase();
    let mut slug = String::new();
    let mut separator = false;
    for c in input.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            if separator && !slug.is_empty() {
                slug.push('-');
            }
            slug.push(c);
            separator = false;
        } else {
            separator = true;
        }
    }
    if slug.len() > MAX_SLUG_LENGTH {
        slug.truncate(MAX_SLUG_LENGTH);
        if let Some(index) = slug.rfind('-').filter(|index| *index > MAX_SLUG_LENGTH / 2) {
            slug.truncate(index);
        } else {
            while slug.ends_with('-') {
                slug.pop();
            }
        }
    }
    slug
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceStateBucket {
    NeedsInput,
    Failed,
    Running,
    Attention,
    Done,
}
impl WorkspaceStateBucket {
    pub fn priority(self) -> u8 {
        match self {
            Self::NeedsInput => 0,
            Self::Failed => 1,
            Self::Running => 2,
            Self::Attention => 3,
            Self::Done => 4,
        }
    }
}
pub fn agent_state_bucket(
    status: &str,
    pending_permission_count: u64,
    requires_attention: bool,
    attention_reason: Option<&str>,
) -> WorkspaceStateBucket {
    if pending_permission_count > 0 || attention_reason == Some("permission") {
        WorkspaceStateBucket::NeedsInput
    } else if status == "error" || attention_reason == Some("error") {
        WorkspaceStateBucket::Failed
    } else if status == "running" {
        WorkspaceStateBucket::Running
    } else if requires_attention {
        WorkspaceStateBucket::Attention
    } else {
        WorkspaceStateBucket::Done
    }
}
