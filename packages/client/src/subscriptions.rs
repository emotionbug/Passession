use crate::{ClientError, ClientEvents, ConnectOptions, DaemonClient, RequestOptions};
use anyhow::{Result, bail, ensure};
use bytes::Bytes;
use paseo_protocol::binary_frames::{
    TerminalOpcode, TerminalResize, decode_terminal_frame, encode_terminal_frame,
    encode_terminal_resize,
};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

/// Snapshot resets the subscription baseline. `restored` requires the consumer to
/// repair history gaps using CursorPages; live streams do not pretend to replay history.
#[derive(Debug)]
pub enum SubscriptionEvent {
    Snapshot { payload: Value, restored: bool },
    Update(Value),
}

pub struct OwnedSubscription {
    pub snapshot: Value,
    pub subscription_id: String,
    client: Arc<DaemonClient>,
    events: ClientEvents<Value>,
    cancel: CancellationToken,
    released: bool,
}
impl OwnedSubscription {
    pub async fn recv(&mut self) -> Result<Value> {
        loop {
            let event = tokio::select! {biased;_ = self.cancel.cancelled()=>bail!("Subscription cancelled"),event=self.events.recv()=>event?};
            if !self.snapshot["requestId"].is_null()
                && event["payload"]["requestId"] == self.snapshot["requestId"]
            {
                continue;
            }
            if event["payload"]["subscriptionId"] == self.subscription_id
                || event["subscriptionId"] == self.subscription_id
            {
                return Ok(event);
            }
        }
    }
    pub async fn release(&mut self) -> Result<()> {
        if self.released {
            return Ok(());
        }
        self.released = true;
        self.cancel.cancel();
        self.client
            .release_subscription(&self.subscription_id)
            .await
    }
}
impl Drop for OwnedSubscription {
    fn drop(&mut self) {
        self.cancel.cancel();
        if !self.released && !self.client.cancellation().is_cancelled() {
            let client = self.client.clone();
            let id = self.subscription_id.clone();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    let _ = client.release_subscription(&id).await;
                });
            }
        }
    }
}
impl DaemonClient {
    /// Subscribe before bootstrap so updates arriving before the response are retained.
    pub async fn observe(
        self: &Arc<Self>,
        request: &str,
        response: &str,
        fields: Value,
        options: RequestOptions,
    ) -> Result<OwnedSubscription> {
        let events = self.subscribe();
        self.require_feature(
            "ownedSubscriptions",
            "Update the host to use owned subscriptions",
        )?;
        let cancel = options.cancel.child_token();
        let snapshot = self.rpc(request, response, fields, options).await?;
        let subscription_id = snapshot["subscriptionId"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Subscription response omitted subscriptionId"))?
            .to_owned();
        Ok(OwnedSubscription {
            snapshot,
            subscription_id,
            client: self.clone(),
            events,
            cancel,
            released: false,
        })
    }
    pub async fn observe_agents(
        self: &Arc<Self>,
        mut fields: Value,
        options: RequestOptions,
    ) -> Result<OwnedSubscription> {
        if fields.is_null() {
            fields = json!({});
        }
        ensure!(fields.is_object(), "Agent query must be an object");
        fields["subscribe"] = json!({});
        self.observe(
            "fetch_agents_request",
            "fetch_agents_response",
            fields,
            options,
        )
        .await
    }
    pub async fn observe_workspaces(
        self: &Arc<Self>,
        mut fields: Value,
        options: RequestOptions,
    ) -> Result<OwnedSubscription> {
        if fields.is_null() {
            fields = json!({});
        }
        ensure!(fields.is_object(), "Workspace query must be an object");
        fields["subscribe"] = json!({});
        self.observe(
            "fetch_workspaces_request",
            "fetch_workspaces_response",
            fields,
            options,
        )
        .await
    }
    pub async fn observe_events(
        self: &Arc<Self>,
        fields: Value,
        options: RequestOptions,
    ) -> Result<OwnedSubscription> {
        self.observe(
            "session.events.set_subscription.request",
            "session.events.set_subscription.response",
            fields,
            options,
        )
        .await
    }
    pub async fn observe_timeline(
        self: &Arc<Self>,
        agent_ids: &[String],
        options: RequestOptions,
    ) -> Result<OwnedSubscription> {
        self.observe(
            "agent.timeline.set_subscription.request",
            "agent.timeline.set_subscription.response",
            json!({"agentIds":agent_ids}),
            options,
        )
        .await
    }
    pub async fn observe_terminals(
        self: &Arc<Self>,
        fields: Value,
        options: RequestOptions,
    ) -> Result<OwnedSubscription> {
        self.observe(
            "subscribe_terminals_request",
            "terminals_changed",
            fields,
            options,
        )
        .await
    }
    pub async fn observe_terminal(
        self: &Arc<Self>,
        terminal_id: &str,
        restore: Option<Value>,
        options: RequestOptions,
    ) -> Result<TerminalStream> {
        let binary = self.subscribe_binary();
        let mut fields = json!({"terminalId":terminal_id});
        if let Some(restore) = restore {
            fields["restore"] = restore;
        }
        let subscription = self
            .observe(
                "subscribe_terminal_request",
                "subscribe_terminal_response",
                fields,
                options,
            )
            .await?;
        let slot = subscription.snapshot["slot"]
            .as_u64()
            .filter(|slot| *slot <= 255)
            .ok_or_else(|| anyhow::anyhow!("Terminal response omitted slot"))?
            as u8;
        Ok(TerminalStream {
            subscription,
            binary,
            slot,
        })
    }
}
#[derive(Debug)]
pub enum TerminalEvent {
    Frame {
        opcode: TerminalOpcode,
        payload: Bytes,
    },
    Exit(Value),
}
pub struct TerminalStream {
    pub subscription: OwnedSubscription,
    binary: ClientEvents<Bytes>,
    slot: u8,
}
impl TerminalStream {
    pub async fn recv(&mut self) -> Result<TerminalEvent> {
        loop {
            tokio::select! {
                event=self.subscription.recv()=>{let event=event?;if event["type"]=="terminal_stream_exit" {self.subscription.released=true;self.subscription.cancel.cancel();return Ok(TerminalEvent::Exit(event));}},
                bytes=self.binary.recv()=>{let bytes=bytes?;if bytes.first().is_some_and(|op|matches!(*op,1..=5)){let frame=decode_terminal_frame(&bytes)?;if frame.slot==self.slot{return Ok(TerminalEvent::Frame{opcode:frame.opcode,payload:bytes.slice(2..)});}}}
            }
        }
    }
    pub async fn input(&self, bytes: &[u8]) -> Result<()> {
        self.subscription
            .client
            .send_binary(encode_terminal_frame(TerminalOpcode::Input, self.slot, bytes).into())
            .await
    }
    pub async fn resize(&self, size: TerminalResize) -> Result<()> {
        self.subscription
            .client
            .send_binary(
                encode_terminal_frame(
                    TerminalOpcode::Resize,
                    self.slot,
                    &encode_terminal_resize(&size)?,
                )
                .into(),
            )
            .await
    }
}

/// Owns connection retry, never mutation replay. Existing requests fail on disconnect;
/// only explicitly owned subscriptions are re-established with a new snapshot.
pub struct ReconnectingClient {
    current: watch::Receiver<Option<Arc<DaemonClient>>>,
    cancel: CancellationToken,
    task: Option<tokio::task::JoinHandle<()>>,
}
impl ReconnectingClient {
    pub async fn connect(options: ConnectOptions, cancel: CancellationToken) -> Result<Self> {
        let cancel = cancel.child_token();
        let first = Arc::new(DaemonClient::connect(options.clone(), cancel.child_token()).await?);
        let (sender, current) = watch::channel(Some(first.clone()));
        let lifetime = cancel.clone();
        let task = tokio::spawn(async move {
            let mut active = first;
            loop {
                let disconnected = active.cancellation();
                tokio::select! {_=lifetime.cancelled()=>{active.close().await;break},_=disconnected.cancelled()=>{}}
                sender.send_replace(None);
                active.close().await;
                let mut delay = Duration::from_millis(100);
                loop {
                    tokio::select! {_=lifetime.cancelled()=>return,_=tokio::time::sleep(delay)=>{}}
                    match DaemonClient::connect(options.clone(), lifetime.child_token()).await {
                        Ok(client) => {
                            active = Arc::new(client);
                            sender.send_replace(Some(active.clone()));
                            break;
                        }
                        Err(_) => {
                            delay = (delay * 2).min(Duration::from_secs(10));
                        }
                    }
                }
            }
        });
        Ok(Self {
            current,
            cancel,
            task: Some(task),
        })
    }
    pub async fn connection(&self, cancel: &CancellationToken) -> Result<Arc<DaemonClient>> {
        wait_connection(&mut self.current.clone(), cancel, &self.cancel).await
    }
    pub fn observe(
        &self,
        request: &str,
        response: &str,
        fields: Value,
        options: RequestOptions,
    ) -> RestoringSubscription {
        let cancel = options.cancel.child_token();
        let stop = cancel.clone();
        let lifetime = self.cancel.clone();
        let mut current = self.current.clone();
        let request = request.to_owned();
        let response = response.to_owned();
        let (tx, rx) = mpsc::channel(256);
        let task = tokio::spawn(async move {
            let run = async {
                let mut restored = false;
                loop {
                    let client = wait_connection(&mut current, &stop, &lifetime).await?;
                    let mut subscription = match client
                        .observe(
                            &request,
                            &response,
                            fields.clone(),
                            RequestOptions {
                                timeout: options.timeout,
                                cancel: stop.child_token(),
                            },
                        )
                        .await
                    {
                        Ok(subscription) => subscription,
                        Err(_) if client.cancellation().is_cancelled() => continue,
                        Err(error) => return Err(error),
                    };
                    tokio::select! {biased;_=stop.cancelled()=>return Ok(()),_=lifetime.cancelled()=>return Ok(()),result=tx.send(Ok(SubscriptionEvent::Snapshot{payload:subscription.snapshot.clone(),restored}))=>{if result.is_err(){return Ok(())}}}
                    restored = true;
                    loop {
                        let result = tokio::select! {biased;_=stop.cancelled()=>{let _=subscription.release().await;return Ok(())},_=lifetime.cancelled()=>return Ok(()),result=subscription.recv()=>result};
                        match result {
                            Ok(event) => {
                                tokio::select! {biased;_=stop.cancelled()=>return Ok(()),_=lifetime.cancelled()=>return Ok(()),result=tx.send(Ok(SubscriptionEvent::Update(event)))=>{if result.is_err(){return Ok(())}}}
                            }
                            Err(error) if client.cancellation().is_cancelled() => {
                                let _ = error;
                                break;
                            }
                            Err(error)
                                if error
                                    .downcast_ref::<tokio::sync::broadcast::error::RecvError>()
                                    .is_some_and(|e| {
                                        matches!(
                                            e,
                                            tokio::sync::broadcast::error::RecvError::Lagged(_)
                                        )
                                    }) =>
                            {
                                let _ = subscription.release().await;
                                break;
                            }
                            Err(error) => return Err(error),
                        }
                    }
                }
            };
            if let Err(error) = run.await {
                tokio::select! {biased;_=stop.cancelled()=>{},_=lifetime.cancelled()=>{},_=tx.send(Err(error))=>{}}
            }
        });
        RestoringSubscription {
            receiver: rx,
            cancel,
            task: Some(task),
        }
    }
    /// Recover a known idempotency key after an uncertain creation. Only a missing
    /// snapshot permits resubmission, always with the original key and fields.
    pub async fn recover_creation(
        &self,
        kind: &str,
        key: &str,
        mut fields: Value,
        options: RequestOptions,
    ) -> Result<Value> {
        ensure!(
            matches!(kind, "agent" | "workspace"),
            "Unknown creation kind"
        );
        ensure!(fields.is_object(), "Creation fields must be an object");
        fields["idempotencyKey"] = key.into();
        let recover = async {
            loop {
                let client = self.connection(&options.cancel).await?;
                let mut subscription = match client
                    .observe(
                        "creation.subscribe.request",
                        "creation.subscribe.response",
                        json!({"kind":kind,"idempotencyKey":key}),
                        RequestOptions {
                            timeout: options.timeout,
                            cancel: options.cancel.child_token(),
                        },
                    )
                    .await
                {
                    Ok(subscription) => subscription,
                    Err(_) if client.cancellation().is_cancelled() => continue,
                    Err(error) => return Err(error),
                };
                let mut snapshot = subscription
                    .snapshot
                    .get("snapshot")
                    .cloned()
                    .unwrap_or(Value::Null);
                if snapshot.is_null() {
                    let result = client
                        .rpc(
                            &format!("{kind}.create.request"),
                            &format!("{kind}.create.response"),
                            fields.clone(),
                            RequestOptions {
                                timeout: options.timeout,
                                cancel: options.cancel.child_token(),
                            },
                        )
                        .await;
                    match result {
                        Ok(value) => {
                            let _ = subscription.release().await;
                            return Ok(value);
                        }
                        Err(_) if client.cancellation().is_cancelled() => continue,
                        Err(error) => return Err(error),
                    }
                }
                loop {
                    match snapshot["phase"].as_str() {
                        Some("completed") => {
                            let _ = subscription.release().await;
                            return Ok(snapshot);
                        }
                        Some("failed") => {
                            return Err(ClientError::new(
                                snapshot["errorCode"].as_str().unwrap_or("CREATION_FAILED"),
                                snapshot["error"].as_str().unwrap_or("Creation failed"),
                            )
                            .details(snapshot)
                            .into());
                        }
                        _ => {}
                    }
                    match subscription.recv().await {
                        Ok(event) => {
                            snapshot = event["payload"]
                                .get("snapshot")
                                .unwrap_or(&event["payload"])
                                .clone()
                        }
                        Err(_) if client.cancellation().is_cancelled() => break,
                        Err(error) => return Err(error),
                    }
                }
            }
        };
        tokio::select! {biased;_=options.cancel.cancelled()=>bail!("Creation recovery cancelled"),_=async{if options.timeout.is_zero(){std::future::pending::<()>().await}else{tokio::time::sleep(options.timeout).await}}=>bail!("Creation recovery timed out"),result=recover=>result}
    }
    pub async fn close(&mut self) {
        self.cancel.cancel();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}
impl Drop for ReconnectingClient {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
async fn wait_connection(
    current: &mut watch::Receiver<Option<Arc<DaemonClient>>>,
    cancel: &CancellationToken,
    lifetime: &CancellationToken,
) -> Result<Arc<DaemonClient>> {
    loop {
        if cancel.is_cancelled() || lifetime.is_cancelled() {
            bail!("Connection wait cancelled")
        }
        if let Some(client) = current.borrow_and_update().clone() {
            if !client.cancellation().is_cancelled() {
                return Ok(client);
            }
        }
        tokio::select! {biased;_=cancel.cancelled()=>bail!("Connection wait cancelled"),_=lifetime.cancelled()=>bail!("Client closed"),result=current.changed()=>result?}
    }
}
pub struct RestoringSubscription {
    receiver: mpsc::Receiver<Result<SubscriptionEvent>>,
    cancel: CancellationToken,
    task: Option<tokio::task::JoinHandle<()>>,
}
impl RestoringSubscription {
    pub async fn recv(&mut self) -> Option<Result<SubscriptionEvent>> {
        self.receiver.recv().await
    }
    pub async fn release(&mut self) {
        self.cancel.cancel();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}
impl Drop for RestoringSubscription {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
