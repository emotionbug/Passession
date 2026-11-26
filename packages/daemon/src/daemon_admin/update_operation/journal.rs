use super::{Operation, now, validate_id};
use anyhow::{Context, Result, bail, ensure};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

pub(crate) struct Lease(File);

impl Lease {
    pub(crate) fn acquire(home: &Path, lane: &str) -> Result<Self> {
        let path = home.join(format!(".native-update-{lane}.lock"));
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        let file = options
            .open(&path)
            .context("Cannot open native update lease")?;
        ensure!(
            file.metadata()?.is_file()
                && !std::fs::symlink_metadata(&path)?.file_type().is_symlink(),
            "Native update lease is not a regular file"
        );
        file.try_lock()
            .context("Another native update executor or admission owns this home")?;
        Ok(Self(file))
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        // The OS also releases this lock on process death. Do not remove the
        // inode: a waiter could otherwise lock an unlinked, unrelated file.
        let _ = self.0.unlock();
    }
}

fn current_path(home: &Path) -> PathBuf {
    home.join("native-update-operation.json")
}

fn history_path(home: &Path, operation_id: &str) -> PathBuf {
    home.join("native-update-history")
        .join(format!("{operation_id}.json"))
}

async fn load(path: &Path, home: &Path) -> Result<Option<Operation>> {
    let Some(bytes) = crate::transport::storage::read_private(path).await? else {
        return Ok(None);
    };
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .context("Invalid native update journal; original file is retained")?;
    ensure!(
        value["schemaVersion"] == 2,
        "Unsupported native update journal schema"
    );
    let operation: Operation = serde_json::from_value(value)
        .context("Incomplete native update journal; original file is retained")?;
    validate_id(&operation.operation_id)?;
    ensure!(
        operation.home == tokio::fs::canonicalize(home).await?,
        "Native update journal belongs to a different home"
    );
    ensure!(
        operation.request.operation_id == operation.operation_id,
        "Native update request identity does not match its journal"
    );
    Ok(Some(operation))
}

pub(super) async fn current(home: &Path) -> Result<Option<Operation>> {
    load(&current_path(home), home).await
}

pub(super) async fn read(home: &Path, operation_id: &str) -> Result<Option<Operation>> {
    validate_id(operation_id)?;
    if let Some(operation) = current(home).await?
        && operation.operation_id == operation_id
    {
        return Ok(Some(operation));
    }
    let archived = load(&history_path(home, operation_id), home).await?;
    if let Some(operation) = &archived {
        ensure!(
            operation.operation_id == operation_id,
            "Archived update identity mismatch"
        );
    }
    Ok(archived)
}

pub(super) async fn save(operation: &mut Operation) -> Result<()> {
    operation.updated_at = now();
    let bytes = serde_json::to_vec(operation)?;
    ensure!(
        bytes.len() <= 1024 * 1024,
        "Native update journal exceeds the private-file limit"
    );
    crate::transport::storage::write_private(&current_path(&operation.home), &bytes).await
}

pub(super) async fn admit(operation: &mut Operation) -> Result<()> {
    if let Some(previous) = current(&operation.home).await? {
        if previous.operation_id == operation.operation_id {
            bail!("Native update operation was already admitted");
        }
        ensure!(
            previous.terminal(),
            "Another native update operation is still active"
        );
        let path = history_path(&operation.home, &previous.operation_id);
        let directory = path.parent().expect("history file has a parent");
        tokio::fs::create_dir_all(directory).await?;
        ensure!(
            !tokio::fs::symlink_metadata(directory)
                .await?
                .file_type()
                .is_symlink(),
            "Native update history directory must not be a symlink"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).await?;
        }
        // Archive before replacing current. During a crash window the current
        // record is authoritative; a subsequent admission writes the same bytes.
        let bytes = serde_json::to_vec(&previous)?;
        if let Some(existing) = crate::transport::storage::read_private(&path).await? {
            ensure!(
                existing == bytes,
                "Conflicting archived native update operation"
            );
        } else {
            crate::transport::storage::write_private(&path, &bytes).await?;
        }
    }
    save(operation).await
}

/// Migration temporarily uses the legacy drain's journal slot, without
/// claiming to be an installed update or replacing previous update status.
pub(super) async fn restore_migration(record: &Operation) -> Result<()> {
    let home = &record.home;
    match record.owner["previousOperationId"].as_str() {
        Some(id) => {
            validate_id(id)?;
            let archived = history_path(home, id);
            let previous = load(&archived, home)
                .await?
                .context("Archived update is missing")?;
            ensure!(
                previous.operation_id == id && previous.terminal(),
                "Invalid previous update"
            );
            let bytes = serde_json::to_vec(&previous)?;
            crate::transport::storage::write_private(&current_path(home), &bytes).await
        }
        None => {
            tokio::fs::remove_file(current_path(home)).await?;
            Ok(())
        }
    }
}

/// Store material that unfinished operations still need. A finished update
/// (committed, failed or rolled back) needs none of its staging, executor copy,
/// candidate or replaced runtime: the selection and running processes protect
/// the runtimes that run, and status reads only the journal.
pub(super) async fn retained(home: &Path) -> Result<(Vec<String>, Vec<PathBuf>)> {
    let mut operations = Vec::new();
    let mut runtimes = Vec::new();
    let mut records = Vec::new();
    records.extend(current(home).await?);
    let directory = home.join("native-update-history");
    match tokio::fs::read_dir(&directory).await {
        Ok(mut entries) => {
            while let Some(entry) = entries.next_entry().await? {
                if entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "json")
                {
                    records.extend(load(&entry.path(), home).await?);
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    for mut operation in records {
        // A late continuation receipt can finish a committed update; failing to
        // read it leaves the update unfinished and its material retained.
        let _ = super::continuation::observe(&mut operation).await;
        if operation.terminal() {
            continue;
        }
        operations.push(operation.operation_id);
        // A committed update is never rolled back, so its replaced runtime is
        // not recovery material; an uncommitted one may roll back to it.
        if operation.phase != super::Phase::Committed {
            runtimes.extend(operation.previous.into_iter().map(|value| value.root));
        }
        runtimes.extend(operation.candidate.into_iter().map(|value| value.root));
    }
    Ok((operations, runtimes))
}
