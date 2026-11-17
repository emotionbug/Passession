use super::command;
use anyhow::{Result, bail};
use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
};
use tokio_util::sync::CancellationToken;

/// Ask Git about each directory frontier before descending. Unlike a recursive
/// filesystem watch, ignored build/dependency trees are never traversed.
pub(super) async fn directories(
    root: &Path,
    env: &BTreeMap<String, String>,
    cancel: CancellationToken,
) -> Result<HashSet<PathBuf>> {
    let mut directories = HashSet::from([root.to_owned()]);
    let mut frontier = vec![root.to_owned()];
    while !frontier.is_empty() {
        let mut children = Vec::new();
        for directory in frontier {
            if cancel.is_cancelled() {
                bail!("Git watch inventory cancelled");
            }
            let mut entries = match tokio::fs::read_dir(&directory).await {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            while let Some(entry) = entries.next_entry().await? {
                if entry.file_name() != ".git" && entry.file_type().await?.is_dir() {
                    children.push(entry.path());
                }
            }
        }
        let mut ignored = HashSet::new();
        for chunk in children.chunks(128) {
            // `-z` needs `--stdin`, which the runner does not feed. Unquoted
            // newline output covers every name except ones Git must C-quote
            // (quotes, backslashes, control characters); such a directory is
            // not matched and stays watched, which only costs a watch.
            let mut args = vec![
                "-c".into(),
                "core.quotePath=false".into(),
                "check-ignore".into(),
                "--no-index".into(),
                "--".into(),
            ];
            args.extend(chunk.iter().map(|path| path.to_string_lossy().into_owned()));
            let result = command::run(
                root,
                &args,
                env,
                cancel.child_token(),
                command::READ_LIMIT,
                command::READ_TIMEOUT,
            )
            .await?;
            if result.status != 0 && result.status != 1 {
                bail!("{}", result.stderr);
            }
            ignored.extend(
                result
                    .stdout
                    .split(|byte| *byte == b'\n')
                    .filter(|path| !path.is_empty())
                    .map(|path| PathBuf::from(String::from_utf8_lossy(path).as_ref())),
            );
        }
        frontier = children
            .into_iter()
            .filter(|path| !ignored.contains(path))
            .collect();
        directories.extend(frontier.iter().cloned());
    }
    // Git still observes tracked files underneath an ignored directory.
    let tracked = command::checked(
        root,
        &["ls-files".into(), "--cached".into(), "-z".into()],
        env,
        cancel,
        command::READ_LIMIT,
        command::READ_TIMEOUT,
    )
    .await?;
    let mut tracked_directories = HashSet::new();
    for file in tracked
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let file = root.join(String::from_utf8_lossy(file).as_ref());
        for directory in file.parent().into_iter().flat_map(Path::ancestors) {
            if !directory.starts_with(root) {
                break;
            }
            if directories.contains(directory) || !tracked_directories.insert(directory.to_owned())
            {
                break;
            }
        }
    }
    for directory in tracked_directories {
        if tokio::fs::metadata(&directory)
            .await
            .is_ok_and(|metadata| metadata.is_dir())
        {
            directories.insert(directory);
        }
    }
    Ok(directories)
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
        let watched = directories(root, &env, cancel.clone()).await?;
        assert!(!watched.contains(&root.join("target")));
        assert!(!watched.contains(&root.join(".git")));
        assert!(watched.contains(&root.join("empty/nested")));
        assert!(watched.contains(&root.join("ignored/tracked")));
        tokio::fs::remove_dir_all(root.join("empty")).await?;
        assert!(
            !directories(root, &env, cancel)
                .await?
                .contains(&root.join("empty/nested"))
        );
        Ok(())
    }
}
