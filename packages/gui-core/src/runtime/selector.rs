//! Port of `connection_selection.dart` (`HostConnectionSelector`; original `HostRuntimeController`
//! probe cycle and `selectBestConnection`).
//!
//! Every saved connection of one host is probed. An unreachable (or credential-rejected) active
//! connection is replaced by the fastest reachable one. A healthy active connection only yields
//! to one that is at least 40 ms faster for three consecutive cycles. Routed hosts have a single
//! path through their origin and get no selector.
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, HashMap, HashSet},
    rc::{Rc, Weak},
    time::{Duration, Instant},
};

use serde_json::{Map, Value, json};
use tokio::sync::oneshot;

use super::{Core, Host, hosts};
use crate::{
    CoreError,
    connection::{self, Control, OpenRequest},
    host_config,
};

const PROBE_TICK: Duration = Duration::from_secs(2);
const PROBE_STEADY: Duration = Duration::from_secs(10);
const PROBE_MAX_BACKOFF: Duration = Duration::from_secs(30);
const PROBE_INACTIVE_WHILE_ONLINE: Duration = Duration::from_secs(120);
const ADAPTIVE_SWITCH_THRESHOLD: Duration = Duration::from_millis(40);
const ADAPTIVE_SWITCH_CONSECUTIVE: u32 = 3;
const PROBE_LATENCY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ProbeStatus {
    Pending,
    Available,
    Unavailable,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Probe {
    status: ProbeStatus,
    latency: Option<Duration>,
}

impl Probe {
    const PENDING: Self = Self {
        status: ProbeStatus::Pending,
        latency: None,
    };
    const UNAVAILABLE: Self = Self {
        status: ProbeStatus::Unavailable,
        latency: None,
    };
    fn available(latency: Duration) -> Self {
        Self {
            status: ProbeStatus::Available,
            latency: Some(latency),
        }
    }
}

/// Original `probeIntervalForConnection`.
fn probe_interval(age: Duration, is_active_online: bool, has_active_online: bool) -> Duration {
    if is_active_online {
        return PROBE_STEADY;
    }
    if has_active_online {
        return PROBE_INACTIVE_WHILE_ONLINE;
    }
    if age < Duration::from_secs(10) {
        Duration::from_secs(2)
    } else if age < Duration::from_secs(30) {
        Duration::from_secs(5)
    } else if age < Duration::from_secs(60) {
        PROBE_STEADY
    } else {
        PROBE_MAX_BACKOFF
    }
}

/// Original `selectBestConnection`: the lowest available latency; ties keep the earlier one.
fn select_best(candidates: &[String], probes: &BTreeMap<String, Probe>) -> Option<String> {
    let mut best: Option<(&String, Duration)> = None;
    for id in candidates {
        let Some(latency) = probes
            .get(id)
            .filter(|p| p.status == ProbeStatus::Available)
            .and_then(|p| p.latency)
        else {
            continue;
        };
        if best.is_none_or(|(_, current)| latency < current) {
            best = Some((id, latency));
        }
    }
    best.map(|(id, _)| id.clone())
}

pub(crate) struct Selector {
    core: Weak<Core>,
    host: Weak<Host>,
    clock: Instant,
    first_seen: RefCell<HashMap<String, Duration>>,
    last_probed: RefCell<HashMap<String, Duration>>,
    auth_rejected: RefCell<HashSet<String>>,
    probes: RefCell<BTreeMap<String, Probe>>,
    switch_candidate: RefCell<Option<String>>,
    switch_hits: Cell<u32>,
    probe_version: Cell<u64>,
    cycle_running: Cell<bool>,
    ticker: RefCell<Option<tokio::task::AbortHandle>>,
    stopped: Cell<bool>,
}

impl Selector {
    pub fn new(core: &Rc<Core>, host: &Rc<Host>) -> Rc<Self> {
        Rc::new(Self {
            core: Rc::downgrade(core),
            host: Rc::downgrade(host),
            clock: Instant::now(),
            first_seen: RefCell::default(),
            last_probed: RefCell::default(),
            auth_rejected: RefCell::default(),
            probes: RefCell::default(),
            switch_candidate: RefCell::new(None),
            switch_hits: Cell::new(0),
            probe_version: Cell::new(0),
            cycle_running: Cell::new(false),
            ticker: RefCell::new(None),
            stopped: Cell::new(false),
        })
    }

    pub fn probes_json(&self) -> Value {
        let probes: Map<String, Value> = self
            .probes
            .borrow()
            .iter()
            .map(|(id, probe)| {
                let status = match probe.status {
                    ProbeStatus::Pending => "pending",
                    ProbeStatus::Available => "available",
                    ProbeStatus::Unavailable => "unavailable",
                };
                (
                    id.clone(),
                    json!({"status": status, "latencyMs": probe.latency.map(|l| l.as_millis() as u64)}),
                )
            })
            .collect();
        Value::Object(probes)
    }

    fn connections(&self) -> Vec<Value> {
        self.host
            .upgrade()
            .map(|host| host.connections.borrow().clone())
            .unwrap_or_default()
    }

    pub fn start(self: &Rc<Self>) {
        self.track_first_seen();
        self.run_cycle_now();
        let selector = Rc::downgrade(self);
        let ticker = tokio::task::spawn_local(async move {
            let mut interval = tokio::time::interval(PROBE_TICK);
            interval.tick().await;
            loop {
                interval.tick().await;
                let Some(selector) = selector.upgrade() else {
                    return;
                };
                if selector.stopped.get() {
                    return;
                }
                selector.run_cycle_now();
            }
        });
        *self.ticker.borrow_mut() = Some(ticker.abort_handle());
    }

    pub fn stop(&self) {
        self.stopped.set(true);
        self.probe_version.set(self.probe_version.get() + 1);
        if let Some(ticker) = self.ticker.borrow_mut().take() {
            ticker.abort();
        }
    }

    /// Original `updateHost`: edits restart first-seen tracking; a changed password forgets
    /// credential rejections and reprobes everything.
    pub fn connections_changed(self: &Rc<Self>, credentials_changed: bool) {
        if credentials_changed {
            self.probe_version.set(self.probe_version.get() + 1);
            self.auth_rejected.borrow_mut().clear();
            self.last_probed.borrow_mut().clear();
            // A cycle of the old version publishes nothing; the next one runs now.
            self.cycle_running.set(false);
        }
        self.track_first_seen();
        self.run_cycle_now();
    }

    /// Original `failConnection` for the active client: a credential rejection excludes that
    /// connection and probes the remaining ones right away.
    pub fn report_active_failure(self: &Rc<Self>, connection_id: &str, error: &CoreError) {
        if !error.is_auth_failure() || self.stopped.get() {
            return;
        }
        self.auth_rejected.borrow_mut().insert(connection_id.to_owned());
        self.set_probe(connection_id, Probe::UNAVAILABLE);
        let connections = self.connections();
        let rejected = self.auth_rejected.borrow().clone();
        for connection in &connections {
            let id = connection["id"].as_str().unwrap_or_default();
            if !rejected.contains(id) {
                self.last_probed.borrow_mut().remove(id);
            }
        }
        if connections
            .iter()
            .any(|c| !rejected.contains(c["id"].as_str().unwrap_or_default()))
        {
            self.run_cycle_now();
        }
    }

    fn run_cycle_now(self: &Rc<Self>) {
        if self.cycle_running.replace(true) || self.stopped.get() {
            return;
        }
        let selector = self.clone();
        tokio::task::spawn_local(async move {
            let version = selector.probe_version.get() + 1;
            selector.probe_version.set(version);
            selector.run_cycle(version).await;
            if selector.probe_version.get() == version {
                selector.cycle_running.set(false);
            }
        });
    }

    fn track_first_seen(&self) {
        let now = self.clock.elapsed();
        let current: HashSet<String> = self
            .connections()
            .iter()
            .filter_map(|c| c["id"].as_str().map(str::to_owned))
            .collect();
        let mut first_seen = self.first_seen.borrow_mut();
        let mut last_probed = self.last_probed.borrow_mut();
        first_seen.retain(|id, _| current.contains(id));
        last_probed.retain(|id, _| current.contains(id));
        for id in current {
            first_seen.entry(id).or_insert(now);
        }
    }

    fn set_probe(&self, id: &str, probe: Probe) {
        let changed = self.probes.borrow_mut().insert(id.to_owned(), probe) != Some(probe);
        if changed {
            self.touch();
        }
    }

    fn publish(&self, probes: &BTreeMap<String, Probe>) {
        if *self.probes.borrow() == *probes {
            return;
        }
        *self.probes.borrow_mut() = probes.clone();
        self.touch();
    }

    fn touch(&self) {
        if let Some(core) = self.core.upgrade() {
            core.touch();
        }
    }

    async fn run_cycle(self: &Rc<Self>, version: u64) {
        let (Some(core), Some(host)) = (self.core.upgrade(), self.host.upgrade()) else {
            return;
        };
        let connections = self.connections();
        if connections.is_empty() {
            self.publish(&BTreeMap::new());
            return;
        }
        let now = self.clock.elapsed();
        let active_id = host.config_id();
        let online = host.status() == hosts::Status::Connected;
        let to_probe: Vec<Value> = connections
            .iter()
            .filter(|connection| {
                let id = connection["id"].as_str().unwrap_or_default();
                if self.auth_rejected.borrow().contains(id) {
                    return false;
                }
                let Some(last) = self.last_probed.borrow().get(id).copied() else {
                    return true;
                };
                let first = self.first_seen.borrow().get(id).copied().unwrap_or(now);
                now.saturating_sub(last)
                    >= probe_interval(now.saturating_sub(first), online && id == active_id, online)
            })
            .cloned()
            .collect();
        if to_probe.is_empty() {
            return;
        }
        let mut probes = self.probes.borrow().clone();
        for connection in &to_probe {
            let id = connection["id"].as_str().unwrap_or_default().to_owned();
            self.last_probed
                .borrow_mut()
                .insert(id.clone(), self.clock.elapsed());
            let preserve = online
                && id == active_id
                && probes.get(&id).is_some_and(|p| p.status == ProbeStatus::Available);
            if !preserve {
                probes.insert(id, Probe::PENDING);
            }
        }
        self.publish(&probes);
        let mut tasks = Vec::new();
        for connection in to_probe {
            let selector = self.clone();
            let core = core.clone();
            let host = host.clone();
            let active = connection["id"] == active_id.as_str();
            tasks.push(tokio::task::spawn_local(async move {
                let id = connection["id"].as_str().unwrap_or_default().to_owned();
                let outcome = if active {
                    selector.probe_active(&host)
                } else {
                    selector.probe_other(&core, &host, &connection).await.map(Some)
                };
                (id, outcome)
            }));
        }
        let mut auth_failure: Option<CoreError> = None;
        for task in tasks {
            let Ok((id, outcome)) = task.await else {
                continue;
            };
            if self.probe_version.get() != version {
                continue;
            }
            match outcome {
                Ok(Some(probe)) => {
                    probes.insert(id, probe);
                }
                Ok(None) => {}
                Err(error) => {
                    if error.is_auth_failure() {
                        self.auth_rejected.borrow_mut().insert(id.clone());
                        auth_failure = Some(error);
                    }
                    probes.insert(id, Probe::UNAVAILABLE);
                }
            }
            self.publish(&probes);
        }
        if self.probe_version.get() != version || self.stopped.get() {
            return;
        }
        self.finalize(&core, &host, version, &probes, auth_failure);
    }

    /// The active connection is measured by its own heartbeat, never by another connection.
    fn probe_active(&self, host: &Rc<Host>) -> Result<Option<Probe>, CoreError> {
        if host.status() != hosts::Status::Connected {
            return Ok(Some(Probe::UNAVAILABLE));
        }
        Ok(host.rtt().map(Probe::available))
    }

    async fn probe_other(&self, core: &Rc<Core>, host: &Rc<Host>, connection: &Value) -> Result<Probe, CoreError> {
        let request = OpenRequest {
            client_id: core.client_id.borrow().clone(),
            // Original `_connectProbe`: the host's expected identity, not the connection's.
            expected_server_id: host.config.borrow()["expectedServerId"]
                .as_str()
                .map(str::to_owned),
            protected_known: hosts::protected_known(core, connection),
            timeout: host_config::probe_connect_timeout(connection),
        };
        let opened =
            connection::open_direct(connection, request, core.buffers.clone(), &core.cancel).await?;
        let (reply, answered) = oneshot::channel();
        let started = Instant::now();
        let measured = if opened.control.send(Control::Ping(reply)).is_ok() {
            tokio::time::timeout(PROBE_LATENCY_TIMEOUT, answered)
                .await
                .ok()
                .and_then(Result::ok)
                .map(|_| started.elapsed())
        } else {
            None
        };
        let client = opened.client.clone();
        tokio::task::spawn_local(async move { client.close().await });
        measured.map(Probe::available).ok_or_else(|| {
            CoreError::new(
                "DAEMON_REQUEST_TIMEOUT",
                format!(
                    "Latency measurement timed out ({}ms)",
                    PROBE_LATENCY_TIMEOUT.as_millis()
                ),
                false,
            )
        })
    }

    fn finalize(
        &self,
        core: &Rc<Core>,
        host: &Rc<Host>,
        version: u64,
        probes: &BTreeMap<String, Probe>,
        auth_failure: Option<CoreError>,
    ) {
        let connections = self.connections();
        let candidates: Vec<String> = connections
            .iter()
            .filter_map(|c| c["id"].as_str().map(str::to_owned))
            .collect();
        let active_id = host.config_id();
        let reset = |selector: &Self| {
            *selector.switch_candidate.borrow_mut() = None;
            selector.switch_hits.set(0);
        };
        if !candidates.contains(&active_id) {
            match select_best(&candidates, probes) {
                Some(next) => self.switch_to(core, host, &next, version),
                None => {
                    if let Some(error) = auth_failure {
                        *host.error.borrow_mut() = Some(error);
                        core.touch();
                    }
                }
            }
            return;
        }
        let active = probes.get(&active_id).copied();
        if active.is_some_and(|p| p.status == ProbeStatus::Unavailable)
            || self.auth_rejected.borrow().contains(&active_id)
        {
            if let Some(next) = select_best(&candidates, probes).filter(|next| *next != active_id) {
                self.switch_to(core, host, &next, version);
            }
            reset(self);
            return;
        }
        let Some(active_latency) = active
            .filter(|p| p.status == ProbeStatus::Available)
            .and_then(|p| p.latency)
        else {
            return;
        };
        let fastest = probes
            .iter()
            .filter(|(_, p)| p.status == ProbeStatus::Available)
            .filter_map(|(id, p)| p.latency.map(|l| (id.clone(), l)))
            .min_by_key(|(_, latency)| *latency);
        let Some((fastest, latency)) = fastest.filter(|(id, _)| *id != active_id) else {
            reset(self);
            return;
        };
        if active_latency.saturating_sub(latency) < ADAPTIVE_SWITCH_THRESHOLD {
            reset(self);
            return;
        }
        let hits = if self.switch_candidate.borrow().as_deref() == Some(fastest.as_str()) {
            self.switch_hits.get() + 1
        } else {
            *self.switch_candidate.borrow_mut() = Some(fastest.clone());
            1
        };
        self.switch_hits.set(hits);
        if hits >= ADAPTIVE_SWITCH_CONSECUTIVE {
            reset(self);
            self.switch_to(core, host, &fastest, version);
        }
    }

    /// Original `switchToConnection`: stale probe cycles never activate.
    fn switch_to(&self, core: &Rc<Core>, host: &Rc<Host>, connection_id: &str, version: u64) {
        if self.probe_version.get() != version || self.stopped.get() {
            return;
        }
        let connection = self
            .connections()
            .into_iter()
            .find(|c| c["id"] == connection_id);
        if let Some(connection) = connection {
            hosts::activate_connection(core, host, connection);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_intervals_follow_the_original_backoff() {
        assert_eq!(probe_interval(Duration::ZERO, true, true), PROBE_STEADY);
        assert_eq!(probe_interval(Duration::ZERO, false, true), PROBE_INACTIVE_WHILE_ONLINE);
        assert_eq!(probe_interval(Duration::from_secs(5), false, false), Duration::from_secs(2));
        assert_eq!(probe_interval(Duration::from_secs(20), false, false), Duration::from_secs(5));
        assert_eq!(probe_interval(Duration::from_secs(45), false, false), PROBE_STEADY);
        assert_eq!(probe_interval(Duration::from_secs(90), false, false), PROBE_MAX_BACKOFF);
    }

    #[test]
    fn best_connection_is_fastest_and_ties_keep_the_earlier() {
        let ids = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
        let mut probes = BTreeMap::new();
        probes.insert("a".into(), Probe::available(Duration::from_millis(20)));
        probes.insert("b".into(), Probe::available(Duration::from_millis(20)));
        probes.insert("c".into(), Probe::UNAVAILABLE);
        assert_eq!(select_best(&ids, &probes).as_deref(), Some("a"));
        probes.insert("b".into(), Probe::available(Duration::from_millis(5)));
        assert_eq!(select_best(&ids, &probes).as_deref(), Some("b"));
        assert_eq!(select_best(&ids[2..], &probes), None);
    }
}
