//! Cargo outputs are shared; mutable runtime resources and bundles belong to a checkout.
//! Only this module's marked generations are eligible for explicit housekeeping.
use crate::common::{absolute, cache, root};
use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use std::{
    env,
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    process::Command,
    sync::LazyLock,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use walkdir::WalkDir;

const MARKER: &str = "paseo-build-cache-v1\n";
const MIN_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const GENERATION_SECONDS: u64 = 30 * 24 * 60 * 60;
const DEFAULT_MAX_BYTES: u64 = 20 * 1024 * 1024 * 1024;
static TARGET: LazyLock<Result<PathBuf, String>> =
    LazyLock::new(|| resolve_target().map_err(|error| format!("{error:#}")));

fn managed_root() -> Result<PathBuf> {
    Ok(cache()?.join("paseo-build/v1"))
}

fn repository_key() -> Result<String> {
    // Source archives (notably container builds) have no Git common directory.
    // Their canonical source root is a separate, checkout-local cache identity.
    let source = root()?;
    let output = Command::new("git")
        .current_dir(&source)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output();
    let directory = match output {
        Ok(output) if output.status.success() => {
            fs::canonicalize(String::from_utf8(output.stdout)?.trim())?
        }
        _ => fs::canonicalize(source)?,
    };
    Ok(hex::encode(Sha256::digest(directory.as_os_str().as_encoded_bytes())))
}

fn generation() -> Result<String> {
    Ok(format!("generation-{}", SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() / GENERATION_SECONDS))
}

fn resolve_target() -> Result<PathBuf> {
    if let Some(target) = env::var_os("CARGO_TARGET_DIR") {
        ensure!(!target.is_empty(), "CARGO_TARGET_DIR must not be empty");
        absolute(target)
    } else {
        Ok(managed_root()?.join(repository_key()?).join(generation()?).join("target"))
    }
}

pub fn target_root() -> Result<PathBuf> {
    TARGET.as_ref().cloned().map_err(|error| anyhow::anyhow!("{error}"))
}

/// Pin checkout staging while a managed development process uses its resources.
pub fn runtime_lease() -> Result<File> {
    let lease = lock(&root()?.join(".dev/run.lock"))?;
    lease.lock_shared()?;
    Ok(lease)
}

fn lock(path: &Path) -> Result<File> {
    let parent = path.parent().context("Lock has no parent")?;
    fs::create_dir_all(parent)?;
    ensure!(!fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()), "Refusing symlink lock {}", path.display());
    Ok(OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?)
}

fn mark(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    ensure!(!fs::symlink_metadata(path)?.file_type().is_symlink(), "Refusing symlink build directory {}", path.display());
    let marker = path.join(".paseo-managed");
    ensure!(!fs::symlink_metadata(&marker).is_ok_and(|metadata| metadata.file_type().is_symlink()), "Refusing symlink cache marker");
    if marker.exists() {
        ensure!(fs::read_to_string(&marker)? == MARKER, "Unknown build directory marker");
    } else {
        ensure!(fs::read_dir(path)?.next().is_none(), "Refusing to adopt existing unmarked build directory {}", path.display());
        fs::write(marker, MARKER)?;
    }
    touch(path)
}

fn touch(path: &Path) -> Result<()> {
    let activity = path.join(".last-used");
    ensure!(!fs::symlink_metadata(&activity).is_ok_and(|metadata| metadata.file_type().is_symlink()), "Refusing symlink cache activity record");
    fs::write(activity, b"\n")?;
    Ok(())
}

/// Held through compilation and staging, not just Cargo's narrower build lock.
/// Lock files live outside removable payloads so waiters never lock deleted inodes.
pub struct Lease {
    target: Option<File>,
    source_lease: File,
    generation: Option<PathBuf>,
    source: PathBuf,
}

impl Lease {
    /// A staged client no longer reads Cargo outputs; other worktrees may build
    /// while it runs. Its own source-bound staging stays pinned until exit.
    pub fn release_target(&mut self) {
        if let Some(path) = self.generation.take() {
            let _ = touch(&path);
        }
        if let Some(target) = self.target.take() {
            // A concurrent fork can retain this file description until exec.
            // Unlock explicitly instead of waiting for its last descriptor to close.
            let _ = target.unlock();
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.release_target();
        let _ = touch(&self.source);
        let _ = self.source_lease.unlock();
    }
}

pub fn acquire() -> Result<Lease> {
    // Wait for this checkout before holding the cache coordinator. A running
    // client must not block unrelated checkouts merely by pinning its bundle.
    let source = root()?.join(".dev/build");
    let source_lease = lock(&root()?.join(".dev/build.lock"))?;
    source_lease.lock()?;
    let managed = managed_root()?;
    let coordinator = lock(&managed.join(".lock"))?;
    coordinator.lock()?;
    let target = target_root()?;
    let (target_lease, generation) = if env::var_os("CARGO_TARGET_DIR").is_none() {
        let directory = target.parent().context("Target has no generation")?;
        let name = directory.file_name().context("Generation has no name")?;
        let project = directory.parent().context("Generation has no project")?;
        let lease = lock(&project.join("locks").join(name))?;
        lease.lock()?;
        mark(directory)?;
        (lease, Some(directory.to_path_buf()))
    } else {
        // Explicit output paths are never pruned, but still need staging-wide serialization.
        fs::create_dir_all(&target)?;
        let canonical = fs::canonicalize(&target)?;
        ensure!(
            !canonical.starts_with(fs::canonicalize(&managed)?),
            "Explicit CARGO_TARGET_DIR must be outside the internally managed cache; leave it unset for managed reuse"
        );
        let key = hex::encode(Sha256::digest(canonical.as_os_str().as_encoded_bytes()));
        let lease = lock(&managed.join("explicit-locks").join(key))?;
        lease.lock()?;
        (lease, None)
    };
    mark(&source)?;
    drop(coordinator);
    Ok(Lease { target: Some(target_lease), source_lease, generation, source })
}

/// Optional accelerators are opt-in and never downloaded. Explicit wrapper and
/// compiler flags win. Target-specific linker flags cannot leak into Android/iOS.
pub fn configure(command: &mut Command) -> Result<()> {
    command.env("CARGO_TARGET_DIR", target_root()?);
    if env::var("PASEO_SCCACHE").as_deref() == Ok("1") && env::var_os("RUSTC_WRAPPER").is_none() {
        let wrapper = which::which("sccache").context("PASEO_SCCACHE=1 requires sccache on PATH")?;
        command.env("RUSTC_WRAPPER", wrapper);
        // sccache cannot cache incremental Rust compilations. An explicit caller
        // choice still wins, including CARGO_INCREMENTAL=1.
        if env::var_os("CARGO_INCREMENTAL").is_none() {
            command.env("CARGO_INCREMENTAL", "0");
        }
    }
    if env::var("PASEO_FAST_LINKER").as_deref() == Ok("1") {
        ensure!(cfg!(target_os = "linux") && cfg!(target_env = "gnu"), "PASEO_FAST_LINKER supports native Linux GNU targets only");
        let target = format!("{}_UNKNOWN_LINUX_GNU", env::consts::ARCH.to_ascii_uppercase());
        let linker = format!("CARGO_TARGET_{target}_LINKER");
        let flags = format!("CARGO_TARGET_{target}_RUSTFLAGS");
        if env::var_os(&linker).is_none() && env::var_os(&flags).is_none()
            && env::var_os("RUSTFLAGS").is_none() && env::var_os("CARGO_ENCODED_RUSTFLAGS").is_none()
        {
            let clang = which::which("clang").context("PASEO_FAST_LINKER=1 requires clang on PATH")?;
            let flavor = if which::which("mold").is_ok() { "mold" } else {
                which::which("ld.lld").context("PASEO_FAST_LINKER=1 requires mold or ld.lld on PATH")?;
                "lld"
            };
            command.env(linker, clang).env(flags, format!("-C link-arg=-fuse-ld={flavor}"));
        }
    }
    Ok(())
}

fn eligible(path: &Path, now: SystemTime) -> Result<bool> {
    if fs::symlink_metadata(path)?.file_type().is_symlink()
        || fs::read_to_string(path.join(".paseo-managed")).ok().as_deref() != Some(MARKER)
    {
        return Ok(false);
    }
    let last = match fs::metadata(path.join(".last-used")) {
        Ok(metadata) => metadata.modified()?,
        Err(_) => return Ok(false),
    };
    Ok(now.duration_since(last).is_ok_and(|age| age >= MIN_AGE))
}

fn bytes(path: &Path) -> Result<u64> {
    let mut total = 0u64;
    for entry in WalkDir::new(path).follow_links(false) {
        let entry = entry?;
        if entry.file_type().is_file() {
            total = total.saturating_add(entry.metadata()?.len());
        }
    }
    Ok(total)
}

/// Dry-run by default. A soft budget never overrides a running/recent lease.
pub fn housekeeping(args: &[String]) -> Result<()> {
    let mut apply = false;
    let mut staging = false;
    let mut max_bytes = DEFAULT_MAX_BYTES;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--apply" => apply = true,
            "--staging" => staging = true,
            "--max-bytes" => {
                index += 1;
                max_bytes = args.get(index).context("--max-bytes requires an integer")?.parse()?;
            }
            other => bail!("Unknown housekeeping option {other}; use [--apply] [--staging] [--max-bytes N]"),
        }
        index += 1;
    }
    let managed = managed_root()?;
    let coordinator = lock(&managed.join(".lock"))?;
    coordinator.lock()?;
    let now = SystemTime::now();
    let current = generation()?;
    let mut candidates = Vec::new();
    let mut total = 0u64;
    for project in fs::read_dir(&managed)? {
        let project = project?;
        let name = project.file_name();
        if !project.file_type()?.is_dir() || name.to_str().is_none_or(|name| name.len() != 64 || !name.bytes().all(|b| b.is_ascii_hexdigit())) {
            continue;
        }
        for entry in fs::read_dir(project.path())? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !entry.file_type()?.is_dir() || !name.strip_prefix("generation-").is_some_and(|n| n.parse::<u64>().is_ok()) {
                continue;
            }
            if fs::read_to_string(entry.path().join(".paseo-managed")).ok().as_deref() != Some(MARKER) {
                continue;
            }
            let lease = lock(&project.path().join("locks").join(name))?;
            if lease.try_lock().is_err() {
                println!("Retaining active generation {} (size not sampled)", entry.path().display());
                continue;
            }
            let size = bytes(&entry.path())?;
            total = total.saturating_add(size);
            if name != current && eligible(&entry.path(), now)? {
                let modified = fs::metadata(entry.path().join(".last-used"))?.modified()?;
                candidates.push((modified, entry.path(), project.path().join("locks").join(name), size));
            }
        }
    }
    candidates.sort_by_key(|(modified, _, _, _)| *modified);
    for (_, path, lock_path, size) in candidates {
        if total <= max_bytes { break; }
        let lease = lock(&lock_path)?;
        if lease.try_lock().is_err() { continue; }
        if !eligible(&path, now)? { continue; }
        println!("{} {} ({size} bytes)", if apply { "Removing" } else { "Would remove" }, path.display());
        if apply { fs::remove_dir_all(&path)?; }
        total = total.saturating_sub(size);
    }
    println!("Inactive managed cache after {}: {total} bytes; soft limit {max_bytes}; active/current/recent generations are retained", if apply { "pruning" } else { "planned pruning" });
    if staging {
        let path = root()?.join(".dev/build");
        let lease = lock(&root()?.join(".dev/build.lock"))?;
        let runtime = lock(&root()?.join(".dev/run.lock"))?;
        if lease.try_lock().is_ok() && runtime.try_lock().is_ok() && path.exists() && eligible(&path, now)? {
            println!("{} {}", if apply { "Removing" } else { "Would remove" }, path.display());
            if apply { fs::remove_dir_all(path)?; }
        } else {
            println!("Checkout staging retained: absent, unmarked, active or used within seven days");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_to_adopt_existing_unmanaged_files() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("keep");
        fs::write(&file, b"operator data").unwrap();
        assert!(mark(directory.path()).is_err());
        assert_eq!(fs::read(file).unwrap(), b"operator data");
        assert!(!directory.path().join(".paseo-managed").exists());
    }

    #[test]
    fn pruning_requires_owned_old_activity_record() {
        let directory = tempfile::tempdir().unwrap();
        mark(directory.path()).unwrap();
        let last = UNIX_EPOCH + Duration::from_secs(1_000_000);
        File::options().write(true).open(directory.path().join(".last-used")).unwrap()
            .set_modified(last).unwrap();
        assert!(!eligible(directory.path(), last + MIN_AGE - Duration::from_secs(1)).unwrap());
        assert!(eligible(directory.path(), last + MIN_AGE).unwrap());
        assert!(!eligible(directory.path(), last - Duration::from_secs(1)).unwrap());
        fs::remove_file(directory.path().join(".paseo-managed")).unwrap();
        assert!(!eligible(directory.path(), last + MIN_AGE).unwrap());
    }

    #[test]
    fn releasing_cargo_lease_keeps_running_bundle_pinned() {
        let directory = tempfile::tempdir().unwrap();
        let target_path = directory.path().join("target.lock");
        let source_path = directory.path().join("source.lock");
        let target = lock(&target_path).unwrap();
        target.lock().unwrap();
        let source = lock(&source_path).unwrap();
        source.lock().unwrap();
        // Duplicates model descriptors inherited by a concurrent fork before exec.
        #[cfg(unix)]
        let _inherited_target = target.try_clone().unwrap();
        #[cfg(unix)]
        let _inherited_source = source.try_clone().unwrap();
        let mut lease = Lease {
            target: Some(target),
            source_lease: source,
            generation: None,
            source: directory.path().to_path_buf(),
        };
        assert!(lock(&target_path).unwrap().try_lock().is_err());
        lease.release_target();
        let another_build = lock(&target_path).unwrap();
        assert!(another_build.try_lock().is_ok());
        assert!(lock(&source_path).unwrap().try_lock().is_err());
        drop(lease);
        assert!(lock(&source_path).unwrap().try_lock().is_ok());
    }
}
