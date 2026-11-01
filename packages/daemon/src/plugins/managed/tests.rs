use super::*;

#[test]
fn source_references_preserve_transport_colons_and_registry_identity() {
    assert_eq!(
        reference::split("git@host:repository.git"),
        ("git@host:repository.git".into(), None)
    );
    assert_eq!(
        reference::split("git@host:repository.git:plugins/example"),
        (
            "git@host:repository.git".into(),
            Some("plugins/example".into())
        )
    );
    assert_eq!(
        reference::split("https://host:443/repository.git:plugins/example"),
        (
            "https://host:443/repository.git".into(),
            Some("plugins/example".into())
        )
    );
    assert_eq!(
        reference::split("cargo:example-plugin@1.2.3:plugin"),
        ("cargo:example-plugin@1.2.3".into(), Some("plugin".into()))
    );
    assert_eq!(
        reference::split("C:\\plugins\\example"),
        ("C:\\plugins\\example".into(), None)
    );
    assert_eq!(
        reference::registry("acme/example", "https://plugins.example/internal/").unwrap(),
        Some(json!({"url":"https://plugins.example/internal","id":"acme/example"}))
    );
    assert!(
        reference::registry("github:acme/example", "https://plugins.example")
            .unwrap()
            .is_none()
    );
    assert_eq!(
        split_source_reference("registry.example:8443/acme/example").unwrap(),
        ("registry.example:8443/acme/example".into(), None)
    );
    assert!(plugin_path(Some("../escape")).is_err());
    assert!(plugin_path(Some("nested/../../escape")).is_err());
}

#[tokio::test]
async fn corrupt_registry_is_not_silently_replaced() {
    let home = tempfile::tempdir().unwrap();
    tokio::fs::create_dir(home.path().join("plugins"))
        .await
        .unwrap();
    let path = home.path().join("plugins/sources.json");
    tokio::fs::write(&path, b"{broken").await.unwrap();
    let sources = ManagedSources::new(home.path().into(), BTreeMap::new());
    assert!(sources.initialize().await.is_err());
    assert_eq!(tokio::fs::read(&path).await.unwrap(), b"{broken");
}

#[tokio::test]
async fn registry_persists_before_publishing_and_survives_restart() {
    let home = tempfile::tempdir().unwrap();
    let sources = ManagedSources::new(home.path().into(), BTreeMap::new());
    sources.initialize().await.unwrap();
    let record = json!({"remote":"file:///repository","revision":"obsolete","root":"obsolete"});
    sources.commit("example", &record).await.unwrap();
    let restarted = ManagedSources::new(home.path().into(), BTreeMap::new());
    restarted.initialize().await.unwrap();
    assert_eq!(
        restarted.record("example").await.unwrap(),
        Some(json!({"kind":"git","remote":"file:///repository"}))
    );
    tokio::fs::remove_file(home.path().join("plugins/sources.json"))
        .await
        .unwrap();
    tokio::fs::create_dir(home.path().join("plugins/sources.json"))
        .await
        .unwrap();
    assert!(
        sources
            .commit("second", &json!({"kind":"cargo","crateName":"example-plugin"}))
            .await
            .is_err()
    );
    assert!(sources.record("second").await.unwrap().is_none());
    assert!(sources.record("example").await.unwrap().is_some());
}

#[tokio::test]
async fn ownership_cannot_be_conferred_on_other_plugins_or_local_directories() {
    let home = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let sources = ManagedSources::new(home.path().into(), BTreeMap::new());
    sources.initialize().await.unwrap();
    sources
        .commit(
            "example",
            &json!({"kind":"git","remote":"file:///repository"}),
        )
        .await
        .unwrap();
    let other = sources
        .root
        .join("other")
        .join(Uuid::new_v4().to_string())
        .join("checkout");
    tokio::fs::create_dir_all(&other).await.unwrap();
    assert!(sources.remove_version("example", &other).await.is_err());
    assert!(tokio::fs::metadata(&other).await.unwrap().is_dir());
    assert!(
        sources
            .remove_version("example", outside.path())
            .await
            .is_err()
    );
    assert!(tokio::fs::metadata(outside.path()).await.unwrap().is_dir());
}

#[tokio::test]
async fn cargo_revision_and_proposal_are_bound_to_acquisition_artifacts() {
    let home = tempfile::tempdir().unwrap();
    let sources = ManagedSources::new(home.path().into(), BTreeMap::new());
    sources.initialize().await.unwrap();
    let version = sources.root.join("example").join(Uuid::new_v4().to_string());
    let package = version.join("crate");
    tokio::fs::create_dir_all(&version).await.unwrap();
    let bytes = tar_fixture("fixture-1.2.3/paseo-plugin.json", NATIVE_MANIFEST);
    cargo::extract(&bytes, &package, "fixture-1.2.3").unwrap();
    let artifact = cargo_target("fixture", "1.2.3", &bytes);
    tokio::fs::write(version.join("source.crate"), &bytes).await.unwrap();
    tokio::fs::write(version.join("artifact.json"), serde_json::to_vec(&artifact).unwrap()).await.unwrap();
    sources.commit("example", &json!({"kind":"cargo","crateName":"fixture"})).await.unwrap();
    let current = sources.describe("example", &package).await.unwrap();
    assert_eq!(current["currentRevision"], "1.2.3");
    assert_eq!(current["artifact"], artifact);
    let proposal = json!({"id":"example","expected":{"identity":current["identity"],"installationRoot":version,"revision":"1.2.3","artifact":artifact},"target":cargo_target("fixture","1.2.4",b"next")});
    sources.assert_current(&proposal, &package).await.unwrap();
    // Even a self-consistent replacement archive at the same version invalidates review.
    let replacement = tar_fixture("fixture-1.2.3/paseo-plugin.json", b"changed");
    tokio::fs::write(version.join("source.crate"), &replacement).await.unwrap();
    tokio::fs::write(version.join("artifact.json"), serde_json::to_vec(&cargo_target("fixture","1.2.3",&replacement)).unwrap()).await.unwrap();
    assert_eq!(sources.describe("example",&package).await.unwrap()["currentRevision"], "1.2.3");
    assert!(sources.assert_current(&proposal, &package).await.is_err());
    // A digest mismatch never becomes a usable current revision.
    tokio::fs::write(version.join("source.crate"), b"corrupt").await.unwrap();
    assert!(sources.describe("example", &package).await.unwrap().get("currentRevision").is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn symlinked_plugin_subdirectories_cannot_escape_acquisition_root() {
    let home = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let root = home.path().join("checkout");
    tokio::fs::create_dir(&root).await.unwrap();
    std::os::unix::fs::symlink(outside.path(), root.join("plugin")).unwrap();
    assert!(containment(&root, &root.join("plugin")).await.is_err());
}

#[tokio::test]
async fn git_updates_install_reviewed_commit_and_reject_replaced_generation() {
    let fixture = tempfile::tempdir().unwrap();
    let repository = fixture.path().join("repository");
    tokio::fs::create_dir(&repository).await.unwrap();
    let global = fixture.path().join("gitconfig");
    tokio::fs::write(&global, b"").await.unwrap();
    let mut env: BTreeMap<String, String> = std::env::vars()
        .filter(|(key, _)| {
            matches!(
                key.as_str(),
                "PATH" | "PATHEXT" | "SYSTEMROOT" | "SystemRoot" | "TEMP" | "TMP"
            )
        })
        .collect();
    env.insert("HOME".into(), fixture.path().to_string_lossy().into_owned());
    env.insert("GIT_CONFIG_NOSYSTEM".into(), "1".into());
    env.insert(
        "GIT_CONFIG_GLOBAL".into(),
        global.to_string_lossy().into_owned(),
    );
    env.insert("GIT_AUTHOR_NAME".into(), "Plugin fixture".into());
    env.insert("GIT_AUTHOR_EMAIL".into(), "fixture@example.invalid".into());
    env.insert("GIT_COMMITTER_NAME".into(), "Plugin fixture".into());
    env.insert(
        "GIT_COMMITTER_EMAIL".into(),
        "fixture@example.invalid".into(),
    );
    let run_git = |args: &[&str]| {
        command(
            "git",
            args.iter().map(|s| (*s).to_owned()).collect(),
            &repository,
            env.clone(),
        )
    };
    run_git(&["init", "-b", "main"]).await.unwrap();
    tokio::fs::write(repository.join("paseo-plugin.json"), NATIVE_MANIFEST)
        .await
        .unwrap();
    run_git(&["add", "."]).await.unwrap();
    run_git(&["commit", "-m", "initial"]).await.unwrap();
    let sources = ManagedSources::new(fixture.path().join("daemon"), env.clone());
    sources.initialize().await.unwrap();
    let remote = url::Url::from_directory_path(&repository)
        .unwrap()
        .to_string();
    let installed = sources
        .place(
            "fixture",
            sources
                .prepare_install(&json!({"source":remote}))
                .await
                .unwrap(),
        )
        .await
        .unwrap();
    sources.verify("fixture", &installed, None).await.unwrap();
    sources.commit("fixture", &installed.record).await.unwrap();
    tokio::fs::write(repository.join("content.txt"), b"reviewed")
        .await
        .unwrap();
    run_git(&["add", "."]).await.unwrap();
    run_git(&["commit", "-m", "reviewed"]).await.unwrap();
    let preview = sources
        .preview("fixture", &installed.directory, None)
        .await
        .unwrap();
    assert_eq!(preview["outcome"], "update");
    let reviewed = preview["target"]["commit"].as_str().unwrap().to_owned();
    tokio::fs::write(repository.join("content.txt"), b"newer-unreviewed")
        .await
        .unwrap();
    run_git(&["add", "."]).await.unwrap();
    run_git(&["commit", "-m", "unreviewed"]).await.unwrap();
    let update = sources
        .prepare_update(&preview["proposal"], &installed.directory)
        .await
        .unwrap();
    let update = sources.place("fixture", update).await.unwrap();
    sources.verify("fixture", &update, None).await.unwrap();
    assert_eq!(
        git::rev_parse(&update.directory, "HEAD", &env)
            .await
            .unwrap(),
        reviewed
    );
    assert_eq!(
        tokio::fs::read(update.directory.join("content.txt"))
            .await
            .unwrap(),
        b"reviewed"
    );
    assert!(
        sources
            .assert_current(&preview["proposal"], &update.directory)
            .await
            .is_err()
    );
    sources.discard(&update).await.unwrap();
    assert!(tokio::fs::metadata(&update.directory).await.is_err());
    assert!(
        tokio::fs::metadata(&installed.directory)
            .await
            .unwrap()
            .is_dir()
    );
    sources.remove("fixture").await.unwrap();
    assert!(sources.record("fixture").await.unwrap().is_none());
    assert!(tokio::fs::metadata(&installed.directory).await.is_err());
}

const NATIVE_MANIFEST: &[u8] = br#"{"id":"fixture","runtime":"native","server":{"executable":"target/release/fixture","args":[]},"build":[["cargo","build","--release","--target-dir","target"]]}"#;

fn tar_fixture(path: &str, contents: &[u8]) -> Vec<u8> {
    let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut archive = tar::Builder::new(encoder);
    let mut header = tar::Header::new_gnu();
    header.set_size(contents.len() as u64);
    header.set_mode(0o600);
    header.set_cksum();
    archive.append_data(&mut header, path, contents).unwrap();
    archive.into_inner().unwrap().finish().unwrap()
}

fn cargo_target(name: &str, version: &str, bytes: &[u8]) -> Value {
    use sha2::{Digest, Sha256};
    json!({"kind":"cargo","version":version,"checksum":hex::encode(Sha256::digest(bytes)),"resolved":cargo::download_url(name,version)})
}

#[test]
fn cargo_versions_select_stable_non_yanked_and_require_exact_pins() {
    let checksum = "ab".repeat(32);
    let document = json!({"crate":{"id":"fixture"},"versions":[
        {"num":"2.0.0","yanked":true,"checksum":checksum},
        {"num":"3.0.0-alpha.1","yanked":false,"checksum":checksum},
        {"num":"1.2.3","yanked":false,"checksum":checksum},
        {"num":"1.2.2","yanked":false,"checksum":checksum}
    ]});
    assert_eq!(cargo::select("fixture",&document,None).unwrap()["version"], "1.2.3");
    assert_eq!(cargo::select("fixture",&document,Some(&semver::Version::parse("3.0.0-alpha.1").unwrap())).unwrap()["version"], "3.0.0-alpha.1");
    assert!(cargo::select("fixture",&document,Some(&semver::Version::parse("2.0.0").unwrap())).is_err());
    assert!(cargo::select("other",&document,None).is_err());
    for source in ["cargo:../fixture", "cargo:fixture@latest", "cargo:fixture@^1", "cargo:-fixture", "cargo:fixture@"] {
        assert!(cargo::parse(source).is_err(), "{source}");
    }
    assert!(normalize_record(&json!({"kind":"cargo","crateName":"fixture@1.2.3"})).is_err());
}

#[tokio::test]
async fn legacy_npm_records_survive_and_report_migration_without_running_tools() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("plugins");
    tokio::fs::create_dir(&root).await.unwrap();
    let record = json!({
        "kind":"npm","packageName":"legacy","retainedMetadata":{"pin":"1.2.3"},
        "registry":{"id":"acme/legacy","url":"https://registry.example","retainedRegistryMetadata":"keep"}
    });
    let original = serde_json::to_vec(&json!({"legacy":record})).unwrap();
    tokio::fs::write(root.join("sources.json"), &original).await.unwrap();
    let sources = ManagedSources::new(home.path().into(), BTreeMap::new());
    sources.initialize().await.unwrap();
    let error = sources.describe("legacy", &root.join("legacy")).await.unwrap_err().to_string();
    assert!(error.contains("disable this plugin"));
    assert!(error.contains("change this existing plugin ID's directory path"));
    assert!(error.contains("removal deletes its settings"));
    assert_eq!(sources.record("legacy").await.unwrap(), Some(record.clone()));
    assert_eq!(tokio::fs::read(root.join("sources.json")).await.unwrap(), original);
    assert!(sources.prepare_install(&json!({"source":"npm:legacy"})).await.is_err());
    sources.commit("another", &json!({"kind":"cargo","crateName":"another"})).await.unwrap();
    assert_eq!(read_json(&root.join("sources.json")).await.unwrap()["legacy"], record);
}

#[test]
fn registry_native_artifacts_preserve_pins_paths_and_source_identity() {
    let artifact = cargo_target("fixture","1.2.3",b"crate");
    let document = json!({"id":"acme/fixture","artifact":{"kind":"cargo","crate":"fixture","version":"1.2.3","resolved":artifact["resolved"],"checksum":artifact["checksum"],"pluginPath":"plugin"}});
    let resolved = registry::resolve_document("acme/fixture",&document).unwrap();
    assert_eq!(resolved.input, json!({"source":"cargo:fixture@1.2.3","pluginPath":"plugin"}));
    assert_eq!(resolved.target, artifact);
    registry::assert_identity(&json!({"kind":"cargo","crateName":"fixture","pluginPath":"plugin"}),&resolved.artifact).unwrap();
    assert!(registry::assert_identity(&json!({"kind":"cargo","crateName":"other","pluginPath":"plugin"}),&resolved.artifact).is_err());
    let mut wrong = document.clone();
    wrong["artifact"]["resolved"] = json!("https://static.crates.io/crates/other/other-1.2.3.crate");
    assert!(registry::resolve_document("acme/fixture",&wrong).is_err());
    wrong = document.clone();
    wrong["artifact"]["pluginPath"] = json!("../escape");
    assert!(registry::resolve_document("acme/fixture",&wrong).is_err());
    assert!(registry::resolve_document("acme/fixture",&json!({"id":"acme/fixture","artifact":{"kind":"npm"}})).is_err());
    let release = json!({"id":"acme/fixture","artifact":{"kind":"release","url":"https://releases.example/fixture-1.2.3.tar.gz","version":"1.2.3","sha256":"ab".repeat(32),"pluginPath":"fixture"}});
    let resolved = registry::resolve_document("acme/fixture",&release).unwrap();
    assert_eq!(resolved.target["format"], "tar.gz");
    assert_eq!(resolved.input["pluginPath"], "fixture");
    registry::assert_identity(&json!({"kind":"release","origin":"https://releases.example","pluginPath":"fixture"}), &resolved.artifact).unwrap();
    assert!(registry::assert_identity(&json!({"kind":"release","origin":"https://other.example","pluginPath":"fixture"}), &resolved.artifact).is_err());
}

#[test]
fn native_archives_reject_links_wrong_roots_and_duplicate_files() {
    let temp = tempfile::tempdir().unwrap();
    let bytes = tar_fixture("wrong-1.2.3/file", b"contents");
    assert!(cargo::extract(&bytes,&temp.path().join("wrong"),"fixture-1.2.3").is_err());
    let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut archive = tar::Builder::new(encoder);
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_size(0);
    header.set_mode(0o777);
    header.set_link_name("../../escape").unwrap();
    header.set_cksum();
    archive.append_data(&mut header,"fixture-1.2.3/link", std::io::empty()).unwrap();
    let bytes = archive.into_inner().unwrap().finish().unwrap();
    assert!(cargo::extract(&bytes,&temp.path().join("link"),"fixture-1.2.3").is_err());
    let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut archive = tar::Builder::new(encoder);
    for content in [b"one", b"two"] {
        let mut header = tar::Header::new_gnu();
        header.set_size(3);
        header.set_mode(0o600);
        header.set_cksum();
        archive.append_data(&mut header,"fixture-1.2.3/file", content.as_slice()).unwrap();
    }
    let bytes = archive.into_inner().unwrap().finish().unwrap();
    assert!(cargo::extract(&bytes,&temp.path().join("duplicate"),"fixture-1.2.3").is_err());
}

#[tokio::test]
async fn release_updates_bind_archive_digest_and_prevent_origin_changes() {
    use sha2::{Digest, Sha256};
    let home = tempfile::tempdir().unwrap();
    let sources = ManagedSources::new(home.path().into(), BTreeMap::new());
    sources.initialize().await.unwrap();
    let root = sources.root.join("fixture").join(Uuid::new_v4().to_string());
    tokio::fs::create_dir_all(&root).await.unwrap();
    let directory = root.join("release");
    let bytes = tar_fixture("paseo-plugin.json", NATIVE_MANIFEST);
    cargo::extract(&bytes,&directory,"").unwrap();
    let artifact = release::target(&json!({"version":"1.2.3","sha256":hex::encode(Sha256::digest(&bytes))}),"https://releases.example/fixture.tar.gz").unwrap();
    tokio::fs::write(root.join("source.archive"),&bytes).await.unwrap();
    tokio::fs::write(root.join("artifact.json"),serde_json::to_vec(&artifact).unwrap()).await.unwrap();
    sources.commit("fixture",&json!({"kind":"release","origin":"https://releases.example"})).await.unwrap();
    assert_eq!(sources.preview("fixture",&directory,None).await.unwrap()["outcome"], "current");
    let selection = json!({"kind":"release","url":"https://releases.example/new.zip","version":"1.2.4","sha256":"ab".repeat(32)});
    let preview = sources.preview("fixture",&directory,Some(&selection)).await.unwrap();
    assert_eq!(preview["outcome"], "update");
    assert_eq!(preview["target"]["format"], "zip");
    assert_eq!(preview["proposal"]["expected"]["artifact"], artifact);
    sources.assert_current(&preview["proposal"],&directory).await.unwrap();
    let mut changed_origin = preview["proposal"].clone();
    changed_origin["target"]["url"] = json!("https://other.example/new.zip");
    assert!(sources.assert_current(&changed_origin,&directory).await.is_err());
    let candidate = Candidate { directory:directory.clone(),default_id:"fixture".into(),record:json!({"kind":"release","origin":"https://releases.example"}),build:Value::Null,version_root:Some(root.clone()),target:Some(artifact) };
    sources.verify("fixture",&candidate,None).await.unwrap();
    tokio::fs::write(root.join("source.archive"),b"tampered").await.unwrap();
    assert!(sources.verify("fixture",&candidate,None).await.is_err());
    assert!(sources.assert_current(&preview["proposal"],&directory).await.is_err());
}

#[test]
fn release_zip_extraction_and_source_urls_preserve_boundaries() {
    use std::io::{Cursor, Write};
    let temp = tempfile::tempdir().unwrap();
    let zip = |name: &str| {
        let mut archive = zip::ZipWriter::new(Cursor::new(Vec::new()));
        archive.start_file(name, zip::write::SimpleFileOptions::default()).unwrap();
        archive.write_all(NATIVE_MANIFEST).unwrap();
        archive.finish().unwrap().into_inner()
    };
    release::extract_zip(&zip("paseo-plugin.json"),&temp.path().join("valid")).unwrap();
    assert_eq!(std::fs::read(temp.path().join("valid/paseo-plugin.json")).unwrap(),NATIVE_MANIFEST);
    for (index,path) in ["../escape", "/absolute", "C:\\\\escape"].iter().enumerate() {
        assert!(release::extract_zip(&zip(path),&temp.path().join(format!("invalid-{index}"))).is_err());
    }
    let url = "release:https://example.com:8443/path:part.zip?token=a:b";
    assert_eq!(reference::split(url),(url.to_owned(),None));
    let input = json!({"version":"1.2.3","sha256":"ab".repeat(32)});
    assert!(release::target(&input,"http://example.com/archive.zip").is_err());
    assert!(release::target(&input,"https://user:secret@example.com/archive.zip").is_err());
    assert!(release::target(&input,"https://example.com/archive.unknown").is_err());
}
