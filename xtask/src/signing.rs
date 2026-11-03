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
fn pe_machine(path: &Path) -> Result<u16> {
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
        &pe[..4] == b"PE\0\0",
        "CRT is not a PE image: {}",
        path.display()
    );
    Ok(u16::from_le_bytes([pe[4], pe[5]]))
}

/// DLL names a PE image imports directly or through its delay-load table.
fn pe_imports(path: &Path) -> Result<Vec<String>> {
    let image = fs::read(path)?;
    let malformed = || format!("Malformed PE import table: {}", path.display());
    let u16_at = |at: usize| {
        image
            .get(at..at + 2)
            .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
    };
    let u32_at = |at: usize| {
        image
            .get(at..at + 4)
            .map(|bytes| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    };
    let pe = u32_at(60).with_context(malformed)? as usize;
    ensure!(
        image.get(..2) == Some(b"MZ".as_slice())
            && image.get(pe..pe + 4) == Some(b"PE\0\0".as_slice()),
        "Not a PE image: {}",
        path.display()
    );
    let sections = u16_at(pe + 6).with_context(malformed)? as usize;
    let optional = pe + 24;
    let section_table = optional + u16_at(pe + 20).with_context(malformed)? as usize;
    let (image_base, directories) = match u16_at(optional).with_context(malformed)? {
        0x20b => (
            image
                .get(optional + 24..optional + 32)
                .map(|bytes| u64::from_le_bytes(bytes.try_into().unwrap()))
                .with_context(malformed)?,
            optional + 112,
        ),
        0x10b => (
            u64::from(u32_at(optional + 28).with_context(malformed)?),
            optional + 96,
        ),
        magic => bail!("Unknown PE optional header {magic:#x}: {}", path.display()),
    };
    let directory_count = u32_at(directories - 4).with_context(malformed)? as usize;
    let offset = |rva: u32| {
        (0..sections).find_map(|index| {
            let header = section_table + index * 40;
            let delta = rva.checked_sub(u32_at(header + 12)?)?;
            let raw = u32_at(header + 20)? as usize;
            (delta < u32_at(header + 16)?).then_some(raw + delta as usize)
        })
    };
    let name = |rva: u32| {
        let start = offset(rva)?;
        let length = image.get(start..)?.iter().position(|&byte| byte == 0)?;
        String::from_utf8(image[start..start + length].to_vec()).ok()
    };
    let mut names = Vec::new();
    // IMAGE_IMPORT_DESCRIPTOR (Name at +12) and ImgDelayDescr (DllNameRVA at +4).
    for (directory, size, name_field) in [(1, 20, 12), (13, 32, 4)] {
        if directory >= directory_count {
            continue;
        }
        let table = u32_at(directories + directory * 8).with_context(malformed)?;
        if table == 0 {
            continue;
        }
        let mut entry = offset(table).with_context(malformed)?;
        loop {
            let mut dll = u32_at(entry + name_field).with_context(malformed)?;
            if dll == 0 {
                break;
            }
            // Delay descriptors without dlattrRva store virtual addresses.
            if directory == 13 && u32_at(entry).with_context(malformed)? & 1 == 0 {
                dll = u64::from(dll)
                    .checked_sub(image_base)
                    .and_then(|rva| u32::try_from(rva).ok())
                    .with_context(malformed)?;
            }
            names.push(name(dll).with_context(malformed)?);
            entry += size;
        }
    }
    Ok(names)
}

/// Select the CRT DLLs that `machine` processes load. The ARM64 redistributable also carries
/// x64-machine images (vcruntime140_1.dll) for emulated x64 applications; an ARM64 process can
/// never load them, so they are omitted only after proving no packaged image imports them.
fn runtime_dlls(crt: &Path, destination: &Path, machine: u16) -> Result<Vec<PathBuf>> {
    let mut dlls = Vec::new();
    let mut emulated = Vec::new();
    for entry in fs::read_dir(crt)? {
        let entry = entry?;
        if !entry.file_type()?.is_file()
            || !entry
                .path()
                .extension()
                .and_then(OsStr::to_str)
                .is_some_and(|s| s.eq_ignore_ascii_case("dll"))
        {
            continue;
        }
        let found = pe_machine(&entry.path())?;
        if found == machine {
            dlls.push(entry.path());
        } else if machine == 0xaa64 && found == 0x8664 {
            emulated.push(entry.path());
        } else {
            bail!(
                "CRT architecture mismatch: {} (machine {found:#06x})",
                entry.path().display()
            );
        }
    }
    ensure!(
        !dlls.is_empty(),
        "Visual C++ CRT directory contains no DLLs"
    );
    if emulated.is_empty() {
        return Ok(dlls);
    }
    let check_imports = |image: &Path| -> Result<()> {
        for name in pe_imports(image)? {
            if let Some(dll) = emulated.iter().find(|dll| {
                dll.file_name()
                    .and_then(OsStr::to_str)
                    .is_some_and(|file| file.eq_ignore_ascii_case(&name))
            }) {
                bail!(
                    "{} imports {name}, but the ARM64 CRT only provides an x64 image: {}",
                    image.display(),
                    dll.display()
                );
            }
        }
        Ok(())
    };
    for dll in &dlls {
        check_imports(dll)?;
    }
    for entry in walkdir::WalkDir::new(destination) {
        let entry = entry?;
        if entry.file_type().is_file()
            && entry
                .path()
                .extension()
                .and_then(OsStr::to_str)
                .is_some_and(|s| s.eq_ignore_ascii_case("dll") || s.eq_ignore_ascii_case("exe"))
        {
            check_imports(entry.path())?;
        }
    }
    for dll in emulated {
        let name = dll.file_name().unwrap().to_string_lossy();
        eprintln!("Omitting x64-only {name} from the ARM64 CRT; no packaged image imports it");
    }
    Ok(dlls)
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
    let dlls = runtime_dlls(crt, destination, machine)?;
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
    /// Minimal PE32+ image with one section holding import and delay-import tables.
    fn image(path: &Path, machine: u16, imports: &[&str], delayed: &[&str]) {
        fn put(image: &mut [u8], at: usize, bytes: &[u8]) {
            image[at..at + bytes.len()].copy_from_slice(bytes);
        }
        let mut pe = vec![0_u8; 0x400];
        put(&mut pe, 0, b"MZ");
        put(&mut pe, 60, &0x40_u32.to_le_bytes());
        put(&mut pe, 0x40, b"PE\0\0");
        put(&mut pe, 0x44, &machine.to_le_bytes());
        put(&mut pe, 0x46, &1_u16.to_le_bytes());
        put(&mut pe, 0x54, &0xf0_u16.to_le_bytes());
        put(&mut pe, 0x58, &0x20b_u16.to_le_bytes());
        put(&mut pe, 0xc4, &16_u32.to_le_bytes());
        put(&mut pe, 0xd0, &0x1000_u32.to_le_bytes());
        put(&mut pe, 0x130, &0x1080_u32.to_le_bytes());
        put(&mut pe, 0x150, &0x200_u32.to_le_bytes());
        put(&mut pe, 0x154, &0x1000_u32.to_le_bytes());
        put(&mut pe, 0x158, &0x200_u32.to_le_bytes());
        put(&mut pe, 0x15c, &0x200_u32.to_le_bytes());
        let mut name = 0x1100_u32;
        for (index, dll) in imports.iter().enumerate() {
            put(&mut pe, 0x200 + index * 20 + 12, &name.to_le_bytes());
            put(&mut pe, name as usize - 0xe00, dll.as_bytes());
            name += 0x20;
        }
        for (index, dll) in delayed.iter().enumerate() {
            put(&mut pe, 0x280 + index * 32, &1_u32.to_le_bytes());
            put(&mut pe, 0x280 + index * 32 + 4, &name.to_le_bytes());
            put(&mut pe, name as usize - 0xe00, dll.as_bytes());
            name += 0x20;
        }
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, pe).unwrap();
    }
    fn arm64_crt(root: &Path) -> PathBuf {
        let crt = root.join("arm64/Microsoft.VC145.CRT");
        image(&crt.join("vcruntime140.dll"), 0xaa64, &[], &[]);
        image(
            &crt.join("msvcp140.dll"),
            0xaa64,
            &["vcruntime140.dll"],
            &[],
        );
        image(
            &crt.join("vcruntime140_1.dll"),
            0x8664,
            &["vcruntime140.dll"],
            &[],
        );
        crt
    }
    #[test]
    fn reads_direct_and_delay_imports() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("app.exe");
        image(
            &path,
            0xaa64,
            &["KERNEL32.dll", "VCRUNTIME140.dll"],
            &["MSVCP140.dll"],
        );
        assert_eq!(
            pe_imports(&path).unwrap(),
            ["KERNEL32.dll", "VCRUNTIME140.dll", "MSVCP140.dll"]
        );
        assert_eq!(pe_machine(&path).unwrap(), 0xaa64);
    }
    #[test]
    fn arm64_crt_omits_only_unimported_x64_images() {
        let temp = tempfile::tempdir().unwrap();
        let crt = arm64_crt(temp.path());
        let payload = temp.path().join("payload");
        image(
            &payload.join("paseo_sessions.exe"),
            0xaa64,
            &["VCRUNTIME140.dll"],
            &["MSVCP140.dll"],
        );
        image(
            &payload.join("runtime/neovim/bin/win32yank.exe"),
            0x8664,
            &["VCRUNTIME140.dll"],
            &[],
        );
        let mut names = runtime_dlls(&crt, &payload, 0xaa64)
            .unwrap()
            .into_iter()
            .map(|dll| dll.file_name().unwrap().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(names, ["msvcp140.dll", "vcruntime140.dll"]);

        image(
            &payload.join("runtime/neovim/bin/tool.exe"),
            0x8664,
            &[],
            &["VCRUNTIME140_1.dll"],
        );
        assert!(runtime_dlls(&crt, &payload, 0xaa64).is_err());
    }
    #[test]
    fn crt_rejects_other_machine_mismatches() {
        let temp = tempfile::tempdir().unwrap();
        let payload = temp.path().join("payload");
        fs::create_dir_all(&payload).unwrap();
        let crt = arm64_crt(temp.path());
        assert!(runtime_dlls(&crt, &payload, 0x8664).is_err());
        image(&crt.join("vcruntime140_1.dll"), 0x14c, &[], &[]);
        assert!(runtime_dlls(&crt, &payload, 0xaa64).is_err());
        image(&crt.join("vcruntime140_1.dll"), 0xaa64, &[], &[]);
        let mut names = runtime_dlls(&crt, &payload, 0xaa64)
            .unwrap()
            .into_iter()
            .map(|dll| dll.file_name().unwrap().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(
            names,
            ["msvcp140.dll", "vcruntime140.dll", "vcruntime140_1.dll"]
        );
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
