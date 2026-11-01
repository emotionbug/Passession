use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use sha2::{Digest, Sha512};
use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::Read,
    path::Path,
};

pub(super) fn basename(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', '\0', ':']),
        "Unsafe artifact basename {name:?}"
    );
    Ok(())
}
pub(super) fn digest512(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hash = Sha512::new();
    let mut buffer = [0_u8; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(STANDARD.encode(hash.finalize()))
}
pub(super) fn names(id: &Identity, runtime: &str) -> Result<Vec<String>> {
    let t = target(&id.target)?;
    ensure!(t.desktop, "Mobile targets have no desktop update feed");
    let (stem, extensions) = match runtime {
        "rust" => (
            "Paseo-Daemon",
            if t.platform == "windows" {
                vec!["zip"]
            } else {
                vec!["tar.gz"]
            },
        ),
        "flutter" => (
            "Paseo-Sessions-Flutter",
            match t.platform {
                "linux" => vec!["AppImage", "deb"],
                "macos" => vec!["zip"],
                _ => vec!["exe"],
            },
        ),
        _ => bail!("Unknown manifest runtime"),
    };
    Ok(extensions
        .iter()
        .map(|ext| format!("{stem}-{}-{}.{ext}", id.version, id.target))
        .collect())
}
pub(super) fn feed(id: &Identity, runtime: &str) -> String {
    format!(
        "{}-{}-{}.json",
        if runtime == "rust" {
            "daemon"
        } else {
            "flutter"
        },
        id.channel,
        id.target
    )
}
pub(super) fn validate_manifest(
    manifest: &Value,
    id: &Identity,
    runtime: &str,
) -> Result<Vec<Value>> {
    let t = target(&id.target)?;
    ensure!(
        t.desktop && ["rust", "flutter"].contains(&runtime),
        "Unsupported feed target/runtime"
    );
    let app_id = if runtime == "rust" {
        "sh.paseo.daemon"
    } else {
        "sh.paseo.sessions.desktop"
    };
    for (key, value) in [
        ("runtime", runtime),
        ("appId", app_id),
        ("version", id.version.as_str()),
        ("channel", id.channel.as_str()),
        ("platform", t.platform),
        ("arch", t.arch),
        ("sourceSha", id.sha.as_str()),
    ] {
        ensure!(
            manifest[key] == value,
            "Manifest identity mismatch {key}: {}",
            id.target
        );
    }
    ensure!(
        manifest["schemaVersion"] == 1,
        "Unsupported manifest schema"
    );
    if runtime == "flutter" {
        chrono::DateTime::parse_from_rfc3339(
            manifest["releaseDate"]
                .as_str()
                .context("Missing release date")?,
        )?;
        ensure!(
            manifest["rolloutHours"]
                .as_f64()
                .is_some_and(|n| n.is_finite() && n >= 0.0),
            "Invalid rollout hours"
        );
    }
    let entries = if let Some(entries) = manifest.get("artifacts") {
        ensure!(
            manifest.get("artifact").is_none(),
            "Ambiguous artifact inventory"
        );
        entries.as_array().context("Invalid artifact list")?.clone()
    } else {
        vec![
            manifest
                .get("artifact")
                .context("Missing artifact")?
                .clone(),
        ]
    };
    let required = names(id, runtime)?;
    ensure!(
        entries.len() == required.len(),
        "Incomplete artifact inventory"
    );
    for name in &required {
        ensure!(
            entries.iter().filter(|e| e["name"] == *name).count() == 1,
            "Missing or duplicate {name}"
        );
    }
    for entry in &entries {
        let name = entry["name"].as_str().context("Missing artifact name")?;
        basename(name)?;
        let format = if name.ends_with(".tar.gz") {
            "tar.gz".to_owned()
        } else {
            name.rsplit('.')
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase()
        };
        ensure!(
            entry["format"] == format && entry["size"].as_u64().is_some_and(|n| n > 0),
            "Invalid artifact size/format: {name}"
        );
        let sha = entry["sha512"].as_str().context("Missing SHA-512")?;
        let decoded = STANDARD.decode(sha).context("Invalid SHA-512 encoding")?;
        ensure!(
            decoded.len() == 64 && STANDARD.encode(&decoded) == sha,
            "Invalid SHA-512 length/encoding"
        );
    }
    Ok(entries)
}
fn validate_desktop(dir: &Path, id: &Identity) -> Result<()> {
    for runtime in ["flutter", "rust"] {
        let manifest = common::read_json(&dir.join(feed(id, runtime)))?;
        for entry in validate_manifest(&manifest, id, runtime)? {
            let path = dir.join(entry["name"].as_str().unwrap());
            ensure!(
                fs::symlink_metadata(&path)?.file_type().is_file(),
                "Artifact is not a regular file"
            );
            ensure!(
                entry["size"] == fs::metadata(&path)?.len() && entry["sha512"] == digest512(&path)?,
                "Manifest integrity failure: {}",
                path.display()
            );
        }
    }
    Ok(())
}
fn inventory(dir: &Path, id: &Identity) -> Result<Vec<String>> {
    let t = target(&id.target)?;
    if t.desktop {
        let metadata_name = format!("packages-{}.json", id.target);
        let metadata = common::read_json(&dir.join(&metadata_name))?;
        ensure!(
            metadata["version"] == id.version
                && metadata["platform"] == t.platform
                && metadata["arch"] == t.arch,
            "Package metadata identity mismatch"
        );
        let actual: Vec<String> = serde_json::from_value(metadata["artifacts"].clone())?;
        let expected = names(id, "flutter")?;
        ensure!(
            actual.len() == expected.len()
                && actual.iter().collect::<BTreeSet<_>>()
                    == expected.iter().collect::<BTreeSet<_>>(),
            "Package metadata inventory mismatch"
        );
        validate_desktop(dir, id)?;
        let mut result = expected;
        result.push(metadata_name);
        result.push(feed(id, "flutter"));
        result.extend(names(id, "rust")?);
        result.push(feed(id, "rust"));
        Ok(result)
    } else {
        let stem = format!("Paseo-Sessions-Flutter-{}-{}", id.version, id.target);
        let allowed = if t.platform == "android" {
            vec![format!("{stem}.apk"), format!("{stem}-debug-signed.apk")]
        } else {
            vec![format!("{stem}.ipa"), format!("{stem}-unsigned.app.zip")]
        };
        let actual: Vec<_> = fs::read_dir(dir)?
            .map(|e| Ok(e?.file_name().to_string_lossy().into_owned()))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .filter(|n| n.starts_with(&format!("{stem}.")) || n.starts_with(&format!("{stem}-")))
            .collect();
        ensure!(
            actual.len() == 1 && allowed.contains(&actual[0]),
            "Expected exactly one correctly signing-labelled mobile artifact"
        );
        Ok(actual)
    }
}
pub(super) fn record(dir: &Path, id: &Identity) -> Result<()> {
    let mut files = Vec::new();
    for name in inventory(dir, id)? {
        basename(&name)?;
        let path = dir.join(&name);
        let meta = fs::symlink_metadata(&path)?;
        ensure!(
            meta.file_type().is_file() && meta.len() > 0,
            "Empty or nonregular artifact {name}"
        );
        files.push(json!({"name":name,"size":meta.len(),"sha256":common::sha256_file(&path)?}));
    }
    let mut record = serde_json::to_value(id)?;
    record["schemaVersion"] = json!(1);
    record["workflow"] = json!(WORKFLOW);
    record["files"] = json!(files);
    common::write_json(&dir.join(format!("provenance-{}.json", id.target)), &record)
}
pub(super) fn verify(dir: &Path, id: &Identity) -> Result<Value> {
    let record = common::read_json(&dir.join(format!("provenance-{}.json", id.target)))?;
    for (key, value) in serde_json::to_value(id)?.as_object().unwrap() {
        ensure!(
            record[key] == *value,
            "Provenance mismatch {}: {key}",
            id.target
        );
    }
    ensure!(
        record["schemaVersion"] == 1 && record["workflow"] == WORKFLOW,
        "Invalid provenance schema/workflow"
    );
    let entries = record["files"]
        .as_array()
        .context("Missing artifact hashes")?;
    let mut actual = BTreeSet::new();
    for entry in entries {
        let name = entry["name"].as_str().context("Missing artifact name")?;
        basename(name)?;
        ensure!(actual.insert(name.to_owned()), "Duplicate artifact {name}");
        let path = dir.join(name);
        let meta = fs::symlink_metadata(&path)?;
        ensure!(
            meta.file_type().is_file()
                && meta.len() > 0
                && entry["size"] == meta.len()
                && entry["sha256"] == common::sha256_file(&path)?,
            "Artifact integrity failure {name}"
        );
    }
    let required: BTreeSet<_> = inventory(dir, id)?.into_iter().collect();
    ensure!(
        actual == required,
        "Provenance inventory is not exact for {}",
        id.target
    );
    Ok(record)
}
fn check_header(path: &Path, format: &str) -> Result<()> {
    let mut header = [0; 12];
    File::open(path)?.read_exact(&mut header)?;
    let valid = match format {
        "appimage" => {
            header[..4] == [0x7f, b'E', b'L', b'F']
                && header[4] == 2
                && header[8..11] == [b'A', b'I', 2]
        }
        "deb" => &header[..8] == b"!<arch>\n",
        "exe" => &header[..2] == b"MZ",
        "zip" => header[..4] == [b'P', b'K', 3, 4],
        _ => false,
    };
    ensure!(valid, "Not a real {format}: {}", path.display());
    Ok(())
}
pub(super) fn manifests(dir: &Path) -> Result<()> {
    let v = common::version()?;
    let parsed = version(&v)?;
    let channel = var("PASEO_RELEASE_CHANNEL", channel(&parsed));
    ensure!(
        channel == super::channel(&parsed),
        "Channel disagrees with release version"
    );
    let sha = required("SOURCE_SHA")?;
    ensure!(valid_sha(&sha), "Exact source SHA required");
    let date = if let Ok(date) = env::var("PASEO_RELEASE_DATE") {
        chrono::DateTime::parse_from_rfc3339(&date)?.with_timezone(&chrono::Utc)
    } else if let Ok(epoch) = env::var("SOURCE_DATE_EPOCH") {
        chrono::DateTime::from_timestamp(epoch.parse()?, 0).context("Invalid SOURCE_DATE_EPOCH")?
    } else {
        chrono::Utc::now()
    };
    let notes = if let Ok(file) = env::var("PASEO_RELEASE_NOTES_FILE") {
        fs::read_to_string(file)?
    } else {
        var("PASEO_RELEASE_NOTES", "")
    };
    let mut generated = 0;
    for t in TARGETS.iter().filter(|t| t.desktop) {
        let metadata_path = dir.join(format!("packages-{}.json", t.id()));
        if !metadata_path.exists() {
            continue;
        }
        let id = Identity {
            sha: sha.clone(),
            source_tag: String::new(),
            version: v.clone(),
            channel: channel.clone(),
            run_id: String::new(),
            target: t.id(),
        };
        let metadata = common::read_json(&metadata_path)?;
        ensure!(
            metadata["version"] == v
                && metadata["platform"] == t.platform
                && metadata["arch"] == t.arch,
            "Package metadata mismatch"
        );
        let expected = names(&id, "flutter")?;
        let actual: Vec<String> = serde_json::from_value(metadata["artifacts"].clone())?;
        ensure!(
            expected.len() == actual.len()
                && expected.iter().collect::<BTreeSet<_>>()
                    == actual.iter().collect::<BTreeSet<_>>(),
            "Incomplete package metadata"
        );
        let mut artifacts = Vec::new();
        for name in expected {
            let format = name.rsplit('.').next().unwrap().to_ascii_lowercase();
            let path = dir.join(&name);
            check_header(&path, &format)?;
            artifacts.push(json!({"name":name,"format":format,"size":fs::metadata(&path)?.len(),"sha512":digest512(&path)?}));
        }
        let mut manifest = json!({"schemaVersion":1,"runtime":"flutter","appId":"sh.paseo.sessions.desktop","channel":channel,"platform":t.platform,"arch":t.arch,"version":v,"releaseDate":date.to_rfc3339_opts(chrono::SecondsFormat::Millis,true),"rolloutHours":hours(&var("ROLLOUT_HOURS","36"))?,"releaseNotes":notes,"sourceSha":sha});
        if artifacts.len() == 1 {
            manifest["artifact"] = artifacts.remove(0);
        } else {
            manifest["artifacts"] = json!(artifacts);
        }
        validate_manifest(&manifest, &id, "flutter")?;
        common::write_json(&dir.join(feed(&id, "flutter")), &manifest)?;
        generated += 1;
    }
    ensure!(generated > 0, "No desktop package metadata found");
    Ok(())
}
pub(super) fn restamp(mut manifest: Value, amount: f64) -> Result<Value> {
    ensure!(amount.is_finite() && amount >= 0.0, "Invalid rollout hours");
    let date = manifest["releaseDate"]
        .as_str()
        .context("Missing release date")?;
    chrono::DateTime::parse_from_rfc3339(date)?;
    let v = version(manifest["version"].as_str().context("Missing version")?)?;
    let id = Identity {
        sha: manifest["sourceSha"]
            .as_str()
            .context("Missing source SHA")?
            .into(),
        version: v.to_string(),
        source_tag: String::new(),
        channel: channel(&v).into(),
        run_id: String::new(),
        target: format!(
            "{}-{}",
            manifest["platform"].as_str().unwrap_or_default(),
            manifest["arch"].as_str().unwrap_or_default()
        ),
    };
    ensure!(valid_sha(&id.sha), "Invalid source SHA");
    validate_manifest(&manifest, &id, "flutter")?;
    manifest["rolloutHours"] = json!(amount);
    Ok(manifest)
}
pub(super) fn stamp(args: &[String]) -> Result<()> {
    ensure!(args.len() >= 2, "stamp HOURS MANIFEST...");
    let hours = hours(&args[0])?;
    let updates: Vec<_> = args[1..]
        .iter()
        .map(|path| Ok((path, restamp(common::read_json(Path::new(path))?, hours)?)))
        .collect::<Result<_>>()?;
    for (path, manifest) in updates {
        common::write_json(Path::new(path), &manifest)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn identity() -> Identity {
        Identity {
            sha: "a".repeat(40),
            source_tag: "v0.13.1".into(),
            version: "0.13.1".into(),
            channel: "stable".into(),
            run_id: "123".into(),
            target: "windows-x64".into(),
        }
    }
    fn manifest() -> Value {
        json!({"schemaVersion":1,"runtime":"flutter","appId":"sh.paseo.sessions.desktop","platform":"windows","arch":"x64","version":"0.13.1","channel":"stable","sourceSha":"a".repeat(40),"releaseDate":"2026-10-01T00:00:00Z","rolloutHours":36,"artifact":{"name":"Paseo-Sessions-Flutter-0.13.1-windows-x64.exe","size":42,"format":"exe","sha512":STANDARD.encode([0_u8;64])}})
    }
    #[test]
    fn manifest_identity_and_hashes_are_strict() {
        let good = manifest();
        assert!(validate_manifest(&good, &identity(), "flutter").is_ok());
        for (field, value) in [
            ("sourceSha", json!("b".repeat(40))),
            ("version", json!("0.13.2")),
            ("runtime", json!("rust")),
            ("arch", json!("arm64")),
        ] {
            let mut bad = good.clone();
            bad[field] = value;
            assert!(validate_manifest(&bad, &identity(), "flutter").is_err());
        }
        let mut bad = good.clone();
        bad["artifact"]["sha512"] = json!("invalid");
        assert!(validate_manifest(&bad, &identity(), "flutter").is_err());
        let mut bad = good;
        bad["artifacts"] = json!([]);
        assert!(validate_manifest(&bad, &identity(), "flutter").is_err());
    }
    #[test]
    fn stamping_preserves_identity_date_and_hashes() {
        let before = manifest();
        let mut after = restamp(before.clone(), 0.0).unwrap();
        assert_eq!(after["rolloutHours"], 0.0);
        after["rolloutHours"] = before["rolloutHours"].clone();
        assert_eq!(after, before);
    }
    #[test]
    fn traversal_rejected_on_every_host() {
        for name in ["../foo", "..\\foo", "C:foo", "", ".", ".."] {
            assert!(basename(name).is_err());
        }
    }
    #[test]
    fn provenance_rejects_tampering_and_wrong_run() {
        let dir = tempfile::tempdir().unwrap();
        let mut id = identity();
        id.target = "android-universal".into();
        let file = dir
            .path()
            .join("Paseo-Sessions-Flutter-0.13.1-android-universal-debug-signed.apk");
        fs::write(&file, b"actual APK bytes").unwrap();
        record(dir.path(), &id).unwrap();
        verify(dir.path(), &id).unwrap();
        let mut wrong = id.clone();
        wrong.run_id = "124".into();
        assert!(verify(dir.path(), &wrong).is_err());
        fs::write(file, b"changed APK bytes").unwrap();
        assert!(verify(dir.path(), &id).is_err());
    }
    #[test]
    fn installer_headers_match_their_formats() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("installer");
        for (format, header) in [
            (
                "appimage",
                vec![0x7f, b'E', b'L', b'F', 2, 1, 1, 0, b'A', b'I', 2, 0],
            ),
            ("deb", b"!<arch>\nxxxx".to_vec()),
            ("exe", b"MZxxxxxxxxxx".to_vec()),
            ("zip", vec![b'P', b'K', 3, 4, 0, 0, 0, 0, 0, 0, 0, 0]),
        ] {
            fs::write(&path, &header).unwrap();
            check_header(&path, format).unwrap();
            let mut corrupt = header;
            corrupt[0] = 0;
            fs::write(&path, corrupt).unwrap();
            assert!(check_header(&path, format).is_err());
        }
    }
    #[test]
    fn mobile_inventory_preserves_signing_labels_and_rejects_ambiguity() {
        for (target, suffixes) in [
            ("android-universal", [".apk", "-debug-signed.apk"]),
            ("ios-arm64", [".ipa", "-unsigned.app.zip"]),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut id = identity();
            id.target = target.into();
            let stem = format!("Paseo-Sessions-Flutter-{}-{target}", id.version);
            for suffix in suffixes {
                let name = format!("{stem}{suffix}");
                fs::write(dir.path().join(&name), b"mobile artifact").unwrap();
                assert_eq!(inventory(dir.path(), &id).unwrap(), vec![name.clone()]);
                fs::remove_file(dir.path().join(name)).unwrap();
            }
            for suffix in suffixes {
                fs::write(
                    dir.path().join(format!("{stem}{suffix}")),
                    b"mobile artifact",
                )
                .unwrap();
            }
            assert!(inventory(dir.path(), &id).is_err());
        }
    }
}
