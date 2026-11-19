//! Same token exchange as expo-notifications `getExpoPushTokenAsync` (original
//! expo_push_client.dart). The daemon accepts Expo tokens, never the device's
//! FCM or APNs token.

use super::PushFailure;
use crate::{action::PlatformError, platform::Shared};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

pub(crate) const PROJECT_ID: &str = "0e7f65ce-0367-46c8-a238-2b65963d235a";
const INSTALLATION_KEY: &str = "mobile.expo.installationId";

pub(super) async fn issue(shared: &Arc<Shared>, device_token: &str) -> Result<String, PushFailure> {
    let installation = match shared.mobile.prefs.get_string(INSTALLATION_KEY) {
        Some(id) => id,
        None => {
            let id = uuid::Uuid::new_v4().hyphenated().to_string().to_lowercase();
            let prefs_shared = shared.clone();
            let saved_id = id.clone();
            tokio::task::spawn_blocking(move || {
                prefs_shared
                    .mobile
                    .prefs
                    .set_string(INSTALLATION_KEY, &saved_id)
            })
            .await
            .map_err(|e| PlatformError::native(e.to_string()))?
            .map_err(|_| {
                PlatformError::native("Could not save the push notification installation identity.")
            })?;
            id
        }
    };
    let environment = crate::native::push_environment()?;
    if environment != "development" && environment != "production" {
        let error = PlatformError::native("Cannot determine the signing profile APNs environment.");
        return Err(error.into());
    }
    let body = json!({
        "type": if cfg!(target_os = "ios") { "apns" } else { "fcm" },
        "deviceId": installation,
        "development": environment == "development",
        "appId": crate::paths::MOBILE_APP_ID,
        "deviceToken": device_token,
        "projectId": PROJECT_ID,
    });
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| PlatformError::native(e.to_string()))?;
    let response = client
        .post("https://exp.host/--/api/v2/push/getExpoPushToken")
        .json(&body)
        .send()
        .await
        .map_err(|error| {
            PushFailure::Transient(PlatformError::native(format!(
                "Expo push token issuance failed: {error}"
            )))
        })?;
    let status = response.status();
    if !status.is_success() {
        // Do not expose the response body: the upstream can echo device tokens.
        let error = PlatformError::native(format!(
            "Expo push token issuance failed (HTTP {}). Check the EAS project push credentials.",
            status.as_u16()
        ));
        return Err(if retryable_status(status) {
            PushFailure::Transient(error)
        } else {
            PushFailure::Permanent(error)
        });
    }
    let text = response
        .text()
        .await
        .map_err(|e| PushFailure::Transient(PlatformError::native(e.to_string())))?;
    let token = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| {
            v.get("data")?
                .get("expoPushToken")?
                .as_str()
                .map(str::to_owned)
        })
        .filter(|token| valid_token(token))
        .ok_or_else(|| PlatformError::native("Expo returned an invalid push token."))?;
    Ok(token)
}

fn retryable_status(status: reqwest::StatusCode) -> bool {
    status.is_server_error()
        || status == reqwest::StatusCode::REQUEST_TIMEOUT
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
}

/// `^(ExponentPushToken|ExpoPushToken)\[[^\]]+\]$`
fn valid_token(token: &str) -> bool {
    let rest = token
        .strip_prefix("ExponentPushToken[")
        .or_else(|| token.strip_prefix("ExpoPushToken["));
    rest.and_then(|r| r.strip_suffix(']'))
        .is_some_and(|inner| !inner.is_empty() && !inner.contains(']'))
}

#[cfg(test)]
mod tests {
    #[test]
    fn token_exchange_retries_service_failures_but_not_bad_credentials() {
        use reqwest::StatusCode;
        assert!(super::retryable_status(StatusCode::SERVICE_UNAVAILABLE));
        assert!(super::retryable_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(super::retryable_status(StatusCode::REQUEST_TIMEOUT));
        assert!(!super::retryable_status(StatusCode::UNAUTHORIZED));
        assert!(!super::retryable_status(StatusCode::BAD_REQUEST));
    }

    #[test]
    fn token_shape() {
        assert!(super::valid_token("ExponentPushToken[abc]"));
        assert!(super::valid_token("ExpoPushToken[x]"));
        assert!(!super::valid_token("ExpoPushToken[]"));
        assert!(!super::valid_token("ExpoPushToken[a]b]"));
    }
}
