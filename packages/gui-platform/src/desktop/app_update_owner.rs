use super::{Shared, app_update, daemon_service};
use paseo_client::desktop_app_bridge::{Method, Owner, Refusal};
use serde_json::Value;
use std::{io, path::Path, sync::Arc, time::Duration};

const CHECK_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) fn start(shared: &Arc<Shared>) {
    let task_shared = shared.clone();
    shared.spawn(async move {
        let Ok(home) = daemon_service::home() else {
            return;
        };
        let result = tokio::select! {
            biased;
            _ = task_shared.desktop.app_update_bridge_stop.notified() => return,
            result = serve(&task_shared, &home) => result,
        };
        if result.is_err() {
            task_shared
                .desktop
                .log("The local desktop update bridge could not run.");
        }
    });
}

async fn serve(shared: &Arc<Shared>, home: &Path) -> anyhow::Result<()> {
    // The daemon or an explicit desktop Start action owns home creation.
    loop {
        match tokio::fs::symlink_metadata(home).await {
            Ok(_) => break,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
    let owner = Owner::bind(home).await?;
    let shared = shared.clone();
    owner
        .serve(move |method, args| {
            let shared = shared.clone();
            async move { dispatch(&shared, method, &args).await }
        })
        .await
}

fn valid_args(method: Method, args: &Value) -> bool {
    let Some(fields) = args.as_object() else {
        return false;
    };
    match method {
        Method::Check => fields.is_empty(),
        Method::Submit => true, // Parsed by the same pinned admission path as local settings.
        Method::Status => {
            fields.len() == 1 && fields.get("operationId").is_some_and(Value::is_string)
        }
    }
}

async fn dispatch(shared: &Arc<Shared>, method: Method, args: &Value) -> Result<Value, Refusal> {
    if !valid_args(method, args) {
        return Err(Refusal::Failed);
    }
    if method != Method::Status && shared.desktop.quitting() {
        return Err(match method {
            Method::Submit => Refusal::Failed,
            _ => Refusal::Unavailable,
        });
    }
    let result = match method {
        Method::Check => tokio::time::timeout(CHECK_TIMEOUT, app_update::check_local(shared, args))
            .await
            .map_err(|_| Refusal::Failed)?,
        Method::Submit => app_update::install_local(shared, args, app_update::Origin::Remote).await,
        Method::Status => app_update::status_local(shared, args).await,
    };
    result.map_err(|error| {
        // Local errors may describe private journal files or process identities.
        if method == Method::Status && error.code == app_update::OPERATION_NOT_FOUND {
            Refusal::OperationNotFound
        } else {
            Refusal::Failed
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn remote_check_and_status_do_not_accept_local_overrides() {
        assert!(valid_args(Method::Check, &json!({})));
        assert!(!valid_args(Method::Check, &Value::Null));
        assert!(!valid_args(
            Method::Check,
            &json!({"releaseChannel":"next"})
        ));
        assert!(valid_args(
            Method::Status,
            &json!({"operationId":"operation"})
        ));
        assert!(!valid_args(Method::Status, &json!({"operationId":null})));
        assert!(!valid_args(
            Method::Status,
            &json!({"operationId":"operation","path":"/foo"})
        ));
    }
}
