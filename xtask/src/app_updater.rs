//! Desktop-only, standard-library Go helper. Never staged with standalone daemons.
use crate::{build, common::*, tailnet};
use anyhow::{Context, Result, bail, ensure};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

pub fn executable_name(platform: &str) -> &'static str {
    if platform == "windows" {
        "passession-update-helper.exe"
    } else {
        "passession-update-helper"
    }
}

pub fn desktop(platform: &str) -> Result<PathBuf> {
    let target = BuildTarget::current()?;
    ensure!(
        platform == target.platform,
        "Updater platform must match the build target"
    );
    let goos = match platform {
        "linux" => "linux",
        "macos" => "darwin",
        "windows" => "windows",
        _ => bail!("Application updater is desktop-only"),
    };
    let goarch = match target.arch {
        "x64" => "amd64",
        "arm64" => "arm64",
        _ => bail!("Unsupported desktop updater architecture"),
    };
    let source = root()?.join("packages/app-updater");
    let pin = tailnet::pinned_go(&source)?;
    ensure!(
        pin == tailnet::pinned_go(&root()?.join("packages/tailnet/native"))?,
        "Desktop Go components must use the same pinned toolchain"
    );
    let go = which::which(env::var_os("GO").unwrap_or_else(|| "go".into()))
        .context("Install Go 1.27.4 or set GO to the pinned toolchain")?;
    let (modules, go_cache) = tailnet::go_caches(target.triple)?;
    let directory = target_root()?.join("paseo-app-updater").join(target.triple);
    fs::create_dir_all(&directory)?;
    let command = || -> Result<Command> {
        let mut command = crate::common::command(&go)?;
        if target.is_cross()? {
            crate::cross::apply(&mut command, &target)?;
        }
        command
            .current_dir(&source)
            .env("GOTOOLCHAIN", "local")
            .env("GOENV", "off")
            .env("GOWORK", "off")
            .env("GOFLAGS", "")
            .env("GOMAXPROCS", "2")
            .env("CGO_ENABLED", "0")
            .env("GOCACHE", &go_cache)
            .env("GOMODCACHE", &modules)
            .env("GOOS", goos)
            .env("GOARCH", goarch)
            .env_remove("GOEXPERIMENT")
            .env_remove("GOAMD64")
            .env_remove("GOARM64");
        Ok(command)
    };
    ensure!(
        capture(command()?.args(["env", "GOVERSION"]))? == pin,
        "Desktop updater requires exactly {pin}"
    );
    let mut sources = BTreeMap::new();
    for entry in walkdir::WalkDir::new(&source).follow_links(false) {
        let entry = entry?;
        if entry.file_type().is_dir() {
            continue;
        }
        ensure!(
            entry.file_type().is_file(),
            "Helper source contains a non-regular file"
        );
        sources.insert(
            entry
                .path()
                .strip_prefix(&source)?
                .to_string_lossy()
                .into_owned(),
            sha256_file(entry.path())?,
        );
    }
    let stamps = crate::components::compiler_stamps()?;
    let inputs = json!({"sources":sources,"go":pin,"goSha256":sha256_file(&go)?,
        "platform":target.platform,"arch":target.arch,"target":target.triple,"goarch":goarch,"stamps":stamps,
        "licenseSha256":sha256_file(&root()?.join("LICENSE"))?,
        "pipeline":hex::encode(Sha256::digest(concat!(include_str!("app_updater.rs"), include_str!("notices.rs")).as_bytes()))});
    let fingerprint = hex::encode(Sha256::digest(serde_json::to_vec(&inputs)?));
    let output = directory.join(executable_name(platform));
    let manifest = directory.join("build.json");
    let notice = directory.join("THIRD-PARTY-NOTICES.txt");
    if output.is_file() && manifest.is_file() && notice.is_file() {
        let previous = read_json(&manifest)?;
        if previous["fingerprint"] == fingerprint
            && previous["executableSha256"] == sha256_file(&output)?
            && previous["noticesSha256"] == sha256_file(&notice)?
        {
            return Ok(output);
        }
    }
    let temporary = tempfile::tempdir_in(&directory)?;
    let binary = temporary.path().join(executable_name(platform));
    let flags = format!(
        "-s -w -buildid= {} {}",
        if platform == "windows" {
            "-H=windowsgui"
        } else {
            ""
        },
        crate::components::go_stamps(&stamps)
    );
    run(command()?
        .args([
            "build",
            "-mod=readonly",
            "-trimpath",
            "-buildvcs=false",
            "-ldflags",
            &flags,
            "-o",
        ])
        .arg(&binary)
        .arg("."))?;
    build::assert_architecture(&binary)?;
    let goroot = capture(command()?.args(["env", "GOROOT"]))?;
    let rendered = crate::notices::render_go_helper(Path::new(&goroot), &pin)?;
    text(&notice, rendered, 0o644)?;
    copy_tree(&binary, &output)?;
    chmod(&output, 0o755)?;
    write_json(
        &manifest,
        &json!({"fingerprint":fingerprint,"inputs":inputs,
        "executableSha256":sha256_file(&output)?,"noticesSha256":sha256_file(&notice)?}),
    )?;
    Ok(output)
}

pub fn require_packaged(gui: &Path, platform: &str) -> Result<()> {
    let helper = gui
        .parent()
        .context("GUI has no parent directory")?
        .join(executable_name(platform));
    ensure!(
        helper.is_file(),
        "Desktop bundle is missing its application update helper: {}",
        helper.display()
    );
    build::assert_architecture(&helper)
}
