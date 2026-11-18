//! Content-based release and CI planning. No build or publication occurs here.
use crate::{common, release};
use anyhow::{Context, Result, bail, ensure};
use paseo_protocol::components::{self, ComponentId, ComponentRef, PackageRef, ReleaseInventory};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Plan {
    pub schema_version: u32,
    pub version: String,
    pub source_sha: String,
    pub channel: String,
    pub targets: Vec<TargetPlan>,
    pub no_changes: bool,
    #[serde(default)]
    pub retained_targets: BTreeMap<String, paseo_protocol::components::TargetInventory>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct TargetPlan {
    pub target: String,
    pub build: Vec<ComponentId>,
    pub reuse: BTreeMap<ComponentId, ComponentRef>,
    pub inputs: BTreeMap<ComponentId, String>,
    pub package: bool,
    pub package_input_hash: String,
    pub atomic_groups: Vec<Vec<ComponentId>>,
    pub packages: Vec<PackageRef>,
}
impl TargetPlan {
    pub(crate) fn changed(&self) -> bool {
        self.package || !self.build.is_empty()
    }
}
pub(crate) fn read(path: &Path) -> Result<Plan> {
    ensure!(
        fs::metadata(path)?.len() <= 4 * 1024 * 1024,
        "Component plan too large"
    );
    let plan: Plan = serde_json::from_slice(&fs::read(path)?)?;
    validate(&plan)?;
    Ok(plan)
}
pub(crate) fn validate(plan: &Plan) -> Result<()> {
    ensure!(
        plan.schema_version == 1,
        "Unsupported component plan schema"
    );
    let version = release::version(&plan.version)?;
    ensure!(
        release::valid_sha(&plan.source_sha) && plan.channel == release::channel(&version),
        "Invalid collection identity"
    );
    let mut seen = BTreeSet::new();
    ensure!(!plan.targets.is_empty(), "Empty component target selection");
    for target in &plan.targets {
        release::target(&target.target)?;
        ensure!(seen.insert(&target.target), "Duplicate component target");
        let ids = supported(&target.target);
        ensure!(
            target.inputs.keys().copied().collect::<BTreeSet<_>>() == ids.iter().copied().collect(),
            "Incomplete input fingerprints"
        );
        let built: BTreeSet<_> = target.build.iter().copied().collect();
        ensure!(
            built.len() == target.build.len(),
            "Duplicate component build"
        );
        ensure!(
            target.reuse.keys().all(|id| !built.contains(id)),
            "Ambiguous build/reuse decision"
        );
        ensure!(
            built
                .union(&target.reuse.keys().copied().collect())
                .copied()
                .collect::<BTreeSet<_>>()
                == ids.into_iter().collect(),
            "Incomplete component decisions"
        );
        for hash in target
            .inputs
            .values()
            .chain(std::iter::once(&target.package_input_hash))
        {
            components::validate_sha256(hash)?;
        }
        for (id, reference) in &target.reuse {
            components::validate_component(reference, &target.target, release::REPOSITORY)?;
            ensure!(
                *id == reference.id && reference.identity.input_hash == target.inputs[id],
                "Reuse fingerprint mismatch"
            );
        }
        ensure!(
            target.atomic_groups == components::required_atomic_groups(&target.target)?,
            "Invalid target atomic groups"
        );
        ensure!(
            target.package || !target.packages.is_empty(),
            "No bootstrap package or prior package"
        );
    }
    for (target, inventory) in &plan.retained_targets {
        ensure!(
            !seen.contains(target),
            "Retained target overlaps planned target"
        );
        components::validate_target_inventory(inventory, target, release::REPOSITORY)?;
    }
    ensure!(
        plan.no_changes != plan.targets.iter().any(TargetPlan::changed),
        "Incorrect no-change decision"
    );
    Ok(())
}
pub(crate) fn supported(target: &str) -> Vec<ComponentId> {
    if target.starts_with("android-") || target.starts_with("ios-") {
        vec![ComponentId::App, ComponentId::Tailnet]
    } else {
        vec![
            ComponentId::App,
            ComponentId::Daemon,
            ComponentId::Tailnet,
            ComponentId::UpdateHelper,
        ]
    }
}
fn triple(target: &str) -> Result<&'static str> {
    Ok(common::BuildTarget::parse(target)?.triple)
}
fn command(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("git").args(args).current_dir(root).output()?;
    ensure!(
        out.status.success(),
        "git read failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(out.stdout)
}
fn files(root: &Path) -> Result<Vec<String>> {
    let bytes = command(
        root,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
    )?;
    let mut files = BTreeSet::new();
    for raw in bytes.split(|b| *b == 0).filter(|s| !s.is_empty()) {
        let path = std::str::from_utf8(raw)?.replace('\\', "/");
        if root.join(&path).exists() {
            files.insert(path);
        }
    }
    Ok(files.into_iter().collect())
}
fn documentation(path: &str) -> bool {
    (path.ends_with(".md") && !path.starts_with("skills/") && !path.contains("/resources/"))
        || path.starts_with("docs/")
        || path.starts_with(".github/ISSUE_TEMPLATE/")
        || [".gitignore", ".gitattributes", "AGENTS.md"].contains(&path)
}
fn input_bytes(root: &Path, path: &str) -> Result<Vec<u8>> {
    let full = root.join(path);
    let metadata = fs::symlink_metadata(&full)?;
    ensure!(
        metadata.file_type().is_file(),
        "Input is not a regular file: {path}"
    );
    // Collection version/build-number files are excluded by their callers. Cargo
    // library versions remain real compile inputs and must never be normalized away.
    Ok(fs::read(full)?)
}
fn fingerprint(root: &Path, paths: &BTreeSet<String>, context: &Value) -> Result<String> {
    let mut hash = Sha256::new();
    hash.update(b"paseo-component-input-v1\0");
    hash.update(serde_json::to_vec(context)?);
    for path in paths {
        let bytes = input_bytes(root, path)?;
        hash.update((path.len() as u64).to_le_bytes());
        hash.update(path.as_bytes());
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
    }
    Ok(hex::encode(hash.finalize()))
}
fn closure<'a>(
    metadata: &'a Value,
    name: &str,
    development: bool,
) -> Result<Vec<(&'a Value, &'a Value)>> {
    let packages = metadata["packages"]
        .as_array()
        .context("Missing Cargo packages")?;
    let nodes = metadata["resolve"]["nodes"]
        .as_array()
        .context("Missing Cargo graph")?;
    let roots: Vec<_> = packages
        .iter()
        .filter(|p| p["name"] == name && p["source"].is_null())
        .collect();
    ensure!(roots.len() == 1, "Ambiguous Cargo root {name}");
    let mut pending = vec![roots[0]["id"].as_str().context("Missing Cargo ID")?];
    let mut seen = BTreeSet::new();
    let mut result = Vec::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id) {
            continue;
        }
        let package = packages
            .iter()
            .find(|p| p["id"] == id)
            .context("Missing resolved package")?;
        let node = nodes
            .iter()
            .find(|n| n["id"] == id)
            .context("Missing resolved node")?;
        for dependency in node["deps"]
            .as_array()
            .context("Missing dependency edges")?
        {
            let kinds = dependency["dep_kinds"]
                .as_array()
                .context("Missing dependency kinds")?;
            ensure!(!kinds.is_empty(), "Ambiguous dependency kind");
            if kinds.iter().any(|k| {
                development || k["kind"].is_null() || k["kind"] == "normal" || k["kind"] == "build"
            }) {
                pending.push(
                    dependency["pkg"]
                        .as_str()
                        .context("Missing dependency ID")?,
                );
            }
        }
        result.push((package, node));
    }
    result.sort_by_key(|(package, _)| package["id"].as_str().unwrap_or_default());
    Ok(result)
}
fn within(path: &str, directory: &str) -> bool {
    path == directory
        || path
            .strip_prefix(directory)
            .is_some_and(|suffix| suffix.starts_with('/'))
}
fn metadata(root: &Path, target: &str) -> Result<Value> {
    let out = common::command(common::cargo())?
        .args([
            "metadata",
            "--locked",
            "--format-version",
            "1",
            "--filter-platform",
            triple(target)?,
        ])
        .current_dir(root)
        .output()?;
    ensure!(
        out.status.success(),
        "Cargo graph unavailable: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(serde_json::from_slice(&out.stdout)?)
}
/// Cargo metadata is workspace-wide; Cargo tree selects the actual standalone
/// invocation, so app-only feature unification cannot invalidate the daemon.
/// The tree spans every platform: with one `--target`, build dependencies follow
/// the host's cfg (a Windows host adds `winreg` under `embed-resource`), so the
/// Linux planner and the native builder would disagree. The target-filtered
/// metadata graph still selects which packages count for this target.
fn compilation_features(
    root: &Path,
    metadata: &Value,
    id: ComponentId,
) -> Result<Option<BTreeMap<String, Value>>> {
    let package = match id {
        ComponentId::App => "paseo-gui",
        ComponentId::Daemon => "paseo-daemon",
        _ => return Ok(None),
    };
    let output = common::command(common::cargo())?
        .args([
            "tree",
            "--locked",
            "--target",
            "all",
            "-p",
            package,
            "--edges",
            "normal,build",
            "--prefix",
            "none",
            "--format",
            "{p}\t{f}",
        ])
        .current_dir(root)
        .output()?;
    ensure!(
        output.status.success(),
        "Standalone Cargo closure unavailable: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut resolved: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for line in std::str::from_utf8(&output.stdout)?
        .lines()
        .filter(|line| !line.is_empty())
    {
        let line = line.strip_suffix(" (*)").unwrap_or(line);
        let (display, features) = line
            .split_once('\t')
            .context("Unrecognized Cargo tree output")?;
        let (name, version) = display
            .split_once(" v")
            .context("Cargo tree lacks package version")?;
        let name = name
            .split_whitespace()
            .next()
            .context("Cargo tree lacks package name")?;
        let version = version
            .split_whitespace()
            .next()
            .context("Cargo tree lacks package version")?;
        let matches: Vec<_> = metadata["packages"]
            .as_array()
            .context("Missing Cargo packages")?
            .iter()
            .filter(|p| p["name"] == name && p["version"] == version)
            .collect();
        // Packages for other targets are absent from the filtered metadata.
        let id = match matches.as_slice() {
            [] => continue,
            [package] => package["id"].as_str().context("Missing package ID")?,
            _ => bail!("Ambiguous standalone Cargo package {name}@{version}"),
        };
        resolved.entry(id.into()).or_default().extend(
            features
                .split(',')
                .filter(|feature| !feature.is_empty())
                .map(str::to_owned),
        );
    }
    ensure!(!resolved.is_empty(), "Empty standalone Cargo closure");
    Ok(Some(
        resolved
            .into_iter()
            .map(|(id, features)| (id, json!(features)))
            .collect(),
    ))
}
fn component_inputs(
    root: &Path,
    all: &[String],
    metadata: &Value,
    target: &str,
    id: ComponentId,
    resolved: Option<&BTreeMap<String, Value>>,
) -> Result<(BTreeSet<String>, Value)> {
    let mut selected = BTreeSet::new();
    let mut dependencies = Vec::new();
    let rust_component = matches!(id, ComponentId::App | ComponentId::Daemon);
    if matches!(id, ComponentId::App | ComponentId::Daemon) {
        let root_name = if id == ComponentId::App {
            "paseo-gui"
        } else {
            "paseo-daemon"
        };
        let lock: toml::Value = toml::from_str(&fs::read_to_string(root.join("Cargo.lock"))?)?;
        for (package, node) in closure(metadata, root_name, false)? {
            let package_id = package["id"].as_str().context("Missing package ID")?;
            if resolved.is_some_and(|features| !features.contains_key(package_id)) {
                continue;
            }
            let features = resolved
                .and_then(|features| features.get(package_id))
                .unwrap_or(&node["features"]);
            if package["source"].is_null() {
                // Windows canonical paths carry a verbatim prefix that Cargo
                // metadata omits; compare both in canonical form.
                let directory = Path::new(
                    package["manifest_path"]
                        .as_str()
                        .context("Missing manifest path")?,
                )
                .parent()
                .context("Missing crate directory")?
                .canonicalize()?;
                let relative = directory
                    .strip_prefix(root)
                    .context("Path dependency outside repository")?
                    .to_str()
                    .context("Non-UTF8 crate path")?
                    .replace('\\', "/");
                selected.extend(
                    all.iter()
                        .filter(|p| {
                            within(p, &relative)
                                && !documentation(p)
                                && !within(p, "packages/tailnet/native")
                        })
                        .cloned(),
                );
                dependencies
                    .push(json!({"path":relative,"name":package["name"],"features":features}));
            } else {
                let entry = lock["package"]
                    .as_array()
                    .context("Missing lockfile packages")?
                    .iter()
                    .find(|p| {
                        p.get("name").and_then(toml::Value::as_str) == package["name"].as_str()
                            && p.get("version").and_then(toml::Value::as_str)
                                == package["version"].as_str()
                            && p.get("source").and_then(toml::Value::as_str)
                                == package["source"].as_str()
                    })
                    .context("Resolved dependency missing from lockfile")?;
                if package["source"]
                    .as_str()
                    .unwrap_or_default()
                    .starts_with("registry+")
                {
                    ensure!(
                        entry
                            .get("checksum")
                            .and_then(toml::Value::as_str)
                            .is_some(),
                        "Registry dependency has no checksum"
                    );
                }
                dependencies.push(json!({"name":package["name"],"version":package["version"],"source":package["source"],"checksum":entry.get("checksum"),"features":features}));
            }
        }
    }
    for path in all {
        if documentation(path)
            || [
                "packaging/release.json",
                "packaging/android/version.properties",
                "Cargo.lock",
            ]
            .contains(&path.as_str())
        {
            continue;
        }
        // Go never compiles *_test.go into shipped libraries or executables.
        if !rust_component && path.ends_with("_test.go") {
            continue;
        }
        let global = (rust_component
            && (path == "Cargo.toml"
                || path.starts_with(".cargo/")
                || path.starts_with("rust-toolchain")))
            || path == ".tool-versions"
            || path.starts_with(".github/actions/setup-")
            || path == ".github/workflows/desktop-release.yml"
            || path.starts_with("xtask/src/components/")
            || path.starts_with("xtask/src/cross_tools/")
            || [
                "xtask/Cargo.toml",
                "xtask/src/main.rs",
                "xtask/src/common.rs",
                "xtask/src/components.rs",
                "xtask/src/cross.rs",
                "xtask/src/cross_tools.rs",
                "xtask/src/notices.rs",
                "xtask/src/signing.rs",
                "LICENSE",
            ]
            .contains(&path.as_str())
            || (matches!(id, ComponentId::App | ComponentId::Daemon)
                && path == "xtask/src/build_cache.rs");
        let specific = match id {
            ComponentId::App => {
                ["xtask/src/gui.rs", "xtask/src/libraries.rs"].contains(&path.as_str())
                    || path == "packaging/desktop/icon.png"
                    || (target.starts_with("windows-")
                        && (path.starts_with("packaging/windows/")
                            || path == "packaging/desktop/icon.ico"))
                    || (target.starts_with("macos-")
                        && path.starts_with("packaging/desktop/macos/"))
                    || (target.starts_with("ios-") && path.starts_with("packaging/ios/"))
                    || path.starts_with("packaging/resource-")
                    || path.starts_with("packaging/native-")
                    || path.starts_with("packaging/cargo-notice-")
            }
            ComponentId::Daemon => {
                path == "xtask/src/build.rs"
                    || path == "xtask/src/libraries.rs"
                    || path.starts_with("skills/")
                    || path.starts_with("packaging/cargo-notice-")
            }
            ComponentId::Tailnet => {
                within(path, "packages/tailnet/native")
                    || path == "xtask/src/tailnet.rs"
                    || ((target.starts_with("android-") || target.starts_with("ios-"))
                        && path == "xtask/src/gui.rs")
                    || (target.starts_with("ios-")
                        && path == "packages/gui-platform/ios/Info.plist")
            }
            ComponentId::UpdateHelper => {
                within(path, "packages/app-updater") || path == "xtask/src/app_updater.rs"
            }
        };
        // Unknown repository build inputs cannot silently reuse any component.
        let known = [
            "packages/",
            "plugin-examples/",
            "packaging/",
            "skills/",
            "xtask/",
            ".github/",
            "docker/",
            "scripts/",
            "fastlane/",
            "test/",
            "tests/",
        ]
        .iter()
        .any(|prefix| path.starts_with(prefix));
        let unknown = (!known
            && !["Cargo.toml", ".tool-versions", "LICENSE"].contains(&path.as_str())
            && !path.starts_with('.'))
            || path.starts_with("scripts/");
        if global || specific || unknown {
            selected.insert(path.clone());
        }
    }
    // Native toolchain selectors are pinned in the recipe/config files above. Runtime
    // source/version stamps are intentionally omitted: reuse retains the original stamps.
    let environment: BTreeMap<_, _> = std::env::vars()
        .filter(|(name, value)| {
            rust_component
                && !value.is_empty()
                && ([
                    "RUSTFLAGS",
                    "CARGO_ENCODED_RUSTFLAGS",
                    "CC",
                    "CXX",
                    "MACOSX_DEPLOYMENT_TARGET",
                    "MAKEPAD",
                    "LIBGHOSTTY_VT_SYS_CPU",
                    "LIBGHOSTTY_VT_SYS_OPTIMIZE",
                ]
                .contains(&name.as_str())
                    || name.starts_with("CARGO_PROFILE_RELEASE_")
                    || (name.starts_with("CARGO_TARGET_") && name.ends_with("_LINKER")))
        })
        .collect();
    Ok((
        selected,
        json!({"target":target,"triple":triple(target)?,"component":id,"abi":id.initial_abi_version(),"profile":"release","dependencies":dependencies,"environment":environment,"nativeRelease":true,"channel":release::channel(&release::version(&common::version()?)?),"repository":release::REPOSITORY}),
    ))
}
fn package_path(platform: &str, path: &str) -> bool {
    let Some(relative) = path.strip_prefix("packaging/") else {
        return false;
    };
    let directory = relative.split('/').next().unwrap_or_default();
    if ["linux", "windows", "macos", "android", "ios"].contains(&directory) {
        return directory == platform;
    }
    if relative.starts_with("desktop/") {
        if !["linux", "windows", "macos"].contains(&platform) {
            return false;
        }
        if relative == "desktop/installer.nsi" || relative == "desktop/icon.ico" {
            return platform == "windows";
        }
        if relative == "desktop/entitlements.plist" || relative.starts_with("desktop/macos/") {
            return platform == "macos";
        }
        if relative.ends_with(".metainfo.xml") {
            return platform == "linux";
        }
    }
    true
}
fn package_hash(root: &Path, all: &[String], target: &str) -> Result<String> {
    let platform = release::target(target)?.platform;
    let selected = all
        .iter()
        .filter(|p| {
            !documentation(p)
                && **p != "packaging/release.json"
                && **p != "packaging/android/version.properties"
                && (package_path(platform, p)
                    || [
                        "xtask/Cargo.toml",
                        "xtask/src/components.rs",
                        "xtask/src/components/build.rs",
                        "xtask/src/common.rs",
                        "xtask/src/package.rs",
                        "xtask/src/assets.rs",
                        "xtask/src/signing.rs",
                    ]
                    .contains(&p.as_str()))
        })
        .cloned()
        .collect();
    fingerprint(root, &selected, &json!({"target":target,"packageSchema":1}))
}
pub(crate) fn decide(
    target: &str,
    inputs: BTreeMap<ComponentId, String>,
    package_input_hash: String,
    prior: Option<&paseo_protocol::components::TargetInventory>,
    prior_package_hash: Option<&str>,
) -> Result<TargetPlan> {
    let atomic_groups = components::required_atomic_groups(target)?;
    let mut build = BTreeSet::new();
    let mut reuse = BTreeMap::new();
    for (id, hash) in &inputs {
        match prior.and_then(|p| p.components.get(id)) {
            Some(reference)
                if reference.identity.input_hash == *hash
                    && reference.identity.abi_version == id.initial_abi_version() =>
            {
                reuse.insert(*id, reference.clone());
            }
            _ => {
                build.insert(*id);
            }
        }
    }
    if target.starts_with("ios-") && build.contains(&ComponentId::Tailnet) {
        build.insert(ComponentId::App);
        reuse.remove(&ComponentId::App);
    }
    let packages = prior.map(|p| p.packages.clone()).unwrap_or_default();
    let atomic_change =
        !atomic_groups.is_empty() && atomic_groups.iter().flatten().any(|id| build.contains(id));
    // A package bundles exact component images, so a rebuilt image makes every
    // package that contains it stale even when the packaging inputs are unchanged.
    let bundled_change = packages
        .iter()
        .any(|package| package.components.keys().any(|id| build.contains(id)));
    let package = packages.is_empty()
        || prior_package_hash != Some(package_input_hash.as_str())
        || atomic_change
        || bundled_change;
    Ok(TargetPlan {
        target: target.into(),
        build: build.into_iter().collect(),
        reuse,
        inputs,
        package,
        package_input_hash,
        atomic_groups,
        packages,
    })
}
pub(crate) fn create(
    targets: &[String],
    prior: Option<&ReleaseInventory>,
    package_hashes: &BTreeMap<String, String>,
) -> Result<Plan> {
    let root = common::root()?.canonicalize()?;
    let all = files(&root)?;
    let version = common::version()?;
    let parsed = release::version(&version)?;
    let source_sha = release::var(
        "SOURCE_SHA",
        String::from_utf8(command(&root, &["rev-parse", "HEAD"])?)?.trim(),
    );
    let mut plans = Vec::new();
    for target in targets {
        let graph = metadata(&root, target)?;
        let mut inputs = BTreeMap::new();
        for id in supported(target) {
            let resolved = compilation_features(&root, &graph, id)?;
            let (paths, context) =
                component_inputs(&root, &all, &graph, target, id, resolved.as_ref())?;
            inputs.insert(id, fingerprint(&root, &paths, &context)?);
        }
        plans.push(decide(
            target,
            inputs,
            package_hash(&root, &all, target)?,
            prior.and_then(|p| p.targets.get(target)),
            package_hashes.get(target).map(String::as_str),
        )?);
    }
    let retained_targets = prior
        .map(|p| {
            p.targets
                .iter()
                .filter(|(target, _)| !targets.contains(target))
                .map(|(target, inventory)| (target.clone(), inventory.clone()))
                .collect()
        })
        .unwrap_or_default();
    let result = Plan {
        schema_version: 1,
        version,
        source_sha,
        channel: release::channel(&parsed).into(),
        no_changes: !plans.iter().any(TargetPlan::changed),
        targets: plans,
        retained_targets,
    };
    ensure!(
        result.no_changes || prior.is_none_or(|p| p.version != result.version),
        "Changed inputs require a new collection version; published component assets are immutable"
    );
    validate(&result)?;
    Ok(result)
}
pub(crate) fn validate_inputs(plan: &Plan, target: &TargetPlan) -> Result<()> {
    validate(plan)?;
    let root = common::root()?.canonicalize()?;
    let sha = String::from_utf8(command(&root, &["rev-parse", "HEAD"])?)?;
    ensure!(
        sha.trim() == plan.source_sha && common::version()? == plan.version,
        "Plan checkout/collection mismatch"
    );
    let graph = metadata(&root, &target.target)?;
    let all = files(&root)?;
    for id in supported(&target.target) {
        let resolved = compilation_features(&root, &graph, id)?;
        let (paths, context) =
            component_inputs(&root, &all, &graph, &target.target, id, resolved.as_ref())?;
        ensure!(
            target.inputs.get(&id) == Some(&fingerprint(&root, &paths, &context)?),
            "Component build inputs changed: {id}"
        );
    }
    ensure!(
        package_hash(&root, &all, &target.target)? == target.package_input_hash,
        "Package inputs changed after planning"
    );
    release::remote::authenticate_reuse(target)
}
fn options(args: &[String]) -> Result<BTreeMap<String, String>> {
    let mut values = BTreeMap::new();
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        let value = if flag == "--no-prior" {
            String::new()
        } else {
            args.next()
                .context("Every planner flag except --no-prior requires a value")?
                .clone()
        };
        ensure!(
            values.insert(flag.clone(), value).is_none(),
            "Duplicate planner option"
        );
    }
    Ok(values)
}
pub(crate) fn dispatch(command_name: &str, args: &[String]) -> Result<()> {
    let options = options(args)?;
    match command_name {
        "plan" => {
            ensure!(
                options
                    .keys()
                    .all(|k| ["--output", "--targets", "--prior-tag", "--no-prior"]
                        .contains(&k.as_str())),
                "Unknown component planner flag"
            );
            let targets = options
                .get("--targets")
                .map(|s| s.split(',').map(str::to_owned).collect())
                .unwrap_or_else(|| release::TARGETS.iter().map(|t| t.id()).collect::<Vec<_>>());
            ensure!(
                !(options.contains_key("--no-prior") && options.contains_key("--prior-tag")),
                "--no-prior and --prior-tag are mutually exclusive"
            );
            let (prior, hashes) = if options.contains_key("--no-prior") {
                (None, BTreeMap::new())
            } else {
                release::remote::prior_inventory(options.get("--prior-tag").map(String::as_str))?
            };
            let plan = create(&targets, prior.as_ref(), &hashes)?;
            let path = PathBuf::from(
                options
                    .get("--output")
                    .map(String::as_str)
                    .unwrap_or("artifacts/component-plan.json"),
            );
            common::write_json(&path, &serde_json::to_value(&plan)?)?;
            println!("{}", serde_json::to_string(&plan)?);
            Ok(())
        }
        "ci-plan" => ci_plan(&options),
        _ => bail!("Unknown planner command {command_name}"),
    }
}
fn ci_plan(options: &BTreeMap<String, String>) -> Result<()> {
    ensure!(
        options
            .keys()
            .all(|k| ["--base", "--output"].contains(&k.as_str())),
        "Unknown CI planner flag"
    );
    let root = common::root()?.canonicalize()?;
    let base = options.get("--base").map(String::as_str).unwrap_or("");
    let paths = if release::valid_sha(base) && base.bytes().any(|b| b != b'0') {
        command(&root, &["diff", "--name-only", "-z", base, "HEAD"])?
    } else {
        command(&root, &["ls-files", "-z"])?
    };
    let changed: BTreeSet<String> = paths
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| Ok(std::str::from_utf8(s)?.to_owned()))
        .collect::<Result<_>>()?;
    let output = ci_decisions(&root, &changed)?;
    if let Some(path) = options.get("--output") {
        common::write_json(Path::new(path), &output)?;
    }
    for (key, value) in output.as_object().unwrap() {
        println!(
            "{key}={}",
            value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| value.to_string())
        );
    }
    Ok(())
}
fn ci_decisions(root: &Path, changed: &BTreeSet<String>) -> Result<Value> {
    let effective: BTreeSet<_> = changed
        .iter()
        .filter(|p| !documentation(p))
        .cloned()
        .collect();
    if effective.is_empty() {
        return Ok(
            json!({"packages":[],"rust":false,"tailnet":false,"helper":false,"abi":false,"gui":false,"native":false}),
        );
    }
    let graph = metadata(root, "linux-x64")?;
    let all: Vec<_> = files(root)?
        .into_iter()
        .chain(effective.iter().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    ci_decisions_for_inputs(root, &effective, &graph, &all)
}

fn ci_decisions_for_inputs(
    root: &Path,
    effective: &BTreeSet<String>,
    graph: &Value,
    all: &[String],
) -> Result<Value> {
    let global = effective.iter().any(|p| {
        [
            "Cargo.toml",
            "Cargo.lock",
            ".tool-versions",
            ".github/workflows/ci.yml",
        ]
        .contains(&p.as_str())
            || p.starts_with(".cargo/")
            || p.starts_with("rust-toolchain")
    });
    let mut packages = BTreeSet::new();
    for package in graph["packages"]
        .as_array()
        .context("Missing package metadata")?
        .iter()
        .filter(|p| {
            graph["workspace_members"]
                .as_array()
                .is_some_and(|members| members.contains(&p["id"]))
        })
    {
        let name = package["name"].as_str().context("Missing package name")?;
        let affected = global
            || closure(graph, name, true)?.iter().any(|(p, _)| {
                if !p["source"].is_null() {
                    return false;
                }
                Path::new(p["manifest_path"].as_str().unwrap_or_default())
                    .parent()
                    .and_then(|dir| dir.canonicalize().ok())
                    .and_then(|dir| dir.strip_prefix(root).ok().map(Path::to_path_buf))
                    .as_deref()
                    .and_then(Path::to_str)
                    .is_some_and(|dir| {
                        effective.iter().any(|path| {
                            within(path, &dir.replace('\\', "/"))
                                && !within(path, "packages/tailnet/native")
                        })
                    })
            });
        if affected {
            packages.insert(name.to_owned());
        }
    }
    let affects = |id| -> Result<bool> {
        let (paths, _) = component_inputs(root, all, graph, "linux-x64", id, None)?;
        Ok(
            (global && matches!(id, ComponentId::App | ComponentId::Daemon))
                || !paths.is_disjoint(effective),
        )
    };
    let ci_recipe = effective.contains(".github/workflows/ci.yml");
    let tailnet = affects(ComponentId::Tailnet)?
        || ci_recipe
        || effective.iter().any(|p| p.starts_with("packages/tailnet/"));
    let helper = affects(ComponentId::UpdateHelper)?
        || ci_recipe
        || effective.iter().any(|p| {
            p.starts_with("packages/app-updater/")
                || p.starts_with("packages/protocol/src/components")
        });
    let abi = tailnet
        || effective.iter().any(|p| {
            p.starts_with("packages/tailnet/src/")
                || p.starts_with("packages/protocol/src/components")
        });
    let gui = packages.iter().any(|p| p.starts_with("paseo-gui"));
    let native = affects(ComponentId::App)?
        || affects(ComponentId::Daemon)?
        || affects(ComponentId::Tailnet)?
        || affects(ComponentId::UpdateHelper)?;
    Ok(
        json!({"packages":packages,"rust":!packages.is_empty(),"tailnet":tailnet,"helper":helper,"abi":abi,"gui":gui,"native":native}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use paseo_protocol::components::{ArtifactRef, ComponentIdentity, TargetInventory};
    fn fixture() -> (BTreeMap<ComponentId, String>, TargetInventory) {
        let inputs: BTreeMap<_, _> = supported("linux-x64")
            .into_iter()
            .map(|id| (id, "a".repeat(64)))
            .collect();
        let identity = |id: ComponentId| ComponentIdentity {
            version: "0.13.26".into(),
            source_sha: "b".repeat(40),
            input_hash: inputs[&id].clone(),
            abi_version: id.initial_abi_version(),
        };
        let artifact = ArtifactRef {
            url:
                "https://github.com/emotionbug/Passession/releases/download/v0.13.26/archive.tar.gz"
                    .into(),
            sha256: "c".repeat(64),
            size: 10,
            format: "tar.gz".into(),
            origin_tag: "v0.13.26".into(),
        };
        let components = inputs
            .keys()
            .map(|id| {
                (
                    *id,
                    ComponentRef {
                        id: *id,
                        target: "linux-x64".into(),
                        identity: identity(*id),
                        artifact: artifact.clone(),
                        executable: "bin/image".into(),
                        executable_sha256: "d".repeat(64),
                        requires: BTreeMap::new(),
                    },
                )
            })
            .collect();
        let packages = vec![PackageRef {
            target: "linux-x64".into(),
            version: "0.13.26".into(),
            source_sha: "b".repeat(40),
            artifact,
            components: inputs.keys().map(|id| (*id, identity(*id))).collect(),
        }];
        (
            inputs,
            TargetInventory {
                components,
                packages,
                atomic_groups: vec![],
            },
        )
    }
    #[test]
    fn no_prior_planner_flag_needs_no_value() {
        let parsed = options(
            &[
                "--no-prior",
                "--targets",
                "windows-x64",
                "--output",
                "plan.json",
            ]
            .map(str::to_owned),
        )
        .unwrap();
        assert!(parsed.contains_key("--no-prior"));
        assert_eq!(parsed["--targets"], "windows-x64");
        assert!(options(&["--targets".to_owned()]).is_err());
        assert!(options(&["--no-prior".to_owned(), "--no-prior".to_owned()]).is_err());
    }

    #[test]
    fn selective_decisions_preserve_original_sources_and_packages() {
        let (inputs, prior) = fixture();
        let unchanged = decide(
            "linux-x64",
            inputs.clone(),
            "e".repeat(64),
            Some(&prior),
            Some(&"e".repeat(64)),
        )
        .unwrap();
        assert!(!unchanged.changed());
        assert_eq!(unchanged.reuse, prior.components);
        for id in [ComponentId::App, ComponentId::Daemon] {
            let mut changed = inputs.clone();
            changed.insert(id, "f".repeat(64));
            let plan = decide(
                "linux-x64",
                changed,
                "e".repeat(64),
                Some(&prior),
                Some(&"e".repeat(64)),
            )
            .unwrap();
            assert_eq!(plan.build, vec![id]);
            assert!(
                plan.package,
                "a rebuilt {id:?} makes the bundled package stale"
            );
            assert_eq!(plan.packages, prior.packages);
            for (id, reference) in plan.reuse {
                assert_eq!(reference, prior.components[&id]);
            }
        }
        // A package that does not bundle the rebuilt component stays as it is.
        let mut unbundled = prior.clone();
        unbundled.packages[0]
            .components
            .remove(&ComponentId::Daemon);
        let mut daemon_only = inputs.clone();
        daemon_only.insert(ComponentId::Daemon, "f".repeat(64));
        assert!(
            !decide(
                "linux-x64",
                daemon_only,
                "e".repeat(64),
                Some(&unbundled),
                Some(&"e".repeat(64)),
            )
            .unwrap()
            .package
        );
        let mut shared = inputs.clone();
        shared.insert(ComponentId::App, "f".repeat(64));
        shared.insert(ComponentId::Daemon, "f".repeat(64));
        assert_eq!(
            decide(
                "linux-x64",
                shared,
                "e".repeat(64),
                Some(&prior),
                Some(&"e".repeat(64))
            )
            .unwrap()
            .build,
            vec![ComponentId::App, ComponentId::Daemon]
        );
        let packaging = decide(
            "linux-x64",
            inputs.clone(),
            "f".repeat(64),
            Some(&prior),
            Some(&"e".repeat(64)),
        )
        .unwrap();
        assert!(packaging.package);
        assert!(packaging.build.is_empty());
        assert_eq!(
            decide("linux-x64", inputs, "e".repeat(64), None, None)
                .unwrap()
                .build
                .len(),
            4
        );
    }
    #[test]
    fn documentation_only_needs_no_cargo_or_native_tools() {
        let paths = BTreeSet::from(["README.md".into(), "docs/guide.txt".into()]);
        let plan = ci_decisions(Path::new("/nonexistent"), &paths).unwrap();
        assert_eq!(plan["packages"], json!([]));
        assert_eq!(plan["native"], false);
    }
    #[test]
    fn source_closure_fixtures_select_only_affected_artifacts() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let all = [
            "Cargo.toml",
            "packages/gui/src/main.rs",
            "packages/daemon/src/main.rs",
            "packages/client/src/lib.rs",
            "packages/tailnet/src/lib.rs",
            "packages/gui-platform/ios/Info.plist",
            "xtask/src/gui.rs",
            "packages/tailnet/native/engine.go",
            "packages/tailnet/native/engine_test.go",
            "packages/app-updater/main.go",
            "packages/app-updater/main_test.go",
            "skills/help/SKILL.md",
            "packaging/linux/control",
            "docs/guide.md",
        ]
        .map(str::to_owned)
        .to_vec();
        for path in &all {
            fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
            fs::write(
                root.join(path),
                if path == "Cargo.toml" {
                    "[workspace.package]\nversion='0.13.26'\n"
                } else {
                    "original"
                },
            )
            .unwrap();
        }
        fs::write(root.join("Cargo.lock"), "version=4\npackage=[]\n").unwrap();
        let graph = json!({"packages":[
            {"id":"gui","name":"paseo-gui","source":null,"manifest_path":root.join("packages/gui/Cargo.toml")},
            {"id":"daemon","name":"paseo-daemon","source":null,"manifest_path":root.join("packages/daemon/Cargo.toml")},
            {"id":"client","name":"paseo-client","source":null,"manifest_path":root.join("packages/client/Cargo.toml")},
            {"id":"tailnet","name":"paseo-tailnet","source":null,"manifest_path":root.join("packages/tailnet/Cargo.toml")},
            {"id":"platform","name":"paseo-gui-platform","source":null,"manifest_path":root.join("packages/gui-platform/Cargo.toml")}
        ],"workspace_members":["gui","daemon","client","tailnet","platform"],"resolve":{"nodes":[
            {"id":"gui","features":[],"deps":[{"pkg":"client","dep_kinds":[{"kind":null}]},{"pkg":"platform","dep_kinds":[{"kind":null}]}]},
            {"id":"daemon","features":[],"deps":[{"pkg":"client","dep_kinds":[{"kind":"build"}]}]},
            {"id":"client","features":[],"deps":[{"pkg":"tailnet","dep_kinds":[{"kind":null}]}]},
            {"id":"tailnet","features":[],"deps":[]},
            {"id":"platform","features":[],"deps":[]}
        ]}});
        let hashes = |target: &str| {
            supported(target)
                .into_iter()
                .map(|id| {
                    let (paths, context) =
                        component_inputs(root, &all, &graph, target, id, None).unwrap();
                    (id, fingerprint(root, &paths, &context).unwrap())
                })
                .collect::<BTreeMap<_, _>>()
        };
        let before = hashes("linux-x64");
        let package_before = package_hash(root, &all, "linux-x64").unwrap();
        for (path, expected) in [
            ("docs/guide.md", vec![]),
            ("Cargo.toml", vec![ComponentId::App, ComponentId::Daemon]),
            (
                "packages/tailnet/native/engine.go",
                vec![ComponentId::Tailnet],
            ),
            ("packages/tailnet/native/engine_test.go", vec![]),
            (
                "packages/app-updater/main.go",
                vec![ComponentId::UpdateHelper],
            ),
            ("packages/app-updater/main_test.go", vec![]),
            ("packages/gui/src/main.rs", vec![ComponentId::App]),
            ("packages/daemon/src/main.rs", vec![ComponentId::Daemon]),
            (
                "packages/client/src/lib.rs",
                vec![ComponentId::App, ComponentId::Daemon],
            ),
            ("skills/help/SKILL.md", vec![ComponentId::Daemon]),
            ("packaging/linux/control", vec![]),
        ] {
            let original = fs::read(root.join(path)).unwrap();
            fs::write(root.join(path), "changed").unwrap();
            let changed: Vec<_> = hashes("linux-x64")
                .into_iter()
                .filter(|(id, hash)| before[id] != *hash)
                .map(|(id, _)| id)
                .collect();
            assert_eq!(changed, expected, "{path}");
            assert_eq!(
                package_hash(root, &all, "linux-x64").unwrap() != package_before,
                path.starts_with("packaging/")
            );
            fs::write(root.join(path), original).unwrap();
        }
        for target in ["linux-x64", "android-arm64", "ios-arm64"] {
            let before = hashes(target);
            for path in ["xtask/src/gui.rs", "packages/gui-platform/ios/Info.plist"] {
                let original = fs::read(root.join(path)).unwrap();
                fs::write(root.join(path), "changed mobile recipe").unwrap();
                let changed: Vec<_> = hashes(target)
                    .into_iter()
                    .filter(|(id, hash)| before[id] != *hash)
                    .map(|(id, _)| id)
                    .collect();
                let tailnet = target.starts_with("ios-")
                    || (target.starts_with("android-") && path == "xtask/src/gui.rs");
                let expected = if tailnet {
                    vec![ComponentId::App, ComponentId::Tailnet]
                } else {
                    vec![ComponentId::App]
                };
                assert_eq!(changed, expected, "{target}: {path}");
                fs::write(root.join(path), original).unwrap();
            }
        }
        for (path, tailnet, helper, native) in [
            ("packages/tailnet/native/engine.go", true, false, true),
            ("packages/tailnet/native/engine_test.go", true, false, false),
            ("packages/app-updater/main.go", false, true, true),
            ("packages/app-updater/main_test.go", false, true, false),
        ] {
            let changed = BTreeSet::from([path.to_owned()]);
            let plan = ci_decisions_for_inputs(root, &changed, &graph, &all).unwrap();
            assert_eq!(plan["packages"], json!([]), "{path}");
            assert_eq!(plan["rust"], false, "{path}");
            assert_eq!(plan["gui"], false, "{path}");
            assert_eq!(plan["tailnet"], tailnet, "{path}");
            assert_eq!(plan["helper"], helper, "{path}");
            assert_eq!(plan["abi"], tailnet, "{path}");
            assert_eq!(plan["native"], native, "{path}");
        }
        let changed = BTreeSet::from(["packages/tailnet/src/lib.rs".to_owned()]);
        let plan = ci_decisions_for_inputs(root, &changed, &graph, &all).unwrap();
        assert_eq!(
            plan["packages"],
            json!(["paseo-client", "paseo-daemon", "paseo-gui", "paseo-tailnet"])
        );
        assert_eq!(plan["rust"], true);
        assert_eq!(plan["gui"], true);
    }
}
