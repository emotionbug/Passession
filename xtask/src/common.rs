use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    env,
    ffi::OsStr,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::Duration,
};

pub fn root() -> Result<PathBuf> {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("xtask must be inside the workspace")?;
    ensure!(
        directory.join("Cargo.toml").is_file(),
        "Workspace Cargo.toml is missing"
    );
    Ok(directory.to_path_buf())
}

pub fn read_json(path: &Path) -> Result<Value> {
    serde_json::from_reader(
        fs::File::open(path).with_context(|| format!("Reading {}", path.display()))?,
    )
    .with_context(|| format!("Parsing {}", path.display()))
}

pub fn write_json(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut temporary = tempfile::NamedTempFile::new_in(path.parent().unwrap_or(Path::new(".")))?;
    serde_json::to_writer_pretty(&mut temporary, value)?;
    temporary.write_all(b"\n")?;
    // Build metadata ships with packages and must be readable by their users.
    chmod(temporary.path(), 0o644)?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("Writing {}", path.display()))?;
    Ok(())
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path).with_context(|| format!("Hashing {}", path.display()))?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(hex::encode(digest.finalize()))
}

pub fn version() -> Result<String> {
    let manifest: toml::Value = toml::from_str(&fs::read_to_string(root()?.join("Cargo.toml"))?)?;
    let value = manifest
        .get("workspace")
        .and_then(|v| v.get("package"))
        .and_then(|v| v.get("version"))
        .and_then(toml::Value::as_str)
        .context("Missing workspace.package.version")?;
    semver::Version::parse(value).context("Invalid workspace version")?;
    Ok(value.to_owned())
}

pub fn platform() -> Result<&'static str> {
    match env::consts::OS {
        "linux" => Ok("linux"),
        "macos" => Ok("macos"),
        "windows" => Ok("windows"),
        other => bail!("Unsupported native platform: {other}"),
    }
}

pub fn arch() -> Result<&'static str> {
    match env::consts::ARCH {
        "x86_64" => Ok("x64"),
        "aarch64" => Ok("arm64"),
        other => bail!("Unsupported native architecture: {other}"),
    }
}

pub fn executable() -> &'static str {
    if cfg!(windows) { "paseo.exe" } else { "paseo" }
}
pub fn app() -> Result<PathBuf> {
    Ok(root()?.join("packages/flutter_app"))
}
pub fn release() -> Result<PathBuf> {
    Ok(root()?.join("artifacts/flutter"))
}
pub fn support() -> Result<PathBuf> {
    Ok(root()?.join("packages/flutter_desktop_host/packaging"))
}
pub fn artifact_stem(target: &str, cpu: &str) -> Result<String> {
    Ok(format!(
        "Paseo-Sessions-Flutter-{}-{target}-{cpu}",
        version()?
    ))
}
pub fn cargo() -> std::ffi::OsString {
    env::var_os("CARGO").unwrap_or_else(|| "cargo".into())
}
pub fn command(program: impl AsRef<OsStr>) -> Result<Command> {
    let mut command = Command::new(program);
    command.current_dir(root()?);
    Ok(command)
}

static SUPERVISED: AtomicBool = AtomicBool::new(false);
static CANCELLED: AtomicBool = AtomicBool::new(false);

/// Keep the wrapper alive long enough to reap its subprocesses and restore
/// temporarily staged mobile credentials. Dev wrappers also supply a private
/// cancellation marker, because Windows taskkill cannot unwind Rust guards.
pub fn supervise_subprocesses() -> Result<()> {
    ctrlc::set_handler(|| CANCELLED.store(true, Ordering::Relaxed))
        .context("Installing build cancellation handler")?;
    SUPERVISED.store(true, Ordering::Relaxed);
    Ok(())
}

fn cancelled() -> bool {
    CANCELLED.load(Ordering::Relaxed)
        || env::var_os("PASEO_XTASK_CANCEL_FILE").is_some_and(|path| Path::new(&path).exists())
}

fn spawn_supervised(command: &mut Command) -> Result<Child> {
    ensure!(!cancelled(), "Build cancelled");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command
        .spawn()
        .with_context(|| format!("Starting {:?}", command.get_program()))
}

fn cancel_child(child: &mut Child, grace: Duration) -> Result<()> {
    #[cfg(unix)]
    {
        let group = i32::try_from(child.id()).context("Child process group exceeds pid_t")?;
        // The root remains unreaped throughout the grace interval, so its PID
        // cannot be reused before the final group-wide cleanup.
        let signal = |number| -> Result<()> {
            if unsafe { libc::kill(-group, number) } == -1 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    return Err(error.into());
                }
            }
            Ok(())
        };
        let terminate = signal(libc::SIGTERM);
        thread::sleep(grace);
        if let Err(error) = signal(libc::SIGKILL) {
            // Still reap the direct child when group termination fails, but
            // preserve the failure: descendant cleanup is not established.
            child
                .kill()
                .context("Stopping build subprocess after group cleanup failed")?;
            child
                .wait()
                .context("Reaping build subprocess after group cleanup failed")?;
            return Err(error.context("Stopping owned build process group"));
        }
        child.wait().context("Reaping cancelled build subprocess")?;
        terminate?;
    }
    #[cfg(windows)]
    {
        let _ = grace;
        let status = Command::new("taskkill")
            .args(["/PID", &child.id().to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context("Stopping owned build process tree")?;
        if !status.success() && child.try_wait()?.is_none() {
            bail!("taskkill could not stop the owned build process tree");
        }
    }
    child.wait().context("Reaping cancelled build subprocess")?;
    Ok(())
}

fn wait_supervised(child: &mut Child) -> Result<ExitStatus> {
    loop {
        if cancelled() {
            cancel_child(child, Duration::from_secs(10))?;
            bail!("Build cancelled");
        }
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {}
            Err(error) => {
                let cleanup = cancel_child(child, Duration::from_secs(10));
                return match cleanup {
                    Ok(()) => Err(error.into()),
                    Err(cleanup) => Err(anyhow::anyhow!(
                        "Waiting for build subprocess failed: {error}; cleanup also failed: {cleanup:#}"
                    )),
                };
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
}

pub fn run(command: &mut Command) -> Result<()> {
    let status = if SUPERVISED.load(Ordering::Relaxed) {
        wait_supervised(&mut spawn_supervised(command)?)?
    } else {
        command
            .status()
            .with_context(|| format!("Starting {:?}", command.get_program()))?
    };
    ensure!(
        status.success(),
        "{:?} exited with {status}",
        command.get_program()
    );
    Ok(())
}

pub fn capture(command: &mut Command) -> Result<String> {
    command.stderr(Stdio::inherit());
    if !SUPERVISED.load(Ordering::Relaxed) {
        let output = command
            .output()
            .with_context(|| format!("Starting {:?}", command.get_program()))?;
        ensure!(
            output.status.success(),
            "{:?} exited with {}",
            command.get_program(),
            output.status
        );
        return Ok(String::from_utf8(output.stdout)?.trim().to_owned());
    }
    command.stdout(Stdio::piped());
    let mut child = spawn_supervised(command)?;
    let mut stdout = child
        .stdout
        .take()
        .context("Missing captured subprocess stdout")?;
    // Drain concurrently: waiting before reading can deadlock a full pipe.
    let reader = thread::spawn(move || -> std::io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes)?;
        Ok(bytes)
    });
    // A failed cleanup must not block credential restoration on a pipe still
    // held by an unkillable descendant. The reader owns and eventually closes
    // that pipe independently.
    let status = wait_supervised(&mut child)?;
    let bytes = reader
        .join()
        .map_err(|_| anyhow::anyhow!("Subprocess stdout reader panicked"))??;
    ensure!(
        status.success(),
        "{:?} exited with {status}",
        command.get_program()
    );
    Ok(String::from_utf8(bytes)?.trim().to_owned())
}

pub fn flutter() -> Result<PathBuf> {
    let binary = if cfg!(windows) {
        "flutter.bat"
    } else {
        "flutter"
    };
    let mut candidates = Vec::new();
    if let Some(path) = env::var_os("FLUTTER_BIN") {
        candidates.push(PathBuf::from(path));
    }
    for key in ["FLUTTER_ROOT", "FLUTTER_HOME"] {
        if let Some(path) = env::var_os(key) {
            candidates.push(PathBuf::from(path).join("bin").join(binary));
        }
    }
    candidates.push(cache()?.join("paseo-flutter-sdk/bin").join(binary));
    if let Some(path) = env::var_os("PATH") {
        candidates.extend(env::split_paths(&path).map(|directory| directory.join(binary)));
    }
    candidates.into_iter().find(|path| path.is_file()).context("Flutter SDK not found. Set FLUTTER_BIN/FLUTTER_ROOT, use $XDG_CACHE_HOME/paseo-flutter-sdk, or add Flutter to PATH")
}

pub fn cache() -> Result<PathBuf> {
    if let Some(path) = env::var_os("XDG_CACHE_HOME") {
        return Ok(path.into());
    }
    Ok(dirs::home_dir()
        .context("Cannot resolve home directory")?
        .join(".cache"))
}

pub fn build_number(target: &str) -> Result<u64> {
    let value = env::var("PASEO_BUILD_NUMBER")
        .or_else(|_| env::var("ANDROID_VERSION_CODE"))
        .unwrap_or_else(|_| "13001".into());
    ensure!(
        !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()),
        "PASEO_BUILD_NUMBER must be numeric"
    );
    let number = value.parse::<u64>()?;
    Ok(if target == "android" {
        number.max(13000)
    } else {
        number
    })
}

pub fn text(path: &Path, contents: impl AsRef<[u8]>, mode: u32) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, contents)?;
    chmod(path, mode)
}

pub fn chmod(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}

/// Materialize links as regular files for relocatable updater archives. Track the
/// active ancestry to reject directory-link cycles without rejecting shared data.
pub fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    fn copy(source: &Path, destination: &Path, ancestors: &mut Vec<PathBuf>) -> Result<()> {
        let canonical =
            fs::canonicalize(source).with_context(|| format!("Copying {}", source.display()))?;
        let metadata = fs::metadata(&canonical)?;
        if metadata.is_dir() {
            ensure!(
                !ancestors.contains(&canonical),
                "Directory link cycle: {}",
                source.display()
            );
            ancestors.push(canonical);
            fs::create_dir_all(destination)?;
            for entry in fs::read_dir(source)? {
                let entry = entry?;
                copy(
                    &entry.path(),
                    &destination.join(entry.file_name()),
                    ancestors,
                )?;
            }
            ancestors.pop();
        } else {
            ensure!(
                metadata.is_file(),
                "Unsupported payload member: {}",
                source.display()
            );
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(source, destination)?;
        }
        Ok(())
    }
    copy(source, destination, &mut Vec::new())
}

pub fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path)?,
        Ok(_) => fs::remove_file(path)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

pub fn absolute(path: impl AsRef<Path>) -> Result<PathBuf> {
    let path = path.as_ref();
    Ok(if path.is_absolute() {
        path.to_path_buf()
    } else {
        root()?.join(path)
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{
        io::{BufRead, BufReader},
        os::unix::process::{CommandExt, ExitStatusExt},
    };

    #[test]
    fn published_json_remains_readable_after_replacement() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir()?;
        let path = directory.path().join("paseo-runtime.json");
        write_json(&path, &json!({"version": 1}))?;
        chmod(&path, 0o600)?;
        write_json(&path, &json!({"version": 2}))?;
        assert_eq!(read_json(&path)?, json!({"version": 2}));
        assert_eq!(fs::metadata(path)?.permissions().mode() & 0o777, 0o644);
        Ok(())
    }

    #[test]
    fn cancellation_reaps_a_process_ignoring_termination() -> Result<()> {
        let mut child = Command::new("sh")
            .args(["-c", "trap '' TERM; printf 'ready\\n'; exec sleep 30"])
            .process_group(0)
            .stdout(Stdio::piped())
            .spawn()?;
        let mut ready = String::new();
        BufReader::new(child.stdout.take().context("Missing fixture stdout")?)
            .read_line(&mut ready)?;
        assert_eq!(ready, "ready\n");
        cancel_child(&mut child, Duration::from_millis(20))?;
        assert_eq!(child.wait()?.signal(), Some(libc::SIGKILL));
        Ok(())
    }
}
