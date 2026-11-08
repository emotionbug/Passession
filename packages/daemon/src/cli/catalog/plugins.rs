use super::*;
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;

pub(super) fn command() -> Command {
    Command::new("plugin")
        .about("Manage trusted, unsandboxed plugins")
        .subcommand_required(true)
        .subcommands([
            Command::new("init")
                .about("Create a native Rust server plugin")
                .arg(positional("directory", true))
                .arg(option("id", "id"))
                .arg(option("sdk-path", "sdk-path").help("Use a local paseo-plugin SDK crate")),
            Command::new("ls")
                .alias("status")
                .arg(positional("id", false)),
            Command::new("logs").arg(positional("id", true)),
            Command::new("install")
                .alias("add")
                .arg(positional("source", true))
                .arg(option("id", "id"))
                .arg(option("ref", "ref"))
                .arg(option("path", "path"))
                .arg(option("version", "version").help("Release archive version"))
                .arg(option("sha256", "sha256").help("Release archive SHA-256 digest"))
                .arg(option("format", "format").value_parser(["tar.gz", "zip"])),
            Command::new("update")
                .arg(positional("id", false))
                .arg(switch("all", "all"))
                .arg(switch("check", "check"))
                .arg(switch("yes", "yes"))
                .arg(option("ref", "ref").conflicts_with_all(["version", "url", "sha256", "format"]))
                .arg(option("version", "version").help("Cargo crate or release archive version"))
                .arg(option("url", "url").requires_all(["version", "sha256"]))
                .arg(option("sha256", "sha256").requires("url"))
                .arg(option("format", "format").requires("url").value_parser(["tar.gz", "zip"])),
            Command::new("reload").arg(positional("id", true)),
            Command::new("enable").arg(positional("id", true)),
            Command::new("disable").arg(positional("id", true)),
            Command::new("remove").arg(positional("id", true)),
        ])
}
fn identity(value: &Value) -> String {
    if value["kind"] == "directory" {
        return text(value, "path").into();
    }
    if value["registry"].is_object() {
        if let Ok(url) = url::Url::parse(text(&value["registry"], "url")) {
            return format!(
                "{}/{}",
                &url[url::Position::BeforeHost..url::Position::AfterPort],
                text(&value["registry"], "id")
            );
        }
    }
    let source = match value["kind"].as_str() {
        Some("cargo") => format!("cargo:{}", text(value, "crateName")),
        Some("release") => format!("release:{}", text(value, "origin")),
        // Preserve the identity of installed legacy records so users can migrate them.
        Some("npm") => format!("npm:{} (legacy; reinstall a native release)", text(value, "packageName")),
        Some("git") => format!("git:{}", text(value, "remote")),
        _ => return "unknown source".into(),
    };
    if value["pluginPath"] == "." {
        source
    } else {
        format!("{source}:{}", text(value, "pluginPath"))
    }
}
fn plugin_output(value: Value, context: &Context) -> Result<Output> {
    if context.output.format != Format::Table || context.output.quiet {
        return Ok(Output::new(value));
    }
    let rows = value
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_else(|| std::slice::from_ref(&value));
    let display:Vec<Value>=rows.iter().map(|p|json!({"id":p["id"],"status":p["status"],"enabled":if p["enabled"]==true{"yes"}else{"no"},"source":if p["installation"].is_object(){identity(&p["installation"]["identity"])}else{"-".into()},"revision":p["installation"]["currentRevision"].as_str().unwrap_or("-"),"path":p["path"],"error":p["error"].as_str().unwrap_or("")})).collect();
    Ok(Output::new(value)
        .table_data(json!(display))
        .columns(&[
            ("PLUGIN", "id"),
            ("STATUS", "status"),
            ("ENABLED", "enabled"),
            ("SOURCE", "source"),
            ("REVISION", "revision"),
            ("DIRECTORY", "path"),
            ("ERROR", "error"),
        ])
        .widths(&[20, 10, 8, 40, 16, 40, 40]))
}
pub(super) async fn execute(
    command: &str,
    m: &ArgMatches,
    context: &Context,
    client: &DaemonClient,
) -> Result<Output> {
    let (feature, message) = match command {
        "logs" => ("pluginLogs", "Update the host to view plugin logs."),
        "install" | "add" => (
            "pluginSourceInstallation",
            "Update the host to install plugin sources.",
        ),
        "update" => (
            "pluginSourceUpdates",
            "Update the host to review plugin updates.",
        ),
        _ => (
            "pluginManagement",
            "Update the host to use plugin management.",
        ),
    };
    if client.server_info()["features"][feature] != true {
        return Err(error("DAEMON_UPDATE_REQUIRED", message));
    }
    match command {
        "ls" | "status" => {
            let payload = rpc(
                client,
                context,
                json!({"type":"plugin.list.request"}),
                "plugin.list.response",
            )
            .await?;
            let mut plugins = response_array(&payload, "plugins")?.to_vec();
            if let Some(id) = string(m, "id") {
                plugins.retain(|p| p["id"] == id);
                if plugins.is_empty() {
                    return Err(anyhow!("Plugin is not configured: {id}"));
                }
            }
            plugin_output(json!(plugins), context)
        }
        "logs" => {
            let payload = rpc(
                client,
                context,
                json!({"type":"plugin.logs.get.request","pluginId":required(m,"id")?}),
                "plugin.logs.get.response",
            )
            .await?;
            Ok(Output::new(json!(response_array(&payload, "entries")?))
                .id("sequence")
                .columns(&[
                    ("TIME", "timestamp"),
                    ("STREAM", "stream"),
                    ("MESSAGE", "message"),
                ])
                .widths(&[24, 8, 80]))
        }
        "install" | "add" => {
            eprintln!(
                "Trusting plugin code: native executables and preparation commands run unsandboxed on the daemon host. Cargo build scripts, dependencies, and future updates are part of the codebase you trust."
            );
            let request = install_request(m)?;
            let payload = client
                .request_cancellable(
                    request,
                    "plugin.source.install.response",
                    Duration::from_secs(300),
                    context.cancel.clone(),
                )
                .await?;
            plugin_output(payload["plugin"].clone(), context)
        }
        "update" => update(m, context, client).await,
        "reload" | "enable" | "disable" => {
            let payload=client.request_cancellable(json!({"type":format!("plugin.{command}.request"),"pluginId":required(m,"id")?}),&format!("plugin.{command}.response"),if command=="reload"{Duration::ZERO}else{Duration::from_secs(60)},context.cancel.clone()).await?;
            plugin_output(payload["plugin"].clone(), context)
        }
        "remove" => {
            let id = required(m, "id")?;
            let list = rpc(
                client,
                context,
                json!({"type":"plugin.list.request"}),
                "plugin.list.response",
            )
            .await?;
            let mut plugin = array(&list["plugins"])
                .iter()
                .find(|p| p["id"] == id)
                .cloned()
                .ok_or_else(|| anyhow!("Plugin is not configured: {id}"))?;
            rpc(
                client,
                context,
                json!({"type":"plugin.remove.request","pluginId":id}),
                "plugin.remove.response",
            )
            .await?;
            plugin["enabled"] = json!(false);
            plugin["status"] = json!("disabled");
            plugin_output(plugin, context)
        }
        _ => Err(anyhow!("Unknown plugin command: {command}")),
    }
}
fn portable(path: &str) -> bool {
    path == "."
        || (!path.is_empty()
            && !path.contains(':')
            && path
                .split(['/', '\\'])
                .all(|p| !p.is_empty() && p != "." && p != ".."))
}
fn source_reference(source: &str, path: Option<&str>) -> Result<String> {
    let (base, suffix) = crate::plugins::split_source_reference(source)?;
    let windows = base.as_bytes().get(1) == Some(&b':')
        && base
            .as_bytes()
            .get(2)
            .is_some_and(|b| *b == b'/' || *b == b'\\');
    let directory = base.starts_with('/')
        || base.starts_with('~')
        || base.starts_with("./")
        || base.starts_with("../")
        || base.starts_with(".\\")
        || base.starts_with("..\\")
        || base == "."
        || base == ".."
        || base.starts_with("\\\\")
        || windows;
    let mut source = if directory {
        let expanded = if base == "~" {
            dirs::home_dir()
                .ok_or_else(|| anyhow!("User home unavailable"))?
                .to_string_lossy()
                .into_owned()
        } else if let Some(rest) = base.strip_prefix("~/") {
            dirs::home_dir()
                .ok_or_else(|| anyhow!("User home unavailable"))?
                .join(rest)
                .to_string_lossy()
                .into_owned()
        } else {
            base
        };
        if Path::new(&expanded).is_absolute() || windows || expanded.starts_with("\\\\") {
            expanded
        } else {
            std::env::current_dir()?
                .join(expanded)
                .to_string_lossy()
                .into_owned()
        }
    } else {
        base
    };
    if let Some(suffix) = suffix {
        source.push(':');
        source.push_str(&suffix);
    }
    if let Some(path) = path {
        if !portable(path) {
            return Err(anyhow!(
                "Plugin path must be a portable path relative to the repository"
            ));
        }
        source.push(':');
        source.push_str(path);
    }
    Ok(source)
}
fn install_request(m: &ArgMatches) -> Result<Value> {
    let source = required(m, "source")?;
    let release = source.starts_with("release:");
    let path = string(m, "path");
    let source = if release {
        source.to_owned()
    } else {
        source_reference(source, path)?
    };
    if release {
        if string(m, "version").is_none() || string(m, "sha256").is_none() {
            return Err(anyhow!("Release installation requires --version and --sha256"));
        }
        if string(m, "ref").is_some() {
            return Err(anyhow!("--ref applies to Git sources, not release archives"));
        }
    } else if ["version", "sha256", "format"].iter().any(|key| string(m, key).is_some()) {
        return Err(anyhow!("--version, --sha256, and --format require a release: source; pin Cargo versions with cargo:<crate>@<version>"));
    }
    let mut request = json!({"type":"plugin.source.install.request","source":source});
    if release {
        if let Some(path) = path {
            if !portable(path) {
                return Err(anyhow!("Plugin path must be a portable path relative to the archive"));
            }
            request["pluginPath"] = json!(path);
        }
    }
    for key in ["id", "ref", "version", "sha256", "format"] {
        if let Some(value) = string(m, key).filter(|value| !value.is_empty()) {
            request[key] = json!(value);
        }
    }
    Ok(request)
}

fn update_target(m: &ArgMatches) -> Result<Option<Value>> {
    if let Some(url) = string(m, "url") {
        let mut target = json!({
            "kind": "release",
            "url": url,
            "version": required(m, "version")?,
            "sha256": required(m, "sha256")?,
        });
        if let Some(format) = string(m, "format") {
            target["format"] = json!(format);
        }
        return Ok(Some(target));
    }
    Ok(string(m, "ref")
        .map(|reference| json!({"kind":"git","ref":reference}))
        .or_else(|| string(m, "version").map(|version| json!({"kind":"cargo","version":version}))))
}

async fn update(m: &ArgMatches, context: &Context, client: &DaemonClient) -> Result<Output> {
    let id = string(m, "id");
    let all = flag(m, "all");
    if id.is_some() == all {
        return Err(anyhow!("Choose one plugin ID or pass --all"));
    }
    let target = update_target(m)?;
    if target.is_some() && all {
        return Err(anyhow!(
            "An explicit target requires one plugin ID, without --all"
        ));
    }
    let mut request = json!({"type":"plugin.source.update.preview.request"});
    if let Some(id) = id {
        request["pluginId"] = json!(id);
    }
    if let Some(target) = &target {
        request["target"] = target.clone();
    }
    let preview = client
        .request_cancellable(
            request,
            "plugin.source.update.preview.response",
            Duration::from_secs(300),
            context.cancel.clone(),
        )
        .await?;
    let mut rows = response_array(&preview, "plugins")?.to_vec();
    if context.output.format == Format::Table {
        for row in &rows {
            let current = row["current"]["currentRevision"]
                .as_str()
                .unwrap_or("unknown");
            let target = row["target"]["commit"]
                .as_str()
                .or_else(|| row["target"]["version"].as_str())
                .unwrap_or("unknown");
            let detail = match text(row, "outcome") {
                "update" => format!("{current} → {target}"),
                "current" => format!("up to date ({current})"),
                "installed-newer" => format!(
                    "installed {current} is newer than latest {target}; keeping installed version"
                ),
                "local" => "local directory; use Reload after editing".into(),
                _ => row["error"].as_str().unwrap_or("Check failed").into(),
            };
            eprintln!("{}: {detail}", text(row, "id"));
            for link in array(&row["links"]) {
                if let Some(link) = link.as_str() {
                    eprintln!("Review: {link}");
                }
            }
        }
    }
    let proposals: Vec<Value> = rows
        .iter()
        .filter_map(|r| r.get("proposal").filter(|p| p.is_object()).cloned())
        .collect();
    if !flag(m, "check") && !proposals.is_empty() {
        if !flag(m, "yes") && target.is_none() {
            return Err(anyhow!("Confirmation required; rerun with --yes."));
        }
        let applied = client
            .request_cancellable(
                json!({"type":"plugin.source.update.apply.request","proposals":proposals}),
                "plugin.source.update.apply.response",
                Duration::from_secs(300),
                context.cancel.clone(),
            )
            .await?;
        let applied = response_array(&applied, "plugins")?;
        for row in &mut rows {
            if let Some(applied) = applied.iter().find(|a| a["id"] == row["id"]) {
                *row = applied.clone();
            }
        }
    }
    let code = if rows.iter().any(|r| r["outcome"] == "error") {
        1
    } else {
        0
    };
    if context.output.format != Format::Table || context.output.quiet {
        return Ok(Output::new(json!(rows)).exit(code));
    }
    let display:Vec<Value>=rows.iter().map(|r|json!({"id":r["id"],"outcome":r["outcome"],"detail":r["error"].as_str().or_else(||r["warning"].as_str()).or_else(||r["plugin"]["installation"]["currentRevision"].as_str()).unwrap_or("")})).collect();
    Ok(Output::new(json!(rows))
        .table_data(json!(display))
        .columns(&[
            ("PLUGIN", "id"),
            ("RESULT", "outcome"),
            ("DETAIL", "detail"),
        ])
        .widths(&[24, 18, 50])
        .exit(code))
}
pub(super) async fn init(m: &ArgMatches) -> Result<Output> {
    let path = PathBuf::from(required(m, "directory")?);
    let absolute = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut directory = PathBuf::new();
    for part in absolute.components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                directory.pop();
            }
            _ => directory.push(part.as_os_str()),
        }
    }
    let id = string(m, "id")
        .or_else(|| directory.file_name().and_then(|v| v.to_str()))
        .ok_or_else(|| anyhow!("Plugin ID is required"))?;
    if id.is_empty()
        || !id.as_bytes()[0].is_ascii_lowercase()
        || !id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(anyhow!("Invalid plugin ID: {id}"));
    }
    let version = env!("CARGO_PKG_VERSION");
    let sdk = if let Some(path) = string(m, "sdk-path") {
        let path = tokio::fs::canonicalize(path)
            .await
            .map_err(|error| anyhow!("Cannot resolve SDK directory {path}: {error}"))?;
        if !tokio::fs::metadata(path.join("Cargo.toml")).await?.is_file() {
            return Err(anyhow!("SDK directory must contain Cargo.toml"));
        }
        let path = path.to_str().ok_or_else(|| anyhow!("SDK path must be valid UTF-8"))?;
        format!("{{ path = {}, version = \"={version}\" }}", serde_json::to_string(path)?)
    } else {
        format!("{{ git = \"https://github.com/emotionbug/Passession.git\", version = \"={version}\" }}")
    };
    tokio::fs::create_dir_all(&directory).await?;
    if tokio::fs::read_dir(&directory)
        .await?
        .next_entry()
        .await?
        .is_some()
    {
        return Err(anyhow!(
            "Plugin directory must be empty: {}",
            directory.display()
        ));
    }
    let executable = format!("target/release/paseo-plugin-server{}", std::env::consts::EXE_SUFFIX);
    let manifest = json!({
        "id": id,
        "runtime": "native",
        "server": {"executable": executable, "args": []},
        "build": [["cargo", "build", "--release", "--target-dir", "target"]],
        "media": [],
        "requirements": {"paseo": format!(">={version}")}
    });
    let cargo = format!(
        "[package]\nname = \"{id}\"\nversion = \"0.1.0\"\nedition = \"2024\"\nrust-version = \"1.93\"\ninclude = [\"paseo-plugin.json\", \"OVERVIEW*.md\", \"README.md\", \"src/**\"]\n\n[[bin]]\nname = \"paseo-plugin-server\"\npath = \"src/main.rs\"\n\n[dependencies]\npaseo-plugin = {sdk}\nserde_json = \"1\"\ntokio = {{ version = \"1\", features = [\"macros\", \"rt-multi-thread\"] }}\n\n[workspace]\n"
    );
    let readme = format!(
        "# {id}\n\nA native Paseo plugin exposing the `greeting.create` RPC. Send `{{\"name\":\"World\"}}` to receive `{{\"message\":\"Hello, World!\"}}`.\n\n## Develop\n\nInstall Rust 1.93 or newer, then run these commands in this directory:\n\n```sh\ncargo build --release --target-dir target\n./{executable} --validate . {version}\npaseo plugin install .\n```\n\nOn Windows, invoke `.\\{windows_executable}` instead of `./{executable}`. Validation prints the registered contributions as JSON. Keep stdout reserved for the SDK protocol; use stderr for diagnostics.\n\nThe manifest's build command is also run during managed installation. After local edits, rebuild and run `paseo plugin reload {id}`. If moving the project between operating systems, update `server.executable` in `paseo-plugin.json` to include `.exe` on Windows and omit it elsewhere.\n\nBy default the SDK comes from the Paseo Git repository. For local SDK development, generate a new plugin with `paseo plugin init <directory> --sdk-path <paseo-checkout>/packages/plugin`; this writes an absolute path dependency instead. Local path dependencies must be replaced with a published SDK dependency before distributing the plugin.\n\nCommit Cargo.lock after the first build. Review dependency and build-script changes before installing or updating: native plugins run unsandboxed with the daemon user's privileges. Include `paseo-plugin.json`, the source, and your completed OVERVIEW.md when publishing a crate or Git repository. Cargo registry publication requires replacing Git/path SDK dependencies with the published `paseo-plugin` version.\n",
        windows_executable = executable.replace('/', "\\")
    );
    let files = [
        ("paseo-plugin.json", format!("{}\n", serde_json::to_string_pretty(&manifest)?)),
        ("Cargo.toml", cargo),
        ("src/main.rs", PLUGIN_MAIN.into()),
        ("README.md", readme),
        (".gitignore", "/target/\n".into()),
    ];
    tokio::fs::create_dir(directory.join("src")).await?;
    for (name, content) in files {
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(directory.join(name))
            .await?;
        file.write_all(content.as_bytes()).await?;
    }
    let mut overview = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(directory.join("OVERVIEW.md"))
        .await?;
    overview.write_all(OVERVIEW.as_bytes()).await?;
    Ok(Output::new(json!({"id":id,"directory":directory}))
        .columns(&[("PLUGIN", "id"), ("DIRECTORY", "directory")])
        .widths(&[20, 60]))
}
const PLUGIN_MAIN: &str = r#"use paseo_plugin::{Plugin, Result, anyhow};
use serde_json::json;

#[tokio::main]
async fn main() -> Result<()> {
    let mut plugin = Plugin::new();
    plugin.rpc("greeting.create", |input, _context| async move {
        let name = input
            .get("name")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow!("greeting.create requires a string name"))?;
        Ok(json!({"message": format!("Hello, {name}!")}))
    })?;
    plugin.run().await
}
"#;

const OVERVIEW: &str = r#"<!--
Use this file to describe your backend plugin for its registry listing. Keep it
beside paseo-plugin.json at the pinned commit and in your Cargo crate.
Replace this comment with useful facts before publishing.

This overview is for people choosing a plugin in Paseo, on the plugin page under
its install command. README assumes a GitHub audience and includes technical
details, installation instructions, and badges. Long, AI-generated READMEs leave
people reading past that material to understand what the plugin does.

Write these parts in order, including only what helps someone decide to install:

1. Describe what your plugin is and does in plain terms first.
2. Explain how it works only when it is not obvious.
3. Explain setup when needed: settings, accounts, tokens, providers, tools, or
   other plugins. Include applicable daemon version and operating system
   requirements. Setup guidance is allowed; installation instructions are not.
4. Explain capabilities and settings worth understanding, what each option does,
   what the plugin reads or sends and where, permissions, and known limits.

Length follows complexity; a simple backend utility may need one paragraph.
Choose headings only when they help. Use plain factual language.
Omit installation commands, badges, changelog, contributing or license sections,
marketing, and unsupported claims. Avoid implementation filler such as empty
cleanup functions or lists of absent features.

To translate the overview, add a whole copy beside this file named for the
language, such as OVERVIEW.zh-CN.md. Readers in that language see it instead.
-->
"#;
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_paths_are_portable() {
        assert!(portable("."));
        assert!(portable("plugins/test"));
        for path in [
            "/absolute",
            "../escape",
            "nested/../escape",
            "C:\\root",
            "a//b",
        ] {
            assert!(!portable(path));
        }
    }
    #[test]
    fn windows_install_paths_are_not_made_local() {
        assert_eq!(
            source_reference("C:\\plugins\\hello", None).unwrap(),
            "C:\\plugins\\hello"
        );
    }
    #[test]
    fn managed_identity_has_explicit_source() {
        assert_eq!(
            identity(&json!({"kind":"cargo","crateName":"review-plugin","pluginPath":"."})),
            "cargo:review-plugin"
        );
    }
    #[test]
    fn native_update_selections_keep_explicit_release_pins() {
        let cargo = command().try_get_matches_from([
            "plugin", "update", "example", "--version", "1.2.3",
        ]).unwrap();
        assert_eq!(
            update_target(cargo.subcommand_matches("update").unwrap()).unwrap(),
            Some(json!({"kind":"cargo","version":"1.2.3"}))
        );
        let digest = "a".repeat(64);
        let release = command().try_get_matches_from([
            "plugin", "update", "example", "--url", "https://example.com/plugin.zip",
            "--version", "1.2.3", "--sha256", &digest, "--format", "zip",
        ]).unwrap();
        assert_eq!(
            update_target(release.subcommand_matches("update").unwrap()).unwrap(),
            Some(json!({"kind":"release","url":"https://example.com/plugin.zip","version":"1.2.3","sha256":digest,"format":"zip"}))
        );
        assert!(command().try_get_matches_from([
            "plugin", "update", "example", "--url", "https://example.com/plugin.zip",
            "--version", "1.2.3",
        ]).is_err());
        assert!(command().try_get_matches_from([
            "plugin", "update", "example", "--ref", "main", "--version", "1.2.3",
        ]).is_err());
    }

    #[test]
    fn release_install_requires_digest_and_rejects_cross_source_options() {
        let digest = "b".repeat(64);
        let release = command().try_get_matches_from([
            "plugin", "install", "release:https://example.com/plugin.zip",
            "--version", "1.2.3", "--sha256", &digest, "--format", "zip",
        ]).unwrap();
        let request = install_request(release.subcommand_matches("install").unwrap()).unwrap();
        assert_eq!(request["source"], "release:https://example.com/plugin.zip");
        assert_eq!(request["sha256"], digest);
        assert_eq!(request["version"], "1.2.3");
        for args in [
            vec!["plugin", "install", "release:https://example.com/plugin.zip"],
            vec!["plugin", "install", "cargo:example", "--version", "1.2.3"],
        ] {
            let matches = command().try_get_matches_from(args).unwrap();
            assert!(install_request(matches.subcommand_matches("install").unwrap()).is_err());
        }
        let cargo = command().try_get_matches_from([
            "plugin", "install", "cargo:example@1.2.3",
        ]).unwrap();
        assert_eq!(
            install_request(cargo.subcommand_matches("install").unwrap()).unwrap()["source"],
            "cargo:example@1.2.3"
        );
    }

    #[test]
    fn release_subpath_preserves_archive_url() {
        let digest = "c".repeat(64);
        let source = "release:https://example.com/plugin.zip?signature=part:one";
        let matches = command().try_get_matches_from([
            "plugin", "install", source, "--version", "1.2.3",
            "--sha256", &digest, "--format", "zip", "--path", "archive/plugin",
        ]).unwrap();
        let request = install_request(matches.subcommand_matches("install").unwrap()).unwrap();
        assert_eq!(request["source"], source);
        assert_eq!(request["pluginPath"], "archive/plugin");
        let matches = command().try_get_matches_from([
            "plugin", "install", source, "--version", "1.2.3",
            "--sha256", &digest, "--format", "zip", "--path", "../escape",
        ]).unwrap();
        assert!(install_request(matches.subcommand_matches("install").unwrap()).is_err());
    }

    #[tokio::test]
    async fn generated_native_manifest_is_accepted_before_build() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("my-plugin");
        let sdk = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugin");
        let matches = command().try_get_matches_from([
            "plugin".to_owned(), "init".into(), directory.to_string_lossy().into_owned(),
            "--sdk-path".into(), sdk.to_string_lossy().into_owned(),
        ]).unwrap();
        init(matches.subcommand_matches("init").unwrap()).await.unwrap();
        let manifest = crate::plugins::read_plugin_manifest(&directory).await.unwrap();
        assert_eq!(manifest["id"], "my-plugin");
        assert_eq!(manifest["runtime"], "native");
        assert_eq!(manifest["server"]["args"], json!([]));
        assert_eq!(manifest["server"]["executable"],
            format!("target/release/paseo-plugin-server{}", std::env::consts::EXE_SUFFIX));
        assert_eq!(manifest["build"], json!([["cargo","build","--release","--target-dir","target"]]));
        assert!(directory.join("Cargo.toml").is_file());
        assert!(directory.join("src/main.rs").is_file());
        assert!(!directory.join("package.json").exists());
    }

    #[tokio::test]
    async fn initialization_rejects_unloadable_ids_without_creating_files() {
        let root = tempfile::tempdir().unwrap();
        for id in ["with.dot", "with_underscore"] {
            let directory = root.path().join(id);
            let matches = command().try_get_matches_from([
                "plugin".to_owned(), "init".into(), directory.to_string_lossy().into_owned(),
            ]).unwrap();
            assert!(init(matches.subcommand_matches("init").unwrap()).await.is_err());
            assert!(!directory.exists());
        }
    }

    #[tokio::test]
    async fn initialization_never_overwrites_existing_content() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("my-plugin");
        tokio::fs::create_dir(&directory).await.unwrap();
        tokio::fs::write(directory.join("keep"), b"user content")
            .await
            .unwrap();
        let matches = command()
            .try_get_matches_from([
                "plugin".to_owned(),
                "init".into(),
                directory.to_string_lossy().into_owned(),
            ])
            .unwrap();
        assert!(
            init(matches.subcommand_matches("init").unwrap())
                .await
                .err()
                .unwrap()
                .to_string()
                .contains("must be empty")
        );
        assert_eq!(
            tokio::fs::read(directory.join("keep")).await.unwrap(),
            b"user content"
        );
        assert!(!directory.join("paseo-plugin.json").exists());
    }
}
