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
/// Reject renamed APKs as bundles and ABI labels that disagree with the payload.
/// Store/signature acceptance still requires the platform signing tools.
pub(crate) fn validate_android(path: &Path, arch: &str, format: &str) -> Result<()> {
    let abis: &[(&str, u8, u16)] = match arch {
        "arm64" => &[("arm64-v8a", 2, 183)],
        "x64" => &[("x86_64", 2, 62)],
        "universal" => &[("arm64-v8a", 2, 183), ("x86_64", 2, 62)],
        _ => bail!("Unknown Android architecture {arch}"),
    };
    let (required, prefix): (&[&str], &str) = match format {
        "apk" => (
            &[
                "AndroidManifest.xml",
                "classes.dex",
                "resources.arsc",
                "assets/THIRD-PARTY-NOTICES.txt",
            ],
            "lib/",
        ),
        "aab" => (
            &[
                "BundleConfig.pb",
                "base/manifest/AndroidManifest.xml",
                "base/dex/classes.dex",
                "base/resources.pb",
                "base/assets/THIRD-PARTY-NOTICES.txt",
            ],
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
            ensure!(
                member.size() > 0 && member.is_file(),
                "Empty Android member: {name}"
            );
        }
        if let Some(relative) = name.strip_prefix(prefix) {
            if member.is_dir() {
                continue;
            }
            let (abi, library) = relative
                .split_once('/')
                .context("Invalid Android library path")?;
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
            if matches!(library, "libmakepad.so" | "libpaseo_tailscale.so") {
                libraries.insert((abi.to_owned(), library.to_owned()));
            }
        }
    }
    ensure!(
        required.iter().all(|name| members.contains(*name)),
        "Incomplete Android {format} structure"
    );
    ensure!(
        libraries
            == abis
                .iter()
                .flat_map(|(abi, _, _)| {
                    ["libmakepad.so", "libpaseo_tailscale.so"]
                        .map(|name| ((*abi).to_owned(), name.to_owned()))
                })
                .collect(),
        "Android native ABI inventory mismatch (Makepad and embedded Tailscale are required for every ABI)"
    );
    Ok(())
}

pub(crate) fn component_plan() -> Result<crate::components::plan::Plan> {
    crate::components::plan::read(
        &common::root()?.join(var("COMPONENT_PLAN", "artifacts/component-plan.json")),
    )
}
pub(crate) fn artifact_name(artifact: &paseo_protocol::components::ArtifactRef) -> Result<&str> {
    let name = artifact
        .url
        .rsplit('/')
        .next()
        .context("Artifact URL has no filename")?;
    basename(name)?;
    Ok(name)
}
fn component_inventory(
    dir: &Path,
    id: &Identity,
) -> Result<(
    crate::components::plan::TargetPlan,
    paseo_protocol::components::TargetInventory,
    BTreeSet<String>,
)> {
    use paseo_protocol::components::{self, TargetInventory};
    let plan = component_plan()?;
    ensure!(
        plan.version == id.version && plan.source_sha == id.sha && plan.channel == id.channel,
        "Collection/record identity mismatch"
    );
    let decision = plan
        .targets
        .iter()
        .find(|p| p.target == id.target)
        .context("Target is outside component plan")?
        .clone();
    ensure!(
        decision.changed(),
        "No-change target must not produce native artifacts"
    );
    let name = format!("components-{}.json", id.target);
    let inventory: TargetInventory = serde_json::from_value(common::read_json(&dir.join(&name))?)?;
    components::validate_target_inventory(&inventory, &id.target, REPOSITORY)?;
    ensure!(
        inventory.atomic_groups == decision.atomic_groups,
        "Atomic inventory differs from plan"
    );
    ensure!(
        inventory.components.len() == decision.inputs.len(),
        "Component inventory is incomplete"
    );
    let mut files = BTreeSet::from([name]);
    for (component, reference) in &inventory.components {
        if let Some(reused) = decision.reuse.get(component) {
            if reference != reused {
                let origin = components::component_artifact_origin(&inventory, *component)?;
                ensure!(
                    decision.package
                        && !decision.atomic_groups.is_empty()
                        && origin.package.is_some()
                        && origin.version == id.version
                        && origin.source_sha == id.sha
                        && reference.identity == reused.identity
                        && reference.requires == reused.requires
                        && reference.executable == reused.executable,
                    "Reused component provenance changed outside signed cohort: {component}"
                );
                verify_artifact(dir, &reference.artifact)?;
                files.insert(artifact_name(&reference.artifact)?.to_owned());
            }
        } else {
            ensure!(
                decision.build.contains(component)
                    && reference.identity.version == id.version
                    && reference.identity.source_sha == id.sha
                    && decision.inputs.get(component) == Some(&reference.identity.input_hash)
                    && reference.artifact.origin_tag == format!("v{}", id.version),
                "Built component identity mismatch: {component}"
            );
            verify_artifact(dir, &reference.artifact)?;
            files.insert(artifact_name(&reference.artifact)?.to_owned());
        }
    }
    if decision.package {
        ensure!(
            !inventory.packages.is_empty(),
            "Missing rebuilt bootstrap package"
        );
        for package in &inventory.packages {
            ensure!(
                package.version == id.version
                    && package.source_sha == id.sha
                    && package.artifact.origin_tag == format!("v{}", id.version),
                "Package source mismatch"
            );
            let identities = inventory
                .components
                .iter()
                .map(|(id, c)| (*id, c.identity.clone()))
                .collect::<std::collections::BTreeMap<_, _>>();
            let signed_cohort = inventory.atomic_groups.iter().any(|group| {
                group.len() == package.components.len()
                    && group.iter().all(|member| {
                        package.components.get(member) == identities.get(member)
                            && inventory
                                .components
                                .get(member)
                                .is_some_and(|reference| reference.artifact == package.artifact)
                    })
            });
            ensure!(
                package.components == identities || signed_cohort,
                "Package component cohort mismatch"
            );
            verify_artifact(dir, &package.artifact)?;
            if target(&id.target)?.platform == "android" {
                validate_android(
                    &dir.join(artifact_name(&package.artifact)?),
                    target(&id.target)?.arch,
                    &package.artifact.format,
                )?;
            }
            files.insert(artifact_name(&package.artifact)?.to_owned());
        }
    } else {
        ensure!(
            inventory.packages == decision.packages,
            "Unchanged packages were rebuilt or relabelled"
        );
    }
    Ok((decision, inventory, files))
}
pub(crate) fn verify_artifact(
    dir: &Path,
    artifact: &paseo_protocol::components::ArtifactRef,
) -> Result<()> {
    let path = dir.join(artifact_name(artifact)?);
    let meta = fs::symlink_metadata(&path)?;
    ensure!(
        meta.file_type().is_file()
            && meta.len() == artifact.size
            && common::sha256_file(&path)? == artifact.sha256,
        "Artifact digest/size mismatch: {}",
        path.display()
    );
    Ok(())
}
pub(super) fn record(dir: &Path, id: &Identity) -> Result<()> {
    let (decision, inventory, names) = component_inventory(dir, id)?;
    let mut files = Vec::new();
    for name in names {
        let path = dir.join(&name);
        files.push(json!({"name":name,"size":fs::metadata(&path)?.len(),"sha256":common::sha256_file(&path)?}));
    }
    let mut record = serde_json::to_value(id)?;
    record["schemaVersion"] = json!(2);
    record["workflow"] = json!(WORKFLOW);
    record["files"] = json!(files);
    record["plan"] = serde_json::to_value(decision)?;
    record["components"] = serde_json::to_value(inventory.components)?;
    record["packages"] = serde_json::to_value(inventory.packages)?;
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
        record["schemaVersion"] == 2 && record["workflow"] == WORKFLOW,
        "Legacy or invalid component provenance; rebuild required"
    );
    let (decision, inventory, required) = component_inventory(dir, id)?;
    ensure!(
        record["plan"] == serde_json::to_value(&decision)?
            && record["components"] == serde_json::to_value(&inventory.components)?
            && record["packages"] == serde_json::to_value(&inventory.packages)?,
        "Recorded component decisions differ from plan"
    );
    remote::authenticate_reuse(&decision)?;
    let mut actual = BTreeSet::new();
    for entry in record["files"]
        .as_array()
        .context("Missing artifact hashes")?
    {
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
    ensure!(
        actual == required,
        "Provenance inventory is not exact for {}",
        id.target
    );
    Ok(record)
}
#[cfg(test)]
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
pub(crate) fn assemble(dir: &Path) -> Result<paseo_protocol::components::ReleaseInventory> {
    use paseo_protocol::components::{self, ReleaseInventory, TargetInventory};
    let plan = component_plan()?;
    let mut targets = plan.retained_targets.clone();
    for decision in &plan.targets {
        let inventory = if decision.changed() {
            serde_json::from_value(common::read_json(
                &dir.join(format!("components-{}.json", decision.target)),
            )?)?
        } else {
            TargetInventory {
                components: decision.reuse.clone(),
                packages: decision.packages.clone(),
                atomic_groups: decision.atomic_groups.clone(),
            }
        };
        ensure!(
            targets.insert(decision.target.clone(), inventory).is_none(),
            "Duplicate collection target"
        );
    }
    let inventory = ReleaseInventory {
        schema_version: 1,
        version: plan.version,
        source_sha: plan.source_sha,
        channel: plan.channel,
        targets,
    };
    components::validate_inventory(&inventory, REPOSITORY)?;
    Ok(inventory)
}
pub(super) fn manifests(dir: &Path) -> Result<()> {
    let date = var(
        "PASEO_RELEASE_DATE",
        &chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    );
    manifests_at(dir, &date)
}
pub(crate) fn manifests_at(dir: &Path, date: &str) -> Result<()> {
    use paseo_protocol::components::ComponentId;
    let plan = component_plan()?;
    if plan.no_changes {
        return Ok(());
    }
    let inventory = assemble(dir)?;
    let name = format!("components-{}.json", inventory.channel);
    let inventory_path = dir.join(&name);
    common::write_json(&inventory_path, &serde_json::to_value(&inventory)?)?;
    let reference = json!({"url":format!("https://github.com/{REPOSITORY}/releases/download/v{}/{}",inventory.version,name),"size":fs::metadata(&inventory_path)?.len(),"sha512":digest512(&inventory_path)?});
    chrono::DateTime::parse_from_rfc3339(date)?;
    for (target_id, target_inventory) in &inventory.targets {
        let target = target(target_id)?;
        if !target.desktop {
            continue;
        }
        for (id, runtime, prefix, app_id) in [
            (ComponentId::App, CLIENT, CLIENT, CLIENT_APP_ID),
            (ComponentId::Daemon, DAEMON, "daemon", "sh.paseo.daemon"),
        ] {
            let component = target_inventory
                .components
                .get(&id)
                .context("Missing desktop feed component")?;
            let manifest = json!({"schemaVersion":2,"runtime":runtime,"appId":app_id,"channel":inventory.channel,"platform":target.platform,"arch":target.arch,
                "version":component.identity.version,"sourceSha":component.identity.source_sha,"collectionVersion":inventory.version,"collectionSourceSha":inventory.source_sha,
                "inventory":reference,"component":component,"releaseDate":date,"rolloutHours":hours(&var("ROLLOUT_HOURS","36"))?});
            common::write_json(
                &dir.join(format!("{prefix}-{}-{target_id}.json", inventory.channel)),
                &manifest,
            )?;
        }
    }
    Ok(())
}
pub(super) fn restamp(mut manifest: Value, amount: f64) -> Result<Value> {
    ensure!(amount.is_finite() && amount >= 0.0, "Invalid rollout hours");
    ensure!(
        manifest["schemaVersion"] == 2,
        "Unsupported rollout manifest schema"
    );
    let target = format!(
        "{}-{}",
        manifest["platform"].as_str().context("Missing platform")?,
        manifest["arch"].as_str().context("Missing arch")?
    );
    super::target(&target)?;
    let component: paseo_protocol::components::ComponentRef =
        serde_json::from_value(manifest["component"].clone())?;
    paseo_protocol::components::validate_component(&component, &target, REPOSITORY)?;
    ensure!(
        component.id == paseo_protocol::components::ComponentId::App
            && manifest["runtime"] == CLIENT
            && manifest["version"] == component.identity.version
            && manifest["sourceSha"] == component.identity.source_sha,
        "Invalid component rollout identity"
    );
    chrono::DateTime::parse_from_rfc3339(
        manifest["releaseDate"]
            .as_str()
            .context("Missing release date")?,
    )?;
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
        android_fixture_libraries(dir, id, debug_signed, true)
    }

    fn android_fixture_libraries(
        dir: &Path,
        id: &Identity,
        debug_signed: bool,
        tailnet: bool,
    ) -> PathBuf {
        let t = target(&id.target).unwrap();
        let label = if debug_signed { "-debug-signed" } else { "" };
        let name = format!(
            "Passession-{}-android-{}{label}.{}",
            id.version, t.arch, t.format
        );
        let path = dir.join(&name);
        let mut archive = zip::ZipWriter::new(File::create(&path).unwrap());
        let required: &[&str] = if t.format == "apk" {
            &[
                "AndroidManifest.xml",
                "classes.dex",
                "resources.arsc",
                "assets/THIRD-PARTY-NOTICES.txt",
            ]
        } else {
            &[
                "BundleConfig.pb",
                "base/manifest/AndroidManifest.xml",
                "base/dex/classes.dex",
                "base/resources.pb",
                "base/assets/THIRD-PARTY-NOTICES.txt",
            ]
        };
        for name in required {
            archive
                .start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
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
            for library in ["libmakepad.so", "libpaseo_tailscale.so"] {
                if !tailnet && library == "libpaseo_tailscale.so" {
                    continue;
                }
                archive
                    .start_file(
                        format!("{prefix}lib/{abi}/{library}"),
                        zip::write::SimpleFileOptions::default(),
                    )
                    .unwrap();
                let mut header = [0; 20];
                header[..4].copy_from_slice(b"\x7fELF");
                header[4] = class;
                header[5] = 1;
                header[18..20].copy_from_slice(&machine.to_le_bytes());
                archive.write_all(&header).unwrap();
            }
        }
        archive.finish().unwrap();
        common::write_json(
            &dir.join(format!("packages-{}.json", id.target)),
            &json!({"version":id.version,"platform":"android","arch":t.arch,"format":t.format,"profile":"release","sourceSha":id.sha,"artifacts":[name]}),
        ).unwrap();
        path
    }

    #[test]
    fn android_requires_embedded_tailnet_for_every_abi() {
        let directory = tempfile::tempdir().unwrap();
        let mut id = identity();
        id.target = "android-arm64".into();
        let incomplete = android_fixture_libraries(directory.path(), &id, true, false);
        assert!(validate_android(&incomplete, "arm64", "apk").is_err());
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
    #[test]
    fn stamping_preserves_identity_date_and_hashes() {
        let before = json!({
            "schemaVersion":2,"runtime":"app","appId":CLIENT_APP_ID,
            "platform":"windows","arch":"x64","version":"0.13.36","sourceSha":"a".repeat(40),
            "releaseDate":"2026-10-01T00:00:00Z","rolloutHours":36,
            "component":{
                "id":"app","target":"windows-x64",
                "identity":{"version":"0.13.36","sourceSha":"a".repeat(40),"inputHash":"b".repeat(64),"abiVersion":1},
                "artifact":{"url":format!("https://github.com/{REPOSITORY}/releases/download/v0.13.36/app.zip"),
                    "originTag":"v0.13.36","sha256":"c".repeat(64),"size":42,"format":"zip"},
                "executable":"app.exe","executableSha256":"d".repeat(64),
                "requires":{"daemon":1,"tailnet":1,"update-helper":2}
            }
        });
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
        let id = identity();
        let file = dir.path().join("component.tar.gz");
        fs::write(&file, b"component archive").unwrap();
        let artifact = paseo_protocol::components::ArtifactRef {
            url:"https://github.com/emotionbug/Passession/releases/download/v0.13.26/component.tar.gz".into(),
            sha256:common::sha256_file(&file).unwrap(),size:fs::metadata(&file).unwrap().len(),format:"tar.gz".into(),origin_tag:"v0.13.26".into(),
        };
        verify_artifact(dir.path(), &artifact).unwrap();
        let mut record = serde_json::to_value(&id).unwrap();
        record["schemaVersion"] = json!(2);
        record["workflow"] = json!(WORKFLOW);
        common::write_json(
            &dir.path().join(format!("provenance-{}.json", id.target)),
            &record,
        )
        .unwrap();
        let mut wrong = id.clone();
        wrong.run_id = "124".into();
        assert!(verify(dir.path(), &wrong).is_err());
        fs::write(file, b"corrupt archive").unwrap();
        assert!(verify_artifact(dir.path(), &artifact).is_err());
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
    fn legacy_provenance_is_not_reinterpreted_as_component_inventory() {
        let directory = tempfile::tempdir().unwrap();
        let id = identity();
        let mut record = serde_json::to_value(&id).unwrap();
        record["schemaVersion"] = json!(1);
        record["workflow"] = json!(WORKFLOW);
        common::write_json(
            &directory
                .path()
                .join(format!("provenance-{}.json", id.target)),
            &record,
        )
        .unwrap();
        assert!(
            verify(directory.path(), &id)
                .unwrap_err()
                .to_string()
                .contains("Legacy")
        );
    }

    #[test]
    fn android_rejects_wrong_format_and_abi() {
        let dir = tempfile::tempdir().unwrap();
        let mut id = identity();
        id.target = "android-arm64".into();
        let apk = android_fixture(dir.path(), &id, true);
        assert!(validate_android(&apk, "arm64", "aab").is_err());
        assert!(validate_android(&apk, "x64", "apk").is_err());
        assert!(validate_android(&apk, "universal", "apk").is_err());
    }
}
