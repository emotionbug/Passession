//! Private review drafts and the review feedback attachment (original
//! review/store.ts, review/attachment.ts and review/feedback/index.ts).

use super::json::{array, int, text};
use serde_json::{Value, json};

/// `Uri.encodeComponent` over a trimmed value.
fn part(value: &str) -> String {
    let mut out = String::new();
    for byte in value.trim().bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Original `buildReviewDraftKey`: one private draft per host, workspace (or
/// normalized cwd), comparison, base ref and whitespace mode.
pub fn review_draft_key(
    server_id: &str,
    workspace_id: &str,
    cwd: &str,
    mode: &str,
    base_ref: &str,
    ignore_whitespace: bool,
) -> String {
    let trimmed = cwd.trim();
    let normalized = if trimmed == "/" { trimmed } else { trimmed.trim_end_matches('/') };
    let workspace = workspace_id.trim();
    let scope = if workspace.is_empty() {
        format!("cwd={}", part(normalized))
    } else {
        format!("workspace={}", part(workspace))
    };
    [
        "review".to_owned(),
        format!("server={}", part(server_id)),
        scope,
        format!("mode={mode}"),
        format!("base={}", part(base_ref)),
        format!("ignoreWhitespace={ignore_whitespace}"),
    ]
    .join(":")
}

/// The device storage key of a draft (original `paseo.review.<key>`).
pub fn storage_key(draft_key: &str) -> String {
    format!("paseo.review.{draft_key}")
}

/// A comment as persisted: `{id, filePath, side, lineNumber, body, createdAt, updatedAt}`.
pub fn valid_comment(comment: &Value) -> bool {
    comment.get("id").is_some_and(Value::is_string)
        && comment.get("filePath").is_some_and(Value::is_string)
        && matches!(comment.get("side").and_then(Value::as_str), Some("old" | "new"))
        && comment
            .get("lineNumber")
            .and_then(Value::as_i64)
            .is_some_and(|line| line > 0)
        && comment.get("body").is_some_and(Value::is_string)
}

/// Original `FeedbackPhase`: sending with a comment count, or failed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FeedbackPhase {
    pub sending: Option<usize>,
    pub failed: Option<String>,
}

/// One shared live draft (original `useReviewDraftStore` entry).
#[derive(Clone, Debug, Default)]
pub struct ReviewDraft {
    pub key: String,
    pub comments: Vec<Value>,
    pub phase: FeedbackPhase,
    pub refs: usize,
    pub loaded: bool,
}

impl ReviewDraft {
    pub fn new(key: &str) -> Self {
        Self {
            key: key.to_owned(),
            ..Default::default()
        }
    }

    /// Original `load`: stored comments that validate, ahead of any added
    /// before the load completed. Malformed storage is ignored.
    pub fn load(&mut self, stored: Option<&Value>) {
        if self.loaded {
            return;
        }
        self.loaded = true;
        let stored: Vec<Value> = match stored {
            Some(Value::Array(items)) => items.iter().filter(|c| valid_comment(c)).cloned().collect(),
            // Flutter stored the JSON text; accept either shape.
            Some(Value::String(raw)) => match serde_json::from_str::<Value>(raw) {
                Ok(Value::Array(items)) => items.into_iter().filter(valid_comment).collect(),
                _ => Vec::new(),
            },
            _ => Vec::new(),
        };
        let mut comments = stored;
        comments.append(&mut self.comments);
        self.comments = comments;
    }

    /// The persisted value: `None` removes the stored draft.
    pub fn persisted(&self) -> Value {
        if self.comments.is_empty() {
            Value::Null
        } else {
            Value::Array(self.comments.clone())
        }
    }

    /// Original `saveReviewDraftComment`: the trimmed body updates the edited
    /// comment while it still exists, otherwise it becomes a new comment.
    /// Returns false when nothing changed (blank body).
    #[allow(clippy::too_many_arguments)]
    pub fn save_comment(
        &mut self,
        id: Option<&str>,
        file_path: &str,
        side: &str,
        line_number: i64,
        body: &str,
        new_id: String,
        now_iso: String,
    ) -> bool {
        let trimmed = body.trim();
        if trimmed.is_empty() {
            return false;
        }
        let index = id.and_then(|id| self.comments.iter().position(|c| c["id"] == id));
        match index {
            Some(index) => {
                let comment = &mut self.comments[index];
                comment["body"] = json!(trimmed);
                comment["updatedAt"] = json!(now_iso);
            }
            None => self.comments.push(json!({
                "id": new_id,
                "filePath": file_path,
                "side": side,
                "lineNumber": line_number,
                "body": trimmed,
                "createdAt": now_iso,
                "updatedAt": now_iso,
            })),
        }
        true
    }

    pub fn delete(&mut self, id: &str) -> bool {
        let before = self.comments.len();
        self.comments.retain(|c| c["id"] != id);
        before != self.comments.len()
    }

    /// Original `clearSentReviewDraftComments`: acknowledgement removes only
    /// the versions included in that submission; a comment edited meanwhile stays.
    pub fn clear_sent(&mut self, sent: &[Value]) -> bool {
        let before = self.comments.len();
        self.comments.retain(|comment| {
            match sent.iter().find(|s| s["id"] == comment["id"]) {
                None => true,
                Some(sent) => sent["updatedAt"] != comment["updatedAt"] || sent["body"] != comment["body"],
            }
        });
        before != self.comments.len()
    }
}

/// One review attachment ready to send (original `ReviewComposerAttachment`).
#[derive(Clone, Debug, PartialEq)]
pub struct AttachmentSnapshot {
    pub comment_count: usize,
    pub attachment: Value,
}

fn context_line(line: &Value, old: Option<i64>, next: Option<i64>) -> Option<Value> {
    if line["type"] == "header" {
        return None;
    }
    Some(json!({
        "oldLineNumber": old,
        "newLineNumber": next,
        "type": line["type"],
        "content": line["content"],
    }))
}

/// Original `buildReviewAttachmentSnapshot`: each comment resolves against the
/// current diff — its file, then the hunk line whose number on the comment's
/// side matches — carrying the hunk header, the target line and up to three
/// lines either side. Comments whose line no longer exists are dropped.
pub fn build_attachment_snapshot<'a>(
    cwd: &str,
    mode: &str,
    base_ref: &str,
    comments: &[Value],
    diff_file: &dyn Fn(&str) -> Option<&'a Value>,
) -> Option<AttachmentSnapshot> {
    let mut resolved = Vec::new();
    for comment in comments {
        let Some(file) = diff_file(text(comment, "filePath")) else {
            continue;
        };
        let mut context = None;
        for hunk in array(file, "hunks") {
            let lines = array(hunk, "lines");
            let header = lines
                .iter()
                .find(|l| l["type"] == "header")
                .map(|l| text(l, "content").to_owned())
                .unwrap_or_else(|| "@@".to_owned());
            let mut old = int(hunk, "oldStart");
            let mut next = int(hunk, "newStart");
            let mut numbered: Vec<(&Value, Option<i64>, Option<i64>)> = Vec::new();
            for line in lines {
                match line["type"].as_str() {
                    Some("remove") => {
                        numbered.push((line, Some(old), None));
                        old += 1;
                    }
                    Some("add") => {
                        numbered.push((line, None, Some(next)));
                        next += 1;
                    }
                    Some("context") => {
                        numbered.push((line, Some(old), Some(next)));
                        old += 1;
                        next += 1;
                    }
                    _ => numbered.push((line, None, None)),
                }
            }
            let side_old = comment["side"] == "old";
            let target_number = comment["lineNumber"].as_i64();
            let Some(index) = numbered.iter().position(|entry| {
                let number = if side_old { entry.1 } else { entry.2 };
                number.is_some() && number == target_number
            }) else {
                continue;
            };
            let target = numbered[index];
            let Some(target_line) = context_line(target.0, target.1, target.2) else {
                break;
            };
            let start = index.saturating_sub(3);
            let end = (index + 4).min(numbered.len());
            let lines: Vec<Value> = numbered[start..end]
                .iter()
                .filter_map(|entry| context_line(entry.0, entry.1, entry.2))
                .collect();
            context = Some(json!({
                "hunkHeader": header,
                "targetLine": target_line,
                "lines": lines,
            }));
            break;
        }
        let Some(context) = context else {
            continue;
        };
        resolved.push(json!({
            "filePath": comment["filePath"],
            "side": comment["side"],
            "lineNumber": comment["lineNumber"],
            "body": comment["body"],
            "context": context,
        }));
    }
    if resolved.is_empty() {
        return None;
    }
    let base = base_ref.trim();
    Some(AttachmentSnapshot {
        comment_count: resolved.len(),
        attachment: json!({
            "type": "review",
            "mimeType": "application/paseo-review",
            "cwd": cwd,
            "mode": mode,
            "baseRef": if base.is_empty() { Value::Null } else { json!(base) },
            "comments": resolved,
        }),
    })
}

/// The draft comments a snapshot carries (original `sentComments` filter).
pub fn sent_comments(comments: &[Value], snapshot: &AttachmentSnapshot) -> Vec<Value> {
    let sent = array(&snapshot.attachment, "comments");
    comments
        .iter()
        .filter(|comment| {
            sent.iter().any(|s| {
                s["filePath"] == comment["filePath"]
                    && s["side"] == comment["side"]
                    && s["lineNumber"] == comment["lineNumber"]
                    && s["body"] == comment["body"]
            })
        })
        .cloned()
        .collect()
}

/// Original `deriveState` disabled reason.
pub fn feedback_disabled_reason(recipient_count: usize, connected: bool, has_attachment: bool) -> Option<&'static str> {
    if recipient_count == 0 {
        return Some("no-agents");
    }
    if !connected {
        return Some("disconnected");
    }
    if !has_attachment {
        return Some("no-context");
    }
    None
}

/// The context lines a comment prompt shows (`path · ±line`).
pub fn comment_subtitle(path: &str, side: &str, line: Option<i64>) -> String {
    format!(
        "{path} · {}{}",
        if side == "old" { "−" } else { "+" },
        line.map(|l| l.to_string()).unwrap_or_default()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patch() -> Value {
        json!({"path": "a.rs", "hunks": [{"oldStart": 1, "newStart": 1, "lines": [
            {"type": "header", "content": "@@ -1,5 +1,5 @@"},
            {"type": "context", "content": "1"},
            {"type": "context", "content": "2"},
            {"type": "remove", "content": "3"},
            {"type": "add", "content": "3'"},
            {"type": "context", "content": "4"},
            {"type": "context", "content": "5"},
        ]}]})
    }

    #[test]
    fn attachment_resolves_hunk_context_and_drops_missing_lines() {
        let file = patch();
        let comments = vec![
            json!({"id": "a", "filePath": "a.rs", "side": "new", "lineNumber": 3, "body": "x"}),
            json!({"id": "b", "filePath": "a.rs", "side": "old", "lineNumber": 99, "body": "y"}),
            json!({"id": "c", "filePath": "gone.rs", "side": "new", "lineNumber": 1, "body": "z"}),
        ];
        let snapshot = build_attachment_snapshot("/repo", "uncommitted", " ", &comments, &|path| {
            (path == "a.rs").then_some(&file)
        })
        .unwrap();
        assert_eq!(snapshot.comment_count, 1);
        let resolved = &snapshot.attachment["comments"][0];
        assert_eq!(resolved["context"]["hunkHeader"], "@@ -1,5 +1,5 @@");
        assert_eq!(resolved["context"]["targetLine"]["content"], "3'");
        assert_eq!(resolved["context"]["lines"].as_array().unwrap().len(), 6);
        assert_eq!(snapshot.attachment["baseRef"], Value::Null);
    }

    #[test]
    fn draft_key_scopes_by_workspace_mode_base_and_whitespace() {
        assert_eq!(
            review_draft_key("srv", "ws 1", "/repo/", "base", "origin/main", true),
            "review:server=srv:workspace=ws%201:mode=base:base=origin%2Fmain:ignoreWhitespace=true"
        );
        assert_eq!(
            review_draft_key("srv", "", "/repo/", "all", "", false),
            "review:server=srv:cwd=%2Frepo:mode=all:base=:ignoreWhitespace=false"
        );
    }

    #[test]
    fn clearing_sent_keeps_comments_edited_meanwhile() {
        let mut draft = ReviewDraft::new("k");
        draft.save_comment(None, "a.rs", "new", 3, " body ", "1".into(), "t1".into());
        draft.save_comment(None, "a.rs", "new", 4, "other", "2".into(), "t1".into());
        let sent = draft.comments.clone();
        draft.save_comment(Some("2"), "a.rs", "new", 4, "edited", "x".into(), "t2".into());
        assert!(draft.clear_sent(&sent));
        assert_eq!(draft.comments.len(), 1);
        assert_eq!(draft.comments[0]["body"], "edited");
        assert!(!draft.save_comment(None, "a.rs", "new", 5, "   ", "3".into(), "t".into()));
    }

    #[test]
    fn load_validates_and_keeps_comments_added_before_load() {
        let mut draft = ReviewDraft::new("k");
        draft.save_comment(None, "b.rs", "old", 2, "early", "e".into(), "t".into());
        draft.load(Some(&json!([
            {"id": "s", "filePath": "a.rs", "side": "new", "lineNumber": 1, "body": "stored"},
            {"id": "bad", "filePath": "a.rs", "side": "mid", "lineNumber": 1, "body": "x"},
        ])));
        let ids: Vec<&str> = draft.comments.iter().map(|c| c["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["s", "e"]);
    }
}
