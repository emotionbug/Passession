//! Owned daemon subscriptions (Flutter `PaseoClient.observe` / `OwnedObservation`) over the
//! core's [`CoreHandle::observe`]: the first snapshot arrives as the observation's
//! `Response`; after a reconnect the core re-subscribes and delivers
//! `subscription.restored` (a fresh snapshot replaces the old one) or `subscription.error`;
//! every routed update carries the observation id in its top-level `observations` list.
//! Dropping an [`Observation`] releases it (`cancel_request`).

use paseo_gui_core::{CoreEvent, CoreHandle, RequestId};
use serde_json::Value;

use super::rpc::{message, payload, str_of, RpcError};

/// One item of an observation stream.
#[derive(Debug, Clone)]
pub enum ObsEvent {
    /// The subscription snapshot (`subscriptionId`, snapshot fields). `restored` for every
    /// snapshot after a reconnect.
    Snapshot { payload: Value, restored: bool },
    /// A routed subscription event message (`{type, payload}`).
    Event(Value),
    /// The first start failed; the observation is over.
    Failed(RpcError),
    /// Re-subscribing after a reconnect failed (`subscription.error`).
    RestoreFailed(String),
}

pub struct Observation {
    core: CoreHandle,
    pub id: RequestId,
    pub host_id: String,
    over: bool,
}

impl Observation {
    pub fn start(core: &CoreHandle, host_id: &str, request_type: &str, response_type: &str, fields: Value) -> Self {
        let id = core.observe(host_id, message(request_type, fields), response_type);
        Self { core: core.clone(), id, host_id: host_id.to_owned(), over: false }
    }

    /// The items of this observation in one core event batch, in wire order.
    pub fn take(&mut self, events: &[CoreEvent]) -> Vec<ObsEvent> {
        let mut out = Vec::new();
        if self.over {
            return out;
        }
        for event in events {
            match event {
                CoreEvent::Response { request_id, result, .. } if *request_id == self.id => match result {
                    Ok(value) => out.push(ObsEvent::Snapshot { payload: payload(value).clone(), restored: false }),
                    Err(error) => {
                        self.over = true;
                        out.push(ObsEvent::Failed(RpcError {
                            message: error.message.clone(),
                            code: error.code.clone(),
                            outcome_unknown: error.outcome_unknown,
                        }));
                    }
                },
                CoreEvent::HostEvent { host_id, event } if *host_id == self.host_id && routed_to(event, &self.id) => {
                    match str_of(event, "type") {
                        "subscription.restored" => {
                            out.push(ObsEvent::Snapshot { payload: payload(event).clone(), restored: true })
                        }
                        "subscription.error" => {
                            let body = payload(event);
                            let text = str_of(body, "error");
                            out.push(ObsEvent::RestoreFailed(text.to_owned()));
                        }
                        _ => out.push(ObsEvent::Event(event.clone())),
                    }
                }
                _ => {}
            }
        }
        out
    }

    pub fn is_over(&self) -> bool {
        self.over
    }
}

impl Drop for Observation {
    fn drop(&mut self) {
        if !self.over {
            self.core.cancel_request(&self.id);
        }
    }
}

/// Whether a host event belongs to observation `id` (`observations` list or the
/// `observationId` of restore notices).
pub fn routed_to(event: &Value, id: &str) -> bool {
    if event
        .get("observations")
        .and_then(Value::as_array)
        .is_some_and(|ids| ids.iter().any(|value| value.as_str() == Some(id)))
    {
        return true;
    }
    str_of(payload(event), "observationId") == id
}
