use paseo_client::{ClientError, crypto::ChannelError, transport::HostError};
use serde_json::{Value, json};

/// A failure reported to the GUI. `outcome_unknown` is true when a submitted request may
/// still have taken effect (Flutter `DaemonException.outcomeUnknown`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoreError {
    pub code: String,
    pub message: String,
    pub outcome_unknown: bool,
}

impl std::fmt::Display for CoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CoreError {}

/// Raised by SDK `request`/`ping` before a frame left this client.
const BEFORE_SUBMISSION: &str = "before submission";

impl CoreError {
    pub(crate) fn new(code: &str, message: impl Into<String>, outcome_unknown: bool) -> Self {
        Self {
            code: code.to_owned(),
            message: message.into(),
            outcome_unknown,
        }
    }

    pub(crate) fn to_json(&self) -> Value {
        json!({"code": self.code, "message": self.message, "outcomeUnknown": self.outcome_unknown})
    }

    /// Classifies SDK, transport and relay security errors without echoing endpoints.
    pub(crate) fn from_anyhow(error: &anyhow::Error) -> Self {
        if let Some(error) = error.downcast_ref::<CoreError>() {
            return error.clone();
        }
        if let Some(error) = error.downcast_ref::<ClientError>() {
            let outcome_unknown = match error.code.as_str() {
                "DAEMON_REQUEST_TIMEOUT" | "DAEMON_REQUEST_CANCELLED" | "DAEMON_CONNECTION_LOST" => {
                    !error.message.contains(BEFORE_SUBMISSION)
                }
                _ => error
                    .details
                    .as_ref()
                    .is_some_and(crate::protocol::server_outcome_unknown),
            };
            return Self::new(&error.code, error.message.clone(), outcome_unknown);
        }
        if let Some(error) = error.downcast_ref::<HostError>() {
            return Self::new(&error.code, error.message.clone(), false);
        }
        for cause in error.chain() {
            if let Some(channel) = cause.downcast_ref::<ChannelError>() {
                let code = if channel.close_code == 4002 {
                    "RELAY_SECURITY"
                } else {
                    "RELAY_RECORD"
                };
                return Self::new(
                    code,
                    format!("Relay security failure: {}", channel.reason),
                    false,
                );
            }
        }
        Self::new("CONNECTION_FAILED", format!("{error:#}"), false)
    }

    /// Original `getDaemonAuthFailureReason`: credential rejections from hello or the transport.
    pub(crate) fn is_auth_failure(&self) -> bool {
        matches!(
            self.code.as_str(),
            "AUTH_REQUIRED" | "AUTH_FAILED" | "authentication_failed"
        )
    }

    /// Failures reconnecting cannot fix (Flutter `PaseoClient.isTerminalFailure`).
    pub(crate) fn is_terminal(&self) -> bool {
        self.is_auth_failure()
            || matches!(
                self.code.as_str(),
                "DAEMON_IDENTITY_MISMATCH"
                    | "RELAY_SECURITY_DOWNGRADE"
                    | "PARENT_CLIENT_CLOSED"
                    | "RELAY_SECURITY"
            )
    }
}

pub(crate) fn not_connected() -> CoreError {
    CoreError::new("DAEMON_NOT_CONNECTED", "Daemon is not connected", false)
}

pub(crate) fn host_unknown(host_id: &str) -> CoreError {
    CoreError::new("HOST_UNKNOWN", format!("Host is not saved: {host_id}"), false)
}

pub(crate) fn invalid_request(message: impl Into<String>) -> CoreError {
    CoreError::new("INVALID_REQUEST", message, false)
}
