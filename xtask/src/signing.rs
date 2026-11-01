use anyhow::{Context, Result, bail, ensure};
use base64::Engine;
use std::{
    env,
    ffi::OsStr,
    fs,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::Command,
};

fn setting(name: &str) -> Option<String> {
    env::var(name).ok().filter(|s| !s.is_empty())
}

// Never render command arguments or tool output from credential-bearing commands.
fn execute(command: &mut Command, sensitive: bool) -> Result<String> {
    let program = command.get_program().to_string_lossy().into_owned();
    let output = command
        .output()
        .with_context(|| format!("Starting {program}"))?;
    if !output.status.success() {
        if sensitive {
            bail!("{program} failed ({})", output.status);
        }
        bail!(
            "{program} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn certificate(source: &str, destination: &Path) -> Result<()> {
    let bytes = if source.starts_with("https://") {
        let client = reqwest::blocking::Client::builder()
            .https_only(true)
            .build()?;
        // URL credentials and query parameters must not appear in error messages.
        let response = client
            .get(source)
            .send()
            .map_err(|_| anyhow::anyhow!("Certificate HTTPS download failed"))?;
        ensure!(
            response.status().is_success(),
            "Certificate download returned HTTP {}",
            response.status()
        );
        response
            .bytes()
            .map_err(|_| anyhow::anyhow!("Reading certificate download failed"))?
            .to_vec()
    } else if Path::new(source).is_file() {
        fs::read(source).map_err(|_| anyhow::anyhow!("Reading certificate file failed"))?
    } else {
        base64::engine::general_purpose::STANDARD
            .decode(
                source
                    .chars()
                    .filter(|c| !c.is_whitespace())
                    .collect::<String>(),
            )
            .map_err(|_| {
                anyhow::anyhow!("Certificate must be an existing path, HTTPS URL, or valid base64")
            })?
    };
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(destination)
        .context("Creating private certificate file")?;
    file.write_all(&bytes)
        .context("Writing private certificate file")?;
    Ok(())
}

pub fn sign_mac(bundle: &Path, stage: &Path, application: bool) -> Result<()> {
    let notary = [
        setting("APPLE_ID"),
        setting("APPLE_APP_SPECIFIC_PASSWORD"),
        setting("APPLE_TEAM_ID"),
    ];
    let count = notary.iter().filter(|v| v.is_some()).count();
    ensure!(
        count == 0 || count == 3,
        "Notarization requires APPLE_ID, APPLE_APP_SPECIFIC_PASSWORD and APPLE_TEAM_ID together."
    );
    fs::create_dir_all(stage)?;
    let temporary = tempfile::Builder::new()
        .prefix("signing-")
        .tempdir_in(stage)?;
    let keychain = temporary.path().join("signing.keychain-db");
    let mut original: Option<Vec<String>> = None;
    let mut created = false;
    let result = (|| -> Result<()> {
        let mut identity = setting("CSC_NAME").unwrap_or_else(|| "-".into());
        if let Some(source) = setting("CSC_LINK") {
            let listing = execute(
                Command::new("security").args(["list-keychains", "-d", "user"]),
                true,
            )?;
            original = Some(
                regex::Regex::new(r#""([^"]+)""#)?
                    .captures_iter(&listing)
                    .map(|c| c[1].to_owned())
                    .collect(),
            );
            let cert = temporary.path().join("certificate.p12");
            certificate(&source, &cert)?;
            let password = hex::encode(rand::random::<[u8; 24]>());
            // Even a failed creation may have created a keychain; always attempt cleanup.
            created = true;
            execute(
                Command::new("security")
                    .args(["create-keychain", "-p", &password])
                    .arg(&keychain),
                true,
            )?;
            execute(
                Command::new("security")
                    .args(["set-keychain-settings", "-lut", "21600"])
                    .arg(&keychain),
                true,
            )?;
            execute(
                Command::new("security")
                    .args(["unlock-keychain", "-p", &password])
                    .arg(&keychain),
                true,
            )?;
            execute(
                Command::new("security")
                    .arg("import")
                    .arg(&cert)
                    .arg("-k")
                    .arg(&keychain)
                    .arg("-P")
                    .arg(setting("CSC_KEY_PASSWORD").unwrap_or_default())
                    .args(["-T", "/usr/bin/codesign"]),
                true,
            )?;
            execute(
                Command::new("security")
                    .args([
                        "set-key-partition-list",
                        "-S",
                        "apple-tool:,apple:,codesign:",
                        "-s",
                        "-k",
                        &password,
                    ])
                    .arg(&keychain),
                true,
            )?;
            execute(
                Command::new("security")
                    .args(["list-keychains", "-d", "user", "-s"])
                    .arg(&keychain)
                    .args(original.as_ref().unwrap()),
                true,
            )?;
            let identities = execute(
                Command::new("security")
                    .args(["find-identity", "-v", "-p", "codesigning"])
                    .arg(&keychain),
                true,
            )?;
            identity = regex::Regex::new(r#"([A-F0-9]{40}) "Developer ID Application:[^"]+""#)?
                .captures(&identities)
                .context("Certificate contains no Developer ID Application identity")?[1]
                .to_owned();
        }
        ensure!(
            count == 0 || identity != "-",
            "Notarization requires Developer ID signing, not ad-hoc signing."
        );
        let entitlements = crate::common::root()?
            .join("packages/flutter_desktop_host/packaging/entitlements.plist");
        let mut paths = Vec::new();
        for entry in walkdir::WalkDir::new(bundle)
            .follow_links(false)
            .contents_first(true)
            .min_depth(1)
        {
            let entry = entry?;
            if entry.file_type().is_symlink() {
                continue;
            }
            let path = entry.path();
            if entry.file_type().is_dir() {
                if matches!(
                    path.extension().and_then(OsStr::to_str),
                    Some("framework" | "app" | "xpc")
                ) {
                    paths.push(path.to_owned());
                }
            } else if entry.file_type().is_file() {
                let mut magic = [0; 4];
                let n = fs::File::open(path)?.read(&mut magic)?;
                if n == 4
                    && matches!(
                        u32::from_be_bytes(magic),
                        0xfeedface
                            | 0xfeedfacf
                            | 0xcefaedfe
                            | 0xcffaedfe
                            | 0xcafebabe
                            | 0xbebafeca
                            | 0xcafebabf
                            | 0xbfbafeca
                    )
                {
                    paths.push(path.to_owned());
                }
            }
        }
        if application {
            paths.push(bundle.to_owned());
        }
        for path in &paths {
            if application && path == bundle {
                crate::build::refresh_runtime_hash(&bundle.join("Contents/Resources/runtime"))?;
            }
            let mut cmd = Command::new("codesign");
            cmd.args([
                "--force",
                "--sign",
                &identity,
                "--options",
                "runtime",
                "--entitlements",
            ])
            .arg(&entitlements);
            if identity != "-" {
                cmd.arg("--timestamp");
            }
            execute(cmd.arg(path), false)?;
        }
        if !application {
            crate::build::refresh_runtime_hash(bundle)?;
        }
        if application {
            execute(
                Command::new("codesign")
                    .args(["--verify", "--deep", "--strict"])
                    .arg(bundle),
                false,
            )?;
        } else {
            for path in &paths {
                execute(
                    Command::new("codesign")
                        .args(["--verify", "--strict"])
                        .arg(path),
                    false,
                )?;
            }
        }
        if count == 3 {
            let upload = temporary.path().join("notary.zip");
            execute(
                Command::new("ditto")
                    .args(["-c", "-k", "--sequesterRsrc", "--keepParent"])
                    .arg(bundle)
                    .arg(&upload),
                false,
            )?;
            execute(
                Command::new("xcrun")
                    .args(["notarytool", "submit"])
                    .arg(&upload)
                    .arg("--apple-id")
                    .arg(notary[0].as_ref().unwrap())
                    .arg("--password")
                    .arg(notary[1].as_ref().unwrap())
                    .arg("--team-id")
                    .arg(notary[2].as_ref().unwrap())
                    .arg("--wait"),
                true,
            )?;
            if application {
                execute(
                    Command::new("xcrun")
                        .args(["stapler", "staple"])
                        .arg(bundle),
                    false,
                )?;
            }
        }
        Ok(())
    })();
    let restore = if let Some(original) = original {
        execute(
            Command::new("security")
                .args(["list-keychains", "-d", "user", "-s"])
                .args(original),
            true,
        )
        .map(|_| ())
    } else {
        Ok(())
    };
    let delete = if created && keychain.exists() {
        execute(
            Command::new("security")
                .arg("delete-keychain")
                .arg(&keychain),
            true,
        )
        .map(|_| ())
    } else {
        Ok(())
    };
    let cleanup = restore.and(delete);
    match (result, cleanup) {
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("Temporary keychain cleanup also failed: {cleanup}")))
        }
        (Err(error), _) => Err(error),
        (Ok(()), cleanup) => cleanup,
    }
}

pub fn sign_windows(files: &[PathBuf], stage: &Path) -> Result<()> {
    let Some(source) = setting("WIN_CSC_LINK") else {
        return Ok(());
    };
    fs::create_dir_all(stage)?;
    let temporary = tempfile::Builder::new()
        .prefix("signing-")
        .tempdir_in(stage)?;
    let cert = temporary.path().join("windows.pfx");
    certificate(&source, &cert)?;
    for file in files {
        execute(
            Command::new(setting("SIGNTOOL").unwrap_or_else(|| "signtool.exe".into()))
                .args([
                    "sign",
                    "/fd",
                    "SHA256",
                    "/td",
                    "SHA256",
                    "/tr",
                    "http://timestamp.digicert.com",
                    "/f",
                ])
                .arg(&cert)
                .arg("/p")
                .arg(setting("WIN_CSC_KEY_PASSWORD").unwrap_or_default())
                .arg(file),
            true,
        )?;
    }
    Ok(())
}

fn host_arch() -> Result<(&'static str, u16)> {
    match env::consts::ARCH {
        "x86_64" => Ok(("x64", 0x8664)),
        "aarch64" => Ok(("arm64", 0xaa64)),
        arch => bail!("Unsupported Windows runtime architecture: {arch}"),
    }
}
fn validate_pe(path: &Path, machine: u16) -> Result<()> {
    let mut file = fs::File::open(path)?;
    let mut header = [0; 64];
    file.read_exact(&mut header)
        .context("Reading CRT DOS header")?;
    ensure!(
        &header[..2] == b"MZ",
        "CRT is not a PE image: {}",
        path.display()
    );
    file.seek(SeekFrom::Start(
        u32::from_le_bytes(header[60..64].try_into()?) as u64,
    ))?;
    let mut pe = [0; 6];
    file.read_exact(&mut pe)?;
    ensure!(
        &pe[..4] == b"PE\0\0" && u16::from_le_bytes([pe[4], pe[5]]) == machine,
        "CRT architecture mismatch: {}",
        path.display()
    );
    Ok(())
}

pub fn windows_runtime(destination: &Path, standalone: bool) -> Result<()> {
    let (arch, machine) = host_arch()?;
    let redist = if let Some(path) = setting("VCToolsRedistDir") {
        PathBuf::from(path)
    } else {
        let vswhere = PathBuf::from(
            setting("ProgramFiles(x86)").unwrap_or_else(|| "C:/Program Files (x86)".into()),
        )
        .join("Microsoft Visual Studio/Installer/vswhere.exe");
        let installation = execute(
            Command::new(vswhere).args([
                "-latest",
                "-products",
                "*",
                "-requires",
                "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
                "-property",
                "installationPath",
            ]),
            false,
        )?;
        ensure!(
            !installation.is_empty(),
            "Visual Studio C++ tools not found; set VCToolsRedistDir."
        );
        let directory = PathBuf::from(installation).join("VC/Redist/MSVC");
        let mut versions = Vec::new();
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Ok(parts) = name
                .split('.')
                .map(str::parse::<u64>)
                .collect::<std::result::Result<Vec<_>, _>>()
            {
                versions.push((parts, entry.path()));
            }
        }
        versions.sort_by(|a, b| b.0.cmp(&a.0));
        versions
            .into_iter()
            .next()
            .context("Visual C++ redistributable files are unavailable")?
            .1
    };
    let crt_pattern = regex::Regex::new(r"^Microsoft\.VC\d+\.CRT$")?;
    let mut crts = Vec::new();
    for entry in
        fs::read_dir(redist.join(arch)).context("Reading architecture-specific CRT directory")?
    {
        let entry = entry?;
        if entry.file_type()?.is_dir() && crt_pattern.is_match(&entry.file_name().to_string_lossy())
        {
            crts.push(entry.path());
        }
    }
    crts.sort();
    let crt = crts
        .last()
        .context("No architecture-specific Visual C++ CRT found")?;
    let mut dlls = Vec::new();
    for entry in fs::read_dir(crt)? {
        let entry = entry?;
        if entry.file_type()?.is_file()
            && entry
                .path()
                .extension()
                .and_then(OsStr::to_str)
                .is_some_and(|s| s.eq_ignore_ascii_case("dll"))
        {
            validate_pe(&entry.path(), machine)?;
            dlls.push(entry.path());
        }
    }
    ensure!(
        !dlls.is_empty(),
        "Visual C++ CRT directory contains no DLLs"
    );
    let mut destinations = vec![destination.to_owned()];
    if !standalone {
        destinations.extend([
            destination.join("runtime"),
            destination.join("runtime/neovim/bin"),
        ]);
    }
    for directory in destinations {
        fs::create_dir_all(&directory)?;
        for dll in &dlls {
            fs::copy(dll, directory.join(dll.file_name().unwrap()))?;
        }
    }
    Ok(())
}

const INVENTORY: &str = ".paseo-sessions-owned-files";
fn nsis_quote(value: &str) -> String {
    value.replace('$', "$$").replace('"', "$\\\"")
}
fn safe_component(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.ends_with(['.', ' '])
        && !name.chars().any(|c| {
            c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
        })
        && !matches!(
            name.split('.')
                .next()
                .unwrap_or("")
                .to_ascii_uppercase()
                .as_str(),
            "CON"
                | "PRN"
                | "AUX"
                | "NUL"
                | "COM1"
                | "COM2"
                | "COM3"
                | "COM4"
                | "COM5"
                | "COM6"
                | "COM7"
                | "COM8"
                | "COM9"
                | "LPT1"
                | "LPT2"
                | "LPT3"
                | "LPT4"
                | "LPT5"
                | "LPT6"
                | "LPT7"
                | "LPT8"
                | "LPT9"
        )
}
pub fn nsis_inventory(payload: &Path, directory: &Path) -> Result<(PathBuf, PathBuf)> {
    ensure!(
        fs::symlink_metadata(payload)?.file_type().is_dir(),
        "Installer payload must be a real directory"
    );
    let mut files = Vec::new();
    let mut directories = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for entry in walkdir::WalkDir::new(payload)
        .follow_links(false)
        .sort_by_file_name()
        .min_depth(1)
    {
        let entry = entry?;
        let relative = entry.path().strip_prefix(payload)?;
        let parts = relative
            .iter()
            .map(|s| s.to_str().context("Non-UTF-8 installer member"))
            .collect::<Result<Vec<_>>>()?;
        ensure!(
            parts.iter().all(|s| safe_component(s)),
            "Unsafe installer member: {}",
            relative.display()
        );
        let member = parts.join("\\");
        ensure!(
            seen.insert(member.to_lowercase()),
            "Case-insensitive installer member collision: {member}"
        );
        ensure!(
            !member.eq_ignore_ascii_case("Uninstall.exe")
                && !member.eq_ignore_ascii_case(INVENTORY),
            "Reserved installer member: {member}"
        );
        if entry.file_type().is_dir() {
            directories.push(member);
        } else if entry.file_type().is_file() {
            files.push(member);
        } else {
            bail!("Unsupported installer payload member (links are forbidden): {member}");
        }
    }
    files.extend(["Uninstall.exe".to_owned(), INVENTORY.to_owned()]);
    let mut source = String::from("!macro PaseoCheckOwnedFiles\n");
    for member in &files {
        let escaped = nsis_quote(member);
        source.push_str(&format!("  IfFileExists \"$INSTDIR\\{escaped}\" 0 +3\n    Push \"{escaped}\"\n    Call PaseoCheckOwnedFile\n"));
    }
    source.push_str("!macroend\n\n!macro PaseoRemoveOwnedFiles\n");
    for member in &files {
        source.push_str(&format!("  Delete \"$INSTDIR\\{}\"\n", nsis_quote(member)));
    }
    directories.sort_by(|a, b| {
        b.split('\\')
            .count()
            .cmp(&a.split('\\').count())
            .then_with(|| b.cmp(a))
    });
    // Never recurse: directories containing user-created files must survive uninstall.
    for member in directories {
        source.push_str(&format!("  RMDir \"$INSTDIR\\{}\"\n", nsis_quote(&member)));
    }
    source.push_str("  RMDir \"$INSTDIR\"\n!macroend\n");
    fs::create_dir_all(directory)?;
    let manifest = directory.join("installed-files.txt");
    let include = directory.join("installed-files.nsh");
    fs::write(&manifest, format!("{}\r\n", files.join("\r\n")))?;
    fs::write(&include, source)?;
    Ok((manifest, include))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn inventory_preserves_nested_ownership_and_escapes_dollars() {
        let temp = tempfile::tempdir().unwrap();
        let payload = temp.path().join("payload");
        fs::create_dir_all(payload.join("a/b")).unwrap();
        fs::write(payload.join("a/b/$value.dll"), b"data").unwrap();
        let (manifest, include) = nsis_inventory(&payload, &temp.path().join("out")).unwrap();
        assert_eq!(
            fs::read_to_string(manifest).unwrap(),
            "a\\b\\$value.dll\r\nUninstall.exe\r\n.paseo-sessions-owned-files\r\n"
        );
        let source = fs::read_to_string(include).unwrap();
        assert!(source.contains("Delete \"$INSTDIR\\a\\b\\$$value.dll\""));
        assert!(!source.contains("RMDir /r"));
        assert!(
            source.find("RMDir \"$INSTDIR\\a\\b\"").unwrap()
                < source.find("RMDir \"$INSTDIR\\a\"").unwrap()
        );
    }
    #[test]
    fn rejects_windows_unsafe_components() {
        for name in [
            "..", "a\\b", "a/b", "a\nb", "a:b", "*.dll", "foo.", "foo ", "NUL.txt", "COM1", "a\"b",
        ] {
            assert!(!safe_component(name), "{name:?}");
        }
        assert!(safe_component("$safe-file.dll"));
    }
    #[test]
    fn rejects_reserved_uninstaller() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("Uninstall.exe"), b"").unwrap();
        assert!(nsis_inventory(temp.path(), &temp.path().join("out")).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn rejects_links_and_case_collisions() {
        let temp = tempfile::tempdir().unwrap();
        let payload = temp.path().join("payload");
        fs::create_dir(&payload).unwrap();
        std::os::unix::fs::symlink("/outside", payload.join("link")).unwrap();
        assert!(nsis_inventory(&payload, &temp.path().join("out")).is_err());
        fs::remove_file(payload.join("link")).unwrap();
        fs::write(payload.join("A.dll"), b"").unwrap();
        fs::write(payload.join("a.dll"), b"").unwrap();
        assert!(nsis_inventory(&payload, &temp.path().join("out")).is_err());
    }
}
