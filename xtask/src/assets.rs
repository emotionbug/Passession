use crate::{build::assert_architecture, common::*};
use anyhow::{Context, Result, ensure};
use reqwest::blocking::Client;
use std::{
    env, fs,
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

/// Download `url` once into `destination`, accepting only the pinned SHA-256.
pub(crate) fn download(url: &str, destination: &Path, digest: &str, github: bool) -> Result<()> {
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
            .join("paseo-packaging")
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
