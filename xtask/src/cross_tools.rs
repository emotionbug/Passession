//! Pinned Linux desktop SDKs and explicit, target-specific compiler wrappers.
mod macos;

use crate::{assets, common::*};
use anyhow::{Context, Result, bail, ensure};
use flate2::read::GzDecoder;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    env, fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
};
use walkdir::WalkDir;

const XWIN_ARCHIVE: &str = "xwin-0.10.0-x86_64-unknown-linux-musl.tar.gz";
const XWIN_SHA256: &str = "d870eb4b2f390878af6da1ccd3cf321d22fcb72720984853b4be732ae597fc88";
const SDK_ARCHIVE: &str = "CLTools_macOSNMOS_SDK-16.4.pkg";
const SDK_SHA256: &str = "ba3453d62b3d2babf67f3a4a44e8073d6555c85f114856f4390a1f53bd76e24a";
const SDK_URL: &str = "https://swcdn.apple.com/content/downloads/52/01/082-41241-A_0747ZN8FHV/dectd075r63pppkkzsb75qk61s0lfee22j/CLTools_macOSNMOS_SDK.pkg";
const VS_SHA256: &str = "da49c304df0fff4e75c20849171840a06840e845ce814104d709dfb5cd461668";
// Fresh official HTTPS bytes match this digest, not the channel's advertised
// VS_SHA256 above; retain both facts instead of silently accepting either.
const VS_DOWNLOAD_SHA256: &str = "d1d8109899558618035724949673f69f8c1b5704f542a2e76f31d87e425d5710";
const VS_URL: &str = "https://download.visualstudio.microsoft.com/download/pr/95995f57-8355-4ff5-8d90-a7e1669fb3b3/da49c304df0fff4e75c20849171840a06840e845ce814104d709dfb5cd461668/VisualStudio.vsman";

fn usage() -> anyhow::Error {
    anyhow::anyhow!(
        "Usage: cargo xtask cross-tools <windows-x64|macos-arm64> [--root DIR] [--github-env FILE]"
    )
}

pub fn run(args: &[String]) -> Result<()> {
    ensure!(
        env::consts::OS == "linux" && env::consts::ARCH == "x86_64",
        "cross-tools requires a Linux x86-64 compiler host"
    );
    let (id, flags) = args.split_first().context(usage())?;
    ensure!(
        matches!(id.as_str(), "windows-x64" | "macos-arm64"),
        "{}",
        usage()
    );
    ensure!(flags.len().is_multiple_of(2), "{}", usage());
    let mut options = BTreeMap::new();
    for pair in flags.chunks_exact(2) {
        ensure!(
            matches!(pair[0].as_str(), "--root" | "--github-env")
                && options.insert(pair[0].as_str(), pair[1].as_str()).is_none(),
            "{}",
            usage()
        );
    }
    let directory = options
        .get("--root")
        .map(|path| PathBuf::from(*path))
        .or_else(|| env::var_os("PASEO_CROSS_ROOT").map(PathBuf::from))
        .unwrap_or(
            dirs::home_dir()
                .context("Home directory unavailable")?
                .join(".cache/paseo-cross"),
        );
    fs::create_dir_all(&directory)?;
    let directory = directory.canonicalize()?;
    // A second provisioner must not replace a toolchain while it is being assembled.
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(directory.join(".provision.lock"))?;
    lock.lock()?;
    let llvm = Llvm::discover(id == "windows-x64")?;
    let (lld, rust_version) = rust_linker()?;
    let zig = which::which(env::var_os("ZIG").unwrap_or_else(|| "zig".into()))
        .context("cross-tools requires Zig 0.15.2 on PATH or ZIG")?
        .canonicalize()?;
    ensure!(
        capture(Command::new(&zig).arg("version"))? == "0.15.2",
        "cross-tools requires exactly Zig 0.15.2"
    );
    let mut environment = BTreeMap::<String, String>::new();
    let path = env::join_paths(
        std::iter::once(llvm.bin.clone())
            .chain(env::split_paths(&env::var_os("PATH").unwrap_or_default())),
    )?;
    put(&mut environment, "PATH", Path::new(&path))?;
    let versions = if id == "windows-x64" {
        windows(&directory, &llvm, &lld, &zig, &mut environment)?;
        json!({"xwin":"0.10.0", "visualStudio":"17.14.45+37906.3", "windowsSdk":"10.0.26100.18", "msvcCrt":"14.44.35226", "msvcHeaders":"14.44.35220", "ucrt":"10.0.26624.1"})
    } else {
        apple(&directory, &llvm, &lld, &zig, &mut environment)?;
        json!({"commandLineTools":"16.4.0.0.1.1747106510", "macosSdk":"15.5", "deploymentTarget":"13.0"})
    };
    let manifest = json!({"schemaVersion":1,"target":id,"env":environment,"tools":{"llvm":llvm.version,"rust":rust_version,"zig":"0.15.2","target":versions},"downloads": if id == "windows-x64" {json!({"xwin":XWIN_SHA256,"visualStudioManifest":VS_DOWNLOAD_SHA256,"visualStudioChannelAdvertisedDigest":VS_SHA256,"payloads":serde_json::from_str::<Value>(include_str!("cross_tools/windows-downloads.json"))?})} else {json!({"appleSdk":SDK_SHA256})}});
    write_json(&directory.join("cross.json"), &manifest)?;
    if let Some(path) = options.get("--github-env") {
        let mut file = fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)?;
        // Keep generated CC/linker flags out of the ambient release-plan input
        // fingerprint. cross-run applies them only to the requested child.
        let value = directory.to_str().context("Root is not UTF-8")?;
        ensure!(
            !value.contains(['\r', '\n']),
            "GitHub environment value contains a newline"
        );
        writeln!(file, "PASEO_CROSS_ROOT={value}")?;
    }
    println!(
        "Provisioned {id}: {}",
        directory.join("cross.json").display()
    );
    Ok(())
}

pub fn run_command(args: &[String]) -> Result<()> {
    let (id, args) = args
        .split_first()
        .context("Usage: cargo xtask cross-run TARGET -- COMMAND [ARGS...]")?;
    ensure!(
        args.first().map(String::as_str) == Some("--") && args.len() >= 2,
        "Usage: cargo xtask cross-run TARGET -- COMMAND [ARGS...]"
    );
    let target = BuildTarget::parse(id)?;
    ensure!(target.is_cross()?, "cross-run requires a cross target");
    let mut child = command(&args[1])?;
    child.args(&args[2..]);
    crate::cross::apply(&mut child, &target)?;
    crate::common::run(&mut child)
}

struct Llvm {
    bin: PathBuf,
    version: String,
}
impl Llvm {
    fn discover(windows: bool) -> Result<Self> {
        let mut names = vec!["clang".to_owned()];
        names.extend((18..=30).rev().map(|n| format!("clang-{n}")));
        for name in names {
            let Ok(clang) = which::which(&name).and_then(|p| {
                p.canonicalize()
                    .map_err(|_| which::Error::CannotFindBinaryPath)
            }) else {
                continue;
            };
            let Ok(version) = capture(Command::new(&clang).arg("--version")) else {
                continue;
            };
            let Some(major) = version
                .split("clang version ")
                .nth(1)
                .and_then(|s| s.split('.').next())
                .and_then(|s| s.parse::<u32>().ok())
            else {
                continue;
            };
            let bin = clang.parent().context("clang has no parent")?.to_path_buf();
            let required: &[&str] = if windows {
                &["clang-cl", "llvm-lib", "llvm-rc"]
            } else {
                &["llvm-ar"]
            };
            if major >= 18 && required.iter().all(|p| bin.join(p).is_file()) {
                return Ok(Self {
                    bin,
                    version: version.lines().next().unwrap_or_default().into(),
                });
            }
        }
        bail!(
            "No complete LLVM >=18 toolchain found; Ubuntu 24.04: install clang-18 llvm-18 (clang-cl, llvm-lib, llvm-rc, llvm-ar required)"
        )
    }
    fn tool(&self, name: &str) -> PathBuf {
        self.bin.join(name)
    }
}

fn rust_linker() -> Result<(PathBuf, String)> {
    let rustc = env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let sysroot = capture(Command::new(&rustc).args(["--print", "sysroot"]))?;
    let version = capture(Command::new(&rustc).arg("-vV"))?;
    let host = version
        .lines()
        .find_map(|s| s.strip_prefix("host: "))
        .context("rustc did not report its host")?;
    let lld = Path::new(&sysroot)
        .join("lib/rustlib")
        .join(host)
        .join("bin/rust-lld");
    ensure!(
        lld.is_file(),
        "Rust bundled linker not found: {}",
        lld.display()
    );
    Ok((lld.canonicalize()?, version))
}

fn shell(path: &Path) -> Result<String> {
    let path = path.to_str().context("Tool path is not UTF-8")?;
    ensure!(!path.contains(['\n', '\r', '\0']), "Invalid tool path");
    Ok(format!("'{}'", path.replace('\'', "'\\''")))
}
fn put(env: &mut BTreeMap<String, String>, name: &str, path: &Path) -> Result<()> {
    env.insert(
        name.into(),
        path.to_str()
            .context("Environment path is not UTF-8")?
            .into(),
    );
    Ok(())
}
fn wrapper(root: &Path, name: &str, body: &str) -> Result<PathBuf> {
    let path = root.join("bin").join(name);
    text(&path, format!("#!/bin/sh\nset -eu\n{body}\n"), 0o755)?;
    Ok(path)
}

/// Legacy feasibility caches are accelerators only: authenticate before copying.
fn download(
    root: &Path,
    name: &str,
    url: &str,
    digest: &str,
    legacy: Option<&Path>,
) -> Result<PathBuf> {
    let output = root.join(name);
    if (!output.is_file() || sha256_file(&output)? != digest)
        && let Some(source) = legacy.filter(|p| p.is_file())
        && sha256_file(source)? == digest
    {
        fs::create_dir_all(output.parent().context("Download has no parent")?)?;
        fs::copy(source, &output)?;
    }
    assets::download(url, &output, digest, false)?;
    Ok(output)
}
fn legacy() -> Result<PathBuf> {
    Ok(dirs::home_dir()
        .context("Home directory unavailable")?
        .join(".cache/passession-tools/cross"))
}

fn tree_digest(path: &Path) -> Result<String> {
    let mut hash = Sha256::new();
    for entry in WalkDir::new(path).follow_links(false).sort_by_file_name() {
        let entry = entry?;
        let relative = entry
            .path()
            .strip_prefix(path)?
            .to_str()
            .context("SDK path is not UTF-8")?;
        hash.update((relative.len() as u64).to_le_bytes());
        hash.update(relative);
        if entry.file_type().is_symlink() {
            hash.update(b"link");
            hash.update(fs::read_link(entry.path())?.as_os_str().as_encoded_bytes());
        } else if entry.file_type().is_file() {
            hash.update(b"file");
            hash.update(sha256_file(entry.path())?);
        } else {
            ensure!(entry.file_type().is_dir(), "Unsupported SDK file");
            hash.update(b"dir");
        }
    }
    Ok(hex::encode(hash.finalize()))
}
fn prepared(directory: &Path, input: &str) -> Result<bool> {
    let state = directory.with_extension("verified.json");
    if !directory.is_dir() || !state.is_file() {
        return Ok(false);
    }
    let state = read_json(&state)?;
    Ok(state["input"].as_str() == Some(input)
        && state["tree"].as_str() == Some(tree_digest(directory)?.as_str()))
}
fn remember(directory: &Path, input: &str) -> Result<()> {
    write_json(
        &directory.with_extension("verified.json"),
        &json!({"input":input,"tree":tree_digest(directory)?}),
    )
}

#[derive(Deserialize)]
struct Payload {
    path: String,
    sha256: String,
    url: String,
}
fn windows(
    root: &Path,
    llvm: &Llvm,
    lld: &Path,
    zig: &Path,
    environment: &mut BTreeMap<String, String>,
) -> Result<()> {
    let old = legacy()?;
    let archive = download(
        root,
        XWIN_ARCHIVE,
        &format!("https://github.com/Jake-Shadle/xwin/releases/download/0.10.0/{XWIN_ARCHIVE}"),
        XWIN_SHA256,
        Some(&old.join(XWIN_ARCHIVE)),
    )?;
    let tools = root.join("xwin-tool");
    if !prepared(&tools, XWIN_SHA256)? {
        remove_if_exists(&tools)?;
        fs::create_dir_all(&tools)?;
        tar::Archive::new(GzDecoder::new(fs::File::open(&archive)?)).unpack(&tools)?;
        remember(&tools, XWIN_SHA256)?;
    }
    let xwin = tools.join("xwin-0.10.0-x86_64-unknown-linux-musl/xwin");
    let channel = root.join("windows-channel.json");
    text(
        &channel,
        include_str!("cross_tools/windows-channel.json"),
        0o644,
    )?;
    let manifest_name = format!("xwin-cache/dl/pkg_manifest_{VS_SHA256}.vsman");
    download(
        root,
        &manifest_name,
        VS_URL,
        VS_DOWNLOAD_SHA256,
        Some(&old.join(&manifest_name)),
    )?;
    for item in
        serde_json::from_str::<Vec<Payload>>(include_str!("cross_tools/windows-downloads.json"))?
    {
        download(
            root,
            &item.path,
            &item.url,
            &item.sha256,
            Some(&old.join(&item.path)),
        )?;
    }
    let sdk = root.join("xwin");
    let input = format!("{XWIN_SHA256}:{VS_SHA256}:10.0.26100.18:14.44.35226");
    if !prepared(&sdk, &input)? {
        remove_if_exists(&sdk)?;
        // Version selection is fixed by the immutable, digest-pinned manifest;
        // xwin's CLI version selectors use different IDs than package versions.
        crate::common::run(
            Command::new(&xwin)
                .arg("--accept-license")
                .arg("--cache-dir")
                .arg(root.join("xwin-cache"))
                .arg("--manifest")
                .arg(&channel)
                .args(["--arch", "x86_64", "splat", "--output"])
                .arg(&sdk),
        )?;
        remember(&sdk, &input)?;
        remove_if_exists(&root.join("xwin-cache/unpack"))?;
    }
    let crt = sdk.join("crt");
    let win = sdk.join("sdk");
    let cc = wrapper(
        root,
        "msvc-cl",
        &format!(
            "exec {} --target=x86_64-pc-windows-msvc -imsvc {} -imsvc {} -imsvc {} -imsvc {} \"$@\"",
            shell(&llvm.tool("clang-cl"))?,
            shell(&crt.join("include"))?,
            shell(&win.join("include/ucrt"))?,
            shell(&win.join("include/um"))?,
            shell(&win.join("include/shared"))?
        ),
    )?;
    let linker = wrapper(
        root,
        "msvc-link",
        &format!(
            "exec {} -flavor link /libpath:{} /libpath:{} /libpath:{} \"$@\"",
            shell(lld)?,
            shell(&crt.join("lib/x86_64"))?,
            shell(&win.join("lib/ucrt/x86_64"))?,
            shell(&win.join("lib/um/x86_64"))?
        ),
    )?;
    let rc = wrapper(
        root,
        "msvc-rc",
        &format!(
            "exec {} /I {} /I {} \"$@\"",
            shell(&llvm.tool("llvm-rc"))?,
            shell(&packaging()?.join("desktop"))?,
            shell(&packaging()?.join("windows"))?
        ),
    )?;
    let libc = root.join("windows-libc.txt");
    text(
        &libc,
        format!(
            "include_dir={}\nsys_include_dir={}\ncrt_dir={}\nmsvc_lib_dir={}\nkernel32_lib_dir={}\ngcc_dir=\n",
            win.join("include/ucrt").display(),
            crt.join("include").display(),
            win.join("lib/ucrt/x86_64").display(),
            crt.join("lib/x86_64").display(),
            win.join("lib/um/x86_64").display()
        ),
        0o644,
    )?;
    let ghostty = wrapper(
        root,
        "zig-msvc",
        &format!(
            "if [ \"${{1:-}}\" = build ]; then\n shift\n exec {} build --libc {} \"$@\"\nfi\nexec {} \"$@\"",
            shell(zig)?,
            shell(&libc)?,
            shell(zig)?
        ),
    )?;
    let go_cc = wrapper(
        root,
        "windows-go-cc",
        &format!("exec {} cc -target x86_64-windows-gnu \"$@\"", shell(zig)?),
    )?;
    for (key, path) in [
        ("CC_x86_64_pc_windows_msvc", cc.clone()),
        ("CXX_x86_64_pc_windows_msvc", cc),
        ("AR_x86_64_pc_windows_msvc", llvm.tool("llvm-lib")),
        ("RC_x86_64_pc_windows_msvc", rc),
        ("CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER", linker),
        ("ZIG", ghostty),
        ("PASEO_CROSS_GO_CC", go_cc),
    ] {
        put(environment, key, &path)?;
    }
    put(
        environment,
        "GHOSTTY_CACHE_DIR",
        &root.join("ghostty-windows"),
    )?;
    Ok(())
}

fn apple(
    root: &Path,
    llvm: &Llvm,
    lld: &Path,
    zig: &Path,
    environment: &mut BTreeMap<String, String>,
) -> Result<()> {
    eprintln!(
        "Apple SDK use requires authorization under Apple's MACOS SDK AND XCODE AGREEMENT; provisioning is not a license grant (see docs/release.md)."
    );
    let archive = download(
        root,
        SDK_ARCHIVE,
        SDK_URL,
        SDK_SHA256,
        Some(&legacy()?.join(SDK_ARCHIVE)),
    )?;
    let destination = root.join("sdk-16.4");
    if !prepared(&destination, SDK_SHA256)? {
        remove_if_exists(&destination)?;
        macos::extract_sdk(&archive, &destination)?;
        remember(&destination, SDK_SHA256)?;
    }
    let sdk = destination.join("Library/Developer/CommandLineTools/SDKs/MacOSX15.5.sdk");
    ensure!(
        sdk.join("SDKSettings.json").is_file(),
        "Expected MacOSX15.5.sdk missing"
    );
    let ld = wrapper(
        root,
        "ld64.lld",
        &format!("exec {} -flavor darwin \"$@\"", shell(lld)?),
    )?;
    let base = format!(
        "{} --target=arm64-apple-macos13.0 -isysroot {} -mmacosx-version-min=13.0",
        shell(&llvm.tool("clang"))?,
        shell(&sdk)?
    );
    let cc = wrapper(
        root,
        "arm64-apple-macos-clang",
        &format!(
            "for arg do\n case \"$arg\" in\n  -c|-S|-E|-fsyntax-only) exec {base} \"$@\" ;;\n esac\ndone\nexec {base} -fuse-ld=lld --ld-path={} \"$@\"",
            shell(&ld)?
        ),
    )?;
    for (key, path) in [
        ("PASEO_CROSS_GO_CC", cc.clone()),
        ("CC_aarch64_apple_darwin", cc.clone()),
        ("CXX_aarch64_apple_darwin", cc.clone()),
        ("CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER", cc),
        ("AR_aarch64_apple_darwin", llvm.tool("llvm-ar")),
        ("SDKROOT", sdk),
        ("ZIG", zig.to_path_buf()),
    ] {
        put(environment, key, &path)?;
    }
    put(environment, "AR", &llvm.tool("llvm-ar"))?;
    environment.insert("MACOSX_DEPLOYMENT_TARGET".into(), "13.0".into());
    put(
        environment,
        "GHOSTTY_CACHE_DIR",
        &root.join("ghostty-macos"),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shell_paths_cannot_escape_quotes() {
        assert_eq!(shell(Path::new("/tmp/a'b c")).unwrap(), "'/tmp/a'\\''b c'");
        assert!(shell(Path::new("/tmp/a\nb")).is_err());
    }
    #[test]
    fn payload_pins_are_complete_and_unique() {
        let payloads: Vec<Payload> =
            serde_json::from_str(include_str!("cross_tools/windows-downloads.json")).unwrap();
        assert_eq!(payloads.len(), 40);
        let mut paths = std::collections::BTreeSet::new();
        for item in payloads {
            assert!(paths.insert(item.path));
            assert_eq!(item.sha256.len(), 64);
            assert!(
                item.url
                    .starts_with("https://download.visualstudio.microsoft.com/")
            );
        }
    }
    #[test]
    fn prepared_sdk_rejects_changed_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let sdk = temp.path().join("sdk");
        fs::create_dir(&sdk).unwrap();
        fs::write(sdk.join("header"), b"original").unwrap();
        remember(&sdk, "pin").unwrap();
        assert!(prepared(&sdk, "pin").unwrap());
        fs::write(sdk.join("header"), b"changed").unwrap();
        assert!(!prepared(&sdk, "pin").unwrap());
    }
}
