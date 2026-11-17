//! What changed between two published core states.
//!
//! The core republishes its whole state (at most every 16 ms) for any directory, host or
//! preference change; most of those publications only move a streaming session's activity
//! timestamp. The app classifies each publication once per batch so retained layers
//! (`common::layer`) repaint only when something they present changed.
//!
//! Each host record carries the core's `directoryRevision`: equal revisions mean equal
//! `projects`, `workspaces`, `agents` and `cachedAt`, so those rows are not compared.

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value};

use super::json as j;

/// Agent fields every stream event refreshes. A change limited to these moves recency
/// (sorting, "updated 2m ago"), not presented content.
const AGENT_ACTIVITY: [&str; 2] = ["updatedAt", "lastActivityAt"];
/// Host connection telemetry refreshed by probes and heartbeats.
const HOST_ACTIVITY: [&str; 3] = ["latencyMs", "probes", "cachedAt"];
/// Saved window layouts: each shell owns its own layout and restores it only once.
const LAYOUT_KEYS: [&str; 2] = ["layout", "windowLayouts"];
/// Core metadata of a host record; see the module documentation.
const DIRECTORY_REVISION: &str = "directoryRevision";
/// Host fields an unchanged directory revision vouches for (and the revision itself).
const DIRECTORY_FIELDS: [&str; 5] = [
    "projects",
    "workspaces",
    "agents",
    "cachedAt",
    DIRECTORY_REVISION,
];

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StateDelta {
    /// Presented content changed: preferences and other shared slices, the host set, a host
    /// record, or an agent beyond its activity timestamps.
    pub content: bool,
    /// Host connection telemetry changed (latency, probes, cache time) and nothing else.
    pub host_activity: bool,
    /// Agents whose only change was an activity timestamp, as `(server id, agent id)`.
    pub agent_activity: HashSet<(String, String)>,
}

impl StateDelta {
    pub fn between(old: &Value, new: &Value) -> Self {
        let mut delta = Self::default();
        if std::ptr::eq(old, new) {
            return delta;
        }
        let (Some(old_map), Some(new_map)) = (old.as_object(), new.as_object()) else {
            delta.content = old != new;
            return delta;
        };
        let keys = old_map.keys().chain(new_map.keys());
        for key in keys {
            if key == "hosts" || LAYOUT_KEYS.contains(&key.as_str()) {
                continue;
            }
            if old_map.get(key) != new_map.get(key) {
                delta.content = true;
                return delta;
            }
        }
        delta.hosts(j::array(old, "hosts"), j::array(new, "hosts"));
        delta
    }

    /// Nothing any layer presents changed.
    pub fn is_empty(&self) -> bool {
        !self.content && !self.host_activity && self.agent_activity.is_empty()
    }

    /// A later publication of the same batch.
    pub fn merge(&mut self, other: Self) {
        self.content |= other.content;
        self.host_activity |= other.host_activity;
        self.agent_activity.extend(other.agent_activity);
    }

    /// Content, or the activity of this one agent, changed.
    pub fn touches_agent(&self, server_id: &str, agent_id: &str) -> bool {
        self.content
            || self
                .agent_activity
                .iter()
                .any(|(server, agent)| server == server_id && agent == agent_id)
    }

    fn hosts(&mut self, old: &[Value], new: &[Value]) {
        if old.len() != new.len() {
            self.content = true;
            return;
        }
        for (old_host, new_host) in old.iter().zip(new) {
            let (Some(old_record), Some(new_record)) = (old_host.as_object(), new_host.as_object())
            else {
                if old_host != new_host {
                    self.content = true;
                    return;
                }
                continue;
            };
            if j::text(old_host, "serverId") != j::text(new_host, "serverId") {
                self.content = true;
                return;
            }
            let same_directory = old_record
                .get(DIRECTORY_REVISION)
                .filter(|revision| revision.is_u64())
                .is_some_and(|revision| new_record.get(DIRECTORY_REVISION) == Some(revision));
            let skipped: &[&str] = if same_directory {
                &DIRECTORY_FIELDS
            } else {
                &["agents", DIRECTORY_REVISION]
            };
            match fields_differ(old_record, new_record, &HOST_ACTIVITY, skipped) {
                Difference::Content => {
                    self.content = true;
                    return;
                }
                Difference::Activity => self.host_activity = true,
                Difference::None => {}
            }
            if same_directory {
                continue;
            }
            let server_id = j::text(new_host, "serverId");
            if self.agents(
                server_id,
                j::array(old_host, "agents"),
                j::array(new_host, "agents"),
            ) {
                self.content = true;
                return;
            }
        }
    }

    /// Records activity-only agent changes; true when any agent's content changed.
    fn agents(&mut self, server_id: &str, old: &[Value], new: &[Value]) -> bool {
        if old.len() != new.len() {
            return true;
        }
        // Directory order is stable between publications; fall back to an id map otherwise.
        let mut by_id: Option<HashMap<&str, &Value>> = None;
        for (index, row) in new.iter().enumerate() {
            let id = j::text(j::agent_of(row), "id");
            let previous = match old.get(index) {
                Some(candidate) if j::text(j::agent_of(candidate), "id") == id => candidate,
                _ => {
                    let map = by_id.get_or_insert_with(|| {
                        old.iter()
                            .map(|row| (j::text(j::agent_of(row), "id"), row))
                            .collect()
                    });
                    match map.get(id) {
                        Some(previous) => previous,
                        None => return true,
                    }
                }
            };
            match agent_difference(previous, row) {
                Difference::Content => return true,
                Difference::Activity => {
                    self.agent_activity
                        .insert((server_id.to_owned(), id.to_owned()));
                }
                Difference::None => {}
            }
        }
        false
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Difference {
    None,
    Activity,
    Content,
}

/// Rows are the agent object or `{agent: {...}, ...}`; the wrapper's other fields are content.
fn agent_difference(old: &Value, new: &Value) -> Difference {
    match (old.get("agent"), new.get("agent")) {
        (Some(Value::Object(old_agent)), Some(Value::Object(new_agent)))
            if !old_agent.is_empty() && !new_agent.is_empty() =>
        {
            let (Some(old_row), Some(new_row)) = (old.as_object(), new.as_object()) else {
                return Difference::Content;
            };
            if fields_differ(old_row, new_row, &[], &["agent"]) == Difference::Content {
                return Difference::Content;
            }
            fields_differ(old_agent, new_agent, &AGENT_ACTIVITY, &[])
        }
        _ => match (old.as_object(), new.as_object()) {
            (Some(old_agent), Some(new_agent)) => {
                fields_differ(old_agent, new_agent, &AGENT_ACTIVITY, &[])
            }
            _ if old == new => Difference::None,
            _ => Difference::Content,
        },
    }
}

/// Compares two records field by field: `activity` fields report [`Difference::Activity`],
/// `skipped` fields are compared by the caller, anything else is content.
fn fields_differ(
    old: &Map<String, Value>,
    new: &Map<String, Value>,
    activity: &[&str],
    skipped: &[&str],
) -> Difference {
    let mut result = Difference::None;
    for (key, value) in old {
        if skipped.contains(&key.as_str()) {
            continue;
        }
        if new.get(key) == Some(value) {
            continue;
        }
        if !activity.contains(&key.as_str()) {
            return Difference::Content;
        }
        result = Difference::Activity;
    }
    for key in new.keys() {
        if old.contains_key(key) || skipped.contains(&key.as_str()) {
            continue;
        }
        if !activity.contains(&key.as_str()) {
            return Difference::Content;
        }
        result = Difference::Activity;
    }
    result
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn state(agents: Value, preferences: Value) -> Value {
        json!({
            "preferences": preferences,
            "layout": {"tree": 1},
            "hosts": [{"serverId": "h", "status": "connected", "latencyMs": 4, "agents": agents}],
        })
    }

    #[test]
    fn streaming_timestamps_are_activity_of_that_agent_only() {
        let old = state(
            json!([
                {"id": "a", "title": "A", "updatedAt": "1"},
                {"agent": {"id": "b", "updatedAt": "1"}, "project": {"id": "p"}}
            ]),
            json!({}),
        );
        let new = state(
            json!([
                {"id": "a", "title": "A", "updatedAt": "2", "lastActivityAt": "2"},
                {"agent": {"id": "b", "updatedAt": "1"}, "project": {"id": "p"}}
            ]),
            json!({}),
        );
        let delta = StateDelta::between(&old, &new);
        assert!(!delta.content);
        assert!(delta.touches_agent("h", "a"));
        assert!(!delta.touches_agent("h", "b"));
    }

    #[test]
    fn presented_agent_fields_and_membership_are_content() {
        let base = state(
            json!([{"agent": {"id": "a", "status": "running"}}]),
            json!({}),
        );
        let renamed = state(json!([{"agent": {"id": "a", "status": "idle"}}]), json!({}));
        let moved = state(
            json!([{"agent": {"id": "a", "status": "running"}, "project": {"id": "q"}}]),
            json!({}),
        );
        let added = state(
            json!([{"agent": {"id": "a", "status": "running"}}, {"id": "b"}]),
            json!({}),
        );
        for next in [renamed, moved, added] {
            let delta = StateDelta::between(&base, &next);
            assert!(delta.content, "{next}");
            assert!(delta.touches_agent("h", "other"));
        }
    }

    #[test]
    fn reordered_agents_compare_by_id() {
        let old = state(
            json!([{"id": "a"}, {"id": "b", "updatedAt": "1"}]),
            json!({}),
        );
        let new = state(
            json!([{"id": "b", "updatedAt": "2"}, {"id": "a"}]),
            json!({}),
        );
        let delta = StateDelta::between(&old, &new);
        assert!(!delta.content);
        assert_eq!(
            delta.agent_activity,
            HashSet::from([("h".to_owned(), "b".to_owned())])
        );
    }

    #[test]
    fn layouts_are_ignored_and_telemetry_is_host_activity() {
        let old = state(json!([]), json!({"theme": "dark"}));
        let mut new = old.clone();
        new["layout"] = json!({"tree": 2});
        new["windowLayouts"] = json!({"w": {}});
        new["hosts"][0]["latencyMs"] = json!(9);
        let delta = StateDelta::between(&old, &new);
        assert!(!delta.content);
        assert!(delta.host_activity);

        new["preferences"] = json!({"theme": "light"});
        assert!(StateDelta::between(&old, &new).content);
        let mut status = old.clone();
        status["hosts"][0]["status"] = json!("connecting");
        assert!(StateDelta::between(&old, &status).content);
    }

    #[test]
    fn equal_directory_revisions_skip_rows_and_new_ones_compare_them() {
        let mut old = state(json!([{"id": "a", "status": "running"}]), json!({}));
        old["hosts"][0]["directoryRevision"] = json!(7);
        let mut same = old.clone();
        same["hosts"][0]["agents"][0]["status"] = json!("idle");
        assert!(StateDelta::between(&old, &same).is_empty());

        let mut renewed = same.clone();
        renewed["hosts"][0]["directoryRevision"] = json!(8);
        assert!(StateDelta::between(&old, &renewed).content);
        let mut timestamp_only = old.clone();
        timestamp_only["hosts"][0]["directoryRevision"] = json!(8);
        timestamp_only["hosts"][0]["agents"][0]["updatedAt"] = json!("2");
        let delta = StateDelta::between(&old, &timestamp_only);
        assert!(!delta.content);
        assert!(delta.touches_agent("h", "a"));
    }
}
