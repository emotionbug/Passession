use super::integrity::{self, digest512, feed, validate_manifest};
use super::*;
use std::{
    collections::BTreeSet,
    fs,
    path::Path,
    process::{Command, Stdio},
    thread,
    time::Duration,
};

pub(super) fn repository() -> Result<()> {
    ensure!(
        var("GITHUB_REPOSITORY", REPOSITORY) == REPOSITORY,
        "Release operations are restricted to {REPOSITORY}; never use gh's upstream default"
    );
    Ok(())
}
pub(super) fn gh(args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("gh")
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .context("Running GitHub CLI")?;
    ensure!(
        out.status.success(),
        "gh {} failed ({})",
        args.first().unwrap_or(&""),
        out.status
    );
    Ok(out.stdout)
}
fn api(path: &str) -> Result<Value> {
    Ok(serde_json::from_slice(&gh(&["api", path])?)?)
}
pub(super) fn lookup(tag: &str) -> Result<Option<Value>> {
    let pages: Value = serde_json::from_slice(&gh(&[
        "api",
        &format!("repos/{REPOSITORY}/releases?per_page=100"),
        "--paginate",
        "--slurp",
    ])?)?;
    let mut matches = Vec::new();
    for page in pages.as_array().context("Invalid releases response")? {
        for release in page.as_array().context("Invalid release page")? {
            if release["tag_name"] == tag {
                matches.push(release.clone());
            }
        }
    }
    ensure!(matches.len() <= 1, "Ambiguous release tag {tag}");
    Ok(matches.pop())
}
fn upload(tag: &str, path: &Path) -> Result<()> {
    for attempt in 0..4 {
        let result = gh(&[
            "release",
            "upload",
            tag,
            path.to_str().context("Non-UTF8 upload path")?,
            "--clobber",
            "--repo",
            REPOSITORY,
        ]);
        if result.is_ok() {
            return Ok(());
        }
        if attempt == 3 {
            return result.map(|_| ());
        }
        thread::sleep(Duration::from_secs(2_u64.pow(attempt + 1)));
    }
    unreachable!()
}
fn asset<'a>(release: &'a Value, name: &str) -> Result<&'a Value> {
    let matches: Vec<_> = release["assets"]
        .as_array()
        .context("Missing release assets")?
        .iter()
        .filter(|a| a["name"] == name)
        .collect();
    ensure!(
        matches.len() == 1,
        "Expected exactly one release asset {name}"
    );
    Ok(matches[0])
}
fn asset_bytes(asset: &Value) -> Result<Vec<u8>> {
    let id = asset["id"].as_u64().context("Invalid asset ID")?;
    gh(&[
        "api",
        &format!("repos/{REPOSITORY}/releases/assets/{id}"),
        "-H",
        "Accept: application/octet-stream",
    ])
}
fn download_asset(asset: &Value, path: &Path) -> Result<()> {
    let id = asset["id"].as_u64().context("Invalid asset ID")?;
    let output = fs::File::create(path)?;
    let status = Command::new("gh")
        .args([
            "api",
            &format!("repos/{REPOSITORY}/releases/assets/{id}"),
            "-H",
            "Accept: application/octet-stream",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::from(output))
        .stderr(Stdio::inherit())
        .status()?;
    ensure!(status.success(), "Release asset download failed");
    Ok(())
}
/// Complete all twelve feeds before a draft is visible. Hash downloaded binaries,
/// not just the size reported by GitHub (same-sized stale uploads are possible).
fn complete_feeds(release: &Value, template: &Identity) -> Result<()> {
    let dir = tempfile::tempdir()?;
    complete_feeds_with(
        release,
        template,
        |asset| Ok(serde_json::from_slice(&asset_bytes(asset)?)?),
        |remote, entry| {
            let path = dir
                .path()
                .join(entry["name"].as_str().context("Missing artifact name")?);
            download_asset(remote, &path)?;
            ensure!(
                entry["size"] == fs::metadata(&path)?.len() && entry["sha512"] == digest512(&path)?,
                "Remote artifact hash mismatch: {}",
                path.display()
            );
            fs::remove_file(path)?;
            Ok(())
        },
    )
}
fn complete_feeds_with(
    release: &Value,
    template: &Identity,
    mut read_feed: impl FnMut(&Value) -> Result<Value>,
    mut check_binary: impl FnMut(&Value, &Value) -> Result<()>,
) -> Result<()> {
    for t in TARGETS.iter().filter(|t| t.desktop) {
        let mut id = template.clone();
        id.target = t.id();
        for runtime in ["flutter", "rust"] {
            let name = feed(&id, runtime);
            let manifest = read_feed(asset(release, &name)?)?;
            for entry in validate_manifest(&manifest, &id, runtime)? {
                let remote = asset(release, entry["name"].as_str().unwrap())?;
                ensure!(
                    remote["size"] == entry["size"],
                    "Wrong remote artifact size"
                );
                check_binary(remote, &entry)?;
            }
        }
    }
    Ok(())
}
fn upload_ordered(
    dir: &Path,
    binaries: &BTreeSet<String>,
    retained: &BTreeSet<String>,
    feeds: &BTreeSet<String>,
    mut send: impl FnMut(&Path) -> Result<()>,
) -> Result<()> {
    for name in binaries.iter().chain(retained).chain(feeds) {
        send(&dir.join(name))?;
    }
    Ok(())
}
pub(super) fn publish() -> Result<()> {
    repository()?;
    let tag = required("RELEASE_TAG")?;
    let ids = expected()?;
    let first = &ids[0];
    ensure!(
        tag == format!("v{}", first.version),
        "Canonical release tag required"
    );
    ensure!(
        common::version()? == first.version,
        "Workspace/source version mismatch"
    );
    let dir = directory()?;
    let amount = hours(&var("ROLLOUT_HOURS", "36"))?;
    let publication_run = required("GITHUB_RUN_ID")?;
    ensure!(
        numeric_id(&publication_run),
        "Missing exact publication run identity"
    );
    let notes = metadata::release_notes(&first.version)?;
    let notes_dir = tempfile::tempdir()?;
    let notes_path = notes_dir.path().join("notes.txt");
    fs::write(&notes_path, notes)?;
    let mut binaries = BTreeSet::new();
    let mut feeds = BTreeSet::new();
    let mut retained = BTreeSet::new();
    // Validate every target before the first GitHub mutation.
    for id in &ids {
        let record = integrity::verify(&dir, id)?;
        retained.insert(format!("provenance-{}.json", id.target));
        for entry in record["files"].as_array().unwrap() {
            let name = entry["name"].as_str().unwrap().to_owned();
            if name.starts_with("Paseo-") {
                binaries.insert(name);
            } else if name.starts_with("flutter-") || name.starts_with("daemon-") {
                feeds.insert(name);
            } else {
                retained.insert(name);
            }
        }
    }
    let release = match lookup(&tag)? {
        Some(release) => release,
        None => {
            ensure!(
                var("CREATE_RELEASE", "false") == "true",
                "Platform-only repair requires an existing release"
            );
            let mut args = vec![
                "release",
                "create",
                &tag,
                "--repo",
                REPOSITORY,
                "--target",
                &first.sha,
                "--title",
                &tag,
                "--notes-file",
                notes_path.to_str().context("Non-UTF8 notes path")?,
                "--draft",
            ];
            if !version(&first.version)?.pre.is_empty() {
                args.push("--prerelease");
            }
            let created = gh(&args);
            match lookup(&tag)? {
                Some(release) => release,
                None => {
                    created?;
                    bail!("Release creation did not produce a resolvable draft");
                }
            }
        }
    };
    let lookup_tag = release["tag_name"]
        .as_str()
        .context("Missing release tag")?;
    // Start a new rollout at publication, not the source commit timestamp.
    // Repairs of a published release retain its original rollout origin.
    let date = match release["published_at"].as_str() {
        Some(date) => chrono::DateTime::parse_from_rfc3339(date)?.with_timezone(&chrono::Utc),
        None => chrono::Utc::now(),
    }
    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    for id in &ids {
        if target(&id.target)?.desktop {
            let name = feed(id, "flutter");
            let mut manifest = integrity::restamp(common::read_json(&dir.join(&name))?, amount)?;
            manifest["releaseDate"] = json!(date);
            let provenance_path = dir.join(format!("provenance-{}.json", id.target));
            let mut provenance = common::read_json(&provenance_path)?;
            provenance["buildFiles"] = provenance["files"].clone();
            provenance["publicationRunId"] = json!(publication_run);
            common::write_json(&dir.join(&name), &manifest)?;
            for entry in provenance["files"].as_array_mut().unwrap() {
                if entry["name"] == name {
                    entry["size"] = json!(fs::metadata(dir.join(&name))?.len());
                    entry["sha256"] = json!(common::sha256_file(&dir.join(&name))?);
                }
            }
            common::write_json(&provenance_path, &provenance)?;
            integrity::verify(&dir, id)?;
        }
    }
    // A failed binary upload cannot advertise an unavailable installer.
    upload_ordered(&dir, &binaries, &retained, &feeds, |path| {
        upload(lookup_tag, path)
    })?;
    let release_id = release["id"].as_u64().context("Missing release ID")?;
    gh(&[
        "api",
        "--method",
        "PATCH",
        &format!("repos/{REPOSITORY}/releases/{release_id}"),
        "-F",
        &format!("body=@{}", notes_path.display()),
    ])?;
    // Mobile-only releases never publish a desktop draft.
    if !feeds.is_empty() {
        let fresh = lookup(&tag)?.context("Release disappeared")?;
        complete_feeds(&fresh, first)?;
        let id = fresh["id"].as_u64().context("Missing release ID")?;
        gh(&[
            "api",
            "--method",
            "PATCH",
            &format!("repos/{REPOSITORY}/releases/{id}"),
            "-F",
            "draft=false",
            "-F",
            &format!("prerelease={}", !version(&first.version)?.pre.is_empty()),
        ])?;
    }
    Ok(())
}
pub(super) fn recovery(args: &[String]) -> Result<()> {
    repository()?;
    ensure!(
        args.is_empty() || args == ["--download"],
        "recovery [--download]"
    );
    let run = required("RECOVERY_RUN_ID")?;
    ensure!(
        numeric_id(&run) && run != var("GITHUB_RUN_ID", ""),
        "Recovery requires an exact prior run ID"
    );
    let info = api(&format!("repos/{REPOSITORY}/actions/runs/{run}"))?;
    ensure!(
        info["path"] == WORKFLOW && info["status"] == "completed",
        "Recovery requires a completed canonical Native Release run"
    );
    let targets: Vec<String> = serde_json::from_str(&required("RECOVERY_TARGETS")?)?;
    ensure!(
        !targets.is_empty() && targets.iter().collect::<BTreeSet<_>>().len() == targets.len(),
        "Invalid recovery target inventory"
    );
    let pages: Value = serde_json::from_slice(&gh(&[
        "api",
        &format!("repos/{REPOSITORY}/actions/runs/{run}/artifacts?per_page=100"),
        "--paginate",
        "--slurp",
    ])?)?;
    let artifacts: Vec<&Value> = pages
        .as_array()
        .context("Invalid artifact pages")?
        .iter()
        .flat_map(|p| p["artifacts"].as_array().into_iter().flatten())
        .collect();
    for id in &targets {
        target(id)?;
        let name = format!("native-release-{id}");
        let matches: Vec<_> = artifacts.iter().filter(|a| a["name"] == name).collect();
        ensure!(
            matches.len() == 1 && matches[0]["expired"] == false,
            "Expected exactly one unexpired artifact for {id}"
        );
    }
    if !args.is_empty() {
        let dir = directory()?;
        fs::create_dir_all(&dir)?;
        for id in targets {
            gh(&[
                "run",
                "download",
                &run,
                "--repo",
                REPOSITORY,
                "--name",
                &format!("native-release-{id}"),
                "--dir",
                dir.to_str().context("Invalid artifact directory")?,
            ])?;
        }
    }
    Ok(())
}
pub(super) fn rollout() -> Result<()> {
    repository()?;
    let tag = required("SOURCE_TAG")?;
    let v = tag_version(&tag)?;
    ensure!(tag == format!("v{v}"), "Canonical release tag required");
    let amount = hours(&required("NEW_HOURS")?)?;
    let release = lookup(&tag)?.context("Release not found")?;
    let dir = tempfile::tempdir()?;
    let mut updates = Vec::new();
    let mut sha = None;
    for t in TARGETS.iter().filter(|t| t.desktop) {
        let name = format!("flutter-{}-{}.json", channel(&v), t.id());
        let manifest: Value = serde_json::from_slice(&asset_bytes(asset(&release, &name)?)?)?;
        ensure!(
            manifest["version"] == v.to_string()
                && manifest["channel"] == channel(&v)
                && manifest["platform"] == t.platform
                && manifest["arch"] == t.arch,
            "Rollout manifest identity mismatch"
        );
        if let Some(sha) = &sha {
            ensure!(
                manifest["sourceSha"] == *sha,
                "Mixed source SHAs in release"
            );
        } else {
            sha = Some(manifest["sourceSha"].clone());
        }
        let updated = integrity::restamp(manifest, amount)?;
        let path = dir.path().join(name);
        common::write_json(&path, &updated)?;
        updates.push(path);
    }
    for path in updates {
        upload(&tag, &path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine, engine::general_purpose::STANDARD};
    fn fixture() -> (Value, Identity, std::collections::BTreeMap<String, Value>) {
        let template = Identity {
            sha: "a".repeat(40),
            source_tag: "v0.13.1".into(),
            version: "0.13.1".into(),
            channel: "stable".into(),
            run_id: "10".into(),
            target: "linux-x64".into(),
        };
        let mut assets = Vec::new();
        let mut manifests = std::collections::BTreeMap::new();
        for t in TARGETS.iter().filter(|t| t.desktop) {
            let mut id = template.clone();
            id.target = t.id();
            for runtime in ["flutter", "rust"] {
                let mut entries = Vec::new();
                for name in integrity::names(&id, runtime).unwrap() {
                    let format = if name.ends_with(".tar.gz") {
                        "tar.gz".into()
                    } else {
                        name.rsplit('.').next().unwrap().to_lowercase()
                    };
                    assets.push(json!({"name":name,"size":10}));
                    entries.push(json!({"name":name,"size":10,"format":format,"sha512":STANDARD.encode([0;64])}));
                }
                let mut manifest = json!({"schemaVersion":1,"runtime":runtime,"appId":if runtime=="rust"{"sh.paseo.daemon"}else{"sh.paseo.sessions.desktop"},"version":id.version,"channel":id.channel,"sourceSha":id.sha,"platform":t.platform,"arch":t.arch,"releaseDate":"2026-10-01T00:00:00Z","rolloutHours":36});
                if entries.len() == 1 {
                    manifest["artifact"] = entries.remove(0);
                } else {
                    manifest["artifacts"] = json!(entries);
                }
                let name = feed(&id, runtime);
                assets.push(json!({"name":name,"size":100}));
                manifests.insert(name, manifest);
            }
        }
        (json!({"assets":assets}), template, manifests)
    }
    #[test]
    fn full_publication_requires_twelve_distinct_valid_feeds() {
        let (release, id, mut manifests) = fixture();
        let mut seen = 0;
        complete_feeds_with(
            &release,
            &id,
            |a| {
                seen += 1;
                Ok(manifests[a["name"].as_str().unwrap()].clone())
            },
            |_, _| Ok(()),
        )
        .unwrap();
        assert_eq!(seen, 12);
        manifests
            .get_mut("daemon-stable-windows-arm64.json")
            .unwrap()["sourceSha"] = json!("b".repeat(40));
        assert!(
            complete_feeds_with(
                &release,
                &id,
                |a| Ok(manifests[a["name"].as_str().unwrap()].clone()),
                |_, _| Ok(())
            )
            .is_err()
        );
        let mut missing = release.clone();
        missing["assets"]
            .as_array_mut()
            .unwrap()
            .retain(|a| a["name"] != "flutter-stable-macos-x64.json");
        assert!(
            complete_feeds_with(
                &missing,
                &id,
                |a| Ok(manifests[a["name"].as_str().unwrap()].clone()),
                |_, _| Ok(())
            )
            .is_err()
        );
    }
    #[test]
    fn remote_binary_integrity_failure_blocks_publication_gate() {
        let (release, id, manifests) = fixture();
        assert!(
            complete_feeds_with(
                &release,
                &id,
                |a| Ok(manifests[a["name"].as_str().unwrap()].clone()),
                |_, _| bail!("same-sized corrupt binary")
            )
            .is_err()
        );
    }
    #[test]
    fn failed_upload_never_reaches_feeds() {
        let binaries = BTreeSet::from(["app.zip".into(), "daemon.zip".into()]);
        let feeds = BTreeSet::from(["flutter.json".into()]);
        let retained = BTreeSet::from(["provenance.json".into()]);
        let mut sent = Vec::new();
        assert!(
            upload_ordered(Path::new("."), &binaries, &retained, &feeds, |p| {
                sent.push(p.file_name().unwrap().to_str().unwrap().to_owned());
                if sent.len() == 2 {
                    bail!("upload failed");
                }
                Ok(())
            })
            .is_err()
        );
        assert_eq!(sent, ["app.zip", "daemon.zip"]);
        sent.clear();
        upload_ordered(Path::new("."), &binaries, &retained, &feeds, |p| {
            sent.push(p.file_name().unwrap().to_str().unwrap().to_owned());
            Ok(())
        })
        .unwrap();
        assert_eq!(
            sent,
            ["app.zip", "daemon.zip", "provenance.json", "flutter.json"]
        );
    }
}
