use crate::{build::assert_architecture, common::*};
use anyhow::{Context, Result, ensure};
use reqwest::blocking::Client;
use serde_json::json;
use std::{
    env, fs,
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

fn download(url: &str, destination: &Path, digest: &str, github: bool) -> Result<()> {
    if destination.is_file() && sha256_file(destination)? == digest {
        return Ok(());
    }
    let client = Client::builder()
        .timeout(Duration::from_secs(600))
        .user_agent("paseo-xtask")
        .build()?;
    let mut request = client.get(url);
    if github {
        request = request
            .header("Accept", "application/octet-stream")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Ok(token) = env::var("GH_TOKEN") {
            request = request.bearer_auth(token);
        }
    }
    let mut response = request.send()?.error_for_status()?;
    let parent = destination.parent().context("Asset path has no parent")?;
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    response.copy_to(&mut temporary)?;
    temporary.flush()?;
    ensure!(
        sha256_file(temporary.path())? == digest,
        "Pinned asset checksum mismatch: {}",
        destination.display()
    );
    temporary
        .persist(destination)
        .map_err(|error| error.error)?;
    Ok(())
}

pub fn neovim(runtime: &Path) -> Result<()> {
    let (name, digest) = match (platform()?, arch()?) {
        ("linux", "x64") => (
            "nvim-linux-x86_64.tar.gz",
            "474430d53e6264f6d6dd18db42d6dc9df3a1b56ca9e88a325bbf860e1a811d87",
        ),
        ("linux", "arm64") => (
            "nvim-linux-arm64.tar.gz",
            "8f1f64a0bdb97247034038c3823c6cbad5bdf9ecd5751b85494b71c3ee04c815",
        ),
        ("macos", "x64") => (
            "nvim-macos-x86_64.tar.gz",
            "481424f1dbc85f637d57e47d27d042aa9c883e5a4fe9ee64fc4ee9573b221998",
        ),
        ("macos", "arm64") => (
            "nvim-macos-arm64.tar.gz",
            "1dbd148222b051ba6c307c4d17a318ed083df018a4f6a5a0ce46955ffc9ab87b",
        ),
        ("windows", "x64") => (
            "nvim-win64.zip",
            "4e12b7103b601ffc8f25954b4da100f339faa73dccb69a5b135e897fa3d893ed",
        ),
        ("windows", "arm64") => (
            "nvim-win-arm64.zip",
            "2e58d259ffcc6145e5112ea02330126c6b57da09087bb9265fb2fa9aa10b485c",
        ),
        _ => anyhow::bail!("No pinned Neovim distribution for this target"),
    };
    let archive = cache()?
        .join("paseo-flutter-packaging/neovim-0.12.6")
        .join(name);
    download(
        &format!("https://github.com/neovim/neovim/releases/download/v0.12.6/{name}"),
        &archive,
        digest,
        false,
    )?;
    let temporary = tempfile::tempdir()?;
    run(command("tar")?
        .arg("-xf")
        .arg(&archive)
        .arg("-C")
        .arg(temporary.path()))?;
    let directory = name
        .strip_suffix(".tar.gz")
        .or_else(|| name.strip_suffix(".zip"))
        .context("Unknown Neovim archive format")?;
    let destination = runtime.join("neovim");
    copy_tree(&temporary.path().join(directory), &destination)?;
    let executable = destination
        .join("bin")
        .join(if cfg!(windows) { "nvim.exe" } else { "nvim" });
    ensure!(
        destination
            .join("share/nvim/runtime/doc/uganda.txt")
            .is_file(),
        "Incomplete official Neovim license/runtime distribution"
    );
    assert_architecture(&executable)?;
    write_json(
        &destination.join("paseo-provenance.json"),
        &json!({"version":"0.12.6","asset":name,"sha256":digest}),
    )
}

pub fn appimage_tools() -> Result<()> {
    ensure!(
        platform()? == "linux",
        "AppImage tools require a Linux host"
    );
    let items = if arch()? == "x64" {
        [
            (
                "APPIMAGETOOL",
                "appimagetool",
                "AppImage/appimagetool",
                324406736_u64,
                "ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0",
            ),
            (
                "APPIMAGE_RUNTIME_FILE",
                "runtime",
                "AppImage/type2-runtime",
                326011592,
                "2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d",
            ),
        ]
    } else {
        [
            (
                "APPIMAGETOOL",
                "appimagetool",
                "AppImage/appimagetool",
                324406707_u64,
                "f0837e7448a0c1e4e650a93bb3e85802546e60654ef287576f46c71c126a9158",
            ),
            (
                "APPIMAGE_RUNTIME_FILE",
                "runtime",
                "AppImage/type2-runtime",
                326011362,
                "00cbdfcf917cc6c0ff6d3347d59e0ca1f7f45a6df1a428a0d6d8a78664d87444",
            ),
        ]
    };
    for (key, name, repository, asset, digest) in items {
        let output = cache()?
            .join("paseo-flutter-packaging")
            .join(arch()?)
            .join(format!("{name}-{asset}"));
        download(
            &format!("https://api.github.com/repos/{repository}/releases/assets/{asset}"),
            &output,
            digest,
            true,
        )?;
        chmod(&output, 0o755)?;
        assert_architecture(&output)?;
        let setting = format!("{key}={}\n", output.display());
        ensure!(
            !output.to_string_lossy().contains(['\n', '\r']),
            "Invalid AppImage cache path"
        );
        if let Some(path) = env::var_os("GITHUB_ENV") {
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(PathBuf::from(path))?
                .write_all(setting.as_bytes())?;
        }
        print!("{setting}");
    }
    Ok(())
}
