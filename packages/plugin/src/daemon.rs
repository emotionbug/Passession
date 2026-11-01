//! Lazily connected daemon API over the plugin's physical IPC session. A host
//! close invalidates the cached client; the next call opens a fresh logical session.
use crate::{Result, anyhow, runtime::Output};
use base64::{Engine, engine::general_purpose::STANDARD};
pub use paseo_client;
use paseo_client::{DaemonClient, transport::HostFrame};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

struct Session {
    incoming: mpsc::Sender<Result<HostFrame>>,
    cancel: CancellationToken,
}
struct Inner {
    output: Output,
    plugin_id: String,
    stop: CancellationToken,
    session: Mutex<Option<Session>>,
    client: tokio::sync::Mutex<Option<Arc<DaemonClient>>>,
}
#[derive(Clone)]
pub struct Daemon(Arc<Inner>);
impl Daemon {
    pub(crate) fn new(output: Output, plugin_id: String, stop: CancellationToken) -> Self {
        Self(Arc::new(Inner {
            output,
            plugin_id,
            stop,
            session: Mutex::new(None),
            client: tokio::sync::Mutex::new(None),
        }))
    }
    pub async fn client(&self) -> Result<Arc<DaemonClient>> {
        let mut cached = self.0.client.lock().await;
        if let Some(client) = cached.as_ref().filter(|c| !c.cancellation().is_cancelled()) {
            return Ok(client.clone());
        }
        cached.take();
        let cancel = self.0.stop.child_token();
        let (outgoing, mut outgoing_rx) = mpsc::channel::<HostFrame>(16);
        let (incoming, incoming_rx) = mpsc::channel(16);
        *self.0.session.lock().unwrap() = Some(Session {
            incoming,
            cancel: cancel.clone(),
        });
        let output = self.0.output.clone();
        let done = cancel.clone();
        let task = tokio::spawn(async move {
            loop {
                let frame = tokio::select! {_=done.cancelled()=>break,frame=outgoing_rx.recv()=>match frame {Some(frame)=>frame,None=>break}};
                let result=match frame {
                    HostFrame::Text(text)=>output.send(json!({"type":"paseo_frame","data":text,"isBinary":false})).await,
                    HostFrame::Binary(bytes)=>output.send(json!({"type":"paseo_frame","data":{"$bytes":STANDARD.encode(bytes)},"isBinary":true})).await,
                    HostFrame::Close{..}=>output.send(json!({"type":"paseo_close"})).await,
                    HostFrame::Flush(reply)=> {let result=output.flush().await; let failed=result.is_err(); let _=reply.send(result); if failed {break;} continue;},
                };
                if result.is_err() {
                    break;
                }
            }
            done.cancel();
        });
        let id = format!("plugin:{}", self.0.plugin_id);
        let client = DaemonClient::from_transport(
            outgoing,
            incoming_rx,
            cancel,
            task,
            &id,
            "cli",
            Duration::from_secs(30),
        )
        .await?;
        let client = Arc::new(client);
        *cached = Some(client.clone());
        Ok(client)
    }
    pub(crate) fn incoming(&self, message: Value) -> Result<()> {
        let guard = self.0.session.lock().unwrap();
        let Some(session) = guard.as_ref() else {
            return Ok(());
        };
        if message["type"] == "paseo_close" {
            session.cancel.cancel();
            return Ok(());
        }
        let frame = if message["isBinary"] == true {
            HostFrame::Binary(
                STANDARD
                    .decode(
                        message["data"]["$bytes"]
                            .as_str()
                            .ok_or_else(|| anyhow!("Invalid binary daemon frame"))?,
                    )?
                    .into(),
            )
        } else {
            HostFrame::Text(
                message["data"]
                    .as_str()
                    .ok_or_else(|| anyhow!("Invalid text daemon frame"))?
                    .to_owned(),
            )
        };
        if session.incoming.try_send(Ok(frame)).is_err() {
            session.cancel.cancel();
            return Err(anyhow!("Daemon IPC consumer exceeded its bounded queue"));
        }
        Ok(())
    }
    pub(crate) async fn close(&self) -> Result<()> {
        self.0.stop.cancel();
        self.0.client.lock().await.take();
        self.0.session.lock().unwrap().take();
        self.0.output.send(json!({"type":"paseo_close"})).await
    }
}
