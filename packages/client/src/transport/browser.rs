use super::{Connection, HostError, HostFrame, Queued, Sender};
use crate::transport::target::{Endpoint, Target};
use anyhow::{Result, anyhow, bail};
use paseo_browser::{Socket, SocketEvent};
use paseo_protocol::crypto::SecurityMode;
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

const BUFFER_LIMIT: usize = 128 * 1024 * 1024;

impl Connection {
    pub async fn connect(
        target: &Target,
        minimum: SecurityMode,
        global: Arc<Semaphore>,
        parent: CancellationToken,
    ) -> Result<Self> {
        let Endpoint::Socket { url, ipc: None } = &target.endpoint else {
            return Err(HostError::new(
                "BROWSER_UNSUPPORTED",
                "IPC and SSH are not available in the browser",
            )
            .into());
        };
        if target.security.is_some() || minimum != SecurityMode::Legacy {
            return Err(HostError::new(
                "BROWSER_UNSUPPORTED",
                "Encrypted pairing is not available in the browser",
            )
            .into());
        }
        let cancel = parent.child_token();
        if cancel.is_cancelled() {
            bail!("Host connection cancelled");
        }
        let (events, mut received) = mpsc::channel(256);
        let (overflow, mut overflowed) = oneshot::channel();
        let mut overflow = Some(overflow);
        let budget = Arc::new(Semaphore::new(BUFFER_LIMIT));
        let socket = Socket::open(url, move |event| {
            if overflow.is_none() {
                return;
            }
            let len = match &event {
                SocketEvent::Text(text) => text.len(),
                SocketEvent::Binary(bytes) => bytes.len(),
                _ => 0,
            };
            if budget
                .clone()
                .try_acquire_many_owned(len as u32)
                .is_ok_and(|permit| events.try_send((event, permit)).is_ok())
            {
                return;
            }
            if let Some(signal) = overflow.take() {
                let _ = signal.send(());
            }
        });
        tokio::select! { biased;
            _ = cancel.cancelled() => bail!("Host connection cancelled"),
            event = received.recv() => match event {
                Some((SocketEvent::Opened, _)) => {},
                Some((SocketEvent::Closed { code }, _)) => return Err(closed(code)),
                _ => bail!("Browser WebSocket connection failed"),
            },
            _ = tokio::time::sleep(Duration::from_secs(10)) => bail!("Host connection timed out"),
        }
        let (tx, mut rx) = mpsc::channel::<Queued>(256);
        let (urgent, mut urgent_rx) = mpsc::channel::<Queued>(8);
        let (incoming_tx, incoming) = mpsc::channel(1);
        let sender = Sender {
            tx,
            urgent,
            global,
            local: Arc::new(Semaphore::new(BUFFER_LIMIT)),
            cancel: cancel.clone(),
        };
        let written = Arc::new(AtomicU64::new(0));
        let counted = written.clone();
        let task_cancel = cancel.clone();
        let task = tokio::spawn(async move {
            let reading = async {
                while let Some((event, _permit)) = received.recv().await {
                    let frame = match event {
                        SocketEvent::Text(text) => HostFrame::Text(text),
                        SocketEvent::Binary(bytes) => HostFrame::Binary(bytes.into()),
                        SocketEvent::Closed { code } => return Err(closed(code)),
                        // The terminal Closed event has the useful close code.
                        SocketEvent::Opened | SocketEvent::Failed => continue,
                    };
                    incoming_tx.send(Ok(frame)).await?;
                }
                Err(anyhow!("Browser WebSocket closed"))
            };
            let writing = async {
                loop {
                    let (item, urgent) = tokio::select! { biased;
                        Some(item) = urgent_rx.recv() => (item, true),
                        item = rx.recv() => match item { Some(item) => (item, false), None => break },
                    };
                    // WebSocket.send returns after enqueueing in the browser. Keep that queue
                    // bounded and make a flush wait for all previous sends to drain.
                    let Queued {
                        frame,
                        _global,
                        _local,
                    } = item;
                    let sent = match frame {
                        HostFrame::Text(text) => socket.send_text(&text),
                        HostFrame::Binary(bytes) => socket.send_binary(&bytes),
                        HostFrame::Close { .. } => break,
                        HostFrame::Flush(reply) => {
                            drain(&socket, 0).await;
                            let _ = reply.send(Ok(()));
                            continue;
                        }
                    };
                    if !sent {
                        bail!("Browser WebSocket send failed");
                    }
                    drain(&socket, 256 * 1024).await;
                    if !urgent {
                        counted.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Ok(())
            };
            let result = tokio::select! { biased;
                _ = task_cancel.cancelled() => Ok(()),
                Ok(()) = &mut overflowed => Err(HostError::new("resource_limit", "Browser WebSocket receive buffer limit reached").into()),
                result = reading => result,
                result = writing => result,
            };
            drop(socket);
            if let Err(error) = result {
                tokio::select! { _ = task_cancel.cancelled() => {}, _ = incoming_tx.send(Err(error)) => {} }
            }
            task_cancel.cancel();
        });
        Ok(Self {
            sender,
            incoming,
            cancel,
            task,
            security: None,
            written,
        })
    }
}

async fn drain(socket: &Socket, remaining: usize) {
    while socket.buffered_amount() > remaining {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn closed(code: u16) -> anyhow::Error {
    if code == 4401 {
        HostError::new(
            "authentication_failed",
            "Host rejected the credentials. Replace the connection credentials.",
        )
        .into()
    } else {
        anyhow!("Browser WebSocket closed ({code})")
    }
}
