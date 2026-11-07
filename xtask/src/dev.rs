//! Native checkout workflows. Child-only environment overrides never mutate the caller.
mod catalog;
mod probes;
mod titles;

use anyhow::{Context, Result, bail, ensure};
use serde_json::json;
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, SystemTime},
};
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;

pub fn run(command: &str, args: &[String]) -> Result<()> {
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(async {
        let cancel = CancellationToken::new();
        let signal_cancel = cancel.clone();
        tokio::spawn(async move {
            #[cfg(unix)] {
                if let Ok(mut term) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                    tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term.recv() => {} }
                } else { let _ = tokio::signal::ctrl_c().await; }
            }
            #[cfg(not(unix))] { let _ = tokio::signal::ctrl_c().await; }
            signal_cancel.cancel();
        });
        match command {
            "dev" => development(args, &cancel).await,
            "maintenance" => match args.first().map(String::as_str) {
                Some("seed-dev-state") => { ensure!(args.len() == 1, "seed-dev-state takes environment configuration only"); seed_worktree() },
                Some("acp-catalog") => catalog::run(&args[1..]).await,
                Some("codex-titles") => titles::run(&args[1..], &cancel).await,
                Some("relay-latency") => probes::run(false, &args[1..], &cancel).await,
                Some("relay-prod") => probes::run(true, &args[1..], &cancel).await,
                _ => bail!("Usage: cargo xtask maintenance <seed-dev-state|acp-catalog|codex-titles|relay-latency|relay-prod> [options]"),
            },
            _ => bail!("Unknown developer command {command}"),
        }
    })
}

fn variable(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}
pub(super) fn user_home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .context("HOME/USERPROFILE is unset")
}
fn absolute(path: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    // Canonicalize the existing ancestor, so aliases cannot bypass ownership checks.
    if path.exists() {
        return Ok(path.canonicalize()?);
    }
    let parent = path.parent().context("path has no parent")?;
    Ok(absolute(parent)?.join(path.file_name().context("path has no filename")?))
}
fn private_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
pub(super) fn private_json(path: &Path, value: &serde_json::Value) -> Result<()> {
    use std::io::Write;
    let parent = path.parent().context("JSON path has no parent")?;
    private_directory(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(&mut temporary, value)?;
    temporary.write_all(b"\n")?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}
fn copy_json_tree(source: &Path, target: &Path) -> Result<()> {
    if !source.exists() {
        return Ok(());
    }
    ensure!(
        !fs::symlink_metadata(source)?.file_type().is_symlink(),
        "Seed directories must not be symbolic links"
    );
    private_directory(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_json_tree(&entry.path(), &target.join(entry.file_name()))?;
        } else if kind.is_file() && entry.path().extension().is_some_and(|e| e == "json") {
            private_json(
                &target.join(entry.file_name()),
                &crate::common::read_json(&entry.path())?,
            )?;
        }
    }
    Ok(())
}
fn seed_home(source: &Path, target: &Path, reset: bool, owned: bool) -> Result<()> {
    if !source.exists() {
        return Ok(());
    }
    let source = absolute(source)?;
    let target = absolute(target)?;
    if source == target {
        return Ok(());
    }
    ensure!(
        !target.starts_with(&source) && !source.starts_with(&target),
        "Seed homes must not contain one another"
    );
    let production = absolute(&user_home()?.join(".paseo"))?;
    ensure!(
        !target.starts_with(&production) && !production.starts_with(&target),
        "Development seed target must not overlap the production Paseo home"
    );
    if reset && target.exists() {
        ensure!(
            owned && target.join(".paseo-dev-home").is_file(),
            "Reset requires a previously managed development home marker"
        );
        fs::remove_dir_all(&target)?;
    } else if target.is_dir() && fs::read_dir(&target)?.next().is_some() {
        return Ok(());
    }
    private_directory(&target)?;
    for directory in ["agents", "projects"] {
        copy_json_tree(&source.join(directory), &target.join(directory))?;
    }
    if source.join("config.json").is_file() {
        ensure!(
            !fs::symlink_metadata(source.join("config.json"))?
                .file_type()
                .is_symlink(),
            "Seed config must not be a symbolic link"
        );
        private_json(
            &target.join("config.json"),
            &crate::common::read_json(&source.join("config.json"))?,
        )?;
    }
    if owned {
        fs::write(target.join(".paseo-dev-home"), b"paseo development home\n")?;
    }
    Ok(())
}
fn seed_worktree() -> Result<()> {
    let Some(source) = variable("PASEO_SOURCE_CHECKOUT_PATH") else {
        return Ok(());
    };
    let target = std::env::var_os("PASEO_WORKTREE_PATH")
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir()?);
    if absolute(Path::new(&source))? == absolute(&target)? {
        return Ok(());
    }
    let source = std::env::var_os("PASEO_DEV_SEED_HOME")
        .map(PathBuf::from)
        .unwrap_or(PathBuf::from(source).join(".dev/paseo-home"));
    seed_home(
        &source,
        &target.join(".dev/paseo-home"),
        variable("PASEO_DEV_RESET_HOME").as_deref() == Some("1"),
        true,
    )
}
fn development_env(root: &Path) -> Result<BTreeMap<OsString, OsString>> {
    let mut env: BTreeMap<OsString, OsString> = std::env::vars_os().collect();
    let owned = variable("PASEO_HOME").is_none()
        || variable("PASEO_DEV_MANAGED_HOME").as_deref() == Some("1");
    let home = std::env::var_os("PASEO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("PASEO_DEV_ROOT")
                .or_else(|| std::env::var_os("PASEO_WORKTREE_PATH"))
                .map(PathBuf::from)
                .unwrap_or(root.to_owned())
                .join(".dev/paseo-home")
        });
    let home = absolute(&home)?;
    let production = absolute(&user_home()?.join(".paseo"))?;
    ensure!(
        !home.starts_with(&production) && !production.starts_with(&home),
        "Development home must not overlap production ~/.paseo; select an isolated PASEO_HOME"
    );
    if let Some(source) = std::env::var_os("PASEO_DEV_SEED_HOME") {
        seed_home(
            Path::new(&source),
            &home,
            variable("PASEO_DEV_RESET_HOME").as_deref() == Some("1"),
            owned,
        )?;
    }
    private_directory(&home)?;
    if owned {
        fs::write(home.join(".paseo-dev-home"), b"paseo development home\n")?;
    }
    let listen = variable("PASEO_LISTEN").unwrap_or_else(|| {
        variable("PASEO_SERVICE_DAEMON_PORT")
            .or_else(|| variable("PASEO_PORT"))
            .map(|p| format!("0.0.0.0:{p}"))
            .unwrap_or_else(|| "127.0.0.1:6768".into())
    });
    if owned || variable("PASEO_DEV_SEED_HOME").is_some() {
        let filename = home.join("config.json");
        let mut config = if filename.exists() {
            crate::common::read_json(&filename)?
        } else {
            json!({})
        };
        ensure!(config.is_object(), "Development config must be an object");
        if config.get("version").is_none() {
            config["version"] = json!(1);
        }
        if config.get("daemon").is_none() {
            config["daemon"] = json!({});
        }
        ensure!(
            config["daemon"].is_object(),
            "config.daemon must be an object"
        );
        config["daemon"]["listen"] = json!(listen);
        if config["daemon"].get("cors").is_none() {
            config["daemon"]["cors"] = json!({});
        }
        ensure!(
            config["daemon"]["cors"].is_object(),
            "config.daemon.cors must be an object"
        );
        config["daemon"]["cors"]["allowedOrigins"] = json!(["*"]);
        private_json(&filename, &config)?;
    }
    for name in [
        "PASEO_NATIVE_RELEASE",
        "PASEO_UPDATE_REPOSITORY",
        "PASEO_RELEASE_CHANNEL",
    ] {
        env.remove(std::ffi::OsStr::new(name));
    }
    env.insert("PASEO_HOME".into(), home.into_os_string());
    env.insert("PASEO_LISTEN".into(), listen.into());
    env.insert("PASEO_NODE_ENV".into(), "development".into());
    env.entry("PASEO_CORS_ORIGINS".into())
        .or_insert_with(|| "*".into());
    if owned {
        env.insert("PASEO_DEV_MANAGED_HOME".into(), "1".into());
    }
    Ok(env)
}
pub(super) async fn stop_child(child: &mut Child) -> Result<()> {
    if child.try_wait()?.is_some() {
        return Ok(());
    }
    #[cfg(unix)]
    if let Some(id) = child.id() {
        unsafe {
            libc::kill(id as libc::pid_t, libc::SIGTERM);
        }
    }
    #[cfg(not(unix))]
    child.start_kill()?;
    match tokio::time::timeout(Duration::from_secs(10), child.wait()).await {
        Ok(status) => {
            status?;
        }
        Err(_) => {
            child.kill().await?;
        }
    }
    Ok(())
}
async fn command_status(
    command: &mut Command,
    cancel: &CancellationToken,
) -> Result<std::process::ExitStatus> {
    let mut child = command.kill_on_drop(true).spawn()?;
    tokio::select! {
        status = child.wait() => Ok(status?),
        _ = cancel.cancelled() => { stop_child(&mut child).await?; bail!("Command cancelled") }
    }
}
fn snapshot(root: &Path) -> Result<BTreeMap<PathBuf, (SystemTime, u64)>> {
    fn visit(path: &Path, out: &mut BTreeMap<PathBuf, (SystemTime, u64)>) -> Result<()> {
        if !path.exists() {
            return Ok(());
        }
        let metadata = fs::symlink_metadata(path)?;
        if metadata.is_symlink() {
            return Ok(());
        }
        if metadata.is_file() {
            out.insert(path.to_owned(), (metadata.modified()?, metadata.len()));
        } else {
            for entry in fs::read_dir(path)? {
                let entry = entry?;
                if !matches!(
                    entry.file_name().to_str(),
                    Some("target" | "dist" | "node_modules" | ".git")
                ) {
                    visit(&entry.path(), out)?;
                }
            }
        }
        Ok(())
    }
    let mut files = BTreeMap::new();
    for member in [
        "Cargo.toml",
        "Cargo.lock",
        "packages/daemon",
        "packages/plugin/src",
        "packages/plugin/Cargo.toml",
        "packages/protocol/src",
        "packages/protocol/Cargo.toml",
        "packages/client/src",
        "packages/client/Cargo.toml",
        "plugins",
        "skills",
    ] {
        visit(&root.join(member), &mut files)?;
    }
    Ok(files)
}
async fn development(args: &[String], cancel: &CancellationToken) -> Result<()> {
    let root = crate::common::root()?;
    let _runtime_lease = crate::build_cache::runtime_lease()?;
    let env = development_env(&root)?;
    if args.first().is_some_and(|s| s == "app") {
        // Build wrapper owns its cargo/Gradle/device descendants and credential
        // restoration. A private sentinel also works without Unix signals.
        let cancellation = tempfile::tempdir()?;
        let cancellation_path = cancellation.path().join("cancel");
        let mut child = Command::new(std::env::current_exe()?)
            .arg("build-gui")
            .args(["--profile", "dev-fast"])
            .args(&args[1..])
            .arg("--run")
            .current_dir(&root)
            .env_clear()
            .envs(&env)
            .env("PASEO_XTASK_CANCEL_FILE", &cancellation_path)
            .spawn()?;
        let status = tokio::select! {
            status = child.wait() => status?,
            _ = cancel.cancelled() => {
                fs::write(&cancellation_path, b"cancel\n")?;
                #[cfg(unix)] if let Some(id) = child.id() { unsafe { libc::kill(id as libc::pid_t, libc::SIGTERM); } }
                child.wait().await?;
                return Ok(());
            }
        };
        ensure!(status.success(), "Development app exited {status}");
        return Ok(());
    }
    if args.first().is_some_and(|s| s == "home") {
        if args.len() == 1 {
            println!(
                "{}",
                Path::new(&env[std::ffi::OsStr::new("PASEO_HOME")]).display()
            );
            return Ok(());
        }
        let rest = if args[1] == "--" {
            &args[2..]
        } else {
            &args[1..]
        };
        let program = rest
            .first()
            .context("dev home requires an executable after --")?;
        let status = command_status(
            Command::new(program)
                .args(&rest[1..])
                .current_dir(&root)
                .env_clear()
                .envs(&env),
            cancel,
        )
        .await?;
        ensure!(status.success(), "Development command exited {status}");
        return Ok(());
    }
    let args = if args.first().is_some_and(|s| s == "daemon" || s == "cli") {
        &args[1..]
    } else {
        args
    };
    let watch = args.first().is_some_and(|s| s == "--watch");
    let args = if watch { &args[1..] } else { args };
    ensure!(
        !watch || args.is_empty(),
        "--watch supports only the foreground development daemon"
    );
    let args = if args.first().is_some_and(|s| s == "--") {
        &args[1..]
    } else {
        args
    };
    let command = if args.is_empty() {
        vec!["daemon".into(), "run".into()]
    } else {
        args.to_vec()
    };
    let executable = crate::build::directory("dev-fast")?.join(crate::common::executable());
    let mut changed = if watch {
        snapshot(&root)?
    } else {
        BTreeMap::new()
    };
    let mut rebuild = variable("PASEO_SKIP_NATIVE_BUILD").as_deref() != Some("1");
    let mut child: Option<Child> = None;
    let mut initial_start = !rebuild;
    loop {
        if cancel.is_cancelled() {
            if let Some(mut child) = child.take() {
                stop_child(&mut child).await?;
            }
            return Ok(());
        }
        if rebuild {
            if let Some(mut child) = child.take() {
                stop_child(&mut child).await?;
            }
            // Avoid nested cargo-run locks, and let an in-flight build settle on
            // cancellation instead of orphaning the Cargo subprocess it owns.
            let result = Command::new(std::env::current_exe()?)
                .arg("build-daemon")
                .current_dir(&root)
                .env_clear()
                .envs(&env)
                .status()
                .await?;
            if cancel.is_cancelled() {
                return Ok(());
            }
            rebuild = false;
            if !result.success() {
                if !watch {
                    bail!("Development build failed: {result}");
                }
                eprintln!("Build failed; watching for source changes");
            } else {
                child = Some(
                    Command::new(&executable)
                        .args(&command)
                        .current_dir(&root)
                        .env_clear()
                        .envs(&env)
                        .stdin(Stdio::inherit())
                        .kill_on_drop(true)
                        .spawn()?,
                );
            }
        } else if initial_start {
            initial_start = false;
            ensure!(
                executable.is_file(),
                "No native development build; run cargo xtask build-daemon"
            );
            child = Some(
                Command::new(&executable)
                    .args(&command)
                    .current_dir(&root)
                    .env_clear()
                    .envs(&env)
                    .kill_on_drop(true)
                    .spawn()?,
            );
        }
        if let Some(active) = child.as_mut() {
            if let Some(status) = active.try_wait()? {
                child = None;
                if status.code() == Some(75) {
                    child = Some(
                        Command::new(&executable)
                            .args(&command)
                            .current_dir(&root)
                            .env_clear()
                            .envs(&env)
                            .kill_on_drop(true)
                            .spawn()?,
                    );
                } else if !watch {
                    ensure!(status.success(), "Daemon exited {status}");
                    return Ok(());
                } else {
                    eprintln!("Daemon exited {status}; watching for source changes");
                }
            }
        }
        tokio::select! { _ = cancel.cancelled() => {}, _ = tokio::time::sleep(Duration::from_millis(250)) => {} }
        if watch {
            let next = snapshot(&root)?;
            if next != changed {
                changed = next;
                rebuild = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn seed_copies_only_durable_json_and_rejects_unowned_reset() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let target = temp.path().join("target");
        fs::create_dir_all(source.join("agents/nested")).unwrap();
        fs::write(source.join("agents/nested/a.json"), b"{\"title\":\"test\"}").unwrap();
        fs::write(source.join("agents/live.sock"), b"runtime").unwrap();
        fs::write(source.join("connection-password.txt"), b"secret").unwrap();
        seed_home(&source, &target, false, true).unwrap();
        assert!(target.join("agents/nested/a.json").is_file());
        assert!(!target.join("agents/live.sock").exists());
        assert!(!target.join("connection-password.txt").exists());
        assert!(seed_home(&source, &target, true, false).is_err());
        fs::write(target.join("old.json"), b"{}").unwrap();
        seed_home(&source, &target, true, true).unwrap();
        assert!(!target.join("old.json").exists());
    }
    #[test]
    fn seed_does_not_follow_symlinks_or_copy_into_itself() {
        let temp = tempfile::tempdir().unwrap();
        assert!(seed_home(temp.path(), &temp.path().join("nested"), false, true).is_err());
        #[cfg(unix)]
        {
            let source = temp.path().join("source");
            let target = temp.path().join("target");
            fs::create_dir_all(source.join("agents")).unwrap();
            std::os::unix::fs::symlink("/etc/passwd", source.join("agents/secret.json")).unwrap();
            seed_home(&source, &target, false, true).unwrap();
            assert!(!target.join("agents/secret.json").exists());
        }
    }
}
