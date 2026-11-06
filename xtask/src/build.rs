use crate::{common::*, libraries, notices, signing};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    env, fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

fn valid_source(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Local operator builds packaged as complete standalone runtimes. They carry
/// their own local provenance and are never labeled as an official release.
pub const STANDALONE_LOCAL: &str = "standalone-local";

fn native_release_env() -> bool {
    env::var("PASEO_NATIVE_RELEASE").as_deref() == Ok("1")
}

/// A standalone-local runtime must come from a local build: no release flag,
/// channel or repository identity may be claimed or carried over.
fn ensure_local_identity(identity: &Value) -> Result<()> {
    ensure!(
        identity["release"] == false
            && identity["channel"].is_null()
            && identity["repository"].is_null()
            && matches!(identity["sourceDirty"], Value::Bool(_) | Value::Null),
        "standalone-local packages only local builds (release=false, no channel/repository); package official releases with --kind standalone-archive"
    );
    Ok(())
}

pub fn release_identity() -> Result<Value> {
    ensure!(
        env::var("PASEO_NATIVE_RELEASE").as_deref() == Ok("1"),
        "Release packaging requires PASEO_NATIVE_RELEASE=1; local builds are not verified releases"
    );
    let repository = env::var("PASEO_UPDATE_REPOSITORY")
        .context("Set PASEO_UPDATE_REPOSITORY=emotionbug/paseo")?;
    ensure!(
        repository == "emotionbug/paseo",
        "Release repository must be emotionbug/paseo"
    );
    let channel =
        env::var("PASEO_RELEASE_CHANNEL").context("Set PASEO_RELEASE_CHANNEL to stable or beta")?;
    ensure!(
        matches!(channel.as_str(), "stable" | "beta"),
        "PASEO_RELEASE_CHANNEL must be stable or beta"
    );
    let source = env::var("PASEO_BUILD_SOURCE_SHA")
        .context("Set PASEO_BUILD_SOURCE_SHA to the exact release commit")?;
    ensure!(
        valid_source(&source),
        "PASEO_BUILD_SOURCE_SHA must be an exact commit hash"
    );
    Ok(
        json!({"version":version()?,"platform":platform()?,"arch":arch()?,"sourceSha":source,"channel":channel,"repository":repository}),
    )
}

pub fn directory(profile: &str) -> Result<PathBuf> {
    ensure!(
        matches!(profile, "debug" | "release"),
        "Unknown native build profile: {profile}"
    );
    Ok(target_root()?.join(profile))
}

/// Decode the binary header, not a filename or `file` utility's textual guess.
pub fn assert_architecture(path: &Path) -> Result<()> {
    let mut input = fs::File::open(path)
        .with_context(|| format!("Reading native binary {}", path.display()))?;
    let mut header = [0; 64];
    input
        .read_exact(&mut header)
        .with_context(|| format!("Truncated native binary {}", path.display()))?;
    let cpu = arch()?;
    let valid = match platform()? {
        "linux" => {
            header[..6] == [0x7f, b'E', b'L', b'F', 2, 1]
                && u16::from_le_bytes([header[18], header[19]])
                    == if cpu == "x64" { 62 } else { 183 }
        }
        "macos" => {
            u32::from_le_bytes(header[..4].try_into()?) == 0xfeedfacf
                && u32::from_le_bytes(header[4..8].try_into()?)
                    == if cpu == "x64" { 0x01000007 } else { 0x0100000c }
        }
        "windows" if &header[..2] == b"MZ" => {
            let offset = u32::from_le_bytes(header[60..64].try_into()?) as u64;
            ensure!(
                offset
                    .checked_add(26)
                    .is_some_and(|end| end <= input.metadata().map(|m| m.len()).unwrap_or(0)),
                "Truncated PE image: {}",
                path.display()
            );
            input.seek(SeekFrom::Start(offset))?;
            let mut pe = [0; 26];
            input.read_exact(&mut pe)?;
            pe[..4] == [b'P', b'E', 0, 0]
                && u16::from_le_bytes([pe[4], pe[5]]) == if cpu == "x64" { 0x8664 } else { 0xaa64 }
                && u16::from_le_bytes([pe[24], pe[25]]) == 0x20b
        }
        _ => false,
    };
    ensure!(
        valid,
        "Expected a native {}-{cpu} binary: {}",
        platform()?,
        path.display()
    );
    Ok(())
}

pub fn build_daemon(args: &[String]) -> Result<()> {
    ensure!(
        args.len() <= 1 && args.iter().all(|arg| arg == "--release"),
        "Usage: cargo xtask build-daemon [--release]"
    );
    build_native(if args.is_empty() { "debug" } else { "release" }, false)
}

pub fn build_native(profile: &str, container: bool) -> Result<()> {
    platform()?;
    arch()?;
    ensure!(
        env::var_os("CARGO_BUILD_TARGET").is_none(),
        "Build native runtimes on their target host; CARGO_BUILD_TARGET is not supported"
    );
    let release = env::var("PASEO_NATIVE_RELEASE").as_deref() == Ok("1");
    ensure!(
        !release || profile == "release",
        "PASEO_NATIVE_RELEASE=1 requires a release build"
    );
    if let Ok(expected) = env::var("PASEO_VERSION") {
        ensure!(
            expected == version()?,
            "PASEO_VERSION must match workspace.package.version"
        );
    }
    let source = if container {
        let source = env::var("PASEO_BUILD_SOURCE_SHA")
            .context("Pass --build-arg PASEO_BUILD_SOURCE_SHA=<source commit>")?;
        ensure!(
            valid_source(&source),
            "Container source identity must be an exact commit hash"
        );
        source
    } else {
        capture(command("git")?.args(["rev-parse", "HEAD"]))?
    };
    let mut identity = if release {
        release_identity()?
    } else {
        json!({"version":version()?,"platform":platform()?,"arch":arch()?,"sourceSha":source,"channel":null,"repository":null})
    };
    // Containers build from an exported tree whose working-copy state is unknown.
    identity["sourceDirty"] = if container {
        Value::Null
    } else {
        json!(!capture(command("git")?.args(["status", "--porcelain"]))?.is_empty())
    };
    ensure!(
        identity["sourceSha"] == source,
        "PASEO_BUILD_SOURCE_SHA must identify the checked-out release source"
    );
    let directory = directory(profile)?;
    remove_if_exists(&directory.join("paseo-build.json"))?;
    let mut cargo = command(cargo())?;
    cargo
        .args(["build", "--locked", "-p", "paseo-daemon", "--bin", "paseo"])
        .env("PASEO_BUILD_SOURCE_SHA", &source);
    if profile == "release" {
        cargo.arg("--release");
    }
    if !release {
        for key in [
            "PASEO_NATIVE_RELEASE",
            "PASEO_UPDATE_REPOSITORY",
            "PASEO_RELEASE_CHANNEL",
        ] {
            cargo.env_remove(key);
        }
    }
    run(&mut cargo)?;
    let executable = directory.join(executable());
    assert_architecture(&executable)?;
    for name in ["skills", "lib", "plugin-worker"] {
        remove_if_exists(&directory.join(name))?;
    }
    copy_tree(&root()?.join("skills"), &directory.join("skills"))?;
    notices::collect(&directory)?;
    identity["schemaVersion"] = json!(1);
    identity["runtime"] = json!("rust");
    identity["release"] = json!(release);
    identity["profile"] = json!(profile);
    identity["executableSha256"] = json!(sha256_file(&executable)?);
    write_json(&directory.join("paseo-build.json"), &identity)?;
    println!("{}", executable.display());
    Ok(())
}

pub struct NativeBuild {
    pub executable: PathBuf,
    pub directory: PathBuf,
    pub identity: Value,
}

pub fn read_native_build(profile: &str, require_release: bool) -> Result<NativeBuild> {
    let executable = match env::var_os("PASEO_NATIVE_EXECUTABLE") {
        Some(path) => absolute(path)?,
        None => directory(profile)?.join(executable()),
    };
    let directory = executable
        .parent()
        .context("Native executable has no parent")?
        .to_path_buf();
    assert_architecture(&executable)?;
    let identity = read_json(&directory.join("paseo-build.json"))?;
    ensure!(
        identity["schemaVersion"] == 1
            && identity["runtime"] == "rust"
            && identity["version"] == version()?
            && identity["platform"] == platform()?
            && identity["arch"] == arch()?
            && identity["executableSha256"] == sha256_file(&executable)?,
        "Native build identity does not match executable/version/host target; run cargo xtask build-daemon"
    );
    ensure!(
        identity["profile"] == profile,
        "Native binary was not built with requested {profile} profile"
    );
    ensure!(
        identity["sourceSha"].as_str().is_some_and(valid_source),
        "Native build source identity is invalid"
    );
    if require_release || env::var("PASEO_NATIVE_RELEASE").as_deref() == Ok("1") {
        let expected = release_identity()?;
        ensure!(
            profile == "release"
                && identity["release"] == true
                && expected
                    .as_object()
                    .context("Invalid release identity")?
                    .iter()
                    .all(|(key, value)| identity.get(key) == Some(value)),
            "Native binary was not built with this exact release provenance; rebuild with cargo xtask build-daemon --release"
        );
    }
    Ok(NativeBuild {
        executable,
        directory,
        identity,
    })
}

pub fn bundle_runtime(destination: &Path, profile: &str, kind: &str) -> Result<()> {
    ensure!(
        matches!(
            kind,
            "desktop-bundled" | "standalone-archive" | STANDALONE_LOCAL | "container"
        ),
        "Unknown native installation kind: {kind}"
    );
    ensure!(
        kind != STANDALONE_LOCAL || !native_release_env(),
        "standalone-local packages a local build; unset PASEO_NATIVE_RELEASE or use --kind standalone-archive"
    );
    // Packages the existing build output only; this never rebuilds the daemon.
    let build = read_native_build(profile, kind == "standalone-archive")?;
    if kind == STANDALONE_LOCAL {
        ensure_local_identity(&build.identity)?;
    }
    let destination = absolute(destination)?;
    // Never erase source build output or a workspace in response to a mistyped destination.
    let parent = destination
        .parent()
        .context("Runtime destination has no parent")?;
    fs::create_dir_all(parent)?;
    let normalized = fs::canonicalize(parent)?.join(
        destination
            .file_name()
            .context("Invalid runtime destination")?,
    );
    ensure!(
        !fs::canonicalize(&build.directory)?.starts_with(&normalized)
            && !fs::canonicalize(root()?)?.starts_with(&normalized),
        "Runtime destination overlaps source/build output"
    );
    ensure!(
        !fs::symlink_metadata(&destination).is_ok_and(|metadata| metadata.file_type().is_symlink()),
        "Runtime destination must not be a symlink"
    );
    remove_if_exists(&destination)?;
    fs::create_dir_all(&destination)?;
    copy_tree(&build.executable, &destination.join(executable()))?;
    chmod(&destination.join(executable()), 0o755)?;
    copy_tree(&build.directory.join("skills"), &destination.join("skills"))?;
    for name in [
        "RUST-THIRD-PARTY-NOTICES.json",
        "RUST-THIRD-PARTY-NOTICES.txt",
    ] {
        copy_tree(&build.directory.join(name), &destination.join(name))?;
    }
    if kind == "desktop-bundled" {
        write_json(
            &destination.join("desktop-runtime.json"),
            &json!({"schemaVersion":1,"executable":executable(),"cliShimPath":executable(),"workingDirectory":".","environment":{}}),
        )?;
    }
    if platform()? == "linux" {
        libraries::native(&destination)?;
    }
    copy_tree(&root()?.join("LICENSE"), &destination.join("LICENSE"))?;
    text(
        &destination.join("THIRD-PARTY-NOTICES.txt"),
        notices::render_runtime(&destination)?,
        0o644,
    )?;
    write_json(
        &destination.join("paseo-runtime.json"),
        &json!({"schemaVersion":1,"runtime":"rust","appId":"sh.paseo.daemon","installationKind":kind,"version":build.identity["version"],"sourceSha":build.identity["sourceSha"],"platform":platform()?,"arch":arch()?,"release":build.identity["release"]}),
    )?;
    let mut provenance = build.identity;
    provenance["sourceExecutableSha256"] = provenance["executableSha256"].clone();
    provenance["executableSha256"] = json!(sha256_file(&destination.join(executable()))?);
    provenance["installationKind"] = json!(kind);
    write_json(&destination.join("bundle-provenance.json"), &provenance)
}

pub fn validate_runtime(runtime: &Path, kind: &str, profile: &str) -> Result<Value> {
    let identity = read_json(&runtime.join("bundle-provenance.json"))?;
    ensure!(
        identity["runtime"] == "rust"
            && identity["version"] == version()?
            && identity["platform"] == platform()?
            && identity["arch"] == arch()?
            && identity["profile"] == profile
            && identity["installationKind"] == kind
            && identity["executableSha256"] == sha256_file(&runtime.join(executable()))?,
        "Staged runtime identity/hash/profile mismatch: {}",
        runtime.display()
    );
    let marker = read_json(&runtime.join("paseo-runtime.json"))?;
    ensure!(
        marker["installationKind"] == kind
            && ["version", "sourceSha", "platform", "arch", "release"]
                .iter()
                .all(|key| marker[key] == identity[key]),
        "Runtime marker does not match bundle provenance: {}",
        runtime.display()
    );
    assert_architecture(&runtime.join(executable()))?;
    if kind == STANDALONE_LOCAL {
        ensure!(
            !native_release_env(),
            "standalone-local runtimes are not release packages; unset PASEO_NATIVE_RELEASE"
        );
        ensure_local_identity(&identity)?;
    } else if native_release_env() || kind == "standalone-archive" {
        let expected = release_identity()?;
        ensure!(
            identity["release"] == true
                && expected
                    .as_object()
                    .context("Invalid release identity")?
                    .iter()
                    .all(|(key, value)| identity.get(key) == Some(value)),
            "Staged runtime release provenance mismatch"
        );
    }
    notices::render_runtime(runtime)?;
    Ok(identity)
}

/// Platform completion shared by official standalone archives and
/// standalone-local runtimes: CRT payload, code signature and final hash.
pub fn finish_standalone_runtime(runtime: &Path, stage: &Path) -> Result<()> {
    match platform()? {
        "macos" => signing::sign_mac(runtime, stage, false)?,
        "windows" => {
            signing::windows_runtime(runtime, true)?;
            signing::sign_windows(&[runtime.join(executable())], stage)?;
            refresh_runtime_hash(runtime)?;
        }
        _ => {}
    }
    Ok(())
}

pub fn refresh_runtime_hash(runtime: &Path) -> Result<()> {
    let path = runtime.join("bundle-provenance.json");
    let mut identity = read_json(&path)?;
    identity["executableSha256"] = json!(sha256_file(&runtime.join(executable()))?);
    write_json(&path, &identity)
}

pub fn stage_command(args: &[String]) -> Result<()> {
    let Some((destination, options)) = args.split_first() else {
        bail!(
            "Usage: cargo xtask stage-runtime DESTINATION [--profile debug|release] [--kind desktop-bundled|standalone-archive|standalone-local|container]"
        );
    };
    let mut profile = if native_release_env() {
        "release"
    } else {
        "debug"
    };
    let mut kind = "desktop-bundled";
    let mut chunks = options.chunks_exact(2);
    for pair in &mut chunks {
        match pair[0].as_str() {
            "--profile" => profile = &pair[1],
            "--kind" => kind = &pair[1],
            other => bail!("Unknown stage-runtime option {other}"),
        }
    }
    ensure!(
        chunks.remainder().is_empty(),
        "stage-runtime option requires a value"
    );
    bundle_runtime(Path::new(destination), profile, kind)?;
    if kind == STANDALONE_LOCAL {
        // Complete it exactly like the official standalone archive payload, so
        // `paseo daemon update --from DESTINATION` receives a full runtime.
        let runtime = absolute(destination)?;
        let stage = tempfile::Builder::new()
            .prefix("paseo-local-runtime-")
            .tempdir()?;
        finish_standalone_runtime(&runtime, stage.path())?;
        validate_runtime(&runtime, kind, profile)?;
        println!("{}", runtime.display());
    }
    Ok(())
}

pub fn container_runtime(args: &[String]) -> Result<()> {
    ensure!(
        platform()? == "linux" && args.len() == 1,
        "Usage: cargo xtask container-runtime DESTINATION (Linux host required)"
    );
    ensure!(
        env::var_os("PASEO_NATIVE_EXECUTABLE").is_none(),
        "Container staging must use its freshly built executable; unset PASEO_NATIVE_EXECUTABLE"
    );
    build_native("release", true)?;
    bundle_runtime(Path::new(&args[0]), "release", "container")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_identity_requires_complete_hex_commit() {
        assert!(valid_source(&"a".repeat(40)));
        assert!(valid_source(&"F".repeat(64)));
        assert!(!valid_source("main"));
        assert!(!valid_source(&"g".repeat(40)));
        assert!(!valid_source(&"a".repeat(39)));
    }
    #[test]
    fn standalone_local_accepts_only_local_build_provenance() {
        let local = json!({"release":false,"channel":null,"repository":null,"sourceDirty":true});
        assert!(ensure_local_identity(&local).is_ok());
        let mut legacy = local.clone();
        legacy.as_object_mut().unwrap().remove("sourceDirty");
        assert!(ensure_local_identity(&legacy).is_ok());
        for (key, value) in [
            ("release", json!(true)),
            ("channel", json!("stable")),
            ("repository", json!("emotionbug/paseo")),
            ("sourceDirty", json!("unknown")),
        ] {
            let mut claimed = local.clone();
            claimed[key] = value;
            assert!(ensure_local_identity(&claimed).is_err(), "{key}");
        }
    }
    #[test]
    fn architecture_rejects_wrong_machine_and_truncated_headers() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("binary");
        fs::write(&path, b"MZ").unwrap();
        assert!(assert_architecture(&path).is_err());
        if cfg!(target_os = "linux") {
            let mut bytes = [0_u8; 64];
            bytes[..6].copy_from_slice(b"\x7fELF\x02\x01");
            let machine: u16 = if arch().unwrap() == "x64" { 183 } else { 62 };
            bytes[18..20].copy_from_slice(&machine.to_le_bytes());
            fs::write(&path, bytes).unwrap();
            assert!(assert_architecture(&path).is_err());
        }
    }
}
