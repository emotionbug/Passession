use super::plan::{self, Plan, TargetPlan};
use crate::{app_updater, build as daemon, common::*, gui, libraries, package, signing, tailnet};
use anyhow::{Context, Result, bail, ensure};
use paseo_protocol::components::{
    ArtifactRef, ComponentId, ComponentIdentity, ComponentRef, PackageRef, TargetInventory,
    validate_component, validate_relative_image, validate_target_inventory,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

const REPOSITORY: &str = "emotionbug/Passession";
const MAX_EXPANDED: u64 = 8 * 1024 * 1024 * 1024;
const MAX_MEMBERS: usize = 200_000;

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Descriptor {
    schema_version: u32,
    id: ComponentId,
    target: String,
    identity: ComponentIdentity,
    executable: String,
    executable_sha256: String,
    requires: BTreeMap<ComponentId, u32>,
    licenses: Vec<String>,
    dependencies: BTreeMap<String, String>,
}

fn descriptor_name(id: ComponentId) -> String {
    format!("component-build-{id}.json")
}

fn selected(target: &TargetPlan, mut compile: impl FnMut(ComponentId) -> Result<()>) -> Result<()> {
    // The app may link/embed the already selected native components. Compilation
    // is never expanded here: only the persisted plan may expand input closures.
    for id in [
        ComponentId::Tailnet,
        ComponentId::Daemon,
        ComponentId::UpdateHelper,
        ComponentId::App,
    ] {
        if target.build.contains(&id) {
            compile(id)?;
        }
    }
    Ok(())
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct HandoffFile {
    sha256: String,
    executable: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct HandoffComponent {
    identity: ComponentIdentity,
    files: BTreeMap<String, HandoffFile>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Handoff {
    schema_version: u32,
    target: String,
    version: String,
    source_sha: String,
    plan_digest: String,
    components: BTreeMap<ComponentId, HandoffComponent>,
}

fn plan_digest(plan: &Plan) -> Result<String> {
    use sha2::{Digest, Sha256};
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(plan)?)))
}

fn compiled_identity(
    plan: &Plan,
    target: &TargetPlan,
    id: ComponentId,
) -> Result<ComponentIdentity> {
    Ok(ComponentIdentity {
        version: plan.version.clone(),
        source_sha: plan.source_sha.clone(),
        input_hash: target
            .inputs
            .get(&id)
            .context("Plan lacks a selected component fingerprint")?
            .clone(),
        abi_version: id.initial_abi_version(),
    })
}

fn handoff_files(root: &Path) -> Result<BTreeMap<String, HandoffFile>> {
    let mut files = BTreeMap::new();
    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .sort_by_file_name()
    {
        let entry = entry?;
        ensure!(
            entry.file_type().is_dir() || entry.file_type().is_file(),
            "Handoff contains a link or non-regular member: {}",
            entry.path().display()
        );
        if entry.file_type().is_dir() {
            continue;
        }
        let name = entry
            .path()
            .strip_prefix(root)?
            .to_str()
            .context("Non-UTF-8 handoff path")?
            .replace('\\', "/");
        safe_member(&name)?;
        #[cfg(unix)]
        let executable = {
            use std::os::unix::fs::PermissionsExt;
            entry.metadata()?.permissions().mode() & 0o111 != 0
        };
        #[cfg(not(unix))]
        let executable = false;
        files.insert(
            name,
            HandoffFile {
                sha256: sha256_file(entry.path())?,
                executable,
            },
        );
    }
    Ok(files)
}

fn write_handoff(
    output: &Path,
    plan: &Plan,
    target: &TargetPlan,
    objects: &BTreeMap<ComponentId, PathBuf>,
) -> Result<()> {
    ensure!(
        !output.exists(),
        "Handoff destination already exists: {}",
        output.display()
    );
    let parent = output.parent().context("Missing handoff parent")?;
    fs::create_dir_all(parent)?;
    let temporary = tempfile::Builder::new()
        .prefix(".handoff-")
        .tempdir_in(parent)?;
    let mut components = BTreeMap::new();
    for id in &target.build {
        let object = temporary.path().join(format!("objects/{id}"));
        copy_tree(&objects[id], &object)?;
        components.insert(
            *id,
            HandoffComponent {
                identity: compiled_identity(plan, target, *id)?,
                files: handoff_files(&object)?,
            },
        );
    }
    let manifest = Handoff {
        schema_version: 1,
        target: target.target.clone(),
        version: plan.version.clone(),
        source_sha: plan.source_sha.clone(),
        plan_digest: plan_digest(plan)?,
        components,
    };
    write_json(
        &temporary.path().join("handoff.json"),
        &serde_json::to_value(manifest)?,
    )?;
    fs::rename(temporary.path(), output)?;
    Ok(())
}

fn verify_handoff(root: &Path, plan: &Plan, target: &TargetPlan) -> Result<Handoff> {
    let path = root.join("handoff.json");
    ensure!(
        fs::metadata(&path)?.len() <= 32 * 1024 * 1024,
        "Handoff manifest exceeds limit"
    );
    let manifest: Handoff = serde_json::from_slice(&fs::read(path)?)?;
    ensure!(
        manifest.schema_version == 1
            && manifest.target == target.target
            && manifest.version == plan.version
            && manifest.source_sha == plan.source_sha
            && manifest.plan_digest == plan_digest(plan)?,
        "Handoff does not match the persisted plan"
    );
    ensure!(
        manifest.components.keys().copied().collect::<BTreeSet<_>>()
            == target.build.iter().copied().collect(),
        "Handoff component selection does not match plan"
    );
    let mut expected = BTreeMap::new();
    for (id, component) in &manifest.components {
        ensure!(
            component.identity == compiled_identity(plan, target, *id)?,
            "Handoff compiled identity does not match plan"
        );
        ensure!(!component.files.is_empty(), "Empty handoff component");
        for (name, file) in &component.files {
            safe_member(name)?;
            expected.insert(format!("objects/{id}/{name}"), &file.sha256);
        }
    }
    // Permissions are restored from the manifest after transfer: artifact upload
    // services can discard Unix executable bits. File bytes remain immutable.
    let actual = handoff_files(root)?;
    let actual: BTreeMap<_, _> = actual
        .iter()
        .filter(|(name, _)| name.as_str() != "handoff.json")
        .map(|(name, file)| (name.clone(), &file.sha256))
        .collect();
    ensure!(
        actual == expected,
        "Handoff contains changed, missing or extra files"
    );
    Ok(manifest)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Both,
    Compile,
    Finalize,
}

pub(crate) fn run(args: &[String]) -> Result<()> {
    let mut path = None;
    let mut target_name = None;
    let mut step = Stage::Both;
    let mut handoff = None;
    let mut options = args.chunks_exact(2);
    for pair in &mut options {
        match pair[0].as_str() {
            "--plan" if path.is_none() => path = Some(pair[1].as_str()),
            "--target" if target_name.is_none() => target_name = Some(pair[1].as_str()),
            "--stage" if step == Stage::Both => {
                step = match pair[1].as_str() {
                    "compile" => Stage::Compile,
                    "finalize" => Stage::Finalize,
                    _ => bail!("Expected --stage compile|finalize"),
                }
            }
            "--handoff" if handoff.is_none() => handoff = Some(absolute(&pair[1])?),
            _ => bail!("Unknown or repeated component build option {}", pair[0]),
        }
    }
    ensure!(
        options.remainder().is_empty(),
        "Component build options need values"
    );
    let path = path.context("Expected --plan PATH")?;
    let target_name = target_name.context("Expected --target ID")?;
    let build_target = BuildTarget::parse(target_name)?;
    // Select the target before any worker threads exist, without mutating the
    // process environment. Compiler children inherit the same explicit identity.
    if matches!(build_target.platform, "linux" | "windows" | "macos")
        && env::var("PASEO_BUILD_TARGET").as_deref() != Ok(target_name)
    {
        return crate::common::run(
            command(env::current_exe()?)?
                .args(["components", "build"])
                .args(args)
                .env("PASEO_BUILD_TARGET", target_name),
        );
    }
    let plan = plan::read(Path::new(path))?;
    let target = plan
        .targets
        .iter()
        .find(|entry| entry.target == target_name)
        .context("Target is not in the persisted plan")?;
    plan::validate_inputs(&plan, target)?;
    if target.build.is_empty() && !target.package {
        println!("{}: no component or container changes", target.target);
        return Ok(());
    }
    ensure!(
        !plan.no_changes,
        "A no-change plan cannot request native work"
    );
    let (os, cpu) = target.target.split_once('-').context("Invalid target")?;
    if matches!(os, "linux" | "macos" | "windows") {
        let native = os == host_platform()? && cpu == host_arch()?;
        ensure!(
            native
                || step == Stage::Compile
                    && host_platform()? == "linux"
                    && matches!(os, "macos" | "windows"),
            "Finalization and combined builds require the exact native target host; cross compilation requires --stage compile on Linux"
        );
    } else if step == Stage::Finalize {
        ensure!(
            host_platform()? == if os == "ios" { "macos" } else { "linux" },
            "Mobile finalization requires its native packaging host"
        );
    }
    let handoff = handoff.unwrap_or(root()?.join(format!("artifacts/handoff-{}", target.target)));
    let _lease = crate::build_cache::acquire()?;
    supervise_subprocesses()?;
    fs::create_dir_all(release()?)?;
    let work = root()?.join(".dev/build/components");
    fs::create_dir_all(&work)?;
    let stage = tempfile::Builder::new()
        .prefix("target-")
        .tempdir_in(&work)?;
    let mut objects = BTreeMap::new();
    let mut refs = target.reuse.clone();
    let verified = if step == Stage::Finalize {
        Some(verify_handoff(&handoff, &plan, target)?)
    } else {
        None
    };
    let needs_reuse = step != Stage::Compile
        && (target.package
            || target.build.contains(&ComponentId::App) && matches!(os, "macos" | "ios"));
    if needs_reuse {
        let mut extracted = BTreeMap::<String, PathBuf>::new();
        for (id, reference) in &target.reuse {
            let object = if let Some(existing) = extracted.get(&reference.artifact.sha256) {
                existing.clone()
            } else {
                let object = stage.path().join(format!("reuse-{id}"));
                let archive = fetch(reference)?;
                extract(&archive, &object, &reference.artifact.format)?;
                extracted.insert(reference.artifact.sha256.clone(), object.clone());
                object
            };
            verify_object(&object, reference)?;
            objects.insert(*id, object);
        }
    }
    if let Some(manifest) = verified {
        for (id, component) in &manifest.components {
            let object = stage.path().join(format!("build-{id}"));
            copy_tree(&handoff.join(format!("objects/{id}")), &object)?;
            for (name, file) in &component.files {
                chmod(
                    &object.join(name),
                    if file.executable { 0o755 } else { 0o644 },
                )?;
            }
            objects.insert(*id, object);
        }
    } else {
        selected(target, |id| {
            let identity = compiled_identity(&plan, target, id)?;
            let object = stage.path().join(format!("build-{id}"));
            fs::create_dir(&object)?;
            let mut process = command(env::current_exe()?)?;
            process
                .args(["components", "compile", &target.target, id.as_str()])
                .arg(&object)
                .env("PASEO_NATIVE_RELEASE", "1")
                .env("PASEO_RELEASE_CHANNEL", &plan.channel)
                .env("PASEO_UPDATE_REPOSITORY", REPOSITORY)
                .env("PASEO_VERSION", &identity.version)
                .env("PASEO_DAEMON_VERSION", &identity.version)
                .env("PASEO_BUILD_SOURCE_SHA", &identity.source_sha)
                .env("PASEO_COMPONENT_VERSION", &identity.version)
                .env("PASEO_COMPONENT_SOURCE_SHA", &identity.source_sha)
                .env("PASEO_COMPONENT_INPUT_HASH", &identity.input_hash)
                .env("PASEO_COMPONENT_ABI", identity.abi_version.to_string());
            if matches!(os, "linux" | "windows" | "macos") {
                process.env("PASEO_BUILD_TARGET", &target.target);
            }
            if os == "ios" && id == ComponentId::App {
                process.arg(
                    objects
                        .get(&ComponentId::Tailnet)
                        .context("iOS requires a staged tailnet component")?,
                );
            }
            crate::common::run(&mut process)?;
            write_json(
                &object.join("compiled-identity.json"),
                &serde_json::to_value(&identity)?,
            )?;
            objects.insert(id, object);
            Ok(())
        })?;
        if step == Stage::Compile {
            write_handoff(&handoff, &plan, target, &objects)?;
            return Ok(());
        }
    }
    selected(target, |id| {
        let object = &objects[&id];
        finish_component(object, os, id)?;
        if os == "macos" && matches!(id, ComponentId::Tailnet | ComponentId::App) {
            return Ok(());
        }
        remove_if_exists(&object.join("compiled-identity.json"))?;
        let reference = archive_component(
            object,
            &plan,
            target,
            id,
            compiled_identity(&plan, target, id)?,
        )?;
        refs.insert(id, reference);
        Ok(())
    })?;
    let mut inventory = TargetInventory {
        components: refs,
        packages: target.packages.clone(),
        atomic_groups: target.atomic_groups.clone(),
    };
    if os == "macos"
        && (target.package
            || target
                .build
                .iter()
                .any(|id| matches!(id, ComponentId::App | ComponentId::Tailnet)))
    {
        let cohort = finalize_mac(&plan, target, &mut inventory, &mut objects, stage.path())?;
        inventory.packages.retain(|package| {
            !package.components.contains_key(&ComponentId::App)
                || package.artifact
                    != target
                        .reuse
                        .get(&ComponentId::App)
                        .map(|reference| reference.artifact.clone())
                        .unwrap_or_else(|| cohort.artifact.clone())
        });
        inventory.packages.push(cohort);
    }
    if target.package {
        let containers = package_target(&plan, target, &inventory, &objects, stage.path())?;
        inventory.packages.retain(|package| {
            inventory
                .components
                .values()
                .any(|reference| reference.artifact == package.artifact)
        });
        inventory.packages.extend(containers);
    }
    validate_target_inventory(&inventory, &target.target, REPOSITORY)?;
    write_json(
        &release()?.join(format!("components-{}.json", target.target)),
        &serde_json::to_value(inventory)?,
    )?;
    Ok(())
}

/// Re-sign only the mandatory OS container; an unchanged compiled identity is
/// copied from its authenticated ref, never manufactured from this collection.
fn finalize_mac(
    plan: &Plan,
    target: &TargetPlan,
    inventory: &mut TargetInventory,
    objects: &mut BTreeMap<ComponentId, PathBuf>,
    stage: &Path,
) -> Result<PackageRef> {
    let cohort = stage.join("signed-app");
    fs::create_dir(&cohort)?;
    let old_app = objects
        .get(&ComponentId::App)
        .context("Missing selected app object")?;
    copy_tree(
        &old_app.join(gui::MACOS_BUNDLE),
        &cohort.join(gui::MACOS_BUNDLE),
    )?;
    let app = cohort.join(gui::MACOS_BUNDLE);
    let bin = app.join("Contents/MacOS");
    let native = objects
        .get(&ComponentId::Tailnet)
        .context("Missing selected tailnet object")?;
    let native_image = if target.build.contains(&ComponentId::Tailnet) {
        native.join(tailnet::library_name("macos")?)
    } else {
        native.join(&inventory.components[&ComponentId::Tailnet].executable)
    };
    copy_tree(&native_image, &bin.join(tailnet::library_name("macos")?))?;
    let helper = objects
        .get(&ComponentId::UpdateHelper)
        .context("Missing bootstrap helper")?;
    copy_tree(
        &helper.join(app_updater::executable_name("macos")),
        &bin.join(app_updater::executable_name("macos")),
    )?;
    let runtime = app.join(gui::desktop_runtime("macos"));
    remove_if_exists(&runtime)?;
    copy_tree(
        objects
            .get(&ComponentId::Daemon)
            .context("Missing bootstrap daemon")?,
        &runtime,
    )?;
    // This derived fallback is part of the app seal, not the independently
    // shipped daemon object. Preserve its compiled identity, update only role.
    remove_if_exists(&runtime.join(descriptor_name(ComponentId::Daemon)))?;
    for name in ["paseo-runtime.json", "bundle-provenance.json"] {
        let mut metadata = read_json(&runtime.join(name))?;
        metadata["installationKind"] = json!("desktop-bundled");
        write_json(&runtime.join(name), &metadata)?;
    }
    let notices = app.join("Contents/Resources/component-notices");
    copy_tree(
        &helper.join("THIRD-PARTY-NOTICES.txt"),
        &notices.join("update-helper.txt"),
    )?;
    if target.build.contains(&ComponentId::Tailnet) {
        copy_tree(
            &native.join("THIRD-PARTY-NOTICES.txt"),
            &notices.join("tailnet.txt"),
        )?;
    } else {
        copy_tree(
            &native.join("Passession.app/Contents/Resources/component-notices/tailnet.txt"),
            &notices.join("tailnet.txt"),
        )?;
    }
    for (key, value) in [
        ("CFBundleShortVersionString", plan.version.clone()),
        ("CFBundleVersion", build_number("macos")?.to_string()),
    ] {
        crate::common::run(
            command("/usr/libexec/PlistBuddy")?
                .args(["-c", &format!("Set :{key} {value}")])
                .arg(app.join("Contents/Info.plist")),
        )?;
    }
    signing::sign_mac(&app, stage, true)?;
    let mut descriptors = Vec::new();
    for id in [ComponentId::App, ComponentId::Tailnet] {
        let identity = if target.build.contains(&id) {
            serde_json::from_value(read_json(&objects[&id].join("compiled-identity.json"))?)?
        } else {
            inventory.components[&id].identity.clone()
        };
        let descriptor = describe(&cohort, &target.target, id, identity)?;
        write_json(
            &cohort.join(descriptor_name(id)),
            &serde_json::to_value(&descriptor)?,
        )?;
        descriptors.push(descriptor);
    }
    let output = release()?.join(asset_name(
        ComponentId::App,
        &plan.version,
        &target.target,
        "tar.gz",
    ));
    package::archive(&cohort, &output, false)?;
    let artifact = artifact(&output, &plan.version, "tar.gz")?;
    let identities = descriptors
        .iter()
        .map(|descriptor| (descriptor.id, descriptor.identity.clone()))
        .collect();
    for descriptor in descriptors {
        objects.insert(descriptor.id, cohort.clone());
        inventory
            .components
            .insert(descriptor.id, component_ref(descriptor, artifact.clone()));
    }
    Ok(PackageRef {
        target: target.target.clone(),
        version: plan.version.clone(),
        source_sha: plan.source_sha.clone(),
        artifact,
        components: identities,
    })
}

/// Child process boundary: component compiler stamps cannot leak to another
/// component's Cargo graph or native cache. No implicit dependency compilation.
pub(crate) fn compile(args: &[String]) -> Result<()> {
    ensure!(
        args.len() == 3 || args.len() == 4,
        "Internal component compiler arguments invalid"
    );
    let (os, cpu) = args[0]
        .split_once('-')
        .context("Invalid component target")?;
    let id: ComponentId = args[1].parse()?;
    let identity =
        super::compiler_stamps()?.context("Component compilation needs explicit stamps")?;
    ensure!(
        identity.abi_version == id.initial_abi_version(),
        "Unexpected component ABI stamp"
    );
    let output = fs::canonicalize(&args[2])?;
    ensure!(
        output.starts_with(fs::canonicalize(root()?.join(".dev/build/components"))?),
        "Component output is outside controlled staging"
    );
    ensure!(
        fs::read_dir(&output)?.next().is_none(),
        "Component staging must be empty"
    );
    if matches!(os, "linux" | "windows" | "macos") {
        ensure!(
            BuildTarget::current()? == BuildTarget::parse(&args[0])?,
            "Compiler target selection disagrees with component plan"
        );
    }
    match id {
        ComponentId::Daemon => {
            ensure!(
                matches!(os, "linux" | "windows" | "macos"),
                "Mobile packages never contain daemons"
            );
            daemon::build_native("release", false)?;
            daemon::bundle_raw_runtime(&output, "release", "standalone-archive")?;
            write_json(
                &output.join("desktop-runtime.json"),
                &json!({"schemaVersion":1,"executable":executable(),"cliShimPath":executable(),"workingDirectory":".","environment":{}}),
            )?;
        }
        ComponentId::UpdateHelper => {
            let built = app_updater::desktop(os)?;
            copy_tree(&built, &output.join(app_updater::executable_name(os)))?;
            copy_tree(
                &built.with_file_name("THIRD-PARTY-NOTICES.txt"),
                &output.join("THIRD-PARTY-NOTICES.txt"),
            )?;
            copy_tree(&root()?.join("LICENSE"), &output.join("LICENSE"))?;
        }
        ComponentId::Tailnet if matches!(os, "linux" | "macos" | "windows") => {
            let built = tailnet::desktop(os, "13.0")?;
            copy_tree(&built.library, &output.join(tailnet::library_name(os)?))?;
            text(
                &output.join("THIRD-PARTY-NOTICES.txt"),
                built.notices,
                0o644,
            )?;
            copy_tree(&root()?.join("LICENSE"), &output.join("LICENSE"))?;
        }
        ComponentId::App | ComponentId::Tailnet => {
            gui::component_compile(os, cpu, id, &output, args.get(3).map(Path::new))?;
        }
    }
    Ok(())
}

fn finish_component(object: &Path, os: &str, id: ComponentId) -> Result<()> {
    match id {
        ComponentId::Daemon => {
            daemon::finish_standalone_runtime(object, object.parent().context("Missing stage")?)
        }
        ComponentId::UpdateHelper => finish_image(
            object,
            &object.join(app_updater::executable_name(os)),
            os,
            false,
        ),
        ComponentId::Tailnet if matches!(os, "linux" | "windows") => {
            let image = object.join(tailnet::library_name(os)?);
            if os == "windows" {
                signing::require_system_imports(&image)?;
            }
            finish_image(object, &image, os, false)
        }
        ComponentId::App if matches!(os, "linux" | "windows") => {
            finish_image(object, &object.join(gui::desktop_executable(os)), os, true)
        }
        _ => Ok(()),
    }
}

fn finish_image(root: &Path, image: &Path, os: &str, app: bool) -> Result<()> {
    match os {
        "linux" if app => libraries::component_app(root)?,
        "linux" => libraries::native(root)?,
        "windows" => {
            if app {
                signing::windows_runtime(root, true)?;
            }
            signing::sign_windows(
                &[image.to_path_buf()],
                root.parent().context("Missing stage")?,
            )?;
        }
        "macos" => signing::sign_mac(root, root.parent().context("Missing stage")?, false)?,
        _ => {}
    }
    Ok(())
}

fn image(id: ComponentId, target: &str) -> Result<String> {
    let (os, cpu) = target.split_once('-').context("Invalid target")?;
    Ok(match (id, os) {
        (ComponentId::App, "macos") => {
            format!("{}/{}", gui::MACOS_BUNDLE, gui::desktop_executable(os))
        }
        (ComponentId::Tailnet, "macos") => format!(
            "{}/Contents/MacOS/{}",
            gui::MACOS_BUNDLE,
            tailnet::library_name(os)?
        ),
        (ComponentId::App, "ios") => "Passession.app/paseo-sessions".into(),
        (ComponentId::App | ComponentId::Tailnet, "android") => format!(
            "jniLibs/{}/{}",
            if cpu == "arm64" {
                "arm64-v8a"
            } else {
                "x86_64"
            },
            if id == ComponentId::App {
                "libmakepad.so"
            } else {
                "libpaseo_tailscale.so"
            }
        ),
        (ComponentId::App, _) => gui::desktop_executable(os).into(),
        (ComponentId::Daemon, "windows") => "paseo.exe".into(),
        (ComponentId::Daemon, _) => "paseo".into(),
        (ComponentId::Tailnet, _) => tailnet::library_name(os)?.into(),
        (ComponentId::UpdateHelper, _) => app_updater::executable_name(os).into(),
    })
}

fn requirements(id: ComponentId, target: &str) -> BTreeMap<ComponentId, u32> {
    if id != ComponentId::App {
        return BTreeMap::new();
    }
    let mut ids = vec![ComponentId::Tailnet];
    if !target.starts_with("android-") && !target.starts_with("ios-") {
        ids.extend([ComponentId::Daemon, ComponentId::UpdateHelper]);
    }
    ids.into_iter()
        .map(|id| (id, id.initial_abi_version()))
        .collect()
}

fn describe(
    root: &Path,
    target: &str,
    id: ComponentId,
    identity: ComponentIdentity,
) -> Result<Descriptor> {
    let executable = image(id, target)?;
    let mut dependencies = BTreeMap::new();
    let mut licenses = Vec::new();
    for entry in walkdir::WalkDir::new(root)
        .min_depth(1)
        .follow_links(false)
        .sort_by_file_name()
    {
        let entry = entry?;
        ensure!(
            entry.file_type().is_dir() || entry.file_type().is_file(),
            "Component contains non-regular payload"
        );
        if entry.file_type().is_dir() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(root)?
            .to_str()
            .context("Non-UTF-8 component path")?
            .replace('\\', "/");
        if relative.starts_with("component-build-") && relative.ends_with(".json") {
            continue;
        }
        validate_relative_image(&relative)?;
        let lower = relative.to_ascii_lowercase();
        if lower.contains("license") || lower.contains("notice") || lower.ends_with("copyright") {
            licenses.push(relative.clone());
        }
        if relative != executable {
            dependencies.insert(relative, sha256_file(entry.path())?);
        }
    }
    ensure!(
        !licenses.is_empty(),
        "Component is missing its license inventory"
    );
    Ok(Descriptor {
        schema_version: 1,
        id,
        target: target.into(),
        identity,
        executable_sha256: sha256_file(&root.join(&executable))?,
        executable,
        requires: requirements(id, target),
        licenses,
        dependencies,
    })
}

fn component_ref(descriptor: Descriptor, artifact: ArtifactRef) -> ComponentRef {
    ComponentRef {
        id: descriptor.id,
        target: descriptor.target,
        identity: descriptor.identity,
        artifact,
        executable: descriptor.executable,
        executable_sha256: descriptor.executable_sha256,
        requires: descriptor.requires,
    }
}
fn asset_name(id: ComponentId, version: &str, target: &str, format: &str) -> String {
    format!("Paseo-Component-{id}-{version}-{target}.{format}")
}
fn artifact(path: &Path, version: &str, format: &str) -> Result<ArtifactRef> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("Invalid artifact filename")?;
    Ok(ArtifactRef {
        url: format!("https://github.com/{REPOSITORY}/releases/download/v{version}/{name}"),
        sha256: sha256_file(path)?,
        size: fs::metadata(path)?.len(),
        format: format.into(),
        origin_tag: format!("v{version}"),
    })
}
fn archive_component(
    root: &Path,
    plan: &Plan,
    target: &TargetPlan,
    id: ComponentId,
    identity: ComponentIdentity,
) -> Result<ComponentRef> {
    let descriptor = describe(root, &target.target, id, identity)?;
    write_json(
        &root.join(descriptor_name(id)),
        &serde_json::to_value(&descriptor)?,
    )?;
    let format = if target.target.starts_with("windows-") {
        "zip"
    } else {
        "tar.gz"
    };
    let output = release()?.join(asset_name(id, &plan.version, &target.target, format));
    package::archive(root, &output, format == "zip")?;
    let reference = component_ref(descriptor, artifact(&output, &plan.version, format)?);
    validate_component(&reference, &target.target, REPOSITORY)?;
    Ok(reference)
}

fn verify_object(root: &Path, reference: &ComponentRef) -> Result<()> {
    validate_component(reference, &reference.target, REPOSITORY)?;
    let path = root.join(descriptor_name(reference.id));
    ensure!(
        fs::metadata(&path)?.len() <= 32 * 1024 * 1024,
        "Component descriptor exceeds limit"
    );
    let descriptor: Descriptor = serde_json::from_slice(&fs::read(path)?)?;
    let expected = describe(
        root,
        &reference.target,
        reference.id,
        reference.identity.clone(),
    )?;
    ensure!(
        descriptor == expected,
        "Reused component file inventory does not match its descriptor"
    );
    ensure!(
        descriptor.executable == reference.executable
            && descriptor.executable_sha256 == reference.executable_sha256
            && descriptor.requires == reference.requires,
        "Reused component does not match authenticated inventory"
    );
    Ok(())
}

fn fetch(reference: &ComponentRef) -> Result<PathBuf> {
    validate_component(reference, &reference.target, REPOSITORY)?;
    let directory = root()?.join(".dev/build/components/downloads");
    fs::create_dir_all(&directory)?;
    let output = directory.join(&reference.artifact.sha256);
    if output.is_file() {
        ensure!(
            fs::metadata(&output)?.len() == reference.artifact.size
                && sha256_file(&output)? == reference.artifact.sha256,
            "Cached component archive is corrupted"
        );
        return Ok(output);
    }
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            let url = attempt.url();
            if attempt.previous().len() < 8
                && url.scheme() == "https"
                && url.username().is_empty()
                && url.password().is_none()
                && url.port().is_none()
                && matches!(
                    url.host_str(),
                    Some(
                        "github.com"
                            | "release-assets.githubusercontent.com"
                            | "objects.githubusercontent.com"
                    )
                )
            {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .build()?;
    let response = client
        .get(&reference.artifact.url)
        .send()?
        .error_for_status()?;
    ensure!(
        !response.status().is_redirection(),
        "Untrusted component download redirect"
    );
    let mut temporary = tempfile::NamedTempFile::new_in(&directory)?;
    let copied = std::io::copy(
        &mut response.take(
            reference
                .artifact
                .size
                .checked_add(1)
                .context("Artifact size overflow")?,
        ),
        temporary.as_file_mut(),
    )?;
    temporary.flush()?;
    ensure!(
        copied == reference.artifact.size
            && sha256_file(temporary.path())? == reference.artifact.sha256,
        "Downloaded component hash/size mismatch"
    );
    temporary.persist(&output).map_err(|error| error.error)?;
    Ok(output)
}

fn safe_member(name: &str) -> Result<&str> {
    let name = name.trim_end_matches('/');
    validate_relative_image(name)?;
    ensure!(
        !name.contains('\\') && !name.contains(':'),
        "Unsafe component archive path"
    );
    Ok(name)
}

fn extract(archive: &Path, output: &Path, format: &str) -> Result<()> {
    ensure!(!output.exists(), "Extraction destination already exists");
    fs::create_dir(output)?;
    let mut seen = BTreeSet::new();
    let mut expanded = 0u64;
    let mut member =
        |name: &str, size: u64, directory: bool, mode: u32, reader: &mut dyn Read| -> Result<()> {
            let name = safe_member(name)?;
            ensure!(
                seen.len() < MAX_MEMBERS && seen.insert(name.to_owned()),
                "Too many or duplicate component archive entries"
            );
            expanded = expanded
                .checked_add(size)
                .context("Expanded size overflow")?;
            ensure!(
                expanded <= MAX_EXPANDED,
                "Component archive expands beyond limit"
            );
            let path = output.join(name);
            if directory {
                fs::create_dir_all(&path)?;
            } else {
                fs::create_dir_all(path.parent().context("Missing member parent")?)?;
                let mut file = fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&path)?;
                let copied = std::io::copy(&mut reader.take(size + 1), &mut file)?;
                ensure!(copied == size, "Archive member size mismatch");
                chmod(&path, if mode & 0o111 != 0 { 0o755 } else { 0o644 })?;
            }
            Ok(())
        };
    match format {
        "tar.gz" => {
            let mut archive =
                tar::Archive::new(flate2::read::GzDecoder::new(fs::File::open(archive)?));
            for entry in archive.entries()? {
                let mut entry = entry?;
                let kind = entry.header().entry_type();
                ensure!(
                    kind.is_file() || kind.is_dir(),
                    "Component archive links/devices are forbidden"
                );
                let name = entry
                    .path()?
                    .to_str()
                    .context("Non-UTF-8 tar path")?
                    .to_owned();
                let size = entry.size();
                let mode = entry.header().mode()?;
                member(&name, size, kind.is_dir(), mode, &mut entry)?;
            }
        }
        "zip" => {
            let mut archive = zip::ZipArchive::new(fs::File::open(archive)?)?;
            for index in 0..archive.len() {
                let mut entry = archive.by_index(index)?;
                let mode = entry.unix_mode().unwrap_or(0o644);
                ensure!(
                    matches!(mode & 0o170000, 0 | 0o100000 | 0o040000),
                    "Component archive links/devices are forbidden"
                );
                let name = entry.name().to_owned();
                let size = entry.size();
                let directory = entry.is_dir();
                member(&name, size, directory, mode, &mut entry)?;
            }
        }
        _ => bail!("Unsupported component archive format: {format}"),
    }
    Ok(())
}

fn package_target(
    plan: &Plan,
    target: &TargetPlan,
    inventory: &TargetInventory,
    objects: &BTreeMap<ComponentId, PathBuf>,
    stage: &Path,
) -> Result<Vec<PackageRef>> {
    let (os, cpu) = target
        .target
        .split_once('-')
        .context("Invalid package target")?;
    let object = |id| {
        objects
            .get(&id)
            .context("Package requires every selected component object")
    };
    let files = if matches!(os, "android" | "ios") {
        gui::component_package_mobile(
            os,
            cpu,
            object(ComponentId::App)?,
            object(ComponentId::Tailnet)?,
            &plan.version,
            &plan.source_sha,
            &plan.channel,
        )?
    } else {
        let payload = stage.join("installer");
        fs::create_dir(&payload)?;
        copy_tree(object(ComponentId::App)?, &payload)?;
        let mut roots = BTreeMap::new();
        roots.insert(ComponentId::App, ".");
        roots.insert(ComponentId::Daemon, "runtime");
        copy_tree(object(ComponentId::Daemon)?, &payload.join("runtime"))?;
        if os == "macos" {
            roots.insert(ComponentId::Tailnet, ".");
            roots.insert(ComponentId::UpdateHelper, "update-helper");
            copy_tree(
                object(ComponentId::UpdateHelper)?,
                &payload.join("update-helper"),
            )?;
        } else {
            for (id, relative) in [
                (ComponentId::Tailnet, "tailnet"),
                (ComponentId::UpdateHelper, "update-helper"),
            ] {
                roots.insert(id, relative);
                copy_tree(object(id)?, &payload.join(relative))?;
            }
        }
        let cohort_packages: Vec<_> = inventory
            .packages
            .iter()
            .filter(|package| {
                inventory
                    .components
                    .values()
                    .any(|reference| reference.artifact == package.artifact)
            })
            .collect();
        write_json(
            &payload.join("components.json"),
            &json!({"schemaVersion":1,"target":target.target,"components":inventory.components,"roots":roots,"packages":cohort_packages}),
        )?;
        let bootstrap = stage.join("bootstrap-archives");
        fs::create_dir(&bootstrap)?;
        if os == "linux" {
            for reference in inventory.components.values() {
                let local = release()?.join(
                    reference
                        .artifact
                        .url
                        .rsplit('/')
                        .next()
                        .context("Missing asset filename")?,
                );
                let source = if target.build.contains(&reference.id) {
                    local
                } else {
                    fetch(reference)?
                };
                ensure!(
                    sha256_file(&source)? == reference.artifact.sha256,
                    "Bootstrap archive hash mismatch"
                );
                copy_tree(
                    &source,
                    &bootstrap.join(format!(
                        "{}.{}",
                        reference.artifact.sha256, reference.artifact.format
                    )),
                )?;
            }
        }
        package::desktop_input(
            &payload,
            true,
            (os == "linux").then_some(bootstrap.as_path()),
        )?;
        let metadata = read_json(&release()?.join(format!("packages-{}.json", target.target)))?;
        metadata["artifacts"]
            .as_array()
            .context("Missing native package outputs")?
            .iter()
            .map(|name| Ok(release()?.join(name.as_str().context("Invalid native package name")?)))
            .collect::<Result<Vec<_>>>()?
    };
    files
        .into_iter()
        .map(|path| {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .context("Invalid package filename")?;
            let format = if name.ends_with(".AppImage") {
                "AppImage"
            } else if name.ends_with(".tar.gz") {
                "tar.gz"
            } else {
                path.extension()
                    .and_then(|value| value.to_str())
                    .context("Missing package format")?
            };
            Ok(PackageRef {
                target: target.target.clone(),
                version: plan.version.clone(),
                source_sha: plan.source_sha.clone(),
                artifact: artifact(&path, &plan.version, format)?,
                components: inventory
                    .components
                    .iter()
                    .map(|(id, reference)| (*id, reference.identity.clone()))
                    .collect(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn target(build: Vec<ComponentId>) -> TargetPlan {
        TargetPlan {
            target: "linux-x64".into(),
            build,
            reuse: BTreeMap::new(),
            inputs: BTreeMap::new(),
            package: false,
            package_input_hash: "0".repeat(64),
            atomic_groups: vec![],
            packages: vec![],
        }
    }

    fn handoff_fixture(work: &Path) -> Result<(Plan, PathBuf)> {
        let mut selected = target(vec![ComponentId::Daemon]);
        selected.inputs.insert(ComponentId::Daemon, "2".repeat(64));
        let plan = Plan {
            schema_version: 1,
            version: "1.2.3".into(),
            source_sha: "1".repeat(40),
            channel: "stable".into(),
            targets: vec![selected],
            no_changes: false,
            retained_targets: BTreeMap::new(),
        };
        let object = work.join("raw");
        fs::create_dir(&object)?;
        text(&object.join("paseo"), "fixture image", 0o755)?;
        text(&object.join("LICENSE"), "fixture license", 0o644)?;
        let output = work.join("handoff");
        write_handoff(
            &output,
            &plan,
            &plan.targets[0],
            &BTreeMap::from([(ComponentId::Daemon, object)]),
        )?;
        Ok((plan, output))
    }

    #[test]
    fn handoff_rejects_changed_missing_and_extra_files() -> Result<()> {
        for change in ["changed", "missing", "extra"] {
            let work = tempfile::tempdir()?;
            let (plan, output) = handoff_fixture(work.path())?;
            verify_handoff(&output, &plan, &plan.targets[0])?;
            let image = output.join("objects/daemon/paseo");
            match change {
                "changed" => fs::write(image, "tampered")?,
                "missing" => fs::remove_file(image)?,
                _ => fs::write(output.join("extra"), "unexpected")?,
            }
            assert!(
                verify_handoff(&output, &plan, &plan.targets[0]).is_err(),
                "{change}"
            );
        }
        Ok(())
    }

    #[test]
    fn handoff_rejects_plan_mismatch() -> Result<()> {
        let work = tempfile::tempdir()?;
        let (mut plan, output) = handoff_fixture(work.path())?;
        plan.targets[0].package_input_hash = "3".repeat(64);
        assert!(verify_handoff(&output, &plan, &plan.targets[0]).is_err());
        Ok(())
    }
    #[test]
    fn no_change_invokes_no_compiler() {
        selected(&target(vec![]), |_| {
            panic!("no-change compiled a component")
        })
        .unwrap();
    }
    #[test]
    fn daemon_only_never_invokes_gui_or_go() {
        let mut calls = Vec::new();
        selected(&target(vec![ComponentId::Daemon]), |id| {
            calls.push(id);
            Ok(())
        })
        .unwrap();
        assert_eq!(calls, [ComponentId::Daemon]);
    }
    #[test]
    fn app_only_does_not_compile_daemon_tailnet_or_helper() {
        let mut calls = Vec::new();
        selected(&target(vec![ComponentId::App]), |id| {
            calls.push(id);
            Ok(())
        })
        .unwrap();
        assert_eq!(calls, [ComponentId::App]);
    }
    #[test]
    fn exact_shared_closure_does_not_expand_to_other_components() {
        let mut calls = Vec::new();
        selected(&target(vec![ComponentId::App, ComponentId::Daemon]), |id| {
            calls.push(id);
            Ok(())
        })
        .unwrap();
        assert_eq!(calls, [ComponentId::Daemon, ComponentId::App]);
    }
    #[test]
    fn package_only_invokes_no_compiler() {
        let mut plan = target(vec![]);
        plan.package = true;
        selected(&plan, |_| panic!("container repack compiled a component")).unwrap();
    }
    #[test]
    fn archive_paths_reject_traversal_and_windows_escapes() {
        for path in [
            "../paseo",
            "/paseo",
            "C:/paseo",
            "dir\\paseo",
            "a/../paseo",
            "./paseo",
        ] {
            assert!(safe_member(path).is_err(), "{path}");
        }
        assert_eq!(
            safe_member("resources/font.ttf").unwrap(),
            "resources/font.ttf"
        );
    }
    #[test]
    fn extraction_rejects_links_in_real_archive() {
        let work = tempfile::tempdir().unwrap();
        let archive = work.path().join("link.tar.gz");
        let encoder = flate2::write::GzEncoder::new(
            fs::File::create(&archive).unwrap(),
            flate2::Compression::default(),
        );
        let mut writer = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o777);
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        writer
            .append_link(&mut header, "escape", "../outside")
            .unwrap();
        writer.into_inner().unwrap().finish().unwrap();
        assert!(extract(&archive, &work.path().join("extracted"), "tar.gz").is_err());
        assert!(!work.path().join("outside").exists());
    }
    #[test]
    fn round_trip_archive_preserves_executable_and_detects_corruption() {
        let work = tempfile::tempdir().unwrap();
        let root = work.path().join("original");
        fs::create_dir(&root).unwrap();
        text(&root.join("paseo"), "fixture executable", 0o755).unwrap();
        text(&root.join("LICENSE"), "fixture license", 0o644).unwrap();
        let identity = ComponentIdentity {
            version: "1.2.3".into(),
            source_sha: "1".repeat(40),
            input_hash: "2".repeat(64),
            abi_version: 1,
        };
        let descriptor = describe(&root, "linux-x64", ComponentId::Daemon, identity).unwrap();
        write_json(
            &root.join(descriptor_name(ComponentId::Daemon)),
            &serde_json::to_value(&descriptor).unwrap(),
        )
        .unwrap();
        let archive = work
            .path()
            .join("Paseo-Component-daemon-1.2.3-linux-x64.tar.gz");
        package::archive(&root, &archive, false).unwrap();
        let reference = component_ref(descriptor, artifact(&archive, "1.2.3", "tar.gz").unwrap());
        let original = reference.clone();
        let extracted = work.path().join("extracted");
        extract(&archive, &extracted, "tar.gz").unwrap();
        verify_object(&extracted, &reference).unwrap();
        assert_eq!(
            reference, original,
            "reuse must not relabel identity or artifact provenance"
        );
        fs::write(extracted.join("LICENSE"), "corrupted").unwrap();
        assert!(verify_object(&extracted, &reference).is_err());
    }
}
