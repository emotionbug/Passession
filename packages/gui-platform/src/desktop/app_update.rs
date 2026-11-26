//! Local APP update admission, authenticated GUI handoff and durable status.
use super::*;
use paseo_protocol::components::ComponentId;
use serde::Deserialize;
use std::collections::BTreeSet;
use std::{path::Path, process::Stdio};
use tokio::io::AsyncWriteExt;

const LIMIT: u64 = 64 * 1024;
pub(super) const OPERATION_NOT_FOUND: &str = "APP_UPDATE_OPERATION_NOT_FOUND";

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Origin {
    Local,
    Remote,
}

fn error(message: impl Into<String>) -> PlatformError {
    PlatformError::new("APP_UPDATE_FAILED", message)
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReleaseIdentity {
    version: String,
    manifest_sha512: String,
}
#[derive(Clone, Debug, PartialEq, serde::Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Submit {
    operation_id: String,
    expected_release: ReleaseIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    selected_components: Option<Vec<ComponentId>>,
}

impl Submit {
    fn parse(args: &Value, origin: Origin) -> Result<Self, PlatformError> {
        use base64::Engine as _;
        let request: Self =
            serde_json::from_value(args.clone()).map_err(|e| error(e.to_string()))?;
        validate_id(&request.operation_id)?;
        if origin == Origin::Remote && request.selected_components.is_none() {
            return Err(error(
                "Select desktop app components for the remote update.",
            ));
        }
        if let Some(selected) = &request.selected_components
            && (selected.is_empty()
                || selected.contains(&ComponentId::Daemon)
                || selected.iter().copied().collect::<BTreeSet<_>>().len() != selected.len())
        {
            return Err(error(
                "Select unique desktop app components, not the daemon.",
            ));
        }
        if request.expected_release.version.is_empty()
            || !base64::engine::general_purpose::STANDARD
                .decode(&request.expected_release.manifest_sha512)
                .is_ok_and(|d| d.len() == 64)
        {
            return Err(error("Invalid pinned APP release."));
        }
        Ok(request)
    }
}

fn operations_root() -> Result<PathBuf, PlatformError> {
    let store = component_store::Store::discover()
        .map_err(PlatformError::from)?
        .ok_or_else(|| {
            error("One-time packaged upgrade required: component inventory is missing.")
        })?;
    Ok(
        component_store::canonical_path(&crate::paths::app_paths().support_dir)?
            .join("desktop-app-update")
            .join(store.installation_id),
    )
}
fn validate_id(id: &str) -> Result<(), PlatformError> {
    let parsed = uuid::Uuid::parse_str(id).map_err(|_| error("Invalid operation UUID."))?;
    if parsed.is_nil() || parsed.hyphenated().to_string() != id {
        return Err(error("Operation UUID must be canonical."));
    }
    Ok(())
}

pub(crate) struct Operations {
    session: String,
    admission: tokio::sync::Mutex<()>,
}

impl Default for Operations {
    fn default() -> Self {
        Self {
            session: uuid::Uuid::new_v4().to_string(),
            admission: Default::default(),
        }
    }
}

pub async fn check_local(shared: &Arc<Shared>, args: &Value) -> Result<Value, PlatformError> {
    let channel = channel(shared, args).await;
    let mut result = match shared.desktop.updater.check_app_release(&channel).await {
        Ok(result) => result,
        Err(super::updater::DiscoveryError::RateLimited(retry_at)) => {
            return Ok(json!({
                "targetKind": "app",
                "updateAvailable": false,
                "latest": null,
                "checkError": super::updater::RATE_LIMIT_MESSAGE,
                "errorCode": super::updater::RATE_LIMIT_CODE,
                "retryAt": retry_at.to_rfc3339(),
            }));
        }
        Err(error) => return Err(error.into()),
    };
    result["targetKind"] = "app".into();
    if result["supported"] == true {
        let actual = observed_components().await?;
        if actual != result["components"] {
            result["supported"] = false.into();
            result["updateAvailable"] = false.into();
            result["availableComponents"] = json!([]);
            result["supportError"] = json!(
                "Running app components differ from the selected installation; complete recovery before updating."
            );
        }
        result["components"] = actual;
    }
    Ok(result)
}

pub(super) async fn install_local(
    shared: &Arc<Shared>,
    args: &Value,
    origin: Origin,
) -> Result<Value, PlatformError> {
    let request = Submit::parse(args, origin)?;
    let channel = channel(shared, &Value::Null).await;
    let root = operations_root()?;
    let updates = &shared.desktop.app_updates;
    let _gate = updates.admission.lock().await;
    private_dir(
        root.parent()
            .ok_or_else(|| error("Missing APP operation parent."))?,
    )
    .await?;
    private_dir(&root).await?;
    let directory = root.join(&request.operation_id);
    if directory.exists() {
        let existing = read_json(&directory.join("operation.json")).await?;
        if existing["request"] != json!(request) {
            return Err(error("Operation ID is already bound to another request."));
        }
        return updates.status(&root, &request.operation_id).await;
    }
    let mut entries = tokio::fs::read_dir(&root).await?;
    while let Some(entry) = entries.next_entry().await? {
        if !entry.file_type().await?.is_dir() {
            continue;
        }
        let id = entry.file_name().to_string_lossy().into_owned();
        if validate_id(&id).is_ok() {
            let operation = updates.status(&root, &id).await?;
            if operation["terminal"] != true || operation["recoveryRequired"] == true {
                return Err(error(format!(
                    "APP update {id} is active or requires recovery; inspect that operation instead."
                )));
            }
        }
    }
    if !helper_path()?.is_file() {
        return Err(error(
            "One-time packaged upgrade required: updater helper is missing.",
        ));
    }
    private_dir(&directory).await?;
    let operation = json!({"operationId":request.operation_id,"targetKind":"app","accepted":true,
        "phase":"preparation","terminal":false,"outcome":null,"candidate":null,"message":"Preparing pinned app update",
        "error":null,"recoveryRequired":false,"actionRequired":null,"request":request,
        "expectedRelease":request.expected_release,"selectedComponents":request.selected_components,
        "targetComponents":{},"observedComponents":null,"restartGroup":"none",
        "ownerSession":updates.session,"helperStarted":false});
    atomic_json(&directory.join("operation.json"), &operation).await?;
    let task_shared = shared.clone();
    shared.spawn(async move {
        if let Err(e) = execute(&task_shared, &channel, &directory, &request, origin).await {
            let _ = fail_operation(&directory, &e.message).await;
        }
    });
    Ok(public_operation(operation))
}

pub async fn status_local(shared: &Arc<Shared>, args: &Value) -> Result<Value, PlatformError> {
    let id = args["operationId"]
        .as_str()
        .ok_or_else(|| error("Missing operationId."))?;
    shared
        .desktop
        .app_updates
        .status(&operations_root()?, id)
        .await
}

impl Operations {
    async fn status(&self, root: &Path, id: &str) -> Result<Value, PlatformError> {
        validate_id(id)?;
        let directory = root.join(id);
        let path = directory.join("operation.json");
        let metadata = tokio::fs::symlink_metadata(&path).await.map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                PlatformError::new(OPERATION_NOT_FOUND, "APP update operation not found.")
            } else {
                err.into()
            }
        })?;
        let mut operation = read_json_file(&path, metadata).await?;
        if operation["operationId"] != id {
            return Err(error("APP operation ID mismatch."));
        }
        if operation["helperStarted"] == true && operation["terminal"] != true {
            if let Ok(helper) = read_json(&directory.join("status.json")).await {
                if helper["phase"] != "committed"
                    && helper["phase"] != "failed"
                    && helper["phase"] != "rolledBack"
                    && operation["ownerSession"] != self.session
                {
                    let pid = helper["helperProcess"]["pid"]
                        .as_u64()
                        .and_then(|p| u32::try_from(p).ok());
                    let alive = match pid {
                        Some(pid) => process_identity(pid)
                            .await
                            .is_ok_and(|identity| identity == helper["helperProcess"]),
                        None => false,
                    };
                    if !alive {
                        operation["phase"] = json!("failed");
                        operation["terminal"] = json!(true);
                        operation["outcome"] = json!("failed");
                        operation["error"] = json!(
                            "APP updater was interrupted. Native installation state requires recovery; this operation will not be replayed."
                        );
                        operation["recoveryRequired"] = json!(true);
                        atomic_json(&directory.join("operation.json"), &operation).await?;
                        return Ok(public_operation(operation));
                    }
                }
                merge_helper(&mut operation, &helper)?;
            } else if operation["ownerSession"] != self.session {
                operation["phase"] = json!("failed");
                operation["terminal"] = json!(true);
                operation["outcome"] = json!("failed");
                operation["error"] = json!(
                    "APP helper handoff was interrupted before its journal appeared. Inspect retained recovery files."
                );
                operation["recoveryRequired"] = json!(true);
                atomic_json(&directory.join("operation.json"), &operation).await?;
            }
        } else if operation["terminal"] != true && operation["ownerSession"] != self.session {
            operation["phase"] = json!("failed");
            operation["terminal"] = json!(true);
            operation["outcome"] = json!("failed");
            operation["error"] = json!(
                "APP preparation was interrupted before helper handoff. This operation will not be replayed."
            );
            atomic_json(&directory.join("operation.json"), &operation).await?;
        }
        Ok(public_operation(operation))
    }
}

fn helper_path() -> Result<PathBuf, PlatformError> {
    if let Some(store) = super::component_store::Store::discover().map_err(PlatformError::from)? {
        let selected = store.selected().map_err(PlatformError::from)?;
        return store
            .image(&selected[&ComponentId::UpdateHelper])
            .map_err(PlatformError::from);
    }
    Ok(std::env::current_exe()?
        .parent()
        .ok_or_else(|| error("GUI directory is missing."))?
        .join(if cfg!(windows) {
            "passession-update-helper.exe"
        } else {
            "passession-update-helper"
        }))
}
fn public_operation(mut operation: Value) -> Value {
    if let Some(map) = operation.as_object_mut() {
        for key in ["request", "ownerSession", "helperStarted"] {
            map.remove(key);
        }
    }
    // The helper binds the complete immutable store selection. Only app
    // components belong to the public operation result.
    for key in ["targetComponents", "observedComponents"] {
        if let Some(components) = operation[key].as_object_mut() {
            components.remove("daemon");
        }
    }
    operation
}
fn merge_helper(operation: &mut Value, helper: &Value) -> Result<(), PlatformError> {
    if helper["schemaVersion"] != 2
        || helper["operationId"] != operation["operationId"]
        || helper["restartGroup"] != operation["restartGroup"]
        || helper["expectedComponents"] != operation["targetComponents"]
    {
        return Err(error(
            "Helper journal identity differs from admitted APP operation.",
        ));
    }
    let phase = helper["phase"]
        .as_str()
        .ok_or_else(|| error("Missing helper phase."))?;
    let outcome = match phase {
        "committed" => Some("committed"),
        "failed" => Some("failed"),
        "rolledBack" => Some("rolled_back"),
        "preparation" | "waitingForParent" | "activating" | "rollingBack" | "starting"
        | "checking" => None,
        _ => return Err(error("Unknown helper journal phase.")),
    };
    if phase == "committed" {
        if helper["observedComponents"] != operation["targetComponents"] {
            return Err(error(
                "Committed component update lacks independently verified expected identities.",
            ));
        }
        operation["observedComponents"] = helper["observedComponents"].clone();
    }
    operation["phase"] = json!(phase);
    operation["outcome"] = json!(outcome);
    operation["terminal"] = json!(outcome.is_some());
    operation["message"] = helper["message"].clone();
    operation["recoveryRequired"] = helper["recoveryRequired"].clone();
    operation["actionRequired"] = if phase == "waitingForAuthorization" {
        json!("osAuthorization")
    } else {
        Value::Null
    };
    if matches!(outcome, Some("failed" | "rolled_back")) {
        operation["error"] = helper["message"].clone();
    }
    Ok(())
}
async fn fail_operation(directory: &Path, message: &str) -> Result<(), PlatformError> {
    let mut operation = read_json(&directory.join("operation.json")).await?;
    if operation["helperStarted"] == true {
        // The helper remains the sole activation authority after handoff.
        // Keep observing its durable result; a GUI monitor error cannot turn a
        // live rollback/commit into an invented terminal failure.
        operation["error"] = json!(message);
        operation["message"] = json!(message);
        operation["recoveryRequired"] = json!(true);
        if let Ok(status) = read_json(&directory.join("status.json")).await {
            merge_helper(&mut operation, &status)?;
            if operation["terminal"] != true {
                operation["recoveryRequired"] = json!(true);
            }
        }
        return atomic_json(&directory.join("operation.json"), &operation).await;
    }
    operation["phase"] = json!("failed");
    operation["terminal"] = json!(true);
    operation["outcome"] = json!("failed");
    operation["error"] = json!(message);
    operation["message"] = json!(message);
    atomic_json(&directory.join("operation.json"), &operation).await
}

// Called only after the launched child has exited, before any runtime handoff.
async fn retire_pre_handoff_helper(
    directory: &Path,
    helper_identity: &Value,
) -> Result<(), PlatformError> {
    let mut operation = read_json(&directory.join("operation.json")).await?;
    let path = directory.join("status.json");
    let safe = match tokio::fs::symlink_metadata(&path).await {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(e) => return Err(e.into()),
        Ok(_) => {
            let status = read_json(&path).await?;
            if status["helperProcess"] != *helper_identity {
                return Err(error("Exited helper process identity mismatch"));
            }
            merge_helper(&mut operation, &status)?;
            operation["restartGroup"] != "none"
                && matches!(
                    status["phase"].as_str(),
                    Some("preparation" | "waitingForParent")
                )
                && status["recoveryRequired"] != true
        }
    };
    if safe {
        operation["helperStarted"] = json!(false);
        atomic_json(&directory.join("operation.json"), &operation).await?;
    }
    Ok(())
}

async fn execute(
    shared: &Arc<Shared>,
    channel: &str,
    directory: &Path,
    request: &Submit,
    origin: Origin,
) -> Result<(), PlatformError> {
    let expected =
        serde_json::to_value(&request.expected_release).map_err(|e| error(e.to_string()))?;
    let selected = request
        .selected_components
        .as_ref()
        .map(|ids| ids.iter().copied().collect::<BTreeSet<_>>());
    let helper_request = shared
        .desktop
        .updater
        .prepare_helper(super::updater::HelperPreparation {
            channel,
            operation: &request.operation_id,
            directory,
            expected: &expected,
            selected: selected.as_ref(),
        })
        .await?;
    let group = helper_request["restartGroup"]
        .as_str()
        .ok_or_else(|| error("Missing restart group"))?
        .to_owned();
    let target: super::component_store::Installed =
        serde_json::from_value(helper_request["targetComponents"].clone())
            .map_err(|e| error(e.to_string()))?;
    let inventory: paseo_protocol::components::ReleaseInventory =
        super::component_store::read_json(
            Path::new(
                helper_request["inventoryPath"]
                    .as_str()
                    .ok_or_else(|| error("Missing inventory path"))?,
            ),
            true,
        )
        .map_err(PlatformError::from)?;
    let mut operation = read_json(&directory.join("operation.json")).await?;
    operation["candidate"] = json!({"version":inventory.version,"sourceSha":inventory.source_sha});
    operation["targetComponents"] = json!(super::component_store::identities(&target));
    operation["restartGroup"] = json!(group);
    atomic_json(&directory.join("operation.json"), &operation).await?;
    let helper = directory.join(if cfg!(windows) {
        "passession-update-helper.exe"
    } else {
        "passession-update-helper"
    });
    super::updater::copy_private(&helper_path()?, &helper, true).await?;
    let request_path = directory.join("request.json");
    atomic_json(&request_path, &helper_request).await?;
    operation["helperStarted"] = json!(true);
    atomic_json(&directory.join("operation.json"), &operation).await?;
    let mut command = tokio::process::Command::new(&helper);
    command
        .arg("--request")
        .arg(&request_path)
        .env_remove("LD_LIBRARY_PATH")
        .env_remove("LD_PRELOAD")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(false);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            operation["helperStarted"] = json!(false);
            atomic_json(&directory.join("operation.json"), &operation).await?;
            return Err(e.into());
        }
    };
    let helper_identity = process_identity(
        child
            .id()
            .ok_or_else(|| error("Missing spawned helper PID"))?,
    )
    .await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    let ready = loop {
        if let Ok(status) = read_json(&directory.join("status.json")).await {
            if status["helperProcess"] != helper_identity {
                return Err(error(
                    "Helper readiness process identity differs from the launched child",
                ));
            }
            merge_helper(&mut operation, &status)?;
            if operation["terminal"] == true {
                atomic_json(&directory.join("operation.json"), &operation).await?;
                return if operation["outcome"] == "committed" {
                    Ok(())
                } else {
                    Err(error(
                        status["message"]
                            .as_str()
                            .unwrap_or("Component helper failed"),
                    ))
                };
            }
            if status["phase"] == "waitingForParent" && status["ready"] == true {
                break status;
            }
        }
        if child.try_wait()?.is_some() {
            retire_pre_handoff_helper(directory, &helper_identity).await?;
            return Err(error(
                "Component helper exited before authenticated readiness",
            ));
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(error(
                "Component helper readiness timed out; existing selection retained",
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    if group == "none" {
        return Err(error(
            "No-restart helper unexpectedly requested runtime handoff",
        ));
    }
    let can_quit = match origin {
        Origin::Local => guarded_consent(shared, QuitReason::AppUpdate).await,
        Origin::Remote => shared.desktop.quit.lock().unwrap().reserve_remote_update(),
    };
    if !can_quit {
        // A ready helper cannot activate until the desktop writes its handoff.
        child.kill().await?;
        retire_pre_handoff_helper(directory, &helper_identity).await?;
        return Err(error(match origin {
            Origin::Local => "Unsaved local documents block application restart.",
            Origin::Remote => {
                "The desktop app is already closing; application restart was not handed off."
            }
        }));
    }
    let handoff =
        json!({"schemaVersion":2,"operationId":request.operation_id,"nonce":ready["nonce"]});
    if let Err(write_error) = atomic_json(&directory.join("handoff.json"), &handoff).await
        && read_json(&directory.join("handoff.json"))
            .await
            .ok()
            .as_ref()
            != Some(&handoff)
    {
        let mut quit = shared.desktop.quit.lock().unwrap();
        match origin {
            Origin::Local => {
                if !quit.prepared {
                    quit.quitting = false;
                }
                post_event(PlatformEvent::QuitCancelled);
            }
            Origin::Remote => quit.remote_update_handoff = false,
        }
        return Err(write_error);
    }
    finish_quit(shared).await;
    Ok(())
}

pub(super) async fn private_dir(path: &Path) -> Result<(), PlatformError> {
    if !path.exists() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = std::fs::DirBuilder::new();
            builder.mode(0o700);
            builder.create(path)?;
        }
        #[cfg(windows)]
        {
            std::fs::create_dir(path)?;
        }
    }
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err(error("APP update directory must be a real directory."));
    }
    if !super::component_store::same_path(
        &super::component_store::canonical_path(path).map_err(PlatformError::from)?,
        path,
    ) {
        return Err(error(
            "APP update directory has a symlink or noncanonical ancestor.",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
            return Err(error(
                "APP update directory must be private and owned by this user.",
            ));
        }
    }
    #[cfg(windows)]
    {
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || {
            paseo_client::windows_security::restrict_directory_to_current_user(&path)
        })
        .await
        .map_err(|e| error(e.to_string()))?
        .map_err(|e| error(format!("{e:#}")))?;
    }
    Ok(())
}
pub(super) async fn read_json(path: &Path) -> Result<Value, PlatformError> {
    let meta = tokio::fs::symlink_metadata(path).await?;
    read_json_file(path, meta).await
}

async fn read_json_file(path: &Path, meta: std::fs::Metadata) -> Result<Value, PlatformError> {
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > LIMIT {
        return Err(error("Invalid APP update journal file."));
    }
    if !super::component_store::same_path(
        &super::component_store::canonical_path(path).map_err(PlatformError::from)?,
        path,
    ) {
        return Err(error("APP journal has a symlink or noncanonical ancestor."));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
            return Err(error("APP journal is not private to the current user."));
        }
    }
    let bytes = tokio::fs::read(path).await?;
    super::component_store::strict_json(&bytes).map_err(PlatformError::from)
}
/// Creates a new updater file with its final access policy before writing bytes.
pub(super) async fn create_private_file(
    path: &Path,
    executable: bool,
) -> std::io::Result<tokio::fs::File> {
    #[cfg(unix)]
    {
        tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(if executable { 0o700 } else { 0o600 })
            .open(path)
            .await
    }
    #[cfg(windows)]
    {
        let _ = executable;
        let path = path.to_owned();
        let file = tokio::task::spawn_blocking(move || {
            paseo_client::windows_security::create_private_file(&path)
        })
        .await
        .map_err(std::io::Error::other)??;
        Ok(tokio::fs::File::from_std(file))
    }
}

pub(super) async fn atomic_json(path: &Path, value: &Value) -> Result<(), PlatformError> {
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4().simple()));
    let mut file = create_private_file(&temporary, false).await?;
    file.write_all(&serde_json::to_vec(value).map_err(|e| error(e.to_string()))?)
        .await?;
    file.sync_all().await?;
    drop(file);
    tokio::fs::rename(&temporary, path).await?;
    #[cfg(unix)]
    std::fs::File::open(
        path.parent()
            .ok_or_else(|| error("Missing journal directory."))?,
    )?
    .sync_all()?;
    Ok(())
}

async fn process_identity(pid: u32) -> Result<Value, PlatformError> {
    tokio::task::spawn_blocking(move || component_store::process_identity(pid))
        .await
        .map_err(|e| error(e.to_string()))?
        .map_err(PlatformError::from)
}

/// A trial app proves its own health before doing anything to the daemon home;
/// a failed trial can restore the prior app without changing that home.
pub(super) async fn recover_health(shared: &Arc<Shared>) -> Result<bool, PlatformError> {
    let Some(directory) = std::env::var_os("PASEO_UPDATE_OPERATION_DIRECTORY") else {
        return Ok(true);
    };
    let directory = PathBuf::from(directory);
    let id = std::env::var("PASEO_UPDATE_OPERATION_ID")
        .map_err(|_| error("Missing recovery operation identity"))?;
    validate_id(&id)?;
    let nonce = std::env::var("PASEO_UPDATE_NONCE").map_err(|_| error("Missing recovery nonce"))?;
    let store =
        component_store::Store::discover()?.ok_or_else(|| error("Component store disappeared"))?;
    let owner = read_json(&store.directory.join("lease-owner.json")).await?;
    if owner["schemaVersion"] != 1
        || owner["operationId"] != id
        || owner["nonce"] != nonce
        || owner["operationDirectory"] != json!(directory)
    {
        return Err(error(
            "Component recovery is not bound to the component-store helper.",
        ));
    }
    let request = read_json(&directory.join("request.json")).await?;
    if request["schemaVersion"] != 2
        || request["operationId"] != id
        || request["operationDirectory"] != json!(directory)
        || request["storeDirectory"] != json!(store.directory)
        || request["installationId"] != store.installation_id
        || request["restartGroup"] != "gui"
    {
        return Err(error("Component recovery request identity differs."));
    }
    let helper = &owner["helperProcess"];
    let helper_pid = helper["pid"]
        .as_u64()
        .and_then(|p| u32::try_from(p).ok())
        .ok_or_else(|| error("Missing helper PID"))?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(240);
    let mut reported = None;
    loop {
        let status = read_json(&directory.join("status.json")).await?;
        if status["schemaVersion"] != 2
            || status["operationId"] != id
            || status["nonce"] != nonce
            || status["helperProcess"] != *helper
        {
            return Err(error("Component health transaction authentication failed"));
        }
        let phase = status["phase"].as_str().unwrap_or("");
        if matches!(phase, "committed" | "rolledBack") {
            return Ok(true);
        }
        if phase == "failed" {
            return Ok(false);
        }
        if process_identity(helper_pid).await? != *helper {
            return Err(error(
                "Component helper process identity changed; recovery is required",
            ));
        }
        if let Ok(stop) = read_json(&directory.join("stop.json")).await
            && stop["schemaVersion"] == 2
            && stop["operationId"] == id
            && stop["nonce"] == nonce
            && stop["restartGroup"] == "gui"
        {
            if !guarded_consent(shared, QuitReason::AppUpdate).await {
                return Err(error(
                    "Unsaved documents prevented rollback; recovery files retained",
                ));
            }
            atomic_json(
                &directory.join("stopped.json"),
                &json!({
                    "schemaVersion":2,"operationId":id,"nonce":nonce
                }),
            )
            .await?;
            finish_quit(shared).await;
            return Ok(false);
        }
        let selection = store.selection()?;
        let health = match phase {
            "checking" => selection
                .trial
                .as_ref()
                .filter(|trial| trial.operation_id == id)
                .map(|trial| ("health.json", &trial.components)),
            "rollingBack" if selection.trial.is_none() => {
                Some(("rollback-health.json", &selection.active))
            }
            _ => None,
        };
        if let Some((name, expected)) = health
            && reported != Some(name)
        {
            let observed = observed_components().await?;
            if observed == json!(component_store::app_identities(expected)) {
                atomic_json(&directory.join(name), &json!({
                    "schemaVersion":2,"operationId":id,"nonce":nonce,
                    "components":observed,"guiProcess":process_identity(std::process::id()).await?
                })).await?;
                reported = Some(name);
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(error(
                "Component health deadline expired; retained journal requires recovery",
            ));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn observed_components() -> Result<Value, PlatformError> {
    let tailnet = paseo_tailnet::native_identity().map_err(PlatformError::from)?;
    let mut command = tokio::process::Command::new(helper_path()?);
    command
        .arg("--identity")
        .env_remove("LD_LIBRARY_PATH")
        .env_remove("LD_PRELOAD")
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    let output = tokio::time::timeout(Duration::from_secs(10), command.output())
        .await
        .map_err(|_| error("Helper identity query timed out"))??;
    if !output.status.success() || output.stdout.len() > LIMIT as usize {
        return Err(error("Selected helper identity query failed"));
    }
    let helper = component_store::strict_json(&output.stdout).map_err(PlatformError::from)?;
    if helper["schemaVersion"] != 1 || helper["id"] != "update-helper" {
        return Err(error("Selected helper identity response differs"));
    }
    Ok(
        json!({"app":component_store::app_identity(),"tailnet":tailnet,"update-helper":helper["identity"]}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn exited_helper_releases_only_proven_pre_handoff_authority() {
        let identity = json!({"pid":123,"birthToken":"child"});
        for (group, phase, recovery, safe) in [
            ("gui", None, false, true),
            ("none", None, false, true),
            ("gui", Some("preparation"), false, true),
            ("gui", Some("waitingForParent"), false, true),
            ("none", Some("preparation"), false, false),
            ("gui", Some("activating"), false, false),
            ("gui", Some("checking"), false, false),
            ("gui", Some("preparation"), true, false),
        ] {
            let temporary = tempfile::tempdir().unwrap();
            let directory = std::fs::canonicalize(temporary.path())
                .unwrap()
                .join("operation");
            private_dir(&directory).await.unwrap();
            let operation = json!({"operationId":"one","restartGroup":group,
                "targetComponents":{"app":{"version":"2"}},"helperStarted":true,
                "phase":"preparation","terminal":false,"recoveryRequired":false});
            atomic_json(&directory.join("operation.json"), &operation)
                .await
                .unwrap();
            if let Some(phase) = phase {
                atomic_json(
                    &directory.join("status.json"),
                    &json!({
                        "schemaVersion":2,"operationId":"one","restartGroup":group,
                        "expectedComponents":operation["targetComponents"],
                        "helperProcess":identity,"phase":phase,"recoveryRequired":recovery
                    }),
                )
                .await
                .unwrap();
            }
            retire_pre_handoff_helper(&directory, &identity)
                .await
                .unwrap();
            assert_eq!(
                read_json(&directory.join("operation.json")).await.unwrap()["helperStarted"],
                !safe
            );
            fail_operation(&directory, "Child exited").await.unwrap();
            let failed = read_json(&directory.join("operation.json")).await.unwrap();
            assert_eq!(failed["terminal"], safe, "{group} {phase:?}");
            assert_eq!(failed["recoveryRequired"], !safe, "{group} {phase:?}");
            if safe {
                assert_eq!(failed["outcome"], "failed");
            } else {
                assert_ne!(failed["outcome"], "committed");
                assert_ne!(failed["outcome"], "rolled_back");
            }
        }
    }
    #[test]
    fn request_is_pinned_and_has_no_remote_paths() {
        use base64::Engine as _;
        let mut args = json!({"operationId":"0f8b6c5e-2a71-4c5b-9a43-7f2a1d0c9e11","expectedRelease":{"version":"2.0.0","manifestSha512":base64::engine::general_purpose::STANDARD.encode([0u8;64])}});
        assert!(Submit::parse(&args, Origin::Local).is_ok());
        assert!(Submit::parse(&args, Origin::Remote).is_err());
        args["selectedComponents"] = Value::Null;
        assert!(Submit::parse(&args, Origin::Remote).is_err());
        args["selectedComponents"] = json!(["app"]);
        assert!(Submit::parse(&args, Origin::Remote).is_ok());
        args["force"] = json!(true);
        assert!(Submit::parse(&args, Origin::Remote).is_err());
        args.as_object_mut().unwrap().remove("force");
        args["path"] = json!("/tmp/installer");
        assert!(Submit::parse(&args, Origin::Remote).is_err());
        args.as_object_mut().unwrap().remove("path");
        args["expectedRelease"]["manifestSha512"] = json!("invalid");
        assert!(Submit::parse(&args, Origin::Remote).is_err());
    }
    #[test]
    fn selected_components_are_part_of_idempotent_request() {
        use base64::Engine as _;
        let mut args = json!({"operationId":"0f8b6c5e-2a71-4c5b-9a43-7f2a1d0c9e11","expectedRelease":{"version":"2.0.0","manifestSha512":base64::engine::general_purpose::STANDARD.encode([0u8;64])},"selectedComponents":["app"]});
        let app = Submit::parse(&args, Origin::Local).unwrap();
        args["selectedComponents"] = json!(["update-helper"]);
        assert_ne!(app, Submit::parse(&args, Origin::Local).unwrap());
        args["selectedComponents"] = json!(["daemon"]);
        assert!(Submit::parse(&args, Origin::Remote).is_err());
        args["selectedComponents"] = json!(["app", "app"]);
        assert!(Submit::parse(&args, Origin::Remote).is_err());
        args["selectedComponents"] = json!([]);
        assert!(Submit::parse(&args, Origin::Remote).is_err());
    }
    #[test]
    fn helper_commit_requires_same_operation_and_observed_components() {
        let mut operation = json!({"operationId":"one","restartGroup":"gui","targetComponents":{"app":{"version":"2"}}});
        let mut helper = json!({"schemaVersion":2,"operationId":"one","restartGroup":"gui","expectedComponents":operation["targetComponents"],"phase":"checking"});
        merge_helper(&mut operation, &helper).unwrap();
        assert_eq!(operation["terminal"], false);
        helper["operationId"] = json!("other");
        assert!(merge_helper(&mut operation, &helper).is_err());
        helper["operationId"] = json!("one");
        helper["phase"] = json!("committed");
        assert!(merge_helper(&mut operation, &helper).is_err());
        helper["observedComponents"] = helper["expectedComponents"].clone();
        merge_helper(&mut operation, &helper).unwrap();
        assert_eq!(operation["outcome"], "committed");
        assert_eq!(operation["terminal"], true);
    }
    #[tokio::test]
    async fn private_journal_preserves_operation_identity() {
        let temporary = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temporary.path())
            .unwrap()
            .join("operation");
        private_dir(&root).await.unwrap();
        let path = root.join("operation.json");
        atomic_json(&path, &json!({"operationId":"one"}))
            .await
            .unwrap();
        assert_eq!(read_json(&path).await.unwrap()["operationId"], "one");
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(std::fs::metadata(path).unwrap().mode() & 0o077, 0);
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_update_journals_are_private_across_handoff() {
        let temporary = tempfile::tempdir().unwrap();
        let support = component_store::canonical_path(temporary.path()).unwrap();
        let base = support.join("desktop-app-update");
        let installation = base.join("this-installation");
        let id = "0f8b6c5e-2a71-4c5b-9a43-7f2a1d0c9e11";
        let directory = installation.join(id);
        for path in [&base, &installation, &directory] {
            private_dir(path).await.unwrap();
            component_store::private(path).unwrap();
        }

        let operation = directory.join("operation.json");
        let handoff = directory.join("handoff.json");
        let preparation = json!({"operationId":id,"helperStarted":false});
        atomic_json(&operation, &preparation).await.unwrap();
        assert_eq!(read_json(&operation).await.unwrap(), preparation);
        let read_private = |path: &Path| component_store::read_json::<Value>(path, true).unwrap();
        assert_eq!(read_private(&operation), preparation);
        for (name, value) in [
            (
                "probe-tailnet.json",
                json!({"component":{"id":"tailnet"},"root":directory.join("validate-digest")}),
            ),
            (
                "request.json",
                json!({"schemaVersion":2,"operationId":id,"restartGroup":"gui"}),
            ),
        ] {
            let path = directory.join(name);
            atomic_json(&path, &value).await.unwrap();
            assert_eq!(read_private(&path), value, "{name}");
        }
        assert!(!handoff.exists());

        let started = json!({"operationId":id,"helperStarted":true});
        atomic_json(&operation, &started).await.unwrap();
        assert_eq!(read_private(&operation), started);
        let authorized = json!({"schemaVersion":2,"operationId":id,"nonce":"fixture"});
        atomic_json(&handoff, &authorized).await.unwrap();
        assert_eq!(read_private(&handoff), authorized);
        for name in ["health.json", "rollback-health.json", "stopped.json"] {
            let path = directory.join(name);
            atomic_json(&path, &authorized).await.unwrap();
            assert_eq!(read_private(&path), authorized, "{name}");
        }
    }

    #[tokio::test]
    async fn interrupted_preparation_is_failed_once_and_remains_installation_bound() {
        let temporary = tempfile::tempdir().unwrap();
        let support = std::fs::canonicalize(temporary.path()).unwrap();
        let base = support.join("desktop-app-update");
        private_dir(&base).await.unwrap();
        let installation = base.join("this-installation");
        private_dir(&installation).await.unwrap();
        let id = "0f8b6c5e-2a71-4c5b-9a43-7f2a1d0c9e11";
        let directory = installation.join(id);
        private_dir(&directory).await.unwrap();
        atomic_json(
            &directory.join("operation.json"),
            &json!({
                "operationId":id,"targetKind":"app","accepted":true,
                "ownerSession":"old-session","helperStarted":false,"phase":"preparation",
                "terminal":false,"outcome":null,"request":{}
            }),
        )
        .await
        .unwrap();
        let updates = Operations::default();
        let first = updates.status(&installation, id).await.unwrap();
        assert_eq!(first["phase"], "failed");
        assert_eq!(first["terminal"], true);
        assert!(first.get("ownerSession").is_none());
        assert!(first.get("request").is_none());
        assert_eq!(updates.status(&installation, id).await.unwrap(), first);
        let missing = updates
            .status(&base.join("other-installation"), id)
            .await
            .unwrap_err();
        assert_eq!(missing.code, OPERATION_NOT_FOUND);
        assert!(!directory.join("request.json").exists());
    }

    #[test]
    fn activation_failure_does_not_claim_rollback() {
        let mut operation = json!({"operationId":"one","restartGroup":"gui","targetComponents":{"app":{"version":"2"}}});
        let mut helper = json!({"schemaVersion":2,"operationId":"one","restartGroup":"gui",
            "expectedComponents":operation["targetComponents"],"phase":"activating","message":"Activating","recoveryRequired":false});
        merge_helper(&mut operation, &helper).unwrap();
        assert!(operation["actionRequired"].is_null());
        assert_eq!(operation["terminal"], false);
        helper["phase"] = json!("failed");
        helper["recoveryRequired"] = json!(true);
        merge_helper(&mut operation, &helper).unwrap();
        assert_eq!(operation["outcome"], "failed");
        assert_eq!(operation["recoveryRequired"], true);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn private_journal_rejects_symlink_and_world_readable_inputs() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let temporary = tempfile::tempdir().unwrap();
        let home = std::fs::canonicalize(temporary.path()).unwrap();
        let file = home.join("journal.json");
        atomic_json(&file, &json!({"operationId":"one"}))
            .await
            .unwrap();
        let link = home.join("link.json");
        symlink(&file, &link).unwrap();
        assert!(read_json(&link).await.is_err());
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_json(&file).await.is_err());
    }
}
