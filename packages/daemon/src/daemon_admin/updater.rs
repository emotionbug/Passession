//! Updates only owned standalone runtimes, never npm or a desktop bundle.
//! The release hash covers the executable, libraries, skills and plugin runtime together.
//! `preparation` stages complete local or official runtimes for the update engine.
use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use futures_util::future::{BoxFuture, FutureExt, Shared, WeakShared};
use paseo_protocol::components::{ComponentId, ComponentRef, ReleaseInventory, parse_inventory};
use serde_json::Value;
use serde_json::json;
use sha2::{Digest, Sha256, Sha512};
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::{Arc, LazyLock, Mutex},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use url::Url;

mod cache;
#[cfg(test)]
mod discovery_tests;
mod preparation;
pub use preparation::{
    input_digest, inspect_runtime, prepare_local_pinned, prepare_release, verify_runtime,
};

const MAX_ARCHIVE: u64 = 1024 * 1024 * 1024;
const MAX_EXPANDED: u64 = 8 * 1024 * 1024 * 1024;
const MARKER: &str = "paseo-runtime.json";
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    collection_version: String,
    component: ComponentRef,
    tag: String,
    source_sha: String,
    platform: String,
    arch: String,
    format: String,
    name: String,
    size: u64,
    digest: String,
    url: Url,
    manifest_digest: String,
}

/// The checked manifest binds the entire artifact and its source provenance.
/// Remote callers supply an identity, never a URL or local path.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReleaseIdentity {
    pub version: String,
    pub manifest_sha512: String,
}

impl ReleaseIdentity {
    pub fn validate(&self) -> Result<()> {
        semver::Version::parse(&self.version)?;
        anyhow::ensure!(
            self.manifest_sha512.len() == 88 && STANDARD.decode(&self.manifest_sha512)?.len() == 64,
            "Invalid release manifest SHA512"
        );
        Ok(())
    }
}

impl Release {
    pub fn identity(&self) -> ReleaseIdentity {
        ReleaseIdentity {
            version: self.collection_version.clone(),
            manifest_sha512: self.manifest_digest.clone(),
        }
    }

    pub fn require_identity(&self, expected: &ReleaseIdentity) -> Result<()> {
        anyhow::ensure!(
            self.identity() == *expected,
            "The official release changed since approval. Check for updates and approve the new release."
        );
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
#[error("GitHub is limiting update checks from this network. Retry after {retry_at}.")]
pub(super) struct GitHubRateLimited {
    pub retry_at: DateTime<Utc>,
}

impl GitHubRateLimited {
    fn from_response(
        status: reqwest::StatusCode,
        headers: &reqwest::header::HeaderMap,
        now: DateTime<Utc>,
    ) -> Option<Self> {
        if !matches!(
            status,
            reqwest::StatusCode::FORBIDDEN | reqwest::StatusCode::TOO_MANY_REQUESTS
        ) {
            return None;
        }
        let header = |name| headers.get(name).and_then(|value| value.to_str().ok());
        let retry_after = header("retry-after");
        let exhausted = header("x-ratelimit-remaining") == Some("0");
        if !exhausted && retry_after.is_none() {
            return None;
        }
        let delay = retry_after.and_then(|value| {
            value
                .parse::<i64>()
                .ok()
                .filter(|seconds| *seconds >= 0)
                .and_then(chrono::TimeDelta::try_seconds)
                .and_then(|delay| now.checked_add_signed(delay))
                .or_else(|| {
                    DateTime::parse_from_rfc2822(value)
                        .ok()
                        .map(|date| date.with_timezone(&Utc))
                })
        });
        let reset = exhausted
            .then(|| header("x-ratelimit-reset")?.parse::<i64>().ok())
            .flatten()
            .and_then(|seconds| DateTime::from_timestamp(seconds, 0));
        Some(Self {
            retry_at: delay
                .into_iter()
                .chain(reset)
                .max()
                .unwrap_or(now + chrono::TimeDelta::minutes(1)),
        })
    }
}

#[derive(Clone, Debug, thiserror::Error)]
#[error("{0:#}")]
pub(super) struct CheckError(Arc<anyhow::Error>);

impl CheckError {
    pub fn rate_limit(&self) -> Option<&GitHubRateLimited> {
        self.0.downcast_ref()
    }
}

impl From<anyhow::Error> for CheckError {
    fn from(error: anyhow::Error) -> Self {
        Self(Arc::new(error))
    }
}

type CheckedRelease = std::result::Result<Arc<Release>, CheckError>;
type CheckFuture = BoxFuture<'static, CheckedRelease>;

#[derive(Default)]
struct ReleaseChecks(Mutex<HashMap<PathBuf, WeakShared<CheckFuture>>>);

impl ReleaseChecks {
    fn flight(
        &self,
        home: &Path,
        discover: impl FnOnce(PathBuf) -> CheckFuture,
    ) -> Shared<CheckFuture> {
        let mut flights = self.0.lock().expect("release check lock poisoned");
        flights.retain(|_, weak| weak.upgrade().is_some_and(|flight| flight.peek().is_none()));
        if let Some(flight) = flights.get(home).and_then(WeakShared::upgrade) {
            return flight;
        }
        let flight = discover(home.to_owned()).shared();
        flights.insert(
            home.to_owned(),
            flight
                .downgrade()
                .expect("new release check has a shared future"),
        );
        flight
    }
}

static RELEASE_CHECKS: LazyLock<ReleaseChecks> = LazyLock::new(ReleaseChecks::default);

/// Metadata/provenance only: no archive download, staging, or installation.
pub(super) async fn check_release(home: &Path) -> CheckedRelease {
    tokio::time::timeout(Duration::from_secs(20), discover_release(home))
        .await
        .context("Official release check timed out")?
}

async fn discover_release(home: &Path) -> CheckedRelease {
    RELEASE_CHECKS
        .flight(home, |home| {
            async move {
                let release = async {
                    let (platform, arch) = platform()?;
                    let client = http()?;
                    discover(
                        &client,
                        &home,
                        DiscoveryTarget {
                            repository: repository()?,
                            channel: channel()?,
                            platform,
                            arch,
                        },
                    )
                    .await
                }
                .await;
                release.map(Arc::new).map_err(CheckError::from)
            }
            .boxed()
        })
        .await
}

pub(crate) fn platform() -> Result<(&'static str, &'static str)> {
    let platform = match std::env::consts::OS {
        "linux" => "linux",
        "macos" => "macos",
        "windows" => "windows",
        _ => bail!("Native updates are unsupported on this platform"),
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        _ => bail!("Native updates are unsupported on this architecture"),
    };
    Ok((platform, arch))
}
/// The only repository whose releases are official for this distribution.
/// Local builds deliberately compile without release identity, yet may still
/// install a genuine newer official release verified against this root.
const OFFICIAL_REPOSITORY: &str = "emotionbug/Passession";
fn repository() -> Result<&'static str> {
    let repository = option_env!("PASEO_UPDATE_REPOSITORY").unwrap_or(OFFICIAL_REPOSITORY);
    let parts = repository.split('/').collect::<Vec<_>>();
    if parts.len() != 2
        || parts.iter().any(|s| {
            s.is_empty()
                || !s
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c))
        })
    {
        bail!("Invalid configured update repository")
    };
    Ok(repository)
}
fn channel() -> Result<&'static str> {
    let channel = option_env!("PASEO_RELEASE_CHANNEL").unwrap_or(if crate::VERSION.contains('-') {
        "beta"
    } else {
        "stable"
    });
    if !matches!(channel, "stable" | "beta") {
        bail!("Invalid configured release channel")
    };
    Ok(channel)
}
fn http() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(format!("paseo-native/{}", crate::VERSION))
        .timeout(Duration::from_secs(180))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            let trusted = trusted_update_host(attempt.url());
            if attempt.previous().len() > 5 || !trusted {
                attempt.error("Untrusted update redirect")
            } else if attempt.previous().len() == 1
                && attempt.previous()[0].host_str() == Some("github.com")
                && attempt.previous()[0]
                    .path()
                    .contains("/releases/latest/download/")
            {
                // Preserve the alias's tag-bearing Location before the asset CDN
                // replaces it with an opaque signed URL.
                attempt.stop()
            } else {
                attempt.follow()
            }
        }))
        .build()?)
}

fn trusted_update_host(url: &Url) -> bool {
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && matches!(
            url.host_str(),
            Some(
                "github.com"
                    | "api.github.com"
                    | "release-assets.githubusercontent.com"
                    | "objects.githubusercontent.com"
            )
        )
}

enum Metadata {
    Bytes(Vec<u8>),
    Redirect(Url),
    NotFound,
}

trait MetadataSource {
    async fn get_metadata(&self, url: &Url) -> Result<Metadata>;
}

impl MetadataSource for reqwest::Client {
    async fn get_metadata(&self, url: &Url) -> Result<Metadata> {
        let mut response = self.get(url.clone()).send().await?;
        if let Some(error) =
            GitHubRateLimited::from_response(response.status(), response.headers(), Utc::now())
        {
            return Err(error.into());
        }
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(Metadata::NotFound);
        }
        if response.status().is_redirection() {
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .context("Missing release redirect location")?
                .to_str()?;
            let location = response.url().join(location)?;
            anyhow::ensure!(trusted_update_host(&location), "Untrusted update redirect");
            return Ok(Metadata::Redirect(location));
        }
        response.error_for_status_ref()?;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            anyhow::ensure!(
                bytes.len() + chunk.len() <= 4 * 1024 * 1024,
                "Release metadata exceeds size limit"
            );
            bytes.extend_from_slice(&chunk);
        }
        Ok(Metadata::Bytes(bytes))
    }
}

async fn json_bytes(client: &impl MetadataSource, url: Url) -> Result<Vec<u8>> {
    match client.get_metadata(&url).await? {
        Metadata::Bytes(bytes) => Ok(bytes),
        Metadata::NotFound => bail!("Release metadata not found at {url}"),
        Metadata::Redirect(_) => bail!("Unexpected release metadata redirect at {url}"),
    }
}

#[derive(Clone, Copy)]
struct DiscoveryTarget<'a> {
    repository: &'a str,
    channel: &'a str,
    platform: &'a str,
    arch: &'a str,
}

fn has_daemon(inventory: &ReleaseInventory, target: &str) -> bool {
    inventory
        .targets
        .get(target)
        .is_some_and(|target| target.components.contains_key(&ComponentId::Daemon))
}

async fn published_release(
    client: &impl MetadataSource,
    repository: &str,
    tag: &str,
) -> Result<Value> {
    let mut url = Url::parse(&format!(
        "https://api.github.com/repos/{repository}/releases/tags/"
    ))?;
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("Invalid release URL"))?
        .pop_if_empty()
        .push(tag);
    let published: Value = serde_json::from_slice(&json_bytes(client, url).await?)?;
    anyhow::ensure!(published["tag_name"] == tag, "GitHub release tag mismatch");
    Ok(published)
}

async fn latest_inventory(
    client: &impl MetadataSource,
    repository: &str,
    name: &str,
) -> Result<Option<(Vec<u8>, Option<String>)>> {
    let alias = Url::parse(&format!(
        "https://github.com/{repository}/releases/latest/download/{name}"
    ))?;
    match client.get_metadata(&alias).await? {
        Metadata::NotFound => Ok(None),
        Metadata::Bytes(bytes) => Ok(Some((bytes, None))),
        Metadata::Redirect(location) => {
            let tag = match location.host_str() {
                Some("github.com") => {
                    let tag = location
                        .path_segments()
                        .and_then(|mut parts| parts.nth_back(1))
                        .context("Missing latest release tag")?;
                    let tag = percent_encoding::percent_decode_str(tag).decode_utf8()?;
                    semver::Version::parse(tag.strip_prefix('v').unwrap_or(&tag))
                        .context("Invalid latest release tag")?;
                    asset_url(
                        &json!({"browser_download_url": location.as_str()}),
                        repository,
                        &tag,
                        name,
                    )?;
                    Some(tag.into_owned())
                }
                Some("release-assets.githubusercontent.com" | "objects.githubusercontent.com") => {
                    None
                }
                _ => bail!("Unexpected latest release redirect host"),
            };
            match client.get_metadata(&location).await? {
                Metadata::NotFound => Ok(None),
                Metadata::Bytes(bytes) => Ok(Some((bytes, tag))),
                Metadata::Redirect(_) => {
                    bail!("Unexpected nested latest release metadata redirect")
                }
            }
        }
    }
}

async fn discover(
    client: &impl MetadataSource,
    home: &Path,
    discovery: DiscoveryTarget<'_>,
) -> Result<Release> {
    let DiscoveryTarget {
        repository,
        channel,
        platform,
        arch,
    } = discovery;
    let target = format!("{platform}-{arch}");
    let name = format!("components-{channel}.json");
    if channel == "stable"
        && let Some((bytes, redirected_tag)) = latest_inventory(client, repository, &name).await?
    {
        let inventory = parse_inventory(&bytes, repository)?;
        let tag = redirected_tag.unwrap_or_else(|| format!("v{}", inventory.version));
        anyhow::ensure!(
            inventory.channel == channel
                && (tag == inventory.version
                    || tag.strip_prefix('v') == Some(inventory.version.as_str())),
            "Latest component inventory collection identity mismatch"
        );
        if has_daemon(&inventory, &target) {
            let key = cache::Key::new(repository, channel, &target, &tag, &bytes);
            // The private entry attests to the exact freshly downloaded bytes.
            // Rebuild the selection from those bytes, not from mutable cached URLs.
            if cache::entries(home).await.contains(&key) {
                return select_daemon(&inventory, platform, arch, &target, key.manifest_sha512);
            }
            let published = published_release(client, repository, &tag).await?;
            let selected = validate_component_release(
                &inventory,
                &published,
                repository,
                channel,
                platform,
                arch,
                key.manifest_sha512.clone(),
            )?;
            verify_release(
                client, &selected, &inventory, &published, &bytes, repository, channel,
            )
            .await?;
            cache::record(home, key).await;
            return Ok(selected);
        }
    }
    discover_from_list(client, home, discovery, &target, &name).await
}

fn asset_url(asset: &Value, repository: &str, tag: &str, name: &str) -> Result<Url> {
    let url = Url::parse(
        asset["browser_download_url"]
            .as_str()
            .context("Missing asset URL")?,
    )?;
    let parts = url
        .path_segments()
        .context("Invalid asset URL")?
        .map(|s| {
            percent_encoding::percent_decode_str(s)
                .decode_utf8()
                .map(|v| v.into_owned())
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut expected = repository.split('/').map(str::to_owned).collect::<Vec<_>>();
    expected.extend([
        "releases".into(),
        "download".into(),
        tag.into(),
        name.into(),
    ]);
    if url.scheme() != "https"
        || url.host_str() != Some("github.com")
        || url.port_or_known_default() != Some(443)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || parts != expected
    {
        bail!("Release asset is not the exact configured repository/tag/name")
    };
    Ok(url)
}
fn unique_asset<'a>(release: &'a Value, name: &str) -> Result<&'a Value> {
    let assets = release["assets"]
        .as_array()
        .context("Missing release assets")?;
    let mut found = assets.iter().filter(|v| v["name"].as_str() == Some(name));
    let asset = found
        .next()
        .with_context(|| format!("Missing release asset {name}"))?;
    if found.next().is_some() {
        bail!("Ambiguous release asset {name}")
    };
    Ok(asset)
}
pub(super) fn validate_component_release(
    inventory: &ReleaseInventory,
    published: &Value,
    repository: &str,
    channel: &str,
    platform: &str,
    arch: &str,
    manifest_digest: String,
) -> Result<Release> {
    paseo_protocol::components::validate_inventory(inventory, repository)?;
    let tag = published["tag_name"]
        .as_str()
        .context("Missing release tag")?;
    anyhow::ensure!(
        inventory.channel == channel
            && (tag == format!("v{}", inventory.version) || tag == inventory.version)
            && published["draft"] != true
            && (channel != "stable" || published["prerelease"] != true),
        "Component inventory collection identity mismatch"
    );
    select_daemon(
        inventory,
        platform,
        arch,
        &format!("{platform}-{arch}"),
        manifest_digest,
    )
}

fn select_daemon(
    inventory: &ReleaseInventory,
    platform: &str,
    arch: &str,
    target: &str,
    manifest_digest: String,
) -> Result<Release> {
    let target = inventory
        .targets
        .get(target)
        .context("Missing daemon target")?;
    let component = target
        .components
        .get(&ComponentId::Daemon)
        .context("Collection has no daemon component for this target")?;
    let format = if platform == "windows" {
        "zip"
    } else {
        "tar.gz"
    };
    let origin =
        paseo_protocol::components::component_artifact_origin(target, ComponentId::Daemon)?;
    anyhow::ensure!(
        origin.package.is_none(),
        "Standalone daemon cannot activate a signed app container"
    );
    anyhow::ensure!(
        component.identity.abi_version == ComponentId::Daemon.initial_abi_version()
            && component.artifact.format == format
            && component.executable == executable_name(platform)
            && component.requires.is_empty()
            && component.artifact.size <= MAX_ARCHIVE,
        "Daemon component ABI or runtime layout is unsupported"
    );
    let url = Url::parse(&component.artifact.url)?;
    let name = url
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .context("Component artifact has no name")?;
    let name = percent_encoding::percent_decode_str(name)
        .decode_utf8()?
        .into_owned();
    // Bind origin separately below: a collection may reuse this exact older component.
    Ok(Release {
        version: component.identity.version.clone(),
        collection_version: inventory.version.clone(),
        tag: component.artifact.origin_tag.clone(),
        source_sha: component.identity.source_sha.clone(),
        platform: platform.into(),
        arch: arch.into(),
        format: format.into(),
        name,
        size: component.artifact.size,
        digest: component.artifact.sha256.clone(),
        url,
        manifest_digest,
        component: component.clone(),
    })
}

pub(super) fn require_origin_component(
    release: &Release,
    origin: &ReleaseInventory,
    published: &Value,
    repository: &str,
) -> Result<()> {
    paseo_protocol::components::validate_inventory(origin, repository)?;
    anyhow::ensure!(
        published["tag_name"] == release.tag
            && published["draft"] != true
            && (release.tag == format!("v{}", origin.version) || release.tag == origin.version)
            && origin.version == release.version
            && origin.source_sha == release.source_sha
            && origin
                .targets
                .get(&release.component.target)
                .and_then(|target| target.components.get(&ComponentId::Daemon))
                == Some(&release.component),
        "Reused daemon component does not match its original release inventory"
    );
    let asset = unique_asset(published, &release.name)?;
    anyhow::ensure!(
        trusted_publisher(&asset["uploader"]["login"], repository),
        "Original daemon artifact publisher is not trusted"
    );
    anyhow::ensure!(
        asset["size"].as_u64() == Some(release.size)
            && asset_url(asset, repository, &release.tag, &release.name)? == release.url,
        "Original component artifact identity mismatch"
    );
    Ok(())
}

fn trusted_publisher(value: &Value, repository: &str) -> bool {
    value.as_str().is_some_and(|login| {
        login == "github-actions[bot]" || Some(login) == repository.split('/').next()
    })
}

pub(super) fn validate_publication(
    inventory: &ReleaseInventory,
    published: &Value,
    asset: &Value,
    provenance_asset: &Value,
    provenance: &Value,
    bytes: &[u8],
    repository: &str,
) -> Result<u64> {
    anyhow::ensure!(
        trusted_publisher(&published["author"]["login"], repository)
            && trusted_publisher(&asset["uploader"]["login"], repository)
            && trusted_publisher(&provenance_asset["uploader"]["login"], repository)
            && published["target_commitish"] == inventory.source_sha,
        "Component inventory publisher or release source is not trusted"
    );
    anyhow::ensure!(
        provenance["schemaVersion"] == 2
            && provenance["workflow"] == ".github/workflows/desktop-release.yml"
            && provenance["version"] == inventory.version
            && provenance["sourceSha"] == inventory.source_sha
            && provenance["channel"] == inventory.channel
            && provenance["inventory"]["name"] == format!("components-{}.json", inventory.channel)
            && provenance["inventory"]["size"].as_u64() == Some(bytes.len() as u64)
            && asset["size"].as_u64() == Some(bytes.len() as u64)
            && provenance["inventory"]["sha256"] == format!("{:x}", Sha256::digest(bytes)),
        "Component inventory does not match its release provenance"
    );
    provenance["runId"]
        .as_u64()
        .or_else(|| {
            provenance["runId"]
                .as_str()
                .and_then(|value| value.parse().ok())
        })
        .filter(|run| *run > 0)
        .context("Missing component publication run identity")
}

fn validate_metadata_asset(asset: &Value, bytes: &[u8], repository: &str) -> Result<()> {
    anyhow::ensure!(
        trusted_publisher(&asset["uploader"]["login"], repository)
            && asset["size"].as_u64() == Some(bytes.len() as u64),
        "Release metadata asset publisher or size changed"
    );
    if let Some(digest) = asset.get("digest").filter(|digest| !digest.is_null()) {
        anyhow::ensure!(
            *digest == format!("sha256:{:x}", Sha256::digest(bytes)),
            "Release metadata asset digest changed"
        );
    }
    Ok(())
}

pub(super) fn validate_target_proof(
    inventory: &ReleaseInventory,
    release: &Release,
    proof: &Value,
) -> Result<u64> {
    let target = inventory
        .targets
        .get(&release.component.target)
        .context("Missing component target")?;
    let source_tag = proof["sourceTag"]
        .as_str()
        .context("Missing target source tag")?;
    let source_tag = source_tag.strip_prefix("refs/tags/").unwrap_or(source_tag);
    let source_version = [
        "desktop-windows-v",
        "desktop-linux-v",
        "desktop-macos-v",
        "desktop-v",
        "android-v",
        "app-v",
        "v",
    ]
    .into_iter()
    .find_map(|prefix| source_tag.strip_prefix(prefix));
    anyhow::ensure!(
        proof["schemaVersion"] == 2
            && proof["workflow"] == ".github/workflows/desktop-release.yml"
            && proof["sha"] == inventory.source_sha
            && source_version == Some(inventory.version.as_str())
            && proof["version"] == inventory.version
            && proof["channel"] == inventory.channel
            && proof["target"] == release.component.target
            && proof["components"] == serde_json::to_value(&target.components)?
            && proof["packages"] == serde_json::to_value(&target.packages)?,
        "Original component target provenance does not match its inventory"
    );
    let files = proof["files"]
        .as_array()
        .filter(|files| files.len() <= 256)
        .context("Invalid target provenance file inventory")?;
    let mut matching = files.iter().filter(|file| file["name"] == release.name);
    let file = matching
        .next()
        .context("Original component archive has no target provenance")?;
    anyhow::ensure!(
        matching.next().is_none()
            && file["size"].as_u64() == Some(release.size)
            && file["sha256"] == release.digest,
        "Original component archive provenance changed"
    );
    proof["runId"]
        .as_u64()
        .or_else(|| proof["runId"].as_str().and_then(|value| value.parse().ok()))
        .filter(|run| *run > 0)
        .context("Missing target publication run identity")
}

async fn verify_publication_run(
    client: &impl MetadataSource,
    repository: &str,
    run: u64,
    require_success: bool,
) -> Result<()> {
    let run: Value = serde_json::from_slice(
        &json_bytes(
            client,
            Url::parse(&format!(
                "https://api.github.com/repos/{repository}/actions/runs/{run}"
            ))?,
        )
        .await?,
    )?;
    anyhow::ensure!(
        run["path"] == ".github/workflows/desktop-release.yml"
            && run["event"] == "workflow_dispatch"
            && run["status"] == "completed"
            && (!require_success || run["conclusion"] == "success")
            && run["head_repository"]["full_name"] == repository,
        "Component publication workflow is not an authenticated release run"
    );
    Ok(())
}

async fn verify_inventory_publication(
    client: &impl MetadataSource,
    inventory: &ReleaseInventory,
    published: &Value,
    bytes: &[u8],
    repository: &str,
    original_component: Option<&Release>,
) -> Result<()> {
    let tag = published["tag_name"]
        .as_str()
        .context("Missing release tag")?;
    let name = format!("components-{}.json", inventory.channel);
    let provenance_name = format!("provenance-components-{}.json", inventory.channel);
    let asset = unique_asset(published, &name)?;
    asset_url(asset, repository, tag, &name)?;
    let provenance_asset = unique_asset(published, &provenance_name)?;
    validate_metadata_asset(asset, bytes, repository)?;
    let provenance_bytes = json_bytes(
        client,
        asset_url(provenance_asset, repository, tag, &provenance_name)?,
    )
    .await?;
    validate_metadata_asset(provenance_asset, &provenance_bytes, repository)?;
    let provenance: Value = serde_json::from_slice(&provenance_bytes)?;
    let run = validate_publication(
        inventory,
        published,
        asset,
        provenance_asset,
        &provenance,
        bytes,
        repository,
    )?;
    verify_publication_run(client, repository, run, true).await?;
    if let Some(component) = original_component {
        let name = format!("provenance-{}.json", component.component.target);
        let asset = unique_asset(published, &name)?;
        let bytes = json_bytes(client, asset_url(asset, repository, tag, &name)?).await?;
        validate_metadata_asset(asset, &bytes, repository)?;
        anyhow::ensure!(
            provenance["targets"][&component.component.target]
                == format!("{:x}", Sha256::digest(&bytes)),
            "Original component target provenance digest changed"
        );
        let proof: Value = serde_json::from_slice(&bytes)?;
        let target_run = validate_target_proof(inventory, component, &proof)?;
        if target_run != run {
            // Recovery may preserve a successful target from a run whose publish
            // job failed. The successful publication binds its exact proof digest.
            verify_publication_run(client, repository, target_run, false).await?;
        }
    }
    verify_source_commit(client, repository, tag, &inventory.source_sha).await
}
async fn discover_from_list(
    client: &impl MetadataSource,
    home: &Path,
    discovery: DiscoveryTarget<'_>,
    target: &str,
    name: &str,
) -> Result<Release> {
    let DiscoveryTarget {
        repository,
        channel,
        platform,
        arch,
    } = discovery;
    let mut newest: Option<(Release, ReleaseInventory, Value, Vec<u8>)> = None;
    for page in 1..=10 {
        let values: Value = serde_json::from_slice(
            &json_bytes(
                client,
                Url::parse(&format!(
                    "https://api.github.com/repos/{repository}/releases?per_page=100&page={page}"
                ))?,
            )
            .await?,
        )?;
        let values = values
            .as_array()
            .context("GitHub did not return a release list")?;
        for release in values {
            if release["draft"] == true || (channel == "stable" && release["prerelease"] == true) {
                continue;
            }
            if !release["assets"]
                .as_array()
                .is_some_and(|assets| assets.iter().any(|a| a["name"] == name))
            {
                continue;
            }
            let asset = unique_asset(release, name)?;
            let tag = release["tag_name"]
                .as_str()
                .context("Missing release tag")?;
            let url = asset_url(asset, repository, tag, name)?;
            let bytes = json_bytes(client, url).await?;
            let digest = STANDARD.encode(Sha512::digest(&bytes));
            let inventory = parse_inventory(&bytes, repository)?;
            if !has_daemon(&inventory, target) {
                continue;
            }
            let candidate = validate_component_release(
                &inventory, release, repository, channel, platform, arch, digest,
            )?;
            if newest.as_ref().is_none_or(|(old, _, _, _)| {
                semver::Version::parse(&candidate.collection_version).unwrap()
                    > semver::Version::parse(&old.collection_version).unwrap()
            }) {
                newest = Some((candidate, inventory, release.clone(), bytes))
            }
        }
        if values.len() < 100 {
            break;
        }
    }
    let (selected, inventory, published, bytes) = newest.with_context(|| {
        format!(
            "No {channel} component inventory is published for {platform}/{arch} in {repository}"
        )
    })?;
    let tag = published["tag_name"]
        .as_str()
        .context("Missing release tag")?;
    let key = cache::Key::with_digest(
        repository,
        channel,
        target,
        tag,
        selected.manifest_digest.clone(),
    );
    if !cache::entries(home).await.contains(&key) {
        verify_release(
            client, &selected, &inventory, &published, &bytes, repository, channel,
        )
        .await?;
        cache::record(home, key).await;
    }
    Ok(selected)
}

async fn verify_release(
    client: &impl MetadataSource,
    selected: &Release,
    inventory: &ReleaseInventory,
    published: &Value,
    bytes: &[u8],
    repository: &str,
    channel: &str,
) -> Result<()> {
    let collection_tag = published["tag_name"]
        .as_str()
        .context("Missing release tag")?;
    verify_inventory_publication(
        client,
        inventory,
        published,
        bytes,
        repository,
        (selected.tag == collection_tag).then_some(selected),
    )
    .await?;
    if selected.tag == collection_tag {
        require_origin_component(selected, inventory, published, repository)?;
    } else {
        let name = format!("components-{channel}.json");
        let origin_release = published_release(client, repository, &selected.tag).await?;
        let asset = unique_asset(&origin_release, &name)?;
        let bytes = json_bytes(client, asset_url(asset, repository, &selected.tag, &name)?).await?;
        let origin = parse_inventory(&bytes, repository)?;
        anyhow::ensure!(
            origin.channel == channel,
            "Component origin channel changed"
        );
        require_origin_component(selected, &origin, &origin_release, repository)?;
        verify_inventory_publication(
            client,
            &origin,
            &origin_release,
            &bytes,
            repository,
            Some(selected),
        )
        .await?;
    }
    Ok(())
}
async fn verify_source_commit(
    client: &impl MetadataSource,
    repository: &str,
    tag: &str,
    source_sha: &str,
) -> Result<()> {
    let mut object: Value = serde_json::from_slice(
        &json_bytes(
            client,
            Url::parse(&format!(
                "https://api.github.com/repos/{repository}/git/ref/tags/{}",
                tag
            ))?,
        )
        .await?,
    )?;
    for _ in 0..8 {
        let sha = object["object"]["sha"]
            .as_str()
            .context("Release tag has no source object")?;
        if !matches!(sha.len(), 40 | 64) || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("Invalid release tag object identity")
        }
        match object["object"]["type"].as_str() {
            Some("commit") => {
                if !sha.eq_ignore_ascii_case(source_sha) {
                    bail!("Release tag commit does not match native source provenance")
                }
                return Ok(());
            }
            Some("tag") => {
                object = serde_json::from_slice(
                    &json_bytes(
                        client,
                        Url::parse(&format!(
                            "https://api.github.com/repos/{repository}/git/tags/{sha}"
                        ))?,
                    )
                    .await?,
                )?;
            }
            _ => bail!("Release tag does not resolve to a source commit"),
        }
    }
    bail!("Release annotated-tag chain exceeds the provenance bound")
}
pub(crate) fn executable_name(platform: &str) -> &'static str {
    if platform == "windows" {
        "paseo.exe"
    } else {
        "paseo"
    }
}
fn read_marker(root: &Path) -> Result<Value> {
    let path = root.join(MARKER);
    let metadata = fs::symlink_metadata(&path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 64 * 1024 {
        bail!("Invalid installed runtime identity marker")
    };
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
pub(super) fn validate_marker(
    marker: &Value,
    version: &str,
    source: &str,
    platform: &str,
    arch: &str,
) -> Result<()> {
    if marker["schemaVersion"] != 1
        || marker["runtime"] != "rust"
        || marker["appId"] != "sh.paseo.daemon"
        || marker["installationKind"] != "standalone-archive"
        || marker["version"] != version
        || marker["sourceSha"] != source
        || marker["platform"] != platform
        || marker["arch"] != arch
    {
        bail!(
            "Runtime ownership/version/provenance mismatch; update through the installation's package manager or desktop application"
        )
    };
    Ok(())
}
pub(crate) fn lease(root: &Path) -> Result<File> {
    let parent = root
        .parent()
        .context("Runtime installation has no parent")?;
    let name = root
        .file_name()
        .context("Runtime installation has no name")?
        .to_string_lossy();
    let path = parent.join(format!(".paseo-{name}.update.lock"));
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = options.open(&path)?;
    if !file.metadata()?.is_file() || fs::symlink_metadata(path)?.file_type().is_symlink() {
        bail!("Invalid native update lock")
    };
    file.try_lock()
        .context("Another process is updating this native installation")?;
    Ok(file)
}
/// Download one discovered component archive, bound to its original size and SHA256.
async fn download(client: &reqwest::Client, release: &Release, archive: &Path) -> Result<()> {
    let mut file = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(archive)
        .await?;
    let response = client
        .get(release.url.clone())
        .send()
        .await?
        .error_for_status()?;
    if response.content_length().is_some_and(|n| n != release.size) {
        bail!("Download content length mismatch")
    }
    let mut stream = response.bytes_stream();
    let mut hash = Sha256::new();
    let mut size = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        size = size
            .checked_add(chunk.len() as u64)
            .context("Artifact size overflow")?;
        if size > release.size {
            bail!("Downloaded artifact exceeds declared size")
        };
        hash.update(&chunk);
        file.write_all(&chunk).await?;
    }
    file.sync_all().await?;
    drop(file);
    if size != release.size || format!("{:x}", hash.finalize()) != release.digest {
        bail!("Downloaded update failed SHA256/size verification")
    }
    Ok(())
}

fn safe_member(path: &Path) -> Result<PathBuf> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        bail!("Unsafe runtime archive path")
    }
    let mut normalized = PathBuf::new();
    for part in path.components() {
        if let Component::Normal(part) = part {
            let text = part.to_str().context("Non-UTF8 archive path")?;
            let stem = text.split('.').next().unwrap_or("").to_ascii_uppercase();
            let reserved = matches!(
                stem.as_str(),
                "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
            ) || stem
                .strip_prefix("COM")
                .or_else(|| stem.strip_prefix("LPT"))
                .is_some_and(|suffix| {
                    matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
                });
            if reserved
                || text.chars().any(|c| c < ' ' || "\\:<>\"|?*".contains(c))
                || text.ends_with('.')
                || text.ends_with(' ')
            {
                bail!("Unsafe cross-platform archive path")
            }
            normalized.push(part);
        }
    }
    if normalized.as_os_str().is_empty() {
        bail!("Empty runtime archive member")
    };
    Ok(normalized)
}
fn write_member(
    root: &Path,
    name: &Path,
    reader: &mut impl Read,
    size: u64,
    mode: u32,
) -> Result<()> {
    let path = root.join(safe_member(name)?);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?
    }
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(if mode & 0o111 != 0 { 0o755 } else { 0o644 });
    }
    // Windows files carry no Unix permission bits.
    #[cfg(not(unix))]
    let _ = mode;
    let mut file = options.open(path)?;
    let copied = std::io::copy(&mut reader.take(size + 1), &mut file)?;
    if copied != size {
        bail!("Runtime archive member size mismatch")
    };
    file.sync_all()?;
    Ok(())
}
pub(super) fn extract(archive: &Path, root: &Path, format: &str) -> Result<()> {
    fs::create_dir(root)?;
    let mut expanded = 0u64;
    let mut names = std::collections::HashSet::new();
    let mut count = 0usize;
    if format == "tar.gz" {
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(File::open(archive)?));
        for entry in archive.entries()? {
            let mut entry = entry?;
            let kind = entry.header().entry_type();
            let raw = entry.path()?.into_owned();
            if kind.is_dir()
                && raw
                    .components()
                    .all(|part| matches!(part, Component::CurDir))
            {
                continue;
            }
            let path = safe_member(&raw)?;
            count += 1;
            if count > 100_000 || !names.insert(path.clone()) {
                bail!("Duplicate or excessive runtime archive members")
            }
            if kind.is_dir() {
                fs::create_dir_all(root.join(path))?;
                continue;
            }
            if !kind.is_file() {
                bail!("Runtime archives cannot contain links or special files")
            }
            let size = entry.size();
            expanded = expanded
                .checked_add(size)
                .context("Archive size overflow")?;
            if expanded > MAX_EXPANDED {
                bail!("Expanded runtime exceeds size limit")
            };
            let mode = entry.header().mode()?;
            write_member(root, &path, &mut entry, size, mode)?;
        }
    } else if format == "zip" {
        let mut archive = zip::ZipArchive::new(File::open(archive)?)?;
        if archive.len() > 100_000 {
            bail!("Excessive runtime archive members")
        }
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index)?;
            let path = safe_member(Path::new(entry.name()))?;
            let mode = entry.unix_mode().unwrap_or(0o644);
            if mode & 0o170000 != 0 && mode & 0o170000 != 0o100000 && mode & 0o170000 != 0o040000 {
                bail!("Runtime archives cannot contain links or special files")
            };
            if !names.insert(path.clone()) {
                bail!("Duplicate runtime archive member")
            }
            if entry.is_dir() {
                fs::create_dir_all(root.join(path))?;
                continue;
            }
            let size = entry.size();
            expanded = expanded
                .checked_add(size)
                .context("Archive size overflow")?;
            if expanded > MAX_EXPANDED {
                bail!("Expanded runtime exceeds size limit")
            };
            write_member(root, &path, &mut entry, size, mode)?;
        }
    } else {
        bail!("Unsupported native runtime archive format")
    }
    Ok(())
}
async fn tree_digest_async(root: PathBuf) -> Result<String> {
    tokio::task::spawn_blocking(move || tree_digest(&root)).await?
}
pub(crate) fn tree_digest(root: &Path) -> Result<String> {
    digest_tree(root, false)
}
/// With `normalized`, Unix modes are hashed as staging assigns them
/// (directories and executable files 0755, other files 0644), so a source
/// directory digest equals the digest of its sealed staged copy.
fn digest_tree(root: &Path, normalized: bool) -> Result<String> {
    if fs::symlink_metadata(root)?.file_type().is_symlink() {
        bail!("Runtime root cannot be a symlink")
    }
    let mut paths = walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .collect::<std::result::Result<Vec<_>, _>>()?;
    paths.sort_by(|a, b| a.path().cmp(b.path()));
    let mut hash = Sha512::new();
    let mut buffer = [0u8; 65536];
    for entry in paths {
        let relative = entry.path().strip_prefix(root)?;
        if relative.as_os_str().is_empty() {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if !metadata.is_dir() && !metadata.is_file() {
            bail!(
                "Runtime contains a symlink or special file: {}",
                relative.display()
            )
        };
        let name = relative
            .to_str()
            .context("Non-UTF8 runtime path")?
            .replace('\\', "/");
        hash.update((name.len() as u64).to_le_bytes());
        hash.update(name.as_bytes());
        hash.update([if metadata.is_dir() { 0 } else { 1 }]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = metadata.permissions().mode() & 0o777;
            let mode = if !normalized {
                mode
            } else if metadata.is_dir() || mode & 0o111 != 0 {
                0o755
            } else {
                0o644
            };
            hash.update(mode.to_le_bytes());
        }
        #[cfg(not(unix))]
        let _ = normalized;
        if metadata.is_file() {
            hash.update(metadata.len().to_le_bytes());
            let mut file = File::open(entry.path())?;
            loop {
                let count = file.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                hash.update(&buffer[..count]);
            }
        }
    }
    Ok(STANDARD.encode(hash.finalize()))
}

pub(crate) async fn validate_binary(path: &Path) -> Result<()> {
    let (platform, arch) = platform()?;
    let mut file = tokio::fs::File::open(path).await?;
    let mut header = [0u8; 64];
    file.read_exact(&mut header).await?;
    match platform {
        "linux" => {
            let machine = u16::from_le_bytes([header[18], header[19]]);
            if &header[..4] != b"\x7fELF"
                || header[4] != 2
                || header[5] != 1
                || machine != (if arch == "x64" { 62 } else { 183 })
            {
                bail!("Native artifact ELF platform/architecture mismatch")
            }
        }
        "macos" => {
            let cpu = u32::from_le_bytes(header[4..8].try_into()?);
            if header[..4] != [0xcf, 0xfa, 0xed, 0xfe]
                || cpu
                    != (if arch == "x64" {
                        0x01000007
                    } else {
                        0x0100000c
                    })
            {
                bail!("Native artifact Mach-O platform/architecture mismatch")
            }
        }
        "windows" => {
            if &header[..2] != b"MZ" {
                bail!("Native artifact is not PE")
            };
            let offset = u32::from_le_bytes(header[60..64].try_into()?) as u64;
            file.seek(std::io::SeekFrom::Start(offset)).await?;
            let mut pe = [0u8; 26];
            file.read_exact(&mut pe).await?;
            let machine = u16::from_le_bytes([pe[4], pe[5]]);
            if &pe[..4] != b"PE\0\0"
                || machine != (if arch == "x64" { 0x8664 } else { 0xaa64 })
                || u16::from_le_bytes([pe[24], pe[25]]) != 0x20b
            {
                bail!("Native artifact PE platform/architecture mismatch")
            }
        }
        _ => unreachable!(),
    }
    Ok(())
}
#[cfg(target_os = "macos")]
async fn native_command(program: &Path, args: &[String]) -> Result<String> {
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(30), command.output())
        .await
        .context("Native signature verification timed out")??;
    if !output.status.success() {
        bail!(
            "Native signature verification failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    };
    Ok(format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    ))
}
/// WinVerifyTrust may fetch revocation and timestamp data, so it runs on a
/// blocking thread under the same deadline the native tools get.
#[cfg(windows)]
async fn windows_signature_check(
    check: impl FnOnce() -> Result<()> + Send + 'static,
) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), tokio::task::spawn_blocking(check))
        .await
        .context("Native signature verification timed out")??
}
/// Official releases must keep the installed publisher identity.
pub(crate) async fn validate_signature(old: &Path, new: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let code = Path::new("/usr/bin/codesign");
        native_command(
            code,
            &[
                "--verify".into(),
                "--strict".into(),
                new.display().to_string(),
            ],
        )
        .await?;
        let old = native_command(
            code,
            &["-d".into(), "--verbose=4".into(), old.display().to_string()],
        )
        .await?;
        let new = native_command(
            code,
            &["-d".into(), "--verbose=4".into(), new.display().to_string()],
        )
        .await?;
        let team = |s: &str| {
            s.lines()
                .find_map(|line| line.strip_prefix("TeamIdentifier=").map(str::to_owned))
        };
        let old_team = team(&old);
        let new_team = team(&new);
        if old_team.as_deref().is_some_and(|v| v != "not set") {
            if old_team != new_team {
                bail!("Native updater signing team differs from installed executable")
            }
        } else if !old.contains("Signature=adhoc") || !new.contains("Signature=adhoc") {
            bail!("Native updater signing identity is not verifiable")
        }
    }
    #[cfg(windows)]
    {
        let (old, new) = (old.to_owned(), new.to_owned());
        windows_signature_check(move || {
            paseo_client::windows_security::verify_update_signature(
                &old,
                &new,
                paseo_client::windows_security::UnsignedPolicy::AllowSignedUpgrade,
            )
        })
        .await?;
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = (old, new);
    }
    Ok(())
}
/// An explicit local build is authorized by the local operator rather than a
/// publisher, so publisher continuity is not inherited; only the candidate's
/// own signature must be intact (macOS: valid, ad-hoc accepted, as applied by
/// `stage-runtime --kind standalone-local`; Windows: valid or unsigned).
async fn validate_local_signature(executable: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    native_command(
        Path::new("/usr/bin/codesign"),
        &[
            "--verify".into(),
            "--strict".into(),
            executable.display().to_string(),
        ],
    )
    .await?;
    #[cfg(windows)]
    {
        let executable = executable.to_owned();
        windows_signature_check(move || {
            paseo_client::windows_security::verify_local_signature(&executable)
        })
        .await?;
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    let _ = executable;
    Ok(())
}
