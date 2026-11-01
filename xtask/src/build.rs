use crate::{assets, common::*, libraries, notices};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    env, fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::Command,
};

fn valid_source(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|b| b.is_ascii_hexdigit())
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
    Ok(absolute(env::var_os("CARGO_TARGET_DIR").unwrap_or_else(|| "target".into()))?.join(profile))
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

fn build_native(profile: &str, container: bool) -> Result<()> {
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
        matches!(kind, "desktop-bundled" | "standalone-archive" | "container"),
        "Unknown native installation kind: {kind}"
    );
    let build = read_native_build(profile, kind == "standalone-archive")?;
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
        assets::neovim(&destination)?;
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
    assert_architecture(&runtime.join(executable()))?;
    if env::var("PASEO_NATIVE_RELEASE").as_deref() == Ok("1") || kind == "standalone-archive" {
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

pub fn refresh_runtime_hash(runtime: &Path) -> Result<()> {
    let path = runtime.join("bundle-provenance.json");
    let mut identity = read_json(&path)?;
    identity["executableSha256"] = json!(sha256_file(&runtime.join(executable()))?);
    write_json(&path, &identity)
}

pub fn stage_command(args: &[String]) -> Result<()> {
    let Some((destination, options)) = args.split_first() else {
        bail!(
            "Usage: cargo xtask stage-runtime DESTINATION [--profile debug|release] [--kind desktop-bundled|standalone-archive|container]"
        );
    };
    let mut profile = if env::var("PASEO_NATIVE_RELEASE").as_deref() == Ok("1") {
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
    bundle_runtime(Path::new(destination), profile, kind)
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

struct Credentials(Vec<(PathBuf, Option<Vec<u8>>, Option<fs::Permissions>)>);
impl Credentials {
    fn stage(&mut self, source: &Path, destination: PathBuf) -> Result<()> {
        let before = if destination.exists() {
            Some(fs::read(&destination)?)
        } else {
            None
        };
        let permissions = fs::metadata(&destination)
            .ok()
            .map(|metadata| metadata.permissions());
        self.0.push((destination.clone(), before, permissions));
        copy_tree(source, &destination)
    }
    fn restore(&mut self) -> Result<()> {
        let mut failure = None;
        for (path, before, permissions) in self.0.drain(..).rev() {
            let result: Result<()> = (|| {
                match before {
                    Some(bytes) => {
                        fs::write(&path, bytes)?;
                        if let Some(permissions) = permissions {
                            fs::set_permissions(&path, permissions)?;
                        }
                    }
                    None => remove_if_exists(&path)?,
                };
                Ok(())
            })();
            if let Err(error) = result {
                failure = Some(error);
            }
        }
        if let Some(error) = failure {
            return Err(error.context("Restoring staged signing/Firebase configuration"));
        }
        Ok(())
    }
}
impl Drop for Credentials {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            eprintln!("Credential cleanup failed: {error:#}");
        }
    }
}

fn flutter_command(sdk: &Path, launch: bool) -> Result<Command> {
    let mut process = command(sdk)?;
    process.current_dir(app()?);
    if env::var_os("PERMISSION_HANDLER_INFO_PLIST").is_none() {
        process.env(
            "PERMISSION_HANDLER_INFO_PLIST",
            app()?.join("ios/Runner/Info.plist"),
        );
    }
    if launch {
        let home = root()?.join(".dev/paseo-home");
        fs::create_dir_all(&home)?;
        process
            .env("PASEO_HOME", home)
            .env(
                "PASEO_LISTEN",
                env::var("PASEO_LISTEN").unwrap_or_else(|_| "127.0.0.1:6768".into()),
            )
            .env("PASEO_NODE_ENV", "development");
    }
    Ok(process)
}

pub fn build_flutter(args: &[String]) -> Result<()> {
    supervise_subprocesses()?;
    let mut target = None;
    let mut mode = "release";
    let mut launch = false;
    let mut device = None;
    let mut android_arch = "universal";
    let mut explicit_arch = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--debug" => mode = "debug",
            "--run" => launch = true,
            "--device" | "--arch" => {
                let option = &args[index];
                index += 1;
                let value = args
                    .get(index)
                    .filter(|arg| !arg.starts_with("--"))
                    .context("Missing option value")?;
                if option == "--device" {
                    device = Some(value.as_str());
                } else {
                    android_arch = value;
                    explicit_arch = true;
                }
            }
            value if !value.starts_with('-') && target.is_none() => target = Some(value),
            other => bail!("Unknown build-flutter argument: {other}"),
        }
        index += 1;
    }
    let target = target.unwrap_or(platform()?);
    let desktop = matches!(target, "linux" | "macos" | "windows");
    ensure!(
        desktop || matches!(target, "android" | "ios"),
        "Unknown Flutter platform: {target}"
    );
    ensure!(
        !desktop || target == platform()?,
        "Desktop builds require a native host of the target platform"
    );
    ensure!(
        target != "ios" || platform()? == "macos",
        "iOS builds require macOS and Xcode"
    );
    ensure!(
        matches!(android_arch, "universal" | "arm64")
            && (!explicit_arch || target == "android" && !launch),
        "--arch universal|arm64 is only supported for Android artifact builds, not --run"
    );
    ensure!(
        device.is_none() || launch && !desktop,
        "--device requires --run on Android or iOS"
    );
    ensure!(
        !desktop || mode == "release" || env::var("PASEO_NATIVE_RELEASE").as_deref() != Ok("1"),
        "PASEO_NATIVE_RELEASE=1 requires a release Flutter build"
    );
    let sdk = flutter()?;
    let mut credentials = Credentials(Vec::new());
    let result = (|| -> Result<()> {
        if target == "android" {
            if let Some(source) = env::var_os("GOOGLE_SERVICES_FILE_SESSIONS") {
                let source = absolute(source)?;
                let config = read_json(&source)?;
                ensure!(
                    config["client"]
                        .as_array()
                        .is_some_and(|clients| clients.iter().any(|client| client["client_info"]
                            ["android_client_info"]["package_name"]
                            == "sh.paseo.sessions")),
                    "Firebase configuration is not for sh.paseo.sessions"
                );
                credentials.stage(&source, app()?.join("android/app/google-services.json"))?;
            }
        }
        let mut ios_firebase = None;
        if target == "ios" {
            if let Some(source) = env::var_os("GOOGLE_SERVICE_INFO_PLIST_SESSIONS") {
                let source = absolute(source)?;
                let identifier = capture(
                    command("/usr/libexec/PlistBuddy")?
                        .args(["-c", "Print :BUNDLE_ID"])
                        .arg(&source),
                )?;
                ensure!(
                    identifier == "sh.paseo.sessions",
                    "Firebase configuration is not for sh.paseo.sessions"
                );
                credentials.stage(&source, app()?.join("ios/Runner/GoogleService-Info.plist"))?;
                ios_firebase = Some(source);
            }
        }
        run(flutter_command(&sdk, launch)?.args(["pub", "get", "--enforce-lockfile"]))?;
        if desktop {
            build_native(mode, false)?;
        }
        let version = version()?;
        let channel = env::var("PASEO_RELEASE_CHANNEL").unwrap_or_else(|_| {
            if version.contains('-') {
                "beta"
            } else {
                "stable"
            }
            .into()
        });
        ensure!(
            matches!(channel.as_str(), "stable" | "beta"),
            "PASEO_RELEASE_CHANNEL must be stable or beta"
        );
        let mut options = vec![
            format!("--{mode}"),
            format!("--build-name={version}"),
            format!("--build-number={}", build_number(target)?),
            format!("--dart-define=PASEO_VERSION={version}"),
            format!("--dart-define=PASEO_RELEASE_CHANNEL={channel}"),
        ];
        if launch && !desktop {
            let devices: Value = serde_json::from_str(&capture(
                flutter_command(&sdk, true)?.args(["devices", "--machine"]),
            )?)?;
            let matching: Vec<_> = devices
                .as_array()
                .context("Invalid Flutter device list")?
                .iter()
                .filter(|item| {
                    item["isSupported"] != false
                        && item["targetPlatform"].as_str().is_some_and(|value| {
                            if target == "android" {
                                value.starts_with("android-")
                            } else {
                                value == "ios"
                            }
                        })
                })
                .collect();
            let selected = if let Some(id) = device {
                matching.iter().find(|item| item["id"] == id).copied()
            } else if matching.len() == 1 {
                matching.first().copied()
            } else {
                None
            };
            let selected = selected.with_context(|| {
                format!(
                    "Select an attached {target} device using --device ID; available: {}",
                    matching
                        .iter()
                        .map(|item| format!("{} ({})", item["name"], item["id"]))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;
            let mut process = flutter_command(&sdk, true)?;
            process
                .args([
                    "run",
                    "-d",
                    selected["id"].as_str().context("Device lacks id")?,
                ])
                .args(&options);
            if let Some(source) = &ios_firebase {
                process.env("GOOGLE_SERVICE_INFO_PLIST_SESSIONS", source);
            }
            return run(&mut process);
        }
        let mut flutter_target = target;
        if target == "android" {
            flutter_target = "apk";
            if android_arch == "arm64" {
                options.push("--target-platform=android-arm64".into());
            }
        }
        if target == "ios" {
            if let Some(plist) = env::var_os("PASEO_IOS_EXPORT_OPTIONS_PLIST") {
                flutter_target = "ipa";
                options.push(format!(
                    "--export-options-plist={}",
                    absolute(plist)?.display()
                ));
            } else {
                options.push("--no-codesign".into());
                eprintln!(
                    "No iOS export/provisioning configuration: building an explicitly unsigned device .app, not an installable IPA"
                );
            }
        }
        let mut process = flutter_command(&sdk, launch)?;
        process.args(["build", flutter_target]).args(&options);
        if let Some(source) = &ios_firebase {
            process.env("GOOGLE_SERVICE_INFO_PLIST_SESSIONS", source);
        }
        run(&mut process)?;
        fs::create_dir_all(release()?)?;
        let output;
        if desktop {
            let capital_mode = if mode == "release" {
                "Release"
            } else {
                "Debug"
            };
            output = app()?.join(match target {
                "linux" => format!("build/linux/{}/{mode}/bundle", arch()?),
                "windows" => format!("build/windows/{}/runner/{capital_mode}", arch()?),
                _ => format!("build/macos/Build/Products/{capital_mode}/Paseo Sessions.app"),
            });
            ensure!(
                output.is_dir(),
                "Native build output missing: {}",
                output.display()
            );
            bundle_runtime(
                &output.join(if target == "macos" {
                    "Contents/Resources/runtime"
                } else {
                    "runtime"
                }),
                mode,
                "desktop-bundled",
            )?;
            write_json(
                &release()?.join(format!("build-{target}-{}.json", arch()?)),
                &json!({"version":version,"platform":target,"arch":arch()?,"mode":mode,"output":output.strip_prefix(root()?)?.to_string_lossy().replace('\\',"/")}),
            )?;
            if launch {
                let executable = output.join(match target {
                    "macos" => "Contents/MacOS/Paseo Sessions",
                    "windows" => "paseo_sessions.exe",
                    _ => "paseo_sessions",
                });
                let mut process = flutter_command(&executable, true)?;
                process.current_dir(&output);
                run(&mut process)?;
            }
        } else if target == "android" {
            let properties = app()?.join("android/key.properties");
            let signing_properties = if properties.is_file() {
                fs::read_to_string(properties)?
            } else {
                String::new()
            };
            let release_key = env::var("ANDROID_KEYSTORE_PATH")
                .is_ok_and(|value| !value.trim().is_empty())
                || regex::Regex::new(r"(?m)^\s*storeFile\s*(?:=|:|\s)\s*\S")?
                    .is_match(&signing_properties);
            let signing = if mode == "debug" || !release_key {
                "-debug-signed"
            } else {
                ""
            };
            output = release()?.join(format!(
                "{}{signing}.apk",
                artifact_stem("android", android_arch)?
            ));
            copy_tree(
                &app()?.join(format!("build/app/outputs/flutter-apk/app-{mode}.apk")),
                &output,
            )?;
        } else if flutter_target == "ipa" {
            let directory = app()?.join("build/ios/ipa");
            let mut ipas = Vec::new();
            for entry in fs::read_dir(directory)? {
                let path = entry?.path();
                if path.extension().is_some_and(|value| value == "ipa") {
                    ipas.push(path);
                }
            }
            ensure!(ipas.len() == 1, "Expected exactly one exported IPA");
            output = release()?.join(format!("{}.ipa", artifact_stem("ios", "arm64")?));
            copy_tree(&ipas[0], &output)?;
        } else {
            output = release()?.join(format!(
                "{}-unsigned.app.zip",
                artifact_stem("ios", "arm64")?
            ));
            run(command("ditto")?
                .args(["-c", "-k", "--sequesterRsrc", "--keepParent"])
                .arg(app()?.join("build/ios/iphoneos/Runner.app"))
                .arg(&output))?;
        }
        println!("Flutter {target} {mode}: {}", output.display());
        Ok(())
    })();
    let cleanup = credentials.restore();
    match (result, cleanup) {
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("Credential restoration also failed: {cleanup:#}")))
        }
        (Err(error), _) => Err(error),
        (_, Err(error)) => Err(error),
        _ => Ok(()),
    }
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
    fn credentials_restore_existing_and_remove_temporary_files() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        fs::write(&source, "secret").unwrap();
        let existing = directory.path().join("existing");
        fs::write(&existing, "original").unwrap();
        let new = directory.path().join("new");
        {
            let mut guard = Credentials(Vec::new());
            guard.stage(&source, existing.clone()).unwrap();
            guard.stage(&source, new.clone()).unwrap();
            assert_eq!(fs::read_to_string(&existing).unwrap(), "secret");
        }
        assert_eq!(fs::read_to_string(existing).unwrap(), "original");
        assert!(!new.exists());
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
