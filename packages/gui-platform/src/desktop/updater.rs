//! Authenticated collection discovery and independent desktop component staging.
use super::component_store::{self, InstalledRef, Store};
use crate::action::PlatformError;
use base64::Engine as _;
use paseo_protocol::components::{self, ComponentId, ComponentRef, ReleaseInventory};
use serde_json::{Value, json};
use sha2::{Digest, Sha256, Sha512};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::io::AsyncWriteExt;
const REPOSITORY: &str = component_store::REPOSITORY;
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
struct Release {
    inventory: ReleaseInventory,
    bytes: Vec<u8>,
    manifest_sha512: String,
}
impl Release {
    fn pin(&self) -> Value {
        json!({"version":self.inventory.version,"manifestSha512":self.manifest_sha512})
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
    last_error: tokio::sync::Mutex<Option<String>>,
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
    pub async fn check_app_release(&self, channel: &str) -> Result<Value, PlatformError> {
        let result = self.fetch_app_release(channel).await;
        *self.last_error.lock().await = result.as_ref().err().map(|e| e.message.clone());
        result
    }
    async fn fetch_app_release(&self, channel: &str) -> Result<Value, PlatformError> {
        let Some(store) = Store::discover().map_err(convert)? else {
            return Ok(
                json!({"current":{"version":self.version,"sourceSha":option_env!("PASEO_BUILD_SOURCE_SHA").unwrap_or("")},"supported":false,"updateAvailable":false,"latest":null,"owner":{"kind":"desktopApp"},"supportError":"One-time packaged upgrade required: this installation has no component inventory.","checkError":null,"components":{},"componentRequirements":{},"availableComponents":[],"restartGroup":"none","atomicGroups":[]}),
            );
        };
        let installed = store.selected().map_err(convert)?;
        let current = store.inventory(&installed).map_err(convert)?;
        let release = self.discover(channel).await?;
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
        let release = self.discover(channel).await?;
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
            json!({"schemaVersion":2,"mode":"componentTransaction","operationId":operation,"operationDirectory":directory,"storeDirectory":store.directory,"baseRoot":store.base_root,"installationId":store.installation_id,"target":store.target,"inventoryPath":inventory_path,"manifestSha512":format!("{:x}",Sha512::digest(&release.bytes)),"currentGeneration":selection.generation,"current":selection.active,"targetComponents":target_components,"staged":staged,"restartGroup":plan.restart_group,"currentExecutable":component_store::canonical_path(std::env::current_exe()?).map_err(convert)?,"parentProcess":component_store::process_identity(std::process::id()).map_err(convert)?}),
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
    async fn discover(&self, channel: &str) -> Result<Release, PlatformError> {
        if !matches!(channel, "stable" | "beta") {
            return Err(error("Invalid component channel"));
        }
        components::validate_repository(REPOSITORY).map_err(convert)?;
        let name = format!("components-{channel}.json");
        let mut newest: Option<Release> = None;
        for page in 1..=10 {
            let releases = self
                .json(&format!(
                    "https://api.github.com/repos/{REPOSITORY}/releases?per_page=100&page={page}"
                ))
                .await?;
            let releases = releases
                .as_array()
                .ok_or_else(|| error("GitHub release list is invalid"))?;
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
                let release = self.authenticate(candidate, channel).await?;
                if newest.as_ref().is_none_or(|best| {
                    version_key(&release.inventory.version) > version_key(&best.inventory.version)
                }) {
                    newest = Some(release);
                }
            }
            if releases.len() < 100 {
                break;
            }
        }
        let release = newest.ok_or_else(|| {
            error(format!(
                "No authenticated {channel} component inventory is published"
            ))
        })?;
        let mut origins = BTreeMap::new();
        let mut tags = BTreeSet::new();
        for target in release.inventory.targets.values() {
            for id in target.components.keys() {
                tags.insert(
                    components::component_artifact_origin(target, *id)
                        .map_err(convert)?
                        .artifact
                        .origin_tag
                        .clone(),
                );
            }
            tags.extend(
                target
                    .packages
                    .iter()
                    .map(|package| package.artifact.origin_tag.clone()),
            );
        }
        for tag in tags {
            if tag == format!("v{}", release.inventory.version) || tag == release.inventory.version
            {
                continue;
            }
            let encoded = url::form_urlencoded::byte_serialize(tag.as_bytes()).collect::<String>();
            let origin = self
                .json(&format!(
                    "https://api.github.com/repos/{REPOSITORY}/releases/tags/{encoded}"
                ))
                .await?;
            origins.insert(tag, self.authenticate(&origin, channel).await?.inventory);
        }
        components::validate_inventory_provenance(&release.inventory, &origins, REPOSITORY)
            .map_err(convert)?;
        Ok(release)
    }
    async fn authenticate(&self, release: &Value, channel: &str) -> Result<Release, PlatformError> {
        let owner = REPOSITORY.split_once('/').unwrap().0;
        let allowed = |value: &Value| {
            value
                .as_str()
                .is_some_and(|v| v == "github-actions[bot]" || v == owner)
        };
        if release["draft"] == true || !allowed(&release["author"]["login"]) {
            return Err(error("Release publisher is not trusted"));
        }
        let assets = release["assets"]
            .as_array()
            .ok_or_else(|| error("Release has no assets"))?;
        let asset = |name: &str| -> Result<&Value, PlatformError> {
            let matches: Vec<_> = assets.iter().filter(|a| a["name"] == name).collect();
            if matches.len() != 1 || !allowed(&matches[0]["uploader"]["login"]) {
                return Err(error(
                    "Release metadata asset is missing/ambiguous or untrusted",
                ));
            }
            Ok(matches[0])
        };
        let name = format!("components-{channel}.json");
        let metadata_asset = asset(&name)?;
        let bytes = self.metadata(asset_url(metadata_asset)?).await?;
        let inventory = components::parse_inventory(&bytes, REPOSITORY).map_err(convert)?;
        if inventory.channel != channel
            || release["target_commitish"] != inventory.source_sha
            || !matches!(release["tag_name"].as_str(),Some(t) if t==inventory.version || t==format!("v{}",inventory.version))
        {
            return Err(error(
                "Collection release source/tag/channel binding differs",
            ));
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
            || provenance["channel"] != channel
            || provenance["inventory"]["name"] != name
            || provenance["inventory"]["size"] != bytes.len() as u64
            || provenance["inventory"]["sha256"] != format!("{:x}", Sha256::digest(&bytes))
        {
            return Err(error("Collection provenance digest or identity mismatch"));
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
            ));
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
                return Err(error("Target provenance digest mismatch"));
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
                return Err(error("Target component provenance differs from inventory"));
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
                    return Err(error("Target provenance workflow is not trusted"));
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
                if artifact.origin_tag != release["tag_name"].as_str().unwrap_or("") {
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
                    return Err(error(
                        "Current-origin artifact lacks exact target provenance",
                    ));
                }
                let published = asset(filename)?;
                if published["browser_download_url"] != artifact.url
                    || published["size"] != artifact.size
                {
                    return Err(error(
                        "Published component asset differs from original reference",
                    ));
                }
                if let Some(digest) = published["digest"].as_str()
                    && digest != format!("sha256:{}", artifact.sha256)
                {
                    return Err(error(
                        "GitHub asset digest differs from component reference",
                    ));
                }
            }
        }
        for (target, entries) in &inventory.targets {
            let current = entries
                .components
                .values()
                .map(|c| &c.artifact)
                .chain(entries.packages.iter().map(|p| &p.artifact))
                .any(|artifact| artifact.origin_tag == release["tag_name"].as_str().unwrap_or(""));
            if current && !proofs.contains_key(target) {
                return Err(error("Current-origin target lacks build provenance"));
            }
        }
        Ok(Release {
            manifest_sha512: base64::engine::general_purpose::STANDARD
                .encode(Sha512::digest(&bytes)),
            inventory,
            bytes,
        })
    }
    async fn request(&self, raw: &str) -> Result<reqwest::Response, PlatformError> {
        let mut url = url::Url::parse(raw).map_err(convert)?;
        for _ in 0..=5 {
            if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
                return Err(error("Component downloads require credential-free HTTPS"));
            }
            let response = tokio::time::timeout(
                Duration::from_secs(30),
                self.client
                    .get(url.clone())
                    .header(
                        reqwest::header::ACCEPT,
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
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| error("Missing redirect location"))?;
                url = url.join(location).map_err(convert)?;
                continue;
            }
            if !response.status().is_success() {
                return Err(error(format!(
                    "Component source returned HTTP {}",
                    response.status()
                )));
            }
            return Ok(response);
        }
        Err(error("Too many component source redirects"))
    }
    async fn json(&self, url: &str) -> Result<Value, PlatformError> {
        component_store::strict_json(&self.metadata(url).await?).map_err(convert)
    }
    async fn metadata(&self, url: &str) -> Result<Vec<u8>, PlatformError> {
        use futures_util::StreamExt as _;
        let mut stream = self.request(url).await?.bytes_stream();
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
    async fn download(&self, url: &str, path: &Path, size: u64) -> Result<(), PlatformError> {
        use futures_util::StreamExt as _;
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut sink = options.open(path).await?;
        let mut stream = self.request(url).await?.bytes_stream();
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
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path).await?;
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
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(if executable { 0o700 } else { 0o600 });
    #[cfg(windows)]
    let _ = executable;
    let mut output = options.open(destination).await?;
    tokio::io::copy(&mut input, &mut output).await?;
    output.sync_all().await?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
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
