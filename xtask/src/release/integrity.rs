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
        DAEMON => (
            "Paseo-Daemon",
            if t.platform == "windows" {
                vec!["zip"]
            } else {
                vec!["tar.gz"]
            },
        ),
        CLIENT => (
            "Paseo-Sessions",
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
        if runtime == DAEMON { "daemon" } else { CLIENT },
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
        t.desktop && [DAEMON, CLIENT].contains(&runtime),
        "Unsupported feed target/runtime"
    );
    let app_id = if runtime == DAEMON {
        "sh.paseo.daemon"
    } else {
        CLIENT_APP_ID
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
    if runtime == CLIENT {
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
    for runtime in [CLIENT, DAEMON] {
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
/// Reject renamed APKs as bundles and ABI labels that disagree with the payload.
/// Store/signature acceptance still requires the platform signing tools.
fn validate_android(path: &Path, arch: &str, format: &str) -> Result<()> {
    let abis: &[(&str, u8, u16)] = match arch {
        "arm64" => &[("arm64-v8a", 2, 183)],
        "x64" => &[("x86_64", 2, 62)],
        "universal" => &[("arm64-v8a", 2, 183), ("x86_64", 2, 62)],
        _ => bail!("Unknown Android architecture {arch}"),
    };
    let (required, prefix): (&[&str], &str) = match format {
        "apk" => (
            &["AndroidManifest.xml", "classes.dex", "resources.arsc", "assets/THIRD-PARTY-NOTICES.txt"],
            "lib/",
        ),
        "aab" => (
            &["BundleConfig.pb", "base/manifest/AndroidManifest.xml", "base/dex/classes.dex", "base/resources.pb", "base/assets/THIRD-PARTY-NOTICES.txt"],
            "base/lib/",
        ),
        _ => bail!("Unknown Android artifact format {format}"),
    };
    let mut archive = zip::ZipArchive::new(File::open(path)?)?;
    let mut members = BTreeSet::new();
    let mut libraries = BTreeSet::new();
    for index in 0..archive.len() {
        let mut member = archive.by_index(index)?;
        let name = member.name().to_owned();
        ensure!(
            member.enclosed_name().is_some() && members.insert(name.clone()),
            "Unsafe or duplicate Android archive member: {name}"
        );
        if required.contains(&name.as_str()) {
            ensure!(member.size() > 0 && member.is_file(), "Empty Android member: {name}");
        }
        if let Some(relative) = name.strip_prefix(prefix) {
            if member.is_dir() {
                continue;
            }
            let (abi, library) = relative.split_once('/').context("Invalid Android library path")?;
            let (_, class, machine) = abis
                .iter()
                .find(|(expected, _, _)| *expected == abi)
                .with_context(|| format!("Unexpected Android ABI: {abi}"))?;
            ensure!(
                !library.contains('/') && library.ends_with(".so"),
                "Invalid Android native library: {name}"
            );
            let mut header = [0; 20];
            member.read_exact(&mut header)?;
            ensure!(
                &header[..4] == b"\x7fELF"
                    && header[4] == *class
                    && header[5] == 1
                    && u16::from_le_bytes([header[18], header[19]]) == *machine,
                "Android native library architecture mismatch: {name}"
            );
            if library == "libmakepad.so" {
                libraries.insert(abi.to_owned());
            }
        }
    }
    ensure!(
        required.iter().all(|name| members.contains(*name)),
        "Incomplete Android {format} structure"
    );
    ensure!(
        libraries == abis.iter().map(|(abi, _, _)| (*abi).to_owned()).collect(),
        "Android native ABI inventory mismatch"
    );
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
        let expected = names(id, CLIENT)?;
        ensure!(
            actual.len() == expected.len()
                && actual.iter().collect::<BTreeSet<_>>()
                    == expected.iter().collect::<BTreeSet<_>>(),
            "Package metadata inventory mismatch"
        );
        validate_desktop(dir, id)?;
        let mut result = expected;
        result.push(metadata_name);
        result.push(feed(id, CLIENT));
        result.extend(names(id, DAEMON)?);
        result.push(feed(id, DAEMON));
        Ok(result)
    } else if t.platform == "android" {
        let metadata_name = format!("packages-{}.json", id.target);
        let metadata = common::read_json(&dir.join(&metadata_name))?;
        ensure!(
            metadata["version"] == id.version
                && metadata["platform"] == "android"
                && metadata["arch"] == t.arch
                && metadata["format"] == t.format
                && metadata["profile"] == "release"
                && metadata["sourceSha"] == id.sha,
            "Android package metadata identity mismatch"
        );
        let stem = format!("Paseo-Sessions-{}-android-{}", id.version, t.arch);
        let allowed = [
            format!("{stem}.{}", t.format),
            format!("{stem}-debug-signed.{}", t.format),
        ];
        let actual: Vec<String> = serde_json::from_value(metadata["artifacts"].clone())?;
        ensure!(
            actual.len() == 1 && allowed.contains(&actual[0]),
            "Expected exactly one correctly signing-labelled Android artifact"
        );
        let siblings: Vec<_> = fs::read_dir(dir)?
            .map(|entry| Ok(entry?.file_name().to_string_lossy().into_owned()))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .filter(|name| {
                (name.starts_with(&format!("{stem}.")) || name.starts_with(&format!("{stem}-")))
                    && name.ends_with(&format!(".{}", t.format))
            })
            .collect();
        ensure!(siblings == actual, "Ambiguous Android artifact inventory");
        validate_android(&dir.join(&actual[0]), t.arch, t.format)?;
        Ok(vec![actual[0].clone(), metadata_name])
    } else {
        let stem = format!("Paseo-Sessions-{}-{}", id.version, id.target);
        let allowed = [format!("{stem}.ipa"), format!("{stem}-unsigned.app.zip")];
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
        let expected = names(&id, CLIENT)?;
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
        let mut manifest = json!({"schemaVersion":1,"runtime":CLIENT,"appId":CLIENT_APP_ID,"channel":channel,"platform":t.platform,"arch":t.arch,"version":v,"releaseDate":date.to_rfc3339_opts(chrono::SecondsFormat::Millis,true),"rolloutHours":hours(&var("ROLLOUT_HOURS","36"))?,"releaseNotes":notes,"sourceSha":sha});
        if artifacts.len() == 1 {
            manifest["artifact"] = artifacts.remove(0);
        } else {
            manifest["artifacts"] = json!(artifacts);
        }
        validate_manifest(&manifest, &id, CLIENT)?;
        common::write_json(&dir.join(feed(&id, CLIENT)), &manifest)?;
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
    validate_manifest(&manifest, &id, CLIENT)?;
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
    use std::io::Write;

    fn android_fixture(dir: &Path, id: &Identity, debug_signed: bool) -> PathBuf {
        let t = target(&id.target).unwrap();
        let label = if debug_signed { "-debug-signed" } else { "" };
        let name = format!("Paseo-Sessions-{}-android-{}{label}.{}", id.version, t.arch, t.format);
        let path = dir.join(&name);
        let mut archive = zip::ZipWriter::new(File::create(&path).unwrap());
        let required: &[&str] = if t.format == "apk" {
            &["AndroidManifest.xml", "classes.dex", "resources.arsc", "assets/THIRD-PARTY-NOTICES.txt"]
        } else {
            &["BundleConfig.pb", "base/manifest/AndroidManifest.xml", "base/dex/classes.dex", "base/resources.pb", "base/assets/THIRD-PARTY-NOTICES.txt"]
        };
        for name in required {
            archive.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
            archive.write_all(b"fixture").unwrap();
        }
        for (arch, abi, class, machine) in [
            ("arm64", "arm64-v8a", 2, 183_u16),
            ("x64", "x86_64", 2, 62_u16),
        ] {
            if t.arch != arch && t.arch != "universal" {
                continue;
            }
            let prefix = if t.format == "aab" { "base/" } else { "" };
            archive.start_file(
                format!("{prefix}lib/{abi}/libmakepad.so"),
                zip::write::SimpleFileOptions::default(),
            ).unwrap();
            let mut header = [0; 20];
            header[..4].copy_from_slice(b"\x7fELF");
            header[4] = class;
            header[5] = 1;
            header[18..20].copy_from_slice(&machine.to_le_bytes());
            archive.write_all(&header).unwrap();
        }
        archive.finish().unwrap();
        common::write_json(
            &dir.join(format!("packages-{}.json", id.target)),
            &json!({"version":id.version,"platform":"android","arch":t.arch,"format":t.format,"profile":"release","sourceSha":id.sha,"artifacts":[name]}),
        ).unwrap();
        path
    }

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
    /// Installed clients accept only this exact feed identity.
    fn manifest() -> Value {
        json!({"schemaVersion":1,"runtime":"flutter","appId":"sh.paseo.sessions.desktop","platform":"windows","arch":"x64","version":"0.13.1","channel":"stable","sourceSha":"a".repeat(40),"releaseDate":"2026-10-01T00:00:00Z","rolloutHours":36,"artifact":{"name":"Paseo-Sessions-0.13.1-windows-x64.exe","size":42,"format":"exe","sha512":STANDARD.encode([0_u8;64])}})
    }
    #[test]
    fn manifest_identity_and_hashes_are_strict() {
        let good = manifest();
        assert!(validate_manifest(&good, &identity(), CLIENT).is_ok());
        assert_eq!(feed(&identity(), CLIENT), "flutter-stable-windows-x64.json");
        for (field, value) in [
            ("sourceSha", json!("b".repeat(40))),
            ("version", json!("0.13.2")),
            ("runtime", json!("rust")),
            ("arch", json!("arm64")),
        ] {
            let mut bad = good.clone();
            bad[field] = value;
            assert!(validate_manifest(&bad, &identity(), CLIENT).is_err());
        }
        let mut bad = good.clone();
        bad["artifact"]["sha512"] = json!("invalid");
        assert!(validate_manifest(&bad, &identity(), CLIENT).is_err());
        let mut bad = good;
        bad["artifacts"] = json!([]);
        assert!(validate_manifest(&bad, &identity(), CLIENT).is_err());
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
        id.target = "android-arm64".into();
        let file = android_fixture(dir.path(), &id, true);
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
        for (target, suffixes) in [("ios-arm64", [".ipa", "-unsigned.app.zip"])] {
            let dir = tempfile::tempdir().unwrap();
            let mut id = identity();
            id.target = target.into();
            let stem = format!("Paseo-Sessions-{}-{target}", id.version);
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
    #[test]
    fn android_variants_preserve_identity_and_reject_ambiguous_signing() {
        for t in TARGETS.iter().filter(|t| t.platform == "android") {
            let dir = tempfile::tempdir().unwrap();
            let mut id = identity();
            id.target = t.id();
            for debug_signed in [false, true] {
                let path = android_fixture(dir.path(), &id, debug_signed);
                record(dir.path(), &id).unwrap();
                let verified = verify(dir.path(), &id).unwrap();
                let names: BTreeSet<_> = verified["files"].as_array().unwrap()
                    .iter().map(|entry| entry["name"].as_str().unwrap()).collect();
                let metadata_name = format!("packages-{}.json", id.target);
                assert_eq!(names, BTreeSet::from([
                    path.file_name().unwrap().to_str().unwrap(),
                    metadata_name.as_str(),
                ]));
                fs::remove_file(path).unwrap();
            }
            android_fixture(dir.path(), &id, false);
            android_fixture(dir.path(), &id, true);
            assert!(record(dir.path(), &id).is_err());
        }
    }

    #[test]
    fn android_rejects_wrong_format_abi_and_source_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let mut id = identity();
        id.target = "android-arm64".into();
        let apk = android_fixture(dir.path(), &id, true);
        assert!(validate_android(&apk, "arm64", "aab").is_err());
        assert!(validate_android(&apk, "x64", "apk").is_err());
        assert!(validate_android(&apk, "universal", "apk").is_err());
        let metadata_path = dir.path().join("packages-android-arm64.json");
        let metadata = common::read_json(&metadata_path).unwrap();
        for (key, value) in [
            ("sourceSha", "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            ("profile", "dev-fast"),
            ("format", "aab"),
            ("arch", "x64"),
        ] {
            let mut wrong = metadata.clone();
            wrong[key] = json!(value);
            common::write_json(&metadata_path, &wrong).unwrap();
            assert!(record(dir.path(), &id).is_err());
        }
    }

}
