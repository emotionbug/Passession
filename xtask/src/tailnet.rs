//! App-owned libtailscale artifacts. Daemon-only commands never enter this pipeline.
use crate::{common::*, notices};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    env,
    ffi::{OsStr, OsString},
    fs,
    path::{Path, PathBuf},
    process::Command,
};
use walkdir::WalkDir;

/// Build tags shared by the library build, the `go list` package set that
/// selects notices, and the build fingerprint. `ts_omit_webclient` drops the
/// node-management web UI on port 5252 (already stubbed on Android); browser
/// sign-in uses the control server's auth URL and is unaffected.
const GO_TAGS: &str = "-tags=ts_omit_webclient";

pub struct NativeBuild {
    pub library: PathBuf,
    pub notices: String,
}

pub fn library_name(platform: &str) -> Result<&'static str> {
    match platform {
        "linux" | "android" => Ok("libpaseo_tailscale.so"),
        "macos" => Ok("libpaseo_tailscale.dylib"),
        "windows" => Ok("paseo_tailscale.dll"),
        "ios" => Ok("libpaseo_tailscale.a"),
        _ => bail!("Unsupported embedded Tailscale platform: {platform}"),
    }
}

struct Target {
    desktop: Option<BuildTarget>,
    triple: String,
    platform: &'static str,
    goos: &'static str,
    goarch: &'static str,
    compiler: PathBuf,
    compiler_args: String,
    cflags: String,
    ldflags: String,
    sdk: String,
}

fn quote(path: &Path) -> Result<String> {
    let path = path.to_str().context("Native compiler path is not UTF-8")?;
    ensure!(
        !path.contains(['\"', '\n', '\r']),
        "Invalid native compiler path"
    );
    Ok(format!("\"{path}\""))
}

fn architecture(arch: &str) -> Result<(&'static str, &'static str)> {
    match arch {
        "x64" | "x86_64" => Ok(("amd64", "x86_64")),
        "arm64" | "aarch64" => Ok(("arm64", "aarch64")),
        _ => bail!("Unsupported embedded Tailscale architecture: {arch}"),
    }
}

fn target_command(program: impl AsRef<OsStr>, target: Option<&BuildTarget>) -> Result<Command> {
    let mut command = crate::common::command(program)?;
    if let Some(target) = target
        && target.is_cross()?
    {
        crate::cross::apply(&mut command, target)?;
    }
    Ok(command)
}

fn command_env(command: &Command, name: &str) -> Option<OsString> {
    command
        .get_envs()
        .find_map(|(key, value)| (key == name).then_some(value))
        .map(|value| value.map(OsStr::to_owned))
        .unwrap_or_else(|| env::var_os(name))
}

pub fn desktop(platform: &str, minimum: &str) -> Result<NativeBuild> {
    let desktop = BuildTarget::current()?;
    ensure!(
        platform == desktop.platform,
        "Tailnet platform must match the build target"
    );
    let platform = desktop.platform;
    ensure!(
        matches!(platform, "linux" | "macos" | "windows"),
        "Desktop native target required"
    );
    let (goarch, cpu) = architecture(desktop.arch)?;
    let cross = desktop.is_cross()?;
    let mut target = Target {
        desktop: Some(desktop),
        triple: desktop.triple.to_owned(),
        platform,
        goos: if platform == "macos" {
            "darwin"
        } else {
            platform
        },
        goarch,
        compiler: PathBuf::new(),
        compiler_args: String::new(),
        cflags: "-O2 -g0".into(),
        ldflags: String::new(),
        sdk: String::new(),
    };
    if cross {
        let configured = target_command("go", Some(&desktop))?;
        // Keep Go's compiler separate from target-qualified Rust C compilers:
        // Windows Go cgo uses Zig GNU, while Rust/Ghostty use the MSVC ABI.
        let compiler = command_env(&configured, "PASEO_CROSS_GO_CC")
            .context("Cross Tailscale requires a provisioned PASEO_CROSS_GO_CC wrapper")?;
        target.compiler = which::which(&compiler)
            .context("Cross Tailscale PASEO_CROSS_GO_CC must name an executable wrapper")?;
        for name in ["CFLAGS", "CGO_CFLAGS"] {
            if let Some(flags) = command_env(&configured, name) {
                target.cflags.push(' ');
                target
                    .cflags
                    .push_str(flags.to_str().context("Cross C flags are not UTF-8")?);
            }
        }
        for name in ["LDFLAGS", "CGO_LDFLAGS"] {
            if let Some(flags) = command_env(&configured, name) {
                target.ldflags.push(' ');
                target
                    .ldflags
                    .push_str(flags.to_str().context("Cross linker flags are not UTF-8")?);
            }
        }
        if platform == "macos" {
            if let Some(sdk) = command_env(&configured, "SDKROOT") {
                target.sdk = sdk
                    .to_str()
                    .context("Cross SDKROOT is not UTF-8")?
                    .to_owned();
                let flags = format!(" -isysroot {}", quote(Path::new(&sdk))?);
                target.cflags.push_str(&flags);
                target.ldflags.push_str(&flags);
            }
            let flags = format!(" -mmacosx-version-min={minimum}");
            target.cflags.push_str(&flags);
            target.ldflags.push_str(&flags);
            target
                .ldflags
                .push_str(" -Wl,-install_name,@rpath/libpaseo_tailscale.dylib");
        } else if platform == "linux" {
            target
                .ldflags
                .push_str(" -Wl,-soname,libpaseo_tailscale.so");
        }
    } else if platform == "macos" {
        let sdk = capture(command("xcrun")?.args(["--sdk", "macosx", "--show-sdk-path"]))?;
        target.compiler = PathBuf::from(capture(
            command("xcrun")?.args(["--sdk", "macosx", "--find", "clang"]),
        )?);
        target.sdk = capture(command("xcrun")?.args(["--sdk", "macosx", "--show-sdk-version"]))?;
        let cpu = if goarch == "arm64" { "arm64" } else { "x86_64" };
        let flags = format!(
            "-isysroot {} -arch {cpu} -mmacosx-version-min={minimum}",
            quote(Path::new(&sdk))?
        );
        target.cflags.push_str(&format!(" {flags}"));
        target.ldflags = format!("{flags} -Wl,-install_name,@rpath/libpaseo_tailscale.dylib");
    } else {
        ensure!(
            matches!(platform, "linux" | "windows"),
            "Desktop native host required"
        );
        target.compiler = which::which(env::var_os("ZIG").unwrap_or_else(|| "zig".into()))
            .context("Embedded Tailscale requires Zig 0.15.2 on PATH or ZIG")?;
        target.sdk = capture(command(&target.compiler)?.arg("version"))?;
        ensure!(
            target.sdk == "0.15.2",
            "Embedded Tailscale C compiler requires exactly Zig 0.15.2"
        );
        target.compiler_args = format!(" cc -target {cpu}-{platform}-gnu");
        if platform == "linux" {
            target.ldflags = "-Wl,-soname,libpaseo_tailscale.so".into();
        }
    }
    build(target)
}

pub fn android(triple: &str, compiler: &Path, ndk: &Path) -> Result<NativeBuild> {
    let (goarch, _) = architecture(triple.split('-').next().context("Missing Android CPU")?)?;
    // Go executes CC directly and cannot invoke NDK's Windows .cmd wrappers.
    let (compiler, compiler_args) = if cfg!(windows) {
        let stem = compiler
            .file_stem()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix("-clang"))
            .context("Invalid NDK clang wrapper")?;
        (
            compiler.with_file_name("clang.exe"),
            format!(" --target={stem}"),
        )
    } else {
        (compiler.to_owned(), String::new())
    };
    build(Target {
        desktop: None,
        triple: triple.into(),
        platform: "android",
        goos: "android",
        goarch,
        compiler,
        compiler_args,
        cflags: "-O2 -g0".into(),
        // Android's 16 KiB page-size devices require aligned load segments.
        ldflags: "-Wl,-z,max-page-size=16384 -Wl,-soname,libpaseo_tailscale.so".into(),
        sdk: fs::read_to_string(ndk.join("source.properties"))?,
    })
}

pub fn ios(triple: &str, simulator: bool, minimum: &str) -> Result<NativeBuild> {
    let sdk_name = if simulator {
        "iphonesimulator"
    } else {
        "iphoneos"
    };
    let sdk = capture(command("xcrun")?.args(["--sdk", sdk_name, "--show-sdk-path"]))?;
    let compiler = capture(command("xcrun")?.args(["--sdk", sdk_name, "--find", "clang"]))?;
    let sdk_version = capture(command("xcrun")?.args(["--sdk", sdk_name, "--show-sdk-version"]))?;
    let target = if simulator {
        format!("arm64-apple-ios{minimum}-simulator")
    } else {
        format!("arm64-apple-ios{minimum}")
    };
    let flags = format!("-isysroot {} -target {target}", quote(Path::new(&sdk))?);
    build(Target {
        desktop: None,
        triple: triple.into(),
        platform: "ios",
        goos: "ios",
        goarch: "arm64",
        compiler: compiler.into(),
        compiler_args: String::new(),
        cflags: format!("-O2 -g0 {flags}"),
        ldflags: flags,
        sdk: sdk_version,
    })
}

pub(crate) fn pinned_go(source: &Path) -> Result<String> {
    let module = fs::read_to_string(source.join("go.mod"))?;
    let pin = module
        .lines()
        .find_map(|line| line.strip_prefix("toolchain go"))
        .or_else(|| module.lines().find_map(|line| line.strip_prefix("go ")))
        .context("Native go.mod must pin a Go toolchain")?;
    ensure!(
        pin.split('.').count() == 3 && pin.split('.').all(|part| part.parse::<u32>().is_ok()),
        "Native go.mod must pin an exact Go patch version"
    );
    Ok(format!("go{pin}"))
}

/// Go module and build caches as `(GOMODCACHE, GOCACHE)`. Local builds keep them
/// beside the Go outputs in Cargo's target directory, partitioned by target
/// triple. CI sets absolute `GOMODCACHE`/`GOCACHE` elsewhere: its Rust cache
/// prunes and restores the target directory, which collides with Go's read-only
/// module tree.
pub(crate) fn go_caches(triple: &str) -> Result<(PathBuf, PathBuf)> {
    let cache = target_root()?.join("paseo-tailnet");
    let pick = |name: &str, default: PathBuf| {
        env::var_os(name)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .unwrap_or(default)
    };
    Ok((
        pick("GOMODCACHE", cache.join("modules")),
        pick("GOCACHE", cache.join("go-cache").join(triple)),
    ))
}

fn build(target: Target) -> Result<NativeBuild> {
    let source = root()?.join("packages/tailnet/native");
    let go = which::which(env::var_os("GO").unwrap_or_else(|| "go".into())).context(
        "Install the exact Go toolchain pinned in packages/tailnet/native/go.mod, or set GO",
    )?;
    let pin = pinned_go(&source)?;
    let directory = target_root()?.join("paseo-tailnet").join(&target.triple);
    let (modules, go_cache) = go_caches(&target.triple)?;
    fs::create_dir_all(&directory)?;
    let mut base = target_command(&go, target.desktop.as_ref())?;
    base.current_dir(&source)
        .env("GOTOOLCHAIN", "local")
        .env("GOENV", "off")
        .env("GOWORK", "off")
        .env("GOFLAGS", "")
        .env("GOMAXPROCS", "2")
        .env("GOCACHE", go_cache)
        .env("GOMODCACHE", modules)
        .env("CGO_ENABLED", "1")
        .env("GOOS", target.goos)
        .env("GOARCH", target.goarch)
        .env(
            "CC",
            format!("{}{}", quote(&target.compiler)?, target.compiler_args),
        )
        .env("CGO_CFLAGS", &target.cflags)
        .env("CGO_LDFLAGS", &target.ldflags)
        .env_remove("GOEXPERIMENT")
        .env_remove("GOAMD64")
        .env_remove("GOARM64");
    let cross = target
        .desktop
        .map(BuildTarget::is_cross)
        .transpose()?
        .unwrap_or(false);
    if !cross {
        base.env("CGO_CPPFLAGS", "")
            .env("CGO_CXXFLAGS", &target.cflags);
    } else if command_env(&base, "CGO_CXXFLAGS").is_none() {
        base.env("CGO_CXXFLAGS", &target.cflags);
    }
    let command = || -> Result<Command> {
        let mut result = target_command(base.get_program(), target.desktop.as_ref())?;
        result.current_dir(&source);
        for (name, value) in base.get_envs() {
            if let Some(value) = value {
                result.env(name, value);
            } else {
                result.env_remove(name);
            }
        }
        Ok(result)
    };
    let toolchain = capture(command()?.args(["env", "GOVERSION"]))?;
    ensure!(
        toolchain == pin,
        "Embedded Tailscale requires {pin}; {} reports {toolchain}",
        go.display()
    );
    let mut compiler_command = target_command(&target.compiler, target.desktop.as_ref())?;
    if target.compiler_args.starts_with(" cc ") {
        compiler_command.arg("cc");
    }
    let compiler_version = capture(compiler_command.arg("--version"))?;
    let mut sources = BTreeMap::new();
    for entry in WalkDir::new(&source).follow_links(false) {
        let entry = entry?;
        if entry.file_type().is_dir() {
            continue;
        }
        ensure!(
            entry.file_type().is_file(),
            "Native source must not contain symlinks: {}",
            entry.path().display()
        );
        sources.insert(
            entry
                .path()
                .strip_prefix(&source)?
                .to_string_lossy()
                .replace('\\', "/"),
            sha256_file(entry.path())?,
        );
    }
    let stamps = crate::components::compiler_stamps()?;
    let identity = json!({
        "schemaVersion": 1, "sources": sources, "target": target.triple,
        "go": toolchain, "goSha256": sha256_file(&go)?, "compiler": target.compiler,
        "compilerSha256": sha256_file(&target.compiler)?, "compilerVersion": compiler_version,
        "compilerArgs": target.compiler_args, "cflags": target.cflags,
        "ldflags": target.ldflags, "sdk": target.sdk, "goTags": GO_TAGS,
        "environment": base.get_envs().map(|(name, value)| (
            name.to_string_lossy().into_owned(),
            value.map(|value| value.to_string_lossy().into_owned())
        )).collect::<BTreeMap<_, _>>(),
        "stamps": stamps,
        "pipeline": hex::encode(Sha256::digest(concat!(include_str!("tailnet.rs"), include_str!("notices.rs")).as_bytes())),
    });
    let fingerprint = hex::encode(Sha256::digest(serde_json::to_vec(&identity)?));
    let library = directory.join(library_name(target.platform)?);
    let notice_path = directory.join("THIRD-PARTY-NOTICES.txt");
    let manifest_path = directory.join("build.json");
    if library.is_file() && notice_path.is_file() && manifest_path.is_file() {
        let previous = read_json(&manifest_path)?;
        if previous["fingerprint"] == fingerprint
            && previous["librarySha256"] == sha256_file(&library)?
            && previous["noticesSha256"] == sha256_file(&notice_path)?
        {
            return Ok(NativeBuild {
                library,
                notices: fs::read_to_string(notice_path)?,
            });
        }
    }
    // Go verifies downloaded module bytes against the committed go.sum; readonly
    // resolution cannot silently update dependency versions or checksums.
    // A shared library ships as built, so it drops the symbol table and DWARF.
    // The iOS archive keeps them: Xcode links and strips the app, and its
    // debug information belongs in the dSYM.
    let (buildmode, strip) = if target.platform == "ios" {
        ("-buildmode=c-archive", "")
    } else {
        ("-buildmode=c-shared", "-s -w ")
    };
    let packages = capture(command()?.args([
        "list",
        "-mod=readonly",
        GO_TAGS,
        "-deps",
        "-json",
        buildmode,
        ".",
    ]))?;
    let mut modules = BTreeMap::new();
    for package in serde_json::Deserializer::from_str(&packages).into_iter::<Value>() {
        let package = package?;
        if let Some(module) = package.get("Module") {
            ensure!(
                module.get("Replace").is_none(),
                "Native Go dependency replacements are not allowed"
            );
            if module["Main"] == true {
                continue;
            }
            let name = module["Path"].as_str().context("Go module has no path")?;
            let version = module["Version"]
                .as_str()
                .context("Go module is not version-pinned")?;
            let directory = module["Dir"]
                .as_str()
                .context("Go module has no source directory")?;
            modules.insert(format!("{name}@{version}"), PathBuf::from(directory));
        }
    }
    run(command()?.args(["mod", "verify"]))?;
    let goroot = capture(command()?.args(["env", "GOROOT"]))?;
    let rendered = notices::render_go(&modules, &source, Path::new(&goroot), &pin)?;
    let temporary = tempfile::tempdir_in(&directory)?;
    let output = temporary.path().join(library_name(target.platform)?);
    run(command()?
        .args([
            "build",
            "-mod=readonly",
            "-trimpath",
            "-buildvcs=false",
            "-p=2",
            GO_TAGS,
            &format!(
                "-ldflags={strip}-buildid= {}",
                crate::components::go_stamps(&stamps)
            ),
        ])
        .arg(buildmode)
        .arg("-o")
        .arg(&output)
        .arg("."))?;
    ensure!(
        output.is_file() && fs::metadata(&output)?.len() > 0,
        "Go produced no embedded Tailscale library"
    );
    if matches!(target.platform, "linux" | "macos" | "windows") {
        crate::build::assert_architecture(&output)?;
    }
    copy_tree(&output, &library)?;
    text(&notice_path, &rendered, 0o644)?;
    write_json(
        &manifest_path,
        &json!({"fingerprint": fingerprint, "inputs": identity,
        "librarySha256": sha256_file(&library)?, "noticesSha256": sha256_file(&notice_path)?}),
    )?;
    Ok(NativeBuild {
        library,
        notices: rendered,
    })
}

pub fn require_packaged(executable: &Path, platform: &str) -> Result<()> {
    let library = executable
        .parent()
        .context("Application executable has no directory")?
        .join(library_name(platform)?);
    ensure!(
        library.is_file() && fs::metadata(&library)?.len() > 0,
        "Required embedded Tailscale library missing: {}; rebuild with cargo xtask build-gui",
        library.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_library_is_mandatory() {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("paseo-sessions");
        assert!(require_packaged(&executable, "linux").is_err());
        fs::write(directory.path().join("libpaseo_tailscale.so"), []).unwrap();
        assert!(require_packaged(&executable, "linux").is_err());
        fs::write(directory.path().join("libpaseo_tailscale.so"), b"library").unwrap();
        require_packaged(&executable, "linux").unwrap();
    }

    #[test]
    fn go_toolchain_must_be_patch_pinned() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("go.mod"),
            "module example.com/native\ngo 1.27\n",
        )
        .unwrap();
        assert!(pinned_go(directory.path()).is_err());
        fs::write(
            directory.path().join("go.mod"),
            "module example.com/native\ngo 1.27.1\ntoolchain go1.27.4\n",
        )
        .unwrap();
        assert_eq!(pinned_go(directory.path()).unwrap(), "go1.27.4");
    }
}
