use super::super::tests::Repo;
use super::*;
use paseo_protocol::wire::{Direction, validate_session};

const EPOCH: i64 = 1_700_000_000;

/// Runs git with fixed author and committer dates so ordering is deterministic.
async fn at(repo: &Repo, offset: i64, values: &[&str]) -> Result<Vec<u8>> {
    let mut env = repo.env.clone();
    let date = format!("{} +0900", EPOCH + offset);
    env.insert("GIT_AUTHOR_DATE".into(), date.clone());
    env.insert("GIT_COMMITTER_DATE".into(), date);
    git(
        repo.path(),
        &args(values),
        &env,
        &CancellationToken::new(),
        INVENTORY_LIMIT,
    )
    .await
}

async fn commit_at(
    repo: &Repo,
    offset: i64,
    message: &str,
    author: Option<&str>,
) -> Result<String> {
    repo.git(&["add", "-A"]).await?;
    let author = author.map(|author| format!("--author={author}"));
    let mut command = vec!["-c", "commit.gpgsign=false", "commit", "-m", message];
    command.extend(author.as_deref());
    at(repo, offset, &command).await?;
    head(repo).await
}

async fn head(repo: &Repo) -> Result<String> {
    Ok(String::from_utf8(repo.git(&["rev-parse", "HEAD"]).await?)?
        .trim()
        .to_owned())
}

fn ids(values: &[&String]) -> Vec<String> {
    values.iter().map(|value| (*value).clone()).collect()
}

struct Fixture {
    repo: Repo,
    root: String,
    feature: String,
    rename: String,
    merge: String,
}

/// root ── rename ── merge (main, light)   origin/main = rename (v1)
///    └─ feature ──┘   (feature, upstream/dev)
async fn fixture() -> Result<Fixture> {
    let repo = Repo::new().await?;
    repo.write("a.txt", b"one\ntwo\nthree\nfour\n").await?;
    repo.write("keep.txt", b"keep\n").await?;
    let root = commit_at(&repo, 1000, "root", None).await?;
    repo.git(&["checkout", "-b", "feature"]).await?;
    repo.write("feature.txt", b"feature\n").await?;
    let feature = commit_at(
        &repo,
        2000,
        "feature work FIX",
        Some("Bob Builder <bob@example.invalid>"),
    )
    .await?;
    repo.git(&["checkout", "main"]).await?;
    repo.git(&["mv", "a.txt", "renamed.txt"]).await?;
    let rename = commit_at(&repo, 3000, "rename file fix", None).await?;
    at(
        &repo,
        4000,
        &[
            "-c",
            "commit.gpgsign=false",
            "merge",
            "--no-ff",
            "-m",
            "Merge feature",
            "-m",
            "Body line one\nBody line two",
            "feature",
        ],
    )
    .await?;
    let merge = head(&repo).await?;
    at(
        &repo,
        5000,
        &["-c", "tag.gpgsign=false", "tag", "-a", "v1", "-m", "release", &rename],
    )
    .await?;
    repo.git(&["tag", "light", &merge]).await?;
    repo.git(&["branch", "Zeta", &root]).await?;
    repo.git(&["branch", "gone", &root]).await?;
    repo.git(&["config", "branch.gone.remote", "origin"]).await?;
    repo.git(&["config", "branch.gone.merge", "refs/heads/missing"])
        .await?;
    repo.git(&["remote", "add", "origin", "https://example.invalid/origin.git"])
        .await?;
    repo.git(&["remote", "add", "upstream", "https://example.invalid/upstream.git"])
        .await?;
    repo.git(&["update-ref", "refs/remotes/origin/main", &rename])
        .await?;
    repo.git(&[
        "symbolic-ref",
        "refs/remotes/origin/HEAD",
        "refs/remotes/origin/main",
    ])
    .await?;
    repo.git(&["update-ref", "refs/remotes/upstream/dev", &feature])
        .await?;
    repo.git(&["branch", "--set-upstream-to=origin/main", "main"])
        .await?;
    Ok(Fixture {
        repo,
        root,
        feature,
        rename,
        merge,
    })
}

fn query() -> LogQuery<'static> {
    LogQuery {
        revision: None,
        skip: None,
        limit: None,
        text: None,
        author: None,
        path: None,
    }
}

impl Fixture {
    async fn log(&self, query: LogQuery<'_>) -> Result<Value> {
        log(
            self.repo.path(),
            &query,
            &self.repo.env,
            CancellationToken::new(),
        )
        .await
    }
    async fn shas(&self, query: LogQuery<'_>) -> Result<Vec<String>> {
        Ok(self.log(query).await?["commits"]
            .as_array()
            .context("commits")?
            .iter()
            .map(|commit| commit["sha"].as_str().unwrap().to_owned())
            .collect())
    }
    async fn commit(&self, sha: &str) -> Result<Value> {
        commit(self.repo.path(), sha, &self.repo.env, CancellationToken::new()).await
    }
}

/// The payload the session layer sends must satisfy the outbound schema.
fn assert_outbound(kind: &str, mut payload: Value) {
    payload["cwd"] = json!("/repo");
    payload["error"] = Value::Null;
    payload["requestId"] = json!("request");
    validate_session(&json!({"type":kind,"payload":payload}), Direction::Outbound)
        .unwrap_or_else(|error| panic!("{kind}: {error}"));
}

#[tokio::test]
async fn log_lists_all_refs_in_date_order_with_parents_and_decorations() -> Result<()> {
    let f = fixture().await?;
    let value = f.log(query()).await?;
    assert_eq!(value["head"], f.merge);
    assert_eq!(value["hasMore"], false);
    let commits = value["commits"].as_array().unwrap();
    let shas: Vec<&str> = commits.iter().map(|c| c["sha"].as_str().unwrap()).collect();
    assert_eq!(shas, [&f.merge, &f.rename, &f.feature, &f.root]);
    let merge = &commits[0];
    assert_eq!(merge["parents"], json!([f.rename, f.feature]));
    assert_eq!(merge["subject"], "Merge feature");
    assert_eq!(merge["authorName"], "Fixture");
    assert_eq!(merge["authorEmail"], "fixture@example.invalid");
    assert_eq!(merge["committerDate"], "2023-11-15T08:20:00+09:00");
    assert!(f.merge.starts_with(merge["shortSha"].as_str().unwrap()));
    assert_eq!(
        merge["refs"],
        json!([
            {"name":"main","kind":"local","isHead":true},
            {"name":"light","kind":"tag","isHead":false},
        ])
    );
    // origin/HEAD is dropped; the annotated tag decorates its commit.
    assert_eq!(
        commits[1]["refs"],
        json!([
            {"name":"origin/main","kind":"remote","isHead":false},
            {"name":"v1","kind":"tag","isHead":false},
        ])
    );
    assert_eq!(
        commits[2]["refs"],
        json!([
            {"name":"feature","kind":"local","isHead":false},
            {"name":"upstream/dev","kind":"remote","isHead":false},
        ])
    );
    assert_eq!(commits[2]["authorName"], "Bob Builder");
    assert_eq!(commits[3]["parents"], json!([]));
    assert_eq!(
        commits[3]["refs"],
        json!([
            {"name":"gone","kind":"local","isHead":false},
            {"name":"Zeta","kind":"local","isHead":false},
        ])
    );
    assert_outbound("checkout.log.response", value);

    f.repo.git(&["checkout", "--detach", &f.rename]).await?;
    let value = f.log(query()).await?;
    assert_eq!(value["head"], f.rename);
    assert_eq!(
        value["commits"][1]["refs"][0],
        json!({"name":"HEAD","kind":"head","isHead":true})
    );
    assert_eq!(
        value["commits"][0]["refs"][0],
        json!({"name":"main","kind":"local","isHead":false})
    );
    Ok(())
}

#[tokio::test]
async fn log_pages_and_filters_by_revision() -> Result<()> {
    let f = fixture().await?;
    let first = f
        .log(LogQuery {
            limit: Some(2),
            ..query()
        })
        .await?;
    assert_eq!(first["hasMore"], true);
    assert_eq!(first["commits"].as_array().unwrap().len(), 2);
    let second = f
        .log(LogQuery {
            skip: Some(2),
            limit: Some(2),
            ..query()
        })
        .await?;
    assert_eq!(second["hasMore"], false);
    assert_eq!(second["commits"][0]["sha"], f.feature);
    assert_eq!(second["commits"][1]["sha"], f.root);
    for (revision, expected) in [
        ("feature", ids(&[&f.feature, &f.root])),
        ("upstream/dev", ids(&[&f.feature, &f.root])),
        ("origin/main", ids(&[&f.rename, &f.root])),
        ("v1", ids(&[&f.rename, &f.root])),
        (&f.feature[..7], ids(&[&f.feature, &f.root])),
        ("HEAD", ids(&[&f.merge, &f.rename, &f.feature, &f.root])),
    ] {
        assert_eq!(
            f.shas(LogQuery {
                revision: Some(revision),
                ..query()
            })
            .await?,
            expected,
            "{revision}"
        );
    }
    for revision in ["missing", "main..feature", "main^", "-n", "main~1"] {
        let error = f
            .log(LogQuery {
                revision: Some(revision),
                ..query()
            })
            .await
            .unwrap_err();
        assert!(error.downcast_ref::<InvalidRevision>().is_some(), "{revision}");
        assert_eq!(crate::git::checkout_error(&error)["code"], "invalid_revision");
    }
    assert!(
        f.log(LogQuery {
            limit: Some(0),
            ..query()
        })
        .await
        .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn log_filters_combine_with_and() -> Result<()> {
    let f = fixture().await?;
    let text = |text| LogQuery {
        text: Some(text),
        ..query()
    };
    // Case-insensitive fixed-string matches: "fix" alone matches two commits,
    // "." is not a regular expression.
    assert_eq!(f.shas(text("FiX")).await?, ids(&[&f.rename, &f.feature]));
    assert_eq!(f.shas(text(".")).await?, ids(&[]));
    assert_eq!(
        f.shas(LogQuery {
            author: Some("BOB@example"),
            ..query()
        })
        .await?,
        ids(&[&f.feature])
    );
    // Either filter alone matches the other commit; together only the
    // commit satisfying both remains.
    assert_eq!(
        f.shas(LogQuery {
            text: Some("fix"),
            author: Some("fixture"),
            ..query()
        })
        .await?,
        ids(&[&f.rename])
    );
    assert_eq!(
        f.shas(LogQuery {
            text: Some("fix"),
            author: Some("bob"),
            ..query()
        })
        .await?,
        ids(&[&f.feature])
    );
    let path = |path| LogQuery {
        path: Some(path),
        ..query()
    };
    assert_eq!(f.shas(path("feature.txt")).await?, ids(&[&f.feature]));
    assert_eq!(f.shas(path("renamed.txt")).await?, ids(&[&f.rename]));
    assert_eq!(
        f.shas(LogQuery {
            path: Some("keep.txt"),
            author: Some("fixture"),
            ..query()
        })
        .await?,
        ids(&[&f.root])
    );
    assert_eq!(
        f.shas(LogQuery {
            path: Some("feature.txt"),
            text: Some("rename"),
            ..query()
        })
        .await?,
        ids(&[])
    );
    assert!(f.log(path("../outside")).await.is_err());
    assert!(f.log(path("/etc/passwd")).await.is_err());
    Ok(())
}

#[tokio::test]
async fn refs_list_branches_remotes_and_peeled_tags() -> Result<()> {
    let f = fixture().await?;
    let value = refs(f.repo.path(), &f.repo.env, CancellationToken::new()).await?;
    assert_eq!(
        value["head"],
        json!({"sha":f.merge,"branch":"main","detached":false})
    );
    let names = |key: &str| -> Vec<String> {
        value[key]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["name"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(names("local"), ["feature", "gone", "main", "Zeta"]);
    assert_eq!(names("remote"), ["origin/main", "upstream/dev"]);
    assert_eq!(names("tags"), ["light", "v1"]);
    let local = &value["local"];
    assert_eq!(
        local[2],
        json!({
            "name":"main","sha":f.merge,"upstream":"origin/main","ahead":2,"behind":0,
            "gone":false,"isCurrent":true,"committerDate":EPOCH + 4000,
        })
    );
    assert_eq!(local[0]["upstream"], Value::Null);
    assert_eq!(local[0]["ahead"], Value::Null);
    assert_eq!(local[0]["isCurrent"], false);
    assert_eq!(local[1]["gone"], true);
    assert_eq!(local[1]["ahead"], Value::Null);
    assert_eq!(
        value["remote"][0],
        json!({"name":"origin/main","remote":"origin","branch":"main","sha":f.rename,"committerDate":EPOCH + 3000})
    );
    assert_eq!(value["remote"][1]["remote"], "upstream");
    assert_eq!(value["remote"][1]["branch"], "dev");
    assert_eq!(
        value["tags"],
        json!([
            {"name":"light","sha":f.merge,"committerDate":EPOCH + 4000},
            {"name":"v1","sha":f.rename,"committerDate":EPOCH + 3000},
        ])
    );
    assert_outbound("checkout.refs.response", value);

    f.repo.git(&["checkout", "--detach", &f.rename]).await?;
    let value = refs(f.repo.path(), &f.repo.env, CancellationToken::new()).await?;
    assert_eq!(
        value["head"],
        json!({"sha":f.rename,"branch":null,"detached":true})
    );
    assert!(
        value["local"]
            .as_array()
            .unwrap()
            .iter()
            .all(|branch| branch["isCurrent"] == false)
    );
    Ok(())
}

#[tokio::test]
async fn commit_get_returns_details_and_first_parent_files() -> Result<()> {
    let f = fixture().await?;
    let merge = f.commit(&f.merge).await?;
    let detail = &merge["commit"];
    assert_eq!(detail["sha"], f.merge);
    assert_eq!(detail["parents"], json!([f.rename, f.feature]));
    assert_eq!(detail["subject"], "Merge feature");
    assert_eq!(detail["body"], "Body line one\nBody line two");
    assert_eq!(detail["committerName"], "Fixture");
    assert_eq!(detail["committerEmail"], "fixture@example.invalid");
    assert_eq!(detail["authorDate"], "2023-11-15T08:20:00+09:00");
    assert_eq!(detail["refs"][0], json!({"name":"main","kind":"local","isHead":true}));
    assert_eq!(
        detail["files"],
        json!([{"path":"feature.txt","oldPath":null,"additions":1,"deletions":0,"status":"added"}])
    );
    assert_outbound("checkout.commit.get.response", merge);

    let rename = f.commit(&f.rename[..8]).await?;
    assert_eq!(rename["commit"]["sha"], f.rename);
    assert_eq!(rename["commit"]["body"], "");
    assert_eq!(
        rename["commit"]["files"],
        json!([{"path":"renamed.txt","oldPath":"a.txt","additions":0,"deletions":0,"status":"renamed"}])
    );
    let root = f.commit(&f.root).await?;
    assert_eq!(root["commit"]["parents"], json!([]));
    assert_eq!(
        root["commit"]["files"],
        json!([
            {"path":"a.txt","oldPath":null,"additions":4,"deletions":0,"status":"added"},
            {"path":"keep.txt","oldPath":null,"additions":1,"deletions":0,"status":"added"},
        ])
    );
    assert_outbound("checkout.commit.get.response", root);

    let missing = f.commit("deadbeefdeadbeef").await?;
    assert_eq!(missing, json!({"commit":null}));
    assert_outbound("checkout.commit.get.response", missing);
    for invalid in ["abc", "main", "HEAD~1", &"a".repeat(65)] {
        assert!(f.commit(invalid).await.is_err(), "{invalid}");
    }
    Ok(())
}

#[tokio::test]
async fn unborn_repository_has_empty_history() -> Result<()> {
    let repo = Repo::new().await?;
    let value = log(repo.path(), &query(), &repo.env, CancellationToken::new()).await?;
    assert_eq!(value, json!({"head":null,"commits":[],"hasMore":false}));
    assert_outbound("checkout.log.response", value);
    let value = refs(repo.path(), &repo.env, CancellationToken::new()).await?;
    assert_eq!(
        value,
        json!({
            "head":{"sha":null,"branch":"main","detached":false},
            "local":[],"remote":[],"tags":[],
        })
    );
    assert_outbound("checkout.refs.response", value);
    Ok(())
}

#[test]
fn request_schemas_accept_dashboard_requests() {
    for request in [
        json!({"type":"checkout.log.request","cwd":"/repo","requestId":"r"}),
        json!({
            "type":"checkout.log.request","cwd":"/repo","requestId":"r","revision":"origin/main",
            "skip":300,"limit":300,"text":"fix","author":null,"path":"src/lib.rs",
        }),
        json!({"type":"checkout.refs.request","cwd":"/repo","requestId":"r"}),
        json!({"type":"checkout.commit.get.request","cwd":"/repo","requestId":"r","sha":"abcd"}),
    ] {
        validate_session(&request, Direction::Inbound).unwrap();
    }
    for request in [
        json!({"type":"checkout.log.request","cwd":"/repo","requestId":"r","limit":0}),
        json!({"type":"checkout.log.request","cwd":"/repo","requestId":"r","limit":1001}),
        json!({"type":"checkout.log.request","cwd":"/repo","requestId":"r","skip":-1}),
        json!({"type":"checkout.commit.get.request","cwd":"/repo","requestId":"r","sha":"xyz1"}),
        json!({"type":"checkout.commit.get.request","cwd":"/repo","requestId":"r"}),
    ] {
        assert!(validate_session(&request, Direction::Inbound).is_err(), "{request}");
    }
}
