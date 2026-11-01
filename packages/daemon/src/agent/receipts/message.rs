//! Durable exact-identity receipts for prompt sends: one record per
//! `(agentId, messageId)`. Locks cover only short file transitions; dispatch
//! ownership is detached and tracked in memory, so queries and dismissals never
//! wait for provider work. A record that outlives its in-memory owner (crash or
//! restart) is reconciled conservatively.
use super::super::notifications::MessageReceipt;
use super::super::storage::{KeyLocks, atomic_json, read_optional};
use crate::json::fingerprint as digest;
use crate::providers::api::{PromptAck, PromptAckFuture, PromptDeliveryError};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::{broadcast, watch};

pub const RECEIPT_UPDATE: &str = "agent.message_receipt.update";
pub const OUTCOME_UNKNOWN: &str = "agent_request_outcome_unknown";
pub const DISMISSED: &str = "agent_request_dismissed";
pub const KEY_CONFLICT: &str = "agent_request_key_conflict";
const OWNER_LOST: &str = "agent_request_owner_lost_before_dispatch";

/// A send failure for an identity that has a receipt. The display text stays
/// the legacy error string; the receipt travels with the response.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct SendReceiptError {
    pub message: String,
    pub receipt: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    version: u32,
    fingerprint: Option<String>,
    agent_id: String,
    #[serde(default)]
    message_id: Option<String>,
    /// Rollback compatibility: older daemons replay "completed" without
    /// dispatch and report anything else as an unknown outcome.
    state: String,
    phase: String,
    revision: u64,
    #[serde(default)]
    dismissed: bool,
    #[serde(default)]
    result: Value,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
    #[serde(default)]
    correlation: Value,
    /// Submitted, with a correlated native acknowledgement still owned live.
    #[serde(default)]
    ack_pending: bool,
}
impl Record {
    fn new(agent_id: &str, message_id: &str, fingerprint: Option<String>) -> Self {
        Self {
            version: 2,
            fingerprint,
            agent_id: agent_id.to_owned(),
            message_id: Some(message_id.to_owned()),
            state: "pending".to_owned(),
            phase: "missing".to_owned(),
            revision: 0,
            dismissed: false,
            result: Value::Null,
            error: None,
            updated_at: None,
            correlation: Value::Null,
            ack_pending: false,
        }
    }
}

/// A legacy receipt has only fingerprint, agentId and pending/completed state.
/// Pending carries no delivery evidence, so it is an unknown outcome; completed
/// meant the host dispatch returned, without separate native acknowledgement.
fn parse(bytes: &[u8], path: &std::path::Path) -> Result<Record> {
    let value: Value = serde_json::from_slice(bytes)?;
    if value.get("version").is_some() {
        let record: Record = serde_json::from_value(value)
            .with_context(|| format!("Invalid message receipt at {}", path.display()))?;
        if !PHASES.contains(&record.phase.as_str()) {
            bail!("Invalid message receipt phase at {}", path.display());
        }
        return Ok(record);
    }
    let (Some(fingerprint), Some(agent_id), Some(state)) = (
        value["fingerprint"].as_str(),
        value["agentId"].as_str(),
        value["state"].as_str(),
    ) else {
        bail!("Invalid message receipt at {}", path.display());
    };
    let completed = match state {
        "completed" => true,
        "pending" => false,
        _ => bail!("Invalid message receipt at {}", path.display()),
    };
    Ok(Record {
        version: 2,
        fingerprint: Some(fingerprint.to_owned()),
        agent_id: agent_id.to_owned(),
        message_id: None,
        state: state.to_owned(),
        phase: if completed { "submitted" } else { "outcome_unknown" }.to_owned(),
        revision: 1,
        dismissed: false,
        result: Value::Null,
        error: (!completed).then(|| OUTCOME_UNKNOWN.to_owned()),
        updated_at: None,
        correlation: json!({"legacy":true}),
        ack_pending: false,
    })
}

pub const PHASES: [&str; 9] = [
    "preparing",
    "queued",
    "dispatching",
    "submitted",
    "accepted",
    "rejected",
    "not_delivered",
    "outcome_unknown",
    "missing",
];
/// Phases that only a live owner can advance.
fn in_flight(phase: &str) -> bool {
    matches!(phase, "preparing" | "queued" | "dispatching")
}
fn may_have_executed(phase: &str) -> bool {
    matches!(
        phase,
        "dispatching" | "submitted" | "accepted" | "outcome_unknown" | "missing"
    )
}

fn identity(agent_id: &str, message_id: &str) -> Result<String> {
    if agent_id.is_empty() || message_id.is_empty() {
        bail!("agentId and messageId are required");
    }
    digest(&json!(["send", agent_id, message_id]))
}

fn project(agent_id: &str, message_id: &str, record: Option<&Record>) -> Value {
    let Some(record) = record else {
        // No record is not evidence of non-delivery: receipts can be lost or
        // removed, so only the absence of a known outcome is reported.
        return json!({
            "agentId":agent_id,"messageId":message_id,"revision":0,"phase":"missing",
            "dismissed":false,"mayHaveExecuted":true,"canRetry":false,"fingerprint":null,
            "result":null,"error":null,"updatedAt":null,"correlation":null
        });
    };
    let mut correlation = record.correlation.clone();
    if record.phase == "submitted" {
        if !correlation.is_object() {
            correlation = json!({});
        }
        correlation["acknowledgementPending"] = json!(record.ack_pending);
    }
    json!({
        "agentId":agent_id,"messageId":message_id,"revision":record.revision,
        "phase":record.phase,"dismissed":record.dismissed,
        "mayHaveExecuted":may_have_executed(&record.phase),
        "canRetry":record.phase == "not_delivered" && !record.dismissed,
        "fingerprint":record.fingerprint,"result":record.result,"error":record.error,
        "updatedAt":record.updated_at,"correlation":correlation
    })
}

fn merge(target: &mut Value, extra: Value) {
    match (target.as_object_mut(), extra) {
        (Some(target), Value::Object(extra)) => target.extend(extra),
        (_, Value::Null) => {}
        (_, extra) => *target = extra,
    }
}

/// Outcome of one dispatch attempt or of its later native acknowledgement.
#[derive(Clone, Debug)]
pub enum Settlement {
    Submitted {
        result: Value,
        correlation: Value,
        ack_pending: bool,
    },
    Accepted {
        result: Option<Value>,
        correlation: Value,
    },
    Rejected(String),
    NotDelivered(String),
    Unknown(String),
}
impl From<PromptAck> for Settlement {
    fn from(ack: PromptAck) -> Self {
        match ack {
            PromptAck::Accepted(evidence) => Self::Accepted {
                result: None,
                correlation: json!({"acknowledgement":evidence}),
            },
            PromptAck::Rejected(error) => Self::Rejected(error),
            PromptAck::NotDelivered(error) => Self::NotDelivered(error),
            PromptAck::Unknown(error) => Self::Unknown(error),
        }
    }
}
impl Settlement {
    /// Classify a failed dispatch. Only explicit evidence is definite; any
    /// failure after the prompt API was invoked is otherwise unknown.
    pub fn failure(error: &anyhow::Error, invoked: bool) -> Self {
        if let Some(delivery) = error.downcast_ref::<PromptDeliveryError>() {
            return if delivery.rejected {
                Self::Rejected(delivery.message.clone())
            } else {
                Self::NotDelivered(delivery.message.clone())
            };
        }
        if invoked {
            Self::Unknown(error.to_string())
        } else {
            Self::NotDelivered(error.to_string())
        }
    }
}

/// Per-attempt dispatch state shared with the manager's prompt path.
pub struct DispatchGate {
    agent_id: String,
    message_ids: Vec<String>,
    /// Direct sends check dismissal at the provider boundary; queued batches
    /// already crossed it when they left the delivery queue.
    check_dismissed: bool,
    invoked: AtomicBool,
    correlation: StdMutex<Value>,
    acknowledgement: StdMutex<Option<PromptAckFuture>>,
}
impl DispatchGate {
    pub fn new(agent_id: &str, message_ids: Vec<String>, check_dismissed: bool) -> Self {
        Self {
            agent_id: agent_id.to_owned(),
            message_ids,
            check_dismissed,
            invoked: AtomicBool::new(false),
            correlation: StdMutex::new(json!({})),
            acknowledgement: StdMutex::new(None),
        }
    }
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }
    pub fn message_ids(&self) -> &[String] {
        &self.message_ids
    }
    /// Called immediately before a provider prompt API that may execute it.
    pub fn invoke(&self) {
        self.invoked.store(true, Ordering::Release);
    }
    /// The provider answered that the invoked API did not deliver the prompt.
    pub fn revoke_invocation(&self) {
        self.invoked.store(false, Ordering::Release);
    }
    pub fn invoked(&self) -> bool {
        self.invoked.load(Ordering::Acquire)
    }
    pub fn correlate(&self, extra: Value) {
        merge(&mut self.correlation.lock().unwrap(), extra);
    }
    pub fn correlation(&self) -> Value {
        self.correlation.lock().unwrap().clone()
    }
    pub fn acknowledge_later(&self, acknowledgement: PromptAckFuture) {
        *self.acknowledgement.lock().unwrap() = Some(acknowledgement);
    }
    pub fn take_acknowledgement(&self) -> Option<PromptAckFuture> {
        self.acknowledgement.lock().unwrap().take()
    }
}

struct Live {
    generation: u64,
    settled: Arc<watch::Sender<bool>>,
    queued: Option<MessageReceipt>,
}
struct Inner {
    directory: PathBuf,
    operations: KeyLocks,
    live: StdMutex<HashMap<String, Live>>,
    generations: AtomicU64,
    events: broadcast::Sender<Value>,
}
/// In-memory ownership of one identity. Dropping it without settling leaves
/// the durable phase to conservative orphan reconciliation.
pub struct LiveGuard {
    inner: Arc<Inner>,
    key: String,
    generation: u64,
    _settled: Arc<watch::Sender<bool>>,
}
impl Drop for LiveGuard {
    fn drop(&mut self) {
        let mut live = self.inner.live.lock().unwrap_or_else(|e| e.into_inner());
        if live
            .get(&self.key)
            .is_some_and(|entry| entry.generation == self.generation)
        {
            live.remove(&self.key);
        }
    }
}

pub enum Admission {
    /// This caller owns the attempt.
    Run(LiveGuard),
    /// Existing durable outcome; see [`replay`].
    Replay(Value),
    /// Another live owner is still before its outcome; wait, then admit again.
    Wait(watch::Receiver<bool>),
}
pub enum QueuedAdmission {
    Owner(LiveGuard, MessageReceipt),
    /// A live queued duplicate shares the owner's dispatch.
    Duplicate(MessageReceipt),
    Replay(Value),
}

#[derive(Clone)]
pub struct MessageReceipts {
    inner: Arc<Inner>,
}
impl MessageReceipts {
    pub fn new(home: PathBuf) -> Self {
        let (events, _) = broadcast::channel(256);
        Self {
            inner: Arc::new(Inner {
                directory: home.join("agent-requests"),
                operations: KeyLocks::default(),
                live: StdMutex::new(HashMap::new()),
                generations: AtomicU64::new(1),
                events,
            }),
        }
    }

    /// Observe an exact durable receipt from a process that does not own sends.
    /// Unlike `get`, this never reconciles or changes another worker's live RPC.
    pub(crate) async fn observe_persisted(
        home: &std::path::Path,
        agent_id: &str,
        message_id: &str,
    ) -> Result<Value> {
        let path = home.join("agent-requests").join(format!("{}.json", identity(agent_id, message_id)?));
        let record = match read_optional(&path).await? {
            Some(bytes) => Some(parse(&bytes, &path)?),
            None => None,
        };
        if let Some(record) = &record {
            if record.agent_id != agent_id
                || record.message_id.as_deref().is_some_and(|id| id != message_id)
            {
                bail!("Message receipt identity mismatch");
            }
        }
        Ok(project(agent_id, message_id, record.as_ref()))
    }
    /// Receipt updates, published only after the durable write.
    pub fn subscribe_events(&self) -> broadcast::Receiver<Value> {
        self.inner.events.subscribe()
    }

    pub async fn get(&self, agent_id: &str, message_id: &str) -> Result<Value> {
        let key = identity(agent_id, message_id)?;
        let _lock = self.inner.operations.lock(&key).await;
        let record = self.inner.load(&key, agent_id, message_id).await?;
        Ok(project(agent_id, message_id, record.as_ref()))
    }

    /// Sticky tombstone: this exact request never dispatches afterwards. A
    /// prompt is stopped only if its owner had not yet crossed the dispatch
    /// boundary; otherwise the delivery evidence stays as it is.
    pub async fn dismiss(&self, agent_id: &str, message_id: &str) -> Result<Value> {
        let key = identity(agent_id, message_id)?;
        let _lock = self.inner.operations.lock(&key).await;
        let mut record = self
            .inner
            .load(&key, agent_id, message_id)
            .await?
            .unwrap_or_else(|| Record::new(agent_id, message_id, None));
        if !record.dismissed {
            record.dismissed = true;
            let phase = record.phase.clone();
            self.inner.write(&key, message_id, &mut record, &phase).await?;
        }
        Ok(project(agent_id, message_id, Some(&record)))
    }

    /// Direct sends: durable `preparing` before any local preparation.
    pub async fn admit(&self, agent_id: &str, message_id: &str, request: &Value) -> Result<Admission> {
        let key = identity(agent_id, message_id)?;
        let fingerprint = digest(request)?;
        let _lock = self.inner.operations.lock(&key).await;
        let record = self.inner.load(&key, agent_id, message_id).await?;
        if let Some(record) = &record {
            if let Some(admission) = self.existing(&key, agent_id, message_id, record, &fingerprint)? {
                return Ok(admission);
            }
        }
        let mut record =
            record.unwrap_or_else(|| Record::new(agent_id, message_id, Some(fingerprint)));
        record.error = None;
        self.inner.write(&key, message_id, &mut record, "preparing").await?;
        Ok(Admission::Run(self.inner.claim(&key, None)))
    }

    fn existing(
        &self,
        key: &str,
        agent_id: &str,
        message_id: &str,
        record: &Record,
        fingerprint: &str,
    ) -> Result<Option<Admission>> {
        let receipt = project(agent_id, message_id, Some(record));
        if record.dismissed {
            return Ok(Some(Admission::Replay(receipt)));
        }
        if record.fingerprint.as_deref() != Some(fingerprint) {
            return Err(SendReceiptError { message: KEY_CONFLICT.to_owned(), receipt }.into());
        }
        if in_flight(&record.phase) {
            if let Some(settled) = self.inner.settled(key) {
                return Ok(Some(Admission::Wait(settled)));
            }
        }
        // Only a proven non-delivery may be attempted again under this identity.
        Ok((record.phase != "not_delivered").then_some(Admission::Replay(receipt)))
    }

    /// Agent-to-agent sends: durable `queued` before enqueue. A live duplicate
    /// shares the queued owner without waiting for provider dispatch.
    pub async fn enqueue<T, P, F, FF>(
        &self,
        agent_id: &str,
        message_id: &str,
        request: &Value,
        prepare: P,
        enqueue: F,
    ) -> Result<QueuedAdmission>
    where
        P: FnOnce() -> Result<T>,
        F: FnOnce(T) -> FF,
        FF: Future<Output = Result<MessageReceipt>>,
    {
        let key = identity(agent_id, message_id)?;
        let fingerprint = digest(request)?;
        let _lock = self.inner.operations.lock(&key).await;
        let record = self.inner.load(&key, agent_id, message_id).await?;
        if let Some(record) = &record {
            if let Some(queued) = self.inner.queued(&key) {
                if !record.dismissed && record.fingerprint.as_deref() == Some(fingerprint.as_str()) {
                    return Ok(QueuedAdmission::Duplicate(queued));
                }
            }
            match self.existing(&key, agent_id, message_id, record, &fingerprint)? {
                Some(Admission::Replay(receipt)) => return Ok(QueuedAdmission::Replay(receipt)),
                Some(_) => {
                    return Ok(QueuedAdmission::Replay(project(agent_id, message_id, Some(record))));
                }
                None => {}
            }
        }
        // Reservation failure leaves no new durable state, so it stays retryable.
        let prepared = prepare()?;
        let mut record =
            record.unwrap_or_else(|| Record::new(agent_id, message_id, Some(fingerprint)));
        record.error = None;
        self.inner.write(&key, message_id, &mut record, "queued").await?;
        match enqueue(prepared).await {
            Ok(receipt) => {
                let guard = self.inner.claim(&key, Some(receipt.clone()));
                Ok(QueuedAdmission::Owner(guard, receipt))
            }
            Err(error) => {
                record.error = Some(error.to_string());
                self.inner.write(&key, message_id, &mut record, "not_delivered").await?;
                Err(error)
            }
        }
    }

    /// The durable dispatch boundary. Returns false, recording non-delivery,
    /// when the request was dismissed before any provider effect.
    pub async fn begin_dispatch(&self, agent_id: &str, message_id: &str) -> Result<bool> {
        let key = identity(agent_id, message_id)?;
        let _lock = self.inner.operations.lock(&key).await;
        let mut record = self
            .inner
            .read(&key)
            .await?
            .context("Message receipt disappeared before dispatch")?;
        if record.dismissed {
            if in_flight(&record.phase) {
                record.error = Some(DISMISSED.to_owned());
                self.inner.write(&key, message_id, &mut record, "not_delivered").await?;
                self.inner.signal(&key);
            }
            return Ok(false);
        }
        if record.phase != "dispatching" {
            self.inner.write(&key, message_id, &mut record, "dispatching").await?;
        }
        Ok(true)
    }

    /// The direct-path boundary for a gate; queued gates crossed it earlier.
    pub async fn begin(&self, gate: &DispatchGate) -> Result<()> {
        if !gate.check_dismissed {
            return Ok(());
        }
        for message_id in &gate.message_ids {
            if !self.begin_dispatch(&gate.agent_id, message_id).await? {
                return Err(PromptDeliveryError::not_delivered(DISMISSED));
            }
        }
        Ok(())
    }

    /// Record an outcome. Dismissal is never cleared. Late authoritative
    /// evidence may improve an unknown outcome; definite outcomes are final.
    pub async fn settle(&self, agent_id: &str, message_id: &str, settlement: Settlement) -> Result<Value> {
        let key = identity(agent_id, message_id)?;
        let _lock = self.inner.operations.lock(&key).await;
        let mut record = self
            .inner
            .read(&key)
            .await?
            .context("Message receipt disappeared before settlement")?;
        let open = in_flight(&record.phase) || record.phase == "submitted";
        let improves = record.phase == "outcome_unknown"
            && matches!(settlement, Settlement::Accepted { .. } | Settlement::Rejected(_));
        if !open && !improves {
            return Ok(project(agent_id, message_id, Some(&record)));
        }
        let phase = match settlement {
            Settlement::Submitted { result, correlation, ack_pending } => {
                record.result = result;
                merge(&mut record.correlation, correlation);
                record.ack_pending = ack_pending;
                record.error = None;
                "submitted"
            }
            Settlement::Accepted { result, correlation } => {
                if let Some(result) = result {
                    record.result = result;
                }
                merge(&mut record.correlation, correlation);
                record.ack_pending = false;
                record.error = None;
                "accepted"
            }
            Settlement::Rejected(error) => {
                record.ack_pending = false;
                record.error = Some(error);
                "rejected"
            }
            Settlement::NotDelivered(error) => {
                record.ack_pending = false;
                record.error = Some(error);
                "not_delivered"
            }
            Settlement::Unknown(error) => {
                record.ack_pending = false;
                record.error = Some(error);
                "outcome_unknown"
            }
        };
        self.inner.write(&key, message_id, &mut record, phase).await?;
        self.inner.signal(&key);
        Ok(project(agent_id, message_id, Some(&record)))
    }

    /// Take over liveness for a detached acknowledgement follower.
    pub fn claim(&self, agent_id: &str, message_id: &str) -> Result<LiveGuard> {
        Ok(self.inner.claim(&identity(agent_id, message_id)?, None))
    }

    /// Release an owner, reconciling whatever it left unsettled.
    pub async fn finish(&self, guard: LiveGuard, agent_id: &str, message_id: &str) -> Result<Value> {
        drop(guard);
        self.get(agent_id, message_id).await
    }
}

impl Inner {
    fn path(&self, key: &str) -> PathBuf {
        self.directory.join(format!("{key}.json"))
    }
    async fn read(&self, key: &str) -> Result<Option<Record>> {
        let path = self.path(key);
        match read_optional(&path).await? {
            Some(bytes) => parse(&bytes, &path).map(Some),
            None => Ok(None),
        }
    }
    /// Read, reconciling an in-flight phase whose owner no longer exists:
    /// before the durable dispatch boundary nothing reached the provider;
    /// after it, or with an unowned acknowledgement, the outcome is unknown.
    async fn load(&self, key: &str, agent_id: &str, message_id: &str) -> Result<Option<Record>> {
        let Some(mut record) = self.read(key).await? else {
            return Ok(None);
        };
        if record.agent_id != agent_id {
            bail!("Message receipt identity mismatch");
        }
        if self.live.lock().unwrap().contains_key(key) {
            return Ok(Some(record));
        }
        let orphaned = match record.phase.as_str() {
            "preparing" | "queued" => Some(("not_delivered", OWNER_LOST)),
            "dispatching" => Some(("outcome_unknown", OUTCOME_UNKNOWN)),
            "submitted" if record.ack_pending => Some(("outcome_unknown", OUTCOME_UNKNOWN)),
            _ => None,
        };
        if let Some((phase, error)) = orphaned {
            record.ack_pending = false;
            record.error = Some(error.to_owned());
            self.write(key, message_id, &mut record, phase).await?;
        }
        Ok(Some(record))
    }
    async fn write(&self, key: &str, message_id: &str, record: &mut Record, phase: &str) -> Result<()> {
        record.version = 2;
        record.phase = phase.to_owned();
        record.revision += 1;
        record.updated_at = Some(
            chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        );
        record.state = if matches!(phase, "submitted" | "accepted") {
            "completed"
        } else {
            "pending"
        }
        .to_owned();
        if record.message_id.is_none() {
            record.message_id = Some(message_id.to_owned());
        }
        atomic_json(&self.path(key), &serde_json::to_value(&*record)?).await?;
        let receipt = project(&record.agent_id, message_id, Some(record));
        let _ = self
            .events
            .send(json!({"type":RECEIPT_UPDATE,"payload":{"receipt":receipt}}));
        Ok(())
    }
    fn claim(self: &Arc<Self>, key: &str, queued: Option<MessageReceipt>) -> LiveGuard {
        let generation = self.generations.fetch_add(1, Ordering::Relaxed);
        let (settled, _) = watch::channel(false);
        let settled = Arc::new(settled);
        self.live.lock().unwrap().insert(
            key.to_owned(),
            Live { generation, settled: settled.clone(), queued },
        );
        LiveGuard {
            inner: self.clone(),
            key: key.to_owned(),
            generation,
            _settled: settled,
        }
    }
    fn settled(&self, key: &str) -> Option<watch::Receiver<bool>> {
        self.live
            .lock()
            .unwrap()
            .get(key)
            .map(|live| live.settled.subscribe())
    }
    fn queued(&self, key: &str) -> Option<MessageReceipt> {
        self.live
            .lock()
            .unwrap()
            .get(key)
            .and_then(|live| live.queued.clone())
    }
    fn signal(&self, key: &str) {
        if let Some(live) = self.live.lock().unwrap().get(key) {
            live.settled.send_replace(true);
        }
    }
}

/// The send result for an existing receipt, never dispatching again.
pub fn replay(receipt: Value) -> Result<Value> {
    let failure = |message: &str| -> anyhow::Error {
        SendReceiptError { message: message.to_owned(), receipt: receipt.clone() }.into()
    };
    if receipt["dismissed"] == true {
        return Err(failure(DISMISSED));
    }
    match receipt["phase"].as_str() {
        Some("submitted" | "accepted") => {
            let mut result = if receipt["result"].is_object() {
                receipt["result"].clone()
            } else {
                json!({"disposition":"turn_started","messageId":receipt["messageId"]})
            };
            result["deduplicated"] = json!(true);
            result["receipt"] = receipt;
            Ok(result)
        }
        Some("rejected") => Err(failure(
            receipt["error"].as_str().unwrap_or("agent_request_rejected"),
        )),
        Some("not_delivered") => Err(failure(
            receipt["error"].as_str().unwrap_or("agent_request_not_delivered"),
        )),
        _ => Err(failure(OUTCOME_UNKNOWN)),
    }
}
