use crate::{build, common::*, libraries, signing};
use anyhow::{Context, Result, bail, ensure};
use base64::Engine;
use flate2::{Compression, write::GzEncoder};
use serde_json::json;
use sha2::{Digest, Sha512};
use std::{
    env, fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};
use walkdir::WalkDir;

fn desktop_entry(command: &str) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName=Paseo Sessions\nComment=Native client for local AI coding agents\nExec={command} %U\nIcon=sh.paseo.Paseo\nTerminal=false\nCategories=Development;\nMimeType=x-scheme-handler/paseo;\nStartupWMClass=sh.paseo.sessions.desktop\n"
    )
}

fn build_input() -> Result<PathBuf> {
    let metadata = read_json(&release()?.join(format!("build-{}-{}.json", platform()?, arch()?)))?;
    ensure!(
        metadata["version"] == version()?
            && metadata["mode"] == "release"
            && metadata["platform"] == platform()?
            && metadata["arch"] == arch()?,
        "Build a matching release with cargo xtask build-flutter first"
    );
    let relative = Path::new(
        metadata["output"]
            .as_str()
            .context("Build metadata lacks output")?,
    );
    ensure!(
        !relative.is_absolute()
            && relative
                .components()
                .all(|part| matches!(part, Component::Normal(_))),
        "Build output must be a relative workspace path without traversal"
    );
    let input = fs::canonicalize(root()?.join(relative)).context("Flutter build output missing")?;
    ensure!(
        input.starts_with(fs::canonicalize(app()?.join("build"))?),
        "Flutter build metadata points outside its build directory"
    );
    build::assert_architecture(&input.join(match platform()? {
        "macos" => "Contents/MacOS/Paseo Sessions",
        "windows" => "paseo_sessions.exe",
        _ => "paseo_sessions",
    }))?;
    build::validate_runtime(
        &input.join(if platform()? == "macos" {
            "Contents/Resources/runtime"
        } else {
            "runtime"
        }),
        "desktop-bundled",
        "release",
    )?;
    Ok(input)
}

pub fn desktop(args: &[String]) -> Result<()> {
    ensure!(
        args.len() <= 1
            && args
                .first()
                .is_none_or(|value| value == platform().unwrap_or("")),
        "Usage: cargo xtask package-desktop [linux|macos|windows] (native host required)"
    );
    let input = build_input()?;
    fs::create_dir_all(release()?)?;
    let stage = tempfile::Builder::new()
        .prefix("paseo-flutter-package-")
        .tempdir()?;
    let mut artifacts = Vec::new();
    match platform()? {
        "linux" => {
            let appdir = stage.path().join("Paseo-Sessions.AppDir");
            copy_tree(&input, &appdir.join("usr/lib/paseo-sessions"))?;
            libraries::appimage(&appdir)?;
            build::refresh_runtime_hash(&appdir.join("usr/lib/paseo-sessions/runtime"))?;
            text(
                &appdir.join("AppRun"),
                "#!/bin/sh\nset -eu\nHERE=\"$(CDPATH= cd -- \"$(dirname -- \"$0\")\" && pwd)\"\nexport PATH=\"$HERE/usr/bin:$PATH\"\nexport LD_LIBRARY_PATH=\"$HERE/usr/lib/paseo-dependencies${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}\"\nif [ \"${1-}\" = \"--paseo-cli\" ]; then\n  shift\n  exec \"$HERE/usr/lib/paseo-sessions/runtime/paseo\" \"$@\"\nfi\nexec \"$HERE/usr/lib/paseo-sessions/paseo_sessions\" \"$@\"\n",
                0o755,
            )?;
            text(
                &appdir.join("sh.paseo.Paseo.desktop"),
                desktop_entry("paseo_sessions"),
                0o644,
            )?;
            let icon = support()?.join("assets/icon.png");
            copy_tree(&icon, &appdir.join("sh.paseo.Paseo.png"))?;
            copy_tree(&icon, &appdir.join(".DirIcon"))?;
            let runtime = env::var_os("APPIMAGE_RUNTIME_FILE").context("Set APPIMAGE_RUNTIME_FILE to a pinned AppImage type-2 runtime; cargo xtask appimage-tools provisions it")?;
            let runtime = absolute(runtime)?;
            build::assert_architecture(&runtime)?;
            let image =
                release()?.join(format!("{}.AppImage", artifact_stem(platform()?, arch()?)?));
            run(
                command(env::var_os("APPIMAGETOOL").unwrap_or_else(|| "appimagetool".into()))?
                    .arg("--runtime-file")
                    .arg(runtime)
                    .arg(&appdir)
                    .arg(&image)
                    .env(
                        "ARCH",
                        if arch()? == "x64" {
                            "x86_64"
                        } else {
                            "aarch64"
                        },
                    )
                    .env("APPIMAGE_EXTRACT_AND_RUN", "1"),
            )?;
            artifacts.push(image);
            let deb = stage.path().join("deb");
            copy_tree(&input, &deb.join("opt/paseo-sessions"))?;
            text(
                &deb.join("usr/bin/paseo-sessions"),
                "#!/bin/sh\nexec /opt/paseo-sessions/paseo_sessions \"$@\"\n",
                0o755,
            )?;
            text(
                &deb.join("usr/share/applications/sh.paseo.Paseo.desktop"),
                desktop_entry("/usr/bin/paseo-sessions"),
                0o644,
            )?;
            copy_tree(
                &icon,
                &deb.join("usr/share/icons/hicolor/512x512/apps/sh.paseo.Paseo.png"),
            )?;
            let libc = capture(command("getconf")?.arg("GNU_LIBC_VERSION"))?;
            let libc = libc
                .split_whitespace()
                .nth(1)
                .context("Cannot determine Linux glibc baseline")?;
            ensure!(
                regex::Regex::new(r"^\d+\.\d+$")?.is_match(libc),
                "Invalid Linux glibc baseline"
            );
            text(
                &deb.join("DEBIAN/control"),
                format!(
                    "Package: paseo-sessions\nVersion: {}\nArchitecture: {}\nMaintainer: Paseo contributors <hello@moboudra.com>\nSection: devel\nPriority: optional\nDepends: libgtk-3-0, libglib2.0-0, libstdc++6, libc6 (>= {libc}), libsecret-1-0, libayatana-appindicator3-1, xdg-utils\nDescription: Paseo Sessions native Flutter client\n",
                    version()?,
                    if arch()? == "x64" { "amd64" } else { "arm64" }
                ),
                0o644,
            )?;
            text(
                &deb.join("DEBIAN/postinst"),
                "#!/bin/sh\nset -e\nif command -v update-desktop-database >/dev/null 2>&1; then update-desktop-database /usr/share/applications; fi\nif command -v gtk-update-icon-cache >/dev/null 2>&1; then gtk-update-icon-cache -q -t -f /usr/share/icons/hicolor || true; fi\n",
                0o755,
            )?;
            let output = release()?.join(format!("{}.deb", artifact_stem(platform()?, arch()?)?));
            run(command("dpkg-deb")?
                .args(["--root-owner-group", "--build"])
                .arg(&deb)
                .arg(&output))?;
            artifacts.push(output);
        }
        "windows" => {
            let payload = stage.path().join("windows");
            copy_tree(&input, &payload)?;
            signing::windows_runtime(&payload, false)?;
            signing::sign_windows(
                &[
                    payload.join("paseo_sessions.exe"),
                    payload.join("runtime/paseo.exe"),
                ],
                stage.path(),
            )?;
            build::refresh_runtime_hash(&payload.join("runtime"))?;
            let (manifest, include) = signing::nsis_inventory(&payload, stage.path())?;
            let nsis = env::var_os("MAKENSIS")
                .map(PathBuf::from)
                .or_else(|| {
                    [
                        ("ProgramFiles(x86)", "C:/Program Files (x86)"),
                        ("ProgramFiles", "C:/Program Files"),
                    ]
                    .into_iter()
                    .map(|(key, fallback)| {
                        PathBuf::from(env::var_os(key).unwrap_or_else(|| fallback.into()))
                            .join("NSIS/makensis.exe")
                    })
                    .find(|path| path.is_file())
                })
                .unwrap_or_else(|| PathBuf::from("makensis.exe"));
            let installer =
                release()?.join(format!("{}.exe", artifact_stem(platform()?, arch()?)?));
            let version = version()?;
            run(command(nsis)?
                .arg("/V3")
                .arg(format!("/DSOURCE={}", payload.display()))
                .arg(format!("/DINVENTORY_INCLUDE={}", include.display()))
                .arg(format!("/DINVENTORY_MANIFEST={}", manifest.display()))
                .arg(format!("/DOUTPUT={}", installer.display()))
                .arg(format!("/DVERSION={version}"))
                .arg(format!(
                    "/DNUMERIC_VERSION={}.0",
                    version.split('-').next().context("Invalid version")?
                ))
                .arg(format!(
                    "/DICON={}",
                    support()?.join("assets/icon.ico").display()
                ))
                .arg(support()?.join("installer.nsi")))?;
            signing::sign_windows(std::slice::from_ref(&installer), stage.path())?;
            artifacts.push(installer);
        }
        "macos" => {
            let bundle = stage.path().join("Paseo Sessions.app");
            // ditto retains bundle/framework symlinks and macOS metadata.
            run(command("ditto")?.arg(&input).arg(&bundle))?;
            let plist = bundle.join("Contents/Info.plist");
            let identifier = capture(
                command("/usr/libexec/PlistBuddy")?
                    .args(["-c", "Print :CFBundleIdentifier"])
                    .arg(&plist),
            )?;
            ensure!(
                identifier == "sh.paseo.sessions.desktop",
                "Unexpected macOS bundle ID: {identifier}"
            );
            run(command("/usr/libexec/PlistBuddy")?
                .arg("-c")
                .arg(format!("Set :CFBundleShortVersionString {}", version()?))
                .arg(&plist))?;
            run(command("/usr/libexec/PlistBuddy")?
                .args(["-c", "Set :LSMinimumSystemVersion 13.0"])
                .arg(&plist))?;
            signing::sign_mac(&bundle, stage.path(), true)?;
            let output = release()?.join(format!("{}.zip", artifact_stem(platform()?, arch()?)?));
            run(command("ditto")?
                .args(["-c", "-k", "--sequesterRsrc", "--keepParent"])
                .arg(&bundle)
                .arg(&output))?;
            artifacts.push(output);
        }
        other => bail!("Unsupported desktop platform: {other}"),
    }
    let names: Vec<_> = artifacts
        .iter()
        .map(|file| {
            file.file_name()
                .context("Artifact has no filename")
                .map(|name| name.to_string_lossy().into_owned())
        })
        .collect::<Result<_>>()?;
    for file in &artifacts {
        ensure!(
            file.is_file() && fs::metadata(file)?.len() > 0,
            "Package command produced no artifact: {}",
            file.display()
        );
    }
    write_json(
        &release()?.join(format!("packages-{}-{}.json", platform()?, arch()?)),
        &json!({"version":version()?,"platform":platform()?,"arch":arch()?,"artifacts":names}),
    )?;
    for artifact in artifacts {
        println!("{}", artifact.display());
    }
    Ok(())
}

fn sha512(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hash = Sha512::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(base64::engine::general_purpose::STANDARD.encode(hash.finalize()))
}

/// The updater only accepts regular files and directories. Do not allow archive
/// writer defaults to turn links, devices or inherited hardlinks into payloads.
fn archive(runtime: &Path, output: &Path, zip: bool) -> Result<()> {
    let mut members = Vec::new();
    for entry in WalkDir::new(runtime)
        .follow_links(false)
        .sort_by_file_name()
        .min_depth(1)
    {
        let entry = entry?;
        ensure!(
            entry.file_type().is_file() || entry.file_type().is_dir(),
            "Unsupported archive member: {}",
            entry.path().display()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            ensure!(
                !entry.file_type().is_file() || entry.metadata()?.nlink() == 1,
                "Hardlinked archive member: {}",
                entry.path().display()
            );
        }
        members.push(entry.into_path());
    }
    let mut temporary =
        tempfile::NamedTempFile::new_in(output.parent().context("Archive path has no parent")?)?;
    if zip {
        let mut writer = zip::ZipWriter::new(temporary.as_file_mut());
        for member in &members {
            let relative = member
                .strip_prefix(runtime)?
                .to_str()
                .context("Non-UTF-8 archive member")?
                .replace('\\', "/");
            let metadata = fs::metadata(member)?;
            let mode = if metadata.is_dir() { 0o755 } else { 0o644 };
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated)
                .unix_permissions(mode);
            if metadata.is_dir() {
                writer.add_directory(format!("{relative}/"), options)?;
            } else {
                writer.start_file(relative, options)?;
                std::io::copy(&mut fs::File::open(member)?, &mut writer)?;
            }
        }
        writer.finish()?;
    } else {
        let encoder = GzEncoder::new(temporary.as_file_mut(), Compression::default());
        let mut writer = tar::Builder::new(encoder);
        writer.follow_symlinks(false);
        for member in &members {
            let relative = member.strip_prefix(runtime)?;
            if member.is_dir() {
                writer.append_dir(relative, member)?;
            } else {
                writer.append_file(relative, &mut fs::File::open(member)?)?;
            }
        }
        writer.into_inner()?.finish()?;
    }
    temporary.flush()?;
    temporary.persist(output).map_err(|error| error.error)?;
    Ok(())
}

pub fn daemon(args: &[String]) -> Result<()> {
    ensure!(
        args.is_empty(),
        "Usage: cargo xtask package-daemon (native release host required)"
    );
    let identity = build::release_identity()?;
    let stage = tempfile::Builder::new()
        .prefix("paseo-daemon-package-")
        .tempdir()?;
    let runtime = stage.path().join("runtime");
    build::bundle_runtime(&runtime, "release", "standalone-archive")?;
    match platform()? {
        "macos" => signing::sign_mac(&runtime, stage.path(), false)?,
        "windows" => {
            signing::windows_runtime(&runtime, true)?;
            signing::sign_windows(&[runtime.join(executable())], stage.path())?;
            build::refresh_runtime_hash(&runtime)?;
        }
        _ => {}
    }
    build::validate_runtime(&runtime, "standalone-archive", "release")?;
    let format = if platform()? == "windows" {
        "zip"
    } else {
        "tar.gz"
    };
    let name = format!(
        "Paseo-Daemon-{}-{}-{}.{format}",
        version()?,
        platform()?,
        arch()?
    );
    fs::create_dir_all(release()?)?;
    let output = release()?.join(&name);
    archive(&runtime, &output, format == "zip")?;
    let manifest = json!({"schemaVersion":1,"runtime":"rust","appId":"sh.paseo.daemon","channel":identity["channel"],"platform":platform()?,"arch":arch()?,"version":version()?,"sourceSha":identity["sourceSha"],"artifact":{"name":name,"format":format,"size":fs::metadata(&output)?.len(),"sha512":sha512(&output)?}});
    write_json(
        &release()?.join(format!(
            "daemon-{}-{}-{}.json",
            identity["channel"]
                .as_str()
                .context("Release channel missing")?,
            platform()?,
            arch()?
        )),
        &manifest,
    )?;
    println!("{}", output.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn archive_roundtrip_retains_payload_and_executable_mode() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = directory.path().join("runtime");
        text(&runtime.join("paseo"), "native fixture", 0o755).unwrap();
        text(&runtime.join("skills/example.txt"), "asset", 0o644).unwrap();
        let output = directory.path().join("runtime.tar.gz");
        archive(&runtime, &output, false).unwrap();
        let extracted = directory.path().join("extracted");
        tar::Archive::new(flate2::read::GzDecoder::new(
            fs::File::open(output).unwrap(),
        ))
        .unpack(&extracted)
        .unwrap();
        assert_eq!(
            fs::read_to_string(extracted.join("skills/example.txt")).unwrap(),
            "asset"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(extracted.join("paseo"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o755
            );
        }
        let zip = directory.path().join("runtime.zip");
        archive(&runtime, &zip, true).unwrap();
        let mut archive = zip::ZipArchive::new(fs::File::open(zip).unwrap()).unwrap();
        let mut payload = String::new();
        archive
            .by_name("skills/example.txt")
            .unwrap()
            .read_to_string(&mut payload)
            .unwrap();
        assert_eq!(payload, "asset");
    }
    #[test]
    #[cfg(unix)]
    fn archives_reject_symbolic_and_hard_links() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = directory.path().join("runtime");
        fs::create_dir(&runtime).unwrap();
        fs::write(runtime.join("file"), "payload").unwrap();
        std::os::unix::fs::symlink("file", runtime.join("link")).unwrap();
        assert!(archive(&runtime, &directory.path().join("bad.tar.gz"), false).is_err());
        fs::remove_file(runtime.join("link")).unwrap();
        fs::hard_link(runtime.join("file"), runtime.join("link")).unwrap();
        assert!(archive(&runtime, &directory.path().join("bad.zip"), true).is_err());
    }
}
