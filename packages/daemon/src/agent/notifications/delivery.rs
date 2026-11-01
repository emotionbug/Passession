use super::envelope::prepare_agent_message;
use crate::state::AppState;
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Weak},
};
use tokio::sync::{Mutex, Notify, OwnedSemaphorePermit, Semaphore, watch};
use tokio::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

const QUEUE_CAPACITY: usize = 1024;
const QUIET_PERIOD: Duration = Duration::from_secs(2);
type DispatchResult = std::result::Result<Value, String>;
#[derive(Clone)]
pub struct MessageReceipt {
    pub message_id: String,
    result: watch::Receiver<Option<DispatchResult>>,
}
impl MessageReceipt {
    pub async fn dispatched(&self, cancel: CancellationToken) -> Result<Value> {
        let mut result = self.result.clone();
        loop {
            if let Some(value) = result.borrow_and_update().clone() {
                return value.map_err(|error| anyhow!(error));
            }
            tokio::select! {
                _ = cancel.cancelled() => bail!("Wait aborted"),
                changed = result.changed() => changed.context("Agent message dispatch stopped")?,
            }
        }
    }
}
struct Admitted {
    input: Value,
    id: String,
    complete: watch::Sender<Option<DispatchResult>>,
    _permit: OwnedSemaphorePermit,
}
struct Recipient {
    pending: Vec<Admitted>,
    inflight: Vec<MessageReceipt>,
    deadline: Instant,
    changed: Arc<Notify>,
}
#[derive(Default)]
struct State {
    closed: bool,
    recipients: HashMap<String, Recipient>,
}
struct Inner {
    app: Weak<AppState>,
    state: Mutex<State>,
    capacity: Arc<Semaphore>,
    drained: Notify,
}
#[derive(Clone)]
pub struct AgentMessageDelivery {
    inner: Arc<Inner>,
}
impl AgentMessageDelivery {
    pub fn new(app: Weak<AppState>) -> Self {
        Self {
            inner: Arc::new(Inner {
                app,
                state: Mutex::new(State::default()),
                capacity: Arc::new(Semaphore::new(QUEUE_CAPACITY)),
                drained: Notify::new(),
            }),
        }
    }
    pub(crate) fn try_reserve(&self) -> Result<OwnedSemaphorePermit> {
        self.inner.capacity.clone().try_acquire_owned().map_err(|error| {
            match error {
                tokio::sync::TryAcquireError::Closed => anyhow!("Agent message delivery is closed"),
                tokio::sync::TryAcquireError::NoPermits => anyhow!("Agent message delivery queue is full"),
            }
        })
    }
    pub async fn enqueue(&self, input: Value) -> Result<MessageReceipt> {
        let permit = self
            .inner
            .capacity
            .clone()
            .acquire_owned()
            .await
            .context("Agent message delivery is closed")?;
        self.enqueue_reserved(input, permit).await
    }
    pub(crate) async fn enqueue_reserved(
        &self,
        mut input: Value,
        permit: OwnedSemaphorePermit,
    ) -> Result<MessageReceipt> {
        let recipient_id = input["recipientId"]
            .as_str()
            .filter(|s| !s.is_empty())
            .context("Missing agent message recipient")?
            .to_owned();
        let id = input["messageId"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let prompt = input
            .get_mut("prompt")
            .context("Missing agent message prompt")?
            .take();
        let (prompt, _) = prepare_agent_message(prompt, Some(&input["source"]), Some(&id))?;
        input["prompt"] = prompt;
        let (complete, result) = watch::channel(None);
        let receipt = MessageReceipt {
            message_id: id.clone(),
            result,
        };
        let mut state = self.inner.state.lock().await;
        if state.closed {
            bail!("Agent message delivery is closed");
        }
        let fresh = !state.recipients.contains_key(&recipient_id);
        let recipient = state
            .recipients
            .entry(recipient_id.clone())
            .or_insert_with(|| Recipient {
                pending: Vec::new(),
                inflight: Vec::new(),
                deadline: Instant::now() + QUIET_PERIOD,
                changed: Arc::new(Notify::new()),
            });
        recipient.pending.push(Admitted {
            input,
            id,
            complete,
            _permit: permit,
        });
        recipient.deadline = Instant::now() + QUIET_PERIOD;
        recipient.changed.notify_one();
        if fresh {
            let inner = self.inner.clone();
            tokio::spawn(async move {
                run_recipient(inner, recipient_id).await;
            });
        }
        Ok(receipt)
    }
    pub async fn wait_for_dispatch(
        &self,
        recipient_id: &str,
        cancel: CancellationToken,
    ) -> Result<()> {
        if cancel.is_cancelled() {
            bail!("Wait aborted");
        }
        let receipts = {
            let state = self.inner.state.lock().await;
            state
                .recipients
                .get(recipient_id)
                .map(|recipient| {
                    let mut receipts = recipient.inflight.clone();
                    receipts.extend(recipient.pending.iter().map(|m| MessageReceipt {
                        message_id: m.id.clone(),
                        result: m.complete.subscribe(),
                    }));
                    receipts
                })
                .unwrap_or_default()
        };
        for receipt in receipts {
            receipt.dispatched(cancel.clone()).await?;
        }
        Ok(())
    }
    pub async fn shutdown(&self) -> Result<()> {
        {
            let mut state = self.inner.state.lock().await;
            state.closed = true;
            self.inner.capacity.close();
            for recipient in state.recipients.values() {
                recipient.changed.notify_one();
            }
        }
        loop {
            let drained = self.inner.drained.notified();
            tokio::pin!(drained);
            drained.as_mut().enable();
            if self.inner.state.lock().await.recipients.is_empty() {
                return Ok(());
            }
            drained.await;
        }
    }
}
fn conflicts(group: &[Admitted], mode: Option<&str>, message: &Admitted) -> bool {
    !group.is_empty()
        && (message.input["activeTurnBehavior"]
            .as_str()
            .unwrap_or("steer")
            != group[0].input["activeTurnBehavior"]
                .as_str()
                .unwrap_or("steer")
            || mode
                .zip(message.input["sessionMode"].as_str())
                .is_some_and(|(a, b)| a != b))
}
fn segment(messages: Vec<Admitted>) -> Vec<Vec<Admitted>> {
    let mut groups = Vec::new();
    let mut group = Vec::new();
    let mut mode: Option<String> = None;
    for message in messages {
        if conflicts(&group, mode.as_deref(), &message) {
            groups.push(std::mem::take(&mut group));
            mode = None;
        }
        if mode.is_none() {
            mode = message.input["sessionMode"].as_str().map(str::to_owned);
        }
        group.push(message);
    }
    if !group.is_empty() {
        groups.push(group);
    }
    groups
}
fn batch_request(messages: &mut [Admitted]) -> Value {
    let text_only = messages.iter().all(|m| m.input["prompt"].is_string());
    let prompt = if text_only {
        json!(
            messages
                .iter()
                .map(|m| m.input["prompt"].as_str().unwrap())
                .collect::<Vec<_>>()
                .join("\n\n")
        )
    } else {
        let mut blocks = Vec::new();
        for message in messages.iter_mut() {
            match message.input["prompt"].take() {
                Value::String(text) => blocks.push(json!({"type":"text","text":text})),
                Value::Array(prompt) => blocks.extend(prompt),
                _ => unreachable!("enqueue validates prompts"),
            }
        }
        Value::Array(blocks)
    };
    let mut request = json!({"prompt":prompt,"preparedAgentMessage":true,
        "clientMessageId":messages[0].id,"messageIds":messages.iter().map(|m| &m.id).collect::<Vec<_>>(),
        "activeTurnBehavior":messages[0].input["activeTurnBehavior"].as_str().unwrap_or("steer"),
        "clearPendingPermissions":messages.iter().any(|m| m.input["clearPendingPermissions"] == true),
        "unarchive":messages.iter().any(|m| m.input["unarchive"] != false)});
    if let Some(mode) = messages
        .iter()
        .find_map(|m| m.input["sessionMode"].as_str())
    {
        request["sessionMode"] = json!(mode);
    }
    request
}
async fn run_recipient(inner: Arc<Inner>, recipient_id: String) {
    loop {
        let wait = {
            let mut state = inner.state.lock().await;
            let closed = state.closed;
            let Some(recipient) = state.recipients.get_mut(&recipient_id) else {
                return;
            };
            if recipient.pending.is_empty() {
                state.recipients.remove(&recipient_id);
                inner.drained.notify_waiters();
                return;
            }
            if !closed && recipient.deadline > Instant::now() {
                Some((recipient.deadline, recipient.changed.clone()))
            } else {
                None
            }
        };
        if let Some((deadline, changed)) = wait {
            tokio::select! { _ = tokio::time::sleep_until(deadline) => {}, _ = changed.notified() => {} }
            continue;
        }
        let messages = {
            let mut state = inner.state.lock().await;
            let closed = state.closed;
            let recipient = state.recipients.get_mut(&recipient_id).unwrap();
            // An admission can reset the quiet deadline between the two locks.
            if !closed && recipient.deadline > Instant::now() {
                continue;
            }
            let messages = std::mem::take(&mut recipient.pending);
            recipient.inflight = messages
                .iter()
                .map(|m| MessageReceipt {
                    message_id: m.id.clone(),
                    result: m.complete.subscribe(),
                })
                .collect();
            messages
        };
        for mut messages in segment(messages) {
            let request = batch_request(&mut messages);
            let result = async {
                let manager = inner
                    .app
                    .upgrade()
                    .context("Daemon is shutting down")?
                    .agents
                    .clone();
                manager
                    .send_immediate(
                        &json!({"kind":"system","source":"agent-message-delivery"}),
                        &recipient_id,
                        request,
                    )
                    .await
            }
            .await
            .map_err(|error: anyhow::Error| error.to_string());
            if let Err(error) = &result {
                tracing::warn!(recipient_id, error, "Agent message delivery failed");
            }
            for message in messages {
                message.complete.send_replace(Some(result.clone()));
            }
        }
        if let Some(recipient) = inner.state.lock().await.recipients.get_mut(&recipient_id) {
            recipient.inflight.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn conflicting_modes_keep_order_and_defaults_coalesce() {
        let capacity = Arc::new(Semaphore::new(4));
        let mut messages = Vec::new();
        for (id, mode, behavior) in [
            ("1", None, "interrupt"),
            ("2", Some("plan"), "steer"),
            ("3", Some("full"), "steer"),
            ("4", None, "steer"),
        ] {
            let (complete, _) = watch::channel(None);
            let mut input = json!({"prompt":id,"activeTurnBehavior":behavior});
            if let Some(mode) = mode {
                input["sessionMode"] = json!(mode);
            }
            messages.push(Admitted {
                input,
                id: id.to_owned(),
                complete,
                _permit: capacity.clone().acquire_owned().await.unwrap(),
            });
        }
        let mut groups = segment(messages);
        assert_eq!(groups.iter().map(Vec::len).collect::<Vec<_>>(), [1, 1, 2]);
        let request = batch_request(&mut groups[2]);
        assert_eq!(request["sessionMode"], "full");
        assert_eq!(request["prompt"], "3\n\n4");
        assert_eq!(request["messageIds"], json!(["3", "4"]));
    }
    #[tokio::test]
    async fn receipt_cancellation_does_not_cancel_dispatch() {
        let (complete, result) = watch::channel(None);
        let receipt = MessageReceipt {
            message_id: "x".to_owned(),
            result,
        };
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(receipt.dispatched(cancel).await.is_err());
        complete.send_replace(Some(Ok(json!("steered"))));
        assert_eq!(
            receipt.dispatched(CancellationToken::new()).await.unwrap(),
            "steered"
        );
    }

    #[tokio::test]
    async fn queued_admission_and_duplicate_do_not_wait_for_dispatch() -> Result<()> {
        use crate::agent::receipts::MessageReceipts;
        let home = tempfile::tempdir()?;
        let receipts = MessageReceipts::new(home.path().to_owned());
        let request = json!({"prompt":"hello"});
        let (complete, result) = watch::channel(None);
        let pending = MessageReceipt { message_id: "message".into(), result };
        let (accepted, owner) = tokio::time::timeout(
            Duration::from_secs(1),
            receipts.enqueue("agent", "message", &request, || Ok(()), |_| async { Ok(pending) }),
        )
        .await??
        .unwrap();
        assert!(owner);
        // The pending receipt is already durable, but cannot be replayed by a
        // reconstructed daemon without proof of dispatch.
        let restarted = MessageReceipts::new(home.path().to_owned());
        let error = restarted
            .enqueue("agent", "message", &request, || Ok(()), |_| async {
                panic!("restart must not enqueue an uncertain message")
            })
            .await
            .err()
            .unwrap();
        assert_eq!(error.to_string(), "agent_request_outcome_unknown");
        let (duplicate, owner) = tokio::time::timeout(
            Duration::from_secs(1),
            receipts.enqueue("agent", "message", &request, || Ok(()), |_| async {
                panic!("live duplicate must not enqueue twice")
            }),
        )
        .await??
        .unwrap();
        assert!(!owner);
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(accepted.dispatched(cancel).await.is_err());
        drop(accepted);
        complete.send_replace(Some(Ok(json!({"disposition":"steered"}))));
        duplicate.dispatched(CancellationToken::new()).await?;
        receipts.finish_queued("agent", "message", true).await?;
        assert!(
            restarted
                .enqueue("agent", "message", &request, || Ok(()), |_| async {
                    panic!("completed receipt must not enqueue again")
                })
                .await?
                .is_none()
        );
        Ok(())
    }

    #[tokio::test]
    async fn closed_dispatch_owner_releases_waiter_and_preserves_unknown_outcome() -> Result<()> {
        use crate::agent::receipts::MessageReceipts;
        let home = tempfile::tempdir()?;
        let receipts = MessageReceipts::new(home.path().to_owned());
        let (complete, result) = watch::channel(None);
        let request = json!({"prompt":"hello"});
        let (receipt, owner) = receipts
            .enqueue("agent", "message", &request, || Ok(()), |_| async {
                Ok(MessageReceipt { message_id: "message".into(), result })
            })
            .await?
            .unwrap();
        assert!(owner);
        drop(complete);
        assert!(
            tokio::time::timeout(
                Duration::from_secs(1),
                receipt.dispatched(CancellationToken::new()),
            )
            .await?
            .is_err()
        );
        receipts.finish_queued("agent", "message", false).await?;
        let error = receipts
            .enqueue("agent", "message", &request, || Ok(()), |_| async {
                panic!("failed dispatch must not be retried implicitly")
            })
            .await
            .err()
            .unwrap();
        assert_eq!(error.to_string(), "agent_request_outcome_unknown");
        Ok(())
    }

    #[tokio::test]
    async fn full_or_closed_queue_rejects_before_durable_admission() -> Result<()> {
        use crate::agent::receipts::MessageReceipts;
        let home = tempfile::tempdir()?;
        let receipts = MessageReceipts::new(home.path().to_owned());
        let queue = AgentMessageDelivery::new(Weak::new());
        let request = json!({"recipientId":"agent","messageId":"message","prompt":"hello","source":{"kind":"agent-message","agentId":"sender"}});
        let full = queue.inner.capacity.clone().acquire_many_owned(QUEUE_CAPACITY as u32).await?;
        let error = receipts
            .enqueue("agent", "message", &request, || queue.try_reserve(), |permit| {
                queue.enqueue_reserved(request.clone(), permit)
            })
            .await
            .err()
            .unwrap();
        assert_eq!(error.to_string(), "Agent message delivery queue is full");
        drop(full);
        queue.shutdown().await?;
        let error = receipts
            .enqueue("agent", "message", &request, || queue.try_reserve(), |permit| {
                queue.enqueue_reserved(request.clone(), permit)
            })
            .await
            .err()
            .unwrap();
        assert_eq!(error.to_string(), "Agent message delivery is closed");
        let queue = AgentMessageDelivery::new(Weak::new());
        let (receipt, owner) = receipts
            .enqueue("agent", "message", &request, || queue.try_reserve(), |permit| {
                queue.enqueue_reserved(request.clone(), permit)
            })
            .await?
            .unwrap();
        assert!(owner, "failed reservation must leave the message retryable");
        queue.shutdown().await?;
        assert!(receipt.dispatched(CancellationToken::new()).await.is_err());
        receipts.finish_queued("agent", "message", false).await?;
        Ok(())
    }
}
