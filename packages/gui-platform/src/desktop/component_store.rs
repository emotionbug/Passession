//! Per-install immutable desktop images. The helper alone mutates selection;
//! readers never interpret a corrupt selection as permission to run the base.
use anyhow::{Context, Result, bail, ensure};
use paseo_protocol::components::{
    self, ComponentId, ComponentIdentity, ComponentRef, PackageRef, TargetInventory,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256, Sha512};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
};

pub(super) const REPOSITORY: &str = match option_env!("PASEO_UPDATE_REPOSITORY") {
    Some(v) => v,
    None => "emotionbug/Passession",
};
const MAX_JSON: u64 = 2 * 1024 * 1024;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct InstalledRef {
    pub component: ComponentRef,
    pub root: String,
}
pub(super) type Installed = BTreeMap<ComponentId, InstalledRef>;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Trial {
    pub operation_id: String,
    pub components: Installed,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Selection {
    pub schema_version: u32,
    pub installation_id: String,
    pub base_root: PathBuf,
    pub target: String,
    pub generation: u64,
    pub active: Installed,
    /// Wire compatibility only: the helper commits null and clears a value left
    /// by an older helper. It is never run, restored or validated.
    pub previous: Option<Installed>,
    pub trial: Option<Trial>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Bootstrap {
    schema_version: u32,
    target: String,
    components: BTreeMap<ComponentId, ComponentRef>,
    roots: BTreeMap<ComponentId, String>,
    packages: Vec<PackageRef>,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Receipt {
    schema_version: u32,
    components: BTreeMap<ComponentId, ComponentRef>,
    packages: Vec<PackageRef>,
    tree_sha512: String,
}
#[derive(Clone, Debug)]
pub(super) struct Store {
    pub directory: PathBuf,
    pub base_root: PathBuf,
    pub installation_id: String,
    pub target: String,
}

pub(super) fn target() -> Result<String> {
    let os = if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    };
    let arch = if cfg!(target_arch = "x86_64") {
        "x64"
    } else if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        bail!("Unsupported desktop component architecture")
    };
    Ok(format!("{os}-{arch}"))
}
pub(super) fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 128 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
fn canonical(path: &Path) -> Result<()> {
    ensure!(
        path.is_absolute() && same_path(&canonical_path(path)?, path),
        "Noncanonical component path: {}",
        path.display()
    );
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        !metadata.file_type().is_symlink(),
        "Component path is a symlink"
    );
    Ok(())
}
pub(super) fn private(path: &Path) -> Result<()> {
    canonical(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::symlink_metadata(path)?;
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o077 == 0,
            "Component store is not private to this user"
        );
    }
    #[cfg(windows)]
    {
        use paseo_client::windows_security::{AllowedDirectoryUsers, verify_private_directory};
        verify_private_directory(path, AllowedDirectoryUsers::CurrentUserAndSystem)
            .context("Unsafe component store ACL")?;
    }
    Ok(())
}
pub(super) fn read_json<T: serde::de::DeserializeOwned>(
    path: &Path,
    is_private: bool,
) -> Result<T> {
    canonical(path)?;
    if is_private {
        private(path)?;
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    ensure!(
        file.metadata()?.is_file() && file.metadata()?.len() <= MAX_JSON,
        "Invalid component metadata size/type"
    );
    let mut bytes = Vec::new();
    file.take(MAX_JSON + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_JSON,
        "Component metadata grew beyond limit"
    );
    Ok(serde_json::from_value(strict_json(&bytes)?)?)
}
fn base_directory(executable: &Path) -> Result<PathBuf> {
    if cfg!(target_os = "macos") {
        Ok(executable
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .and_then(Path::parent)
            .context("Missing signed application root")?
            .to_owned())
    } else {
        Ok(executable
            .parent()
            .context("Missing application root")?
            .to_owned())
    }
}
impl Store {
    pub fn discover() -> Result<Option<Self>> {
        let executable = canonical_path(std::env::current_exe()?)?;
        let base_root = match std::env::var_os("PASEO_COMPONENT_BASE_ROOT") {
            Some(root) => {
                let root = normalize_path(Path::new(&root));
                canonical(&root)?;
                root
            }
            None => match appimage_payload(&executable)? {
                Some((image, _)) => image,
                None => selected_object_base(&executable)?.unwrap_or(base_directory(&executable)?),
            },
        };
        let raw = base_root
            .to_str()
            .context("Installation path must be UTF-8")?;
        let normalized = if cfg!(windows) {
            raw.to_lowercase()
        } else {
            raw.to_owned()
        };
        let installation_id = format!("{:x}", Sha256::digest(normalized.as_bytes()));
        let directory = crate::paths::app_paths()
            .support_dir
            .join("desktop-components")
            .join(&installation_id);
        if let Some(hint) = std::env::var_os("PASEO_COMPONENT_STORE") {
            ensure!(
                Path::new(&hint) == directory,
                "Component store environment does not match original installation"
            );
        }
        let store = Self {
            directory,
            base_root,
            installation_id,
            target: target()?,
        };
        if entry_exists(&store.directory)? {
            private(&store.directory)?;
        }
        if !entry_exists(&store.directory.join("selection.json"))?
            && entry_exists(&store.directory.join("established"))?
        {
            bail!(
                "Established component installation has lost its selection; refusing stale bootstrap fallback"
            );
        }
        if store.base_root.is_file() && !entry_exists(&store.directory.join("selection.json"))? {
            let (_, payload) =
                appimage_payload(&executable)?.context("AppImage bootstrap binding is missing")?;
            store.import_appimage(&payload)?;
        }
        if !entry_exists(&store.directory.join("selection.json"))?
            && !entry_exists(&store.base_root.join("components.json"))?
        {
            ensure!(
                std::env::var_os("PASEO_COMPONENT_BASE_ROOT").is_none(),
                "Bound component installation is missing its selection"
            );
            return Ok(None);
        }
        let selection = store.selection()?;
        let selected = store.runnable(&selection)?;
        let selected_app = store.image(&selected[&ComponentId::App])?;
        let base_app = store
            .bootstrap()
            .ok()
            .and_then(|s| store.image(&s[&ComponentId::App]).ok());
        let mounted = appimage_payload(&executable)?
            .is_some_and(|(image, _)| image == store.base_root)
            && sha256_file(&executable)? == selected[&ComponentId::App].component.executable_sha256;
        ensure!(
            executable == selected_app || base_app.as_ref() == Some(&executable) || mounted,
            "Running GUI does not belong to this installation"
        );
        Ok(Some(store))
    }
    fn bootstrap(&self) -> Result<Installed> {
        let bootstrap: Bootstrap = read_json(&self.base_root.join("components.json"), false)?;
        ensure!(
            bootstrap.schema_version == 1 && bootstrap.target == self.target,
            "Bootstrap component target mismatch"
        );
        ensure!(
            bootstrap.roots.len() == bootstrap.components.len(),
            "Incomplete bootstrap root map"
        );
        let mut installed = BTreeMap::new();
        for (id, component) in bootstrap.components {
            let root = bootstrap
                .roots
                .get(&id)
                .context("Missing bootstrap component root")?;
            let root = if root == "." {
                "base".to_owned()
            } else {
                components::validate_relative_image(root)?;
                format!("base/{root}")
            };
            installed.insert(id, InstalledRef { component, root });
        }
        self.validate_map(&installed)?;
        Ok(installed)
    }
    pub fn selection(&self) -> Result<Selection> {
        let path = self.directory.join("selection.json");
        if !entry_exists(&path)? {
            ensure!(
                !entry_exists(&self.directory.join("established"))?,
                "Established component selection is missing"
            );
            return Ok(Selection {
                schema_version: 1,
                installation_id: self.installation_id.clone(),
                base_root: self.base_root.clone(),
                target: self.target.clone(),
                generation: 0,
                active: self.bootstrap()?,
                previous: None,
                trial: None,
            });
        }
        private(&self.directory)?;
        let selection: Selection = read_json(&path, true)?;
        ensure!(
            selection.schema_version == 1
                && selection.installation_id == self.installation_id
                && selection.base_root == self.base_root
                && selection.target == self.target,
            "Component selection installation binding mismatch"
        );
        self.validate_map(&selection.active)?;
        // Orphan trials are ignored before touching their possibly interrupted
        // objects. Corrupt active state is never permission to run stale base.
        Ok(selection)
    }
    pub fn runnable(&self, selection: &Selection) -> Result<Installed> {
        if let Some(trial) = &selection.trial
            && self.trial_authorized(selection, trial)?
        {
            self.validate_map(&trial.components)?;
            return Ok(trial.components.clone());
        }
        Ok(selection.active.clone())
    }
    fn trial_authorized(&self, selection: &Selection, trial: &Trial) -> Result<bool> {
        let lease_path = self.directory.join("lease");
        private(&lease_path)?;
        let lease = OpenOptions::new().read(true).write(true).open(lease_path)?;
        match lease.try_lock() {
            Ok(()) => {
                lease.unlock()?;
                return Ok(false);
            }
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
        }
        let owner: serde_json::Value = read_json(&self.directory.join("lease-owner.json"), true)?;
        ensure!(
            owner["schemaVersion"] == 1
                && owner["operationId"] == trial.operation_id
                && owner["generation"] == selection.generation,
            "Invalid component trial lease owner"
        );
        let pid = owner["helperProcess"]["pid"]
            .as_u64()
            .and_then(|p| u32::try_from(p).ok())
            .context("Missing trial helper PID")?;
        if process_identity(pid).ok().as_ref() != Some(&owner["helperProcess"]) {
            return Ok(false);
        }
        let operation = PathBuf::from(
            owner["operationDirectory"]
                .as_str()
                .context("Missing trial operation directory")?,
        );
        private(&operation)?;
        let status: serde_json::Value = read_json(&operation.join("status.json"), true)?;
        ensure!(
            status["schemaVersion"] == 2
                && status["operationId"] == trial.operation_id
                && status["nonce"] == owner["nonce"]
                && status["helperProcess"] == owner["helperProcess"],
            "Component trial transaction identity mismatch"
        );
        if !matches!(status["phase"].as_str(), Some("starting" | "checking")) {
            return Ok(false);
        }
        let request_path = operation.join("request.json");
        let request: serde_json::Value = read_json(&request_path, true)?;
        ensure!(
            request["schemaVersion"] == 2
                && request["mode"] == "componentTransaction"
                && request["operationId"] == trial.operation_id
                && request["installationId"] == self.installation_id
                && request["baseRoot"] == serde_json::to_value(&self.base_root)?
                && request["storeDirectory"] == serde_json::to_value(&self.directory)?
                && request["target"] == self.target
                && request["currentGeneration"] == selection.generation
                && request["current"] == serde_json::to_value(&selection.active)?
                && request["targetComponents"] == serde_json::to_value(&trial.components)?
                && status["expectedComponents"]
                    == serde_json::to_value(identities(&trial.components))?,
            "Component trial differs from its authenticated request"
        );
        let journal: serde_json::Value = read_json(&operation.join("journal.json"), true)?;
        let raw = std::fs::read(&request_path)?;
        ensure!(
            raw.len() <= 64 * 1024
                && journal["schemaVersion"] == 2
                && journal["requestDigest"] == format!("{:x}", Sha512::digest(&raw))
                && journal["original"]["active"] == serde_json::to_value(&selection.active)?
                && journal["original"]["generation"] == selection.generation
                && journal["status"]["operationId"] == trial.operation_id
                && journal["status"]["helperProcess"] == owner["helperProcess"]
                && journal["status"]["nonce"] == owner["nonce"],
            "Component trial journal authority differs"
        );
        let helper = operation.join(if cfg!(windows) {
            "passession-update-helper.exe"
        } else {
            "passession-update-helper"
        });
        private(&helper)?;
        ensure!(
            same_path(&process_image(pid)?, &helper)
                && sha256_file(&helper)?
                    == selection.active[&ComponentId::UpdateHelper]
                        .component
                        .executable_sha256,
            "The trial lease holder is not the authenticated copied update helper"
        );
        Ok(true)
    }
    pub fn selected(&self) -> Result<Installed> {
        let selection = self.selection()?;
        self.runnable(&selection)
    }
    pub fn inventory(&self, installed: &Installed) -> Result<TargetInventory> {
        let mut packages = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for reference in installed.values() {
            if !seen.insert(reference.component.artifact.sha256.clone()) {
                continue;
            }
            let candidates = if reference.root.starts_with("base") {
                let bootstrap: Bootstrap =
                    read_json(&self.base_root.join("components.json"), false)?;
                bootstrap.packages
            } else {
                let receipt: Receipt = read_json(
                    &self
                        .directory
                        .join("receipts")
                        .join(format!("{}.json", reference.component.artifact.sha256)),
                    true,
                )?;
                receipt.packages
            };
            for package in candidates {
                if package.artifact == reference.component.artifact && !packages.contains(&package)
                {
                    packages.push(package);
                }
            }
        }
        Ok(TargetInventory {
            components: installed
                .iter()
                .map(|(id, r)| (*id, r.component.clone()))
                .collect(),
            packages,
            atomic_groups: components::required_atomic_groups(&self.target)?,
        })
    }
    pub fn image(&self, reference: &InstalledRef) -> Result<PathBuf> {
        components::validate_component(&reference.component, &self.target, REPOSITORY)?;
        let root = if reference.root == "base" {
            self.base_root.clone()
        } else if let Some(relative) = reference.root.strip_prefix("base/") {
            components::validate_relative_image(relative)?;
            self.base_root.join(relative)
        } else {
            ensure!(
                reference.root == format!("objects/{}", reference.component.artifact.sha256),
                "Invalid immutable object root"
            );
            self.directory.join(&reference.root)
        };
        canonical(&root)?;
        if reference.root.starts_with("objects/") {
            private(&self.directory)?;
            let digest = &reference.component.artifact.sha256;
            let receipt: Receipt = read_json(
                &self
                    .directory
                    .join("receipts")
                    .join(format!("{digest}.json")),
                true,
            )?;
            ensure!(
                receipt.schema_version == 1
                    && receipt.components.get(&reference.component.id)
                        == Some(&reference.component),
                "Immutable component receipt differs from selection"
            );
            let archive = self.directory.join("archives").join(digest);
            private(&archive)?;
            ensure!(
                archive.metadata()?.is_file() && sha256_file(&archive)? == *digest,
                "Retained component archive digest mismatch"
            );
            ensure!(
                tree_digest(&root)? == receipt.tree_sha512,
                "Immutable component resources/dependencies changed"
            );
        } else {
            let bootstrap: Bootstrap = read_json(&self.base_root.join("components.json"), false)?;
            let initial_root = bootstrap
                .roots
                .get(&reference.component.id)
                .context("Missing bootstrap component root")?;
            let expected_root = if initial_root == "." {
                "base".to_owned()
            } else {
                format!("base/{initial_root}")
            };
            ensure!(
                bootstrap.components.get(&reference.component.id) == Some(&reference.component)
                    && reference.root == expected_root,
                "Bootstrap reference differs from installed inventory"
            );
        }
        let image = root.join(&reference.component.executable);
        canonical(&image)?;
        ensure!(
            image.is_file() && sha256_file(&image)? == reference.component.executable_sha256,
            "Selected {} image hash mismatch",
            reference.component.id
        );
        Ok(image)
    }
    fn validate_map(&self, installed: &Installed) -> Result<()> {
        ensure!(
            installed.len() == 4,
            "Incomplete desktop component selection"
        );
        components::validate_target_inventory(
            &self.inventory(installed)?,
            &self.target,
            REPOSITORY,
        )?;
        for (id, reference) in installed {
            ensure!(
                *id == reference.component.id,
                "Component identity map mismatch"
            );
            self.image(reference)?;
        }
        Ok(())
    }
    pub async fn ensure_directories(&self) -> Result<()> {
        let support = &crate::paths::app_paths().support_dir;
        std::fs::create_dir_all(support)?;
        let parent = support.join("desktop-components");
        super::app_update::private_dir(&parent)
            .await
            .map_err(|e| anyhow::anyhow!(e.message))?;
        super::app_update::private_dir(&self.directory)
            .await
            .map_err(|e| anyhow::anyhow!(e.message))?;
        Ok(())
    }

    fn import_appimage(&self, payload: &Path) -> Result<()> {
        let bootstrap: Bootstrap = read_json(&payload.join("components.json"), false)?;
        ensure!(
            bootstrap.schema_version == 1 && bootstrap.target == self.target,
            "AppImage bootstrap identity mismatch"
        );
        let inventory = TargetInventory {
            components: bootstrap.components.clone(),
            packages: bootstrap.packages.clone(),
            atomic_groups: components::required_atomic_groups(&self.target)?,
        };
        components::validate_target_inventory(&inventory, &self.target, REPOSITORY)?;
        let parent = self.directory.parent().context("Missing store parent")?;
        std::fs::create_dir_all(parent.parent().context("Missing support directory")?)?;
        create_private(parent)?;
        create_private(&self.directory)?;
        let lease = open_private(&self.directory.join("lease"), false)?;
        lease.lock()?;
        if entry_exists(&self.directory.join("selection.json"))? {
            return Ok(());
        }
        ensure!(
            !entry_exists(&self.directory.join("established"))?,
            "Interrupted component bootstrap requires recovery"
        );
        for name in ["archives", "receipts", "objects"] {
            create_private(&self.directory.join(name))?;
        }
        let mut installed = Installed::new();
        let mut seen = std::collections::BTreeSet::new();
        for (id, component) in &bootstrap.components {
            let digest = &component.artifact.sha256;
            if seen.insert(digest) {
                let source = payload
                    .join("component-bootstrap")
                    .join(format!("{digest}.{}", component.artifact.format));
                canonical(&source)?;
                ensure!(
                    source.metadata()?.len() == component.artifact.size
                        && sha256_file(&source)? == *digest,
                    "AppImage embedded component archive digest differs"
                );
                let object = self.directory.join("objects").join(digest);
                let archive = self.directory.join("archives").join(digest);
                let receipt_path = self
                    .directory
                    .join("receipts")
                    .join(format!("{digest}.json"));
                if !entry_exists(&object)? {
                    let temporary = self
                        .directory
                        .join("objects")
                        .join(format!(".import-{}", uuid::Uuid::new_v4()));
                    extract(&source, &component.artifact.format, &temporary)?;
                    #[cfg(unix)]
                    sync_tree(&temporary)?;
                    std::fs::rename(&temporary, &object)?;
                    #[cfg(unix)]
                    File::open(self.directory.join("objects"))?.sync_all()?;
                    let mut output = open_private(&archive, true)?;
                    std::io::copy(&mut File::open(&source)?, &mut output)?;
                    output.sync_all()?;
                    let receipt = Receipt {
                        schema_version: 1,
                        components: bootstrap
                            .components
                            .iter()
                            .filter(|(_, c)| c.artifact.sha256 == *digest)
                            .map(|(id, c)| (*id, c.clone()))
                            .collect(),
                        packages: bootstrap
                            .packages
                            .iter()
                            .filter(|p| p.artifact.sha256 == *digest)
                            .cloned()
                            .collect(),
                        tree_sha512: tree_digest(&object)?,
                    };
                    atomic_sync(&receipt_path, &receipt)?;
                }
            }
            installed.insert(
                *id,
                InstalledRef {
                    component: component.clone(),
                    root: format!("objects/{digest}"),
                },
            );
        }
        self.validate_map(&installed)?;
        let selection = Selection {
            schema_version: 1,
            installation_id: self.installation_id.clone(),
            base_root: self.base_root.clone(),
            target: self.target.clone(),
            generation: 0,
            active: installed,
            previous: None,
            trial: None,
        };
        atomic_sync(
            &self.directory.join("established"),
            &serde_json::json!({"schemaVersion":1,"installationId":self.installation_id}),
        )?;
        atomic_sync(&self.directory.join("selection.json"), &selection)?;
        Ok(())
    }
}
pub(super) fn identities(installed: &Installed) -> BTreeMap<ComponentId, ComponentIdentity> {
    installed
        .iter()
        .map(|(id, r)| (*id, r.component.identity.clone()))
        .collect()
}

pub(super) fn app_identities(installed: &Installed) -> BTreeMap<ComponentId, ComponentIdentity> {
    installed
        .iter()
        .filter(|(id, _)| **id != ComponentId::Daemon)
        .map(|(id, r)| (*id, r.component.identity.clone()))
        .collect()
}

pub(super) fn process_identity(pid: u32) -> Result<serde_json::Value> {
    #[cfg(target_os = "linux")]
    let birth = {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
        stat.rsplit_once(')')
            .and_then(|(_, s)| s.split_whitespace().nth(19))
            .context("Missing process birth token")?
            .to_owned()
    };
    #[cfg(target_os = "macos")]
    let birth = {
        let output = Command::new("/bin/ps")
            .env("LC_ALL", "C")
            .args(["-p", &pid.to_string(), "-o", "lstart="])
            .output()?;
        ensure!(output.status.success(), "Cannot identify process");
        String::from_utf8(output.stdout)?.trim().to_owned()
    };
    #[cfg(windows)]
    let birth = unsafe {
        use windows::Win32::{
            Foundation::{CloseHandle, FILETIME},
            System::Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
        };
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)?;
        let (mut c, mut e, mut k, mut u) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        let result = GetProcessTimes(handle, &mut c, &mut e, &mut k, &mut u);
        let _ = CloseHandle(handle);
        result?;
        (((c.dwHighDateTime as u64) << 32) | c.dwLowDateTime as u64).to_string()
    };
    ensure!(!birth.is_empty(), "Empty process birth token");
    Ok(serde_json::json!({"pid":pid,"birthToken":birth}))
}

/// Before any threads or library load. Environment is only a binding hint;
/// selection, root, image digest and lease are all independently checked.
pub fn bootstrap_process(argv: &[String]) -> Result<()> {
    if argv.get(1).map(String::as_str) == Some("--component-identity") {
        println!(
            "{}",
            serde_json::json!({"schemaVersion":1,"id":"app","identity":app_identity()})
        );
        std::process::exit(0);
    }
    if argv.get(1).map(String::as_str) == Some("--component-tailnet-probe") {
        ensure!(argv.len() == 3, "Invalid component probe arguments");
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Probe {
            component: ComponentRef,
            archive: PathBuf,
            root: PathBuf,
        }
        let path = PathBuf::from(&argv[2]);
        let probe: Probe = read_json(&path, true)?;
        let directory = path.parent().context("Missing probe directory")?;
        private(directory)?;
        components::validate_component(&probe.component, &target()?, REPOSITORY)?;
        ensure!(
            probe.component.id == ComponentId::Tailnet
                && probe.archive.parent() == Some(directory)
                && probe.root.parent() == Some(directory),
            "Invalid tailnet probe scope"
        );
        ensure!(
            sha256_file(&probe.archive)? == probe.component.artifact.sha256,
            "Probe archive identity mismatch"
        );
        let image = probe.root.join(&probe.component.executable);
        canonical(&image)?;
        ensure!(
            sha256_file(&image)? == probe.component.executable_sha256,
            "Probe image identity mismatch"
        );
        unsafe {
            paseo_tailnet::select_validated_library(image)?;
        }
        ensure!(
            paseo_tailnet::native_identity()? == serde_json::to_value(&probe.component.identity)?,
            "Native tailnet identity/ABI differs from descriptor"
        );
        std::process::exit(0);
    }
    let Some(store) = Store::discover()? else {
        return Ok(());
    };
    let selected = store.selected()?;
    let app = store.image(&selected[&ComponentId::App])?;
    let current = canonical_path(std::env::current_exe()?)?;
    if app != current {
        ensure!(
            std::env::var_os("PASEO_COMPONENT_REEXEC").is_none(),
            "Component launcher re-exec loop refused"
        );
        let mut command = Command::new(&app);
        command
            .args(argv.iter().skip(1))
            .env("PASEO_COMPONENT_STORE", &store.directory)
            .env("PASEO_COMPONENT_BASE_ROOT", &store.base_root)
            .env("PASEO_COMPONENT_REEXEC", "1")
            .env_remove("LD_LIBRARY_PATH")
            .env_remove("LD_PRELOAD")
            .env_remove("APPIMAGE")
            .env_remove("APPDIR");
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            return Err(command.exec().into());
        }
        #[cfg(windows)]
        {
            command.spawn()?;
            std::process::exit(0);
        }
    }
    let tailnet = &selected[&ComponentId::Tailnet];
    ensure!(
        tailnet.component.identity.abi_version == 1,
        "Unsupported selected tailnet ABI"
    );
    let library = store.image(tailnet)?;
    // SAFETY: store binding, immutable image digest, ABI declaration and path were
    // checked above, and this entry runs before any threads/native initialization.
    unsafe {
        paseo_tailnet::select_validated_library(library)?;
    }
    ensure!(
        app_identity() == serde_json::to_value(&selected[&ComponentId::App].component.identity)?,
        "Running GUI compiled identity differs from selection"
    );
    ensure!(
        paseo_tailnet::native_identity()? == serde_json::to_value(&tailnet.component.identity)?,
        "Loaded tailnet compiled identity/ABI differs from selection"
    );
    Ok(())
}

/// Extract into a fresh private validation directory, never into live objects.
pub(super) fn extract(archive: &Path, format: &str, destination: &Path) -> Result<()> {
    ensure!(
        !entry_exists(destination)?,
        "Component extraction destination already exists"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(destination)?;
    }
    #[cfg(windows)]
    std::fs::create_dir(destination)?;
    let mut total = 0u64;
    let mut links: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut members = 0usize;
    match format {
        "tar.gz" => {
            let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(File::open(archive)?));
            for entry in tar.entries()? {
                let mut entry = entry?;
                let relative = entry.path()?.into_owned();
                safe_member(&relative)?;
                members += 1;
                ensure!(members <= 100_000, "Component archive has too many members");
                if entry.header().entry_type().is_symlink() {
                    let target = entry
                        .link_name()?
                        .context("Missing component link target")?
                        .into_owned();
                    links.push((destination.join(relative), target));
                    continue;
                }
                let kind = entry.header().entry_type();
                ensure!(
                    kind.is_file() || kind.is_dir(),
                    "Links/devices are not admitted in component archives"
                );
                total = total
                    .checked_add(entry.size())
                    .context("Component archive size overflow")?;
                ensure!(
                    total <= 16 * 1024 * 1024 * 1024,
                    "Expanded component exceeds limit"
                );
                let output = destination.join(relative);
                if kind.is_dir() {
                    std::fs::create_dir_all(&output)?;
                } else {
                    std::fs::create_dir_all(output.parent().context("Missing member parent")?)?;
                    let mut file = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&output)?;
                    std::io::copy(&mut entry, &mut file)?;
                    file.sync_all()?;
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let mode = entry.header().mode()?;
                        std::fs::set_permissions(
                            &output,
                            std::fs::Permissions::from_mode(if mode & 0o111 != 0 {
                                0o700
                            } else {
                                0o600
                            }),
                        )?;
                    }
                }
            }
        }
        "zip" => {
            let mut zip = zip::ZipArchive::new(File::open(archive)?)?;
            ensure!(
                zip.len() <= 100_000,
                "Component archive has too many members"
            );
            for n in 0..zip.len() {
                let mut entry = zip.by_index(n)?;
                let relative = entry.enclosed_name().context("Unsafe ZIP path")?;
                safe_member(&relative)?;
                let mode = entry.unix_mode().unwrap_or(0);
                ensure!(
                    mode & 0o170000 == 0
                        || matches!(mode & 0o170000, 0o100000 | 0o040000 | 0o120000),
                    "Special ZIP member rejected"
                );
                if mode & 0o170000 == 0o120000 {
                    ensure!(entry.size() <= 4096, "Component link target exceeds limit");
                    let mut target = String::new();
                    entry.read_to_string(&mut target)?;
                    links.push((destination.join(relative), PathBuf::from(target)));
                    continue;
                }
                total = total
                    .checked_add(entry.size())
                    .context("Component archive size overflow")?;
                ensure!(
                    total <= 16 * 1024 * 1024 * 1024,
                    "Expanded component exceeds limit"
                );
                let output = destination.join(relative);
                if entry.is_dir() {
                    std::fs::create_dir_all(&output)?;
                } else {
                    std::fs::create_dir_all(output.parent().context("Missing member parent")?)?;
                    let mut file = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&output)?;
                    std::io::copy(&mut entry, &mut file)?;
                    file.sync_all()?;
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        std::fs::set_permissions(
                            &output,
                            std::fs::Permissions::from_mode(if mode & 0o111 != 0 {
                                0o700
                            } else {
                                0o600
                            }),
                        )?;
                    }
                }
            }
        }
        _ => bail!("Unsupported component archive format"),
    }
    for (path, target) in &links {
        ensure!(
            target.is_relative() && !target.to_string_lossy().contains('\\'),
            "Absolute component link rejected"
        );
        let mut depth = path
            .parent()
            .context("Missing link parent")?
            .strip_prefix(destination)?
            .components()
            .count();
        for part in target.components() {
            match part {
                std::path::Component::Normal(_) => depth += 1,
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    ensure!(depth > 0, "Component link escapes root");
                    depth -= 1;
                }
                _ => bail!("Unsafe component link"),
            }
        }
        std::fs::create_dir_all(path.parent().context("Missing link parent")?)?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, path)?;
        #[cfg(windows)]
        bail!("Windows component symlinks are not supported");
    }
    for (path, _) in links {
        ensure!(
            canonical_path(&path)?.starts_with(destination),
            "Component link resolves outside object"
        );
    }
    Ok(())
}
fn safe_member(path: &Path) -> Result<()> {
    ensure!(
        !path.as_os_str().is_empty()
            && path
                .components()
                .all(|c| matches!(c, std::path::Component::Normal(_)))
            && !path.to_string_lossy().contains('\\'),
        "Unsafe component archive path"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_traversal_and_absolute_members() {
        for path in ["../outside", "/absolute", "a/../b", "a\\b"] {
            assert!(safe_member(Path::new(path)).is_err());
        }
        assert!(safe_member(Path::new("resources/fonts/font.ttf")).is_ok());
    }
    #[test]
    fn archive_duplicate_does_not_overwrite() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("a.tar.gz");
        let file = File::create(&archive).unwrap();
        let gz = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut tar = tar::Builder::new(gz);
        for data in [b"old", b"new"] {
            let mut h = tar::Header::new_gnu();
            h.set_size(3);
            h.set_mode(0o755);
            h.set_cksum();
            tar.append_data(&mut h, "image", &data[..]).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap();
        let output = temp.path().join("unpacked");
        assert!(extract(&archive, "tar.gz", &output).is_err());
        assert_eq!(std::fs::read(output.join("image")).unwrap(), b"old");
    }
    #[test]
    fn json_limit_and_symlink_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("metadata.json");
        std::fs::write(&file, b"{}").unwrap();
        assert!(read_json::<serde_json::Value>(&file, false).is_ok());
        #[cfg(unix)]
        {
            let link = temp.path().join("link");
            std::os::unix::fs::symlink(&file, &link).unwrap();
            assert!(read_json::<serde_json::Value>(&link, false).is_err());
        }
        File::create(&file).unwrap().set_len(MAX_JSON + 1).unwrap();
        assert!(read_json::<serde_json::Value>(&file, false).is_err());
    }
}

pub(super) fn app_identity() -> serde_json::Value {
    serde_json::json!({"version":option_env!("PASEO_VERSION").unwrap_or(env!("CARGO_PKG_VERSION")),
        "sourceSha":option_env!("PASEO_BUILD_SOURCE_SHA").unwrap_or(""),
        "inputHash":option_env!("PASEO_COMPONENT_INPUT_HASH").unwrap_or(""),"abiVersion":1})
}

fn appimage_payload(executable: &Path) -> Result<Option<(PathBuf, PathBuf)>> {
    if !cfg!(target_os = "linux") {
        return Ok(None);
    }
    let Some(image) = std::env::var_os("APPIMAGE") else {
        return Ok(None);
    };
    let image = std::fs::canonicalize(image)?;
    ensure!(
        image.is_file(),
        "AppImage installation is not a regular file"
    );
    let mount = std::fs::canonicalize(
        std::env::var_os("APPDIR").context("Missing AppImage mount binding")?,
    )?;
    let payload = mount.join("usr/lib/paseo-sessions");
    ensure!(
        executable.parent() == Some(payload.as_path()),
        "AppImage executable/mount binding differs"
    );
    let mut header = [0u8; 11];
    File::open(&image)?.read_exact(&mut header)?;
    ensure!(
        &header[..4] == b"\x7fELF" && &header[8..11] == b"AI\x02",
        "Original installation is not a type-2 AppImage"
    );
    Ok(Some((image, payload)))
}

fn open_private(path: &Path, create_new: bool) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    if create_new {
        options.create_new(true);
    } else {
        options.create(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    private(path)?;
    Ok(file)
}
fn create_private(path: &Path) -> Result<()> {
    if !entry_exists(path)? {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new().mode(0o700).create(path)?;
        }
        #[cfg(windows)]
        {
            std::fs::create_dir(path)?;
            paseo_client::windows_security::restrict_directory_to_current_user(path)
                .context("Cannot protect component directory")?;
        }
    }
    private(path)
}
fn atomic_sync(path: &Path, value: &impl Serialize) -> Result<()> {
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let mut file = open_private(&temporary, true)?;
    serde_json::to_writer(&mut file, value)?;
    file.flush()?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&temporary, path)?;
    #[cfg(unix)]
    File::open(path.parent().context("Missing metadata directory")?)?.sync_all()?;
    Ok(())
}

/// Cross-language receipt digest; see component-helper-api.md. Links are only
/// followed for containment validation, never for entry enumeration or hashing.
fn tree_digest(root: &Path) -> Result<String> {
    fn walk(root: &Path, at: &Path, entries: &mut Vec<(String, PathBuf)>) -> Result<()> {
        for entry in std::fs::read_dir(at)? {
            let entry = entry?;
            let path = entry.path();
            let relative = path
                .strip_prefix(root)?
                .to_str()
                .context("Component paths must be UTF-8")?
                .replace('\\', "/");
            let metadata = std::fs::symlink_metadata(&path)?;
            entries.push((relative, path.clone()));
            if metadata.is_dir() {
                walk(root, &path, entries)?;
            }
            ensure!(
                entries.len() <= 100_000,
                "Component tree exceeds entry limit"
            );
        }
        Ok(())
    }
    let mut entries = Vec::new();
    walk(root, root, &mut entries)?;
    entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let mut digest = Sha512::new();
    let mut buffer = [0u8; 128 * 1024];
    for (relative, path) in entries {
        let metadata = std::fs::symlink_metadata(&path)?;
        let (kind, size, link) = if metadata.file_type().is_symlink() {
            let target = std::fs::read_link(&path)?;
            ensure!(
                target.is_relative() && canonical_path(&path)?.starts_with(root),
                "Component link escapes immutable object"
            );
            let bytes = target
                .to_str()
                .context("Component link is not UTF-8")?
                .as_bytes()
                .to_vec();
            ("l", bytes.len() as u64, Some(bytes))
        } else if metadata.is_dir() {
            ("d", 0, None)
        } else {
            ensure!(metadata.is_file(), "Special file in component object");
            ("f", metadata.len(), None)
        };
        #[cfg(unix)]
        let executable = {
            use std::os::unix::fs::PermissionsExt;
            u8::from(metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        };
        #[cfg(windows)]
        let executable = 0u8;
        digest.update(
            format!("{kind} {}:{relative} {executable} {size}\n", relative.len()).as_bytes(),
        );
        if let Some(bytes) = link {
            digest.update(&bytes);
        } else if kind == "f" {
            let mut file = File::open(&path)?;
            loop {
                let n = file.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                digest.update(&buffer[..n]);
            }
        }
        digest.update(b"\n");
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn selected_object_base(executable: &Path) -> Result<Option<PathBuf>> {
    let root = crate::paths::app_paths()
        .support_dir
        .join("desktop-components");
    let Ok(relative) = executable.strip_prefix(&root) else {
        return Ok(None);
    };
    let mut parts = relative.components();
    let id = parts
        .next()
        .context("Missing component installation")?
        .as_os_str()
        .to_str()
        .context("Invalid installation path")?;
    components::validate_sha256(id)?;
    ensure!(
        parts.next().is_some_and(|p| p.as_os_str() == "objects"),
        "Invalid selected component path"
    );
    let digest = parts
        .next()
        .context("Missing component object")?
        .as_os_str()
        .to_str()
        .context("Invalid object path")?;
    components::validate_sha256(digest)?;
    let selection: Selection = read_json(&root.join(id).join("selection.json"), true)?;
    ensure!(
        selection.installation_id == id,
        "Selected object installation binding differs"
    );
    Ok(Some(selection.base_root))
}

#[cfg(all(test, target_os = "linux"))]
mod selection_tests {
    use super::*;
    use paseo_protocol::components::ArtifactRef;

    fn fixture() -> (tempfile::TempDir, Store) {
        let temp = tempfile::tempdir().unwrap();
        let base_root = temp.path().join("base");
        let directory = temp.path().join("store");
        create_private(&base_root).unwrap();
        create_private(&directory).unwrap();
        let target = target().unwrap();
        let mut components = BTreeMap::new();
        let mut roots = BTreeMap::new();
        for id in [
            ComponentId::App,
            ComponentId::Daemon,
            ComponentId::Tailnet,
            ComponentId::UpdateHelper,
        ] {
            let image = base_root.join(id.as_str());
            std::fs::write(&image, id.as_str()).unwrap();
            let identity = ComponentIdentity {
                version: "1.0.0".into(),
                source_sha: "a".repeat(40),
                input_hash: "b".repeat(64),
                abi_version: id.initial_abi_version(),
            };
            components.insert(
                id,
                ComponentRef {
                    id,
                    target: target.clone(),
                    identity,
                    artifact: ArtifactRef {
                        url: format!(
                            "https://github.com/{REPOSITORY}/releases/download/v1.0.0/{id}.tar.gz"
                        ),
                        sha256: format!("{:x}", Sha256::digest(id.as_str().as_bytes())),
                        size: 10,
                        format: "tar.gz".into(),
                        origin_tag: "v1.0.0".into(),
                    },
                    executable: id.as_str().into(),
                    executable_sha256: sha256_file(&image).unwrap(),
                    requires: if id == ComponentId::App {
                        [
                            ComponentId::Daemon,
                            ComponentId::Tailnet,
                            ComponentId::UpdateHelper,
                        ]
                        .into_iter()
                        .map(|dependency| (dependency, dependency.initial_abi_version()))
                        .collect()
                    } else {
                        BTreeMap::new()
                    },
                },
            );
            roots.insert(id, ".");
        }
        atomic_sync(&base_root.join("components.json"),&serde_json::json!({"schemaVersion":1,"target":target,"components":components,"roots":roots,"packages":[]})).unwrap();
        let installation_id = format!(
            "{:x}",
            Sha256::digest(base_root.to_str().unwrap().as_bytes())
        );
        (
            temp,
            Store {
                directory,
                base_root,
                installation_id,
                target,
            },
        )
    }
    #[test]
    fn missing_established_selection_never_reverts_to_base() {
        let (_temp, store) = fixture();
        assert_eq!(store.selection().unwrap().generation, 0);
        atomic_sync(
            &store.directory.join("established"),
            &serde_json::json!({"schemaVersion":1,"installationId":store.installation_id}),
        )
        .unwrap();
        assert!(store.selection().is_err());
    }
    #[test]
    fn orphan_trial_uses_complete_active_without_loading_candidate() {
        let (_temp, store) = fixture();
        let mut selection = store.selection().unwrap();
        let active = selection.active.clone();
        let mut broken = active.clone();
        broken.get_mut(&ComponentId::Daemon).unwrap().root = "objects/missing".into();
        selection.trial = Some(Trial {
            operation_id: uuid::Uuid::new_v4().to_string(),
            components: broken,
        });
        atomic_sync(&store.directory.join("selection.json"), &selection).unwrap();
        open_private(&store.directory.join("lease"), true).unwrap();
        let loaded = store.selection().unwrap();
        assert_eq!(store.runnable(&loaded).unwrap(), active);
    }
    #[test]
    fn corrupt_active_image_and_foreign_installation_are_rejected() {
        let (_temp, store) = fixture();
        let mut selection = store.selection().unwrap();
        atomic_sync(&store.directory.join("selection.json"), &selection).unwrap();
        selection.installation_id = "0".repeat(64);
        atomic_sync(&store.directory.join("selection.json"), &selection).unwrap();
        assert!(store.selection().is_err());
        selection.installation_id = store.installation_id.clone();
        atomic_sync(&store.directory.join("selection.json"), &selection).unwrap();
        std::fs::write(store.base_root.join("daemon"), b"corrupt").unwrap();
        assert!(store.selection().is_err());
    }
    #[test]
    fn unsafe_selected_root_cannot_choose_arbitrary_executable() {
        let (_temp, store) = fixture();
        let selection = store.selection().unwrap();
        let mut reference = selection.active[&ComponentId::Daemon].clone();
        reference.root = "../other".into();
        assert!(store.image(&reference).is_err());
        reference.root = "base/other".into();
        assert!(store.image(&reference).is_err());
    }
    #[test]
    fn resource_digest_detects_dependency_corruption() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("library.so"), b"trusted").unwrap();
        let before = tree_digest(temp.path()).unwrap();
        std::fs::write(temp.path().join("library.so"), b"corrupt").unwrap();
        assert_ne!(before, tree_digest(temp.path()).unwrap());
    }
}

/// Reject duplicate keys before converting security-sensitive maps to DTOs.
/// serde_json's recursion limit also rejects excessively nested input.
pub(super) fn strict_json(bytes: &[u8]) -> Result<serde_json::Value> {
    struct Unique(serde_json::Value);
    impl<'de> Deserialize<'de> for Unique {
        fn deserialize<D: serde::Deserializer<'de>>(
            deserializer: D,
        ) -> std::result::Result<Self, D::Error> {
            struct Visitor;
            impl<'de> serde::de::Visitor<'de> for Visitor {
                type Value = Unique;
                fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                    f.write_str("JSON without duplicate keys")
                }
                fn visit_bool<E: serde::de::Error>(
                    self,
                    v: bool,
                ) -> std::result::Result<Unique, E> {
                    Ok(Unique(v.into()))
                }
                fn visit_i64<E: serde::de::Error>(self, v: i64) -> std::result::Result<Unique, E> {
                    Ok(Unique(v.into()))
                }
                fn visit_u64<E: serde::de::Error>(self, v: u64) -> std::result::Result<Unique, E> {
                    Ok(Unique(v.into()))
                }
                fn visit_f64<E: serde::de::Error>(self, v: f64) -> std::result::Result<Unique, E> {
                    Ok(Unique(
                        serde_json::Number::from_f64(v)
                            .ok_or_else(|| E::custom("Invalid JSON number"))?
                            .into(),
                    ))
                }
                fn visit_str<E: serde::de::Error>(self, v: &str) -> std::result::Result<Unique, E> {
                    Ok(Unique(v.into()))
                }
                fn visit_string<E: serde::de::Error>(
                    self,
                    v: String,
                ) -> std::result::Result<Unique, E> {
                    Ok(Unique(v.into()))
                }
                fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Unique, E> {
                    Ok(Unique(serde_json::Value::Null))
                }
                fn visit_seq<A: serde::de::SeqAccess<'de>>(
                    self,
                    mut seq: A,
                ) -> std::result::Result<Unique, A::Error> {
                    let mut values = Vec::new();
                    while let Some(Unique(value)) = seq.next_element::<Unique>()? {
                        if values.len() >= 100_000 {
                            return Err(serde::de::Error::custom("JSON array exceeds limit"));
                        }
                        values.push(value);
                    }
                    Ok(Unique(values.into()))
                }
                fn visit_map<A: serde::de::MapAccess<'de>>(
                    self,
                    mut map: A,
                ) -> std::result::Result<Unique, A::Error> {
                    let mut values = serde_json::Map::new();
                    while let Some((key, Unique(value))) = map.next_entry::<String, Unique>()? {
                        if values.len() >= 100_000 || values.insert(key, value).is_some() {
                            return Err(serde::de::Error::custom(
                                "Duplicate or excessive JSON keys",
                            ));
                        }
                    }
                    Ok(Unique(values.into()))
                }
            }
            deserializer.deserialize_any(Visitor)
        }
    }
    ensure!(
        bytes.len() as u64 <= MAX_JSON,
        "Component JSON exceeds limit"
    );
    Ok(serde_json::from_slice::<Unique>(bytes)?.0)
}

pub(super) fn normalize_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let value = path.to_string_lossy();
        if let Some(rest) = value.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = value.strip_prefix(r"\\?\") {
            return PathBuf::from(rest);
        }
    }
    path.to_owned()
}
pub(super) fn canonical_path(path: impl AsRef<Path>) -> Result<PathBuf> {
    Ok(normalize_path(&std::fs::canonicalize(path)?))
}
pub(super) fn same_path(left: &Path, right: &Path) -> bool {
    #[cfg(windows)]
    {
        return normalize_path(left)
            .to_string_lossy()
            .eq_ignore_ascii_case(&normalize_path(right).to_string_lossy());
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

pub fn report_startup_error(error: &anyhow::Error) {
    eprintln!("Desktop component startup refused: {error:#}");
    rfd::MessageDialog::new()
        .set_title("Passession component recovery required")
        .set_description(format!("The selected desktop components could not be verified. No older bundled runtime was started.\n\n{error:#}"))
        .set_level(rfd::MessageLevel::Error)
        .set_buttons(rfd::MessageButtons::Ok)
        .show();
}

#[cfg(test)]
mod metadata_tests {
    #[test]
    fn duplicate_keys_and_trailing_json_cannot_change_selection_meaning() {
        assert!(super::strict_json(br#"{"active":{"daemon":1,"daemon":2}}"#).is_err());
        assert!(super::strict_json(br#"{"schemaVersion":1,"schemaVersion":2}"#).is_err());
        assert!(super::strict_json(br#"{} {}"#).is_err());
        assert!(super::strict_json(br#"{"active":{"daemon":1},"trial":null}"#).is_ok());
    }
    #[cfg(windows)]
    #[test]
    fn windows_extended_paths_have_one_installation_binding() {
        use std::path::Path;
        assert_eq!(
            super::normalize_path(Path::new(r"\\?\C:\App")),
            Path::new(r"C:\App")
        );
        assert_eq!(
            super::normalize_path(Path::new(r"\\?\UNC\host\share\App")),
            Path::new(r"\\host\share\App")
        );
        assert!(super::same_path(
            Path::new(r"\\?\C:\App"),
            Path::new(r"c:\app")
        ));
    }
}

#[cfg(unix)]
fn sync_tree(root: &Path) -> Result<()> {
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            sync_tree(&entry.path())?;
        } else if kind.is_file() {
            File::open(entry.path())?.sync_all()?;
        }
    }
    File::open(root)?.sync_all()?;
    Ok(())
}

fn process_image(pid: u32) -> Result<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        canonical_path(std::fs::read_link(format!("/proc/{pid}/exe"))?)
    }
    #[cfg(target_os = "macos")]
    {
        #[link(name = "proc")]
        unsafe extern "C" {
            fn proc_pidpath(pid: i32, buffer: *mut libc::c_void, capacity: u32) -> i32;
        }
        let mut buffer = [0u8; 4096];
        let count = unsafe {
            proc_pidpath(
                i32::try_from(pid)?,
                buffer.as_mut_ptr().cast(),
                buffer.len() as u32,
            )
        };
        ensure!(count > 0, "Cannot identify component helper executable");
        let end = buffer.iter().position(|b| *b == 0).unwrap_or(buffer.len());
        canonical_path(Path::new(std::str::from_utf8(&buffer[..end])?))
    }
    #[cfg(windows)]
    unsafe {
        use windows::{
            Win32::{
                Foundation::CloseHandle,
                System::Threading::{
                    OpenProcess, PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION,
                    QueryFullProcessImageNameW,
                },
            },
            core::PWSTR,
        };
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)?;
        let mut buffer = [0u16; 32768];
        let mut length = buffer.len() as u32;
        let result = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_FORMAT(0),
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        );
        let _ = CloseHandle(handle);
        result?;
        canonical_path(String::from_utf16(&buffer[..length as usize])?)
    }
}

fn entry_exists(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}
