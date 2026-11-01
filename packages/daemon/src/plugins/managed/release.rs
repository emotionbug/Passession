use super::{cargo, read_json, reference::string};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{io::Cursor, path::{Component, Path}, time::Duration};
use tokio::io::AsyncReadExt;

const ARCHIVE_LIMIT: usize = 256 * 1024 * 1024;
const EXTRACTED_LIMIT: u64 = 256 * 1024 * 1024;

pub fn origin(value: &str) -> Result<String> {
    let url = url::Url::parse(value)?;
    if url.scheme() != "https" || url.host_str().is_none() || !url.username().is_empty()
        || url.password().is_some() || url.fragment().is_some() {
        bail!("Plugin releases require an HTTPS URL without credentials or fragments");
    }
    Ok(url.origin().ascii_serialization())
}

pub fn target(input: &Value, url: &str) -> Result<Value> {
    origin(url)?;
    let path = url::Url::parse(url)?.path().to_owned();
    let format = input.get("format").map(|v| v.as_str().context("Invalid release archive format")).transpose()?
        .or_else(|| if path.ends_with(".tar.gz") || path.ends_with(".tgz") { Some("tar.gz") } else if path.ends_with(".zip") { Some("zip") } else { None })
        .context("Release archive format is required: tar.gz or zip")?;
    let target = json!({"kind":"release","url":url,"version":string(input,"version")?,"sha256":string(input,"sha256")?,"format":format});
    validate(&target)?;
    Ok(target)
}

pub fn validate(target: &Value) -> Result<()> {
    if target["kind"] != "release" { bail!("Invalid release artifact kind"); }
    origin(string(target,"url")?)?;
    semver::Version::parse(string(target,"version")?)?;
    if !matches!(string(target,"format")?, "tar.gz" | "zip") { bail!("Release archive format must be tar.gz or zip"); }
    let digest = string(target,"sha256")?;
    if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        bail!("Release SHA256 must be a lowercase 64-character digest");
    }
    Ok(())
}

pub async fn acquire(root: &Path, target: &Value) -> Result<()> {
    validate(target)?;
    let client = reqwest::Client::builder().https_only(true)
        .redirect(reqwest::redirect::Policy::limited(5))
        .timeout(Duration::from_secs(120)).build()?;
    let mut response = client.get(string(target,"url")?).send().await?.error_for_status()?;
    if response.content_length().is_some_and(|size| size > ARCHIVE_LIMIT as u64) { bail!("Release archive exceeds size limit"); }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len() + chunk.len() > ARCHIVE_LIMIT { bail!("Release archive exceeds size limit"); }
        bytes.extend_from_slice(&chunk);
    }
    verify_digest(&bytes, target)?;
    let output = root.join("release");
    match string(target,"format")? {
        "tar.gz" => cargo::extract(&bytes, &output, "")?,
        "zip" => extract_zip(&bytes, &output)?,
        _ => unreachable!(),
    }
    tokio::fs::write(root.join("source.archive"), &bytes).await?;
    tokio::fs::write(root.join("artifact.json"), serde_json::to_vec(target)?).await?;
    Ok(())
}

pub(super) fn extract_zip(bytes: &[u8], output: &Path) -> Result<()> {
    std::fs::create_dir(output)?;
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))?;
    if archive.len() > 100_000 { bail!("Release archive has too many entries"); }
    let mut total = 0u64;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let path = Path::new(entry.name());
        if path.as_os_str().is_empty() || !path.components().all(|part| matches!(part, Component::Normal(_)))
            || entry.name().contains(['\\', ':']) {
            bail!("Release archive contains an unsafe path");
        }
        let mode = entry.unix_mode().unwrap_or(0);
        let kind = mode & 0o170000;
        if kind != 0 && kind != 0o100000 && kind != 0o040000 { bail!("Release archive links and special files are not allowed"); }
        let destination = output.join(path);
        total = total.checked_add(entry.size()).context("Release archive size overflow")?;
        if total > EXTRACTED_LIMIT { bail!("Release archive exceeds extracted size limit"); }
        if entry.is_dir() {
            std::fs::create_dir_all(destination)?;
        } else {
            std::fs::create_dir_all(destination.parent().context("Missing archive entry parent")?)?;
            let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(destination)?;
            let size = entry.size();
            let copied = std::io::copy(&mut std::io::Read::take(&mut entry, size + 1), &mut file)?;
            if copied != size { bail!("Release archive entry size mismatch"); }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                file.set_permissions(std::fs::Permissions::from_mode(if mode & 0o111 != 0 { 0o700 } else { 0o600 }))?;
            }
        }
    }
    Ok(())
}

fn verify_digest(bytes: &[u8], target: &Value) -> Result<()> {
    if hex::encode(Sha256::digest(bytes)) != string(target,"sha256")? { bail!("Release archive SHA256 checksum mismatch"); }
    Ok(())
}

pub async fn artifact(root: &Path) -> Result<Value> {
    for path in [root.join("artifact.json"), root.join("source.archive")] {
        let metadata = tokio::fs::symlink_metadata(path).await?;
        if !metadata.is_file() || metadata.file_type().is_symlink() { bail!("Release acquisition artifact must be a regular file"); }
    }
    let target = read_json(&root.join("artifact.json")).await?;
    validate(&target)?;
    let mut bytes = Vec::new();
    tokio::fs::File::open(root.join("source.archive")).await?.take(ARCHIVE_LIMIT as u64 + 1).read_to_end(&mut bytes).await?;
    if bytes.len() > ARCHIVE_LIMIT { bail!("Release archive exceeds size limit"); }
    verify_digest(&bytes, &target)?;
    Ok(target)
}
