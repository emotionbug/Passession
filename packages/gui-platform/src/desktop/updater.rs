//! Native application updates (original desktop_updater.dart). Trust is GitHub
//! HTTPS + SHA512 with the installed platform signing identity retained.

use crate::action::PlatformError;
use base64::Engine as _;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha512};
use std::{
    cmp::Ordering,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::io::AsyncWriteExt;

const APP_ID: &str = crate::paths::DESKTOP_APP_ID;
const DEFAULT_CHANNEL: &str = match option_env!("PASEO_RELEASE_CHANNEL") {
    Some(channel) => channel,
    None => "stable",
};
const REPOSITORY: &str = match option_env!("PASEO_UPDATE_REPOSITORY") {
    Some(repository) => repository,
    None => "emotionbug/Passession",
};
/// Release-feed runtime identity: manifests are `<runtime>-<channel>-<platform>-<arch>.json`
/// with a matching `runtime` field. The default keeps the existing native feed so
/// installed clients and this build upgrade through the same manifests.
const FEED_RUNTIME: &str = match option_env!("PASEO_UPDATE_RUNTIME") {
    Some(runtime) => runtime,
    None => "flutter",
};
const REPLACE_HELPER: &str = include_str!("../../assets/replace.sh");

#[derive(Clone, PartialEq)]
struct Release {
    version: String,
    channel: String,
    name: String,
    format: String,
    size: u64,
    digest: String,
    url: url::Url,
    notes: Option<String>,
    date: Option<String>,
    rollout: Option<f64>,
}

impl Release {
    fn key(&self) -> String {
        format!("{}:{}:{}:{}:{}:{}", self.channel, self.version, self.format, self.digest, self.size, self.url)
    }
}

struct Prepared {
    release: Release,
    directory: PathBuf,
    file: PathBuf,
    app: Option<PathBuf>,
}

struct Replacement {
    directory: PathBuf,
    target: PathBuf,
    staged: PathBuf,
    backup: PathBuf,
}

#[derive(Default)]
struct State {
    admitted: Option<Release>,
    prepared: Option<Prepared>,
    last_error: Option<String>,
    install_log: Option<String>,
}

pub(crate) struct Updater {
    version: String,
    state: tokio::sync::Mutex<State>,
    client: reqwest::Client,
}

fn platform() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(windows) {
        "windows"
    } else {
        "linux"
    }
}

fn arch() -> Result<&'static str, PlatformError> {
    if cfg!(target_arch = "x86_64") {
        Ok("x64")
    } else if cfg!(target_arch = "aarch64") {
        Ok("arm64")
    } else {
        Err(PlatformError::new("update", format!("No native updater package for {}.", std::env::consts::ARCH)))
    }
}

fn update_error(message: impl Into<String>) -> PlatformError {
    PlatformError::new("update", message)
}

impl Updater {
    pub fn new(version: String) -> Self {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(30))
            .user_agent("Paseo-Flutter-Updater")
            .build()
            .expect("reqwest client");
        Self { version, state: tokio::sync::Mutex::new(State::default()), client }
    }

    pub fn default_channel() -> &'static str {
        DEFAULT_CHANNEL
    }

    pub async fn diagnostics(&self) -> Value {
        let state = self.state.lock().await;
        json!({
            "currentVersion": self.version,
            "releaseChannel": DEFAULT_CHANNEL,
            "repository": REPOSITORY,
            "platform": platform(),
            "abi": format!("{}_{}", std::env::consts::OS, std::env::consts::ARCH),
            "configured": !self.version.is_empty(),
            "manifestPattern": format!("{FEED_RUNTIME}-<channel>-{}-<arch>.json", platform()),
            "lastError": state.last_error,
            "preparedVersion": state.prepared.as_ref().map(|p| p.release.version.clone()),
            "installLog": state.install_log,
        })
    }

    pub async fn check(&self, channel: &str, manual: bool) -> Value {
        let mut state = self.state.lock().await;
        self.check_locked(&mut state, channel, manual).await
    }

    async fn check_locked(&self, state: &mut State, channel: &str, manual: bool) -> Value {
        let mut found: Option<Release> = None;
        let result: Result<Value, PlatformError> = async {
            self.validate_configuration(channel)?;
            let release = self.discover(channel).await?;
            found = Some(release.clone());
            let newer = Version::parse(&release.version)?.cmp(&Version::parse(&self.version)?) == Ordering::Greater;
            let admitted_same = state.admitted.as_ref().map(Release::key) == Some(release.key());
            if !newer || (!manual && !admit(&release).await? && !admitted_same) {
                state.admitted = None;
                discard_prepared(state).await;
                state.last_error = None;
                return Ok(self.check_result(state, None, None));
            }
            state.admitted = Some(release.clone());
            self.prepare(state, &release).await?;
            state.last_error = None;
            Ok(self.check_result(state, Some(&release), None))
        }
        .await;
        match result {
            Ok(value) => value,
            Err(error) => {
                state.last_error = Some(error.message.clone());
                let keep = found.filter(|r| state.admitted.as_ref().map(Release::key) == Some(r.key()));
                self.check_result(state, keep.as_ref(), Some(error.message))
            }
        }
    }

    fn check_result(&self, state: &State, release: Option<&Release>, error: Option<String>) -> Value {
        json!({
            "hasUpdate": release.is_some(),
            "readyToInstall": release.is_some_and(|r| state.prepared.as_ref().map(|p| p.release.key()) == Some(r.key())),
            "currentVersion": self.version,
            "latestVersion": release.map(|r| r.version.clone()).unwrap_or_else(|| self.version.clone()),
            "body": release.and_then(|r| r.notes.clone()),
            "date": release.and_then(|r| r.date.clone()),
            "errorMessage": error,
        })
    }

    fn install_result(installed: bool, launched: bool, version: &str, message: &str) -> Value {
        json!({"installed": installed, "installerLaunched": launched, "version": version, "message": message})
    }

    /// `before_quit` runs the guarded quit preparation; its failure or refusal
    /// aborts the installation before anything is replaced.
    pub async fn install<F, Fut>(&self, channel: &str, before_quit: F) -> Value
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<(), PlatformError>>,
    {
        let mut state = self.state.lock().await;
        let result: Result<Value, PlatformError> = async {
            let checked = self.check_locked(&mut state, channel, true).await;
            if !checked["errorMessage"].is_null() || checked["hasUpdate"] != true {
                let message = checked["errorMessage"].as_str().unwrap_or("No update available.").to_owned();
                return Ok(Self::install_result(false, false, &self.version, &message));
            }
            // A newer publication or withdrawn package invalidates an older download.
            let current = self.discover(channel).await?;
            if state.prepared.as_ref().map(|p| p.release.key()) != Some(current.key()) {
                discard_prepared(&mut state).await;
                return Err(update_error("The release changed during download. Check for updates again."));
            }
            let mut prepared = state.prepared.take().expect("prepared release");
            let staged = async {
                validate_file(&prepared.file, &prepared.release).await?;
                validate_native(&mut prepared).await?;
                stage_replacement(&prepared).await
            }
            .await;
            let replacement = match staged {
                Ok(replacement) => replacement,
                Err(error) => {
                    state.prepared = Some(prepared);
                    return Err(error);
                }
            };
            let mut helper_paths = None;
            let prelude = async {
                if replacement.is_some() {
                    let helper = prepared.directory.join("replace.sh");
                    tokio::fs::write(&helper, REPLACE_HELPER).await?;
                    let log = prepared.directory.join("install.log");
                    tokio::fs::write(&log, format!("Replacing {}\n", prepared.release.version)).await?;
                    helper_paths = Some((helper, log));
                }
                Ok::<_, PlatformError>(())
            }
            .await;
            if let Some((_, log)) = &helper_paths {
                state.install_log = Some(log.display().to_string());
            }
            let ready = match prelude {
                Ok(()) => before_quit().await,
                Err(error) => Err(error),
            };
            if let Err(error) = ready {
                if let Some(replacement) = &replacement {
                    let _ = tokio::fs::remove_dir_all(&replacement.directory).await;
                }
                state.prepared = Some(prepared);
                return Err(error);
            }
            let version = prepared.release.version.clone();
            if let (Some(replacement), Some((helper, log))) = (&replacement, &helper_paths) {
                let spawned = std::process::Command::new("/bin/sh")
                    .arg(helper)
                    .arg(std::process::id().to_string())
                    .arg(&replacement.target)
                    .arg(&replacement.staged)
                    .arg(&replacement.backup)
                    .arg(platform())
                    .arg(log)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn();
                if let Err(error) = spawned {
                    let _ = tokio::fs::remove_dir_all(&replacement.directory).await;
                    state.prepared = Some(prepared);
                    return Err(update_error(format!("Could not start the update helper: {error}")));
                }
                state.prepared = Some(prepared);
                return Ok(Self::install_result(
                    false,
                    true,
                    &version,
                    "Update verified. The application will close, replace itself, and restart.",
                ));
            }
            if cfg!(windows) {
                let script = format!(
                    "$ErrorActionPreference = 'Stop'; Start-Process -FilePath {} -PassThru | Select-Object -ExpandProperty Id",
                    powershell_quote(&prepared.file.display().to_string())
                );
                let launched = powershell(&script).await;
                state.prepared = Some(prepared);
                launched?;
                return Ok(Self::install_result(
                    false,
                    true,
                    &version,
                    "The verified installer has started. Complete its installation prompts.",
                ));
            }
            let (command, flag) = if prepared.release.format == "deb" { ("/usr/bin/dpkg", "-i") } else { ("/usr/bin/rpm", "-U") };
            let installed = run("/usr/bin/pkexec", &[command.as_ref(), flag.as_ref(), prepared.file.as_os_str()]).await;
            state.prepared = Some(prepared);
            installed?;
            state.last_error = None;
            Ok(Self::install_result(
                true,
                true,
                &version,
                "The native package manager installed the update. Restart the application.",
            ))
        }
        .await;
        match result {
            Ok(value) => value,
            Err(error) => {
                state.last_error = Some(error.message.clone());
                Self::install_result(false, false, &self.version, &format!("Update failed: {}", error.message))
            }
        }
    }

    fn validate_configuration(&self, channel: &str) -> Result<(), PlatformError> {
        if self.version.is_empty() {
            return Err(update_error(
                "This build has no PASEO_VERSION release identity. Install a packaged native release.",
            ));
        }
        Version::parse(&self.version)?;
        if channel != "stable" && channel != "beta" {
            return Err(PlatformError::invalid(format!("Unknown release channel: {channel}")));
        }
        let valid_part = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c));
        match REPOSITORY.split_once('/') {
            Some((owner, repo)) if valid_part(owner) && valid_part(repo) => Ok(()),
            _ => Err(update_error("PASEO_UPDATE_REPOSITORY must be a GitHub owner/repository.")),
        }
    }

    async fn discover(&self, channel: &str) -> Result<Release, PlatformError> {
        let format = installed_format().await?;
        let arch = arch()?;
        let name = format!("{FEED_RUNTIME}-{channel}-{}-{arch}.json", platform());
        let mut newest: Option<(Version, Release)> = None;
        // Pagination is bounded; a missing channel is an explicit error.
        for page in 1..=10 {
            let url = url::Url::parse_with_params(
                &format!("https://api.github.com/repos/{REPOSITORY}/releases"),
                &[("per_page", "100"), ("page", &page.to_string())],
            )
            .map_err(|e| update_error(e.to_string()))?;
            let data = self.json(url).await?;
            let Value::Array(releases) = data else {
                return Err(update_error("GitHub did not return a release list."));
            };
            for release in &releases {
                let release = object(release)?;
                if release.get("draft") == Some(&Value::Bool(true))
                    || (channel == "stable" && release.get("prerelease") == Some(&Value::Bool(true)))
                {
                    continue;
                }
                let assets: Vec<&Map<String, Value>> = match release.get("assets") {
                    Some(Value::Array(items)) => items.iter().map(object).collect::<Result<_, _>>()?,
                    _ => Vec::new(),
                };
                let manifests: Vec<_> = assets.iter().filter(|a| a.get("name").and_then(Value::as_str) == Some(name.as_str())).collect();
                if manifests.is_empty() {
                    continue;
                }
                if manifests.len() != 1 {
                    return Err(update_error(format!("Ambiguous release manifest {name}.")));
                }
                let manifest_value = self.json(asset_url(manifests[0])?).await?;
                let manifest = object(&manifest_value)?;
                let field = |key: &str| manifest.get(key).and_then(Value::as_str);
                if manifest.get("schemaVersion").and_then(Value::as_i64) != Some(1)
                    || field("runtime") != Some(FEED_RUNTIME)
                    || field("appId") != Some(APP_ID)
                    || field("channel") != Some(channel)
                    || field("platform") != Some(platform())
                    || field("arch") != Some(arch)
                {
                    return Err(update_error(format!("Release identity mismatch in {name}.")));
                }
                let version = field("version").ok_or_else(|| update_error("Release manifest has no version."))?.to_owned();
                let parsed = Version::parse(&version)?;
                if channel == "stable" && !parsed.prerelease.is_empty() {
                    return Err(update_error(format!("Stable update manifest contains prerelease {version}.")));
                }
                let entries: Vec<&Map<String, Value>> = match manifest.get("artifacts") {
                    Some(Value::Array(items)) => items.iter().map(object).collect::<Result<_, _>>()?,
                    _ => vec![object(manifest.get("artifact").unwrap_or(&Value::Null))?],
                };
                let candidates: Vec<_> = entries.iter().filter(|e| e.get("format").and_then(Value::as_str) == Some(format)).collect();
                if candidates.len() != 1 {
                    return Err(update_error(format!("Release {version} has no unique {format} artifact.")));
                }
                let artifact = candidates[0];
                let artifact_name = artifact.get("name").and_then(Value::as_str).unwrap_or("");
                if artifact_name.is_empty()
                    || artifact_name.contains('/')
                    || artifact_name.contains('\\')
                    || artifact_name == "."
                    || artifact_name == ".."
                {
                    return Err(update_error("Invalid update asset filename."));
                }
                let matches: Vec<_> = assets.iter().filter(|a| a.get("name").and_then(Value::as_str) == Some(artifact_name)).collect();
                if matches.len() != 1 {
                    return Err(update_error(format!("Release artifact {artifact_name} is unavailable.")));
                }
                let size = artifact.get("size").and_then(Value::as_u64).unwrap_or(0);
                let digest = artifact.get("sha512").and_then(Value::as_str).unwrap_or("");
                let digest_ok = base64::engine::general_purpose::STANDARD.decode(digest).is_ok_and(|d| d.len() == 64);
                if size == 0 || size > 8 * 1024 * 1024 * 1024 || !digest_ok || matches[0].get("size").and_then(Value::as_u64) != Some(size) {
                    return Err(update_error("Invalid release artifact size or SHA512."));
                }
                let candidate = Release {
                    version: version.clone(),
                    channel: channel.to_owned(),
                    name: artifact_name.to_owned(),
                    format: format.to_owned(),
                    size,
                    digest: digest.to_owned(),
                    url: asset_url(matches[0])?,
                    notes: field("releaseNotes").or_else(|| release.get("body").and_then(Value::as_str)).map(str::to_owned),
                    date: field("releaseDate").or_else(|| release.get("published_at").and_then(Value::as_str)).map(str::to_owned),
                    rollout: manifest.get("rolloutHours").and_then(|v| match v {
                        Value::Number(n) => n.as_f64(),
                        Value::String(s) => s.parse().ok(),
                        _ => None,
                    }),
                };
                if newest.as_ref().is_none_or(|(best, _)| parsed.cmp(best) == Ordering::Greater) {
                    newest = Some((parsed, candidate));
                }
            }
            if releases.len() < 100 {
                break;
            }
        }
        newest.map(|(_, release)| release).ok_or_else(|| {
            update_error(format!("No {channel} native release is published for {}/{arch} in {REPOSITORY}.", platform()))
        })
    }

    async fn prepare(&self, state: &mut State, release: &Release) -> Result<(), PlatformError> {
        if state.prepared.as_ref().map(|p| p.release.key()) == Some(release.key()) {
            return Ok(());
        }
        discard_prepared(state).await;
        let directory = std::env::temp_dir().join(format!("paseo-flutter-update-{}", uuid::Uuid::new_v4().simple()));
        tokio::fs::create_dir(&directory).await?;
        let file = directory.join(&release.name);
        let result = async {
            self.download(&release.url, &file, release.size).await?;
            validate_file(&file, release).await?;
            let mut prepared = Prepared { release: release.clone(), directory: directory.clone(), file: file.clone(), app: None };
            validate_native(&mut prepared).await?;
            Ok::<_, PlatformError>(prepared)
        }
        .await;
        match result {
            Ok(prepared) => {
                state.prepared = Some(prepared);
                Ok(())
            }
            Err(error) => {
                let _ = tokio::fs::remove_dir_all(&directory).await;
                Err(error)
            }
        }
    }

    async fn request(&self, mut url: url::Url) -> Result<reqwest::Response, PlatformError> {
        for _ in 0..=5 {
            if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
                return Err(update_error("Updates require HTTPS."));
            }
            let response = tokio::time::timeout(
                Duration::from_secs(30),
                self.client
                    .get(url.clone())
                    .header(reqwest::header::ACCEPT, "application/vnd.github+json, application/octet-stream")
                    .send(),
            )
            .await
            .map_err(|_| update_error("Update source timed out."))?
            .map_err(|e| update_error(e.to_string()))?;
            let status = response.status().as_u16();
            if matches!(status, 301 | 302 | 303 | 307 | 308) {
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| update_error("Update redirect has no location."))?;
                url = url.join(location).map_err(|e| update_error(e.to_string()))?;
                continue;
            }
            if status != 200 {
                return Err(update_error(format!(
                    "Update source returned HTTP {status}; release may be unavailable or GitHub rate-limited."
                )));
            }
            return Ok(response);
        }
        Err(update_error("Too many update redirects."))
    }

    async fn json(&self, url: url::Url) -> Result<Value, PlatformError> {
        use futures_util::StreamExt as _;
        let response = self.request(url).await?;
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::new();
        loop {
            let next = tokio::time::timeout(Duration::from_secs(30), stream.next())
                .await
                .map_err(|_| update_error("Update metadata download timed out."))?;
            let Some(chunk) = next else { break };
            let chunk = chunk.map_err(|e| update_error(e.to_string()))?;
            bytes.extend_from_slice(&chunk);
            if bytes.len() > 16 * 1024 * 1024 {
                return Err(update_error("Update metadata exceeds 16 MiB."));
            }
        }
        serde_json::from_slice(&bytes).map_err(|e| update_error(format!("Invalid update metadata: {e}")))
    }

    async fn download(&self, url: &url::Url, file: &Path, size: u64) -> Result<(), PlatformError> {
        use futures_util::StreamExt as _;
        let response = self.request(url.clone()).await?;
        let mut stream = response.bytes_stream();
        let mut sink = tokio::fs::File::create(file).await?;
        let mut received = 0u64;
        loop {
            let next = tokio::time::timeout(Duration::from_secs(120), stream.next())
                .await
                .map_err(|_| update_error("Update download timed out."))?;
            let Some(chunk) = next else { break };
            let chunk = chunk.map_err(|e| update_error(e.to_string()))?;
            received += chunk.len() as u64;
            if received > size {
                return Err(update_error("Update download exceeds the published size."));
            }
            sink.write_all(&chunk).await?;
        }
        sink.flush().await?;
        sink.sync_all().await?;
        if received != size {
            return Err(update_error("Update download is incomplete."));
        }
        Ok(())
    }
}

fn object(value: &Value) -> Result<&Map<String, Value>, PlatformError> {
    value.as_object().ok_or_else(|| update_error("Expected an update metadata object."))
}

fn asset_url(asset: &Map<String, Value>) -> Result<url::Url, PlatformError> {
    let raw = asset.get("browser_download_url").and_then(Value::as_str).unwrap_or("");
    let url = url::Url::parse(raw).map_err(|_| update_error("Release asset is not hosted by the configured GitHub repository."))?;
    if url.scheme() != "https"
        || url.host_str() != Some("github.com")
        || !url.path().starts_with(&format!("/{REPOSITORY}/releases/download/"))
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(update_error("Release asset is not hosted by the configured GitHub repository."));
    }
    Ok(url)
}

async fn discard_prepared(state: &mut State) {
    if let Some(old) = state.prepared.take() {
        let _ = tokio::fs::remove_dir_all(&old.directory).await;
    }
}

/// Staged rollout: a stable bucket per installation from `<support>/.updaterId`.
async fn admit(release: &Release) -> Result<bool, PlatformError> {
    let Some(hours) = release.rollout.filter(|h| release.channel == "stable" && h.is_finite() && *h > 0.0) else {
        return Ok(true);
    };
    let Some(date) = release.date.as_deref().and_then(|d| chrono::DateTime::parse_from_rfc3339(d).ok()) else {
        return Ok(true);
    };
    let age = (chrono::Utc::now() - date.with_timezone(&chrono::Utc)).num_milliseconds() as f64 / 3_600_000.0;
    if age < 0.0 {
        return Ok(false);
    }
    let support = &crate::paths::app_paths().support_dir;
    tokio::fs::create_dir_all(support).await?;
    let file = support.join(".updaterId");
    let mut id = tokio::fs::read_to_string(&file).await.map(|s| s.trim().to_owned()).unwrap_or_default();
    if uuid::Uuid::parse_str(&id).map(|u| u.hyphenated().to_string().len() != id.len()).unwrap_or(true) {
        id = uuid::Uuid::new_v4().hyphenated().to_string();
        tokio::fs::write(&file, &id).await?;
    }
    let hex = id.replace('-', "");
    let bucket = u64::from_str_radix(&hex[24..], 16).unwrap_or(0) as f64 / 4_294_967_296.0;
    Ok(bucket < (age / hours).min(1.0))
}

async fn validate_file(file: &Path, release: &Release) -> Result<(), PlatformError> {
    let path = file.to_owned();
    let digest = tokio::task::spawn_blocking(move || -> std::io::Result<(u64, String)> {
        use std::io::Read as _;
        let mut input = std::fs::File::open(path)?;
        let mut hasher = Sha512::new();
        let mut buffer = vec![0u8; 1 << 20];
        let mut length = 0u64;
        loop {
            let read = input.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            length += read as u64;
            hasher.update(&buffer[..read]);
        }
        Ok((length, base64::engine::general_purpose::STANDARD.encode(hasher.finalize())))
    })
    .await
    .map_err(|e| update_error(e.to_string()))??;
    if digest.0 != release.size || digest.1 != release.digest {
        return Err(update_error("The downloaded update failed its SHA512/size verification."));
    }
    Ok(())
}

async fn run(executable: &str, args: &[&std::ffi::OsStr]) -> Result<String, PlatformError> {
    if !Path::new(executable).exists() {
        return Err(update_error(format!("Required native update tool is unavailable: {executable}")));
    }
    let output = tokio::process::Command::new(executable)
        .args(args)
        .output()
        .await
        .map_err(|e| update_error(format!("{executable}: {e}")))?;
    if !output.status.success() {
        let name = Path::new(executable).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        return Err(update_error(format!(
            "{name} failed ({}): {}\n{}",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn powershell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

async fn powershell(script: &str) -> Result<String, PlatformError> {
    let root = std::env::var("SystemRoot").map_err(|_| update_error("Windows SystemRoot is unavailable."))?;
    let encoded: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let encoded = base64::engine::general_purpose::STANDARD.encode(encoded);
    let exe = Path::new(&root).join("System32").join("WindowsPowerShell").join("v1.0").join("powershell.exe");
    run(
        &exe.display().to_string(),
        &["-NoLogo".as_ref(), "-NoProfile".as_ref(), "-NonInteractive".as_ref(), "-EncodedCommand".as_ref(), encoded.as_ref()],
    )
    .await
}

fn installed_app() -> Result<PathBuf, PlatformError> {
    let executable = std::env::current_exe()?.display().to_string();
    let marker = executable
        .rfind(".app/Contents/MacOS/")
        .ok_or_else(|| update_error("The running executable is not inside a macOS application bundle."))?;
    Ok(PathBuf::from(&executable[..marker + 4]))
}

async fn installed_format() -> Result<&'static str, PlatformError> {
    if cfg!(windows) {
        return Ok("exe");
    }
    if cfg!(target_os = "macos") {
        installed_app()?;
        return Ok("zip");
    }
    if let Some(image) = std::env::var_os("APPIMAGE").filter(|v| !v.is_empty())
        && Path::new(&image).exists() {
            return Ok("appimage");
        }
    let executable = std::fs::canonicalize(std::env::current_exe()?)?;
    if Path::new("/usr/bin/dpkg-query").exists()
        && let Ok(output) = tokio::process::Command::new("/usr/bin/dpkg-query").arg("-S").arg(&executable).output().await {
            let text = String::from_utf8_lossy(&output.stdout);
            if output.status.success()
                && text.lines().any(|line| {
                    line.strip_prefix("paseo-sessions").is_some_and(|rest| {
                        rest.starts_with(": ") || (rest.starts_with(':') && rest[1..].split_once(": ").is_some_and(|(arch, _)| !arch.contains(' ')))
                    })
                })
            {
                return Ok("deb");
            }
        }
    if Path::new("/usr/bin/rpm").exists()
        && let Ok(output) = tokio::process::Command::new("/usr/bin/rpm").args(["-qf", "--qf", "%{NAME}"]).arg(&executable).output().await
            && output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "paseo-sessions" {
                return Ok("rpm");
            }
    Err(update_error(
        "This Linux installation is not an AppImage or an installed paseo-sessions deb/rpm. Use a packaged native release.",
    ))
}

async fn signature(app: &Path) -> Result<String, PlatformError> {
    let output = tokio::process::Command::new("/usr/bin/codesign")
        .args(["-d", "--verbose=4"])
        .arg(app)
        .output()
        .await
        .map_err(|e| update_error(e.to_string()))?;
    if !output.status.success() {
        return Err(update_error(format!(
            "Cannot read installed macOS signing identity: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(format!("{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr)))
}

fn team(identity: &str) -> Option<String> {
    identity.lines().find_map(|line| line.strip_prefix("TeamIdentifier=").map(str::to_owned))
}

async fn validate_native(prepared: &mut Prepared) -> Result<(), PlatformError> {
    let release = prepared.release.clone();
    if cfg!(windows) {
        let current = std::env::current_exe()?.display().to_string();
        let script = format!(
            "$ErrorActionPreference = 'Stop'\n\
$old = Get-AuthenticodeSignature -LiteralPath {old}\n\
$new = Get-AuthenticodeSignature -LiteralPath {new}\n\
if ($new.Status -ne 'Valid' -and $new.Status -ne 'NotSigned') {{ throw 'Installer Authenticode signature is invalid.' }}\n\
if ($old.Status -eq 'Valid') {{\n\
  if ($new.Status -ne 'Valid' -or $old.SignerCertificate.Subject -ne $new.SignerCertificate.Subject) {{ throw 'Installer publisher does not match the installed application.' }}\n\
}} elseif ($old.Status -ne 'NotSigned') {{ throw 'The installed application has an invalid Authenticode signature.' }}\n",
            old = powershell_quote(&current),
            new = powershell_quote(&prepared.file.display().to_string()),
        );
        powershell(&script).await?;
        return Ok(());
    }
    if cfg!(target_os = "macos") {
        let extracted = prepared.directory.join("expanded");
        let _ = tokio::fs::remove_dir_all(&extracted).await;
        tokio::fs::create_dir(&extracted).await?;
        // Validate members before extracting to reject traversal outside staging.
        let listing = run("/usr/bin/unzip", &["-Z1".as_ref(), prepared.file.as_os_str()]).await?;
        for member in listing.lines().filter(|l| !l.is_empty()) {
            if member.starts_with('/') || member.split('/').any(|p| p == "..") || member.contains('\\') {
                return Err(update_error("Unsafe path in macOS update archive."));
            }
        }
        run("/usr/bin/ditto", &["-x".as_ref(), "-k".as_ref(), prepared.file.as_os_str(), extracted.as_os_str()]).await?;
        let mut apps = Vec::new();
        let mut entries = tokio::fs::read_dir(&extracted).await?;
        while let Some(entry) = entries.next_entry().await? {
            let kind = entry.file_type().await?;
            if kind.is_dir() && entry.path().extension().is_some_and(|e| e == "app") {
                apps.push(entry.path());
            }
        }
        if apps.len() != 1 {
            return Err(update_error("The macOS archive must contain exactly one root application bundle."));
        }
        let app = apps.remove(0);
        let plist = app.join("Contents").join("Info.plist");
        let id = run("/usr/libexec/PlistBuddy", &["-c".as_ref(), "Print :CFBundleIdentifier".as_ref(), plist.as_os_str()]).await?;
        let version = run("/usr/libexec/PlistBuddy", &["-c".as_ref(), "Print :CFBundleShortVersionString".as_ref(), plist.as_os_str()]).await?;
        if id.trim() != APP_ID || version.trim() != release.version {
            return Err(update_error("macOS package bundle identity/version mismatch."));
        }
        run("/usr/bin/codesign", &["--verify".as_ref(), "--deep".as_ref(), "--strict".as_ref(), app.as_os_str()]).await?;
        let old_identity = signature(&installed_app()?).await?;
        let new_identity = signature(&app).await?;
        match team(&old_identity).filter(|t| t != "not set") {
            Some(old_team) => {
                if team(&new_identity).as_deref() != Some(old_team.as_str()) {
                    return Err(update_error("The update signing team does not match the installed application."));
                }
                run("/usr/sbin/spctl", &["--assess".as_ref(), "--type".as_ref(), "execute".as_ref(), app.as_os_str()]).await?;
            }
            None => {
                if !old_identity.contains("Signature=adhoc") || !new_identity.contains("Signature=adhoc") {
                    return Err(update_error("The installed application has no verifiable macOS signing identity."));
                }
            }
        }
        prepared.app = Some(app);
        return Ok(());
    }
    if release.format == "appimage" {
        let header = {
            use tokio::io::AsyncReadExt as _;
            let mut file = tokio::fs::File::open(&prepared.file).await?;
            let mut bytes = [0u8; 20];
            file.read_exact(&mut bytes).await.map(|_| bytes).ok()
        };
        let machine = if arch()? == "x64" { 62u16 } else { 183u16 };
        let valid = header.is_some_and(|b| {
            b[0..4] == [127, 69, 76, 70] && b[4] == 2 && b[5] == 1 && b[8] == 65 && b[9] == 73 && b[10] == 2 && u16::from_le_bytes([b[18], b[19]]) == machine
        });
        if !valid {
            return Err(update_error("Update is not a type-2 AppImage for this architecture."));
        }
        return Ok(());
    }
    if !Path::new("/usr/bin/pkexec").exists() {
        return Err(update_error("Package updates require polkit pkexec, which is not installed."));
    }
    let values = if release.format == "deb" {
        run(
            "/usr/bin/dpkg-deb",
            &["--show".as_ref(), "--showformat=${Package}\n${Version}\n${Architecture}\n".as_ref(), prepared.file.as_os_str()],
        )
        .await?
    } else {
        run("/usr/bin/rpm", &["-qp".as_ref(), "--qf".as_ref(), "%{NAME}\n%{VERSION}\n%{ARCH}\n".as_ref(), prepared.file.as_os_str()]).await?
    };
    let fields: Vec<&str> = values.trim().split('\n').collect();
    let expected_arch = match (release.format.as_str(), arch()?) {
        ("deb", "x64") => "amd64",
        ("deb", _) => "arm64",
        (_, "x64") => "x86_64",
        _ => "aarch64",
    };
    if fields.len() != 3 || fields[0] != "paseo-sessions" || fields[1] != release.version || fields[2] != expected_arch {
        return Err(update_error("The native package name/version/architecture does not match the release manifest."));
    }
    Ok(())
}

async fn stage_replacement(prepared: &Prepared) -> Result<Option<Replacement>, PlatformError> {
    if !cfg!(target_os = "macos") && prepared.release.format != "appimage" {
        return Ok(None);
    }
    let target = if cfg!(target_os = "macos") {
        installed_app()?
    } else {
        std::fs::canonicalize(std::env::var_os("APPIMAGE").ok_or_else(|| update_error("APPIMAGE is not set."))?)?
    };
    let parent = target.parent().ok_or_else(|| update_error("Installation has no parent directory."))?;
    let directory = parent.join(format!(".paseo-update-{}", uuid::Uuid::new_v4().simple()));
    tokio::fs::create_dir(&directory).await?;
    let staged = directory.join(target.file_name().unwrap_or_default());
    let result = async {
        if cfg!(target_os = "macos") {
            let app = prepared.app.as_ref().ok_or_else(|| update_error("The macOS update was not extracted."))?;
            run("/usr/bin/ditto", &[app.as_os_str(), staged.as_os_str()]).await?;
        } else {
            tokio::fs::copy(&prepared.file, &staged).await?;
            run("/bin/chmod", &["755".as_ref(), staged.as_os_str()]).await?;
        }
        Ok::<_, PlatformError>(())
    }
    .await;
    if let Err(error) = result {
        let _ = tokio::fs::remove_dir_all(&directory).await;
        return Err(error);
    }
    Ok(Some(Replacement { backup: directory.join("previous"), directory, target, staged }))
}

/// SemVer 2.0 precedence (original `_Version`).
#[derive(Debug, PartialEq, Eq)]
struct Version {
    core: [u64; 3],
    prerelease: Vec<String>,
}

impl Version {
    fn parse(value: &str) -> Result<Self, PlatformError> {
        let invalid = || update_error(format!("Invalid release version: {value}"));
        let without_build = value.split_once('+').map_or(value, |(v, build)| {
            if build.is_empty() { "" } else { v }
        });
        let (core, pre) = match without_build.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (without_build, None),
        };
        let parts: Vec<&str> = core.split('.').collect();
        if parts.len() != 3 {
            return Err(invalid());
        }
        let mut numbers = [0u64; 3];
        for (slot, part) in numbers.iter_mut().zip(&parts) {
            if part.is_empty() || !part.chars().all(|c| c.is_ascii_digit()) || (part.len() > 1 && part.starts_with('0')) {
                return Err(invalid());
            }
            *slot = part.parse().map_err(|_| invalid())?;
        }
        let prerelease: Vec<String> = match pre {
            None => Vec::new(),
            Some(pre) => pre.split('.').map(str::to_owned).collect(),
        };
        for part in &prerelease {
            let numeric = !part.is_empty() && part.chars().all(|c| c.is_ascii_digit());
            if part.is_empty()
                || !part.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                || (numeric && part.len() > 1 && part.starts_with('0'))
            {
                return Err(update_error(format!("Invalid prerelease version: {value}")));
            }
        }
        Ok(Self { core: numbers, prerelease })
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        match self.core.cmp(&other.core) {
            Ordering::Equal => {}
            unequal => return unequal,
        }
        match (self.prerelease.is_empty(), other.prerelease.is_empty()) {
            (true, true) => return Ordering::Equal,
            (true, false) => return Ordering::Greater,
            (false, true) => return Ordering::Less,
            _ => {}
        }
        for (left, right) in self.prerelease.iter().zip(&other.prerelease) {
            let ordering = match (left.parse::<u64>(), right.parse::<u64>()) {
                (Ok(a), Ok(b)) => a.cmp(&b),
                (Ok(_), Err(_)) => Ordering::Less,
                (Err(_), Ok(_)) => Ordering::Greater,
                _ => left.cmp(right),
            };
            if ordering != Ordering::Equal {
                return ordering;
            }
        }
        self.prerelease.len().cmp(&other.prerelease.len())
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_precedence() {
        let v = |s| Version::parse(s).unwrap();
        assert!(v("0.13.4") > v("0.13.3"));
        assert!(v("1.0.0") > v("1.0.0-beta.2"));
        assert!(v("1.0.0-beta.11") > v("1.0.0-beta.2"));
        assert!(v("1.0.0-beta") > v("1.0.0-alpha.1"));
        assert!(v("1.0.0-alpha.1") > v("1.0.0-alpha"));
        assert_eq!(v("1.0.0+build"), v("1.0.0"));
        assert!(Version::parse("01.0.0").is_err());
        assert!(Version::parse("1.0.0-01").is_err());
        assert!(Version::parse("1.0").is_err());
    }
}
