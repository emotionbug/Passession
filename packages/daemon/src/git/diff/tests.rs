use super::*;

pub(super) struct Repo {
    directory: tempfile::TempDir,
    pub(super) env: BTreeMap<String, String>,
}
impl Repo {
    pub(super) async fn new() -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let mut env = BTreeMap::new();
        env.insert("GIT_CONFIG_NOSYSTEM".into(), "1".into());
        env.insert("GIT_CONFIG_GLOBAL".into(), null_device().into());
        env.insert(
            "HOME".into(),
            directory.path().to_string_lossy().into_owned(),
        );
        env.insert("GIT_AUTHOR_NAME".into(), "Fixture".into());
        env.insert("GIT_AUTHOR_EMAIL".into(), "fixture@example.invalid".into());
        env.insert("GIT_COMMITTER_NAME".into(), "Fixture".into());
        env.insert(
            "GIT_COMMITTER_EMAIL".into(),
            "fixture@example.invalid".into(),
        );
        let repo = Self { directory, env };
        repo.git(&["init", "-b", "main"]).await?;
        Ok(repo)
    }
    pub(super) fn path(&self) -> &Path {
        self.directory.path()
    }
    pub(super) async fn git(&self, values: &[&str]) -> Result<Vec<u8>> {
        git(
            self.path(),
            &args(values),
            &self.env,
            &CancellationToken::new(),
            INVENTORY_LIMIT,
        )
        .await
    }
    pub(super) async fn write(&self, path: &str, bytes: &[u8]) -> Result<()> {
        tokio::fs::write(self.path().join(path), bytes).await?;
        Ok(())
    }
    pub(super) async fn commit(&self, message: &str) -> Result<String> {
        self.git(&["add", "-A"]).await?;
        self.git(&["-c", "commit.gpgsign=false", "commit", "-m", message])
            .await?;
        Ok(String::from_utf8(self.git(&["rev-parse", "HEAD"]).await?)?
            .trim()
            .to_owned())
    }
    async fn diff(&self, compare: Value) -> Result<Value> {
        read(self.path(), &compare, &self.env, CancellationToken::new()).await
    }
}

#[tokio::test]
async fn unborn_summary_full_binary_and_whitespace() -> Result<()> {
    let repo = Repo::new().await?;
    repo.write("new.txt", b"first\nlast").await?;
    repo.write("binary.dat", b"a\0b").await?;
    let summary = repo
        .diff(json!({"mode":"uncommitted","detail":"summary"}))
        .await?;
    assert_eq!(summary["error"], Value::Null);
    assert_eq!(summary["files"][0]["status"], "binary");
    assert_eq!(summary["files"][1]["additions"], 2);
    assert_eq!(summary["files"][1]["hunks"], json!([]));
    let full = repo.diff(json!({"mode":"uncommitted"})).await?;
    assert_eq!(full["files"][1]["hunks"][0]["lines"][2]["content"], "last");
    repo.commit("initial").await?;
    repo.write("new.txt", b" first\nlast   ").await?;
    assert_eq!(
        repo.diff(json!({"mode":"uncommitted","ignoreWhitespace":true}))
            .await?["files"],
        json!([])
    );
    Ok(())
}

#[tokio::test]
async fn root_commit_rename_and_binary_file_reads() -> Result<()> {
    let repo = Repo::new().await?;
    // Git quotes these paths outside NUL-separated output. Windows forbids
    // control characters in file names, so it renames spaced non-ASCII names.
    let (old, new) = if cfg!(windows) {
        ("old file name.txt", "new file nämé.txt")
    } else {
        ("old file\tname.txt", "new file\nname.txt")
    };
    repo.write(old, b"one\ntwo\nthree\nfour\n").await?;
    repo.write("binary.dat", b"\0binary").await?;
    let root = repo.commit("root").await?;
    let entry = commit_file(repo.path(), &root, old, &repo.env, CancellationToken::new())
        .await?
        .unwrap();
    assert_eq!(entry["isNew"], true);
    assert_eq!(entry["additions"], 4);
    assert!(
        commit_file(
            repo.path(),
            &root,
            "binary.dat",
            &repo.env,
            CancellationToken::new()
        )
        .await?
        .is_none()
    );
    repo.git(&["mv", old, new]).await?;
    let rename = repo.commit("rename").await?;
    let entry = commit_file(
        repo.path(),
        &rename,
        new,
        &repo.env,
        CancellationToken::new(),
    )
    .await?
    .unwrap();
    assert_eq!(entry["oldPath"], old);
    assert_eq!(entry["path"], new);
    assert!(!entry["isNew"].as_bool().unwrap());
    assert!(
        commit_file(
            repo.path(),
            &rename,
            "absent",
            &repo.env,
            CancellationToken::new()
        )
        .await?
        .is_none()
    );
    let history = list_commits(repo.path(), &repo.env, CancellationToken::new()).await?;
    assert_eq!(history["commits"][0]["files"][0]["status"], "renamed");
    assert_eq!(history["commits"][0]["isOnRemote"], false);
    Ok(())
}

#[tokio::test]
async fn all_comparison_restores_only_private_index() -> Result<()> {
    let repo = Repo::new().await?;
    repo.write("restored.txt", b"original\n").await?;
    repo.commit("base").await?;
    repo.git(&["checkout", "-b", "feature"]).await?;
    repo.git(&["rm", "restored.txt"]).await?;
    repo.commit("delete").await?;
    repo.write("restored.txt", b"changed\n").await?;
    let index = repo.path().join(".git/index");
    let before = tokio::fs::read(&index).await?;
    let result = repo.diff(json!({"mode":"all","baseRef":"main"})).await?;
    assert_eq!(result["error"], Value::Null);
    assert_eq!(result["files"].as_array().unwrap().len(), 1);
    assert_eq!(result["files"][0]["isNew"], false);
    assert_eq!(result["files"][0]["additions"], 1);
    assert_eq!(result["files"][0]["deletions"], 1);
    assert_eq!(tokio::fs::read(&index).await?, before);
    Ok(())
}

#[tokio::test]
async fn budgets_keep_small_neighbors_and_real_numstat() -> Result<()> {
    let repo = Repo::new().await?;
    repo.write("a-huge", b"old\n").await?;
    repo.write("z-small", b"old\n").await?;
    repo.commit("base").await?;
    repo.write("a-huge", &vec![b'x'; FILE_LIMIT + 1]).await?;
    repo.write("z-small", b"new\n").await?;
    let result = repo.diff(json!({"mode":"uncommitted"})).await?;
    assert_eq!(result["error"], Value::Null);
    assert_eq!(result["files"][0]["status"], "too_large");
    assert_eq!(result["files"][0]["additions"], 1);
    assert_eq!(result["files"][1]["status"], "ok");
    assert_eq!(result["files"][1]["hunks"][0]["lines"][2]["content"], "new");
    Ok(())
}

#[tokio::test]
async fn merge_commit_file_uses_first_parent() -> Result<()> {
    let repo = Repo::new().await?;
    repo.write("base", b"base\n").await?;
    repo.commit("base").await?;
    repo.git(&["checkout", "-b", "side"]).await?;
    repo.write("side", b"side\n").await?;
    repo.commit("side").await?;
    repo.git(&["checkout", "main"]).await?;
    repo.write("main", b"main\n").await?;
    repo.commit("main").await?;
    repo.git(&[
        "-c",
        "commit.gpgsign=false",
        "merge",
        "--no-ff",
        "-m",
        "merge",
        "side",
    ])
    .await?;
    let entry = commit_file(
        repo.path(),
        "HEAD",
        "side",
        &repo.env,
        CancellationToken::new(),
    )
    .await?
    .unwrap();
    assert_eq!(entry["isNew"], true);
    assert_eq!(entry["additions"], 1);
    assert!(
        commit_file(
            repo.path(),
            "HEAD",
            "main",
            &repo.env,
            CancellationToken::new()
        )
        .await?
        .is_none()
    );
    Ok(())
}

#[test]
fn nul_records_preserve_rename_and_tab_newline_paths() -> Result<()> {
    let changes = parse::changes(b"R100\0old\tname\0new\nname\0M\0ordinary\0")?;
    assert_eq!(changes[0].path, "new\nname");
    assert_eq!(changes[0].old_path.as_deref(), Some("old\tname"));
    let stats = parse::stats(b"1\t2\t\0old\tname\0new\nname\0-\t-\tbinary\0")?;
    assert_eq!(stats["new\nname"].deletions, 2);
    assert!(stats["binary"].binary);
    Ok(())
}

#[tokio::test]
async fn aggregate_budget_keeps_later_small_files_and_cancellation_propagates() -> Result<()> {
    let repo = Repo::new().await?;
    for name in ["a", "b", "c"] {
        repo.write(name, &vec![b'x'; 750_000]).await?;
    }
    repo.write("z", b"small\n").await?;
    let result = repo.diff(json!({"mode":"uncommitted"})).await?;
    assert_eq!(result["error"], Value::Null);
    assert_eq!(result["files"][0]["status"], "ok");
    assert_eq!(result["files"][1]["status"], "ok");
    assert_eq!(result["files"][2]["status"], "too_large");
    assert_eq!(result["files"][3]["status"], "ok");
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(
        read(
            repo.path(),
            &json!({"mode":"uncommitted"}),
            &repo.env,
            cancel
        )
        .await
        .is_err()
    );
    Ok(())
}

/// The checkout snapshot's `diffStat`, produced from its resolved root, branch and base.
async fn snapshot_diff_stat(repo: &Repo) -> Result<Value> {
    let cancel = CancellationToken::new();
    let mut snapshot = crate::git::facts::ObservedRepo::read(
        repo.path(),
        repo.path(),
        &json!({}),
        &repo.env,
        &cancel,
    )
    .await?
    .unwrap();
    Ok(snapshot.diff_stat(&repo.env, cancel).await)
}

#[tokio::test]
async fn snapshot_diff_stat_requires_comparison_and_counts_untracked_text() -> Result<()> {
    let repo = Repo::new().await?;
    repo.write("tracked", b"old\n").await?;
    repo.commit("base").await?;
    repo.write("tracked", b"new\nmore\n").await?;
    repo.write("untracked", b"one\ntwo").await?;
    repo.write("binary", b"\0binary").await?;
    // On the base branch with no origin counterpart there is nothing to compare.
    assert_eq!(snapshot_diff_stat(&repo).await?, Value::Null);
    repo.git(&["update-ref", "refs/remotes/origin/main", "HEAD"])
        .await?;
    // Tracked +2/-1 against origin/main, plus two untracked text lines; binary adds none.
    assert_eq!(
        snapshot_diff_stat(&repo).await?,
        json!({"additions":4,"deletions":1})
    );
    repo.commit("changes").await?;
    repo.git(&["update-ref", "refs/remotes/origin/main", "HEAD"])
        .await?;
    assert_eq!(snapshot_diff_stat(&repo).await?, Value::Null);
    Ok(())
}

#[tokio::test]
async fn untracked_embedded_repositories_do_not_hide_checkout_files() -> Result<()> {
    let repo = Repo::new().await?;
    repo.write("tracked", b"base\n").await?;
    repo.commit("base").await?;
    repo.git(&["update-ref", "refs/remotes/origin/main", "HEAD"])
        .await?;
    repo.write("tracked", b"staged\n").await?;
    repo.git(&["add", "tracked"]).await?;
    repo.write("tracked", b"working\n").await?;
    tokio::fs::create_dir_all(repo.path().join("tmp/ordinary/deep")).await?;
    repo.write("tmp/ordinary/deep/new.txt", b"new\n").await?;
    repo.git(&["init", "-b", "main", "tmp/embedded"]).await?;
    repo.write("tmp/embedded/inner.txt", b"nested\n").await?;
    repo.git(&["-C", "tmp/embedded", "add", "inner.txt"])
        .await?;
    repo.git(&[
        "-C",
        "tmp/embedded",
        "-c",
        "commit.gpgsign=false",
        "commit",
        "-m",
        "nested",
    ])
    .await?;
    repo.write("tmp/embedded/inner.txt", b"nested dirty\n")
        .await?;
    repo.git(&["init", "-b", "main", "tmp/unborn"]).await?;
    repo.write("tmp/unborn/private.txt", b"not a parent file\n")
        .await?;
    let inventory = repo
        .git(&["ls-files", "-z", "--others", "--exclude-standard"])
        .await?;
    let paths: Vec<_> = inventory.split(|byte| *byte == 0).collect();
    assert!(paths.contains(&b"tmp/embedded/".as_slice()));
    assert!(paths.contains(&b"tmp/unborn/".as_slice()));
    assert!(paths.contains(&b"tmp/ordinary/deep/new.txt".as_slice()));
    let index = tokio::fs::read(repo.path().join(".git/index")).await?;
    let nested_index = tokio::fs::read(repo.path().join("tmp/embedded/.git/index")).await?;
    let nested_head = repo
        .git(&["-C", "tmp/embedded", "rev-parse", "HEAD"])
        .await?;
    for mode in ["uncommitted", "all"] {
        for detail in ["summary", "full"] {
            let result = repo
                .diff(json!({
                    "mode":mode, "detail":detail, "baseRef":"main",
                }))
                .await?;
            assert_eq!(result["error"], Value::Null, "{result}");
            let files = result["files"].as_array().unwrap();
            assert_eq!(files.len(), 2, "{result}");
            assert_eq!(files[0]["path"], "tmp/ordinary/deep/new.txt");
            assert_eq!(files[0]["additions"], 1);
            assert_eq!(files[1]["path"], "tracked");
            assert_eq!(files[1]["additions"], 1);
            assert_eq!(files[1]["deletions"], 1);
            if detail == "full" {
                assert_eq!(files[0]["hunks"][0]["lines"][1]["content"], "new");
            } else {
                assert_eq!(files[0]["hunks"], json!([]));
            }
        }
    }
    assert_eq!(
        snapshot_diff_stat(&repo).await?,
        json!({"additions":2,"deletions":1})
    );
    assert_eq!(
        tokio::fs::read(repo.path().join(".git/index")).await?,
        index
    );
    assert_eq!(
        tokio::fs::read(repo.path().join("tmp/embedded/.git/index")).await?,
        nested_index,
    );
    assert_eq!(
        repo.git(&["-C", "tmp/embedded", "rev-parse", "HEAD"])
            .await?,
        nested_head
    );
    assert_eq!(
        tokio::fs::read(repo.path().join("tracked")).await?,
        b"working\n"
    );
    assert_eq!(
        tokio::fs::read(repo.path().join("tmp/embedded/inner.txt")).await?,
        b"nested dirty\n",
    );
    assert_eq!(
        tokio::fs::read(repo.path().join("tmp/unborn/private.txt")).await?,
        b"not a parent file\n",
    );
    assert!(!repo.path().join("tmp/unborn/.git/index").exists());
    Ok(())
}

#[tokio::test]
async fn untracked_disappearing_entries_and_non_files_are_not_diff_errors() -> Result<()> {
    let repo = Repo::new().await?;
    tokio::fs::create_dir(repo.path().join("directory")).await?;
    repo.write("vanished", b"removed after inventory\n").await?;
    tokio::fs::remove_file(repo.path().join("vanished")).await?;
    for path in ["directory", "directory/", "vanished"] {
        let change = Change {
            path: path.into(),
            old_path: None,
            status: 'A',
            untracked: true,
        };
        for summary in [true, false] {
            assert!(
                untracked_file(
                    repo.path(),
                    &change,
                    summary,
                    &mut 0,
                    &parse::PatchFormat::default(),
                    &CancellationToken::new(),
                )
                .await?
                .is_none()
            );
        }
    }
    #[cfg(unix)]
    {
        // ENOTDIR is not a disappeared leaf. Keep unrelated filesystem errors.
        repo.write("ordinary", b"file, not directory\n").await?;
        let change = Change {
            path: "ordinary/child".into(),
            old_path: None,
            status: 'A',
            untracked: true,
        };
        assert!(
            untracked_file(
                repo.path(),
                &change,
                true,
                &mut 0,
                &parse::PatchFormat::default(),
                &CancellationToken::new(),
            )
            .await
            .is_err()
        );
    }
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn untracked_special_files_are_skipped_and_symlinks_are_not_followed() -> Result<()> {
    use std::os::unix::{ffi::OsStrExt, fs::symlink, net::UnixListener};

    let repo = Repo::new().await?;
    repo.write("tracked", b"base\n").await?;
    repo.commit("base").await?;
    repo.git(&["update-ref", "refs/remotes/origin/main", "HEAD"])
        .await?;
    let fifo = std::ffi::CString::new(repo.path().join("fifo").as_os_str().as_bytes())?;
    // A FIFO without any writer would block if opened as a regular file.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    let _socket = UnixListener::bind(repo.path().join("socket"))?;
    let outside = tempfile::NamedTempFile::new()?;
    tokio::fs::write(outside.path(), b"must not read target contents\n").await?;
    symlink(outside.path(), repo.path().join("link"))?;
    symlink("absent", repo.path().join("dangling"))?;
    symlink("fifo", repo.path().join("fifo-link"))?;
    for path in ["fifo", "socket"] {
        let change = Change {
            path: path.into(),
            old_path: None,
            status: 'A',
            untracked: true,
        };
        assert!(
            untracked_file(
                repo.path(),
                &change,
                false,
                &mut 0,
                &parse::PatchFormat::default(),
                &CancellationToken::new(),
            )
            .await?
            .is_none()
        );
    }
    for detail in ["summary", "full"] {
        let result = repo
            .diff(json!({"mode":"uncommitted","detail":detail}))
            .await?;
        assert_eq!(result["error"], Value::Null, "{result}");
        let files = result["files"].as_array().unwrap();
        assert_eq!(files.len(), 3, "{result}");
        for (file, (path, target)) in files.iter().zip([
            ("dangling", "absent"),
            ("fifo-link", "fifo"),
            ("link", outside.path().to_str().unwrap()),
        ]) {
            assert_eq!(file["path"], path);
            assert_eq!(file["additions"], 1);
            if detail == "full" {
                assert_eq!(file["hunks"][0]["lines"][1]["content"], target);
            }
        }
    }
    assert_eq!(
        snapshot_diff_stat(&repo).await?,
        json!({"additions":3,"deletions":0})
    );
    Ok(())
}

#[tokio::test]
async fn uncommitted_entries_mark_only_untracked_files() -> Result<()> {
    let repo = Repo::new().await?;
    repo.write("tracked", b"old\n").await?;
    repo.commit("base").await?;
    repo.write("tracked", b"new\n").await?;
    repo.write("staged", b"staged\n").await?;
    repo.git(&["add", "staged"]).await?;
    repo.write("untracked", b"one\n").await?;
    for detail in ["summary", "full"] {
        let result = repo
            .diff(json!({"mode":"uncommitted","detail":detail}))
            .await?;
        assert_eq!(result["error"], Value::Null, "{result}");
        let files = result["files"].as_array().unwrap();
        let entry = |path: &str| files.iter().find(|f| f["path"] == path).unwrap().clone();
        // Staged additions and untracked files are both new; only the latter is untracked.
        assert_eq!(entry("staged")["isNew"], true);
        assert!(entry("staged").get("untracked").is_none());
        assert!(entry("tracked").get("untracked").is_none());
        assert_eq!(entry("untracked")["isNew"], true);
        assert_eq!(entry("untracked")["untracked"], true);
    }
    Ok(())
}
