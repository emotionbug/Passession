//! Port of `composer/forge_auto_attach.dart` plus the forge parts of `forge_picker.dart`:
//! repository issue/change-request URLs typed into the composer resolve through forge search
//! and attach in source order. The Dart futures/timers become an explicit state machine; the
//! owner runs the 300ms debounce timer and the search requests.

use super::json::*;
use serde_json::Value;
use std::collections::HashSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ForgeRef {
    pub change_request: bool,
    pub number: i64,
}

impl ForgeRef {
    pub fn kind(&self) -> &'static str {
        if self.change_request { "change_request" } else { "issue" }
    }
    pub fn key(&self) -> String {
        format!("{}:{}", self.kind(), self.number)
    }
}

/// protocol git-remote.ts normalizeHost.
pub fn normalize_git_host(host: &str) -> String {
    host.trim().trim_end_matches('.').to_lowercase()
}

fn normalize_remote_path(path: &str) -> Option<String> {
    let mut normalized = path.trim().trim_matches('/').to_owned();
    if let Some(stripped) = normalized.strip_suffix(".git") {
        normalized = stripped.to_owned();
    }
    if normalized.is_empty() { None } else { Some(normalized) }
}

fn valid_remote_host(host: &str) -> bool {
    let bytes = host.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    let ok = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    if !ok(bytes[0]) || !ok(bytes[bytes.len() - 1]) {
        return false;
    }
    bytes.iter().all(|&c| ok(c) || c == b'.' || c == b'_' || c == b'-')
}

fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = input.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Minimal URL split: (scheme, host, path) for `scheme://[userinfo@]host[:port]/path?query#frag`.
fn split_url(url: &str) -> Option<(String, String, String)> {
    let (scheme, rest) = url.split_once("://")?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    let host_port = authority.rsplit_once('@').map(|(_, h)| h).unwrap_or(authority);
    let host = if host_port.starts_with('[') {
        host_port.split(']').next().unwrap_or("").trim_start_matches('[').to_owned()
    } else {
        host_port.split(':').next().unwrap_or("").to_owned()
    };
    let remainder = &rest[end..];
    let path_end = remainder.find(['?', '#']).unwrap_or(remainder.len());
    Some((scheme.to_lowercase(), host, remainder[..path_end].to_owned()))
}

/// protocol git-remote.ts parseGitRemoteLocation.
pub fn parse_git_remote_location(remote_url: &str) -> Option<(String, String)> {
    let trimmed = remote_url.trim();
    if trimmed.is_empty() {
        return None;
    }
    if !trimmed.contains("://") {
        // scp-style `user@host:path`
        let (user_host, path) = trimmed.split_once(':')?;
        let (user, host) = user_host.split_once('@')?;
        if user.is_empty() || user.contains('@') || host.is_empty() || path.is_empty() {
            return None;
        }
        let host = normalize_git_host(host);
        let path = normalize_remote_path(path)?;
        return valid_remote_host(&host).then_some((host, path));
    }
    let (scheme, host, path) = split_url(trimmed)?;
    if !matches!(scheme.as_str(), "https" | "http" | "ssh") {
        return None;
    }
    let host = normalize_git_host(&host);
    let decoded = percent_decode(&path)?;
    let path = normalize_remote_path(&decoded)?;
    valid_remote_host(&host).then_some((host, path))
}

/// protocol forge-manifest.ts FORGE_DEFINITIONS cloudHosts.
const FORGE_CLOUD_HOSTS: &[(&str, &[&str])] = &[
    ("github", &["github.com", "ssh.github.com"]),
    ("gitlab", &["gitlab.com"]),
    ("gitea", &["gitea.com"]),
    ("codeberg", &["codeberg.org"]),
];

/// git/forges urlGrammar.referencePaths, in CLIENT_FORGE_LOGIC_MODULES order.
const FORGE_REFERENCE_PATHS: &[(&str, bool, &str)] = &[
    ("github", true, "/pull/"),
    ("github", false, "/issues/"),
    ("gitlab", true, "/-/merge_requests/"),
    ("gitlab", false, "/-/issues/"),
];

fn web_urls(body: &str) -> Vec<&str> {
    // https?://[^\s<>"'`)\]]+ (case-insensitive scheme)
    let mut out = Vec::new();
    let lower = body.to_ascii_lowercase();
    let mut search = 0;
    while search < body.len() {
        let next = [lower[search..].find("http://"), lower[search..].find("https://")]
            .into_iter()
            .flatten()
            .min();
        let Some(offset) = next else { break };
        let start = search + offset;
        let mut end = start;
        for (index, ch) in body[start..].char_indices() {
            if ch.is_whitespace() || matches!(ch, '<' | '>' | '"' | '\'' | '`' | ')' | ']') {
                break;
            }
            end = start + index + ch.len_utf8();
        }
        let scheme_len = if lower[start..].starts_with("https://") { 8 } else { 7 };
        if end > start + scheme_len {
            out.push(&body[start..end]);
        }
        search = end.max(start + scheme_len);
    }
    out
}

/// git/forge-refs.ts extractForgeRefs.
pub fn extract_forge_refs(text: &str, remote_url: Option<&str>) -> Vec<ForgeRef> {
    let Some((remote_host, remote_path)) = remote_url.and_then(parse_git_remote_location) else {
        return Vec::new();
    };
    let body = text.trim();
    if body.is_empty() {
        return Vec::new();
    }
    let mut web_hosts: HashSet<String> = HashSet::from([remote_host.clone()]);
    let mut forge_ids: HashSet<&str> = HashSet::new();
    for (forge, hosts) in FORGE_CLOUD_HOSTS {
        let hosts: Vec<String> = hosts.iter().map(|h| normalize_git_host(h)).collect();
        if hosts.contains(&remote_host) {
            forge_ids.insert(forge);
            web_hosts.extend(hosts);
        }
    }
    let mut refs = Vec::new();
    let mut seen = HashSet::new();
    for raw in web_urls(body) {
        let raw = raw.trim_end_matches(['.', ',', ';', ':', '!']);
        let Some((scheme, host, path)) = split_url(raw) else { continue };
        if scheme != "http" && scheme != "https" {
            continue;
        }
        if !web_hosts.contains(&normalize_git_host(&host)) {
            continue;
        }
        let Some(pathname) = percent_decode(&path) else { continue };
        for (forge, change, infix) in FORGE_REFERENCE_PATHS {
            if !forge_ids.is_empty() && !forge_ids.contains(forge) {
                continue;
            }
            let prefix = format!("/{remote_path}{infix}").to_lowercase();
            if !pathname.to_lowercase().starts_with(&prefix) {
                continue;
            }
            let tail = &pathname[prefix.len()..];
            let digits: String = tail.chars().take_while(char::is_ascii_digit).collect();
            let after = &tail[digits.len()..];
            if digits.is_empty() || !(after.is_empty() || after.starts_with('/')) {
                continue;
            }
            let Ok(number) = digits.parse::<i64>() else { continue };
            if number <= 0 {
                continue;
            }
            let reference = ForgeRef { change_request: *change, number };
            if seen.insert(reference.key()) {
                refs.push(reference);
            }
            break;
        }
    }
    refs
}

/// forge-auto-attach.ts attachmentKey: `kind:number` of a forge attachment, None otherwise.
pub fn forge_attachment_key(attachment: &Value) -> Option<String> {
    let kind = attachment.get("type").filter(|v| !v.is_null()).unwrap_or(get(attachment, "kind"));
    let kind = s(kind);
    let change = kind == "forge_change_request" || kind == "github_pr";
    let issue = kind == "forge_issue" || kind == "github_issue";
    if !change && !issue {
        return None;
    }
    let number = attachment
        .get("number")
        .filter(|v| !v.is_null())
        .unwrap_or(get(get(attachment, "item"), "number"));
    Some(format!("{}:{}", if change { "change_request" } else { "issue" }, display(number)))
}

/// The composer state the auto-attach follows.
#[derive(Clone, Debug, PartialEq)]
pub struct ForgeInput {
    pub text: String,
    pub remote_url: Option<String>,
    pub attachments: Vec<Value>,
    pub connected: bool,
    pub server_id: String,
    pub cwd: String,
}

/// One forge search the owner must run (`searchForgeItems(cwd, number)`).
#[derive(Clone, Debug)]
pub struct ForgeSearch {
    pub lookup: u64,
    pub index: usize,
    pub cwd: String,
    pub query: String,
}

struct Lookup {
    id: u64,
    refs: Vec<ForgeRef>,
    initial: ForgeInput,
    settled: Vec<bool>,
    items: Vec<Option<Value>>,
    next: usize,
    complete: bool,
}

struct Run {
    keys: Vec<String>,
    refs: Vec<ForgeRef>,
    initial: ForgeInput,
    lookup: Option<u64>,
}

/// What the owner does after an auto-attach transition.
#[derive(Default, Debug)]
pub struct ForgeEffects {
    /// (Re)start the 300ms debounce for the current run; `false` with `cancel_timer` stops it.
    pub start_timer: bool,
    pub cancel_timer: bool,
    pub searches: Vec<ForgeSearch>,
    /// Search items to attach now, in source order.
    pub attach: Vec<Value>,
}

#[derive(Default)]
pub struct ForgeAutoAttach {
    latest: Option<ForgeInput>,
    removed: HashSet<String>,
    lookups: Vec<Lookup>,
    run: Option<Run>,
    previous_target: Option<(String, String)>,
    relevance_deps: Option<String>,
    candidate_deps: Option<String>,
    next_lookup: u64,
}

fn attached(attachments: &[Value], reference: &ForgeRef) -> bool {
    let key = reference.key();
    attachments.iter().any(|a| forge_attachment_key(a).as_deref() == Some(key.as_str()))
}

fn present(reference: &ForgeRef, input: &ForgeInput) -> bool {
    extract_forge_refs(&input.text, input.remote_url.as_deref())
        .iter()
        .any(|candidate| candidate.key() == reference.key())
}

fn same_target(a: &ForgeInput, b: &ForgeInput) -> bool {
    a.server_id == b.server_id && a.cwd == b.cwd && a.remote_url == b.remote_url
}

impl ForgeAutoAttach {
    /// markForgeAttachmentRemoved: a removed reference never re-attaches.
    pub fn mark_removed(&mut self, attachment: &Value) {
        if let Some(key) = forge_attachment_key(attachment) {
            self.removed.insert(key);
        }
    }

    fn relevance_key(&self, input: &ForgeInput) -> String {
        let mut keys: Vec<String> = extract_forge_refs(&input.text, input.remote_url.as_deref())
            .iter()
            .map(ForgeRef::key)
            .filter(|key| !self.removed.contains(key))
            .collect();
        keys.sort();
        keys.join("|")
    }

    fn candidate_refs(&self, input: &ForgeInput) -> Vec<ForgeRef> {
        extract_forge_refs(&input.text, input.remote_url.as_deref())
            .into_iter()
            .filter(|r| !self.removed.contains(&r.key()) && !attached(&input.attachments, r))
            .collect()
    }

    fn relevant(&self, reference: &ForgeRef, initial: &ForgeInput, current: &ForgeInput) -> bool {
        current.connected
            && !self.removed.contains(&reference.key())
            && same_target(initial, current)
            && present(reference, current)
    }

    pub fn update(&mut self, input: ForgeInput) -> ForgeEffects {
        let mut effects = ForgeEffects::default();
        self.latest = Some(input.clone());
        let target = format!("{:?}|{}|{}|{}", input.remote_url, input.connected, input.server_id, input.cwd);
        let relevance = format!("{}|{target}", self.relevance_key(&input));
        if self.relevance_deps.as_deref() != Some(relevance.as_str()) {
            self.relevance_deps = Some(relevance);
            let ids: Vec<u64> = self.lookups.iter().map(|l| l.id).collect();
            for id in ids {
                self.invalidate_irrelevant(id, &input, &mut effects);
            }
        }
        let candidates = format!(
            "{}|{target}",
            self.candidate_refs(&input).iter().map(ForgeRef::key).collect::<Vec<_>>().join("|")
        );
        if self.candidate_deps.as_deref() == Some(candidates.as_str()) {
            return effects;
        }
        self.candidate_deps = Some(candidates);
        self.cleanup_run(&mut effects);
        self.start_run(&mut effects);
        effects
    }

    /// The previous run's cleanup (`_run`'s returned closure).
    fn cleanup_run(&mut self, effects: &mut ForgeEffects) {
        let Some(run) = self.run.take() else { return };
        effects.cancel_timer = true;
        let Some(lookup) = run.lookup else { return };
        let current = self.latest.clone().expect("latest input");
        let current_keys: Vec<String> = self.candidate_refs(&current).iter().map(ForgeRef::key).collect();
        let retained: Vec<String> = run.keys.iter().filter(|k| current_keys.contains(k)).cloned().collect();
        if current_keys != retained {
            self.invalidate(lookup, &run.keys, effects);
        } else {
            self.invalidate_irrelevant(lookup, &current, effects);
        }
    }

    fn start_run(&mut self, effects: &mut ForgeEffects) {
        let initial = self.latest.clone().expect("latest input");
        let previous = self.previous_target.replace((initial.server_id.clone(), initial.cwd.clone()));
        if let Some((server, cwd)) = previous
            && !cwd.trim().is_empty()
            && !initial.cwd.trim().is_empty()
            && (server != initial.server_id || cwd != initial.cwd)
        {
            for reference in extract_forge_refs(&initial.text, initial.remote_url.as_deref()) {
                self.removed.insert(reference.key());
            }
        }
        if !initial.connected || initial.cwd.trim().is_empty() {
            return;
        }
        let refs: Vec<ForgeRef> = extract_forge_refs(&initial.text, initial.remote_url.as_deref())
            .into_iter()
            .filter(|r| {
                !self.removed.contains(&r.key())
                    && !self.lookups.iter().any(|l| l.has_pending(&r.key()))
                    && !attached(&initial.attachments, r)
            })
            .collect();
        if refs.is_empty() {
            return;
        }
        self.run = Some(Run { keys: refs.iter().map(ForgeRef::key).collect(), refs, initial, lookup: None });
        effects.cancel_timer = false;
        effects.start_timer = true;
    }

    /// The 300ms debounce elapsed: start the lookup and its searches.
    pub fn timer_fired(&mut self) -> ForgeEffects {
        let mut effects = ForgeEffects::default();
        let Some(run) = self.run.as_mut() else { return effects };
        if run.lookup.is_some() {
            return effects;
        }
        self.next_lookup += 1;
        let id = self.next_lookup;
        run.lookup = Some(id);
        let refs = run.refs.clone();
        let initial = run.initial.clone();
        self.lookups.push(Lookup {
            id,
            settled: vec![false; refs.len()],
            items: vec![None; refs.len()],
            refs: refs.clone(),
            initial,
            next: 0,
            complete: false,
        });
        let snapshot = self.latest.clone().expect("latest input");
        for (index, reference) in refs.iter().enumerate() {
            if !snapshot.connected || !present(reference, &snapshot) {
                self.settle(id, index, None, &mut effects);
            } else {
                effects.searches.push(ForgeSearch {
                    lookup: id,
                    index,
                    cwd: snapshot.cwd.clone(),
                    query: reference.number.to_string(),
                });
            }
        }
        effects
    }

    /// A forge search finished (`items` from searchForgeItems, or Err).
    pub fn search_finished(&mut self, lookup: u64, index: usize, snapshot_cwd: &str, items: Result<Vec<Value>, String>) -> ForgeEffects {
        let mut effects = ForgeEffects::default();
        let Some(entry) = self.lookups.iter().find(|l| l.id == lookup) else { return effects };
        let Some(reference) = entry.refs.get(index).copied() else { return effects };
        let initial = entry.initial.clone();
        let item = items.ok().and_then(|items| {
            items.into_iter().find(|candidate| {
                s(get(candidate, "kind")) == reference.kind() && get(candidate, "number").as_i64() == Some(reference.number)
            })
        });
        let current = self.latest.clone().expect("latest input");
        let item = item.filter(|_| {
            !self.removed.contains(&reference.key())
                && same_target(&initial, &current)
                && current.cwd == snapshot_cwd
                && present(&reference, &current)
                && !attached(&current.attachments, &reference)
        });
        self.settle(lookup, index, item, &mut effects);
        effects
    }

    fn settle(&mut self, lookup: u64, index: usize, item: Option<Value>, effects: &mut ForgeEffects) {
        let Some(entry) = self.lookups.iter_mut().find(|l| l.id == lookup) else { return };
        if entry.settled[index] {
            return;
        }
        entry.settled[index] = true;
        entry.items[index] = item;
        self.drain(lookup, effects);
    }

    fn drain(&mut self, lookup: u64, effects: &mut ForgeEffects) {
        let latest_attachments = self.latest.as_ref().map(|l| l.attachments.clone()).unwrap_or_default();
        let removed = self.removed.clone();
        let Some(position) = self.lookups.iter().position(|l| l.id == lookup) else { return };
        let entry = &mut self.lookups[position];
        while entry.next < entry.refs.len() && entry.settled[entry.next] {
            let item = entry.items[entry.next].take();
            let reference = entry.refs[entry.next];
            entry.next += 1;
            if let Some(item) = item {
                let already = attached(&latest_attachments, &reference)
                    || effects.attach.iter().any(|a| s(get(a, "kind")) == reference.kind() && get(a, "number").as_i64() == Some(reference.number));
                if !removed.contains(&reference.key()) && !already {
                    effects.attach.push(item);
                }
            }
        }
        if !entry.complete && entry.next == entry.refs.len() {
            entry.complete = true;
            self.lookups.remove(position);
        }
    }

    fn invalidate(&mut self, lookup: u64, keys: &[String], effects: &mut ForgeEffects) {
        let Some(entry) = self.lookups.iter_mut().find(|l| l.id == lookup) else { return };
        for index in 0..entry.refs.len() {
            if !keys.contains(&entry.refs[index].key()) || index < entry.next {
                continue;
            }
            entry.settled[index] = true;
            entry.items[index] = None;
        }
        self.drain(lookup, effects);
    }

    fn invalidate_irrelevant(&mut self, lookup: u64, current: &ForgeInput, effects: &mut ForgeEffects) {
        let Some(entry) = self.lookups.iter().find(|l| l.id == lookup) else { return };
        let keys: Vec<String> = entry
            .refs
            .iter()
            .enumerate()
            .filter(|(index, reference)| {
                !(self.relevant(reference, &entry.initial, current)
                    && (entry.settled[*index] || !attached(&current.attachments, reference)))
            })
            .map(|(_, reference)| reference.key())
            .collect();
        self.invalidate(lookup, &keys, effects);
    }
}

impl Lookup {
    fn has_pending(&self, key: &str) -> bool {
        self.refs
            .iter()
            .position(|r| r.key() == key)
            .is_some_and(|index| !self.settled[index])
    }
}

/// use-forge-search-query.ts: request fields and response normalization for a forge search
/// (`forge.search` or the legacy `github_search`, whose `pr` items become change requests).
pub fn forge_search_request(legacy: bool, cwd: &str, query: &str) -> (Value, &'static str) {
    let kind = if legacy { "github_search_request" } else { "forge.search.request" };
    let response = if legacy { "github_search_response" } else { "forge.search.response" };
    (
        serde_json::json!({"type": kind, "cwd": cwd, "query": query.trim(), "limit": 20}),
        response,
    )
}

pub fn forge_search_items(legacy: bool, result: &Value) -> Vec<Value> {
    maps(get(result, "items"))
        .into_iter()
        .map(|item| {
            if !legacy {
                return item;
            }
            let kind = if s(get(&item, "kind")) == "pr" { "change_request" } else { "issue" };
            merged(&item, &serde_json::json!({"kind": kind}))
        })
        .collect()
}
