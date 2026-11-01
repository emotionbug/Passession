use crate::{build::assert_architecture, common::*};
use anyhow::{Context, Result, ensure};
use regex::Regex;
use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    fs,
    io::Read,
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

/// Collect the complete non-host ELF closure and patch every ELF's own RPATH.
/// The daemon remains independently runnable, without an AppRun environment.
fn bundle(root: &Path, destination: &Path, notices: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    fs::create_dir_all(notices)?;
    let host = Regex::new(
        r"^(ld-linux|lib(c|m|pthread|dl|rt|resolv|nss_[^.]+|anl|util)\.so|lib(GL|GLX|EGL|GLdispatch|drm|gbm)[-.])",
    )?;
    let mut queue = VecDeque::new();
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry?;
        if entry.file_type().is_file() {
            queue.push_back(entry.into_path());
        }
    }
    let mut visited = HashSet::new();
    let mut copies = BTreeMap::<PathBuf, PathBuf>::new();
    while let Some(file) = queue.pop_front() {
        if !visited.insert(fs::canonicalize(&file)?) || !is_elf(&file)? {
            continue;
        }
        assert_architecture(&file)?;
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
            if source.starts_with(root) || host.is_match(&basename.to_string_lossy()) {
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
                queue.push_back(target);
            }
        }
        let old = capture(command("patchelf")?.arg("--print-rpath").arg(&file))?;
        let mut paths: Vec<String> = old
            .split(':')
            .filter(|value| *value == "$ORIGIN" || value.starts_with("$ORIGIN/"))
            .map(str::to_owned)
            .collect();
        let relative = relative(file.parent().context("ELF has no directory")?, destination)?;
        let local = if relative.is_empty() {
            "$ORIGIN".to_owned()
        } else {
            format!("$ORIGIN/{relative}")
        };
        if !paths.contains(&local) {
            paths.push(local);
        }
        run(command("patchelf")?
            .arg("--set-rpath")
            .arg(paths.join(":"))
            .arg(&file))?;
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
