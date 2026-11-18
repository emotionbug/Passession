use super::{command, scheduler};
use anyhow::{Result, bail};
use paseo_client::process::{CommandSpec, ManagedProcess};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{io::AsyncReadExt, runtime::Handle};
use tokio_util::sync::CancellationToken;

pub(super) fn subtree<'a>(
    paths: &'a BTreeSet<PathBuf>,
    base: &'a Path,
) -> impl Iterator<Item = &'a PathBuf> {
    paths
        .range(base.to_owned()..)
        .take_while(move |p| p.starts_with(base))
}

pub(super) fn is_dir(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.is_dir()),
        Err(error) if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
            Ok(false)
        }
        Err(error) => Err(error.into()),
    }
}

/// Walk just `base`. Register each parent before reading it so a child created
/// concurrently is either discovered in the walk or delivered as an event.
/// Ignored directory trees are never descended into, even for tracked files.
pub(super) fn visible_directories(
    root: &Path,
    base: &Path,
    env: &BTreeMap<String, String>,
    cancel: &CancellationToken,
    runtime: &Handle,
    mut register: impl FnMut(&Path) -> Result<bool>,
) -> Result<BTreeSet<PathBuf>> {
    let mut directories = BTreeSet::new();
    let mut frontier = BTreeSet::from([base.to_owned()]);
    while !frontier.is_empty() {
        if cancel.is_cancelled() {
            bail!("Git watch inventory cancelled");
        }
        let ignored = runtime.block_on(ignored_paths(
            root,
            frontier.iter().filter(|p| *p != root).map(PathBuf::as_path),
            true,
            env,
            cancel,
        ))?;
        let mut children = BTreeSet::new();
        for directory in frontier {
            let relative = directory.strip_prefix(root)?;
            if ignored.contains(relative) || !register(&directory)? {
                continue;
            }
            let entries = match fs::read_dir(&directory) {
                Ok(entries) => entries,
                Err(error)
                    if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) =>
                {
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            for entry in entries {
                let entry = entry?;
                if entry.file_name() != ".git" && entry.file_type()?.is_dir() {
                    children.insert(entry.path());
                }
            }
            directories.insert(directory);
        }
        frontier = children;
    }
    Ok(directories)
}

/// List only tracked paths Git considers ignored, not every tracked path.
/// Store their ancestor directories even when absent, so a later create can
/// add the required watches without another index query.
pub(super) async fn tracked_ignored_dirs(
    root: &Path,
    base: &Path,
    env: &BTreeMap<String, String>,
    cancel: &CancellationToken,
) -> Result<BTreeSet<PathBuf>> {
    let mut args = [
        "--literal-pathspecs",
        "ls-files",
        "-z",
        "--cached",
        "--ignored",
        "--exclude-standard",
    ]
    .map(str::to_owned)
    .to_vec();
    let relative = base.strip_prefix(root)?;
    if let Some(path) = relative.to_str().filter(|path| !path.is_empty()) {
        args.push("--".into());
        #[cfg(windows)]
        args.push(path.replace('\\', "/"));
        #[cfg(not(windows))]
        args.push(path.into());
    }
    let bytes = command::checked(
        root,
        &args,
        env,
        cancel.child_token(),
        command::READ_LIMIT,
        command::READ_TIMEOUT,
    )
    .await?;
    let mut dirs = BTreeSet::new();
    for path in git_paths(&bytes) {
        let file = root.join(path?);
        if !file.starts_with(base) {
            continue;
        }
        for dir in file.ancestors().skip(1).take_while(|p| p.starts_with(base)) {
            if !dirs.insert(dir.to_owned()) {
                break;
            }
        }
    }
    Ok(dirs)
}

/// Git's default check retains tracked files even when they match an ignore
/// rule. Directory traversal uses --no-index and adds tracked exceptions
/// independently; ordinary file event batches use the default index check.
pub(super) async fn ignored_paths<'a>(
    root: &Path,
    paths: impl Iterator<Item = &'a Path>,
    no_index: bool,
    env: &BTreeMap<String, String>,
    cancel: &CancellationToken,
) -> Result<BTreeSet<PathBuf>> {
    let mut input = Vec::new();
    for path in paths {
        let path = path.strip_prefix(root)?;
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            input.extend_from_slice(path.as_os_str().as_bytes());
        }
        #[cfg(windows)]
        input.extend_from_slice(
            path.to_str()
                .ok_or_else(|| anyhow::anyhow!("Git path is not Unicode"))?
                .replace('\\', "/")
                .as_bytes(),
        );
        input.push(0);
    }
    if input.is_empty() {
        return Ok(BTreeSet::new());
    }
    let _permit = scheduler::acquire(cancel).await?;
    let mut environment = env.clone();
    environment.extend([
        ("GIT_TERMINAL_PROMPT".into(), "0".into()),
        ("GIT_OPTIONAL_LOCKS".into(), "0".into()),
        ("LC_ALL".into(), "C".into()),
    ]);
    let mut args = ["check-ignore", "-z", "--stdin"]
        .map(str::to_owned)
        .to_vec();
    if no_index {
        args.push("--no-index".into());
    }
    let (process, stdout) = ManagedProcess::spawn(CommandSpec {
        program: "git".into(),
        args,
        cwd: Some(root.into()),
        env: environment,
        clear_env: false,
    })
    .await?;
    let read = async {
        let mut output = Vec::new();
        stdout
            .take((command::READ_LIMIT + 1) as u64)
            .read_to_end(&mut output)
            .await?;
        if output.len() > command::READ_LIMIT {
            bail!("Git ignore output exceeded limit");
        }
        Ok(output)
    };
    let write = async {
        process.write_bytes(&input).await?;
        process.close_stdin().await
    };
    let result = tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(anyhow::anyhow!("Git ignore check cancelled")),
        _ = tokio::time::sleep(command::READ_TIMEOUT) => Err(anyhow::anyhow!("Git ignore check timed out")),
        result = async { tokio::try_join!(read, write, process.wait()) } => result,
    };
    let (output, (), status) = match result {
        Ok(result) => result,
        Err(error) => {
            if let Err(cleanup) = process.terminate(Duration::ZERO).await {
                return Err(error.context(format!("Git ignore check cleanup failed: {cleanup:#}")));
            }
            return Err(error);
        }
    };
    if status != 0 && status != 1 {
        bail!("{}", process.stderr_tail());
    }
    git_paths(&output).collect()
}

fn git_paths(output: &[u8]) -> impl Iterator<Item = Result<PathBuf>> {
    output
        .split(|byte| *byte == 0)
        .filter(|p| !p.is_empty())
        .map(|path| {
            #[cfg(unix)]
            {
                use std::os::unix::ffi::OsStrExt;
                Ok(PathBuf::from(std::ffi::OsStr::from_bytes(path)))
            }
            #[cfg(windows)]
            {
                Ok(PathBuf::from(std::str::from_utf8(path)?))
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn watch_tree_skips_ignored_trees_but_includes_empty_and_tracked_directories()
    -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path();
        let env = BTreeMap::new();
        let cancel = CancellationToken::new();
        command::checked(
            root,
            &["init".into()],
            &env,
            cancel.clone(),
            command::READ_LIMIT,
            command::READ_TIMEOUT,
        )
        .await?;
        for directory in ["target/deep/tree", "empty/nested", "ignored/tracked"] {
            tokio::fs::create_dir_all(root.join(directory)).await?;
        }
        tokio::fs::write(root.join(".gitignore"), "target/\nignored/\n").await?;
        tokio::fs::write(root.join("ignored/tracked/file"), "tracked").await?;
        command::checked(
            root,
            &["add".into(), "-f".into(), "ignored/tracked/file".into()],
            &env,
            cancel.clone(),
            command::READ_LIMIT,
            command::READ_TIMEOUT,
        )
        .await?;
        let tracked = tracked_ignored_dirs(root, root, &env, &cancel).await?;
        let root = root.to_owned();
        let runtime = Handle::current();
        let scanned = tokio::task::spawn_blocking(move || {
            visible_directories(&root, &root, &env, &cancel, &runtime, |_| Ok(true))
        })
        .await??;
        assert!(!scanned.contains(&temp.path().join("target")));
        assert!(!scanned.contains(&temp.path().join(".git")));
        assert!(scanned.contains(&temp.path().join("empty/nested")));
        assert!(!scanned.contains(&temp.path().join("ignored")));
        assert!(tracked.contains(&temp.path().join("ignored/tracked")));
        Ok(())
    }
}
