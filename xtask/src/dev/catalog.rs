use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{collections::BTreeMap, ops::Range, time::Duration};

#[derive(Debug)]
struct Literal {
    range: Range<usize>,
    value: Value,
    items: Vec<Literal>,
    fields: BTreeMap<String, Literal>,
}
struct Parser<'a> {
    source: &'a str,
    at: usize,
}
impl Parser<'_> {
    fn trivia(&mut self) -> Result<()> {
        loop {
            while self
                .source
                .as_bytes()
                .get(self.at)
                .is_some_and(u8::is_ascii_whitespace)
            {
                self.at += 1;
            }
            if self.source[self.at..].starts_with("//") {
                self.at = self.source[self.at..]
                    .find('\n')
                    .map(|n| self.at + n + 1)
                    .unwrap_or(self.source.len());
            } else if self.source[self.at..].starts_with("/*") {
                self.at += 2;
                let mut depth = 1;
                while depth > 0 {
                    ensure!(self.at < self.source.len(), "Unterminated catalog comment");
                    if self.source[self.at..].starts_with("/*") {
                        depth += 1;
                        self.at += 2;
                    } else if self.source[self.at..].starts_with("*/") {
                        depth -= 1;
                        self.at += 2;
                    } else {
                        self.at += self.source[self.at..].chars().next().unwrap().len_utf8();
                    }
                }
            } else {
                return Ok(());
            }
        }
    }
    fn character(&mut self) -> Result<char> {
        let c = self.source[self.at..]
            .chars()
            .next()
            .context("Unexpected end of catalog")?;
        self.at += c.len_utf8();
        Ok(c)
    }
    fn string(&mut self) -> Result<String> {
        let raw = self.source.as_bytes()[self.at] == b'r';
        if raw {
            self.at += 1;
        }
        let quote = self.character()?;
        ensure!(
            !self.source[self.at..].starts_with(&quote.to_string().repeat(2)),
            "Multiline catalog strings are unsupported"
        );
        let mut value = String::new();
        loop {
            let c = self.character()?;
            if c == quote {
                return Ok(value);
            }
            ensure!(c != '\n' && c != '\r', "Newline in catalog string");
            if raw || c != '\\' {
                value.push(c);
                continue;
            }
            let escaped = self.character()?;
            value.push(match escaped {
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                'b' => '\u{8}',
                'f' => '\u{c}',
                'v' => '\u{b}',
                '\\' | '\'' | '"' | '$' | '/' => escaped,
                'x' | 'u' => {
                    let braced = escaped == 'u' && self.source[self.at..].starts_with('{');
                    if braced {
                        self.at += 1;
                    }
                    let end = if braced {
                        self.at
                            + self.source[self.at..]
                                .find('}')
                                .context("Unterminated Unicode escape")?
                    } else {
                        self.at + if escaped == 'x' { 2 } else { 4 }
                    };
                    let hex = self
                        .source
                        .get(self.at..end)
                        .context("Truncated character escape")?;
                    ensure!(
                        !hex.is_empty()
                            && hex.len() <= 6
                            && hex.bytes().all(|c| c.is_ascii_hexdigit()),
                        "Invalid character escape"
                    );
                    let value = char::from_u32(u32::from_str_radix(hex, 16)?)
                        .context("Invalid Unicode scalar")?;
                    self.at = end + usize::from(braced);
                    value
                }
                _ => bail!("Unsupported catalog string escape"),
            });
        }
    }
    fn literal(&mut self) -> Result<Literal> {
        self.trivia()?;
        let start = self.at;
        let first = *self
            .source
            .as_bytes()
            .get(self.at)
            .context("Missing catalog literal")?;
        let mut items = Vec::new();
        let mut fields = BTreeMap::new();
        let value = if first == b'\''
            || first == b'"'
            || (first == b'r'
                && self
                    .source
                    .as_bytes()
                    .get(self.at + 1)
                    .is_some_and(|c| *c == b'\'' || *c == b'"'))
        {
            Value::String(self.string()?)
        } else if first == b'[' || first == b'{' {
            self.at += 1;
            let close = if first == b'[' { b']' } else { b'}' };
            self.trivia()?;
            while self.source.as_bytes().get(self.at) != Some(&close) {
                let item = self.literal()?;
                if first == b'{' {
                    let key = item
                        .value
                        .as_str()
                        .context("Catalog map keys must be strings")?
                        .to_owned();
                    self.trivia()?;
                    ensure!(self.character()? == ':', "Expected ':' in catalog");
                    ensure!(
                        fields.insert(key, self.literal()?).is_none(),
                        "Duplicate catalog map key"
                    );
                } else {
                    items.push(item);
                }
                self.trivia()?;
                if self.source.as_bytes().get(self.at) == Some(&close) {
                    break;
                }
                ensure!(self.character()? == ',', "Expected ',' in catalog");
                self.trivia()?;
            }
            self.at += 1;
            if first == b'[' {
                Value::Array(items.iter().map(|v| v.value.clone()).collect())
            } else {
                Value::Object(
                    fields
                        .iter()
                        .map(|(k, v)| (k.clone(), v.value.clone()))
                        .collect(),
                )
            }
        } else {
            while self
                .source
                .as_bytes()
                .get(self.at)
                .is_some_and(|c| c.is_ascii_alphanumeric() || matches!(*c, b'.' | b'-' | b'+'))
            {
                self.at += 1;
            }
            ensure!(self.at > start, "Expected literal at byte {start}");
            let value: Value = serde_json::from_str(&self.source[start..self.at])?;
            ensure!(
                value.is_boolean() || value.is_null() || value.is_number(),
                "Expected scalar catalog literal"
            );
            value
        };
        Ok(Literal {
            range: start..self.at,
            value,
            fields,
            items,
        })
    }
}
/// ACP provider catalog shipped by the Rust settings feature (JSON array).
const CATALOG: &str = "packages/gui/src/features/settings/model/acp_catalog.json";

fn catalog(source: &str) -> Result<Vec<Literal>> {
    let root = Parser { source, at: 0 }.literal()?;
    ensure!(root.value.is_array(), "The ACP catalog must be a JSON array");
    for entry in &root.items {
        ensure!(entry.value.is_object(), "Catalog entries must be maps");
        if let Some(command) = entry.fields.get("command") {
            ensure!(
                command.value.is_array() && command.items.iter().all(|a| a.value.is_string()),
                "Catalog commands must be string arrays"
            );
        }
    }
    Ok(root.items)
}
fn package(command: &[Value]) -> Option<(&'static str, usize, bool)> {
    let first = command.first()?.as_str()?;
    let start = match first {
        "npm" if command.get(1)?.as_str()? == "exec" => 2,
        "npx" | "uvx" => 1,
        _ => return None,
    };
    if first == "uvx" {
        if let Some(index) = command.iter().position(|v| v == "--from") {
            return (index + 1 < command.len()).then_some(("pypi", index + 1, true));
        }
    }
    (start..command.len())
        .find(|&index| {
            command[index]
                .as_str()
                .is_some_and(|arg| !arg.starts_with('-'))
        })
        .map(|index| (if first == "uvx" { "pypi" } else { "npm" }, index, false))
}
fn split_package(spec: &str) -> (&str, Option<&str>) {
    for delimiter in ["==", ">="] {
        if let Some(index) = spec.find(delimiter) {
            return (&spec[..index], Some(&spec[index..]));
        }
    }
    if let Some(index) = spec.rfind('@').filter(|&index| index > 0) {
        (&spec[..index], Some(&spec[index + 1..]))
    } else {
        (spec, None)
    }
}
fn exact(selector: &str) -> bool {
    semver::Version::parse(selector.strip_prefix("==").unwrap_or(selector)).is_ok()
}
fn json_string(value: &str) -> Result<String> {
    Ok(serde_json::to_string(value)?)
}
async fn latest(client: &reqwest::Client, registry: &str, name: &str) -> Result<String> {
    let mut url = reqwest::Url::parse(if registry == "npm" {
        "https://registry.npmjs.org/"
    } else {
        "https://pypi.org/pypi/"
    })?;
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("Invalid registry URL"))?
        .pop_if_empty()
        .push(name)
        .push(if registry == "npm" { "latest" } else { "json" });
    let response: Value = client
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let version = if registry == "npm" {
        response.get("version")
    } else {
        response.pointer("/info/version")
    };
    Ok(version
        .and_then(Value::as_str)
        .context("Registry response did not include version")?
        .to_owned())
}
pub(super) async fn run(args: &[String]) -> Result<()> {
    for arg in args {
        ensure!(
            matches!(
                arg.as_str(),
                "--json" | "--fail-on-drift" | "--no-network" | "--update" | "--help" | "-h"
            ),
            "Unknown catalog argument {arg}"
        );
    }
    if args.iter().any(|v| matches!(v.as_str(), "--help" | "-h")) {
        println!(
            "cargo xtask maintenance acp-catalog [--json] [--fail-on-drift] [--no-network] [--update]"
        );
        return Ok(());
    }
    let has = |s: &str| args.iter().any(|v| v == s);
    ensure!(
        !has("--update") || !has("--no-network"),
        "--update requires registry access"
    );
    let path = crate::common::root()?.join(CATALOG);
    let source = std::fs::read_to_string(&path)?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    let mut results = Vec::new();
    let mut replacements = Vec::new();
    for entry in catalog(&source)? {
        let command = entry.value.get("command").and_then(Value::as_array);
        let Some((registry, index, uses_from)) = command.and_then(|c| package(c)) else {
            results.push(json!({"id":entry.value["id"],"title":entry.value["title"],"command":entry.value["command"],"checked":false,"status":"skipped","reasons":["not a supported package-runner command"]}));
            continue;
        };
        let command = command.unwrap();
        let (name, selector) = split_package(command[index].as_str().unwrap());
        let (latest, error) = if has("--no-network") {
            (None, None)
        } else {
            match latest(&client, registry, name).await {
                Ok(version) => (Some(version), None),
                Err(error) => (None, Some(error.to_string())),
            }
        };
        let mut reasons = Vec::new();
        if error.is_some() {
            reasons.push("registry version lookup failed");
        }
        if selector.is_none() {
            reasons.push("missing package version selector");
        } else if !selector.is_some_and(exact) {
            reasons.push("command selector is not an exact version pin");
        }
        if let Some(latest) = &latest {
            if selector.is_some_and(exact)
                && selector.map(|v| v.strip_prefix("==").unwrap_or(v)) != Some(latest.as_str())
            {
                reasons.push("command selector is not latest registry version");
            }
            if entry.value["version"].as_str() != Some(latest) {
                reasons.push("catalog version is not latest registry version");
            }
        }
        let updated = latest.as_ref().map(|latest| {
            let mut result = command.clone();
            result[index] = json!(format!(
                "{name}{}{latest}",
                if registry == "pypi" && uses_from {
                    "=="
                } else {
                    "@"
                }
            ));
            result
        });
        if has("--update") {
            if let (Some(latest), Some(updated)) = (&latest, &updated) {
                let version = entry
                    .fields
                    .get("version")
                    .context("Cannot update entry without version")?;
                let command_literal = entry.fields.get("command").unwrap();
                replacements.push((version.range.clone(), json_string(latest)?));
                if updated[index] != command[index] {
                    replacements.push((
                        command_literal.items[index].range.clone(),
                        json_string(updated[index].as_str().unwrap())?,
                    ));
                }
            }
        }
        results.push(json!({"id":entry.value["id"],"title":entry.value["title"],"registry":registry,"packageName":name,"selector":selector,"catalogVersion":entry.value["version"],"latestVersion":latest,"currentCommand":command,"updatedCommand":updated,"checked":true,"status":if reasons.is_empty(){"ok"}else{"drift"},"reasons":reasons,"registryError":error}));
    }
    if has("--update") {
        let mut updated = source;
        replacements.sort_by(|a, b| b.0.start.cmp(&a.0.start));
        for (range, text) in replacements {
            updated.replace_range(range, &text);
        }
        std::fs::write(path, updated)?;
    }
    let drift = results.iter().filter(|r| r["status"] == "drift").count();
    if has("--json") {
        println!("{}", serde_json::to_string_pretty(&results)?);
    } else {
        println!(
            "ACP catalog version drift\nchecked: {}\ndrift: {drift}\nskipped: {}",
            results.iter().filter(|r| r["checked"] == true).count(),
            results.iter().filter(|r| r["checked"] == false).count()
        );
        for result in results.iter().filter(|r| r["status"] != "ok") {
            println!("{}: {}", result["id"], serde_json::to_string(result)?);
        }
    }
    ensure!(
        !has("--fail-on-drift") || drift == 0,
        "ACP catalog has {drift} entries with drift"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_literal_ranges_without_evaluating_code() {
        let source = "[{\"id\": \"é\", \"command\": [\"npx\", \"@vendor/acp@1.2.3\"], \"version\": \"1.2.3\", \"metadata\": {\"enabled\": true}}]\n";
        let entries = catalog(source).unwrap();
        assert_eq!(entries[0].value["id"], "é");
        let range = entries[0].fields["command"].items[1].range.clone();
        assert_eq!(&source[range], "\"@vendor/acp@1.2.3\"");
        assert!(catalog(&source.replace("true", "unsafe()")).is_err());
        assert!(catalog("{\"id\": \"not a list\"}").is_err());
    }
    #[test]
    fn registry_specs_preserve_scopes_and_python_pins() {
        assert_eq!(
            split_package("@vendor/acp@1.2.3"),
            ("@vendor/acp", Some("1.2.3"))
        );
        assert_eq!(split_package("acp==1.2.3"), ("acp", Some("==1.2.3")));
        assert!(exact("==1.2.3"));
        assert!(!exact(">=1.2.3"));
        assert_eq!(
            package(&[
                json!("uvx"),
                json!("--from"),
                json!("acp==1.2.3"),
                json!("acp")
            ]),
            Some(("pypi", 2, true))
        );
        assert_eq!(
            package(&[
                json!("npm"),
                json!("exec"),
                json!("--yes"),
                json!("acp@1.2.3")
            ]),
            Some(("npm", 3, false))
        );
    }
}
