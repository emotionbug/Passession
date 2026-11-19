//! Section-keyed changes between immutable core publications.
use super::json as j;
use paseo_gui_core::{CoreState, DirectorySnapshot};
use serde_json::{Map, Value};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

const AGENT_ACTIVITY: [&str; 2] = ["updatedAt", "lastActivityAt"];
const HOST_ACTIVITY: [&str; 3] = ["latencyMs", "probes", "cachedAt"];
const DIRECTORY_FIELDS: [&str; 2] = ["directoryRevision", "directoryKey"];
const DRAFTS: &str = "paseo.chat.drafts.v1";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StateDelta {
    pub sections: HashSet<String>,
    pub prefs_keys: HashSet<String>,
    pub hosts: HashSet<String>,
    pub directories: HashSet<String>,
    pub content: bool,
    pub host_activity: bool,
    pub agent_activity: HashSet<(String, String)>,
}

impl StateDelta {
    pub fn between(old: &CoreState, new: &CoreState) -> Self {
        let mut delta = Self::default();
        if std::ptr::eq(old, new) {
            return delta;
        }
        let before = &old["revisions"];
        let after = &new["revisions"];
        for section in [
            "preferences",
            "layouts",
            "suppressedRoutes",
            "storage",
            "tailnet",
            "session",
            "hostSet",
        ] {
            if before[section] != after[section] {
                delta.sections.insert(section.to_owned());
                if matches!(
                    section,
                    "preferences" | "suppressedRoutes" | "storage" | "session" | "hostSet"
                ) {
                    delta.content = true;
                }
            }
        }
        let old_prefs = before["prefs"].as_object();
        let new_prefs = after["prefs"].as_object();
        for key in old_prefs
            .into_iter()
            .flat_map(|map| map.keys())
            .chain(new_prefs.into_iter().flat_map(|map| map.keys()))
        {
            if old_prefs.and_then(|map| map.get(key)) != new_prefs.and_then(|map| map.get(key)) {
                delta.prefs_keys.insert(key.clone());
            }
        }
        if !delta.prefs_keys.is_empty() {
            delta.sections.insert("prefs".into());
        }
        let mut previous: HashMap<u64, &Value> = j::array(old, "hosts")
            .iter()
            .filter_map(|host| Some((host["directoryKey"].as_u64()?, host)))
            .collect();
        for host in j::array(new, "hosts") {
            let server = j::text(host, "serverId");
            let Some(previous_host) = host["directoryKey"]
                .as_u64()
                .and_then(|key| previous.remove(&key))
            else {
                delta.content = true;
                delta.hosts.insert(server.to_owned());
                delta.directories.insert(server.to_owned());
                continue;
            };
            if let (Some(old), Some(new)) = (previous_host.as_object(), host.as_object()) {
                match fields_differ(old, new, &HOST_ACTIVITY, &DIRECTORY_FIELDS) {
                    Difference::Content => {
                        delta.content = true;
                        delta.hosts.insert(server.to_owned());
                    }
                    Difference::Activity => delta.host_activity = true,
                    Difference::None => {}
                }
            }
            match (old.directory(previous_host), new.directory(host)) {
                (Some(before), Some(after)) if before.revision == after.revision => {}
                (Some(before), Some(after)) => {
                    if delta.directory(server, before, after) {
                        delta.content = true;
                        delta.directories.insert(server.to_owned());
                    }
                }
                (None, None) => {}
                _ => {
                    delta.content = true;
                    delta.directories.insert(server.to_owned());
                }
            }
        }
        for host in previous.values() {
            delta.content = true;
            delta.hosts.insert(j::text(host, "serverId").to_owned());
            delta
                .directories
                .insert(j::text(host, "serverId").to_owned());
        }
        delta
    }

    pub fn preference_panels(&self) -> bool {
        self.sections.contains("tailnet") || self.prefs_keys.iter().any(|key| key != DRAFTS)
    }

    /// No presenting shell layer changed (drafts and saved layouts have no presenter).
    pub fn is_empty(&self) -> bool {
        !self.content
            && !self.host_activity
            && self.agent_activity.is_empty()
            && !self.preference_panels()
    }

    pub fn merge(&mut self, other: Self) {
        self.sections.extend(other.sections);
        self.prefs_keys.extend(other.prefs_keys);
        self.hosts.extend(other.hosts);
        self.directories.extend(other.directories);
        self.content |= other.content;
        self.host_activity |= other.host_activity;
        self.agent_activity.extend(other.agent_activity);
    }

    pub fn touches_agent(&self, server: &str, agent: &str) -> bool {
        self.content
            || self
                .agent_activity
                .iter()
                .any(|(host, id)| host == server && id == agent)
    }

    fn directory(
        &mut self,
        server: &str,
        old: &DirectorySnapshot,
        new: &DirectorySnapshot,
    ) -> bool {
        let rows_differ = |old: &Arc<Vec<Arc<Value>>>, new: &Arc<Vec<Arc<Value>>>| {
            !Arc::ptr_eq(old, new)
                && (old.len() != new.len()
                    || old
                        .iter()
                        .zip(new.iter())
                        .any(|(old, new)| !Arc::ptr_eq(old, new)))
        };
        if rows_differ(&old.projects, &new.projects)
            || rows_differ(&old.workspaces, &new.workspaces)
        {
            return true;
        }
        if old.cached_at != new.cached_at {
            self.host_activity = true;
        }
        if old.agents.len() != new.agents.len() {
            return true;
        }
        let mut inspect = |previous: &Arc<Value>, row: &Arc<Value>| {
            if Arc::ptr_eq(previous, row) {
                return false;
            }
            match agent_difference(previous, row) {
                Difference::Content => true,
                Difference::Activity => {
                    let id = j::text(j::agent_of(row), "id");
                    self.agent_activity
                        .insert((server.to_owned(), id.to_owned()));
                    false
                }
                Difference::None => false,
            }
        };
        let mut content = false;
        if new
            .agents
            .for_each_replacement(&old.agents, |old, new| content |= inspect(old, new))
        {
            return content;
        }
        let mut by_id: Option<HashMap<&str, &Arc<Value>>> = None;
        for (candidate, row) in old.agents.iter().zip(new.agents.iter()) {
            let id = j::text(j::agent_of(row), "id");
            let previous = if j::text(j::agent_of(candidate), "id") == id {
                candidate
            } else {
                let map = by_id.get_or_insert_with(|| {
                    old.agents
                        .iter()
                        .map(|row| (j::text(j::agent_of(row), "id"), row))
                        .collect()
                });
                let Some(previous) = map.get(id) else {
                    content = true;
                    continue;
                };
                *previous
            };
            content |= inspect(previous, row);
        }
        content
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Difference {
    None,
    Activity,
    Content,
}

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
            (Some(old), Some(new)) => fields_differ(old, new, &AGENT_ACTIVITY, &[]),
            _ if old == new => Difference::None,
            _ => Difference::Content,
        },
    }
}

fn fields_differ(
    old: &Map<String, Value>,
    new: &Map<String, Value>,
    activity: &[&str],
    skipped: &[&str],
) -> Difference {
    let mut result = Difference::None;
    for key in old.keys().chain(new.keys()) {
        if skipped.contains(&key.as_str()) || old.get(key) == new.get(key) {
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
    use super::*;
    use serde_json::json;

    fn state(directories: HashMap<u64, Arc<DirectorySnapshot>>) -> CoreState {
        let mut state = CoreState::new(
            json!({"hosts":[{"serverId":"a","directoryKey":1},{"serverId":"b","directoryKey":2}],
            "revisions":{"prefs":{},"layouts":1}}),
        );
        state.directories = Arc::new(directories);
        state
    }

    #[test]
    fn drafts_and_layout_publications_have_no_shell_presenter() {
        let old = state(HashMap::new());
        let mut drafts = old.clone();
        drafts.value_mut()["revisions"]["prefs"][DRAFTS] = json!(2);
        drafts.prefs = Arc::new(Map::from_iter([(DRAFTS.into(), json!("draft"))]));
        assert!(StateDelta::between(&old, &drafts).is_empty());
        let mut layout = old.clone();
        layout.value_mut()["revisions"]["layouts"] = json!(3);
        layout.layouts = Arc::new(std::collections::BTreeMap::from([(
            "main".into(),
            json!({"tree":1}),
        )]));
        assert!(StateDelta::between(&old, &layout).is_empty());
    }

    #[test]
    fn changed_directory_preserves_other_host_and_classifies_only_its_rows() {
        let a = Arc::new(DirectorySnapshot {
            revision: 1,
            agents: [Arc::new(json!({"id":"x","title":"old"}))]
                .into_iter()
                .collect(),
            ..DirectorySnapshot::default()
        });
        let b = Arc::new(DirectorySnapshot {
            revision: 2,
            ..DirectorySnapshot::default()
        });
        let old = state(HashMap::from([(1, a), (2, b.clone())]));
        let changed = Arc::new(DirectorySnapshot {
            revision: 3,
            agents: [Arc::new(json!({"id":"x","title":"new"}))]
                .into_iter()
                .collect(),
            ..DirectorySnapshot::default()
        });
        let new = state(HashMap::from([(1, changed), (2, b)]));
        assert!(Arc::ptr_eq(&old.directories[&2], &new.directories[&2]));
        let delta = StateDelta::between(&old, &new);
        assert_eq!(delta.directories, HashSet::from(["a".to_owned()]));
        assert!(delta.content);
    }

    fn agents_state(agents: Value, revision: u64) -> CoreState {
        let directory = Arc::new(DirectorySnapshot {
            revision,
            agents: agents
                .as_array()
                .unwrap()
                .iter()
                .cloned()
                .map(Arc::new)
                .collect(),
            ..DirectorySnapshot::default()
        });
        let mut state = state(HashMap::from([(1, directory)]));
        state.value_mut()["hosts"] =
            json!([{"serverId":"h","directoryKey":1,"status":"connected","latencyMs":4}]);
        state
    }

    #[test]
    fn streaming_timestamps_are_activity_of_that_agent_only() {
        let old = agents_state(
            json!([
                {"id":"a","title":"A","updatedAt":"1"},
                {"agent":{"id":"b","updatedAt":"1"},"project":{"id":"p"}}
            ]),
            1,
        );
        let new = agents_state(
            json!([
                {"id":"a","title":"A","updatedAt":"2","lastActivityAt":"2"},
                {"agent":{"id":"b","updatedAt":"1"},"project":{"id":"p"}}
            ]),
            2,
        );
        let delta = StateDelta::between(&old, &new);
        assert!(!delta.content);
        assert!(delta.touches_agent("h", "a"));
        assert!(!delta.touches_agent("h", "b"));
    }

    #[test]
    fn presented_agent_fields_and_membership_are_content() {
        let base = agents_state(json!([{"agent":{"id":"a","status":"running"}}]), 1);
        for agents in [
            json!([{"agent":{"id":"a","status":"idle"}}]),
            json!([{"agent":{"id":"a","status":"running"},"project":{"id":"q"}}]),
            json!([{"agent":{"id":"a","status":"running"}},{"id":"b"}]),
        ] {
            let next = agents_state(agents, 2);
            let delta = StateDelta::between(&base, &next);
            assert!(delta.content);
            assert!(delta.touches_agent("h", "other"));
        }
    }

    #[test]
    fn reordered_agents_compare_by_id() {
        let old = agents_state(json!([{"id":"a"},{"id":"b","updatedAt":"1"}]), 1);
        let new = agents_state(json!([{"id":"b","updatedAt":"2"},{"id":"a"}]), 2);
        let delta = StateDelta::between(&old, &new);
        assert!(!delta.content);
        assert_eq!(
            delta.agent_activity,
            HashSet::from([("h".to_owned(), "b".to_owned())])
        );
    }

    #[test]
    fn layouts_are_ignored_and_telemetry_is_host_activity() {
        let old = agents_state(json!([]), 1);
        let mut new = old.clone();
        new.value_mut()["revisions"]["layouts"] = json!(2);
        new.value_mut()["hosts"][0]["latencyMs"] = json!(9);
        let delta = StateDelta::between(&old, &new);
        assert!(!delta.content);
        assert!(delta.host_activity);
        new.value_mut()["revisions"]["preferences"] = json!(3);
        assert!(StateDelta::between(&old, &new).content);
        let mut status = old.clone();
        status.value_mut()["hosts"][0]["status"] = json!("connecting");
        assert!(StateDelta::between(&old, &status).content);
    }
}
