//! Native Rust GUI (`paseo-gui`, Makepad) builds for desktop, Android and iOS.
//! Mobile packages are remote-only clients and never contain the daemon.
use crate::{assets, build, common::*, notices};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    env, fs,
    io::Read,
    path::{Component, Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};
use walkdir::WalkDir;

pub const DESKTOP_APP_ID: &str = "sh.paseo.sessions.desktop";
pub const MOBILE_APP_ID: &str = "sh.paseo.sessions";
pub const MACOS_BUNDLE: &str = "Paseo Sessions.app";
const IOS_BUNDLE: &str = "Paseo Sessions.app";
const IOS_EXECUTABLE: &str = "paseo-sessions";
const CARGO_BIN: &str = "paseo-sessions";
/// Supported Android floor (Android 7.0), the same as the Flutter client.
/// The NDK linker for this API level has no stubs for newer system libraries.
const ANDROID_API: u32 = 24;
/// System libraries introduced after `ANDROID_API`: a `DT_NEEDED` entry for any
/// of them makes the library fail to load on older devices. makepad-platform
/// 1.0.0 links the first three unconditionally; the vendored copy loads them at
/// runtime instead.
const ANDROID_NEWER_LIBRARIES: [(&str, u32); 5] = [
    ("libaaudio.so", 26),
    ("libamidi.so", 29),
    ("libnativewindow.so", 26),
    ("libbinder_ndk.so", 29),
    ("libsync.so", 26),
];
const ANDROID_NDK: &str = "28.2.13676358";
const GRADLE_VERSION: &str = "9.9.0";
const GRADLE_SHA256: &str = "0b00b0ffe07c4d4789c12d4935a44a7057d55774b7540bd4a3f76f336deb177a";
const UPDATE_REPOSITORY: &str = "emotionbug/Passession";
const MACOS_MINIMUM: &str = "13.0";

/// Installed client executable relative to the desktop bundle root. The daemon
/// CLI (`paseo open`), shortcuts and the updater locate the client by these names.
pub fn desktop_executable(platform: &str) -> &'static str {
    match platform {
        "macos" => "Contents/MacOS/Paseo Sessions",
        "windows" => "paseo_sessions.exe",
        _ => "paseo_sessions",
    }
}
pub fn desktop_runtime(platform: &str) -> &'static str {
    if platform == "macos" {
        "Contents/Resources/runtime"
    } else {
        "runtime"
    }
}
/// Makepad's `MAKEPAD_PACKAGE_DIR` root (Linux/Windows: the working directory
/// the client switches to; macOS: the bundle resource path).
fn desktop_resources(platform: &str) -> &'static str {
    if platform == "macos" {
        "Contents/Resources"
    } else {
        ""
    }
}

/// Temporarily staged mobile credentials, restored on every exit path.
pub(crate) struct Credentials(Vec<(PathBuf, Option<Vec<u8>>, Option<fs::Permissions>)>);
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

/// Compile-time identity the client reads with `option_env!`.
struct Identity {
    version: String,
    channel: String,
    build_number: u64,
    source: String,
    release: bool,
}
impl Identity {
    fn resolve(target: &str) -> Result<Self> {
        let release = env::var("PASEO_NATIVE_RELEASE").as_deref() == Ok("1");
        let version = version()?;
        let (channel, source) = if release {
            let identity = build::release_identity()?;
            (
                identity["channel"]
                    .as_str()
                    .context("Release channel missing")?
                    .to_owned(),
                identity["sourceSha"]
                    .as_str()
                    .context("Release source missing")?
                    .to_owned(),
            )
        } else {
            let channel = env::var("PASEO_RELEASE_CHANNEL").unwrap_or_else(|_| {
                if version.contains('-') { "beta" } else { "stable" }.into()
            });
            (channel, capture(command("git")?.args(["rev-parse", "HEAD"]))?)
        };
        ensure!(
            matches!(channel.as_str(), "stable" | "beta"),
            "PASEO_RELEASE_CHANNEL must be stable or beta"
        );
        Ok(Self {
            version,
            channel,
            build_number: build_number(target)?,
            source,
            release,
        })
    }
    fn apply(&self, cargo: &mut Command) {
        cargo
            .env("PASEO_VERSION", &self.version)
            .env("PASEO_RELEASE_CHANNEL", &self.channel)
            .env("PASEO_UPDATE_REPOSITORY", UPDATE_REPOSITORY)
            .env("PASEO_BUILD_NUMBER", self.build_number.to_string())
            .env("PASEO_BUILD_SOURCE_SHA", &self.source);
        if self.release {
            cargo.env("PASEO_NATIVE_RELEASE", "1");
        } else {
            cargo.env_remove("PASEO_NATIVE_RELEASE");
        }
    }
}

fn cargo_profile(cargo: &mut Command, mode: &str) {
    cargo.args(["--profile", if mode == "debug" { "dev" } else { mode }]);
}

/// Fresh directory under the GUI output root; never a caller-chosen path.
fn fresh(path: &Path) -> Result<()> {
    ensure!(
        path.starts_with(gui_output()?),
        "Refusing to recreate {} outside the GUI output directory",
        path.display()
    );
    remove_if_exists(path)?;
    fs::create_dir_all(path)?;
    Ok(())
}

/// Copy every Makepad-aware resource directory to `<root>/makepad/<crate>/resources`,
/// leaving out the reviewed exclusions in `packaging/resource-notices.json`, and
/// fail if a client source references a resource that was not staged.
fn stage_resources(metadata: &Value, root: &Path) -> Result<Vec<notices::PackagedResource>> {
    let excluded = notices::excluded_resources()?;
    let mut packaged = Vec::new();
    for directory in notices::gui_resource_directories(metadata)? {
        let destination = root
            .join("makepad")
            .join(directory.name.replace('-', "_"))
            .join("resources");
        for entry in WalkDir::new(&directory.path)
            .follow_links(false)
            .sort_by_file_name()
            .min_depth(1)
        {
            let entry = entry?;
            let relative = entry.path().strip_prefix(&directory.path)?;
            ensure!(
                !entry.file_type().is_symlink(),
                "Resource links are not packaged: {}",
                entry.path().display()
            );
            if entry.file_type().is_dir() {
                continue;
            }
            let file = relative
                .components()
                .map(|part| match part {
                    Component::Normal(name) => name.to_str().context("Non-UTF-8 resource path"),
                    _ => bail!("Invalid resource path {}", relative.display()),
                })
                .collect::<Result<Vec<_>>>()?
                .join("/");
            let sha256 = sha256_file(entry.path())?;
            if notices::is_excluded(&excluded, &directory.name, &directory.version, &file, &sha256)? {
                continue;
            }
            copy_tree(entry.path(), &destination.join(relative))?;
            chmod(&destination.join(relative), 0o644)?;
            packaged.push(notices::PackagedResource {
                package: directory.name.clone(),
                version: directory.version.clone(),
                file,
                sha256,
            });
        }
    }
    ensure!(
        packaged.iter().any(|resource| resource.package == "makepad-widgets"),
        "makepad-widgets resources were not found in the paseo-gui dependency closure"
    );
    notices::ensure_references_staged(&packaged)?;
    Ok(packaged)
}

/// App-level license files: the GUI closure, packaged fonts and vendored components.
fn stage_notices(
    metadata: &Value,
    packaged: &[notices::PackagedResource],
    platform: &str,
    root: &Path,
) -> Result<()> {
    text(
        &root.join("THIRD-PARTY-NOTICES.txt"),
        notices::render_app(metadata, packaged, platform)?,
        0o644,
    )?;
    copy_tree(&crate::common::root()?.join("LICENSE"), &root.join("LICENSE"))?;
    chmod(&root.join("LICENSE"), 0o644)
}

/// Replace `$(NAME)` placeholders; any unknown placeholder is an error.
fn substitute(template: &str, values: &[(&str, &str)]) -> Result<String> {
    let mut output = template.to_owned();
    for (name, value) in values {
        output = output.replace(&format!("$({name})"), value);
    }
    if let Some(start) = output.find("$(") {
        let end = output[start..].find(')').map_or(output.len(), |end| start + end + 1);
        bail!("Unresolved placeholder {} in plist template", &output[start..end]);
    }
    Ok(output)
}

fn plist_value(plist: &Path, key: &str) -> Result<String> {
    capture(
        command("/usr/libexec/PlistBuddy")?
            .arg("-c")
            .arg(format!("Print :{key}"))
            .arg(plist),
    )
}

/// Set a plist key, replacing any existing value of another type.
fn plist_set(plist: &Path, key: &str, kind: &str, value: &str) -> Result<()> {
    let _ = Command::new("/usr/libexec/PlistBuddy")
        .arg("-c")
        .arg(format!("Delete :{key}"))
        .arg(plist)
        .output();
    run(command("/usr/libexec/PlistBuddy")?
        .arg("-c")
        .arg(format!("Add :{key} {kind} {value}"))
        .arg(plist))
}

/// Cargo feature of `paseo-gui` that compiles the in-process desktop lifecycle
/// driver used by `tools/native-desktop-smoke`. Never part of a package.
pub const SMOKE_FEATURE: &str = "desktop-smoke";

fn build_desktop(target: &str, mode: &str, identity: &Identity, smoke: bool) -> Result<PathBuf> {
    // macOS needs separate deployment targets; local builds need separate identity.
    let combined_release = identity.release && target != "macos";
    if !combined_release {
        build::build_native(mode, false)?;
    }
    let mut cargo = command(cargo())?;
    cargo
        .args(["build", "--locked", "-p", "paseo-gui", "--bin", CARGO_BIN])
        .env("MAKEPAD_PACKAGE_DIR", "makepad");
    if combined_release {
        cargo.args(["-p", "paseo-daemon", "--bin", "paseo"]);
    }
    if smoke {
        cargo.arg("--features").arg(format!("paseo-gui/{SMOKE_FEATURE}"));
    }
    cargo_profile(&mut cargo, mode);
    identity.apply(&mut cargo);
    if target == "macos" {
        cargo
            .env("MAKEPAD", "apple_bundle")
            .env("MACOSX_DEPLOYMENT_TARGET", MACOS_MINIMUM);
    }
    if combined_release {
        build::build_desktop_release(mode, &mut cargo)?;
    } else {
        run(&mut cargo)?;
    }
    let compiled = target_root()?
        .join(mode)
        .join(format!("{CARGO_BIN}{}", env::consts::EXE_SUFFIX));
    build::assert_architecture(&compiled)?;
    let output = gui_output()?
        .join("desktop")
        .join(format!("{target}-{}", arch()?))
        .join(if smoke {
            format!("smoke-{mode}")
        } else {
            mode.to_owned()
        });
    fresh(&output)?;
    let bundle = output.join(if target == "macos" { MACOS_BUNDLE } else { "bundle" });
    let executable = bundle.join(desktop_executable(target));
    copy_tree(&compiled, &executable)?;
    chmod(&executable, 0o755)?;
    let resources = bundle.join(desktop_resources(target));
    if target == "macos" {
        let template = fs::read_to_string(packaging()?.join("desktop/macos/Info.plist"))?;
        let build_number = identity.build_number.to_string();
        text(
            &bundle.join("Contents/Info.plist"),
            substitute(
                &template,
                &[
                    ("PASEO_VERSION", identity.version.as_str()),
                    ("PASEO_BUILD_NUMBER", build_number.as_str()),
                    ("MACOS_MINIMUM", MACOS_MINIMUM),
                ],
            )?,
            0o644,
        )?;
        ensure!(
            plist_value(&bundle.join("Contents/Info.plist"), "CFBundleIdentifier")? == DESKTOP_APP_ID,
            "macOS Info.plist template lost the {DESKTOP_APP_ID} bundle identifier"
        );
        text(&bundle.join("Contents/PkgInfo"), "APPL????", 0o644)?;
        fs::create_dir_all(&resources)?;
        run(command("iconutil")?
            .args(["-c", "icns"])
            .arg(packaging()?.join("desktop/macos/AppIcon.iconset"))
            .arg("-o")
            .arg(resources.join("AppIcon.icns")))?;
    }
    let metadata = notices::cargo_metadata(notices::host_triple()?)?;
    let packaged = stage_resources(&metadata, &resources)?;
    stage_notices(&metadata, &packaged, target, &resources)?;
    build::bundle_runtime(&bundle.join(desktop_runtime(target)), mode, "desktop-bundled")?;
    // A smoke bundle carries a test driver: it gets no build descriptor, so
    // package-desktop can never pick it up.
    if !smoke {
        fs::create_dir_all(release()?)?;
        write_json(
            &release()?.join(format!("build-{target}-{}.json", arch()?)),
            &json!({"version":identity.version,"platform":target,"arch":arch()?,"mode":mode,"output":bundle.strip_prefix(gui_output()?)?.to_string_lossy().replace('\\', "/")}),
        )?;
    }
    Ok(bundle)
}

struct AndroidAbi {
    abi: &'static str,
    triple: &'static str,
    clang: &'static str,
    /// ELF class (1 = 32-bit, 2 = 64-bit) and e_machine.
    class: u8,
    machine: u16,
}
/// 64-bit ABIs only: the Ghostty terminal bindings (libghostty-vt-sys) assert
/// a 64-bit layout, so `armeabi-v7a` cannot be built.
static ANDROID_ABIS: [AndroidAbi; 2] = [
    AndroidAbi {
        abi: "arm64-v8a",
        triple: "aarch64-linux-android",
        clang: "aarch64-linux-android",
        class: 2,
        machine: 183,
    },
    AndroidAbi {
        abi: "x86_64",
        triple: "x86_64-linux-android",
        clang: "x86_64-linux-android",
        class: 2,
        machine: 62,
    },
];

fn android_abis(arch: &str) -> Result<&'static [AndroidAbi]> {
    match arch {
        "arm64" => Ok(&ANDROID_ABIS[..1]),
        "x64" => Ok(&ANDROID_ABIS[1..]),
        "universal" => Ok(&ANDROID_ABIS),
        "arm" => bail!(
            "32-bit Android (armeabi-v7a) is not supported: the Ghostty terminal library is 64-bit only; use --arch arm64|x64|universal"
        ),
        _ => bail!("Unknown Android architecture: {arch}; expected arm64|x64|universal"),
    }
}

fn android_sdk() -> Result<PathBuf> {
    let home = env::var_os("ANDROID_HOME").filter(|value| !value.is_empty());
    let root = env::var_os("ANDROID_SDK_ROOT").filter(|value| !value.is_empty());
    if let (Some(home), Some(root)) = (&home, &root) {
        ensure!(
            fs::canonicalize(home).ok() == fs::canonicalize(root).ok(),
            "ANDROID_HOME and ANDROID_SDK_ROOT select different SDKs"
        );
    }
    let sdk = PathBuf::from(home.or(root).context("Set ANDROID_HOME to the Android SDK")?);
    ensure!(sdk.is_dir(), "Android SDK not found: {}", sdk.display());
    Ok(sdk)
}

fn android_ndk(sdk: &Path) -> Result<PathBuf> {
    let ndk = env::var_os("ANDROID_NDK_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| sdk.join("ndk").join(ANDROID_NDK));
    let properties = fs::read_to_string(ndk.join("source.properties"))
        .with_context(|| format!("Install Android NDK {ANDROID_NDK} at {}", ndk.display()))?;
    ensure!(
        properties
            .lines()
            .any(|line| line.replace(' ', "") == format!("Pkg.Revision={ANDROID_NDK}")),
        "Android NDK at {} is not {ANDROID_NDK}",
        ndk.display()
    );
    Ok(ndk)
}

fn elf_identity(path: &Path) -> Result<(u8, u16)> {
    let mut header = [0; 20];
    fs::File::open(path)?
        .read_exact(&mut header)
        .with_context(|| format!("Truncated ELF {}", path.display()))?;
    ensure!(
        header[..4] == *b"\x7fELF" && header[5] == 1,
        "Not a little-endian ELF: {}",
        path.display()
    );
    Ok((header[4], u16::from_le_bytes([header[18], header[19]])))
}

/// `DT_NEEDED` entries of a little-endian ELF32/ELF64 shared library, read
/// through `PT_DYNAMIC` and the `PT_LOAD` mapping of `DT_STRTAB`.
fn elf_needed(image: &[u8]) -> Result<Vec<String>> {
    let malformed = || anyhow::anyhow!("Malformed ELF dynamic section");
    ensure!(
        image.get(..4) == Some(b"\x7fELF".as_slice()) && image.get(5) == Some(&1),
        "Not a little-endian ELF"
    );
    let wide = match image[4] {
        1 => false,
        2 => true,
        class => bail!("Unknown ELF class {class}"),
    };
    let word = |at: usize| -> Result<u64> {
        Ok(if wide {
            u64::from_le_bytes(image.get(at..at + 8).ok_or_else(malformed)?.try_into()?)
        } else {
            u64::from(u32::from_le_bytes(image.get(at..at + 4).ok_or_else(malformed)?.try_into()?))
        })
    };
    let half = |at: usize| -> Result<usize> {
        Ok(usize::from(u16::from_le_bytes(
            image.get(at..at + 2).ok_or_else(malformed)?.try_into()?,
        )))
    };
    let size = if wide { 8 } else { 4 };
    let program_offset = word(if wide { 32 } else { 28 })? as usize;
    let (entry_size, count) = if wide { (half(54)?, half(56)?) } else { (half(42)?, half(44)?) };
    // (type, file offset, virtual address, file size) of every program header.
    let mut segments = Vec::with_capacity(count);
    for index in 0..count {
        let header = program_offset
            .checked_add(index.checked_mul(entry_size).ok_or_else(malformed)?)
            .ok_or_else(malformed)?;
        let kind = u32::from_le_bytes(image.get(header..header + 4).ok_or_else(malformed)?.try_into()?);
        let (offset, address, file_size) = if wide {
            (word(header + 8)?, word(header + 16)?, word(header + 32)?)
        } else {
            (word(header + 4)?, word(header + 8)?, word(header + 16)?)
        };
        segments.push((kind, offset, address, file_size));
    }
    let Some(&(_, dynamic_offset, _, dynamic_size)) = segments.iter().find(|segment| segment.0 == 2) else {
        return Ok(Vec::new());
    };
    let mut needed = Vec::new();
    let mut string_table = None;
    let mut at = dynamic_offset as usize;
    let end = at.checked_add(dynamic_size as usize).ok_or_else(malformed)?;
    while at + 2 * size <= end {
        let (tag, value) = (word(at)?, word(at + size)?);
        match tag {
            0 => break,
            1 => needed.push(value),
            5 => string_table = Some(value),
            _ => {}
        }
        at += 2 * size;
    }
    if needed.is_empty() {
        return Ok(Vec::new());
    }
    let address = string_table.ok_or_else(malformed)?;
    let table = segments
        .iter()
        .find(|&&(kind, _, start, length)| kind == 1 && address >= start && address < start + length)
        .map(|&(_, offset, start, _)| (address - start + offset) as usize)
        .ok_or_else(malformed)?;
    needed
        .into_iter()
        .map(|name| {
            let start = table.checked_add(name as usize).ok_or_else(malformed)?;
            let length = image
                .get(start..)
                .and_then(|rest| rest.iter().position(|&byte| byte == 0))
                .ok_or_else(malformed)?;
            Ok(String::from_utf8(image[start..start + length].to_vec())?)
        })
        .collect()
}

/// Reject a library that would not load on the supported Android floor.
fn ensure_android_floor(library: &Path) -> Result<()> {
    let needed = elf_needed(&fs::read(library)?)
        .with_context(|| format!("Reading dynamic dependencies of {}", library.display()))?;
    let newer: Vec<String> = needed
        .iter()
        .filter_map(|name| {
            ANDROID_NEWER_LIBRARIES
                .iter()
                .find(|(library, _)| library == name)
                .map(|(library, api)| format!("{library} (API {api})"))
        })
        .collect();
    ensure!(
        newer.is_empty(),
        "{} links system libraries newer than Android API {ANDROID_API}: {}. Load them at runtime instead (packages/gui-platform/vendor/makepad-platform).",
        library.display(),
        newer.join(", ")
    );
    Ok(())
}

/// Pinned Gradle distribution, verified and unpacked once into the user cache.
fn gradle() -> Result<PathBuf> {
    let directory = cache()?.join("paseo-packaging");
    let home = directory.join(format!("gradle-{GRADLE_VERSION}"));
    let launcher = home
        .join("bin")
        .join(if cfg!(windows) { "gradle.bat" } else { "gradle" });
    if launcher.is_file() {
        return Ok(launcher);
    }
    let archive = directory.join(format!("gradle-{GRADLE_VERSION}-bin.zip"));
    assets::download(
        &format!("https://services.gradle.org/distributions/gradle-{GRADLE_VERSION}-bin.zip"),
        &archive,
        GRADLE_SHA256,
        false,
    )?;
    let staging = tempfile::Builder::new()
        .prefix("gradle-")
        .tempdir_in(&directory)?;
    let mut zip = zip::ZipArchive::new(fs::File::open(&archive)?)?;
    for index in 0..zip.len() {
        let mut member = zip.by_index(index)?;
        let relative = member
            .enclosed_name()
            .context("Unsafe Gradle distribution member")?;
        let path = staging.path().join(relative);
        if member.is_dir() {
            fs::create_dir_all(&path)?;
            continue;
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        std::io::copy(&mut member, &mut fs::File::create(&path)?)?;
        if let Some(mode) = member.unix_mode() {
            chmod(&path, mode & 0o777)?;
        }
    }
    let unpacked = staging.path().join(format!("gradle-{GRADLE_VERSION}"));
    ensure!(
        unpacked.join("bin").is_dir(),
        "Incomplete Gradle distribution"
    );
    remove_if_exists(&home)?;
    fs::rename(unpacked, &home)?;
    ensure!(launcher.is_file(), "Gradle launcher missing after unpacking");
    Ok(launcher)
}

fn release_key(android: &Path) -> Result<bool> {
    let properties = android.join("key.properties");
    let signing = if properties.is_file() {
        fs::read_to_string(properties)?
    } else {
        String::new()
    };
    Ok(
        env::var("ANDROID_KEYSTORE_PATH").is_ok_and(|value| !value.trim().is_empty())
            || regex::Regex::new(r"(?m)^\s*storeFile\s*(?:=|:|\s)\s*\S")?.is_match(&signing),
    )
}

/// Build the APK or AAB for `abis`; returns Gradle's output inside the GUI work directory.
fn build_android(mode: &str, format: &str, abis: &[AndroidAbi], identity: &Identity) -> Result<PathBuf> {
    let sdk = android_sdk()?;
    let ndk = android_ndk(&sdk)?;
    let host = match platform()? {
        "linux" => "linux-x86_64",
        "macos" => "darwin-x86_64",
        _ => "windows-x86_64",
    };
    let tools = ndk.join("toolchains/llvm/prebuilt").join(host).join("bin");
    let work = gui_output()?.join("android").join(mode);
    fresh(&work.join("jniLibs"))?;
    fresh(&work.join("assets"))?;
    for abi in abis {
        let clang = tools.join(format!(
            "{}{ANDROID_API}-clang{}",
            abi.clang,
            if cfg!(windows) { ".cmd" } else { "" }
        ));
        let archiver = tools.join(format!("llvm-ar{}", env::consts::EXE_SUFFIX));
        ensure!(clang.is_file(), "NDK linker missing: {}", clang.display());
        let underscored = abi.triple.replace('-', "_");
        let mut cargo = command(cargo())?;
        cargo
            .args([
                "rustc",
                "--locked",
                "-p",
                "paseo-gui",
                "--lib",
                "--crate-type",
                "cdylib",
                "--target",
                abi.triple,
            ])
            .env(
                format!("CARGO_TARGET_{}_LINKER", underscored.to_ascii_uppercase()),
                &clang,
            )
            .env(format!("CC_{underscored}"), &clang)
            .env(format!("AR_{underscored}"), &archiver)
            .env("ANDROID_NDK_HOME", &ndk)
            .env("ANDROID_HOME", &sdk);
        cargo_profile(&mut cargo, mode);
        identity.apply(&mut cargo);
        run(&mut cargo)?;
        let library = target_root()?
            .join(abi.triple)
            .join(mode)
            .join("libpaseo_gui.so");
        ensure!(
            elf_identity(&library)? == (abi.class, abi.machine),
            "{} is not an {} library",
            library.display(),
            abi.abi
        );
        ensure_android_floor(&library)?;
        // MakepadActivity loads "makepad".
        copy_tree(&library, &work.join("jniLibs").join(abi.abi).join("libmakepad.so"))?;
    }
    let metadata = notices::cargo_metadata(abis[0].triple)?;
    let packaged = stage_resources(&metadata, &work.join("assets"))?;
    stage_notices(&metadata, &packaged, "android", &work.join("assets"))?;

    let android = packaging()?.join("android");
    let mut credentials = Credentials(Vec::new());
    let result = (|| -> Result<PathBuf> {
        if let Some(source) = env::var_os("GOOGLE_SERVICES_FILE_SESSIONS") {
            let source = absolute(source)?;
            let config = read_json(&source)?;
            ensure!(
                config["client"]
                    .as_array()
                    .is_some_and(|clients| clients.iter().any(|client| client["client_info"]
                        ["android_client_info"]["package_name"]
                        == MOBILE_APP_ID)),
                "Firebase configuration is not for {MOBILE_APP_ID}"
            );
            credentials.stage(&source, android.join("app/google-services.json"))?;
        }
        // Rust's dev-fast profile packages as an Android debug variant.
        let variant = if mode == "release" { "release" } else { "debug" };
        let task = match (format, variant) {
            ("apk", "release") => "assembleRelease",
            ("apk", _) => "assembleDebug",
            ("aab", "release") => "bundleRelease",
            ("aab", _) => "bundleDebug",
            _ => bail!("Unknown Android format: {format}"),
        };
        let gradle_build = work.join("gradle");
        run(command(gradle()?)?
            .current_dir(&android)
            .args(["--no-daemon", "--console=plain", task])
            .arg("--project-cache-dir")
            .arg(work.join("gradle-cache"))
            .arg(format!("-PpaseoBuildDir={}", gradle_build.display()))
            .arg(format!("-PpaseoVersionName={}", identity.version))
            .arg(format!("-PpaseoVersionCode={}", identity.build_number))
            .arg(format!("-PpaseoJniLibs={}", work.join("jniLibs").display()))
            .arg(format!("-PpaseoAssets={}", work.join("assets").display()))
            .arg(format!("-PpaseoNdkPath={}", ndk.display()))
            .arg(format!(
                "-PpaseoAbis={}",
                abis.iter().map(|abi| abi.abi).collect::<Vec<_>>().join(",")
            ))
            .env("ANDROID_HOME", &sdk)
            .env("ANDROID_SDK_ROOT", &sdk))?;
        let built = if format == "aab" {
            gradle_build.join(format!("app/outputs/bundle/{variant}/app-{variant}.aab"))
        } else {
            gradle_build.join(format!("app/outputs/apk/{variant}/app-{variant}.apk"))
        };
        ensure!(built.is_file(), "Gradle produced no {format} at {}", built.display());
        Ok(built)
    })();
    let cleanup = credentials.restore();
    match (result, cleanup) {
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("Credential restoration also failed: {cleanup:#}")))
        }
        (Err(error), _) | (_, Err(error)) => Err(error),
        (Ok(output), Ok(())) => Ok(output),
    }
}

/// Keep Gradle's output in place for incremental builds; publish an independent artifact copy.
fn android_artifact(
    built: &Path,
    mode: &str,
    format: &str,
    android_arch: &str,
    identity: &Identity,
) -> Result<PathBuf> {
    let signing = if mode != "release" || !release_key(&packaging()?.join("android"))? {
        "-debug-signed"
    } else {
        ""
    };
    fs::create_dir_all(release()?)?;
    let output = release()?.join(format!(
        "{}{signing}.{format}",
        artifact_stem("android", android_arch)?
    ));
    copy_tree(built, &output)?;
    let suffix = if format == "aab" { "-aab" } else { "" };
    write_json(
        &release()?.join(format!("packages-android-{android_arch}{suffix}.json")),
        &json!({
            "version": identity.version,
            "platform": "android",
            "arch": android_arch,
            "format": format,
            "profile": mode,
            "sourceSha": identity.source,
            "artifacts": [output.file_name().and_then(|name| name.to_str()).context("Android artifact has no UTF-8 filename")?],
        }),
    )?;
    Ok(output)
}

fn ios_template() -> Result<PathBuf> {
    Ok(root()?.join("packages/gui-platform/ios"))
}

/// Assemble the iOS bundle; `simulator` selects a simulator triple.
fn build_ios(mode: &str, identity: &Identity, simulator: bool) -> Result<PathBuf> {
    ensure!(platform()? == "macos", "iOS builds require macOS and Xcode");
    let triple = match (simulator, arch()?) {
        (false, _) => "aarch64-apple-ios",
        (true, "arm64") => "aarch64-apple-ios-sim",
        (true, _) => bail!(
            "The iOS simulator build needs an Apple-silicon Mac: Ghostty ships no x86_64 simulator library"
        ),
    };
    let work = gui_output()?.join("ios").join(triple).join(mode);
    fresh(&work)?;
    let build_number = identity.build_number.to_string();
    let plist = work.join("Info.plist");
    text(
        &plist,
        substitute(
            &fs::read_to_string(ios_template()?.join("Info.plist"))?,
            &[
                ("PASEO_VERSION", identity.version.as_str()),
                ("PASEO_BUILD_NUMBER", build_number.as_str()),
                ("EXECUTABLE_NAME", IOS_EXECUTABLE),
            ],
        )?,
        0o644,
    )?;
    ensure!(
        plist_value(&plist, "CFBundleIdentifier")? == MOBILE_APP_ID
            && plist_value(&plist, "CFBundleExecutable")? == IOS_EXECUTABLE,
        "iOS Info.plist must declare {MOBILE_APP_ID} and executable {IOS_EXECUTABLE}"
    );
    let minimum = plist_value(&plist, "MinimumOSVersion")?;
    let mut cargo = command(cargo())?;
    cargo
        .args(["build", "--locked", "-p", "paseo-gui", "--bin", CARGO_BIN, "--target", triple])
        .env("IPHONEOS_DEPLOYMENT_TARGET", &minimum);
    cargo_profile(&mut cargo, mode);
    identity.apply(&mut cargo);
    run(&mut cargo)?;
    let app = work.join(IOS_BUNDLE);
    fs::create_dir_all(&app)?;
    let executable = app.join(IOS_EXECUTABLE);
    copy_tree(
        &target_root()?
            .join(triple)
            .join(mode)
            .join(CARGO_BIN),
        &executable,
    )?;
    chmod(&executable, 0o755)?;
    fs::rename(&plist, app.join("Info.plist"))?;
    let partial = work.join("assets-partial.plist");
    run(command("xcrun")?
        .arg("actool")
        .arg(packaging()?.join("ios/Assets.xcassets"))
        .arg("--compile")
        .arg(&app)
        .args([
            "--platform",
            if simulator { "iphonesimulator" } else { "iphoneos" },
            "--minimum-deployment-target",
            minimum.as_str(),
            "--app-icon",
            "AppIcon",
            "--target-device",
            "iphone",
            "--target-device",
            "ipad",
            "--output-format",
            "human-readable-text",
            "--output-partial-info-plist",
        ])
        .arg(&partial))?;
    run(command("/usr/libexec/PlistBuddy")?
        .arg("-c")
        .arg(format!("Merge {}", partial.display()))
        .arg(app.join("Info.plist")))?;
    let metadata = notices::cargo_metadata(triple)?;
    let packaged = stage_resources(&metadata, &app)?;
    stage_notices(&metadata, &packaged, "ios", &app)?;
    Ok(app)
}

/// Device signing from a provisioning profile; returns `false` when no
/// profile was configured and the bundle stays unsigned.
fn sign_ios(app: &Path, simulator: bool) -> Result<bool> {
    if simulator {
        run(command("codesign")?
            .args(["--force", "--sign", "-", "--timestamp=none"])
            .arg(app))?;
        return Ok(true);
    }
    let Some(profile) = env::var_os("PASEO_IOS_PROVISIONING_PROFILE").filter(|v| !v.is_empty())
    else {
        return Ok(false);
    };
    let identity = env::var("PASEO_IOS_SIGNING_IDENTITY")
        .ok()
        .filter(|value| !value.is_empty())
        .context("PASEO_IOS_PROVISIONING_PROFILE requires PASEO_IOS_SIGNING_IDENTITY")?;
    let profile = absolute(profile)?;
    let work = tempfile::tempdir()?;
    let decoded = work.path().join("profile.plist");
    text(
        &decoded,
        capture(command("security")?.args(["cms", "-D", "-i"]).arg(&profile))?,
        0o600,
    )?;
    let team = plist_value(&decoded, "TeamIdentifier:0")?;
    let application = plist_value(&decoded, "Entitlements:application-identifier")?;
    ensure!(
        application == format!("{team}.{MOBILE_APP_ID}") || application == format!("{team}.*"),
        "Provisioning profile is for {application}, not {team}.{MOBILE_APP_ID}"
    );
    let entitlements = work.path().join("entitlements.plist");
    copy_tree(&ios_template()?.join("Paseo.entitlements"), &entitlements)?;
    plist_set(
        &entitlements,
        "application-identifier",
        "string",
        &format!("{team}.{MOBILE_APP_ID}"),
    )?;
    plist_set(&entitlements, "com.apple.developer.team-identifier", "string", &team)?;
    plist_set(
        &entitlements,
        "get-task-allow",
        "bool",
        &plist_value(&decoded, "Entitlements:get-task-allow")?,
    )?;
    if plist_value(&entitlements, "aps-environment").is_ok() {
        let environment = plist_value(&decoded, "Entitlements:aps-environment")
            .context("The app requests push notifications; the provisioning profile lacks aps-environment")?;
        plist_set(&entitlements, "aps-environment", "string", &environment)?;
    }
    copy_tree(&profile, &app.join("embedded.mobileprovision"))?;
    run(command("codesign")?
        .args(["--force", "--sign", identity.as_str(), "--entitlements"])
        .arg(&entitlements)
        .arg(app))?;
    run(command("codesign")?
        .args(["--verify", "--deep", "--strict"])
        .arg(app))?;
    Ok(true)
}

fn package_ios(app: &Path, signed: bool) -> Result<PathBuf> {
    fs::create_dir_all(release()?)?;
    if signed {
        let stage = tempfile::tempdir()?;
        let payload = stage.path().join("Payload");
        fs::create_dir_all(&payload)?;
        run(command("ditto")?.arg(app).arg(payload.join(IOS_BUNDLE)))?;
        let output = release()?.join(format!("{}.ipa", artifact_stem("ios", "arm64")?));
        run(command("ditto")?
            .args(["-c", "-k", "--sequesterRsrc", "--keepParent"])
            .arg(&payload)
            .arg(&output))?;
        return Ok(output);
    }
    eprintln!(
        "No PASEO_IOS_PROVISIONING_PROFILE: building an explicitly unsigned device .app, not an installable IPA"
    );
    let output = release()?.join(format!("{}-unsigned.app.zip", artifact_stem("ios", "arm64")?));
    run(command("ditto")?
        .args(["-c", "-k", "--sequesterRsrc", "--keepParent"])
        .arg(app)
        .arg(&output))?;
    Ok(output)
}

/// Environment of a launched development client: the checkout's dev home,
/// the dev daemon endpoint and a private client state directory.
fn development_launch(process: &mut Command) -> Result<()> {
    let home = root()?.join(".dev/paseo-home");
    let state = root()?.join(".dev/gui-state");
    fs::create_dir_all(&home)?;
    fs::create_dir_all(&state)?;
    process
        .env("PASEO_HOME", home)
        .env(
            "PASEO_LISTEN",
            env::var("PASEO_LISTEN").unwrap_or_else(|_| "127.0.0.1:6768".into()),
        )
        .env("PASEO_NODE_ENV", "development")
        .env("PASEO_GUI_STATE_DIR", state);
    Ok(())
}

fn adb() -> Result<PathBuf> {
    let bundled = android_sdk()?
        .join("platform-tools")
        .join(format!("adb{}", env::consts::EXE_SUFFIX));
    if bundled.is_file() {
        return Ok(bundled);
    }
    which::which("adb").context("adb not found in the Android SDK platform-tools or PATH")
}

/// `adb devices -l` entries that are ready (`device` state): (serial, description).
fn adb_devices(listing: &str) -> Vec<(String, String)> {
    listing
        .lines()
        .skip_while(|line| !line.starts_with("List of devices"))
        .skip(1)
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let serial = fields.next()?;
            (fields.next()? == "device")
                .then(|| (serial.to_owned(), fields.collect::<Vec<_>>().join(" ")))
        })
        .collect()
}

fn select<'a>(
    target: &str,
    candidates: &'a [(String, String)],
    requested: Option<&str>,
) -> Result<&'a (String, String)> {
    let found = match requested {
        Some(id) => candidates
            .iter()
            .find(|(serial, name)| serial == id || name == id),
        None if candidates.len() == 1 => candidates.first(),
        None => None,
    };
    found.with_context(|| {
        format!(
            "Select an attached {target} device using --device ID; available: {}",
            candidates
                .iter()
                .map(|(serial, name)| format!("{name} ({serial})"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    })
}

/// The attached device to run on and its primary ABI, which selects the only
/// library built for a development run (emulators are usually x86_64).
fn android_device(adb: &Path, device: Option<&str>) -> Result<(String, &'static AndroidAbi)> {
    let devices = adb_devices(&capture(command(adb)?.args(["devices", "-l"]))?);
    let (serial, _) = select("Android", &devices, device)?;
    let reported = capture(command(adb)?.args(["-s", serial.as_str(), "shell", "getprop", "ro.product.cpu.abi"]))?;
    let abi = ANDROID_ABIS
        .iter()
        .find(|abi| abi.abi == reported)
        .with_context(|| {
            format!("Unsupported Android ABI {reported} on {serial}; Paseo Sessions builds 64-bit arm64-v8a and x86_64 only")
        })?;
    Ok((serial.clone(), abi))
}

fn run_android(adb: &Path, serial: &str, apk: &Path) -> Result<()> {
    run(command(adb)?.args(["-s", serial, "install", "-r"]).arg(apk))?;
    let component = capture(command(adb)?.args([
        "-s",
        serial,
        "shell",
        "cmd",
        "package",
        "resolve-activity",
        "--brief",
        "-c",
        "android.intent.category.LAUNCHER",
        MOBILE_APP_ID,
    ]))?;
    let component = component
        .lines()
        .last()
        .filter(|line| line.starts_with(&format!("{MOBILE_APP_ID}/")))
        .context("The installed package has no launcher activity")?
        .to_owned();
    run(command(adb)?.args(["-s", serial, "shell", "am", "start", "-W", "-n", component.as_str()]))?;
    let started = Instant::now();
    let pid = loop {
        let output = command(adb)?
            .args(["-s", serial, "shell", "pidof", MOBILE_APP_ID])
            .output()?;
        let pid = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if output.status.success() && !pid.is_empty() {
            break pid;
        }
        ensure!(
            started.elapsed() < Duration::from_secs(20),
            "{MOBILE_APP_ID} did not start on {serial}"
        );
        thread::sleep(Duration::from_millis(250));
    };
    run(command(adb)?.args(["-s", serial, "logcat", "-v", "brief", "--pid", pid.as_str()]))
}

fn ios_devices(simulator: bool) -> Result<Vec<(String, String)>> {
    let mut devices = Vec::new();
    if simulator {
        let listing: Value = serde_json::from_str(&capture(
            command("xcrun")?.args(["simctl", "list", "devices", "booted", "--json"]),
        )?)?;
        for (runtime, entries) in listing["devices"]
            .as_object()
            .context("Invalid simctl device list")?
        {
            if !runtime.contains("iOS") {
                continue;
            }
            for entry in entries.as_array().into_iter().flatten() {
                if entry["state"] == "Booted" {
                    devices.push((
                        entry["udid"].as_str().context("Simulator lacks udid")?.to_owned(),
                        entry["name"].as_str().unwrap_or_default().to_owned(),
                    ));
                }
            }
        }
    } else {
        let output = tempfile::NamedTempFile::new()?;
        run(command("xcrun")?
            .args(["devicectl", "list", "devices", "--json-output"])
            .arg(output.path()))?;
        let listing = read_json(output.path())?;
        for entry in listing["result"]["devices"].as_array().into_iter().flatten() {
            if entry["hardwareProperties"]["platform"] == "iOS" {
                devices.push((
                    entry["identifier"]
                        .as_str()
                        .context("Device lacks identifier")?
                        .to_owned(),
                    entry["deviceProperties"]["name"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                ));
            }
        }
    }
    Ok(devices)
}

/// Select one connected device or booted simulator; `true` marks a simulator.
fn ios_target(device: Option<&str>) -> Result<(bool, String)> {
    let physical = ios_devices(false)?;
    let mut all = physical.clone();
    all.extend(ios_devices(true)?);
    let (serial, _) = select("iOS", &all, device)?;
    Ok((!physical.iter().any(|(id, _)| id == serial), serial.clone()))
}

fn run_ios(app: &Path, simulator: bool, device: &str) -> Result<()> {
    if simulator {
        run(command("xcrun")?.args(["simctl", "install", device]).arg(app))?;
        return run(command("xcrun")?.args([
            "simctl",
            "launch",
            "--console-pty",
            "--terminate-running-process",
            device,
            MOBILE_APP_ID,
        ]));
    }
    run(command("xcrun")?
        .args(["devicectl", "device", "install", "app", "--device", device])
        .arg(app))?;
    run(command("xcrun")?.args([
        "devicectl",
        "device",
        "process",
        "launch",
        "--terminate-existing",
        "--console",
        "--device",
        device,
        MOBILE_APP_ID,
    ]))
}

pub fn build_gui(args: &[String]) -> Result<()> {
    supervise_subprocesses()?;
    let mut lease = crate::build_cache::acquire()?;
    let mut target = None;
    let mut mode = "release";
    let mut launch = false;
    let mut smoke = false;
    let mut device = None;
    let mut android_arch = None;
    let mut format = "apk";
    let mut explicit_format = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--debug" => mode = "debug",
            "--run" => launch = true,
            "--smoke" => smoke = true,
            "--device" | "--arch" | "--profile" | "--format" => {
                let option = &args[index];
                index += 1;
                let value = args
                    .get(index)
                    .filter(|arg| !arg.starts_with("--"))
                    .context("Missing option value")?;
                match option.as_str() {
                    "--device" => device = Some(value.as_str()),
                    "--arch" => android_arch = Some(value.as_str()),
                    "--profile" => mode = value,
                    "--format" => {
                        format = value;
                        explicit_format = true;
                    }
                    _ => unreachable!(),
                }
            }
            value if !value.starts_with('-') && target.is_none() => target = Some(value),
            other => bail!("Unknown build-gui argument: {other}"),
        }
        index += 1;
    }
    let target = target.unwrap_or(platform()?);
    let desktop = matches!(target, "linux" | "macos" | "windows");
    ensure!(
        desktop || matches!(target, "android" | "ios"),
        "Unknown GUI platform: {target}"
    );
    ensure!(
        !desktop || target == platform()?,
        "Desktop builds require a native host of the target platform"
    );
    ensure!(
        env::var_os("CARGO_BUILD_TARGET").is_none(),
        "CARGO_BUILD_TARGET is not supported; xtask selects the GUI target"
    );
    ensure!(
        matches!(mode, "dev-fast" | "debug" | "release"),
        "--profile must be dev-fast|debug|release"
    );
    ensure!(
        android_arch.is_none() || target == "android" && !launch,
        "--arch arm64|x64|universal is only supported for Android artifact builds, not --run"
    );
    ensure!(
        matches!(format, "apk" | "aab") && (!explicit_format || target == "android"),
        "--format apk|aab is only supported for Android"
    );
    ensure!(!launch || format == "apk", "--run requires APK output, not an Android App Bundle");
    let android_arch = android_arch.unwrap_or(if format == "aab" { "universal" } else { "arm64" });
    let abis = android_abis(android_arch)?;
    ensure!(
        device.is_none() || launch && !desktop,
        "--device requires --run on Android or iOS"
    );
    ensure!(
        !smoke || desktop && !launch && env::var("PASEO_NATIVE_RELEASE").as_deref() != Ok("1"),
        "--smoke builds an unpackaged desktop bundle with the lifecycle test driver for tools/native-desktop-smoke; it cannot be combined with --run or a release identity"
    );
    ensure!(
        mode == "release" || env::var("PASEO_NATIVE_RELEASE").as_deref() != Ok("1"),
        "PASEO_NATIVE_RELEASE=1 requires a release GUI build"
    );
    require_zig()?;
    let identity = Identity::resolve(target)?;
    match target {
        "android" if launch => {
            let adb = adb()?;
            let (serial, abi) = android_device(&adb, device)?;
            let apk = build_android(mode, "apk", std::slice::from_ref(abi), &identity)?;
            lease.release_target();
            run_android(&adb, &serial, &apk)?;
        }
        "android" => {
            let built = build_android(mode, format, abis, &identity)?;
            let artifact = android_artifact(&built, mode, format, android_arch, &identity)?;
            println!("GUI android {mode} {format}: {}", artifact.display());
        }
        "ios" => {
            let (simulator, selected) = if launch {
                let (simulator, selected) = ios_target(device)?;
                (simulator, Some(selected))
            } else {
                (false, None)
            };
            let app = build_ios(mode, &identity, simulator)?;
            let signed = sign_ios(&app, simulator)?;
            match selected {
                Some(selected) => {
                    ensure!(
                        signed,
                        "Running on an iOS device requires PASEO_IOS_PROVISIONING_PROFILE and PASEO_IOS_SIGNING_IDENTITY"
                    );
                    lease.release_target();
                    run_ios(&app, simulator, &selected)?;
                }
                None => println!("GUI ios {mode}: {}", package_ios(&app, signed)?.display()),
            }
        }
        _ => {
            let bundle = build_desktop(target, mode, &identity, smoke)?;
            println!("GUI {target} {mode}{}: {}", if smoke { " smoke" } else { "" }, bundle.display());
            lease.release_target();
            if launch {
                let mut process = command(bundle.join(desktop_executable(target)))?;
                process.current_dir(&bundle);
                development_launch(&mut process)?;
                run(&mut process)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn plist_templates_reject_unresolved_placeholders() {
        assert_eq!(
            substitute("<string>$(PASEO_VERSION)</string>", &[("PASEO_VERSION", "1.2.3")]).unwrap(),
            "<string>1.2.3</string>"
        );
        assert!(substitute("$(PASEO_VERSION) $(PRODUCT_NAME)", &[("PASEO_VERSION", "1")]).is_err());
    }
    #[test]
    fn adb_selection_ignores_unready_devices_and_requires_choice() {
        let listing = "* daemon started successfully\nList of devices attached\nemulator-5554 device product:sdk model:Pixel_8\nR5CT offline\nZX1 unauthorized usb:1-1\n9A device usb:2-1 model:Phone\n\n";
        let devices = adb_devices(listing);
        assert_eq!(
            devices.iter().map(|(serial, _)| serial.as_str()).collect::<Vec<_>>(),
            ["emulator-5554", "9A"]
        );
        assert!(select("Android", &devices, None).is_err());
        assert_eq!(select("Android", &devices, Some("9A")).unwrap().0, "9A");
        assert!(select("Android", &devices, Some("R5CT")).is_err());
        assert_eq!(select("Android", &devices[..1], None).unwrap().0, "emulator-5554");
    }
    #[test]
    fn android_arch_selection_never_includes_another_abi() {
        for (arch, expected) in [("arm64", "arm64-v8a"), ("x64", "x86_64")] {
            let selected = android_abis(arch).unwrap();
            assert_eq!(selected.iter().map(|abi| abi.abi).collect::<Vec<_>>(), [expected]);
        }
        assert_eq!(
            android_abis("universal").unwrap().iter().map(|abi| abi.abi).collect::<Vec<_>>(),
            ["arm64-v8a", "x86_64"]
        );
        assert!(ANDROID_ABIS.iter().all(|abi| abi.class == 2));
        assert!(android_abis("arm").is_err());
        assert!(android_abis("aarch64").is_err());
    }
    /// Minimal ELF64 shared library: PT_DYNAMIC at file offset 0x100 and a
    /// PT_LOAD mapping the string table (file offset 0x200) at 0x1200.
    fn library(needed: &[&str]) -> Vec<u8> {
        fn put(image: &mut [u8], at: usize, bytes: &[u8]) {
            image[at..at + bytes.len()].copy_from_slice(bytes);
        }
        let mut image = vec![0_u8; 0x300];
        put(&mut image, 0, b"\x7fELF\x02\x01");
        put(&mut image, 32, &64_u64.to_le_bytes());
        put(&mut image, 54, &56_u16.to_le_bytes());
        put(&mut image, 56, &2_u16.to_le_bytes());
        // PT_LOAD: offset 0x200 mapped at 0x1200.
        put(&mut image, 64, &1_u32.to_le_bytes());
        put(&mut image, 64 + 8, &0x200_u64.to_le_bytes());
        put(&mut image, 64 + 16, &0x1200_u64.to_le_bytes());
        put(&mut image, 64 + 32, &0x100_u64.to_le_bytes());
        // PT_DYNAMIC at 0x100.
        put(&mut image, 120, &2_u32.to_le_bytes());
        put(&mut image, 120 + 8, &0x100_u64.to_le_bytes());
        put(&mut image, 120 + 32, &0x80_u64.to_le_bytes());
        let mut dynamic = 0x100;
        let mut name = 1;
        for library in needed {
            put(&mut image, dynamic, &1_u64.to_le_bytes());
            put(&mut image, dynamic + 8, &(name as u64).to_le_bytes());
            put(&mut image, 0x200 + name, library.as_bytes());
            name += library.len() + 1;
            dynamic += 16;
        }
        put(&mut image, dynamic, &5_u64.to_le_bytes());
        put(&mut image, dynamic + 8, &0x1200_u64.to_le_bytes());
        image
    }
    #[test]
    fn android_floor_rejects_newer_system_libraries() {
        assert_eq!(
            elf_needed(&library(&["libandroid.so", "liblog.so"])).unwrap(),
            ["libandroid.so", "liblog.so"]
        );
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("libpaseo_gui.so");
        fs::write(&path, library(&["libandroid.so", "libEGL.so"])).unwrap();
        ensure_android_floor(&path).unwrap();
        fs::write(&path, library(&["libandroid.so", "libamidi.so"])).unwrap();
        let error = ensure_android_floor(&path).unwrap_err().to_string();
        assert!(error.contains("libamidi.so (API 29)"), "{error}");
        assert!(elf_needed(&library(&["libandroid.so"])[..0x120]).is_err());
    }
}
