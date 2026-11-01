//! Assemble native SDKs once; consumers only install immutable release artifacts.
use crate::common;
use anyhow::{Context, Result, bail, ensure};
use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    env,
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
    process::Command,
};

const REPOSITORY: &str = "emotionbug/paseo";
const SOURCE: &str = "https://github.com/flutter/flutter.git";
const MAX_ARCHIVE: u64 = 8 * 1024 * 1024 * 1024;
const MAX_EXPANDED: u64 = 20 * 1024 * 1024 * 1024;
const WINDOWS_BOOTSTRAP: &str = "bin/internal/shared.bat";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BootstrapPatch {
    path: String,
    upstream_sha256: String,
    patched_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Pin {
    schema_version: u32,
    version: String,
    channel: String,
    source_repository: String,
    source_revision: String,
    engine_revision: String,
    release_repository: String,
    release_tag: String,
    windows_bootstrap_patch: Option<BootstrapPatch>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Manifest {
    schema_version: u32,
    sdk: Pin,
    platform: String,
    architecture: String,
    dart_abi: String,
    source_revision: String,
    engine_revision: String,
    builder_repository: String,
    builder_sha: String,
    archive: String,
    archive_sha256: String,
    archive_size: u64,
}

fn hex_identity(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn pin() -> Result<Pin> {
    let pin: Pin = serde_json::from_reader(File::open(
        common::root()?.join(".github/flutter-sdk.json"),
    )?)?;
    ensure!(
        pin.schema_version == 1,
        "Unsupported Flutter SDK pin schema"
    );
    let version = semver::Version::parse(&pin.version)?;
    ensure!(
        version.pre.is_empty() && version.build.is_empty(),
        "SDK version must be a stable exact version"
    );
    ensure!(pin.channel == "stable", "Unsupported Flutter SDK channel");
    ensure!(
        pin.source_repository == SOURCE && pin.release_repository == REPOSITORY,
        "SDK repositories must be the official source and reserved Paseo release repository"
    );
    ensure!(
        hex_identity(&pin.source_revision, 40) && hex_identity(&pin.engine_revision, 40),
        "SDK source/engine revisions must be full lowercase SHAs"
    );
    if let Some(patch) = &pin.windows_bootstrap_patch {
        ensure!(
            patch.path == WINDOWS_BOOTSTRAP
                && hex_identity(&patch.upstream_sha256, 64)
                && hex_identity(&patch.patched_sha256, 64)
                && patch.upstream_sha256 != patch.patched_sha256,
            "Invalid pinned Windows bootstrap correction"
        );
    }
    let prefix = format!("flutter-sdk-{}-r", pin.version);
    let revision = pin
        .release_tag
        .strip_prefix(&prefix)
        .context("SDK tag must use the reserved flutter-sdk-VERSION-rN namespace")?;
    ensure!(
        !revision.is_empty()
            && !revision.starts_with('0')
            && revision.bytes().all(|b| b.is_ascii_digit()),
        "Invalid SDK artifact revision"
    );
    Ok(pin)
}

fn corrected_windows_bootstrap(source: &str, patch: &BootstrapPatch) -> Result<String> {
    ensure!(
        hex::encode(Sha256::digest(source.as_bytes())) == patch.upstream_sha256,
        "Windows bootstrap source differs from the reviewed upstream bytes"
    );
    // The pinned batch script invokes literal `$git`, unlike the POSIX launcher.
    // Fix its revision lookup so normal launches can reuse the built snapshot.
    let before = "PUSHD %FLUTTER_ROOT% ^& $git rev-parse HEAD";
    ensure!(
        source.matches(before).count() == 1,
        "Windows bootstrap correction no longer applies"
    );
    let corrected = source.replacen(before, "PUSHD %FLUTTER_ROOT% ^& git rev-parse HEAD", 1);
    ensure!(
        hex::encode(Sha256::digest(corrected.as_bytes())) == patch.patched_sha256,
        "Windows bootstrap correction differs from its pinned digest"
    );
    Ok(corrected)
}

fn native_platform() -> Result<&'static str> {
    ensure!(
        env::consts::ARCH == "aarch64",
        "Flutter SDK assembly/install requires a native ARM64 Rust process; emulated x64 is not supported"
    );
    let platform = env::consts::OS;
    ensure!(
        ["linux", "windows"].contains(&platform),
        "Flutter SDK artifacts only support Linux/Windows ARM64"
    );
    if let Ok(arch) = env::var("RUNNER_ARCH") {
        ensure!(arch == "ARM64", "The GitHub runner must be native ARM64");
    }
    Ok(platform)
}

fn required(name: &str) -> Result<String> {
    let value = env::var(name).with_context(|| format!("Missing {name}"))?;
    ensure!(!value.is_empty(), "Missing {name}");
    Ok(value)
}

fn builder_sha() -> Result<String> {
    ensure!(
        required("GITHUB_REPOSITORY")? == REPOSITORY,
        "SDK artifacts must be built/published by {REPOSITORY}"
    );
    let sha = required("GITHUB_SHA")?;
    ensure!(hex_identity(&sha, 40), "Invalid GITHUB_SHA");
    Ok(sha)
}

fn stem(pin: &Pin, platform: &str) -> String {
    format!("paseo-flutter-sdk-{}-{platform}-arm64", pin.version)
}

fn git_config(platform: &str) -> String {
    format!(
        "[core]\n\trepositoryformatversion = 0\n\tbare = false\n\tautocrlf = false\n\tlongpaths = true\n\tsymlinks = {}\n[remote \"origin\"]\n\turl = {SOURCE}\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n",
        platform != "windows"
    )
}

fn output_directory() -> Result<PathBuf> {
    Ok(common::root()?.join("artifacts/flutter-sdk"))
}

// Ignore inherited Git configuration/credentials for the private official clone.
fn git(sdk: &Path) -> Command {
    let mut command = Command::new("git");
    command.current_dir(sdk);
    for (key, _) in env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            command.env_remove(key);
        }
    }
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("GIT_TERMINAL_PROMPT", "0");
    command
}

fn sdk_command(program: &Path, sdk: &Path, home: &Path) -> Command {
    let mut command = Command::new(program);
    command.current_dir(sdk);
    for (key, _) in env::vars_os() {
        let name = key.to_string_lossy();
        if ["FLUTTER_", "DART_", "PUB_", "GIT_"]
            .iter()
            .any(|prefix| name.starts_with(prefix))
            || ["GH_TOKEN", "GITHUB_TOKEN"].contains(&name.as_ref())
        {
            command.env_remove(key);
        }
    }
    command
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("APPDATA", home.join("AppData/Roaming"))
        .env("LOCALAPPDATA", home.join("AppData/Local"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("PUB_CACHE", sdk.join(".pub-cache"))
        .env("CI", "true")
        .env("FLUTTER_SUPPRESS_ANALYTICS", "true")
        .env("DART_SUPPRESS_ANALYTICS", "true")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        );
    command
}

fn flutter_path(sdk: &Path, platform: &str) -> PathBuf {
    sdk.join(if platform == "windows" {
        "bin/flutter.bat"
    } else {
        "bin/flutter"
    })
}

fn dart_path(sdk: &Path, platform: &str) -> PathBuf {
    sdk.join(if platform == "windows" {
        "bin/cache/dart-sdk/bin/dart.exe"
    } else {
        "bin/cache/dart-sdk/bin/dart"
    })
}

fn read_text(path: &Path) -> Result<String> {
    ensure!(
        fs::metadata(path)?.len() <= 1024 * 1024,
        "Unexpectedly large SDK identity file: {}",
        path.display()
    );
    Ok(fs::read_to_string(path)?.trim().to_owned())
}

fn verify_binary(path: &Path, platform: &str) -> Result<()> {
    let mut file = File::open(path)
        .with_context(|| format!("Opening native Dart binary {}", path.display()))?;
    let mut header = [0_u8; 64];
    file.read_exact(&mut header)?;
    match platform {
        "linux" => ensure!(
            &header[..4] == b"\x7fELF"
                && header[4] == 2
                && header[5] == 1
                && u16::from_le_bytes([header[18], header[19]]) == 183,
            "Dart must be a 64-bit little-endian AArch64 ELF, never x64"
        ),
        "windows" => {
            ensure!(&header[..2] == b"MZ", "Dart is not a Windows PE executable");
            let offset = u32::from_le_bytes(header[60..64].try_into()?);
            file.seek(SeekFrom::Start(u64::from(offset)))?;
            let mut pe = [0_u8; 26];
            file.read_exact(&mut pe)?;
            ensure!(
                &pe[..4] == b"PE\0\0"
                    && u16::from_le_bytes([pe[4], pe[5]]) == 0xaa64
                    && u16::from_le_bytes([pe[24], pe[25]]) == 0x20b,
                "Dart must be native ARM64 PE32+, never emulated x64"
            );
        }
        _ => bail!("Unsupported SDK platform"),
    }
    Ok(())
}

fn verify_version(value: &Value, pin: &Pin) -> Result<()> {
    for (field, expected) in [
        ("frameworkVersion", &pin.version),
        ("channel", &pin.channel),
        ("frameworkRevision", &pin.source_revision),
        ("engineRevision", &pin.engine_revision),
    ] {
        ensure!(
            value.get(field).and_then(Value::as_str) == Some(expected.as_str()),
            "Flutter {field} does not match pinned identity"
        );
    }
    Ok(())
}

fn verify_sdk(sdk: &Path, pin: &Pin, platform: &str) -> Result<()> {
    ensure!(
        sdk.join(".git").is_dir(),
        "SDK must preserve its private Git checkout"
    );
    ensure!(
        read_text(&sdk.join(".git/config"))? == git_config(platform).trim(),
        "SDK Git configuration is not the credential-free assembly configuration"
    );
    ensure!(
        read_text(&sdk.join("bin/internal/engine.version"))? == pin.engine_revision,
        "Source engine revision mismatch"
    );
    for stamp in ["engine.stamp", "engine-dart-sdk.stamp"] {
        ensure!(
            read_text(&sdk.join("bin/cache").join(stamp))? == pin.engine_revision,
            "SDK {stamp} mismatch"
        );
    }
    let revision = common::capture(git(sdk).args(["rev-parse", "HEAD"]))?;
    ensure!(
        revision == pin.source_revision,
        "SDK source revision mismatch"
    );
    ensure!(
        common::capture(git(sdk).args([
            "rev-parse",
            &format!("refs/tags/{}^{{commit}}", pin.version)
        ]))? == pin.source_revision,
        "SDK version tag mismatch"
    );
    ensure!(
        common::capture(git(sdk).args(["symbolic-ref", "--short", "HEAD"]))? == pin.channel,
        "SDK branch/channel mismatch"
    );
    ensure!(
        common::capture(git(sdk).args(["remote", "get-url", "origin"]))? == SOURCE,
        "SDK source remote mismatch"
    );
    let mut diff = git(sdk);
    diff.args([
        "-c",
        "core.filemode=false",
        "diff",
        "--no-ext-diff",
        "--quiet",
        "HEAD",
        "--",
        ".",
    ]);
    if let Some(patch) = pin
        .windows_bootstrap_patch
        .as_ref()
        .filter(|_| platform == "windows")
    {
        ensure!(
            common::sha256_file(&sdk.join(&patch.path))? == patch.patched_sha256,
            "SDK Windows bootstrap differs from the reviewed correction"
        );
        diff.arg(format!(":(exclude){}", patch.path));
    }
    common::run(&mut diff)
        .context("SDK tracked source differs from the pinned commit and declared correction")?;
    let expected_stamp = format!("{}:", pin.source_revision);
    let stamp = read_text(&sdk.join("bin/cache/flutter_tools.stamp"))?;
    ensure!(
        stamp
            == if platform == "windows" {
                format!("\"{expected_stamp}\"")
            } else {
                expected_stamp
            },
        "Prebuilt Flutter tool stamp mismatch; refusing to rebuild"
    );
    for file in [
        "LICENSE",
        "bin/cache/dart-sdk/LICENSE",
        "bin/cache/flutter_tools.snapshot",
        "packages/flutter_tools/.dart_tool/package_config.json",
        "packages/flutter_tools/pubspec.lock",
    ] {
        ensure!(
            fs::metadata(sdk.join(file))
                .with_context(|| format!("Missing SDK {file}"))?
                .len()
                > 0,
            "Empty SDK {file}"
        );
    }
    ensure!(
        fs::metadata(sdk.join("packages/flutter_tools/pubspec.lock"))?.modified()?
            >= fs::metadata(sdk.join("packages/flutter_tools/pubspec.yaml"))?.modified()?,
        "Flutter package lock is stale; install must not bootstrap"
    );
    verify_version(
        &common::read_json(&sdk.join("bin/cache/flutter.version.json"))?,
        pin,
    )?;
    verify_package_config(sdk)?;
    verify_binary(&dart_path(sdk, platform), platform)
}

fn runtime_identity(sdk: &Path, pin: &Pin, platform: &str, home: &Path) -> Result<()> {
    verify_sdk(sdk, pin, platform)?;
    let dart = dart_path(sdk, platform);
    // Dart --version can use stderr; retain both streams for the ABI check.
    let output = sdk_command(&dart, sdk, home).arg("--version").output()?;
    ensure!(
        output.status.success(),
        "Installed Dart could not run natively"
    );
    let version = format!(
        "{}{}",
        String::from_utf8(output.stdout)?,
        String::from_utf8(output.stderr)?
    );
    ensure!(
        version.contains(&format!("\"{platform}_arm64\"")),
        "Dart runtime ABI is not {platform}_arm64: {version}"
    );
    // Run the existing snapshot directly: unlike the shell entry point, this
    // cannot download Dart, run pub upgrade, or compile a missing SDK snapshot.
    let output = common::capture(
        sdk_command(&dart, sdk, home)
            .arg(format!(
                "--packages={}",
                sdk.join("packages/flutter_tools/.dart_tool/package_config.json")
                    .display()
            ))
            .arg(sdk.join("bin/cache/flutter_tools.snapshot"))
            .args(["--version", "--machine"]),
    )?;
    verify_version(&serde_json::from_str(&output)?, pin)
}

fn verify_package_config(sdk: &Path) -> Result<()> {
    let canonical = sdk.canonicalize()?;
    let root = reqwest::Url::from_directory_path(&canonical)
        .map_err(|()| anyhow::anyhow!("Cannot encode SDK file URL"))?;
    let base = root.join("packages/flutter_tools/.dart_tool/")?;
    let config =
        common::read_json(&sdk.join("packages/flutter_tools/.dart_tool/package_config.json"))?;
    let packages = config["packages"]
        .as_array()
        .context("Missing Flutter packages")?;
    ensure!(
        !packages.is_empty(),
        "Flutter package configuration is empty"
    );
    for package in packages {
        let uri = package["rootUri"]
            .as_str()
            .context("Missing package rootUri")?;
        ensure!(
            reqwest::Url::parse(uri).is_err() && !uri.starts_with('/'),
            "Flutter package root is not portable"
        );
        let path = base
            .join(uri)?
            .to_file_path()
            .map_err(|()| anyhow::anyhow!("Non-file package root"))?;
        ensure!(
            path.canonicalize()?.starts_with(&canonical),
            "Flutter package escapes installed SDK"
        );
    }
    Ok(())
}

fn portable_package_config(sdk: &Path) -> Result<()> {
    let path = sdk.join("packages/flutter_tools/.dart_tool/package_config.json");
    let mut config = common::read_json(&path)?;
    let packages = config
        .get_mut("packages")
        .and_then(Value::as_array_mut)
        .context("Missing Flutter package configuration")?;
    let canonical = sdk.canonicalize()?;
    let root_url = reqwest::Url::from_directory_path(&canonical)
        .map_err(|()| anyhow::anyhow!("Cannot encode SDK file URL"))?;
    let base = root_url.join("packages/flutter_tools/.dart_tool/")?;
    for package in packages {
        let uri = package
            .get("rootUri")
            .and_then(Value::as_str)
            .context("Missing package rootUri")?;
        let absolute = base.join(uri)?;
        let path = absolute
            .to_file_path()
            .map_err(|()| anyhow::anyhow!("Non-file package rootUri"))?;
        ensure!(
            path.canonicalize()?.starts_with(&canonical),
            "Flutter package references files outside the SDK"
        );
        let relative = base
            .make_relative(&absolute)
            .context("Cannot make Flutter package reference portable")?;
        ensure!(
            !relative.contains("://"),
            "Flutter package reference is not portable"
        );
        package["rootUri"] = Value::String(relative);
    }
    common::write_json(&path, &config)
}

fn check_credential_path(path: &Path) -> Result<()> {
    for component in path.components() {
        let name = component.as_os_str().to_string_lossy().to_ascii_lowercase();
        ensure!(
            ![
                ".git-credentials",
                ".netrc",
                "_netrc",
                "credentials.json",
                "pub-tokens.json",
                "access_tokens.json"
            ]
            .contains(&name.as_str()),
            "Credential-bearing path cannot enter an SDK artifact: {}",
            path.display()
        );
    }
    Ok(())
}

fn create_archive(sdk: &Path, destination: &Path) -> Result<()> {
    let mut file = tempfile::NamedTempFile::new_in(
        destination.parent().context("Missing archive directory")?,
    )?;
    let canonical = sdk.canonicalize()?;
    {
        let encoder = GzEncoder::new(file.as_file_mut(), Compression::default());
        let mut archive = tar::Builder::new(encoder);
        archive.follow_symlinks(false);
        // Keep sparse inputs portable; the extractor rejects special tar types.
        archive.sparse(false);
        for entry in walkdir::WalkDir::new(sdk).sort_by_file_name() {
            let entry = entry?;
            let relative = entry.path().strip_prefix(sdk)?;
            check_credential_path(relative)?;
            let archive_path = Path::new("flutter").join(relative);
            if entry.file_type().is_symlink() {
                ensure!(
                    entry.path().canonicalize()?.starts_with(&canonical),
                    "SDK symlink escapes checkout"
                );
                validate_link(&archive_path, &fs::read_link(entry.path())?)?;
            }
            ensure!(
                entry.file_type().is_file()
                    || entry.file_type().is_dir()
                    || entry.file_type().is_symlink(),
                "Unsupported SDK filesystem object"
            );
            archive.append_path_with_name(entry.path(), archive_path)?;
        }
        archive.into_inner()?.finish()?.sync_all()?;
    }
    file.persist_noclobber(destination)
        .map_err(|error| error.error)?;
    Ok(())
}

fn build(pin: &Pin) -> Result<()> {
    let platform = native_platform()?;
    let builder_sha = builder_sha()?;
    let temporary = tempfile::tempdir()?;
    let sdk = temporary.path().join("flutter");
    let home = temporary.path().join("home");
    fs::create_dir(&home)?;
    common::run(
        git(temporary.path())
            .args([
                "-c",
                "core.autocrlf=false",
                "clone",
                "--depth",
                "1",
                "--branch",
                &pin.version,
                "--single-branch",
                SOURCE,
            ])
            .arg(&sdk),
    )?;
    ensure!(
        common::capture(git(&sdk).args(["rev-parse", "HEAD"]))? == pin.source_revision,
        "Official Flutter tag does not match pin"
    );
    ensure!(
        read_text(&sdk.join("bin/internal/engine.version"))? == pin.engine_revision,
        "Official Flutter engine does not match pin"
    );
    common::run(git(&sdk).args(["checkout", "-B", &pin.channel]))?;
    // No credential helper, extraheader, runner path or inherited include is
    // retained. Flutter still has its version tag, objects, branch and origin.
    fs::write(sdk.join(".git/config"), git_config(platform))?;
    if let Some(patch) = pin
        .windows_bootstrap_patch
        .as_ref()
        .filter(|_| platform == "windows")
    {
        let path = sdk.join(&patch.path);
        fs::write(
            &path,
            corrected_windows_bootstrap(&fs::read_to_string(&path)?, patch)?,
        )?;
    }
    common::run(
        sdk_command(&flutter_path(&sdk, platform), &sdk, &home).args(["--version", "--machine"]),
    )?;
    // Reject the Windows bootstrap script's possible x64 fallback before any
    // SDK is packaged or desktop artifacts are fetched.
    verify_binary(&dart_path(&sdk, platform), platform)?;
    common::run(
        sdk_command(&flutter_path(&sdk, platform), &sdk, &home).args([
            "precache",
            if platform == "linux" {
                "--linux"
            } else {
                "--windows"
            },
        ]),
    )?;
    portable_package_config(&sdk)?;
    runtime_identity(&sdk, pin, platform, &home)?;
    let directory = output_directory()?;
    fs::create_dir_all(&directory)?;
    let name = stem(pin, platform);
    let archive = directory.join(format!("{name}.tar.gz"));
    let manifest_path = directory.join(format!("{name}.json"));
    ensure!(
        !archive.exists() && !manifest_path.exists(),
        "SDK outputs already exist; refusing to overwrite"
    );
    create_archive(&sdk, &archive)?;
    let manifest = Manifest {
        schema_version: 1,
        sdk: pin.clone(),
        platform: platform.into(),
        architecture: "arm64".into(),
        dart_abi: format!("{platform}_arm64"),
        source_revision: pin.source_revision.clone(),
        engine_revision: pin.engine_revision.clone(),
        builder_repository: REPOSITORY.into(),
        builder_sha,
        archive: format!("{name}.tar.gz"),
        archive_sha256: common::sha256_file(&archive)?,
        archive_size: fs::metadata(&archive)?.len(),
    };
    common::write_json(&manifest_path, &serde_json::to_value(manifest)?)?;
    println!(
        "Assembled native {platform}-arm64 SDK: {}",
        archive.display()
    );
    Ok(())
}

fn validate_manifest(manifest: &Manifest, pin: &Pin, platform: &str) -> Result<()> {
    ensure!(
        manifest.schema_version == 1 && manifest.sdk == *pin,
        "SDK manifest does not match the configured pin"
    );
    ensure!(
        ["linux", "windows"].contains(&platform)
            && manifest.platform == platform
            && manifest.architecture == "arm64"
            && manifest.dart_abi == format!("{platform}_arm64"),
        "SDK manifest platform/ABI mismatch"
    );
    ensure!(
        manifest.source_revision == pin.source_revision
            && manifest.engine_revision == pin.engine_revision,
        "SDK manifest source/engine mismatch"
    );
    ensure!(
        manifest.builder_repository == REPOSITORY && hex_identity(&manifest.builder_sha, 40),
        "Invalid SDK builder identity"
    );
    ensure!(
        manifest.archive == format!("{}.tar.gz", stem(pin, platform)),
        "Invalid SDK archive name"
    );
    ensure!(
        manifest.archive_size > 0
            && manifest.archive_size <= MAX_ARCHIVE
            && hex_identity(&manifest.archive_sha256, 64),
        "Invalid SDK archive integrity metadata"
    );
    Ok(())
}

fn load_manifest(path: &Path, pin: &Pin, platform: &str) -> Result<Manifest> {
    ensure!(
        fs::metadata(path)?.len() <= 64 * 1024,
        "SDK manifest exceeds size limit"
    );
    let manifest = serde_json::from_reader(File::open(path)?)?;
    validate_manifest(&manifest, pin, platform)?;
    Ok(manifest)
}

fn verify_archive(path: &Path, manifest: &Manifest) -> Result<()> {
    ensure!(
        fs::symlink_metadata(path)?.is_file(),
        "SDK archive is not a regular file"
    );
    ensure!(
        fs::metadata(path)?.len() == manifest.archive_size,
        "SDK archive size mismatch"
    );
    ensure!(
        common::sha256_file(path)? == manifest.archive_sha256,
        "SDK archive SHA-256 mismatch"
    );
    Ok(())
}

fn safe_entry_path(path: &Path) -> Result<()> {
    let text = path.to_str().context("Non-UTF-8 SDK archive path")?;
    ensure!(!text.contains(['\\', ':', '\0']), "Unsafe SDK archive path");
    ensure!(
        !text.split('/').any(|part| part == "." || part == ".."),
        "Traversal in SDK archive path"
    );
    let mut components = path.components();
    ensure!(
        components.next() == Some(Component::Normal("flutter".as_ref())),
        "SDK archive must have exactly one flutter root"
    );
    for component in components {
        ensure!(
            matches!(component, Component::Normal(_)),
            "Traversal in SDK archive path"
        );
        let name = component.as_os_str().to_string_lossy();
        ensure!(!name.ends_with(['.', ' ']), "Ambiguous SDK archive path");
        let base = name
            .split('.')
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        let numbered_device = base
            .strip_prefix("COM")
            .or_else(|| base.strip_prefix("LPT"))
            .is_some_and(|number| {
                matches!(
                    number,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            });
        ensure!(
            !["CON", "PRN", "AUX", "NUL"].contains(&base.as_str()) && !numbered_device,
            "Windows device name in SDK archive"
        );
    }
    check_credential_path(path)
}

fn validate_link(path: &Path, target: &Path) -> Result<()> {
    let text = target.to_str().context("Non-UTF-8 SDK link")?;
    ensure!(
        !text.contains(['\\', ':', '\0']) && !target.is_absolute(),
        "Absolute SDK symlink target"
    );
    let mut depth = path
        .parent()
        .context("SDK link has no parent")?
        .components()
        .count();
    for component in target.components() {
        match component {
            Component::Normal(_) => depth += 1,
            Component::CurDir => (),
            Component::ParentDir => {
                ensure!(depth > 1, "SDK symlink escapes flutter root");
                depth -= 1;
            }
            _ => bail!("Unsafe SDK symlink"),
        }
    }
    Ok(())
}

fn extract_archive(path: &Path, destination: &Path) -> Result<PathBuf> {
    let mut archive = tar::Archive::new(GzDecoder::new(File::open(path)?));
    archive.set_preserve_permissions(false);
    archive.set_preserve_mtime(true);
    let mut paths = HashSet::new();
    let mut size = 0_u64;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        safe_entry_path(&path)?;
        for parent in path
            .ancestors()
            .skip(1)
            .filter(|path| !path.as_os_str().is_empty())
        {
            if let Ok(metadata) = fs::symlink_metadata(destination.join(parent)) {
                ensure!(
                    !metadata.file_type().is_symlink(),
                    "SDK entry traverses a symlink"
                );
            }
        }
        // Case-insensitive uniqueness is needed even when publication validates
        // a Windows archive on a Linux runner.
        ensure!(
            paths.insert(path.to_string_lossy().to_lowercase()),
            "Duplicate SDK archive path"
        );
        ensure!(paths.len() <= 1_000_000, "Too many SDK archive entries");
        size = size
            .checked_add(entry.size())
            .context("SDK archive size overflow")?;
        ensure!(size <= MAX_EXPANDED, "Expanded SDK exceeds size limit");
        let kind = entry.header().entry_type();
        ensure!(
            kind.is_file() || kind.is_dir() || kind.is_symlink(),
            "Unsupported SDK archive entry type"
        );
        if kind.is_symlink() {
            validate_link(
                &path,
                &entry.link_name()?.context("Missing symlink target")?,
            )?;
        }
        ensure!(
            entry.unpack_in(destination)?,
            "SDK archive entry escaped extraction root"
        );
    }
    // Consume the gzip trailer as well: a valid tar prefix is not sufficient.
    io::copy(&mut archive.into_inner(), &mut io::sink())?;
    let sdk = destination.join("flutter");
    let canonical = sdk.canonicalize()?;
    for entry in walkdir::WalkDir::new(&sdk).follow_links(false) {
        let entry = entry?;
        if entry.file_type().is_symlink() {
            ensure!(
                entry.path().canonicalize()?.starts_with(&canonical),
                "SDK symlink chain escapes the SDK"
            );
        }
    }
    Ok(sdk)
}

fn gh(args: &[&str]) -> Result<String> {
    common::capture(Command::new("gh").args(args))
}

fn release(pin: &Pin) -> Result<Value> {
    let value = gh(&["api", &format!("repos/{REPOSITORY}/releases/tags/{}", pin.release_tag)])
        .with_context(|| format!("SDK release {} is unavailable. Run the dedicated Flutter ARM64 SDK workflow in {REPOSITORY} before app CI; consumers never build an SDK", pin.release_tag))?;
    Ok(serde_json::from_str(&value)?)
}

fn validate_release(value: &Value, pin: &Pin, draft: bool) -> Result<String> {
    ensure!(
        value["tag_name"].as_str() == Some(&pin.release_tag)
            && value["prerelease"].as_bool() == Some(true)
            && value["draft"].as_bool() == Some(draft),
        "SDK release must be the exact reserved {}prerelease",
        if draft { "draft " } else { "published " }
    );
    let sha = value["target_commitish"]
        .as_str()
        .context("Missing SDK release target")?;
    ensure!(
        hex_identity(sha, 40),
        "SDK release must target an immutable builder SHA"
    );
    let assets = value["assets"]
        .as_array()
        .context("Missing SDK release assets")?;
    let expected: HashSet<_> = ["linux", "windows"]
        .iter()
        .flat_map(|platform| {
            [
                format!("{}.json", stem(pin, platform)),
                format!("{}.tar.gz", stem(pin, platform)),
            ]
        })
        .collect();
    let actual: HashSet<_> = assets
        .iter()
        .map(|asset| asset["name"].as_str().unwrap_or_default().to_owned())
        .collect();
    ensure!(
        assets.len() == 4 && actual == expected,
        "SDK release must contain exactly both native archives and manifests"
    );
    if !draft {
        let reference: Value = serde_json::from_str(&gh(&[
            "api",
            &format!("repos/{REPOSITORY}/git/ref/tags/{}", pin.release_tag),
        ])?)?;
        ensure!(
            reference["object"]["type"].as_str() == Some("commit")
                && reference["object"]["sha"].as_str() == Some(sha),
            "SDK release tag does not point to its immutable builder commit"
        );
    }
    Ok(sha.to_owned())
}

fn download_release_assets(pin: &Pin, directory: &Path) -> Result<()> {
    common::run(
        Command::new("gh")
            .args([
                "release",
                "download",
                &pin.release_tag,
                "--repo",
                REPOSITORY,
                "--dir",
            ])
            .arg(directory),
    )
}

fn publish(pin: &Pin) -> Result<()> {
    let sha = builder_sha()?;
    let directory = output_directory()?;
    let expected: HashSet<_> = ["linux", "windows"]
        .iter()
        .flat_map(|platform| {
            [
                format!("{}.json", stem(pin, platform)),
                format!("{}.tar.gz", stem(pin, platform)),
            ]
        })
        .collect();
    let actual: HashSet<_> = fs::read_dir(&directory)?
        .map(|entry| Ok(entry?.file_name().to_string_lossy().into_owned()))
        .collect::<Result<_>>()?;
    ensure!(
        actual == expected,
        "Publish requires exactly Linux/Windows ARM64 archives and manifests"
    );
    for platform in ["linux", "windows"] {
        let manifest = load_manifest(
            &directory.join(format!("{}.json", stem(pin, platform))),
            pin,
            platform,
        )?;
        ensure!(
            manifest.builder_sha == sha,
            "SDK builder SHA does not match publication GITHUB_SHA"
        );
        let archive = directory.join(&manifest.archive);
        verify_archive(&archive, &manifest)?;
        let temporary = tempfile::tempdir()?;
        let sdk = extract_archive(&archive, temporary.path())?;
        verify_sdk(&sdk, pin, platform)?;
    }
    // Refuse existing tags too: gh create --target cannot retarget an existing
    // tag. No retry ever edits/deletes another release or replaces any asset.
    let refs: Value = serde_json::from_str(&gh(&[
        "api",
        &format!(
            "repos/{REPOSITORY}/git/matching-refs/tags/{}",
            pin.release_tag
        ),
    ])?)?;
    ensure!(
        !refs
            .as_array()
            .context("Invalid Git refs response")?
            .iter()
            .any(|value| value["ref"].as_str() == Some(&format!("refs/tags/{}", pin.release_tag))),
        "Reserved SDK tag already exists. Never clobber it; bump the artifact rN pin for a new assembly"
    );
    gh(&[
        "release",
        "create",
        &pin.release_tag,
        "--repo",
        REPOSITORY,
        "--target",
        &sha,
        "--draft",
        "--prerelease",
        "--latest=false",
        "--title",
        &format!("Flutter {} native ARM64 SDK (assembly)", pin.version),
        "--notes",
        "Reusable Linux and Windows ARM64 Flutter SDKs assembled from pinned official Flutter source and engine artifacts. Toolchain prerelease only; not a Paseo product update.",
    ])?;
    let mut upload = Command::new("gh");
    upload.args(["release", "upload", &pin.release_tag, "--repo", REPOSITORY]);
    let mut names: Vec<_> = expected.iter().collect();
    names.sort();
    for name in &names {
        upload.arg(directory.join(name));
    }
    common::run(&mut upload)?;
    let remote = release(pin)?;
    ensure!(
        validate_release(&remote, pin, true)? == sha,
        "Draft release target mismatch"
    );
    let downloaded = tempfile::tempdir()?;
    download_release_assets(pin, downloaded.path())?;
    for name in names {
        ensure!(
            common::sha256_file(&directory.join(name))?
                == common::sha256_file(&downloaded.path().join(name))?,
            "Uploaded SDK asset differs: {name}"
        );
    }
    gh(&[
        "release",
        "edit",
        &pin.release_tag,
        "--repo",
        REPOSITORY,
        "--draft=false",
        "--prerelease",
        "--latest=false",
    ])?;
    ensure!(
        validate_release(&release(pin)?, pin, false)? == sha,
        "Published release identity mismatch"
    );
    println!(
        "Published immutable SDK prerelease {} (never latest)",
        pin.release_tag
    );
    Ok(())
}

fn download_asset(
    client: &reqwest::blocking::Client,
    release: &Value,
    name: &str,
    destination: &Path,
    maximum: u64,
) -> Result<()> {
    let asset = release["assets"]
        .as_array()
        .context("Missing release assets")?
        .iter()
        .find(|asset| asset["name"].as_str() == Some(name))
        .context("Missing SDK release asset")?;
    let expected = asset["size"].as_u64().context("Missing SDK asset size")?;
    ensure!(
        expected > 0 && expected <= maximum,
        "Invalid SDK download size"
    );
    let url = asset["browser_download_url"]
        .as_str()
        .context("Missing SDK download URL")?;
    ensure!(
        url.starts_with(&format!(
            "https://github.com/{REPOSITORY}/releases/download/"
        )),
        "SDK download URL is outside reserved repository"
    );
    // The repository is public. Never forward GH_TOKEN into redirects or files.
    let response = client.get(url).send()?.error_for_status()?;
    let mut temporary = tempfile::NamedTempFile::new_in(
        destination.parent().context("Missing download directory")?,
    )?;
    let copied = io::copy(&mut response.take(maximum + 1), temporary.as_file_mut())?;
    ensure!(copied == expected, "SDK download size mismatch");
    temporary.as_file_mut().sync_all()?;
    temporary
        .persist(destination)
        .map_err(|error| error.error)?;
    Ok(())
}

fn append_environment(file: &str, value: &str) -> Result<()> {
    ensure!(!value.contains(['\r', '\n']), "Unsafe CI environment path");
    let path = required(file)?;
    writeln!(OpenOptions::new().append(true).open(path)?, "{value}")?;
    Ok(())
}

fn install(pin: &Pin, args: &[String]) -> Result<()> {
    ensure!(
        args.len() == 4 && args[0] == "--version" && args[2] == "--channel",
        "Usage: cargo xtask flutter-sdk install --version VERSION --channel CHANNEL"
    );
    ensure!(
        args[1] == pin.version && args[3] == pin.channel,
        "Requested Flutter version/channel does not match .github/flutter-sdk.json; update the pin and publish its dedicated SDK release first"
    );
    let platform = native_platform()?;
    // Fail before downloading if the caller cannot receive the CI environment.
    required("GITHUB_ENV")?;
    required("GITHUB_PATH")?;
    let base = PathBuf::from(required("RUNNER_TEMP")?).join("paseo-flutter-sdk");
    let downloads = base
        .join("downloads")
        .join(&pin.release_tag)
        .join(format!("{platform}-arm64"));
    fs::create_dir_all(&downloads)?;
    let remote = release(pin)?;
    let sha = validate_release(&remote, pin, false)?;
    let name = stem(pin, platform);
    let client = reqwest::blocking::Client::builder()
        .user_agent("paseo-flutter-sdk-installer")
        .build()?;
    let manifest_path = downloads.join(format!("{name}.json"));
    download_asset(
        &client,
        &remote,
        &format!("{name}.json"),
        &manifest_path,
        64 * 1024,
    )?;
    let manifest = load_manifest(&manifest_path, pin, platform)?;
    ensure!(
        manifest.builder_sha == sha,
        "SDK builder identity differs from release target"
    );
    let archive = downloads.join(&manifest.archive);
    if !archive.exists() {
        download_asset(&client, &remote, &manifest.archive, &archive, MAX_ARCHIVE)?;
    }
    verify_archive(&archive, &manifest)?;
    let installation = tempfile::Builder::new()
        .prefix("installed-")
        .tempdir_in(&base)?;
    let sdk = extract_archive(&archive, installation.path())?;
    let home = tempfile::tempdir_in(&base)?;
    runtime_identity(&sdk, pin, platform, home.path())?;
    // Keep only the fully validated installation. Consumers never invoke a
    // source bootstrap; the normal Flutter launcher finds all stamps ready.
    let location = installation.keep().join("flutter");
    append_environment(
        "GITHUB_ENV",
        &format!("FLUTTER_ROOT={}", location.display()),
    )?;
    append_environment("GITHUB_PATH", &location.join("bin").display().to_string())?;
    println!(
        "Installed native {platform}-arm64 Flutter {} at {}",
        pin.version,
        location.display()
    );
    Ok(())
}

pub fn run(args: &[String]) -> Result<()> {
    let (command, args) = args
        .split_first()
        .context("Usage: cargo xtask flutter-sdk build|publish|install")?;
    let pin = pin()?;
    match command.as_str() {
        "build" if args.is_empty() => build(&pin),
        "publish" if args.is_empty() => publish(&pin),
        "install" => install(&pin, args),
        _ => bail!(
            "Usage: cargo xtask flutter-sdk build|publish|install --version VERSION --channel CHANNEL (flags only for install)"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archives_preserve_sparse_input_as_portable_file() -> Result<()> {
        use std::io::{Seek, SeekFrom};

        let temporary = tempfile::tempdir()?;
        let sdk = temporary.path().join("source");
        fs::create_dir_all(sdk.join("bin/cache"))?;
        let mut sparse = File::create(sdk.join("bin/cache/sparse-data"))?;
        sparse.write_all(b"begin")?;
        sparse.seek(SeekFrom::Start(1024 * 1024))?;
        sparse.write_all(b"end")?;
        drop(sparse);

        let archive = temporary.path().join("sdk.tar.gz");
        create_archive(&sdk, &archive)?;
        let destination = temporary.path().join("installed");
        fs::create_dir(&destination)?;
        let installed = extract_archive(&archive, &destination)?;
        let data = fs::read(installed.join("bin/cache/sparse-data"))?;
        assert_eq!(data.len(), 1024 * 1024 + 3);
        assert_eq!(&data[..5], b"begin");
        assert!(data[5..1024 * 1024].iter().all(|byte| *byte == 0));
        assert_eq!(&data[1024 * 1024..], b"end");
        Ok(())
    }

    #[test]
    fn archive_paths_and_links_stay_inside_sdk() {
        for path in [
            "../flutter/file",
            "/flutter/file",
            "flutter/../escape",
            "flutter/C:stream",
            "flutter/dir\\escape",
            "flutter/file.",
        ] {
            assert!(safe_entry_path(Path::new(path)).is_err(), "{path}");
        }
        assert!(safe_entry_path(Path::new("flutter/.git/objects/pack/data.pack")).is_ok());
        assert!(validate_link(Path::new("flutter/bin/link"), Path::new("../LICENSE")).is_ok());
        assert!(validate_link(Path::new("flutter/bin/link"), Path::new("../../escape")).is_err());
        assert!(validate_link(Path::new("flutter/link"), Path::new("/etc/passwd")).is_err());
    }

    #[test]
    fn source_and_hash_identity_require_full_lowercase_hex() {
        assert!(hex_identity("6a19cca56475dbfba1478ee68d7bd0c2ef891da1", 40));
        assert!(!hex_identity("6a19cca", 40));
        assert!(!hex_identity(
            "6A19cca56475dbfba1478ee68d7bd0c2ef891da1",
            40
        ));
    }

    #[test]
    fn credentials_are_not_sdk_assets() {
        assert!(check_credential_path(Path::new(".pub-cache/credentials.json")).is_err());
        assert!(check_credential_path(Path::new(".git-credentials")).is_err());
        assert!(check_credential_path(Path::new("bin/cache/dart-sdk/LICENSE")).is_ok());
    }

    #[test]
    fn native_binary_check_rejects_x64_fallback() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("dart");
        let mut elf = [0_u8; 64];
        elf[..6].copy_from_slice(b"\x7fELF\x02\x01");
        elf[18..20].copy_from_slice(&183_u16.to_le_bytes());
        fs::write(&path, elf)?;
        verify_binary(&path, "linux")?;
        elf[18..20].copy_from_slice(&62_u16.to_le_bytes());
        fs::write(&path, elf)?;
        assert!(verify_binary(&path, "linux").is_err());

        let mut pe = [0_u8; 90];
        pe[..2].copy_from_slice(b"MZ");
        pe[60..64].copy_from_slice(&64_u32.to_le_bytes());
        pe[64..68].copy_from_slice(b"PE\0\0");
        pe[68..70].copy_from_slice(&0xaa64_u16.to_le_bytes());
        pe[88..90].copy_from_slice(&0x20b_u16.to_le_bytes());
        fs::write(&path, pe)?;
        verify_binary(&path, "windows")?;
        pe[68..70].copy_from_slice(&0x8664_u16.to_le_bytes());
        fs::write(&path, pe)?;
        assert!(verify_binary(&path, "windows").is_err());
        Ok(())
    }

    #[test]
    fn manifests_cannot_change_platform_pin_or_archive_name() -> Result<()> {
        let pin = pin()?;
        let mut manifest = Manifest {
            schema_version: 1,
            sdk: pin.clone(),
            platform: "linux".into(),
            architecture: "arm64".into(),
            dart_abi: "linux_arm64".into(),
            source_revision: pin.source_revision.clone(),
            engine_revision: pin.engine_revision.clone(),
            builder_repository: REPOSITORY.into(),
            builder_sha: "a".repeat(40),
            archive: format!("{}.tar.gz", stem(&pin, "linux")),
            archive_sha256: "b".repeat(64),
            archive_size: 1,
        };
        validate_manifest(&manifest, &pin, "linux")?;
        assert!(validate_manifest(&manifest, &pin, "windows").is_err());
        manifest.architecture = "x64".into();
        assert!(validate_manifest(&manifest, &pin, "linux").is_err());
        manifest.architecture = "arm64".into();
        manifest.source_revision = "c".repeat(40);
        assert!(validate_manifest(&manifest, &pin, "linux").is_err());
        manifest.source_revision = pin.source_revision.clone();
        manifest.archive = "../other.tar.gz".into();
        assert!(validate_manifest(&manifest, &pin, "linux").is_err());
        Ok(())
    }
}
