use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::path::{Path, PathBuf};

pub fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || !id.as_bytes()[0].is_ascii_lowercase()
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        bail!("Invalid plugin ID: {id}");
    }
    Ok(())
}
pub fn validate_contribution_id(id: &str) -> Result<()> {
    if id.is_empty()
        || !id.as_bytes()[0].is_ascii_lowercase()
        || !id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
        })
    {
        bail!("Invalid plugin contribution ID: {id}");
    }
    Ok(())
}
pub async fn read(directory: &Path) -> Result<Value> {
    let path = directory.join("paseo-plugin.json");
    let bytes = tokio::fs::read(&path)
        .await
        .with_context(|| format!("Plugin manifest is missing: {}", path.display()))?;
    if bytes.len() > 1024 * 1024 {
        bail!("Plugin manifest exceeds 1 MiB");
    }
    let mut manifest: Value =
        serde_json::from_slice(&bytes).context("Invalid plugin manifest JSON")?;
    let object = manifest
        .as_object()
        .context("Plugin manifest must be an object")?;
    validate_id(
        object
            .get("id")
            .and_then(Value::as_str)
            .context("Plugin manifest requires id")?,
    )?;
    if object.get("runtime").and_then(Value::as_str) != Some("native") {
        bail!("This plugin uses an unsupported JavaScript or legacy runtime. Install a native Rust plugin release with runtime:\"native\" and server.executable in paseo-plugin.json; existing settings and installation records are preserved.");
    }
    let server = object.get("server").and_then(Value::as_object)
        .context("Native plugin manifest requires server.executable and server.args")?;
    let executable = server.get("executable").and_then(Value::as_str)
        .context("Native plugin server.executable must be a relative file path")?;
    validate_asset(executable)?;
    if executable.split('/').any(|part| part.is_empty() || part == ".") {
        bail!("Native plugin executable requires a normalized relative file path");
    }
    if is_script(Path::new(executable)) {
        bail!("Native plugin server.executable cannot select a script or interpreter shim");
    }
    if !server.get("args").is_some_and(|args| args.as_array().is_some_and(|args|
        args.iter().all(|arg| arg.as_str().is_some_and(|arg| !arg.contains('\0')))
    )) {
        bail!("Native plugin server.args must be an array of strings without NUL bytes");
    }
    for field in ["name", "description"] {
        if let Some(value) = object.get(field)
            && value.as_str().is_none_or(|s| s.trim().is_empty()) {
                bail!("Plugin manifest {field} must be a nonempty string");
            }
    }
    if let Some(value) = object.get("icon") {
        let icon = value.as_str().context("Plugin icon must be a path")?;
        validate_asset(icon)?;
        if !icon.to_ascii_lowercase().ends_with(".png") {
            bail!("Plugin icon must be a PNG");
        }
    }
    if let Some(media) = object.get("media") {
        for item in media.as_array().context("Plugin media must be an array")? {
            let item = item.as_str().context("Invalid plugin media path")?;
            if !item.starts_with("https://") {
                validate_asset(item)?;
            } else {
                url::Url::parse(item)?;
            }
        }
    }
    if let Some(build) = object.get("build") {
        let commands = build.as_array().context("Plugin build must be an array")?;
        if commands.is_empty() {
            bail!("Plugin build must contain a command");
        }
        for command in commands {
            let args = command
                .as_array()
                .context("Plugin build command must be an array")?;
            if args.is_empty()
                || args
                    .iter()
                    .any(|arg| arg.as_str().is_none_or(|arg| arg.trim().is_empty()))
            {
                bail!("Invalid plugin build command");
            }
        }
    }
    if let Some(requirements) = object.get("requirements") {
        let requirements = requirements
            .as_object()
            .context("Invalid plugin requirements")?;
        if requirements.keys().any(|key| key != "paseo") {
            bail!("Unknown plugin requirement");
        }
        if requirements
            .get("paseo")
            .is_some_and(|value| value.as_str().is_none_or(|s| s.trim().is_empty()))
        {
            bail!("Invalid requirements.paseo");
        }
    }
    if let Some(range) = manifest["requirements"]["paseo"].as_str() {
        node_semver::Range::parse(range).with_context(|| {
            format!(
                "Invalid requirements.paseo: {range}. Use a semver range such as \">=0.8.0\"."
            )
        })?;
    }
    for field in ["name", "description"] {
        if let Some(text) = manifest.get(field).and_then(Value::as_str) {
            let trimmed = text.trim().to_owned();
            manifest[field] = Value::String(trimmed);
        }
    }
    Ok(manifest)
}
#[derive(Debug)]
pub struct NativeEntry {
    pub executable: PathBuf,
    pub args: Vec<String>,
    pub manifest: Value,
}

pub async fn native_entry(directory: &Path) -> Result<NativeEntry> {
    use tokio::io::AsyncReadExt;
    let manifest = read(directory).await?;
    let relative = manifest["server"]["executable"].as_str().unwrap();
    let root = tokio::fs::canonicalize(directory).await?;
    let executable = tokio::fs::canonicalize(root.join(relative)).await
        .with_context(|| format!("Native plugin executable is missing: {relative}. Build the Rust plugin with its manifest build commands or install a compiled release."))?;
    if !executable.starts_with(&root) {
        bail!("Native plugin executable escapes the plugin directory");
    }
    if is_script(&executable) {
        bail!("Native plugin executable resolves to a script or interpreter shim");
    }
    let metadata = tokio::fs::metadata(&executable).await?;
    if !metadata.is_file() {
        bail!("Native plugin executable is not a regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            bail!("Native plugin server.executable is not executable: {relative}");
        }
    }
    let name = executable.file_stem().and_then(|name| name.to_str()).unwrap_or("").to_ascii_lowercase();
    if ["node", "nodejs", "bun", "deno", "sh", "bash", "zsh", "cmd", "powershell", "pwsh", "python", "python3", "ruby", "perl"].contains(&name.as_str()) {
        bail!("Native plugin server.executable must be the plugin binary, not an interpreter");
    }
    let mut header = [0_u8; 4];
    tokio::fs::File::open(&executable).await?.read_exact(&mut header).await
        .context("Native plugin executable has no native binary header")?;
    if !native_header(header) {
        bail!("Native plugin server.executable must be an ELF, Mach-O or PE binary, not a script or interpreter shim");
    }
    let args = manifest["server"]["args"].as_array().unwrap().iter()
        .map(|arg| arg.as_str().unwrap().to_owned()).collect();
    Ok(NativeEntry { executable, args, manifest })
}

fn is_script(path: &Path) -> bool {
    path.extension().and_then(|ext| ext.to_str()).is_some_and(|ext|
        ["js", "mjs", "cjs", "ts", "tsx", "sh", "bat", "cmd", "ps1", "py", "rb", "pl"].iter().any(|script| ext.eq_ignore_ascii_case(script))
    )
}

fn native_header(header: [u8; 4]) -> bool {
    header == *b"\x7fELF" || header[..2] == *b"MZ" ||
        matches!(u32::from_be_bytes(header), 0xfeedface | 0xcefaedfe | 0xfeedfacf | 0xcffaedfe | 0xcafebabe | 0xbebafeca | 0xcafebabf | 0xbfbafeca)
}

pub fn check_compatibility(manifest: &Value) -> Result<()> {
    check_version(manifest, env!("CARGO_PKG_VERSION"))
}
pub(super) fn check_version(manifest: &Value, version: &str) -> Result<()> {
    let id = manifest["id"].as_str().unwrap_or("");
    let requirement = manifest["requirements"]["paseo"].as_str();
    let range_text = requirement.unwrap_or("<0.8.0");
    let range = node_semver::Range::parse(range_text).context("Invalid requirements.paseo")?;
    let parsed = node_semver::Version::parse(version).context(
        "Cannot check plugin requirements: Paseo daemon version is unknown. Update the daemon.",
    )?;
    let stable = node_semver::Version::parse(version.split(['-', '+']).next().unwrap_or(version))?;
    if range.satisfies(&parsed) || range.satisfies(&stable) {
        return Ok(());
    }
    let action = if requirement.is_none() {
        "This plugin has no requirements.paseo and targets Paseo before 0.8. Ask its author to migrate it: https://paseo.sh/docs/plugins/migration"
    } else {
        "Use a compatible plugin version or update the daemon."
    };
    bail!("Plugin \"{id}\" requires Paseo {range_text}. Your daemon is {version}. {action}")
}
pub fn validate_asset(path: &str) -> Result<()> {
    if path.is_empty()
        || path != path.trim()
        || path.starts_with('/')
        || path.ends_with('/')
        || path.split('/').any(|part| part == "..")
        || path.split('/').next_back() == Some(".")
        || path
            .chars()
            .any(|ch| ch.is_control() || "\\:?#".contains(ch))
    {
        bail!("Expected a relative file path inside the plugin package");
    }
    Ok(())
}
pub async fn icon(directory: &Path, path: &str) -> Result<String> {
    let directory = tokio::fs::canonicalize(directory).await?;
    let relative = Path::new(path);
    if relative.is_absolute() {
        bail!("Provider icon must be relative to plugin directory");
    }
    let file = tokio::fs::canonicalize(directory.join(relative)).await?;
    if !file.starts_with(&directory) {
        bail!("Provider icon is outside plugin directory");
    }
    let bytes = tokio::fs::read(&file).await?;
    if bytes.len() > 64 * 1024 {
        bail!("Provider icon exceeds 64 KiB");
    }
    let svg = String::from_utf8(bytes).context("Provider icon must be UTF-8 SVG")?;
    validate_svg(&svg)?;
    Ok(svg)
}
fn validate_svg(svg: &str) -> Result<()> {
    static PATTERNS: std::sync::LazyLock<(regex::Regex, regex::Regex, regex::Regex)> =
        std::sync::LazyLock::new(|| {
            (
        regex::Regex::new(r"(?i)^\s*<svg(?:\s|>)").unwrap(),
        regex::Regex::new(r"(?i)<(?:script|foreignObject|style)(?:\s|>)|\son[a-z0-9_-]*\s*=|javascript\s*:").unwrap(),
        regex::Regex::new(r#"(?i)\s(?:href|xlink:href)\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))"#).unwrap(),
    )
        });
    let (document, unsafe_content, href) = &*PATTERNS;
    if !document.is_match(svg) {
        bail!("Provider icon is not an SVG document");
    }
    if unsafe_content.is_match(svg) {
        bail!("Provider icon contains executable content");
    }
    for captures in href.captures_iter(svg) {
        let value = captures
            .get(1)
            .or_else(|| captures.get(2))
            .or_else(|| captures.get(3))
            .unwrap()
            .as_str();
        if !value.starts_with('#') {
            bail!("Provider icon external href references are not allowed");
        }
    }
    Ok(())
}
pub fn resolve_directory(path: &str, home: &Path) -> Result<PathBuf> {
    let path = if path == "~" {
        home.to_owned()
    } else if let Some(tail) = path.strip_prefix("~/") {
        home.join(tail)
    } else {
        PathBuf::from(path)
    };
    Ok(if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    })
}
