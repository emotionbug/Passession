use super::integrity;
use super::*;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
    process::{Command, Stdio},
    thread,
    time::Duration,
};

pub(super) fn repository() -> Result<()> {
    ensure!(
        var("GITHUB_REPOSITORY", REPOSITORY) == REPOSITORY,
        "Release operations are restricted to {REPOSITORY}; never use gh's upstream default"
    );
    Ok(())
}
pub(super) fn gh(args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("gh")
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .context("Running GitHub CLI")?;
    ensure!(
        out.status.success(),
        "gh {} failed ({})",
        args.first().unwrap_or(&""),
        out.status
    );
    Ok(out.stdout)
}
fn api(path: &str) -> Result<Value> {
    Ok(serde_json::from_slice(&gh(&["api", path])?)?)
}
pub(super) fn lookup(tag: &str) -> Result<Option<Value>> {
    let pages: Value = serde_json::from_slice(&gh(&[
        "api",
        &format!("repos/{REPOSITORY}/releases?per_page=100"),
        "--paginate",
        "--slurp",
    ])?)?;
    let mut matches = Vec::new();
    for page in pages.as_array().context("Invalid releases response")? {
        for release in page.as_array().context("Invalid release page")? {
            if release["tag_name"] == tag {
                matches.push(release.clone());
            }
        }
    }
    ensure!(matches.len() <= 1, "Ambiguous release tag {tag}");
    Ok(matches.pop())
}
fn upload(tag: &str, path: &Path) -> Result<()> {
    for attempt in 0..4 {
        let result = gh(&[
            "release",
            "upload",
            tag,
            path.to_str().context("Non-UTF8 upload path")?,
            "--clobber",
            "--repo",
            REPOSITORY,
        ]);
        if result.is_ok() {
            return Ok(());
        }
        if attempt == 3 {
            return result.map(|_| ());
        }
        thread::sleep(Duration::from_secs(2_u64.pow(attempt + 1)));
    }
    unreachable!()
}
fn asset<'a>(release: &'a Value, name: &str) -> Result<&'a Value> {
    let matches: Vec<_> = release["assets"]
        .as_array()
        .context("Missing release assets")?
        .iter()
        .filter(|a| a["name"] == name)
        .collect();
    ensure!(
        matches.len() == 1,
        "Expected exactly one release asset {name}"
    );
    Ok(matches[0])
}
fn asset_bytes(asset: &Value) -> Result<Vec<u8>> {
    let id = asset["id"].as_u64().context("Invalid asset ID")?;
    gh(&[
        "api",
        &format!("repos/{REPOSITORY}/releases/assets/{id}"),
        "-H",
        "Accept: application/octet-stream",
    ])
}
fn download_asset(asset: &Value, path: &Path) -> Result<()> {
    let id = asset["id"].as_u64().context("Invalid asset ID")?;
    let output = fs::File::create(path)?;
    let status = Command::new("gh")
        .args([
            "api",
            &format!("repos/{REPOSITORY}/releases/assets/{id}"),
            "-H",
            "Accept: application/octet-stream",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::from(output))
        .stderr(Stdio::inherit())
        .status()?;
    ensure!(status.success(), "Release asset download failed");
    Ok(())
}
fn trusted_publisher(value: &Value, field: &str) -> Result<()> {
    let login = value[field]["login"]
        .as_str()
        .context("Missing release publisher")?;
    ensure!(
        login == "github-actions[bot]" || login == REPOSITORY.split('/').next().unwrap(),
        "Untrusted release publisher {login}"
    );
    Ok(())
}
fn trusted_asset_bytes(release: &Value, name: &str) -> Result<Vec<u8>> {
    let remote = asset(release, name)?;
    trusted_publisher(remote, "uploader")?;
    ensure!(
        remote["size"]
            .as_u64()
            .is_some_and(|n| n <= 4 * 1024 * 1024),
        "Oversized release metadata"
    );
    let bytes = asset_bytes(remote)?;
    ensure!(
        remote["size"] == bytes.len() as u64,
        "Release metadata size mismatch"
    );
    if let Some(digest) = remote["digest"].as_str() {
        use sha2::Digest;
        ensure!(
            digest == format!("sha256:{}", hex::encode(sha2::Sha256::digest(&bytes))),
            "Release metadata digest mismatch"
        );
    }
    Ok(bytes)
}
fn verify_remote_artifact(
    release: &Value,
    reference: &paseo_protocol::components::ArtifactRef,
) -> Result<()> {
    let name = integrity::artifact_name(reference)?;
    let remote = asset(release, name)?;
    trusted_publisher(remote, "uploader")?;
    let download =
        |tag: &str| format!("https://github.com/{REPOSITORY}/releases/download/{tag}/{name}");
    // A draft serves its assets from an `untagged-…` path; publication moves
    // them to the tag path that the reference already names.
    let url_matches = if release["draft"] == true {
        let remote_url = remote["browser_download_url"].as_str().unwrap_or_default();
        release["tag_name"]
            .as_str()
            .is_some_and(|tag| reference.url == download(tag))
            && remote_url
                .strip_prefix(&format!(
                    "https://github.com/{REPOSITORY}/releases/download/untagged-"
                ))
                .and_then(|rest| rest.split_once('/'))
                .is_some_and(|(_, asset_name)| asset_name == name)
    } else {
        remote["browser_download_url"] == reference.url
    };
    ensure!(
        remote["size"] == reference.size && url_matches,
        "Origin artifact identity mismatch"
    );
    if let Some(digest) = remote["digest"].as_str() {
        ensure!(
            digest == format!("sha256:{}", reference.sha256),
            "Origin artifact SHA-256 mismatch"
        );
    } else {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("artifact");
        download_asset(remote, &path)?;
        ensure!(
            fs::metadata(&path)?.len() == reference.size
                && common::sha256_file(&path)? == reference.sha256,
            "Downloaded origin artifact hash mismatch"
        );
    }
    Ok(())
}
fn authenticate_inventory(
    tag: &str,
    cache: &mut BTreeMap<String, paseo_protocol::components::ReleaseInventory>,
    visiting: &mut BTreeSet<String>,
) -> Result<()> {
    use paseo_protocol::components;
    use sha2::Digest;
    if cache.contains_key(tag) {
        return Ok(());
    }
    ensure!(
        visiting.len() < 32 && visiting.insert(tag.into()),
        "Cyclic or excessive component origin chain"
    );
    let release = lookup(tag)?.context("Pinned origin release is missing")?;
    ensure!(
        release["draft"] == false && release["tag_name"] == tag,
        "Origin release is not published"
    );
    trusted_publisher(&release, "author")?;
    let version = tag_version(tag)?;
    let channel = channel(&version);
    let name = format!("components-{channel}.json");
    let bytes = trusted_asset_bytes(&release, &name)?;
    let inventory = components::parse_inventory(&bytes, REPOSITORY)?;
    ensure!(
        inventory.version == version.to_string()
            && release["target_commitish"] == inventory.source_sha,
        "Collection source does not match origin release"
    );
    let provenance: Value = serde_json::from_slice(&trusted_asset_bytes(
        &release,
        &format!("provenance-components-{channel}.json"),
    )?)?;
    ensure!(
        provenance["schemaVersion"] == 2
            && provenance["workflow"] == WORKFLOW
            && provenance["version"] == inventory.version
            && provenance["sourceSha"] == inventory.source_sha
            && provenance["channel"] == inventory.channel,
        "Invalid collection provenance"
    );
    ensure!(
        provenance["inventory"]["name"] == name
            && provenance["inventory"]["size"] == bytes.len() as u64
            && provenance["inventory"]["sha256"] == hex::encode(sha2::Sha256::digest(&bytes)),
        "Collection inventory hash mismatch"
    );
    let run = provenance["runId"]
        .as_str()
        .context("Missing collection provenance run")?;
    ensure!(numeric_id(run), "Invalid collection run");
    let workflow = api(&format!("repos/{REPOSITORY}/actions/runs/{run}"))?;
    ensure!(
        workflow["path"] == WORKFLOW
            && workflow["event"] == "workflow_dispatch"
            && workflow["status"] == "completed"
            && workflow["conclusion"] == "success"
            && workflow["head_repository"]["full_name"] == REPOSITORY,
        "Origin was not produced by a successful canonical release workflow"
    );
    let mut checked_runs = BTreeSet::from([run.to_owned()]);
    for (target, hash) in provenance["targets"]
        .as_object()
        .context("Missing provenance targets")?
    {
        super::target(target)?;
        let record_bytes = trusted_asset_bytes(&release, &format!("provenance-{target}.json"))?;
        ensure!(
            hash.as_str() == Some(hex::encode(sha2::Sha256::digest(&record_bytes)).as_str()),
            "Target provenance digest mismatch"
        );
        let record: Value = serde_json::from_slice(&record_bytes)?;
        ensure!(
            record["schemaVersion"] == 2
                && record["workflow"] == WORKFLOW
                && record["sha"] == inventory.source_sha
                && record["version"] == inventory.version
                && record["target"] == *target,
            "Target provenance collection mismatch"
        );
        let source_tag = record["sourceTag"]
            .as_str()
            .context("Missing target source tag")?;
        ensure!(
            tag_version(source_tag)?.to_string() == inventory.version
                && record["channel"] == inventory.channel,
            "Target source tag/channel mismatch"
        );
        let target_run = record["runId"]
            .as_str()
            .context("Missing target build run")?;
        ensure!(numeric_id(target_run), "Invalid target build run");
        if checked_runs.insert(target_run.into()) {
            let workflow = api(&format!("repos/{REPOSITORY}/actions/runs/{target_run}"))?;
            ensure!(
                workflow["path"] == WORKFLOW
                    && workflow["event"] == "workflow_dispatch"
                    && workflow["status"] == "completed"
                    && workflow["head_repository"]["full_name"] == REPOSITORY,
                "Recovered target came from a noncanonical workflow"
            );
        }
        let target_inventory = inventory
            .targets
            .get(target)
            .context("Provenance target outside inventory")?;
        ensure!(
            record["components"] == serde_json::to_value(&target_inventory.components)?
                && record["packages"] == serde_json::to_value(&target_inventory.packages)?,
            "Target provenance component mismatch"
        );
    }
    let mut origins = BTreeSet::new();
    let mut checked = BTreeSet::new();
    for (target_id, target) in &inventory.targets {
        for reference in target
            .components
            .values()
            .map(|c| &c.artifact)
            .chain(target.packages.iter().map(|p| &p.artifact))
        {
            if reference.origin_tag == tag {
                ensure!(
                    provenance["targets"].get(target_id).is_some(),
                    "Current-origin artifact has no authenticated target proof"
                );
                if checked.insert(reference.url.clone()) {
                    verify_remote_artifact(&release, reference)?;
                }
            } else {
                origins.insert(reference.origin_tag.clone());
            }
        }
    }
    for origin in origins {
        authenticate_inventory(&origin, cache, visiting)?;
    }
    components::validate_inventory_provenance(&inventory, cache, REPOSITORY)?;
    visiting.remove(tag);
    cache.insert(tag.into(), inventory);
    Ok(())
}
pub(crate) fn authenticate_reuse(decision: &crate::components::plan::TargetPlan) -> Result<()> {
    let mut cache = BTreeMap::new();
    for reference in decision
        .reuse
        .values()
        .map(|c| &c.artifact)
        .chain(decision.packages.iter().map(|p| &p.artifact))
    {
        authenticate_inventory(&reference.origin_tag, &mut cache, &mut BTreeSet::new())?;
    }
    for (id, reference) in &decision.reuse {
        ensure!(
            cache
                .get(&reference.artifact.origin_tag)
                .and_then(|i| i.targets.get(&decision.target))
                .and_then(|t| t.components.get(id))
                == Some(reference),
            "Reused component differs from authenticated origin"
        );
    }
    for package in &decision.packages {
        ensure!(
            cache
                .get(&package.artifact.origin_tag)
                .and_then(|i| i.targets.get(&decision.target))
                .is_some_and(|t| t.packages.contains(package)),
            "Reused package differs from authenticated origin"
        );
    }
    Ok(())
}
pub(crate) fn prior_inventory(
    explicit_tag: Option<&str>,
) -> Result<(
    Option<paseo_protocol::components::ReleaseInventory>,
    BTreeMap<String, String>,
)> {
    repository()?;
    let current = version(&common::version()?)?;
    let selected = if let Some(tag) = explicit_tag {
        Some(lookup(tag)?.context("Requested prior release does not exist")?)
    } else {
        let pages: Value = serde_json::from_slice(&gh(&[
            "api",
            &format!("repos/{REPOSITORY}/releases?per_page=100"),
            "--paginate",
            "--slurp",
        ])?)?;
        let mut releases = Vec::new();
        for page in pages.as_array().context("Invalid release pages")? {
            for candidate in page.as_array().context("Invalid release page")? {
                let Some(tag) = candidate["tag_name"].as_str() else {
                    continue;
                };
                if let Ok(v) = tag_version(tag)
                    && candidate["draft"] == false
                    && v <= current
                    && channel(&v) == channel(&current)
                {
                    releases.push((v, candidate.clone()));
                }
            }
        }
        releases.sort_by(|a, b| a.0.cmp(&b.0));
        releases.pop().map(|(_, release)| release)
    };
    let Some(selected) = selected else {
        return Ok((None, BTreeMap::new()));
    };
    let tag = selected["tag_name"]
        .as_str()
        .context("Missing prior release tag")?;
    let prior_version = tag_version(tag)?;
    ensure!(
        prior_version <= current && channel(&prior_version) == channel(&current),
        "Prior release is outside the collection channel/version"
    );
    let mut cache = BTreeMap::new();
    authenticate_inventory(tag, &mut cache, &mut BTreeSet::new())?;
    let inventory = cache
        .remove(tag)
        .context("Missing authenticated prior inventory")?;
    let mut hashes = BTreeMap::new();
    let collection_provenance: Value = serde_json::from_slice(&trusted_asset_bytes(
        &selected,
        &format!("provenance-components-{}.json", inventory.channel),
    )?)?;
    for (target, target_inventory) in &inventory.targets {
        let record_name = format!("provenance-{target}.json");
        let origin = if collection_provenance["targets"].get(target).is_some() {
            selected.clone()
        } else if let Some(package) = target_inventory.packages.first() {
            lookup(&package.artifact.origin_tag)?.context("Missing package origin")?
        } else {
            continue;
        };
        let provenance: Value =
            serde_json::from_slice(&trusted_asset_bytes(&origin, &record_name)?)?;
        let hash = provenance["plan"]["packageInputHash"]
            .as_str()
            .context("Missing package fingerprint")?;
        paseo_protocol::components::validate_sha256(hash)?;
        hashes.insert(target.clone(), hash.into());
    }
    Ok((Some(inventory), hashes))
}
pub(crate) fn recovery_plan() -> Result<crate::components::plan::Plan> {
    repository()?;
    let run = required("RECOVERY_RUN_ID")?;
    ensure!(numeric_id(&run), "Exact recovery run required");
    let info = api(&format!("repos/{REPOSITORY}/actions/runs/{run}"))?;
    ensure!(
        info["path"] == WORKFLOW
            && info["event"] == "workflow_dispatch"
            && info["status"] == "completed"
            && info["head_repository"]["full_name"] == REPOSITORY,
        "Invalid recovery workflow"
    );
    let temporary = tempfile::tempdir()?;
    gh(&[
        "run",
        "download",
        &run,
        "--repo",
        REPOSITORY,
        "--name",
        "native-component-plan",
        "--dir",
        temporary.path().to_str().context("Invalid recovery path")?,
    ])?;
    let plan = crate::components::plan::read(&temporary.path().join("component-plan.json"))?;
    ensure!(
        plan.source_sha == required("SOURCE_SHA")? && plan.version == common::version()?,
        "Recovery plan source mismatch"
    );
    Ok(plan)
}
#[cfg(test)]
fn upload_ordered(
    dir: &Path,
    binaries: &BTreeSet<String>,
    retained: &BTreeSet<String>,
    feeds: &BTreeSet<String>,
    mut send: impl FnMut(&Path) -> Result<()>,
) -> Result<()> {
    for name in binaries.iter().chain(retained).chain(feeds) {
        send(&dir.join(name))?;
    }
    Ok(())
}
fn upload_immutable(tag: &str, release: &Value, path: &Path) -> Result<()> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .context("Invalid upload basename")?;
    let existing: Vec<_> = release["assets"]
        .as_array()
        .context("Missing release assets")?
        .iter()
        .filter(|a| a["name"] == name)
        .collect();
    ensure!(existing.len() <= 1, "Ambiguous immutable asset");
    if let Some(remote) = existing.first() {
        let temporary = tempfile::tempdir()?;
        let downloaded = temporary.path().join("asset");
        download_asset(remote, &downloaded)?;
        ensure!(
            common::sha256_file(path)? == common::sha256_file(&downloaded)?,
            "Refusing to replace immutable release asset {name}"
        );
        return Ok(());
    }
    gh(&[
        "release",
        "upload",
        tag,
        path.to_str().context("Invalid upload path")?,
        "--repo",
        REPOSITORY,
    ])?;
    Ok(())
}
fn upload_metadata(tag: &str, release: &Value, path: &Path) -> Result<()> {
    ensure!(
        path.extension()
            .is_some_and(|extension| extension == "json"),
        "Only draft metadata may be replaced"
    );
    if release["draft"] == true {
        upload(tag, path)
    } else {
        upload_immutable(tag, release, path)
    }
}
pub(super) fn publish() -> Result<()> {
    repository()?;
    let plan = integrity::component_plan()?;
    if plan.no_changes {
        println!("No component or package changes; no release published");
        return Ok(());
    }
    let tag = required("RELEASE_TAG")?;
    ensure!(
        tag == format!("v{}", plan.version)
            && plan.version == common::version()?
            && plan.source_sha == required("SOURCE_SHA")?,
        "Publication collection mismatch"
    );
    let run = required("GITHUB_RUN_ID")?;
    ensure!(numeric_id(&run), "Missing publication run identity");
    let ids = expected()?;
    let changed: BTreeSet<_> = plan
        .targets
        .iter()
        .filter(|t| t.changed())
        .map(|t| t.target.as_str())
        .collect();
    ensure!(
        ids.iter()
            .map(|id| id.target.as_str())
            .collect::<BTreeSet<_>>()
            == changed,
        "Expected targets do not match changed component plan"
    );
    let dir = directory()?;
    let mut binaries = BTreeSet::new();
    let mut metadata_files = BTreeSet::new();
    let mut provenance_hashes = BTreeMap::new();
    // Every immutable input and every reused origin is verified before mutation.
    for id in &ids {
        let record = integrity::verify(&dir, id)?;
        let provenance_name = format!("provenance-{}.json", id.target);
        provenance_hashes.insert(
            id.target.clone(),
            common::sha256_file(&dir.join(&provenance_name))?,
        );
        metadata_files.insert(provenance_name);
        for entry in record["files"]
            .as_array()
            .context("Missing recorded artifacts")?
        {
            let name = entry["name"]
                .as_str()
                .context("Invalid recorded artifact")?
                .to_owned();
            if name.ends_with(".json") {
                metadata_files.insert(name);
            } else {
                binaries.insert(name);
            }
        }
    }
    let inventory = integrity::assemble(&dir)?;
    let mut origins = BTreeMap::new();
    for target in inventory.targets.values() {
        for reference in target
            .components
            .values()
            .map(|c| &c.artifact)
            .chain(target.packages.iter().map(|p| &p.artifact))
        {
            if reference.origin_tag != tag {
                authenticate_inventory(&reference.origin_tag, &mut origins, &mut BTreeSet::new())?;
            }
        }
    }
    paseo_protocol::components::validate_inventory_provenance(&inventory, &origins, REPOSITORY)?;
    common::write_json(
        &dir.join(format!("components-{}.json", plan.channel)),
        &serde_json::to_value(&inventory)?,
    )?;
    let inventory_name = format!("components-{}.json", plan.channel);
    let provenance_name = format!("provenance-components-{}.json", plan.channel);
    common::write_json(
        &dir.join(&provenance_name),
        &json!({"schemaVersion":2,"workflow":WORKFLOW,"version":plan.version,"sourceSha":plan.source_sha,"channel":plan.channel,"runId":run,
        "inventory":{"name":inventory_name,"size":fs::metadata(dir.join(&inventory_name))?.len(),"sha256":common::sha256_file(&dir.join(&inventory_name))?},"targets":provenance_hashes}),
    )?;
    metadata_files.insert(provenance_name);
    let notes_directory = tempfile::tempdir()?;
    let notes_path = notes_directory.path().join("notes.txt");
    fs::write(
        &notes_path,
        format!(
            "{}\n\nCollection source: [`{sha}`](https://github.com/{REPOSITORY}/commit/{sha}). Component build sources are retained in `{inventory_name}`.\n",
            metadata::release_notes(&plan.version)?,
            sha = plan.source_sha
        ),
    )?;
    let release = match lookup(&tag)? {
        Some(release) => {
            ensure!(
                release["target_commitish"] == plan.source_sha,
                "Existing collection source mismatch"
            );
            release
        }
        None => {
            ensure!(
                var("CREATE_RELEASE", "false") == "true",
                "Platform repair requires an existing release"
            );
            serde_json::from_slice(&gh(&[
                "api",
                "--method",
                "POST",
                &format!("repos/{REPOSITORY}/releases"),
                "-f",
                &format!("tag_name={tag}"),
                "-f",
                &format!("target_commitish={}", plan.source_sha),
                "-f",
                &format!("name={tag}"),
                "-F",
                &format!("body=@{}", notes_path.display()),
                "-F",
                "draft=true",
                "-F",
                &format!("prerelease={}", !version(&plan.version)?.pre.is_empty()),
            ])?)?
        }
    };
    let release_id = release["id"].as_u64().context("Missing release ID")?;
    let release_date = release["published_at"]
        .as_str()
        .or_else(|| release["created_at"].as_str())
        .context("Missing release date")?;
    integrity::manifests_at(&dir, release_date)?;
    // Never re-upload old component/package binaries. Inventory and feeds are last.
    for name in &binaries {
        upload_immutable(&tag, &release, &dir.join(name))?;
    }
    for name in &metadata_files {
        upload_metadata(&tag, &release, &dir.join(name))?;
    }
    let fresh = api(&format!("repos/{REPOSITORY}/releases/{release_id}"))?;
    for target in inventory.targets.values() {
        for reference in target
            .components
            .values()
            .map(|c| &c.artifact)
            .chain(target.packages.iter().map(|p| &p.artifact))
        {
            if reference.origin_tag == tag {
                verify_remote_artifact(&fresh, reference)?;
            }
        }
    }
    upload_metadata(&tag, &fresh, &dir.join(&inventory_name))?;
    let mut feeds = BTreeSet::new();
    for target in inventory
        .targets
        .keys()
        .filter(|id| super::target(id).is_ok_and(|t| t.desktop))
    {
        for prefix in [CLIENT, "daemon"] {
            let name = format!("{prefix}-{}-{target}.json", plan.channel);
            upload_metadata(&tag, &fresh, &dir.join(&name))?;
            feeds.insert(name);
        }
    }
    let uploaded = api(&format!("repos/{REPOSITORY}/releases/{release_id}"))?;
    for name in metadata_files
        .iter()
        .chain(&feeds)
        .chain(std::iter::once(&inventory_name))
    {
        let bytes = trusted_asset_bytes(&uploaded, name)?;
        ensure!(
            bytes == fs::read(dir.join(name))?,
            "Uploaded release metadata differs: {name}"
        );
    }
    gh(&[
        "api",
        "--method",
        "PATCH",
        &format!("repos/{REPOSITORY}/releases/{release_id}"),
        "-f",
        &format!("tag_name={tag}"),
        "-F",
        "draft=false",
        "-F",
        &format!("prerelease={}", !version(&plan.version)?.pre.is_empty()),
    ])?;
    Ok(())
}
pub(super) fn recovery(args: &[String]) -> Result<()> {
    repository()?;
    ensure!(
        args.is_empty() || args == ["--download"],
        "recovery [--download]"
    );
    let run = required("RECOVERY_RUN_ID")?;
    ensure!(
        numeric_id(&run) && run != var("GITHUB_RUN_ID", ""),
        "Recovery requires an exact prior run ID"
    );
    let info = api(&format!("repos/{REPOSITORY}/actions/runs/{run}"))?;
    ensure!(
        info["path"] == WORKFLOW
            && info["event"] == "workflow_dispatch"
            && info["status"] == "completed"
            && info["head_repository"]["full_name"] == REPOSITORY,
        "Recovery requires a completed canonical Native Release run"
    );
    let targets: Vec<String> = serde_json::from_str(&required("RECOVERY_TARGETS")?)?;
    ensure!(
        !targets.is_empty() && targets.iter().collect::<BTreeSet<_>>().len() == targets.len(),
        "Invalid recovery target inventory"
    );
    let pages: Value = serde_json::from_slice(&gh(&[
        "api",
        &format!("repos/{REPOSITORY}/actions/runs/{run}/artifacts?per_page=100"),
        "--paginate",
        "--slurp",
    ])?)?;
    let artifacts: Vec<&Value> = pages
        .as_array()
        .context("Invalid artifact pages")?
        .iter()
        .flat_map(|p| p["artifacts"].as_array().into_iter().flatten())
        .collect();
    for id in &targets {
        target(id)?;
        let name = format!("native-release-{id}");
        let matches: Vec<_> = artifacts.iter().filter(|a| a["name"] == name).collect();
        ensure!(
            matches.len() == 1 && matches[0]["expired"] == false,
            "Expected exactly one unexpired artifact for {id}"
        );
    }
    if !args.is_empty() {
        let dir = directory()?;
        fs::create_dir_all(&dir)?;
        for id in targets {
            gh(&[
                "run",
                "download",
                &run,
                "--repo",
                REPOSITORY,
                "--name",
                &format!("native-release-{id}"),
                "--dir",
                dir.to_str().context("Invalid artifact directory")?,
            ])?;
        }
    }
    Ok(())
}
pub(super) fn rollout() -> Result<()> {
    repository()?;
    let tag = required("SOURCE_TAG")?;
    let v = tag_version(&tag)?;
    ensure!(tag == format!("v{v}"), "Canonical release tag required");
    let amount = hours(&required("NEW_HOURS")?)?;
    let release = lookup(&tag)?.context("Release not found")?;
    let mut authenticated = BTreeMap::new();
    authenticate_inventory(&tag, &mut authenticated, &mut BTreeSet::new())?;
    let inventory = authenticated
        .get(&tag)
        .context("Missing authenticated rollout inventory")?;
    let targets: Vec<_> = inventory
        .targets
        .keys()
        .filter_map(|id| target(id).ok().filter(|t| t.desktop))
        .collect();
    let dir = tempfile::tempdir()?;
    let mut updates = Vec::new();
    for t in targets {
        let name = format!("{CLIENT}-{}-{}.json", channel(&v), t.id());
        let manifest: Value = serde_json::from_slice(&asset_bytes(asset(&release, &name)?)?)?;
        ensure!(
            manifest["collectionVersion"] == v.to_string()
                && manifest["channel"] == channel(&v)
                && manifest["platform"] == t.platform
                && manifest["arch"] == t.arch,
            "Rollout manifest identity mismatch"
        );
        let expected = inventory
            .targets
            .get(&t.id())
            .and_then(|t| {
                t.components
                    .get(&paseo_protocol::components::ComponentId::App)
            })
            .context("Missing app cohort")?;
        ensure!(
            manifest["schemaVersion"] == 2
                && manifest["collectionSourceSha"] == inventory.source_sha
                && manifest["component"] == serde_json::to_value(expected)?,
            "Rollout feed does not reference authenticated component inventory"
        );
        let bytes =
            trusted_asset_bytes(&release, &format!("components-{}.json", inventory.channel))?;
        use base64::Engine;
        use sha2::Digest;
        ensure!(
            manifest["inventory"]["sha512"]
                == base64::engine::general_purpose::STANDARD.encode(sha2::Sha512::digest(bytes)),
            "Rollout inventory pin mismatch"
        );
        let updated = integrity::restamp(manifest, amount)?;
        let path = dir.path().join(name);
        common::write_json(&path, &updated)?;
        updates.push(path);
    }
    for path in updates {
        upload(&tag, &path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn origin_assets_require_exact_url_hash_size_and_publisher() {
        let reference = paseo_protocol::components::ArtifactRef {
            url:"https://github.com/emotionbug/Passession/releases/download/v0.13.26/component.tar.gz".into(),
            sha256:"a".repeat(64),size:42,format:"tar.gz".into(),origin_tag:"v0.13.26".into(),
        };
        let release = json!({"assets":[{"name":"component.tar.gz","size":42,"browser_download_url":reference.url,"digest":format!("sha256:{}",reference.sha256),"uploader":{"login":"github-actions[bot]"}}]});
        verify_remote_artifact(&release, &reference).unwrap();
        let mut draft = release.clone();
        draft["draft"] = json!(true);
        draft["tag_name"] = json!("v0.13.26");
        draft["assets"][0]["browser_download_url"] = json!(
            "https://github.com/emotionbug/Passession/releases/download/untagged-d84fe91d/component.tar.gz"
        );
        verify_remote_artifact(&draft, &reference).unwrap();
        let mut other_tag = draft.clone();
        other_tag["tag_name"] = json!("v0.13.27");
        assert!(verify_remote_artifact(&other_tag, &reference).is_err());
        let mut published_untagged = draft.clone();
        published_untagged["draft"] = json!(false);
        assert!(verify_remote_artifact(&published_untagged, &reference).is_err());
        for (key, value) in [
            ("size", json!(43)),
            ("digest", json!(format!("sha256:{}", "b".repeat(64)))),
            (
                "browser_download_url",
                json!("https://example.invalid/component.tar.gz"),
            ),
            ("uploader", json!({"login":"untrusted"})),
        ] {
            let mut corrupt = release.clone();
            corrupt["assets"][0][key] = value;
            assert!(
                verify_remote_artifact(&corrupt, &reference).is_err(),
                "{key}"
            );
        }
    }
    #[test]
    fn failed_upload_never_reaches_feeds() {
        let binaries = BTreeSet::from(["app.zip".into(), "daemon.zip".into()]);
        let feeds = BTreeSet::from(["app.json".into()]);
        let retained = BTreeSet::from(["provenance.json".into()]);
        let mut sent = Vec::new();
        assert!(
            upload_ordered(Path::new("."), &binaries, &retained, &feeds, |p| {
                sent.push(p.file_name().unwrap().to_str().unwrap().to_owned());
                if sent.len() == 2 {
                    bail!("upload failed");
                }
                Ok(())
            })
            .is_err()
        );
        assert_eq!(sent, ["app.zip", "daemon.zip"]);
        sent.clear();
        upload_ordered(Path::new("."), &binaries, &retained, &feeds, |p| {
            sent.push(p.file_name().unwrap().to_str().unwrap().to_owned());
            Ok(())
        })
        .unwrap();
        assert_eq!(
            sent,
            ["app.zip", "daemon.zip", "provenance.json", "app.json"]
        );
    }
}
