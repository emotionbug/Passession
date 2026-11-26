//! Authenticated collection discovery and independent desktop component staging.
use super::app_update;
use super::component_store::{self, InstalledRef, Store};
use crate::action::PlatformError;
use base64::Engine as _;
use chrono::{DateTime, TimeDelta, Utc};
use paseo_protocol::components::{self, ComponentId, ComponentRef, ReleaseInventory};
use reqwest::{StatusCode, header};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256, Sha512};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, Weak},
    time::Duration,
};
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, OnceCell};
use url::Url;
const REPOSITORY: &str = component_store::REPOSITORY;
const MAX_VERIFIED_RELEASES: usize = 8;
pub(super) const RATE_LIMIT_CODE: &str = "github_rate_limited";
pub(super) const RATE_LIMIT_MESSAGE: &str = "GitHub is limiting update checks from this network.";
const DEFAULT_CHANNEL: &str = match option_env!("PASEO_RELEASE_CHANNEL") {
    Some(v) => v,
    None => "stable",
};
fn error(message: impl Into<String>) -> PlatformError {
    PlatformError::new("COMPONENT_UPDATE_FAILED", message)
}
fn convert(error: impl std::fmt::Display) -> PlatformError {
    self::error(error.to_string())
}

#[derive(Clone, Debug)]
pub(super) enum DiscoveryError {
    Platform(PlatformError),
    Status(StatusCode),
    RateLimited(DateTime<Utc>),
}

impl From<PlatformError> for DiscoveryError {
    fn from(error: PlatformError) -> Self {
        Self::Platform(error)
    }
}

impl From<DiscoveryError> for PlatformError {
    fn from(error: DiscoveryError) -> Self {
        match error {
            DiscoveryError::Platform(error) => error,
            DiscoveryError::Status(status) => {
                self::error(format!("Component source returned HTTP {status}"))
            }
            DiscoveryError::RateLimited(retry_at) => Self::new(
                RATE_LIMIT_CODE,
                format!(
                    "{RATE_LIMIT_MESSAGE} Try again after {}.",
                    retry_at.to_rfc3339()
                ),
            ),
        }
    }
}

type DiscoveryResult<T> = Result<T, DiscoveryError>;
type DiscoveryFlight = OnceCell<DiscoveryResult<Arc<Release>>>;

#[derive(Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct VerifiedRelease {
    repository: String,
    channel: String,
    tag: String,
    manifest_sha512: String,
    target: String,
}

async fn verified_releases(path: &Path) -> Vec<VerifiedRelease> {
    let Ok(json) = app_update::read_json(path).await else {
        return Vec::new();
    };
    serde_json::from_value::<Vec<VerifiedRelease>>(json)
        .ok()
        .filter(|entries| entries.len() <= MAX_VERIFIED_RELEASES)
        .unwrap_or_default()
}

struct Release {
    inventory: ReleaseInventory,
    bytes: Vec<u8>,
    manifest_sha512: sha2::digest::Output<Sha512>,
}
impl Release {
    fn parse(bytes: Vec<u8>, channel: &str) -> Result<Self, PlatformError> {
        let inventory = components::parse_inventory(&bytes, REPOSITORY).map_err(convert)?;
        if inventory.channel != channel {
            return Err(error("Collection release channel differs"));
        }
        Ok(Self {
            manifest_sha512: Sha512::digest(&bytes),
            inventory,
            bytes,
        })
    }

    fn require_tag(&self, tag: &str) -> Result<(), PlatformError> {
        if tag == self.inventory.version
            || tag.strip_prefix('v') == Some(self.inventory.version.as_str())
        {
            Ok(())
        } else {
            Err(error(
                "Collection release tag differs from inventory version",
            ))
        }
    }

    fn has_app_target(&self, target: &str) -> bool {
        self.inventory
            .targets
            .get(target)
            .is_some_and(|entries| entries.components.contains_key(&ComponentId::App))
    }

    fn manifest_id(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(&self.manifest_sha512[..])
    }

    fn pin(&self) -> Value {
        json!({"version": self.inventory.version, "manifestSha512": self.manifest_id()})
    }
}

fn plan_app_updates(
    target: &str,
    current: &components::TargetInventory,
    available: &components::TargetInventory,
    selected: Option<&BTreeSet<ComponentId>>,
) -> Result<components::ComponentUpdatePlan, PlatformError> {
    let all_app_changes;
    let selected = match selected {
        Some(selected) if !selected.contains(&ComponentId::Daemon) => selected,
        Some(_) => return Err(error("Daemon activation is not an app update.")),
        None => {
            all_app_changes = components::changed_components(current, available)
                .into_iter()
                .filter(|id| *id != ComponentId::Daemon)
                .collect();
            &all_app_changes
        }
    };
    components::plan_updates(target, REPOSITORY, current, available, Some(selected))
        .map_err(convert)
}

fn app_requirements(requirements: &BTreeMap<ComponentId, u32>) -> BTreeMap<ComponentId, u32> {
    requirements
        .iter()
        .filter(|(id, _)| **id != ComponentId::Daemon)
        .map(|(id, version)| (*id, *version))
        .collect()
}
pub(crate) struct Updater {
    version: String,
    client: reqwest::Client,
    last_error: Mutex<Option<String>>,
    flights: Mutex<BTreeMap<(String, String), Weak<DiscoveryFlight>>>,
    cache_root: PathBuf,
    cache_io: Mutex<()>,
    #[cfg(test)]
    test_server: Option<Url>,
}
pub(super) struct HelperPreparation<'a> {
    pub channel: &'a str,
    pub operation: &'a str,
    pub directory: &'a Path,
    pub expected: &'a Value,
    pub selected: Option<&'a BTreeSet<ComponentId>>,
}
impl Updater {
    pub fn new(version: String) -> Self {
        Self {
            version,
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(30))
                .user_agent("Passession-Component-Updater")
                .build()
                .expect("reqwest client"),
            last_error: Default::default(),
            flights: Default::default(),
            cache_root: crate::paths::app_paths().support_dir.clone(),
            cache_io: Default::default(),
            #[cfg(test)]
            test_server: None,
        }
    }
    pub fn default_channel() -> &'static str {
        DEFAULT_CHANNEL
    }
    pub async fn diagnostics(&self) -> Value {
        let components = Store::discover()
            .and_then(|store| store.map(|s| s.selected()).transpose())
            .map(|selected| selected.map(|set| component_store::identities(&set)));
        json!({"currentVersion":self.version,"releaseChannel":DEFAULT_CHANNEL,"repository":REPOSITORY,
            "platform":std::env::consts::OS,"abi":format!("{}_{}",std::env::consts::OS,std::env::consts::ARCH),
            "configured":!self.version.is_empty(),"manifestPattern":"components-<channel>.json",
            "components":components.as_ref().ok().and_then(|v|v.as_ref()),"componentError":components.as_ref().err().map(|e|e.to_string()),
            "lastError":*self.last_error.lock().await})
    }
    pub(super) async fn check_app_release(&self, channel: &str) -> DiscoveryResult<Value> {
        let result = self.fetch_app_release(channel).await;
        *self.last_error.lock().await = result.as_ref().err().map(|e| match e {
            DiscoveryError::Platform(e) => e.message.clone(),
            e => PlatformError::from(e.clone()).message,
        });
        result
    }
    async fn fetch_app_release(&self, channel: &str) -> DiscoveryResult<Value> {
        let Some(store) = Store::discover().map_err(convert)? else {
            return Ok(
                json!({"current":{"version":self.version,"sourceSha":option_env!("PASEO_BUILD_SOURCE_SHA").unwrap_or("")},"supported":false,"updateAvailable":false,"latest":null,"owner":{"kind":"desktopApp"},"supportError":"One-time packaged upgrade required: this installation has no component inventory.","checkError":null,"components":{},"componentRequirements":{},"availableComponents":[],"restartGroup":"none","atomicGroups":[]}),
            );
        };
        let installed = store.selected().map_err(convert)?;
        let current = store.inventory(&installed).map_err(convert)?;
        let release = self.discover(channel, &store.target).await?;
        let available = release
            .inventory
            .targets
            .get(&store.target)
            .ok_or_else(|| error("Collection has no inventory for this target"))?;
        let plan = plan_app_updates(&store.target, &current, available, None)?;
        let changes:Vec<_>=plan.changed.iter().map(|id|json!({"id":id,"current":current.components[id].identity,"target":available.components[id].identity,"downloadSize":available.components[id].artifact.size,"artifactSha256":available.components[id].artifact.sha256,"activationOnly":current.components[id].identity==available.components[id].identity,"requires":app_requirements(&available.components[id].requires)})).collect();
        let requirements: BTreeMap<_, _> = current
            .components
            .iter()
            .filter(|(id, _)| **id != ComponentId::Daemon)
            .map(|(id, c)| (*id, app_requirements(&c.requires)))
            .collect();
        Ok(
            json!({"current":{"version":self.version,"sourceSha":option_env!("PASEO_BUILD_SOURCE_SHA").unwrap_or("")},"supported":true,"updateAvailable":!plan.changed.is_empty(),"latest":release.pin(),"owner":{"kind":"desktopApp"},"supportError":null,"checkError":null,"components":component_store::app_identities(&installed),"componentRequirements":requirements,"availableComponents":changes,"restartGroup":plan.restart_group,"atomicGroups":available.atomic_groups}),
        )
    }
    pub(super) async fn prepare_helper(
        &self,
        preparation: HelperPreparation<'_>,
    ) -> Result<Value, PlatformError> {
        let HelperPreparation {
            channel,
            operation,
            directory,
            expected,
            selected,
        } = preparation;
        let store = Store::discover().map_err(convert)?.ok_or_else(|| {
            error("One-time packaged upgrade required: component inventory is missing")
        })?;
        store.ensure_directories().await.map_err(convert)?;
        let selection = store.selection().map_err(convert)?;
        if selection.trial.is_some() {
            return Err(error(
                "A component trial already needs completion or recovery",
            ));
        }
        let current = store.inventory(&selection.active).map_err(convert)?;
        let release = self.discover(channel, &store.target).await?;
        if release.pin() != *expected {
            return Err(error("The collection changed. Check for updates again."));
        }
        let available = release
            .inventory
            .targets
            .get(&store.target)
            .ok_or_else(|| error("Collection target is missing"))?;
        let plan = plan_app_updates(&store.target, &current, available, selected)?;
        let inventory_path = directory.join("inventory.json");
        write_private(&inventory_path, &release.bytes).await?;
        let mut target_components = selection.active.clone();
        let mut staged = BTreeMap::new();
        let mut archives = BTreeMap::<String, PathBuf>::new();
        let mut expanded = BTreeMap::<String, PathBuf>::new();
        for id in &plan.changed {
            let component = &plan.components[id];
            let digest = &component.artifact.sha256;
            let archive = if let Some(path) = archives.get(digest) {
                path.clone()
            } else {
                let path = directory.join(format!("{digest}.{}", component.artifact.format));
                let cached = store.directory.join("archives").join(digest);
                if cached.try_exists()? {
                    store
                        .image(&InstalledRef {
                            component: component.clone(),
                            root: format!("objects/{digest}"),
                        })
                        .map_err(convert)?;
                    copy_private(&cached, &path, false).await?;
                } else {
                    self.download(&component.artifact.url, &path, component.artifact.size)
                        .await?;
                }
                if component_store::sha256_file(&path).map_err(convert)? != *digest {
                    return Err(error("Downloaded component archive digest differs"));
                }
                archives.insert(digest.clone(), path.clone());
                path
            };
            let root = if let Some(root) = expanded.get(digest) {
                root.clone()
            } else {
                let root = directory.join(format!("validate-{digest}"));
                let source = archive.clone();
                let dest = root.clone();
                let format = component.artifact.format.clone();
                tokio::task::spawn_blocking(move || {
                    component_store::extract(&source, &format, &dest)
                })
                .await
                .map_err(convert)?
                .map_err(convert)?;
                expanded.insert(digest.clone(), root.clone());
                root
            };
            self.validate_staged(&store, component, &root, &archive, directory)
                .await?;
            target_components.insert(
                *id,
                InstalledRef {
                    component: component.clone(),
                    root: format!("objects/{digest}"),
                },
            );
            staged.insert(*id, archive);
        }
        Ok(
            json!({"schemaVersion":2,"mode":"componentTransaction","operationId":operation,"operationDirectory":directory,"storeDirectory":store.directory,"baseRoot":store.base_root,"installationId":store.installation_id,"target":store.target,"inventoryPath":inventory_path,"manifestSha512":format!("{:x}",release.manifest_sha512),"currentGeneration":selection.generation,"current":selection.active,"targetComponents":target_components,"staged":staged,"restartGroup":plan.restart_group,"currentExecutable":component_store::canonical_path(std::env::current_exe()?).map_err(convert)?,"parentProcess":component_store::process_identity(std::process::id()).map_err(convert)?}),
        )
    }
    async fn validate_staged(
        &self,
        store: &Store,
        component: &ComponentRef,
        root: &Path,
        archive: &Path,
        directory: &Path,
    ) -> Result<(), PlatformError> {
        let image = root.join(&component.executable);
        if !image.is_file()
            || !component_store::same_path(
                &component_store::canonical_path(&image).map_err(convert)?,
                &image,
            )
            || component_store::sha256_file(&image).map_err(convert)? != component.executable_sha256
        {
            return Err(error("Staged component image digest/path differs"));
        }
        if component.identity.abi_version != component.id.initial_abi_version() {
            return Err(error("Unsupported component ABI"));
        }
        validate_publisher(
            &store
                .image(&store.selected().map_err(convert)?[&ComponentId::App])
                .map_err(convert)?,
            &image,
            component.id,
        )
        .await?;
        match component.id {
            ComponentId::Daemon => return Err(error("Daemon activation is not an app update.")),
            ComponentId::UpdateHelper => {
                let output = command_output(&image, &["--identity".as_ref()]).await?;
                let value = component_store::strict_json(output.as_bytes()).map_err(convert)?;
                if value["schemaVersion"] != 1
                    || value["id"] != json!(component.id)
                    || value["identity"] != json!(component.identity)
                {
                    return Err(error(
                        "Staged executable reports a different component identity/ABI",
                    ));
                }
            }
            ComponentId::Tailnet => {
                let probe = directory.join(format!("probe-{}.json", component.id));
                super::app_update::atomic_json(
                    &probe,
                    &json!({"component":component,"archive":archive,"root":root}),
                )
                .await?;
                command_output(
                    &std::env::current_exe()?,
                    &["--component-tailnet-probe".as_ref(), probe.as_os_str()],
                )
                .await?;
            }
            ComponentId::App => {
                let output = command_output(&image, &["--component-identity".as_ref()]).await?;
                let value = component_store::strict_json(output.as_bytes()).map_err(convert)?;
                if value["schemaVersion"] != 1
                    || value["id"] != "app"
                    || value["identity"] != json!(component.identity)
                {
                    return Err(error("Staged GUI reports a different identity/ABI"));
                }
            }
        }
        Ok(())
    }
    async fn discover(&self, channel: &str, target: &str) -> DiscoveryResult<Arc<Release>> {
        if !matches!(channel, "stable" | "beta") {
            return Err(error("Invalid component channel").into());
        }
        components::validate_repository(REPOSITORY).map_err(convert)?;
        let flight = {
            let mut flights = self.flights.lock().await;
            flights.retain(|_, flight| flight.strong_count() != 0);
            let entry = flights
                .entry((channel.to_owned(), target.to_owned()))
                .or_default();
            entry.upgrade().unwrap_or_else(|| {
                let flight = Arc::new(OnceCell::new());
                *entry = Arc::downgrade(&flight);
                flight
            })
        };
        // A later check fetches fresh inventory; only overlapping callers share the cell.
        flight
            .get_or_init(|| self.discover_uncached(channel, target))
            .await
            .clone()
    }

    async fn discover_uncached(
        &self,
        channel: &str,
        target: &str,
    ) -> DiscoveryResult<Arc<Release>> {
        let name = format!("components-{channel}.json");
        let latest = if channel == "stable" {
            self.latest_available(&name, target).await?
        } else {
            None
        };
        let (release, tag, published) = match latest {
            Some((release, tag)) => (release, tag, None),
            None => {
                let (release, published) = self.discover_from_list(channel, &name, target).await?;
                let tag = release_tag(&published)?.to_owned();
                (release, tag, Some(published))
            }
        };
        // The cache attests to these exact bytes; the result always uses fresh inventory.
        let key = VerifiedRelease {
            repository: REPOSITORY.to_owned(),
            channel: channel.to_owned(),
            tag: tag.clone(),
            manifest_sha512: release.manifest_id(),
            target: target.to_owned(),
        };
        let path = {
            let _cache = self.cache_io.lock().await;
            self.verified_cache_path().await.ok()
        };
        if let Some(path) = &path
            && verified_releases(path).await.contains(&key)
        {
            return Ok(Arc::new(release));
        }

        let direct = published.is_none();
        let published = match published {
            Some(published) => published,
            None => self.release_by_tag(&tag).await?,
        };
        if release_tag(&published)? != tag || channel == "stable" && published["prerelease"] == true
        {
            return Err(error("GitHub release does not match the selected collection").into());
        }
        self.authenticate(&release, &published, &name, direct)
            .await?;
        self.verify_origins(&release, &name).await?;

        if let Some(path) = &path {
            let _cache = self.cache_io.lock().await;
            let mut entries = verified_releases(path).await;
            entries.truncate(MAX_VERIFIED_RELEASES - 1);
            entries.insert(0, key);
            let _ = app_update::atomic_json(path, &json!(entries)).await;
        }
        Ok(Arc::new(release))
    }

    async fn verified_cache_path(&self) -> Result<PathBuf, PlatformError> {
        let directory =
            component_store::canonical_path(&self.cache_root)?.join("desktop-release-check");
        app_update::private_dir(&directory).await?;
        Ok(directory.join("verified-releases-v1.json"))
    }

    async fn latest_available(
        &self,
        name: &str,
        target: &str,
    ) -> DiscoveryResult<Option<(Release, String)>> {
        let url = format!("https://github.com/{REPOSITORY}/releases/latest/download/{name}");
        let (response, tag) = match self.request(&url, Some(name)).await {
            Ok(response) => response,
            Err(DiscoveryError::Status(StatusCode::NOT_FOUND)) => return Ok(None),
            Err(error) => return Err(error),
        };
        let release = Release::parse(metadata_bytes(response).await?, "stable")?;
        let tag = tag.unwrap_or_else(|| format!("v{}", release.inventory.version));
        release.require_tag(&tag)?;
        Ok(release.has_app_target(target).then_some((release, tag)))
    }

    async fn discover_from_list(
        &self,
        channel: &str,
        name: &str,
        target: &str,
    ) -> DiscoveryResult<(Release, Value)> {
        let mut newest: Option<(Release, Value)> = None;
        for page in 1..=10 {
            let Value::Array(releases) = self
                .json(&format!(
                    "https://api.github.com/repos/{REPOSITORY}/releases?per_page=100&page={page}"
                ))
                .await?
            else {
                return Err(error("GitHub release list is invalid").into());
            };
            let page_complete = releases.len() < 100;
            for candidate in releases {
                if candidate["draft"] == true
                    || channel == "stable" && candidate["prerelease"] == true
                {
                    continue;
                }
                if !candidate["assets"]
                    .as_array()
                    .is_some_and(|assets| assets.iter().any(|a| a["name"] == name))
                {
                    continue;
                }
                let release = self.read_inventory(&candidate, channel, name).await?;
                if release.has_app_target(target)
                    && newest.as_ref().is_none_or(|best| {
                        version_key(&release.inventory.version)
                            > version_key(&best.0.inventory.version)
                    })
                {
                    newest = Some((release, candidate));
                }
            }
            if page_complete {
                break;
            }
        }
        newest.ok_or_else(|| {
            error(format!(
                "No {channel} app component inventory is published for {target}"
            ))
            .into()
        })
    }

    async fn release_by_tag(&self, tag: &str) -> DiscoveryResult<Value> {
        let encoded = url::form_urlencoded::byte_serialize(tag.as_bytes()).collect::<String>();
        self.json(&format!(
            "https://api.github.com/repos/{REPOSITORY}/releases/tags/{encoded}"
        ))
        .await
    }

    async fn read_inventory(
        &self,
        release: &Value,
        channel: &str,
        name: &str,
    ) -> DiscoveryResult<Release> {
        let tag = release_tag(release)?;
        let metadata_asset = trusted_asset(trusted_release_assets(release)?, name)?;
        let inventory = Release::parse(self.metadata(asset_url(metadata_asset)?).await?, channel)?;
        inventory.require_tag(tag)?;
        Ok(inventory)
    }

    async fn verify_origins(&self, release: &Release, name: &str) -> DiscoveryResult<()> {
        let mut origins = BTreeMap::new();
        let mut tags = BTreeSet::new();
        for target in release.inventory.targets.values() {
            for id in target.components.keys() {
                tags.insert(
                    components::component_artifact_origin(target, *id)
                        .map_err(convert)?
                        .artifact
                        .origin_tag
                        .as_str(),
                );
            }
            tags.extend(
                target
                    .packages
                    .iter()
                    .map(|p| p.artifact.origin_tag.as_str()),
            );
        }
        for origin_tag in tags {
            // Legacy unprefixed release tags are accepted for the same collection version.
            if origin_tag.strip_prefix('v').unwrap_or(origin_tag) == release.inventory.version {
                continue;
            }
            let published = self.release_by_tag(origin_tag).await?;
            if release_tag(&published)? != origin_tag {
                return Err(error("Origin release tag differs from the artifact reference").into());
            }
            let origin = self
                .read_inventory(&published, &release.inventory.channel, name)
                .await?;
            self.authenticate(&origin, &published, name, false).await?;
            origins.insert(origin_tag.to_owned(), origin.inventory);
        }
        components::validate_inventory_provenance(&release.inventory, &origins, REPOSITORY)
            .map_err(convert)?;
        Ok(())
    }

    async fn authenticate(
        &self,
        candidate: &Release,
        release: &Value,
        name: &str,
        direct: bool,
    ) -> DiscoveryResult<()> {
        let Release {
            inventory, bytes, ..
        } = candidate;
        let channel = &inventory.channel;
        let tag = release_tag(release)?;
        candidate.require_tag(tag)?;
        let assets = trusted_release_assets(release)?;
        let asset = |name: &str| trusted_asset(assets, name);
        let metadata_url = asset_url(asset(name)?)?;
        if release["target_commitish"] != inventory.source_sha
            || direct
                && release_asset_tag(&Url::parse(metadata_url).map_err(convert)?, name)? != tag
        {
            return Err(error("Collection release source/tag binding differs").into());
        }
        let provenance = self
            .json(asset_url(asset(&format!(
                "provenance-components-{channel}.json"
            ))?)?)
            .await?;
        if provenance["schemaVersion"] != 2
            || provenance["workflow"] != ".github/workflows/desktop-release.yml"
            || provenance["version"] != inventory.version
            || provenance["sourceSha"] != inventory.source_sha
            || provenance["channel"] != *channel
            || provenance["inventory"]["name"] != name
            || provenance["inventory"]["size"] != bytes.len() as u64
            || provenance["inventory"]["sha256"] != format!("{:x}", Sha256::digest(bytes))
        {
            return Err(error("Collection provenance digest or identity mismatch").into());
        }
        let run_id = provenance["runId"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| provenance["runId"].as_u64().map(|n| n.to_string()))
            .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            .ok_or_else(|| error("Invalid release workflow run ID"))?;
        let run = self
            .json(&format!(
                "https://api.github.com/repos/{REPOSITORY}/actions/runs/{run_id}"
            ))
            .await?;
        if run["path"] != ".github/workflows/desktop-release.yml"
            || run["event"] != "workflow_dispatch"
            || run["status"] != "completed"
            || run["conclusion"] != "success"
            || run["head_repository"]["full_name"] != REPOSITORY
        {
            return Err(error(
                "Collection workflow provenance is not a completed trusted release run",
            )
            .into());
        }
        let proofs = provenance["targets"]
            .as_object()
            .ok_or_else(|| error("Missing target provenance map"))?;
        for (target, digest) in proofs {
            let target_inventory = inventory
                .targets
                .get(target)
                .ok_or_else(|| error("Provenance target absent from inventory"))?;
            let target_bytes = self
                .metadata(asset_url(asset(&format!("provenance-{target}.json"))?)?)
                .await?;
            if digest.as_str() != Some(format!("{:x}", Sha256::digest(&target_bytes)).as_str()) {
                return Err(error("Target provenance digest mismatch").into());
            }
            let proof = component_store::strict_json(&target_bytes).map_err(convert)?;
            if proof["schemaVersion"] != 2
                || proof["sha"] != inventory.source_sha
                || !source_tag_matches(&proof["sourceTag"], &inventory.version)
                || proof["version"] != inventory.version
                || proof["channel"] != inventory.channel
                || proof["target"] != *target
                || proof["workflow"] != ".github/workflows/desktop-release.yml"
                || proof["components"] != json!(target_inventory.components)
                || proof["packages"] != json!(target_inventory.packages)
            {
                return Err(error("Target component provenance differs from inventory").into());
            }
            let target_run = proof["runId"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| proof["runId"].as_u64().map(|n| n.to_string()))
                .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
                .ok_or_else(|| error("Missing target workflow run"))?;
            if target_run != run_id {
                let run = self
                    .json(&format!(
                        "https://api.github.com/repos/{REPOSITORY}/actions/runs/{target_run}"
                    ))
                    .await?;
                if run["path"] != ".github/workflows/desktop-release.yml"
                    || run["event"] != "workflow_dispatch"
                    || run["status"] != "completed"
                    || run["head_repository"]["full_name"] != REPOSITORY
                {
                    return Err(error("Target provenance workflow is not trusted").into());
                }
            }
            let files = proof["files"]
                .as_array()
                .ok_or_else(|| error("Missing changed artifact provenance"))?;
            for artifact in target_inventory
                .components
                .values()
                .map(|c| &c.artifact)
                .chain(target_inventory.packages.iter().map(|p| &p.artifact))
            {
                if artifact.origin_tag != tag {
                    continue;
                }
                let filename = artifact
                    .url
                    .rsplit('/')
                    .next()
                    .ok_or_else(|| error("Missing artifact filename"))?;
                let matching: Vec<_> = files
                    .iter()
                    .filter(|file| file["name"] == filename)
                    .collect();
                if matching.len() != 1
                    || matching[0]["size"] != artifact.size
                    || matching[0]["sha256"] != artifact.sha256
                {
                    return Err(
                        error("Current-origin artifact lacks exact target provenance").into(),
                    );
                }
                let published = asset(filename)?;
                if published["browser_download_url"] != artifact.url
                    || published["size"] != artifact.size
                {
                    return Err(
                        error("Published component asset differs from original reference").into(),
                    );
                }
                if let Some(digest) = published["digest"].as_str()
                    && digest != format!("sha256:{}", artifact.sha256)
                {
                    return Err(
                        error("GitHub asset digest differs from component reference").into(),
                    );
                }
            }
        }
        for (target, entries) in &inventory.targets {
            let current = entries
                .components
                .values()
                .map(|c| &c.artifact)
                .chain(entries.packages.iter().map(|p| &p.artifact))
                .any(|artifact| artifact.origin_tag == tag);
            if current && !proofs.contains_key(target) {
                return Err(error("Current-origin target lacks build provenance").into());
            }
        }
        Ok(())
    }
    async fn request(
        &self,
        raw: &str,
        latest_asset: Option<&str>,
    ) -> DiscoveryResult<(reqwest::Response, Option<String>)> {
        let mut url = Url::parse(raw).map_err(convert)?;
        let mut tag = None;
        for hop in 0..=5 {
            if url.scheme() != "https"
                || !url.username().is_empty()
                || url.password().is_some()
                || !matches!(
                    url.host_str(),
                    Some(
                        "github.com"
                            | "api.github.com"
                            | "release-assets.githubusercontent.com"
                            | "objects.githubusercontent.com"
                    )
                )
            {
                return Err(error("Untrusted component source or redirect").into());
            }
            if let Some(name) = latest_asset
                && hop != 0
                && url.host_str() == Some("github.com")
            {
                let redirected_tag = release_asset_tag(&url, name)?;
                if tag.as_ref().is_some_and(|tag| tag != &redirected_tag) {
                    return Err(
                        error("Latest release redirects disagree on the collection tag").into(),
                    );
                }
                tag = Some(redirected_tag);
            }
            #[cfg(test)]
            let request_url = self
                .test_server
                .as_ref()
                .map(|server| {
                    let mut local = server.clone();
                    local.set_path(url.path());
                    local.set_query(url.query());
                    local
                })
                .unwrap_or_else(|| url.clone());
            #[cfg(not(test))]
            let request_url = url.clone();

            let response = tokio::time::timeout(
                Duration::from_secs(30),
                self.client
                    .get(request_url)
                    .header(
                        header::ACCEPT,
                        "application/vnd.github+json, application/octet-stream",
                    )
                    .send(),
            )
            .await
            .map_err(convert)?
            .map_err(convert)?;
            if response.status().is_redirection() {
                let location = response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| error("Missing redirect location"))?;
                url = url.join(location).map_err(convert)?;
                continue;
            }
            if !response.status().is_success() {
                if let Some(retry_at) = rate_limit_retry(&response) {
                    return Err(DiscoveryError::RateLimited(retry_at));
                }
                return Err(DiscoveryError::Status(response.status()));
            }
            return Ok((response, tag));
        }
        Err(error("Too many component source redirects").into())
    }
    async fn json(&self, url: &str) -> DiscoveryResult<Value> {
        component_store::strict_json(&self.metadata(url).await?).map_err(|e| convert(e).into())
    }
    async fn metadata(&self, url: &str) -> DiscoveryResult<Vec<u8>> {
        let (response, _) = self.request(url, None).await?;
        metadata_bytes(response).await.map_err(Into::into)
    }
    async fn download(&self, url: &str, path: &Path, size: u64) -> Result<(), PlatformError> {
        use futures_util::StreamExt as _;
        let mut sink = app_update::create_private_file(path, false).await?;
        let mut stream = self.request(url, None).await?.0.bytes_stream();
        let mut received = 0u64;
        while let Some(chunk) = tokio::time::timeout(Duration::from_secs(120), stream.next())
            .await
            .map_err(convert)?
        {
            let chunk = chunk.map_err(convert)?;
            received = received
                .checked_add(chunk.len() as u64)
                .ok_or_else(|| error("Download size overflow"))?;
            if received > size {
                return Err(error("Component exceeds published size"));
            }
            sink.write_all(&chunk).await?;
        }
        if received != size {
            return Err(error("Component download is incomplete"));
        }
        sink.sync_all().await?;
        Ok(())
    }
}

fn rate_limit_retry(response: &reqwest::Response) -> Option<DateTime<Utc>> {
    if !matches!(
        response.status(),
        StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS
    ) {
        return None;
    }
    let headers = response.headers();
    let retry_after = headers.get(header::RETRY_AFTER);
    let exhausted = headers
        .get("x-ratelimit-remaining")
        .and_then(|v| v.to_str().ok())
        == Some("0");
    if !exhausted && retry_after.is_none() {
        return None;
    }
    let now = Utc::now();
    let backoff = retry_after.and_then(|value| {
        let value = value.to_str().ok()?;
        value
            .parse::<i64>()
            .ok()
            .filter(|seconds| *seconds >= 0)
            .and_then(TimeDelta::try_seconds)
            .and_then(|delay| now.checked_add_signed(delay))
            .or_else(|| {
                DateTime::parse_from_rfc2822(value)
                    .ok()
                    .map(|time| time.with_timezone(&Utc))
            })
    });
    let reset = headers
        .get("x-ratelimit-reset")
        .filter(|_| exhausted)
        .and_then(|value| DateTime::from_timestamp(value.to_str().ok()?.parse().ok()?, 0));
    // GitHub recommends waiting at least one minute if it supplies no usable reset time.
    Some(
        backoff
            .into_iter()
            .chain(reset)
            .max()
            .unwrap_or_else(|| now + TimeDelta::minutes(1)),
    )
}

async fn metadata_bytes(response: reqwest::Response) -> Result<Vec<u8>, PlatformError> {
    use futures_util::StreamExt as _;
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = tokio::time::timeout(Duration::from_secs(30), stream.next())
        .await
        .map_err(convert)?
    {
        let chunk = chunk.map_err(convert)?;
        if bytes.len() + chunk.len() > 2 * 1024 * 1024 {
            return Err(error("Component metadata exceeds 2 MiB"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn release_tag(release: &Value) -> Result<&str, PlatformError> {
    release["tag_name"]
        .as_str()
        .ok_or_else(|| error("Missing release tag"))
}

fn trusted_publisher(value: &Value) -> bool {
    let owner = REPOSITORY.split_once('/').unwrap().0;
    value
        .as_str()
        .is_some_and(|login| login == "github-actions[bot]" || login == owner)
}

fn trusted_release_assets(release: &Value) -> Result<&[Value], PlatformError> {
    if release["draft"] == true || !trusted_publisher(&release["author"]["login"]) {
        return Err(error("Release publisher is not trusted"));
    }
    release["assets"]
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| error("Release has no assets"))
}

fn trusted_asset<'a>(assets: &'a [Value], name: &str) -> Result<&'a Value, PlatformError> {
    let mut matching = assets.iter().filter(|asset| asset["name"] == name);
    let asset = matching
        .next()
        .filter(|asset| trusted_publisher(&asset["uploader"]["login"]));
    if let Some(asset) = asset
        && matching.next().is_none()
    {
        return Ok(asset);
    }
    Err(error(
        "Release metadata asset is missing/ambiguous or untrusted",
    ))
}

fn release_asset_tag(url: &Url, name: &str) -> Result<String, PlatformError> {
    let tag = url
        .path()
        .strip_prefix('/')
        .and_then(|path| path.strip_prefix(REPOSITORY))
        .and_then(|path| path.strip_prefix("/releases/download/"))
        .and_then(|path| path.split_once('/'))
        .filter(|&(tag, asset)| !tag.is_empty() && asset == name)
        .map(|(tag, _)| tag)
        .filter(|_| url.query().is_none() && url.fragment().is_none())
        .ok_or_else(|| error("Release asset URL does not match the selected inventory"))?;
    Ok(percent_encoding::percent_decode_str(tag)
        .decode_utf8()
        .map_err(convert)?
        .into_owned())
}

fn asset_url(asset: &Value) -> Result<&str, PlatformError> {
    let raw = asset["browser_download_url"]
        .as_str()
        .ok_or_else(|| error("Missing asset URL"))?;
    let url = url::Url::parse(raw).map_err(convert)?;
    if url.scheme() != "https"
        || url.host_str() != Some("github.com")
        || !url
            .path()
            .starts_with(&format!("/{REPOSITORY}/releases/download/"))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(error("Asset URL is outside configured release repository"));
    }
    Ok(raw)
}
// Shared validation already establishes strict SemVer. Compare numeric release
// core, then prerelease precedence; collection source is not a component version.
fn version_key(value: &str) -> (u64, u64, u64, bool, Vec<VersionPart>) {
    let core = value.split('+').next().unwrap_or(value);
    let (numbers, pre) = core.split_once('-').unwrap_or((core, ""));
    let mut n = numbers.split('.').map(|n| n.parse().unwrap_or(0));
    (
        n.next().unwrap_or(0),
        n.next().unwrap_or(0),
        n.next().unwrap_or(0),
        pre.is_empty(),
        pre.split('.')
            .map(|s| {
                s.parse::<u64>()
                    .map(VersionPart::Number)
                    .unwrap_or_else(|_| VersionPart::Text(s.into()))
            })
            .collect(),
    )
}
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum VersionPart {
    Number(u64),
    Text(String),
}
async fn write_private(path: &Path, bytes: &[u8]) -> Result<(), PlatformError> {
    let mut file = app_update::create_private_file(path, false).await?;
    file.write_all(bytes).await?;
    file.sync_all().await?;
    Ok(())
}
async fn command_output(
    executable: &Path,
    args: &[&std::ffi::OsStr],
) -> Result<String, PlatformError> {
    let mut command = tokio::process::Command::new(executable);
    command
        .args(args)
        .env_remove("LD_LIBRARY_PATH")
        .env_remove("LD_PRELOAD")
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    let output = tokio::time::timeout(Duration::from_secs(30), command.output())
        .await
        .map_err(convert)??;
    if !output.status.success()
        || output.stdout.len() > 2 * 1024 * 1024
        || output.stderr.len() > 2 * 1024 * 1024
    {
        return Err(error(format!(
            "Component validation command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    String::from_utf8(output.stdout).map_err(convert)
}
async fn validate_publisher(
    current: &Path,
    image: &Path,
    id: ComponentId,
) -> Result<(), PlatformError> {
    #[cfg(windows)]
    {
        use paseo_client::windows_security::{UnsignedPolicy, verify_update_signature};
        let (current, image) = (current.to_path_buf(), image.to_path_buf());
        tokio::time::timeout(
            Duration::from_secs(30),
            tokio::task::spawn_blocking(move || {
                verify_update_signature(&current, &image, UnsignedPolicy::PreserveUnsigned)
            }),
        )
        .await
        .map_err(convert)?
        .map_err(convert)?
        .map_err(|e| error(format!("{e:#}")))?;
    }
    if cfg!(target_os = "macos") {
        let candidate = if matches!(id, ComponentId::App | ComponentId::Tailnet) {
            image
                .ancestors()
                .find(|p| p.extension().is_some_and(|e| e == "app"))
                .ok_or_else(|| error("Signed app cohort is missing"))?
        } else {
            image
        };
        command_output(
            Path::new("/usr/bin/codesign"),
            &[
                "--verify".as_ref(),
                "--deep".as_ref(),
                "--strict".as_ref(),
                candidate.as_os_str(),
            ],
        )
        .await?;
        let signing = |p: &Path| {
            let mut cmd = tokio::process::Command::new("/usr/bin/codesign");
            cmd.args(["-d", "--verbose=4"]).arg(p);
            cmd
        };
        let old = signing(current).output().await?;
        let new = signing(candidate).output().await?;
        if !old.status.success() || !new.status.success() {
            return Err(error("Cannot inspect component signing identity"));
        }
        let old = String::from_utf8_lossy(&old.stderr);
        let new = String::from_utf8_lossy(&new.stderr);
        let team = |s: &str| {
            s.lines()
                .find_map(|l| l.strip_prefix("TeamIdentifier="))
                .map(str::to_owned)
        };
        match team(&old).filter(|s| s != "not set") {
            Some(expected) => {
                if team(&new) != Some(expected) {
                    return Err(error("Component signing team mismatch"));
                }
                command_output(
                    Path::new("/usr/sbin/spctl"),
                    &[
                        "--assess".as_ref(),
                        "--type".as_ref(),
                        "execute".as_ref(),
                        candidate.as_os_str(),
                    ],
                )
                .await?;
            }
            None => {
                if !old.contains("Signature=adhoc") || !new.contains("Signature=adhoc") {
                    return Err(error("Unverifiable component signature"));
                }
            }
        }
    }
    Ok(())
}

fn source_tag_matches(value: &Value, version: &str) -> bool {
    let Some(tag) = value.as_str() else {
        return false;
    };
    let tag = tag.strip_prefix("refs/tags/").unwrap_or(tag);
    [
        "desktop-windows-v",
        "desktop-linux-v",
        "desktop-macos-v",
        "desktop-v",
        "android-v",
        "app-v",
        "v",
    ]
    .into_iter()
    .any(|prefix| tag.strip_prefix(prefix) == Some(version))
}

pub(super) async fn copy_private(
    source: &Path,
    destination: &Path,
    executable: bool,
) -> Result<(), PlatformError> {
    let mut input = tokio::fs::File::open(source).await?;
    let mut output = app_update::create_private_file(destination, executable).await?;
    tokio::io::copy(&mut input, &mut output).await?;
    output.sync_all().await?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::net::TcpListener;

    const TARGET: &str = "linux-x64";
    const WORKFLOW: &str = ".github/workflows/desktop-release.yml";
    const BUNDLE: &str = "passession.tar.gz";

    #[derive(Default)]
    struct Routes {
        responses: BTreeMap<String, (&'static str, String, Vec<u8>)>,
        requests: Vec<String>,
    }

    struct TestSite {
        url: Url,
        routes: Arc<StdMutex<Routes>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl TestSite {
        async fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
            let routes = Arc::new(StdMutex::new(Routes::default()));
            let task = tokio::spawn({
                let routes = Arc::clone(&routes);
                async move {
                    loop {
                        let (socket, _) = listener.accept().await.unwrap();
                        let mut socket = BufReader::new(socket);
                        let mut request = String::new();
                        socket.read_line(&mut request).await.unwrap();
                        let Some(path) = request.split_ascii_whitespace().nth(1).map(str::to_owned)
                        else {
                            continue;
                        };
                        loop {
                            request.clear();
                            if socket.read_line(&mut request).await.unwrap() == 0
                                || request == "\r\n"
                            {
                                break;
                            }
                        }
                        let (status, headers, bytes) = {
                            let mut routes = routes.lock().unwrap();
                            let response = routes.responses.get(&path).cloned();
                            routes.requests.push(path);
                            response.unwrap_or_else(|| ("404 Not Found", String::new(), Vec::new()))
                        };
                        let response = format!(
                            "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: {}\r\n{headers}\r\n",
                            bytes.len()
                        );
                        socket.write_all(response.as_bytes()).await.unwrap();
                        socket.write_all(&bytes).await.unwrap();
                    }
                }
            });
            Self { url, routes, task }
        }

        fn updater(&self, root: &Path) -> Updater {
            let mut updater = Updater::new("1.0.0".into());
            updater.cache_root = root.to_owned();
            updater.test_server = Some(self.url.clone());
            updater.client = reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap();
            updater
        }

        fn reply(&self, path: &str, status: &'static str, headers: String, body: Vec<u8>) {
            self.routes
                .lock()
                .unwrap()
                .responses
                .insert(path.into(), (status, headers, body));
        }

        fn json_reply(&self, path: &str, value: &Value) {
            self.reply(
                path,
                "200 OK",
                String::new(),
                serde_json::to_vec(value).unwrap(),
            );
        }

        fn publish(&self, inventory: &Value) -> Value {
            let version = inventory["version"].as_str().unwrap();
            let channel = inventory["channel"].as_str().unwrap();
            let source = &inventory["sourceSha"];
            let tag = format!("v{version}");
            let base = format!("/{REPOSITORY}/releases/download/{tag}");
            let name = format!("components-{channel}.json");
            let provenance = format!("provenance-{name}");
            let target_provenance = format!("provenance-{TARGET}.json");
            let bundle_sha = "c".repeat(64);
            let asset = |name: &str| {
                json!({
                    "name": name,
                    "browser_download_url": format!("https://github.com{base}/{name}"),
                    "uploader": {"login": "github-actions[bot]"},
                })
            };
            let mut assets = vec![asset(&name), asset(&provenance)];
            let entries = &inventory["targets"][TARGET];
            let mut proofs = json!({});
            if entries["components"]
                .as_object()
                .unwrap()
                .values()
                .any(|component| component["artifact"]["originTag"] == tag)
            {
                let proof = json!({
                    "schemaVersion": 2, "workflow": WORKFLOW, "sha": source, "sourceTag": tag,
                    "version": version, "channel": channel, "target": TARGET, "runId": 2,
                    "components": entries["components"], "packages": entries["packages"],
                    "files": [{"name": BUNDLE, "size": 123, "sha256": bundle_sha}],
                });
                proofs[TARGET] = json!(format!(
                    "{:x}",
                    Sha256::digest(serde_json::to_vec(&proof).unwrap())
                ));
                self.json_reply(&format!("{base}/{target_provenance}"), &proof);
                let mut bundle = asset(BUNDLE);
                bundle["size"] = json!(123);
                bundle["digest"] = json!(format!("sha256:{bundle_sha}"));
                assets.extend([asset(&target_provenance), bundle]);
            }
            let bytes = serde_json::to_vec(inventory).unwrap();
            self.json_reply(&format!("{base}/{provenance}"), &json!({
                "schemaVersion": 2, "workflow": WORKFLOW, "version": version, "sourceSha": source,
                "channel": channel, "runId": 1, "targets": proofs,
                "inventory": {"name": name, "size": bytes.len(), "sha256": format!("{:x}", Sha256::digest(&bytes))},
            }));
            self.reply(&format!("{base}/{name}"), "200 OK", String::new(), bytes);
            let release = json!({
                "tag_name": tag, "draft": false, "prerelease": channel == "beta",
                "author": {"login": "github-actions[bot]"}, "target_commitish": source,
                "assets": assets,
            });
            self.json_reply(
                &format!("/repos/{REPOSITORY}/releases/tags/{tag}"),
                &release,
            );
            for run in [1, 2] {
                self.json_reply(
                    &format!("/repos/{REPOSITORY}/actions/runs/{run}"),
                    &json!({
                        "path": WORKFLOW, "event": "workflow_dispatch", "status": "completed",
                        "conclusion": "success", "head_repository": {"full_name": REPOSITORY},
                    }),
                );
            }
            release
        }

        fn latest(&self, tag: &str) {
            self.reply(
                &latest_path(),
                "302 Found",
                format!(
                    "Location: /{REPOSITORY}/releases/download/{tag}/components-stable.json\r\n"
                ),
                Vec::new(),
            );
        }

        fn take_requests(&self) -> Vec<String> {
            std::mem::take(&mut self.routes.lock().unwrap().requests)
        }
    }

    impl Drop for TestSite {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    fn inventory(version: &str, channel: &str, with_app: bool) -> Value {
        let mut components = json!({});
        let source = "a".repeat(40);
        let origin = format!("v{version}");
        for id in [
            ComponentId::App,
            ComponentId::Daemon,
            ComponentId::Tailnet,
            ComponentId::UpdateHelper,
        ] {
            if id != ComponentId::Daemon && !with_app {
                continue;
            }
            components[id.as_str()] = json!({
                "id": id, "target": TARGET,
                "identity": {"version": version, "sourceSha": source, "inputHash": "b".repeat(64), "abiVersion": id.initial_abi_version()},
                "artifact": {"url": format!("https://github.com/{REPOSITORY}/releases/download/{origin}/{BUNDLE}"),
                    "sha256": "c".repeat(64), "size": 123, "format": "tar.gz", "originTag": origin},
                "executable": id.as_str(), "executableSha256": "d".repeat(64),
                "requires": if id == ComponentId::App {
                    json!({"daemon": 1, "tailnet": 1, "update-helper": 2})
                } else {
                    json!({})
                },
            });
        }
        json!({
            "schemaVersion": 1, "version": version, "sourceSha": source, "channel": channel,
            "targets": {(TARGET): {"components": components, "packages": [], "atomicGroups": []}},
        })
    }

    fn latest_path() -> String {
        format!("/{REPOSITORY}/releases/latest/download/components-stable.json")
    }

    fn list_path() -> String {
        format!("/repos/{REPOSITORY}/releases?per_page=100&page=1")
    }

    fn rest_count(requests: &[String]) -> usize {
        requests
            .iter()
            .filter(|path| path.starts_with("/repos/"))
            .count()
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_update_staging_creates_private_files_without_clobbering() {
        let temporary = tempfile::tempdir().unwrap();
        let root = component_store::canonical_path(temporary.path()).unwrap();
        let directory = root.join("operation");
        app_update::private_dir(&directory).await.unwrap();

        let manifest = inventory("2.0.0", "stable", true);
        let metadata = directory.join("inventory.json");
        write_private(&metadata, &serde_json::to_vec(&manifest).unwrap())
            .await
            .unwrap();
        assert!(write_private(&metadata, b"{}").await.is_err());
        assert_eq!(
            component_store::read_json::<Value>(&metadata, true).unwrap(),
            manifest
        );

        let site = TestSite::start().await;
        let updater = site.updater(&root);
        let published = format!("/{REPOSITORY}/releases/download/v2.0.0/passession.zip");
        let url = format!("https://github.com{published}");
        let bytes = b"component archive fixture";
        site.reply(&published, "200 OK", String::new(), bytes.to_vec());
        let archive = directory.join(format!("{:x}.zip", Sha256::digest(bytes)));
        updater
            .download(&url, &archive, bytes.len() as u64)
            .await
            .unwrap();
        assert!(updater.download(&url, &archive, 0).await.is_err());
        assert_eq!(site.take_requests(), vec![published]);
        component_store::private(&archive).unwrap();
        assert_eq!(std::fs::read(&archive).unwrap(), bytes);

        let source = root.join("installed-helper.exe");
        std::fs::write(&source, b"installed helper fixture").unwrap();
        let helper = directory.join("passession-update-helper.exe");
        copy_private(&source, &helper, true).await.unwrap();
        assert!(copy_private(&archive, &helper, true).await.is_err());
        component_store::private(&helper).unwrap();
        assert_eq!(std::fs::read(helper).unwrap(), b"installed helper fixture");
    }

    #[tokio::test]
    async fn stable_discovery_shares_checks_and_reuses_verified_bytes_across_restarts() {
        let site = TestSite::start().await;
        let root = tempfile::tempdir().unwrap();
        let mut manifest = inventory("2.0.0", "stable", true);
        site.publish(&manifest);
        site.latest("v2.0.0");

        let updater = site.updater(root.path());
        let (first, concurrent) = tokio::join!(
            updater.discover("stable", TARGET),
            updater.discover("stable", TARGET),
        );
        let first = first.unwrap();
        assert!(Arc::ptr_eq(&first, &concurrent.unwrap()));
        let requests = site.take_requests();
        let latest = latest_path();
        assert_eq!(requests.iter().filter(|path| *path == &latest).count(), 1);
        assert_eq!(rest_count(&requests), 3);
        assert!(!requests.contains(&list_path()));

        let updater = site.updater(root.path());
        let again = updater.discover("stable", TARGET).await.unwrap();
        assert_eq!(again.pin(), first.pin());
        let requests = site.take_requests();
        assert!(requests.contains(&latest_path()));
        assert_eq!(rest_count(&requests), 0);

        manifest["targets"][TARGET]["components"]["app"]["identity"]["inputHash"] =
            json!("e".repeat(64));
        site.publish(&manifest);
        let republished = updater.discover("stable", TARGET).await.unwrap();
        assert_eq!(republished.inventory.version, first.inventory.version);
        assert_ne!(republished.pin(), first.pin());
        assert_eq!(rest_count(&site.take_requests()), 3);

        let cache = updater.verified_cache_path().await.unwrap();
        tokio::fs::write(&cache, b"not JSON").await.unwrap();
        updater.discover("stable", TARGET).await.unwrap();
        assert_eq!(rest_count(&site.take_requests()), 3);
        assert_eq!(verified_releases(&cache).await.len(), 1);
    }

    #[tokio::test]
    async fn stable_carried_forward_components_require_exact_verified_origin() {
        let site = TestSite::start().await;
        let root = tempfile::tempdir().unwrap();
        let updater = site.updater(root.path());
        let mut carried = inventory("2.0.0", "stable", true);
        site.publish(&carried);
        carried["version"] = json!("3.0.0");
        carried["sourceSha"] = json!("f".repeat(40));
        site.latest("v3.0.0");

        let original = carried["targets"][TARGET]["components"]["app"].clone();
        carried["targets"][TARGET]["components"]["app"]["executableSha256"] = json!("e".repeat(64));
        site.publish(&carried);
        assert!(matches!(
            updater.discover("stable", TARGET).await,
            Err(DiscoveryError::Platform(_))
        ));
        assert_eq!(rest_count(&site.take_requests()), 5);
        assert!(updater.discover("stable", TARGET).await.is_err());
        assert_eq!(rest_count(&site.take_requests()), 5);

        carried["targets"][TARGET]["components"]["app"] = original;
        site.publish(&carried);
        updater.discover("stable", TARGET).await.unwrap();
        assert_eq!(rest_count(&site.take_requests()), 5);
        updater.discover("stable", TARGET).await.unwrap();
        assert_eq!(rest_count(&site.take_requests()), 0);
    }

    #[tokio::test]
    async fn discovery_falls_back_to_list_for_beta_and_missing_stable_app() {
        let site = TestSite::start().await;
        let root = tempfile::tempdir().unwrap();
        let updater = site.updater(root.path());
        let beta = site.publish(&inventory("4.0.0-beta.1", "beta", true));
        site.json_reply(&list_path(), &json!([beta]));
        let release = updater.discover("beta", TARGET).await.unwrap();
        assert_eq!(release.inventory.version, "4.0.0-beta.1");
        let requests = site.take_requests();
        assert_eq!(requests.first(), Some(&list_path()));
        assert_eq!(rest_count(&requests), 3);
        updater.discover("beta", TARGET).await.unwrap();
        assert_eq!(rest_count(&site.take_requests()), 1);

        let incomplete = site.publish(&inventory("3.0.0", "stable", false));
        let stable = site.publish(&inventory("2.0.0", "stable", true));
        site.latest("v3.0.0");
        site.json_reply(&list_path(), &json!([incomplete, stable]));
        let release = updater.discover("stable", TARGET).await.unwrap();
        assert_eq!(release.inventory.version, "2.0.0");
        let requests = site.take_requests();
        assert_eq!(requests.first(), Some(&latest_path()));
        assert!(requests.contains(&list_path()));
        assert_eq!(rest_count(&requests), 3);

        updater.discover("stable", TARGET).await.unwrap();
        let requests = site.take_requests();
        assert_eq!(rest_count(&requests), 1);
        assert!(requests.contains(&list_path()));
    }

    #[tokio::test]
    async fn discovery_rejects_latest_redirect_tag_mismatch_before_rest() {
        let site = TestSite::start().await;
        let root = tempfile::tempdir().unwrap();
        let updater = site.updater(root.path());
        site.latest("v9.9.9");
        site.json_reply(
            &format!("/{REPOSITORY}/releases/download/v9.9.9/components-stable.json"),
            &inventory("2.0.0", "stable", true),
        );
        assert!(matches!(
            updater.discover("stable", TARGET).await,
            Err(DiscoveryError::Platform(_))
        ));
        assert_eq!(rest_count(&site.take_requests()), 0);
    }

    #[tokio::test]
    async fn discovery_reports_typed_github_rate_limit_with_reset_time() {
        let site = TestSite::start().await;
        let root = tempfile::tempdir().unwrap();
        let updater = site.updater(root.path());
        let reset = 1_900_000_000;
        site.reply(
            &list_path(),
            "403 Forbidden",
            format!("X-RateLimit-Remaining: 0\r\nX-RateLimit-Reset: {reset}\r\n"),
            Vec::new(),
        );
        assert!(matches!(
            updater.discover("stable", TARGET).await,
            Err(DiscoveryError::RateLimited(at)) if at.timestamp() == reset
        ));
        assert_eq!(rest_count(&site.take_requests()), 1);

        let before = Utc::now();
        site.reply(
            &list_path(),
            "429 Too Many Requests",
            "Retry-After: 600\r\n".into(),
            Vec::new(),
        );
        assert!(matches!(
            updater.discover("beta", TARGET).await,
            Err(DiscoveryError::RateLimited(at))
                if at >= before + TimeDelta::minutes(10) && at <= Utc::now() + TimeDelta::minutes(10)
        ));
        site.reply(&list_path(), "403 Forbidden", String::new(), Vec::new());
        assert!(matches!(
            updater.discover("beta", TARGET).await,
            Err(DiscoveryError::Status(StatusCode::FORBIDDEN))
        ));
    }

    #[test]
    fn collection_semver_order() {
        assert!(version_key("1.2.3") > version_key("1.2.3-beta.2"));
        assert!(version_key("1.2.3-beta.10") > version_key("1.2.3-beta.2"));
        assert!(version_key("2.0.0") > version_key("1.99.99"));
    }
    #[test]
    fn reject_asset_credentials_and_wrong_repository() {
        for url in [
            "http://github.com/emotionbug/Passession/releases/download/v1/a",
            "https://evil.test/emotionbug/Passession/releases/download/v1/a",
            "https://github.com/emotionbug/Passession/releases/download/v1/a?token=secret",
        ] {
            assert!(asset_url(&json!({"browser_download_url":url})).is_err());
        }
    }
}
