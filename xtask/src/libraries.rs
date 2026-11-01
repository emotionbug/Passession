use crate::{build::assert_architecture, common::*};
use anyhow::{Context, Result, ensure};
use regex::Regex;
use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use walkdir::WalkDir;

fn relative(from: &Path, to: &Path) -> Result<String> {
    let from: Vec<_> = from.components().collect();
    let to: Vec<_> = to.components().collect();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    ensure!(common > 0, "Cannot make runtime library path relative");
    let mut path = PathBuf::new();
    for _ in common..from.len() {
        path.push("..");
    }
    for component in &to[common..] {
        path.push(component.as_os_str());
    }
    Ok(path.to_string_lossy().replace('\\', "/"))
}

fn is_elf(file: &Path) -> Result<bool> {
    let mut header = [0; 4];
    let mut input = fs::File::open(file)?;
    Ok(input.read(&mut header)? == 4 && header == *b"\x7fELF")
}

fn has_dynamic_segment(file: &Path) -> Result<bool> {
    let mut input = fs::File::open(file)?;
    let mut header = [0; 64];
    input.read_exact(&mut header)?;
    ensure!(
        header[4] == 2 && header[5] == 1,
        "Expected a 64-bit little-endian ELF"
    );
    let offset = u64::from_le_bytes(header[32..40].try_into()?);
    let stride = u16::from_le_bytes(header[54..56].try_into()?) as u64;
    let count = u16::from_le_bytes(header[56..58].try_into()?);
    ensure!(
        count == 0 || stride >= 56,
        "Invalid ELF program header size"
    );
    for index in 0..count {
        let position = offset
            .checked_add(u64::from(index) * stride)
            .context("ELF program header offset overflow")?;
        input.seek(SeekFrom::Start(position))?;
        let mut kind = [0; 4];
        input.read_exact(&mut kind)?;
        if u32::from_le_bytes(kind) == 2 {
            return Ok(true);
        }
    }
    Ok(false)
}

fn system_notice(source: &Path, destination: &Path) -> Result<()> {
    let name = source
        .file_name()
        .context("System dependency has no filename")?
        .to_string_lossy();
    let ownership = capture(command("dpkg-query")?.args(["-S", &format!("*/{name}")]))?;
    let package = ownership
        .lines()
        .next()
        .and_then(|line| line.split_once(": "))
        .map(|(owner, _)| owner.split(':').next().unwrap_or(owner))
        .context("Cannot identify redistributed system library owner")?;
    let copyright = Path::new("/usr/share/doc").join(package).join("copyright");
    ensure!(
        copyright.is_file(),
        "Missing redistribution notice for {}",
        source.display()
    );
    copy_tree(
        &copyright,
        &destination.join(format!("{package}.copyright")),
    )
}

fn relocate_elf(file: &Path, destination: &Path) -> Result<bool> {
    if !is_elf(file)? {
        return Ok(false);
    }
    assert_architecture(file)?;
    if !has_dynamic_segment(file)? {
        return Ok(false);
    }
    let old = capture(command("patchelf")?.arg("--print-rpath").arg(file))?;
    let mut paths: Vec<String> = old
        .split(':')
        .filter(|value| *value == "$ORIGIN" || value.starts_with("$ORIGIN/"))
        .map(str::to_owned)
        .collect();
    if !paths.iter().any(|path| path == "$ORIGIN") {
        paths.push("$ORIGIN".to_owned());
    }
    let relative = relative(file.parent().context("ELF has no directory")?, destination)?;
    let local = if relative.is_empty() {
        "$ORIGIN".to_owned()
    } else {
        format!("$ORIGIN/{relative}")
    };
    if !paths.contains(&local) {
        paths.push(local);
    }
    let relocated = paths.join(":");
    if relocated != old {
        run(command("patchelf")?
            .arg("--set-rpath")
            .arg(relocated)
            .arg(file))?;
    }
    Ok(true)
}

/// Collect the complete non-host ELF closure and patch every ELF's own RPATH.
/// The daemon remains independently runnable, without an AppRun environment.
fn bundle(root: &Path, destination: &Path, notices: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    fs::create_dir_all(notices)?;
    let root = fs::canonicalize(root)?;
    let destination = fs::canonicalize(destination)?;
    let host = Regex::new(
        r"^(ld-linux|lib(c|m|pthread|dl|rt|resolv|nss_[^.]+|anl|util)\.so|lib(GL|GLX|EGL|GLdispatch|drm|gbm)[-.])",
    )?;
    let mut queue = VecDeque::new();
    // Relocate the whole payload before resolving transitive dependencies.
    // Otherwise a plugin's old absolute RPATH can select the build-tree engine.
    for entry in WalkDir::new(&root).follow_links(false) {
        let entry = entry?;
        if entry.file_type().is_file() && relocate_elf(entry.path(), &destination)? {
            queue.push_back(entry.into_path());
        }
    }
    let mut visited = HashSet::new();
    let mut copies = BTreeMap::<PathBuf, PathBuf>::new();
    while let Some(file) = queue.pop_front() {
        if !visited.insert(fs::canonicalize(&file)?) {
            continue;
        }
        let output = Command::new("ldd")
            .arg(&file)
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .output()
            .context("Starting ldd")?;
        let result = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if result.contains("not a dynamic executable") || result.contains("statically linked") {
            continue;
        }
        ensure!(
            output.status.success() && !result.contains("=> not found"),
            "Unresolved runtime libraries for {}:\n{result}",
            file.display()
        );
        for line in result.lines() {
            let Some((_, tail)) = line.split_once("=>") else {
                continue;
            };
            let Some(source) = tail
                .split_whitespace()
                .next()
                .filter(|s| s.starts_with('/'))
            else {
                continue;
            };
            let source = Path::new(source);
            let basename = source.file_name().context("Invalid ldd library path")?;
            if fs::canonicalize(source)?.starts_with(&root)
                || host.is_match(&basename.to_string_lossy())
            {
                continue;
            }
            let target = destination.join(basename);
            if let Some(previous) = copies.get(&target) {
                ensure!(
                    fs::canonicalize(previous)? == fs::canonicalize(source)?
                        || sha256_file(previous)? == sha256_file(source)?,
                    "Conflicting shared library {}",
                    basename.to_string_lossy()
                );
            } else {
                ensure!(
                    !target.exists(),
                    "Unexpected preexisting shared library {}",
                    target.display()
                );
                copy_tree(source, &target)?;
                copies.insert(target.clone(), source.to_path_buf());
                if relocate_elf(&target, &destination)? {
                    queue.push_back(target);
                }
            }
        }
    }
    for source in copies.values() {
        system_notice(source, notices)?;
    }
    Ok(())
}

pub fn native(runtime: &Path) -> Result<()> {
    bundle(
        runtime,
        &runtime.join("lib"),
        &runtime.join("lib/licenses/system"),
    )
}

pub fn appimage(appdir: &Path) -> Result<()> {
    let executables = appdir.join("usr/bin");
    fs::create_dir_all(&executables)?;
    // Speech capture/encoding programs are deliberately not a runtime payload.
    for name in ["xdg-email", "xdg-open", "xdg-mime", "xdg-settings"] {
        let source = which::which(name)
            .with_context(|| format!("Install xdg-utils before packaging (missing {name})"))?;
        copy_tree(&source, &executables.join(name))?;
        chmod(&executables.join(name), 0o755)?;
    }
    let notices = appdir.join("usr/share/doc/paseo-bundled-dependencies");
    bundle(appdir, &appdir.join("usr/lib/paseo-dependencies"), &notices)?;
    copy_tree(
        Path::new("/usr/share/doc/xdg-utils/copyright"),
        &notices.join("xdg-utils.copyright"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dynamic_detection_uses_program_header_offsets_and_stride() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("fixture");
        let mut bytes = vec![0_u8; 240];
        bytes[..6].copy_from_slice(b"\x7fELF\x02\x01");
        bytes[32..40].copy_from_slice(&128_u64.to_le_bytes());
        bytes[54..56].copy_from_slice(&56_u16.to_le_bytes());
        bytes[56..58].copy_from_slice(&2_u16.to_le_bytes());
        bytes[128..132].copy_from_slice(&1_u32.to_le_bytes());
        bytes[184..188].copy_from_slice(&2_u32.to_le_bytes());
        fs::write(&path, &bytes)?;
        assert!(has_dynamic_segment(&path)?);
        bytes[184..188].copy_from_slice(&1_u32.to_le_bytes());
        fs::write(&path, &bytes)?;
        assert!(!has_dynamic_segment(&path)?);
        bytes[54..56].copy_from_slice(&4_u16.to_le_bytes());
        fs::write(&path, &bytes)?;
        assert!(has_dynamic_segment(&path).is_err());
        Ok(())
    }

    #[test]
    fn library_paths_are_relocatable_at_each_runtime_depth() {
        assert_eq!(
            relative(Path::new("/runtime"), Path::new("/runtime/lib")).unwrap(),
            "lib"
        );
        assert_eq!(
            relative(Path::new("/runtime/neovim/bin"), Path::new("/runtime/lib")).unwrap(),
            "../../lib"
        );
        assert_eq!(
            relative(Path::new("/runtime/lib"), Path::new("/runtime/lib")).unwrap(),
            ""
        );
    }
}
