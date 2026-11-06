//! Read-only parser for Android `SharedPreferences` XML (`<map>` of `<string>`, `<boolean>`,
//! `<int>`, `<long>`, `<float>` and `<set>` entries) used to import Flutter preferences.
use serde_json::{Map, Number, Value};

fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let Some(end) = rest.find(';') else { break };
        let entity = &rest[1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
                .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                .or_else(|| entity.strip_prefix('#').and_then(|dec| dec.parse().ok()))
                .and_then(char::from_u32),
        };
        match decoded {
            Some(ch) => {
                out.push(ch);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn attribute<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let marker = format!("{name}=\"");
    let start = tag.find(&marker)? + marker.len();
    let end = tag[start..].find('"')? + start;
    Some(&tag[start..end])
}

pub(crate) fn parse(text: &str) -> Map<String, Value> {
    let mut values = Map::new();
    let mut rest = text;
    while let Some(open) = rest.find('<') {
        rest = &rest[open + 1..];
        let Some(close) = rest.find('>') else { break };
        let tag = &rest[..close];
        rest = &rest[close + 1..];
        let kind = tag.split_whitespace().next().unwrap_or_default();
        let Some(name) = attribute(tag, "name").map(unescape) else {
            continue;
        };
        let self_closing = tag.ends_with('/');
        match kind {
            "string" if !self_closing => {
                let Some(end) = rest.find("</string>") else { break };
                values.insert(name, Value::String(unescape(&rest[..end])));
                rest = &rest[end + "</string>".len()..];
            }
            "string" => {
                values.insert(name, Value::String(String::new()));
            }
            "boolean" => {
                if let Some(value) = attribute(tag, "value") {
                    values.insert(name, Value::Bool(value == "true"));
                }
            }
            "int" | "long" => {
                if let Some(value) = attribute(tag, "value").and_then(|v| v.parse::<i64>().ok()) {
                    values.insert(name, Value::Number(value.into()));
                }
            }
            "float" => {
                if let Some(value) = attribute(tag, "value")
                    .and_then(|v| v.parse::<f64>().ok())
                    .and_then(Number::from_f64)
                {
                    values.insert(name, Value::Number(value));
                }
            }
            "set" if !self_closing => {
                let Some(end) = rest.find("</set>") else { break };
                let mut items = Vec::new();
                let mut body = &rest[..end];
                while let Some(start) = body.find("<string>") {
                    let after = &body[start + "<string>".len()..];
                    let Some(stop) = after.find("</string>") else { break };
                    items.push(Value::String(unescape(&after[..stop])));
                    body = &after[stop + "</string>".len()..];
                }
                values.insert(name, Value::Array(items));
                rest = &rest[end + "</set>".len()..];
            }
            _ => {}
        }
    }
    values
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_flutter_preferences() {
        let xml = r#"<?xml version='1.0' encoding='utf-8' standalone='yes' ?>
<map>
    <string name="flutter.paseo.chat.drafts.v1">{&quot;drafts&quot;:{}} &amp; &#228;</string>
    <boolean name="flutter.paseo.relayProtected.k" value="true" />
    <long name="flutter.count" value="3" />
    <string name="flutter.empty" />
</map>"#;
        let values = parse(xml);
        assert_eq!(values["flutter.paseo.chat.drafts.v1"], "{\"drafts\":{}} & ä");
        assert_eq!(values["flutter.paseo.relayProtected.k"], true);
        assert_eq!(values["flutter.count"], 3);
        assert_eq!(values["flutter.empty"], "");
    }
}
