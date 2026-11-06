//! Port of `features/logs/terminal_snapshot.dart` (itself `protocol/src/terminal-snapshot.ts`).
//! Opcode 4 carries a JSON cell grid; this renders it to ANSI so the headless parser
//! rebuilds the screen. Opcode 5 (restore) is ANSI already.

use serde_json::Value;

use crate::features::workspace::rpc::{array_of, flag, int_of, object_of, str_of};

pub fn render_terminal_snapshot(state: &Value) -> String {
    let grid = array_of(state, "grid");
    let history = array_of(state, "scrollback");
    let rows: Vec<&Value> = history.iter().chain(grid.iter()).collect();
    let wraps: Vec<&Value> = array_of(state, "scrollbackWrapped")
        .iter()
        .chain(array_of(state, "gridWrapped").iter())
        .collect();
    let has_wraps = wraps.len() == rows.len();
    let cols = int_of(state, "cols").max(0) as usize;
    let mut out = String::new();
    if !has_wraps {
        out.push_str("\x1b[?7l");
    }
    let blank = Value::Object(serde_json::Map::from_iter([("char".to_owned(), Value::String(" ".into()))]));
    for (i, row) in rows.iter().enumerate() {
        let cells = row.as_array().map(Vec::as_slice).unwrap_or(&[]);
        let continues = has_wraps && wraps[i] == &Value::Bool(true);
        let mut length = cells.len();
        while length > 0 {
            let cell = &cells[length - 1];
            if cell.get("char").and_then(Value::as_str) != Some(" ") || styled(cell) {
                break;
            }
            length -= 1;
        }
        if continues && length < cols {
            length = cols;
        }
        let mut previous = "\x1b[0m".to_owned();
        for col in 0..length {
            let cell = cells.get(col).unwrap_or(&blank);
            let ch = cell.get("char").and_then(Value::as_str);
            if ch == Some("") {
                // Wide-glyph continuation, not a blank.
                continue;
            }
            let style = style(cell);
            if style != previous {
                out.push_str(&style);
                previous = style;
            }
            out.push_str(match ch {
                Some(text) if !text.is_empty() => text,
                _ => " ",
            });
        }
        if previous != "\x1b[0m" {
            out.push_str("\x1b[0m");
        }
        if i + 1 < rows.len() && !continues {
            out.push_str("\r\n");
        }
    }
    let cursor = object_of(state, "cursor");
    out.push_str("\x1b[0m");
    let cursor_style = str_of(cursor, "style");
    if !cursor_style.is_empty() {
        let base = match cursor_style {
            "block" => 1,
            "underline" => 3,
            _ => 5,
        };
        let steady = cursor.get("blink") == Some(&Value::Bool(false));
        out.push_str(&format!("\x1b[{} q", base + usize::from(steady)));
    }
    out.push_str(&format!("\x1b[{};{}H", int_of(cursor, "row") + 1, int_of(cursor, "col") + 1));
    out.push_str(if flag(cursor, "hidden") { "\x1b[?25l" } else { "\x1b[?25h" });
    if !has_wraps {
        out.push_str("\x1b[?7h");
    }
    out
}

fn styled(cell: &Value) -> bool {
    ["fg", "bg", "fgMode", "bgMode"].iter().any(|key| cell.get(*key).is_some_and(|v| !v.is_null()))
        || ["bold", "dim", "italic", "underline", "inverse", "strikethrough"]
            .iter()
            .any(|key| cell.get(*key) == Some(&Value::Bool(true)))
}

fn style(cell: &Value) -> String {
    let mut codes: Vec<i64> = vec![0];
    for (key, code) in [("bold", 1), ("dim", 2), ("italic", 3), ("underline", 4), ("inverse", 7), ("strikethrough", 9)] {
        if flag(cell, key) {
            codes.push(code);
        }
    }
    for background in [false, true] {
        let key = if background { "bg" } else { "fg" };
        let mode_key = if background { "bgMode" } else { "fgMode" };
        if cell.get(key).is_none_or(Value::is_null) || cell.get(mode_key).is_none_or(Value::is_null) {
            continue;
        }
        let value = int_of(cell, key);
        match int_of(cell, mode_key) {
            1 => codes.push(if value >= 8 {
                (if background { 100 } else { 90 }) + value - 8
            } else {
                (if background { 40 } else { 30 }) + value
            }),
            2 => codes.extend([if background { 48 } else { 38 }, 5, value]),
            3 => codes.extend([
                if background { 48 } else { 38 },
                2,
                (value >> 16) & 255,
                (value >> 8) & 255,
                value & 255,
            ]),
            _ => {}
        }
    }
    let joined: Vec<String> = codes.iter().map(i64::to_string).collect();
    format!("\x1b[{}m", joined.join(";"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renders_rows_with_wraps_and_cursor() {
        let state = json!({
            "cols": 4,
            "grid": [[{"char": "a"}, {"char": " "}], [{"char": "b", "bold": true}]],
            "gridWrapped": [false, false],
            "scrollback": [],
            "scrollbackWrapped": [],
            "cursor": {"row": 1, "col": 1, "style": "block", "blink": false}
        });
        let text = render_terminal_snapshot(&state);
        assert_eq!(text, "a\r\n\x1b[0;1mb\x1b[0m\x1b[0m\x1b[2 q\x1b[2;2H\x1b[?25h");
    }

    #[test]
    fn missing_wraps_disable_autowrap_and_colors_encode() {
        let state = json!({
            "cols": 2,
            "grid": [[{"char": "x", "fg": 9, "fgMode": 1}, {"char": "", }, {"char": "y", "bg": 0x102030, "bgMode": 3}]],
            "cursor": {"row": 0, "col": 0, "hidden": true}
        });
        let text = render_terminal_snapshot(&state);
        assert!(text.starts_with("\x1b[?7l"));
        assert!(text.contains("\x1b[0;91mx"));
        assert!(text.contains("\x1b[0;48;2;16;32;48my"));
        assert!(text.ends_with("\x1b[?25l\x1b[?7h"));
    }
}
