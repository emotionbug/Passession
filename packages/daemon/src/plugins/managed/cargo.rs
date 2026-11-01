use super::{read_json, reference::string};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{io::Read, path::{Component, Path}, time::Duration};

const ARCHIVE_LIMIT: usize = 64 * 1024 * 1024;
const EXTRACTED_LIMIT: u64 = 256 * 1024 * 1024;

pub fn parse(source: &str) -> Result<(&str, Option<&str>)> {
    let source = source.strip_prefix("cargo:").context("Cargo sources must start with cargo:")?;
    let (name, version) = source.split_once('@').map_or((source, None), |(name, version)| (name, Some(version)));
    if name.is_empty() || name.len() > 64 || !name.as_bytes()[0].is_ascii_alphabetic()
        || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        bail!("Invalid Cargo crate name");
    }
    if let Some(version) = version {
        semver::Version::parse(version).context("Cargo sources require an exact semantic version")?;
    }
    Ok((name, version))
}

pub fn download_url(name: &str, version: &str) -> String {
    format!("https://static.crates.io/crates/{name}/{name}-{version}.crate")
}

pub fn validate(target: &Value) -> Result<()> {
    if target["kind"] != "cargo" { bail!("Invalid Cargo artifact kind"); }
    semver::Version::parse(string(target, "version")?)?;
    let checksum = string(target, "checksum")?;
    if checksum.len() != 64 || !checksum.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        bail!("Cargo artifact checksum must be a lowercase SHA256 digest");
    }
    let url = url::Url::parse(string(target, "resolved")?)?;
    if url.scheme() != "https" || url.host_str() != Some("static.crates.io")
        || !url.username().is_empty() || url.password().is_some() || url.port().is_some()
        || url.query().is_some() || url.fragment().is_some() {
        bail!("Cargo artifacts must use an immutable static.crates.io HTTPS URL");
    }
    Ok(())
}

fn validate_identity(name: &str, target: &Value) -> Result<()> {
    validate(target)?;
    if string(target, "resolved")? != download_url(name, string(target, "version")?) {
        bail!("Cargo artifact URL does not match its crate and version");
    }
    Ok(())
}

async fn fetch(url: &str, limit: usize) -> Result<Vec<u8>> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("paseo/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(120))
        .build()?;
    let mut response = client.get(url).send().await?.error_for_status()?;
    if response.content_length().is_some_and(|size| size > limit as u64) {
        bail!("Cargo response exceeds size limit");
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len() + chunk.len() > limit { bail!("Cargo response exceeds size limit"); }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

pub async fn resolve(name: &str, requested: Option<&str>) -> Result<Value> {
    parse(&format!("cargo:{name}"))?;
    let requested = requested.map(semver::Version::parse).transpose()?;
    let bytes = fetch(&format!("https://crates.io/api/v1/crates/{name}"), 16 * 1024 * 1024).await?;
    let document: Value = serde_json::from_slice(&bytes).context("Invalid crates.io response")?;
    select(name, &document, requested.as_ref())
}

pub(super) fn select(name: &str, document: &Value, requested: Option<&semver::Version>) -> Result<Value> {
    if document["crate"]["id"] != name { bail!("Cargo registry returned a different crate"); }
    let mut selected = None;
    for item in document["versions"].as_array().context("Cargo registry versions are missing")? {
        let version = semver::Version::parse(string(item, "num")?)?;
        if item["yanked"].as_bool().context("Cargo registry yanked flag is missing")?
            || requested.map_or(!version.pre.is_empty(), |wanted| *wanted != version) {
            continue;
        }
        if selected.as_ref().is_none_or(|(previous, _)| version.cmp_precedence(previous).is_gt()) {
            selected = Some((version, item));
        }
    }
    let (version, item) = selected.context("No matching non-yanked Cargo version; choose an available exact version")?;
    let target = json!({"kind":"cargo","version":version.to_string(),"checksum":string(item,"checksum")?,"resolved":download_url(name,&version.to_string())});
    validate_identity(name, &target)?;
    Ok(target)
}

pub async fn acquire(source: &str, root: &Path, reviewed: Option<&Value>) -> Result<(String, Value)> {
    let (name, requested) = parse(source)?;
    let target = match reviewed {
        Some(target) => {
            validate_identity(name, target)?;
            if requested.is_some_and(|version| target["version"] != version) { bail!("Cargo target changed since review"); }
            target.clone()
        }
        None => resolve(name, requested).await?,
    };
    let bytes = fetch(string(&target, "resolved")?, ARCHIVE_LIMIT).await?;
    verify_digest(&bytes, string(&target, "checksum")?)?;
    let output = root.join("crate");
    let prefix = format!("{name}-{}", string(&target, "version")?);
    // Keep extraction in this future: cancellation must not race staging cleanup
    // against an independently running extraction worker.
    extract(&bytes, &output, &prefix)?;
    tokio::fs::write(root.join("source.crate"), &bytes).await?;
    tokio::fs::write(root.join("artifact.json"), serde_json::to_vec(&target)?).await?;
    Ok((name.to_owned(), target))
}

fn verify_digest(bytes: &[u8], expected: &str) -> Result<()> {
    if hex::encode(Sha256::digest(bytes)) != expected { bail!("Cargo artifact SHA256 checksum mismatch"); }
    Ok(())
}

pub(super) fn extract(bytes: &[u8], output: &Path, prefix: &str) -> Result<()> {
    std::fs::create_dir(output)?;
    let decoder = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(decoder.take(EXTRACTED_LIMIT + 1));
    let mut total = 0u64;
    let mut entries = 0usize;
    for entry in archive.entries()? {
        let mut entry = entry?;
        entries += 1;
        if entries > 100_000 { bail!("Cargo archive has too many entries"); }
        let path = entry.path()?.into_owned();
        let mut components = path.components();
        let relative = if prefix.is_empty() {
            path.as_path()
        } else {
            if components.next() != Some(Component::Normal(std::ffi::OsStr::new(prefix))) {
                bail!("Cargo archive has an unexpected package root");
            }
            components.as_path()
        };
        if !relative.components().all(|part| matches!(part, Component::Normal(_)))
            || relative.to_string_lossy().contains(['\\', ':']) {
            bail!("Cargo archive contains an unsafe path");
        }
        let kind = entry.header().entry_type();
        if !kind.is_file() && !kind.is_dir() { bail!("Cargo archive links and special files are not allowed"); }
        if relative.as_os_str().is_empty() {
            if kind.is_dir() { continue; }
            bail!("Cargo archive package root must be a directory");
        }
        total = total.checked_add(entry.size()).context("Cargo archive size overflow")?;
        if total > EXTRACTED_LIMIT { bail!("Cargo archive exceeds extracted size limit"); }
        let destination = output.join(relative);
        if kind.is_dir() {
            std::fs::create_dir_all(&destination)?;
        } else {
            std::fs::create_dir_all(destination.parent().context("Missing archive entry parent")?)?;
            let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&destination)?;
            std::io::copy(&mut entry, &mut file)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = if entry.header().mode()? & 0o111 != 0 { 0o700 } else { 0o600 };
                file.set_permissions(std::fs::Permissions::from_mode(mode))?;
            }
        }
    }
    Ok(())
}

pub async fn artifact(root: &Path, name: &str) -> Result<Value> {
    for path in [root.join("artifact.json"), root.join("source.crate")] {
        let metadata = tokio::fs::symlink_metadata(&path).await?;
        if !metadata.is_file() || metadata.file_type().is_symlink() { bail!("Cargo acquisition artifact must be a regular file"); }
    }
    let target = read_json(&root.join("artifact.json")).await?;
    validate_identity(name, &target)?;
    let file = tokio::fs::File::open(root.join("source.crate")).await?;
    let mut bytes = Vec::new();
    tokio::io::AsyncReadExt::read_to_end(&mut tokio::io::AsyncReadExt::take(file, ARCHIVE_LIMIT as u64 + 1), &mut bytes).await?;
    if bytes.len() > ARCHIVE_LIMIT { bail!("Cargo archive exceeds size limit"); }
    verify_digest(&bytes, string(&target, "checksum")?)?;
    Ok(target)
}
