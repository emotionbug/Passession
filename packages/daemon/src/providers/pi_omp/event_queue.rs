use super::transport::Transport;
use crate::agent::event_stage::EventStage;
use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use std::sync::{Arc, Weak, atomic::{AtomicUsize, Ordering}};
use tokio::sync::{Mutex, Notify, mpsc};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct State {
    stage: EventStage,
    pending: bool,
    failure: Option<String>,
}
struct Shared {
    state: Mutex<State>,
    changed: Notify,
    writers: AtomicUsize,
    consumer_gone: CancellationToken,
    runtime: Weak<Transport>,
}
pub(super) struct Sender(Arc<Shared>);
impl Clone for Sender {
    fn clone(&self) -> Self {
        self.0.writers.fetch_add(1, Ordering::Relaxed);
        Self(self.0.clone())
    }
}
impl Drop for Sender {
    fn drop(&mut self) {
        self.0.writers.fetch_sub(1, Ordering::AcqRel);
        self.0.changed.notify_one();
    }
}
impl Sender {
    pub(super) async fn send(&self, event: Value) -> Result<()> {
        if self.0.consumer_gone.is_cancelled() { bail!("Provider event consumer closed"); }
        let mut state = self.0.state.lock().await;
        if let Some(error) = &state.failure { return Err(anyhow!(error.clone())); }
        if let Err(error) = state.stage.push(event).await {
            let message = format!("Provider event spool failed: {error}");
            state.failure = Some(message.clone());
            drop(state);
            self.0.changed.notify_one();
            if let Some(runtime) = self.0.runtime.upgrade() { runtime.event_queue_failed(&message); }
            return Err(anyhow!(message));
        }
        state.pending = true;
        drop(state);
        self.0.changed.notify_one();
        Ok(())
    }
}

/// Only the delivery pump waits for the event consumer. The stdout reader can
/// continue resolving ACKs and host controls while ordinary events spill in FIFO
/// order using the same bounded-memory spool as admission staging.
pub(super) fn channel(runtime: &Arc<Transport>, cancel: CancellationToken) -> (Sender, mpsc::Receiver<Value>) {
    let shared = Arc::new(Shared {
        state: Mutex::new(State::default()), changed: Notify::new(),
        writers: AtomicUsize::new(1), consumer_gone: CancellationToken::new(),
        runtime: Arc::downgrade(runtime),
    });
    // The spool, not another large frame-count-only queue, owns the backlog.
    let (output, receiver) = mpsc::channel(1);
    let delivery = shared.clone();
    tokio::spawn(async move {
        let _consumer = delivery.consumer_gone.clone().drop_guard();
        let result = tokio::select! {
            _ = cancel.cancelled() => Ok(()),
            result = pump(&delivery, &output) => result,
        };
        if let Err(error) = result {
            let message = format!("Provider event delivery failed: {error}");
            if let Some(runtime) = delivery.runtime.upgrade() { runtime.event_queue_failed(&message); }
            // A terminal delivery error is not silently converted into an empty
            // history. Retain it until the consumer drains or closes.
            let _ = output.send(json!({"type":"process_exit","error":message})).await;
        }
    });
    (Sender(shared), receiver)
}
async fn pump(shared: &Shared, output: &mpsc::Sender<Value>) -> Result<()> {
    loop {
        let changed = shared.changed.notified();
        let stage = {
            let mut state = shared.state.lock().await;
            if let Some(error) = &state.failure { return Err(anyhow!(error.clone())); }
            if state.pending {
                state.pending = false;
                Some(std::mem::take(&mut state.stage))
            } else {
                if shared.writers.load(Ordering::Acquire) == 0 { return Ok(()); }
                None
            }
        };
        let Some(stage) = stage else {
            tokio::select! { _ = output.closed() => return Ok(()), _ = changed => {} }
            continue;
        };
        let mut replay = stage.replay().await?;
        loop {
            let batch = replay.next_batch().await?;
            if batch.is_empty() { break; }
            for event in batch {
                if output.send(event).await.is_err() { return Ok(()); }
            }
        }
    }
}
