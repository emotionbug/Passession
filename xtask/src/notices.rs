use crate::common::{read_json, root, write_json};
use anyhow::{Context, Result, bail, ensure};
use regex::Regex;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
};
use walkdir::WalkDir;

const BOUNDARY: &str = "Rust daemon Cargo dependency closure (normal and build dependencies; dev-only edges excluded). Includes upstream license/notice files and nested vendored-component notices; this is not a source-offer or a toolchain/system-library compliance certification.";
const DATE_HEADER: &str = "// Native port of V8's DateParser (dateparser.h, dateparser-inl.h, dateparser.cc):\n\
// https://github.com/v8/v8/tree/main/src/date\n\
// Copyright 2011, 2014 the V8 project authors. All rights reserved.\n\
// Redistribution and use in source and binary forms, with or without\n\
// modification, are permitted provided that the following conditions are met:\n\
// * Redistributions of source code must retain the above copyright notice,\n\
//   this list of conditions and the following disclaimer.\n\
// * Redistributions in binary form must reproduce the above copyright notice,\n\
//   this list of conditions and the following disclaimer in the documentation\n\
//   and/or other materials provided with the distribution.\n\
// * Neither the name of Google Inc. nor the names of its contributors may be\n\
//   used to endorse or promote products derived from this software without\n\
//   specific prior written permission.\n\
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS \"AS IS\"\n\
// AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE\n\
// IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE\n\
// ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT OWNER OR CONTRIBUTORS BE\n\
// LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR\n\
// CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF\n\
// SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS\n\
// INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN\n\
// CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE)\n\
// ARISING IN ANY WAY OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE\n\
// POSSIBILITY OF SUCH DAMAGE.\n";

fn digest(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}
fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    let result = value
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("Missing string field {key}"))?;
    ensure!(
        !result.trim().is_empty() && !result.contains('\0'),
        "Empty or invalid {key}"
    );
    Ok(result)
}
fn array<'a>(value: &'a Value, key: &str) -> Result<&'a Vec<Value>> {
    value
        .get(key)
        .and_then(Value::as_array)
        .with_context(|| format!("Missing array field {key}"))
}
fn text(path: &Path) -> Result<String> {
    let value =
        fs::read_to_string(path).with_context(|| format!("Reading notice {}", path.display()))?;
    ensure!(
        !value.trim().is_empty() && !value.contains('\0'),
        "Empty or non-text notice: {}",
        path.display()
    );
    Ok(value)
}
fn host_target(os: &str, arch: &str) -> Result<&'static str> {
    match (os, arch) {
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-gnu"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("windows", "x86_64") => Ok("x86_64-pc-windows-msvc"),
        ("windows", "aarch64") => Ok("aarch64-pc-windows-msvc"),
        _ => bail!("Unsupported native Cargo host: {os}-{arch}"),
    }
}

struct Notice {
    source: String,
    text: String,
    license: bool,
}

fn package_notices(directory: &Path, license_file: Option<&str>) -> Result<Vec<Notice>> {
    let notice_name = Regex::new(
        r"(?i)^(?:licen[cs]es?|copying|copyright|notices?|unlicense|authors|patents|third[-_ ]?party[-_ ]?(?:notices?|licenses?))(?:[._ -].*)?$",
    )?;
    let license_name = Regex::new(r"(?i)^(?:licen[cs]es?|copying|unlicense)(?:[._ -].*)?$")?;
    let mut paths = BTreeMap::<PathBuf, bool>::new();
    for entry in WalkDir::new(directory)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            !matches!(
                entry.file_name().to_str(),
                Some("node_modules" | ".git" | "target")
            )
        })
    {
        let entry =
            entry.with_context(|| format!("Scanning notices in {}", directory.display()))?;
        if entry.file_type().is_dir() {
            continue;
        }
        let relative = entry.path().strip_prefix(directory)?;
        let in_licenses = relative.parent().is_some_and(|parent| {
            parent.components().any(|component| {
                let name = component.as_os_str().to_string_lossy();
                name.eq_ignore_ascii_case("licenses") || name.eq_ignore_ascii_case("licences")
            })
        });
        let name = entry.file_name().to_string_lossy();
        if in_licenses || notice_name.is_match(&name) {
            ensure!(
                !entry.file_type().is_symlink(),
                "Symlink notice is not a self-contained source: {}",
                entry.path().display()
            );
            paths.insert(
                entry.path().to_owned(),
                in_licenses || license_name.is_match(&name),
            );
        }
    }
    if let Some(file) = license_file {
        paths.insert(directory.join(file), true);
    }
    paths
        .into_iter()
        .map(|(path, license)| {
            let source = path
                .strip_prefix(directory)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            Ok(Notice {
                source,
                text: text(&path)?,
                license,
            })
        })
        .collect()
}

fn dependency_closure<'a>(metadata: &'a Value, root: &str) -> Result<Vec<&'a Value>> {
    let mut packages = BTreeMap::new();
    for pkg in array(metadata, "packages")? {
        ensure!(
            packages.insert(string(pkg, "id")?, pkg).is_none(),
            "Duplicate Cargo package ID"
        );
    }
    let mut nodes = BTreeMap::new();
    for node in array(&metadata["resolve"], "nodes")? {
        ensure!(
            nodes.insert(string(node, "id")?, node).is_none(),
            "Duplicate Cargo node ID"
        );
    }
    let root = packages
        .iter()
        .find(|(_, pkg)| pkg["name"] == root && pkg["source"].is_null())
        .map(|(id, _)| *id)
        .with_context(|| format!("Cargo metadata lacks workspace package {root}"))?;
    let mut pending = vec![root];
    let mut visited = BTreeSet::new();
    while let Some(id) = pending.pop() {
        if !visited.insert(id) {
            continue;
        }
        ensure!(
            packages.contains_key(id),
            "Incomplete Cargo package metadata: {id}"
        );
        let node = nodes
            .get(id)
            .with_context(|| format!("Incomplete Cargo resolve metadata: {id}"))?;
        for dep in array(node, "deps")? {
            let kinds = array(dep, "dep_kinds")?;
            ensure!(!kinds.is_empty(), "Missing Cargo dependency kinds: {id}");
            let mut included = false;
            for kind in kinds {
                match kind.get("kind") {
                    Some(Value::Null) => included = true,
                    Some(Value::String(kind)) if kind == "normal" || kind == "build" => {
                        included = true
                    }
                    Some(Value::String(kind)) if kind == "dev" => (),
                    _ => bail!("Invalid Cargo dependency kind: {id}"),
                }
            }
            if included {
                pending.push(string(dep, "pkg")?);
            }
        }
    }
    Ok(visited
        .into_iter()
        .filter(|id| *id != root)
        .map(|id| packages[id])
        .collect())
}

/// Pinned license texts for crates published without them. A vendored fork may
/// cite its upstream project at `upstreamRevision`; `revision` still proves the
/// exact published source through its `.cargo_vcs_info.json`. A crate published
/// without VCS information is identified instead by `checksum`, the registry
/// SHA-256 Cargo.lock records for the downloaded `.crate`, and its license URL
/// must then be pinned by `upstreamRevision`. Supplements identify registry
/// sources only: a vendored or patched path copy (`source` null) must carry its
/// own license file, because its bytes no longer match the published revision.
fn supplement_notices(
    pkg: &Value,
    directory: &Path,
    supplements: &[Value],
    checksum: Option<&str>,
) -> Result<Vec<Notice>> {
    if pkg["source"].is_null() {
        return Ok(Vec::new());
    }
    let matches: Vec<_> = supplements
        .iter()
        .filter(|entry| {
            entry["name"] == pkg["name"]
                && entry["version"] == pkg["version"]
                && entry["repository"] == pkg["repository"]
        })
        .collect();
    if matches.is_empty() {
        return Ok(Vec::new());
    }
    let is_sha = |value: &str, length: usize| {
        value.len() == length && value.bytes().all(|b| b.is_ascii_hexdigit())
    };
    let mut vcs = None;
    let mut urls = BTreeSet::new();
    let mut notices = Vec::new();
    for entry in matches {
        let url = string(entry, "url")?;
        let value = string(entry, "text")?;
        ensure!(
            urls.insert(url),
            "Duplicate Cargo notice supplement: {}@{} {url}",
            pkg["name"],
            pkg["version"]
        );
        let published = match entry.get("revision") {
            Some(_) => {
                let revision = string(entry, "revision")?;
                if vcs.is_none() {
                    vcs = Some(read_json(&directory.join(".cargo_vcs_info.json"))?);
                }
                is_sha(revision, 40)
                    && vcs.as_ref().and_then(|vcs| vcs["git"]["sha1"].as_str()) == Some(revision)
            }
            None => {
                let expected = string(entry, "checksum")?;
                ensure!(
                    entry.get("upstreamRevision").is_some(),
                    "Checksum-bound supplement needs upstreamRevision: {}@{}",
                    pkg["name"],
                    pkg["version"]
                );
                is_sha(expected, 64) && checksum == Some(expected)
            }
        };
        ensure!(
            published && digest(value) == string(entry, "sha256")?,
            "Stale Cargo notice supplement: {}@{}",
            pkg["name"],
            pkg["version"]
        );
        let pin = match entry.get("upstreamRevision") {
            Some(_) => string(entry, "upstreamRevision")?,
            None => string(entry, "revision")?,
        };
        ensure!(
            is_sha(pin, 40) && url.starts_with("https://") && url.split('/').any(|part| part == pin),
            "Supplement URL is not revision-pinned: {url}"
        );
        notices.push(Notice {
            source: url.to_owned(),
            text: value.to_owned(),
            license: true,
        });
    }
    Ok(notices)
}

/// Registry checksums from the workspace Cargo.lock, keyed by name, version and source.
fn lock_checksums(workspace: &Path) -> Result<BTreeMap<(String, String, String), String>> {
    let lock: toml::Value = toml::from_str(
        &fs::read_to_string(workspace.join("Cargo.lock")).context("Reading Cargo.lock")?,
    )?;
    let mut checksums = BTreeMap::new();
    for package in lock
        .get("package")
        .and_then(toml::Value::as_array)
        .context("Cargo.lock has no packages")?
    {
        let field = |key: &str| package.get(key).and_then(toml::Value::as_str);
        if let (Some(name), Some(version), Some(source), Some(checksum)) =
            (field("name"), field("version"), field("source"), field("checksum"))
        {
            checksums.insert(
                (name.to_owned(), version.to_owned(), source.to_owned()),
                checksum.to_owned(),
            );
        }
    }
    Ok(checksums)
}

fn collect_document(
    metadata: &Value,
    root: &str,
    boundary: &str,
    supplements: &[Value],
) -> Result<Value> {
    let workspace = Path::new(string(metadata, "workspace_root")?).canonicalize()?;
    let members: BTreeSet<_> = array(metadata, "workspace_members")?
        .iter()
        .map(|v| v.as_str().context("Invalid Cargo workspace member"))
        .collect::<Result<_>>()?;
    let mut packages = Vec::new();
    let mut texts = BTreeMap::new();
    // Fixture workspaces in tests have no lockfile; a checksum-bound
    // supplement then fails as stale instead of matching.
    let checksums = if workspace.join("Cargo.lock").is_file() {
        lock_checksums(&workspace)?
    } else {
        BTreeMap::new()
    };
    for pkg in dependency_closure(metadata, root)? {
        let name = string(pkg, "name")?;
        let version = string(pkg, "version")?;
        let directory = Path::new(string(pkg, "manifest_path")?)
            .parent()
            .context("Cargo manifest has no parent")?;
        let license_file = pkg.get("license_file").and_then(Value::as_str);
        let mut notices = package_notices(directory, license_file)?;
        let checksum = pkg["source"].as_str().and_then(|source| {
            checksums
                .get(&(name.to_owned(), version.to_owned(), source.to_owned()))
                .map(String::as_str)
        });
        notices.extend(supplement_notices(pkg, directory, supplements, checksum)?);
        // Only native workspace members may inherit the application's license.
        if !notices.iter().any(|notice| notice.license)
            && pkg["source"].is_null()
            && members.contains(string(pkg, "id")?)
            && directory.canonicalize()?.starts_with(&workspace)
        {
            let manifest: toml::Value =
                toml::from_str(&fs::read_to_string(directory.join("Cargo.toml"))?)?;
            if manifest
                .get("package")
                .and_then(|p| p.get("license"))
                .and_then(|l| l.get("workspace"))
                .and_then(toml::Value::as_bool)
                == Some(true)
            {
                notices.push(Notice {
                    source: "workspace-root/LICENSE (inherited workspace.package.license)".into(),
                    text: text(&workspace.join("LICENSE"))?,
                    license: true,
                });
            }
        }
        ensure!(
            notices.iter().any(|notice| notice.license),
            "No upstream license text for Cargo dependency {name}@{version}; add a revision-pinned upstream supplement, not an SPDX template"
        );
        if name == "libsqlite3-sys" {
            let source = text(&directory.join("sqlite3/sqlite3.c"))?;
            let prefix: String = source.chars().take(4096).collect();
            let comments = Regex::new(r"(?s)/\*.*?\*/")?;
            let dedication = comments
                .find_iter(&prefix)
                .find(|m| m.as_str().contains("disclaims copyright"))
                .context("Missing SQLite public-domain dedication")?;
            notices.push(Notice {
                source: "sqlite3/sqlite3.c (opening dedication)".into(),
                text: dedication.as_str().into(),
                license: false,
            });
        }
        let mut refs = Vec::new();
        for notice in notices {
            let hash = digest(&notice.text);
            texts.insert(hash.clone(), notice.text);
            refs.push(json!({"source": notice.source, "sha256": hash}));
        }
        refs.sort_by(|a, b| {
            a["source"]
                .as_str()
                .cmp(&b["source"].as_str())
                .then_with(|| a["sha256"].as_str().cmp(&b["sha256"].as_str()))
        });
        packages.push(json!({"name": name, "version": version, "source": pkg["source"].as_str().unwrap_or("path dependency"), "license": pkg["license"], "notices": refs}));
    }
    packages.sort_by_cached_key(|p| {
        format!(
            "{}@{}:{}",
            p["name"].as_str().unwrap(),
            p["version"].as_str().unwrap(),
            p["source"].as_str().unwrap()
        )
    });
    Ok(json!({"schemaVersion": 1, "boundary": boundary, "packages": packages, "texts": texts}))
}

fn render(document: &Value) -> Result<String> {
    ensure!(
        document["schemaVersion"] == 1,
        "Malformed third-party notice schema"
    );
    let packages = array(document, "packages")?;
    ensure!(!packages.is_empty(), "Empty third-party package inventory");
    let texts = document["texts"]
        .as_object()
        .context("Missing notice texts")?;
    for (hash, value) in texts {
        let value = value.as_str().context("Non-text notice")?;
        ensure!(
            !value.trim().is_empty() && !value.contains('\0') && digest(value) == *hash,
            "Corrupt notice text: {hash}"
        );
    }
    let mut associations: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for pkg in packages {
        let name = string(pkg, "name")?;
        let version = string(pkg, "version")?;
        string(pkg, "source")?;
        let notices = array(pkg, "notices")?;
        ensure!(!notices.is_empty(), "Missing attribution for {name}");
        for notice in notices {
            let source = string(notice, "source")?;
            let hash = string(notice, "sha256")?;
            ensure!(
                texts.contains_key(hash),
                "Missing notice for {name}: {source}"
            );
            associations.entry(hash).or_default().push(format!(
                "{name}@{version} ({}): {source}",
                pkg["license"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("see upstream text")
            ));
        }
    }
    ensure!(
        associations.len() == texts.len(),
        "Unattributed notice text"
    );
    let mut output = format!("{}\n\n", string(document, "boundary")?);
    for (index, (hash, mut owners)) in associations.into_iter().enumerate() {
        owners.sort();
        if index != 0 {
            output.push('\n');
        }
        output.push_str(&format!(
            "{}\n{}\nSHA-256: {hash}\n\n{}\n",
            "=".repeat(72),
            owners.join("\n"),
            texts[hash].as_str().unwrap()
        ));
    }
    Ok(output)
}

const ABI_ASSET_DIRECTORY: &str = "packages/daemon/src/providers/opencode/assets";

/// The retained ABI bundles are data loaded by the external OpenCode CLI, not
/// a Node runtime dependency. Validate their frozen attribution without npm.
fn validate_embedded_inventory(directory: &Path, inventory: &Value) -> Result<()> {
    render(inventory)?;
    let marker = Regex::new(
        r"(?m)^// ((?:[^\r\n]*/)?node_modules/((?:@[^/\r\n]+/)?[^/\r\n]+)/[^\r\n]+)\r?$",
    )?;
    let mut assets = BTreeMap::new();
    let mut observed_packages = BTreeSet::new();
    for asset in array(inventory, "assets")? {
        let path = string(asset, "path")?;
        ensure!(
            matches!(path, "legacy-plugin.mjs" | "v2-plugin.mjs"),
            "Unexpected OpenCode ABI asset: {path}"
        );
        let source = text(&directory.join(path))?;
        ensure!(
            digest(&source) == string(asset, "sha256")?,
            "Changed OpenCode ABI asset {path}; refresh its reviewed attribution inventory"
        );
        let observed: BTreeSet<_> = marker
            .captures_iter(&source)
            .map(|capture| (capture[1].to_owned(), capture[2].to_owned()))
            .collect();
        ensure!(
            !observed.is_empty(),
            "No retained module provenance in {path}"
        );
        let mut declared = BTreeSet::new();
        for module in array(asset, "modules")? {
            let pair = (
                string(module, "source")?.to_owned(),
                string(module, "package")?.to_owned(),
            );
            ensure!(
                declared.insert(pair),
                "Duplicate module attribution in {path}"
            );
        }
        ensure!(
            observed == declared,
            "Incomplete or stale module attribution in {path}"
        );
        observed_packages.extend(observed.into_iter().map(|(_, package)| package));
        ensure!(
            assets.insert(path, source).is_none(),
            "Duplicate OpenCode ABI asset: {path}"
        );
    }
    ensure!(
        assets.len() == 2,
        "Both retained OpenCode ABI assets must be attributed"
    );
    let mut declared_packages = BTreeSet::new();
    for package in array(inventory, "packages")? {
        let name = string(package, "name")?;
        ensure!(
            declared_packages.insert(name.to_owned()),
            "Duplicate ABI package attribution: {name}"
        );
        let evidence = &package["manifestEvidence"];
        string(evidence, "path")?;
        let manifest_text = string(evidence, "text")?;
        ensure!(
            digest(manifest_text) == string(evidence, "sha256")?,
            "Corrupt retained manifest evidence for {name}"
        );
        let manifest: Value =
            serde_json::from_str(manifest_text).context("Parsing retained ABI package manifest")?;
        ensure!(
            manifest["name"] == name && manifest["license"] == package["license"],
            "ABI attribution disagrees with retained manifest for {name}"
        );
        string(&manifest, "version")?;
        match string(package, "versionAuthority")? {
            "unverified-bundle-version" => {
                ensure!(
                    package["version"] == "unverified",
                    "Unproven ABI package version must remain explicitly unverified: {name}"
                );
            }
            "embedded-version-and-lock" => {
                ensure!(
                    name == "zod",
                    "Unsupported embedded version proof for {name}"
                );
                let proof = &package["embeddedVersionEvidence"];
                let source = assets
                    .get(string(proof, "asset")?)
                    .context("Missing embedded version proof asset")?;
                let declaration = string(proof, "text")?;
                let section = format!("// {}\n{declaration}", string(proof, "source")?);
                ensure!(
                    source.contains(&section),
                    "Missing exact embedded version declaration for {name}"
                );
                let version_pattern = Regex::new(
                    r"^var version = \{\n  major: (\d+),\n  minor: (\d+),\n  patch: (\d+)\n\};$",
                )?;
                let version = version_pattern
                    .captures(declaration)
                    .context("Malformed embedded Zod version declaration")?;
                let version = format!("{}.{}.{}", &version[1], &version[2], &version[3]);
                ensure!(
                    package["version"] == version
                        && manifest["version"] == version
                        && package["lockEvidence"]["version"] == version,
                    "Embedded, manifest and lock versions disagree for {name}"
                );
                string(&package["lockEvidence"], "resolved")?;
                string(&package["lockEvidence"], "integrity")?;
            }
            authority => bail!("Unsupported ABI version authority: {authority}"),
        }
    }
    ensure!(
        declared_packages == observed_packages,
        "ABI package attributions do not match the retained bundle modules"
    );
    Ok(())
}

fn embedded_inventory(root: &Path) -> Result<Value> {
    let directory = root.join(ABI_ASSET_DIRECTORY);
    let inventory = read_json(&directory.join("third-party-notices.json"))?;
    validate_embedded_inventory(&directory, &inventory)?;
    Ok(inventory)
}

fn render_native_document(root: &Path, document: &Value) -> Result<String> {
    let embedded = embedded_inventory(root)?;
    ensure!(
        document["embeddedAbi"] == embedded,
        "Missing or stale embedded OpenCode ABI attribution in native build"
    );
    Ok(format!("{}\n{}", render(document)?, render(&embedded)?))
}

const GUI_BOUNDARY: &str = "Paseo Sessions application Cargo dependency closure for the packaged target (normal and build dependencies; dev-only edges excluded). Includes upstream license/notice files and nested vendored-component notices; this is not a source-offer or a toolchain/system-library compliance certification.";
const SUPPLEMENTS: &str = "packaging/cargo-notice-supplements.json";
const RESOURCE_NOTICES: &str = "packaging/resource-notices.json";

pub fn host_triple() -> Result<&'static str> {
    host_target(std::env::consts::OS, std::env::consts::ARCH)
}

/// Resolved Cargo metadata as seen by one compilation target.
pub fn cargo_metadata(target: &str) -> Result<Value> {
    let output = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args([
            "metadata",
            "--locked",
            "--format-version",
            "1",
            "--filter-platform",
            target,
        ])
        .current_dir(root()?)
        .output()
        .context("Running Cargo notice metadata collection")?;
    ensure!(
        output.status.success(),
        "Cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).context("Parsing Cargo notice metadata")
}

fn supplements(root: &Path) -> Result<Vec<Value>> {
    read_json(&root.join(SUPPLEMENTS))?
        .as_array()
        .cloned()
        .context("Invalid Cargo notice supplements")
}

pub fn collect(directory: &Path) -> Result<()> {
    let root = root()?;
    let metadata = cargo_metadata(host_triple()?)?;
    let mut document =
        collect_document(&metadata, "paseo-daemon", BOUNDARY, &supplements(&root)?)?;
    document["embeddedAbi"] = embedded_inventory(&root)?;
    let rendered = format!(
        "{}\n{}",
        render(&document)?,
        render(&document["embeddedAbi"])?
    );
    fs::create_dir_all(directory)?;
    write_json(&directory.join("RUST-THIRD-PARTY-NOTICES.json"), &document)?;
    fs::write(directory.join("RUST-THIRD-PARTY-NOTICES.txt"), rendered)?;
    Ok(())
}

/// A `resources/` directory Makepad resolves as `crate://<name>/resources`.
pub struct ResourceDirectory {
    pub name: String,
    pub version: String,
    pub path: PathBuf,
}

/// Only Makepad-aware packages register live files, so only their resource
/// directories are reachable through `crate://` dependencies.
pub fn gui_resource_directories(metadata: &Value) -> Result<Vec<ResourceDirectory>> {
    let root_package = array(metadata, "packages")?
        .iter()
        .find(|pkg| pkg["name"] == "paseo-gui" && pkg["source"].is_null())
        .context("Cargo metadata lacks workspace package paseo-gui")?;
    let mut result = Vec::new();
    for pkg in std::iter::once(root_package).chain(dependency_closure(metadata, "paseo-gui")?) {
        let name = string(pkg, "name")?;
        let makepad = name.starts_with("makepad-")
            || array(pkg, "dependencies")?.iter().any(|dep| {
                dep["kind"].is_null()
                    && dep["name"]
                        .as_str()
                        .is_some_and(|name| name.starts_with("makepad-"))
            });
        let path = Path::new(string(pkg, "manifest_path")?)
            .parent()
            .context("Cargo manifest has no parent")?
            .join("resources");
        if makepad && path.is_dir() {
            result.push(ResourceDirectory {
                name: name.to_owned(),
                version: string(pkg, "version")?.to_owned(),
                path,
            });
        }
    }
    result.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(result)
}

/// One file copied from a resource directory into a package.
pub struct PackagedResource {
    pub package: String,
    pub version: String,
    /// Slash-separated path below `resources/`.
    pub file: String,
    pub sha256: String,
}

fn is_font(file: &str) -> bool {
    Regex::new(r"(?i)\.(?:ttf|otf|ttc|woff2?)(?:\.\d+)?$")
        .expect("valid font pattern")
        .is_match(file)
}

/// License text pinned to an exact upstream git revision in its URL, or to the
/// digest of an immutable release archive it was taken from (`archiveSha256`).
fn pinned(notice: &Value) -> Result<bool> {
    let source = string(notice, "source")?;
    let revision = source
        .split(['/', '#'])
        .any(|part| part.len() == 40 && part.bytes().all(|b| b.is_ascii_hexdigit()));
    let archive = notice["archiveSha256"]
        .as_str()
        .is_some_and(|digest| digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()));
    Ok(source.starts_with("https://") && (revision || archive))
}

/// Font and vendored-component attribution, bound to the exact packaged bytes.
fn resource_document(manifest: &Value, packaged: &[PackagedResource], platform: &str) -> Result<Value> {
    ensure!(
        manifest["schemaVersion"] == 1,
        "Unsupported resource notice schema"
    );
    let texts = manifest["texts"]
        .as_object()
        .context("Missing resource notice texts")?;
    let mut packages = Vec::new();
    let mut used = BTreeMap::new();
    let mut attach = |entry: &Value, name: String, version: String| -> Result<()> {
        let notices = array(entry, "notices")?;
        ensure!(!notices.is_empty(), "Missing attribution for {name}");
        for notice in notices {
            let source = string(notice, "source")?;
            let hash = string(notice, "sha256")?;
            ensure!(pinned(notice)?, "Resource notice is not revision-pinned: {source}");
            let value = texts
                .get(hash)
                .and_then(Value::as_str)
                .with_context(|| format!("Missing notice text for {name}: {source}"))?;
            ensure!(digest(value) == hash, "Corrupt resource notice text: {hash}");
            used.insert(hash.to_owned(), json!(value));
        }
        packages.push(json!({"name": name, "version": version, "source": "packaged resource", "license": entry["license"], "notices": notices}));
        Ok(())
    };
    let fonts = array(manifest, "fonts")?;
    for resource in packaged.iter().filter(|resource| is_font(&resource.file)) {
        let entry = fonts
            .iter()
            // Workspace resources omit `version`; the digest still binds the bytes.
            .find(|entry| {
                entry["crate"] == resource.package.as_str()
                    && entry
                        .get("version")
                        .is_none_or(|version| version == resource.version.as_str())
                    && entry["file"] == resource.file.as_str()
            })
            .with_context(|| {
                format!(
                    "No license entry in {RESOURCE_NOTICES} for font {}@{}/resources/{}",
                    resource.package, resource.version, resource.file
                )
            })?;
        ensure!(
            entry["sha256"] == resource.sha256.as_str(),
            "Font {}@{}/resources/{} differs from its reviewed license entry",
            resource.package,
            resource.version,
            resource.file
        );
        attach(
            entry,
            format!("{}/resources/{}", resource.package, resource.file),
            resource.version.clone(),
        )?;
    }
    for component in array(manifest, "components")? {
        if array(component, "platforms")?
            .iter()
            .any(|value| value == platform)
        {
            let path = root()?.join(string(component, "path")?);
            ensure!(path.exists(), "Missing vendored component {}", path.display());
            attach(component, string(component, "name")?.to_owned(), "vendored".into())?;
        }
    }
    Ok(json!({"schemaVersion": 1, "boundary": manifest["boundary"], "packages": packages, "texts": used}))
}

/// A third-party resource deliberately left out of every package. It is bound
/// to the exact reviewed bytes, so an upstream change is re-reviewed rather
/// than silently excluded or silently shipped.
pub struct ExcludedResource {
    pub package: String,
    pub version: String,
    pub file: String,
    pub sha256: String,
}

pub fn excluded_resources() -> Result<Vec<ExcludedResource>> {
    let manifest = read_json(&root()?.join(RESOURCE_NOTICES))?;
    let mut result = Vec::new();
    for entry in array(&manifest, "excluded")? {
        string(entry, "reason")?;
        result.push(ExcludedResource {
            package: string(entry, "crate")?.to_owned(),
            version: string(entry, "version")?.to_owned(),
            file: string(entry, "file")?.to_owned(),
            sha256: string(entry, "sha256")?.to_owned(),
        });
    }
    Ok(result)
}

/// Whether a resource file is excluded; a name match with different bytes is an error.
pub fn is_excluded(
    excluded: &[ExcludedResource],
    package: &str,
    version: &str,
    file: &str,
    sha256: &str,
) -> Result<bool> {
    let Some(entry) = excluded
        .iter()
        .find(|entry| entry.package == package && entry.version == version && entry.file == file)
    else {
        return Ok(false);
    };
    ensure!(
        entry.sha256 == sha256,
        "Excluded resource {package}@{version}/resources/{file} changed since review; update {RESOURCE_NOTICES}"
    );
    Ok(true)
}

/// Client source roots and the Cargo package `crate://self` means inside them.
const CLIENT_SOURCES: [(&str, &str); 3] = [
    ("packages/gui/src", "paseo-gui"),
    ("packages/gui-core/src", "paseo-gui-core"),
    ("packages/gui-platform/src", "paseo-gui-platform"),
];

/// `crate://<crate>/resources/<file>` references in one source text that name
/// no staged resource, as `line: reference`.
fn missing_references(source: &str, package: &str, packaged: &[PackagedResource]) -> Result<Vec<String>> {
    let reference = Regex::new(r#"crate://([A-Za-z0-9_]+)/resources/([^"'\s)]+)"#)?;
    let mut missing = Vec::new();
    for (index, line) in source.lines().enumerate() {
        for capture in reference.captures_iter(line) {
            let owner = match &capture[1] {
                "self" => package.replace('-', "_"),
                name => name.to_owned(),
            };
            let file = &capture[2];
            if !packaged
                .iter()
                .any(|resource| resource.package.replace('-', "_") == owner && resource.file == file)
            {
                missing.push(format!("{}: crate://{}/resources/{file}", index + 1, &capture[1]));
            }
        }
    }
    Ok(missing)
}

/// Every resource client sources reference must be staged. Makepad loads fonts
/// lazily and panics on a missing dependency when a style uses it, so an
/// excluded or absent font would otherwise surface only at runtime.
pub fn ensure_references_staged(packaged: &[PackagedResource]) -> Result<()> {
    let root = root()?;
    let mut missing = Vec::new();
    for (crate_directory, package) in CLIENT_SOURCES {
        let directory = root.join(crate_directory);
        if !directory.is_dir() {
            continue;
        }
        for entry in WalkDir::new(&directory).follow_links(false).sort_by_file_name() {
            let entry = entry?;
            if !entry.file_type().is_file()
                || entry.path().extension().is_none_or(|extension| extension != "rs")
            {
                continue;
            }
            let source = fs::read_to_string(entry.path())
                .with_context(|| format!("Reading {}", entry.path().display()))?;
            let relative = entry.path().strip_prefix(&root)?.display().to_string();
            for reference in missing_references(&source, package, packaged)? {
                missing.push(format!("{relative}:{reference}"));
            }
        }
    }
    ensure!(
        missing.is_empty(),
        "Client sources reference resources that are not staged (excluded in {RESOURCE_NOTICES} or absent):\n{}",
        missing.join("\n")
    );
    Ok(())
}

/// Complete application notices: GUI Cargo closure plus packaged fonts and
/// vendored platform components.
pub fn render_app(metadata: &Value, packaged: &[PackagedResource], platform: &str) -> Result<String> {
    let root = root()?;
    let gui = collect_document(metadata, "paseo-gui", GUI_BOUNDARY, &supplements(&root)?)?;
    let resources = resource_document(&read_json(&root.join(RESOURCE_NOTICES))?, packaged, platform)?;
    let mut output = render(&gui)?;
    if !array(&resources, "packages")?.is_empty() {
        output.push('\n');
        output.push_str(&render(&resources)?);
    }
    Ok(output)
}

fn date_notice(source: &str) -> Result<&str> {
    let length: usize = source
        .split_inclusive('\n')
        .take_while(|line| line.starts_with("//") && line.ends_with('\n'))
        .map(str::len)
        .sum();
    let header = &source[..length];
    ensure!(
        header == DATE_HEADER,
        "Native DateParser must retain its exact complete upstream copyright and BSD-3-Clause notice"
    );
    Ok(header)
}

pub fn render_runtime(directory: &Path) -> Result<String> {
    let document = read_json(&directory.join("RUST-THIRD-PARTY-NOTICES.json"))?;
    ensure!(
        document["boundary"] == BOUNDARY,
        "Unexpected Cargo notice boundary"
    );
    let root = root()?;
    let source = text(&root.join("packages/daemon/src/agent/date_parse.rs"))?;
    Ok(format!(
        "{}\n{}",
        date_notice(&source)?,
        render_native_document(&root, &document)?
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn abi_fixture() -> (tempfile::TempDir, Value) {
        let directory = tempfile::tempdir().unwrap();
        let source = "// node_modules/example/index.js\nexport const example = 1;\n";
        let manifest = r#"{"name":"example","version":"1.2.3","license":"MIT"}"#;
        let notice = "Retained upstream license text";
        let mut assets = Vec::new();
        for path in ["legacy-plugin.mjs", "v2-plugin.mjs"] {
            fs::write(directory.path().join(path), source).unwrap();
            assets.push(json!({"path":path,"sha256":digest(source),"modules":[{"source":"node_modules/example/index.js","package":"example"}]}));
        }
        let inventory = json!({"schemaVersion":1,"boundary":"Test ABI attribution","assets":assets,
            "packages":[{"name":"example","version":"unverified","source":"retained input","license":"MIT",
                "versionAuthority":"unverified-bundle-version",
                "manifestEvidence":{"path":"node_modules/example/package.json","sha256":digest(manifest),"text":manifest},
                "notices":[{"source":"LICENSE","sha256":digest(notice)}]}],
            "texts":{(digest(notice)):notice}});
        (directory, inventory)
    }

    #[test]
    fn abi_attribution_is_offline_and_rejects_missing_or_corrupt_evidence() {
        let (directory, inventory) = abi_fixture();
        validate_embedded_inventory(directory.path(), &inventory).unwrap();
        for mutation in [
            "module", "package", "asset", "license", "manifest", "version",
        ] {
            let mut changed = inventory.clone();
            match mutation {
                "module" => {
                    changed["assets"][0]["modules"] = json!([]);
                }
                "package" => {
                    changed["packages"] = json!([]);
                }
                "asset" => {
                    changed["assets"].as_array_mut().unwrap().pop();
                }
                "license" => {
                    *changed["texts"]
                        .as_object_mut()
                        .unwrap()
                        .values_mut()
                        .next()
                        .unwrap() = json!("changed");
                }
                "manifest" => {
                    changed["packages"][0]["manifestEvidence"]["text"] = json!("changed");
                }
                "version" => {
                    changed["packages"][0]["version"] = json!("1.2.3");
                }
                _ => unreachable!(),
            }
            assert!(
                validate_embedded_inventory(directory.path(), &changed).is_err(),
                "{mutation}"
            );
        }
        fs::write(directory.path().join("v2-plugin.mjs"), "// changed bundle").unwrap();
        assert!(validate_embedded_inventory(directory.path(), &inventory).is_err());
    }

    #[test]
    fn retained_opencode_assets_have_complete_digest_bound_attribution() {
        embedded_inventory(&root().unwrap()).unwrap();
    }

    #[test]
    fn abi_attribution_rejects_checkout_line_ending_changes() {
        let (directory, inventory) = abi_fixture();
        let path = directory.path().join("legacy-plugin.mjs");
        let source = fs::read_to_string(&path).unwrap();
        fs::write(path, source.replace('\n', "\r\n")).unwrap();
        assert!(validate_embedded_inventory(directory.path(), &inventory).is_err());
    }

    #[test]
    fn supported_hosts_are_explicit() {
        for os in ["linux", "macos", "windows"] {
            for arch in ["x86_64", "aarch64"] {
                assert!(host_target(os, arch).is_ok());
            }
        }
        assert!(host_target("linux", "arm").is_err());
    }

    #[test]
    fn closure_keeps_transitive_build_and_normal_edges_not_dev_only() {
        let metadata = json!({"packages": [
            {"id":"root", "name":"paseo-daemon"}, {"id":"normal"}, {"id":"build"}, {"id":"nested"}, {"id":"dev"}
        ], "resolve":{"nodes":[
            {"id":"root", "deps":[{"pkg":"normal","dep_kinds":[{"kind":null}]},{"pkg":"build","dep_kinds":[{"kind":"build"}]},{"pkg":"dev","dep_kinds":[{"kind":"dev"}]}]},
            {"id":"normal","deps":[]}, {"id":"build","deps":[{"pkg":"nested","dep_kinds":[{"kind":null}]}]},
            {"id":"nested","deps":[{"pkg":"root","dep_kinds":[{"kind":null}]}]}, {"id":"dev","deps":[]}
        ]}});
        let ids: Vec<_> = dependency_closure(&metadata, "paseo-daemon")
            .unwrap()
            .iter()
            .map(|p| p["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["build", "nested", "normal"]);
        let mut incomplete = metadata.clone();
        incomplete["resolve"]["nodes"]
            .as_array_mut()
            .unwrap()
            .remove(3);
        assert!(dependency_closure(&incomplete, "paseo-daemon").is_err());
    }

    #[test]
    fn nested_notices_and_license_directories_are_retained() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("vendor/licenses")).unwrap();
        fs::create_dir_all(dir.path().join("target")).unwrap();
        fs::write(
            dir.path().join("vendor/licenses/component.txt"),
            "component license",
        )
        .unwrap();
        fs::write(dir.path().join("vendor/NOTICE"), "copyright").unwrap();
        fs::write(dir.path().join("target/LICENSE"), "excluded").unwrap();
        let notices = package_notices(dir.path(), None).unwrap();
        assert_eq!(notices.len(), 2);
        assert!(
            notices
                .iter()
                .any(|n| n.source == "vendor/licenses/component.txt" && n.license)
        );
    }

    #[test]
    fn renderer_verifies_hashes_and_source_attribution() {
        let hash = digest("license text");
        let mut document = json!({"schemaVersion":1,"boundary":BOUNDARY,"packages":[{"name":"crate","version":"1","source":"registry","notices":[{"source":"LICENSE","sha256":hash}]}],"texts":{}});
        document["texts"][&hash] = json!("license text");
        assert!(
            render(&document)
                .unwrap()
                .contains("crate@1 (see upstream text): LICENSE")
        );
        document["packages"][0]["notices"][0]["source"] = json!("");
        assert!(render(&document).is_err());
        document["packages"][0]["notices"][0]["source"] = json!("LICENSE");
        document["texts"][&hash] = json!("tampered");
        assert!(render(&document).is_err());
    }

    #[test]
    fn date_header_requires_all_upstream_terms() {
        assert_eq!(
            date_notice(&format!("{DATE_HEADER}\nfn main() {{}}")).unwrap(),
            DATE_HEADER
        );
        assert!(date_notice(&DATE_HEADER.replace('\n', "\r\n")).is_err());
        let source = text(
            &root()
                .unwrap()
                .join("packages/daemon/src/agent/date_parse.rs"),
        )
        .unwrap();
        assert_eq!(date_notice(&source).unwrap(), DATE_HEADER);
        assert!(
            date_notice(&DATE_HEADER.replace(
                "// * Neither the name of Google Inc. nor the names of its contributors may be\n",
                ""
            ))
            .is_err()
        );
    }

    #[test]
    fn workspace_license_fallback_never_applies_to_external_dependencies() {
        let dir = tempfile::tempdir().unwrap();
        let member = dir.path().join("member");
        fs::create_dir(&member).unwrap();
        fs::write(dir.path().join("LICENSE"), "workspace upstream license").unwrap();
        fs::write(
            member.join("Cargo.toml"),
            "[package]\nname = \"member\"\nlicense.workspace = true\n",
        )
        .unwrap();
        let mut metadata = json!({
            "workspace_root": dir.path(), "workspace_members": ["root", "member"],
            "packages": [
                {"id":"root", "name":"paseo-daemon"},
                {"id":"member", "name":"member", "version":"1", "manifest_path":member.join("Cargo.toml"), "source":null, "license":"Apache-2.0"}
            ],
            "resolve":{"nodes":[
                {"id":"root", "deps":[{"pkg":"member","dep_kinds":[{"kind":null}]}]},
                {"id":"member", "deps":[]}
            ]}
        });
        let document = collect_document(&metadata, "paseo-daemon", BOUNDARY, &[]).unwrap();
        assert!(
            render(&document)
                .unwrap()
                .contains("workspace-root/LICENSE (inherited workspace.package.license)")
        );
        metadata["packages"][1]["source"] = json!("registry+https://example.com");
        assert!(collect_document(&metadata, "paseo-daemon", BOUNDARY, &[]).is_err());
        metadata["packages"][1]["source"] = Value::Null;
        metadata["workspace_members"] = json!(["root"]);
        assert!(collect_document(&metadata, "paseo-daemon", BOUNDARY, &[]).is_err());
    }

    #[test]
    fn supplement_requires_matching_revision_and_digest() {
        let dir = tempfile::tempdir().unwrap();
        let revision = "0123456789012345678901234567890123456789";
        fs::write(
            dir.path().join(".cargo_vcs_info.json"),
            json!({"git":{"sha1":revision}}).to_string(),
        )
        .unwrap();
        let pkg = json!({"name":"crate","version":"1","repository":"https://example.com/repo","source":"registry+https://github.com/rust-lang/crates.io-index"});
        let mut supplement = pkg.clone();
        supplement.as_object_mut().unwrap().remove("source");
        supplement["revision"] = json!(revision);
        supplement["url"] = json!(format!("https://example.com/repo/{revision}/LICENSE"));
        supplement["text"] = json!("upstream license");
        supplement["sha256"] = json!(digest("upstream license"));
        assert_eq!(
            supplement_notices(&pkg, dir.path(), &[supplement.clone()], None)
                .unwrap()
                .len(),
            1
        );
        // A vendored path copy must ship its own license file instead.
        let mut vendored = pkg.clone();
        vendored["source"] = Value::Null;
        assert!(
            supplement_notices(&vendored, dir.path(), &[supplement.clone()], None)
                .unwrap()
                .is_empty()
        );
        assert!(
            supplement_notices(&pkg, dir.path(), &[supplement.clone(), supplement.clone()], None)
                .is_err()
        );
        let upstream = "2223456789012345678901234567890123456789";
        let mut fork = supplement.clone();
        fork["upstreamRevision"] = json!(upstream);
        assert!(supplement_notices(&pkg, dir.path(), &[fork.clone()], None).is_err());
        fork["url"] = json!(format!("https://example.com/upstream/{upstream}/LICENSE"));
        assert_eq!(supplement_notices(&pkg, dir.path(), &[fork.clone()], None).unwrap().len(), 1);
        // Without VCS information the registry checksum binds the published
        // bytes; the upstream revision still pins the license URL.
        let checksum = "c".repeat(64);
        let mut unversioned = fork.clone();
        unversioned.as_object_mut().unwrap().remove("revision");
        unversioned["checksum"] = json!(checksum);
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(
            supplement_notices(&pkg, empty.path(), &[unversioned.clone()], Some(&checksum))
                .unwrap()
                .len(),
            1
        );
        assert!(supplement_notices(&pkg, empty.path(), &[unversioned.clone()], Some(&"d".repeat(64))).is_err());
        assert!(supplement_notices(&pkg, empty.path(), &[unversioned.clone()], None).is_err());
        unversioned.as_object_mut().unwrap().remove("upstreamRevision");
        assert!(supplement_notices(&pkg, empty.path(), &[unversioned], Some(&checksum)).is_err());
        supplement["revision"] = json!("1123456789012345678901234567890123456789");
        assert!(supplement_notices(&pkg, dir.path(), &[supplement], None).is_err());
    }

    #[test]
    fn packaged_fonts_require_digest_bound_pinned_licenses() {
        let license = "font license";
        let source = format!("https://example.com/{}/OFL.txt", "a".repeat(40));
        let manifest = json!({"schemaVersion":1,"boundary":"Fonts","components":[],
            "fonts":[{"crate":"makepad-widgets","version":"1.0.0","file":"Font.ttf","sha256":"11","license":"OFL-1.1",
                "notices":[{"source":source,"sha256":digest(license)}]}],
            "texts":{(digest(license)):license}});
        let resource = |file: &str, sha256: &str| PackagedResource {
            package: "makepad-widgets".into(),
            version: "1.0.0".into(),
            file: file.into(),
            sha256: sha256.into(),
        };
        let document =
            resource_document(&manifest, &[resource("Font.ttf", "11"), resource("icons/a.svg", "22")], "linux")
                .unwrap();
        assert!(render(&document).unwrap().contains("makepad-widgets/resources/Font.ttf@1.0.0"));
        assert!(resource_document(&manifest, &[resource("Font.ttf", "12")], "linux").is_err());
        assert!(resource_document(&manifest, &[resource("Other.ttf.2", "11")], "linux").is_err());
        let mut unpinned = manifest.clone();
        unpinned["fonts"][0]["notices"][0]["source"] = json!("https://example.com/main/OFL.txt");
        assert!(resource_document(&unpinned, &[resource("Font.ttf", "11")], "linux").is_err());
    }

    #[test]
    fn exclusions_match_exact_reviewed_bytes() {
        let excluded = [ExcludedResource {
            package: "makepad-widgets".into(),
            version: "1.0.0".into(),
            file: "LiberationMono-Regular.ttf".into(),
            sha256: "aa".into(),
        }];
        assert!(is_excluded(&excluded, "makepad-widgets", "1.0.0", "LiberationMono-Regular.ttf", "aa").unwrap());
        // Another version or crate is a different, unreviewed file: it is packaged
        // and must pass the font license check instead.
        assert!(!is_excluded(&excluded, "makepad-widgets", "1.0.1", "LiberationMono-Regular.ttf", "aa").unwrap());
        assert!(!is_excluded(&excluded, "paseo-gui", "1.0.0", "LiberationMono-Regular.ttf", "aa").unwrap());
        // Changed bytes under the reviewed name need a new review.
        assert!(is_excluded(&excluded, "makepad-widgets", "1.0.0", "LiberationMono-Regular.ttf", "bb").is_err());
    }

    #[test]
    fn referenced_resources_must_be_staged() {
        let staged = |package: &str, file: &str| PackagedResource {
            package: package.into(),
            version: "1".into(),
            file: file.into(),
            sha256: "a".repeat(64),
        };
        let packaged = [
            staged("paseo-gui", "fonts/NotoSansKR-Regular.otf"),
            staged("makepad-widgets", "IBMPlexSans-Text.ttf"),
        ];
        let source = r#"
            latin = font("crate://makepad_widgets/resources/IBMPlexSans-Text.ttf", -0.1, 0.0),
            korean = font("crate://self/resources/fonts/NotoSansKR-Regular.otf", 0.0, 0.0),
            chinese = font("crate://makepad_fonts_chinese_regular/resources/LXGWWenKaiRegular.ttf",
        "#;
        assert_eq!(
            missing_references(source, "paseo-gui", &packaged).unwrap(),
            ["4: crate://makepad_fonts_chinese_regular/resources/LXGWWenKaiRegular.ttf"]
        );
        // `self` resolves to the crate whose sources reference it.
        assert_eq!(missing_references(source, "paseo-gui-platform", &packaged).unwrap().len(), 2);
    }
}
