//! "Open in" editor and file-manager targets (original desktop_integrations.dart).
//! Commands are argv vectors; no user text is ever interpreted by a shell.

use crate::{action::{EditorTarget, PlatformError}, platform::OpenEditorRequest};
use std::{
    collections::HashMap,
    ffi::OsString,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Style {
    Goto,
    Kiro,
    Zed,
    Studio,
    JetBrains,
}

struct Target {
    id: &'static str,
    label: &'static str,
    commands: &'static [&'static str],
    mac_application: Option<&'static str>,
    mac_launcher: Option<&'static str>,
    windows_directory: Option<&'static str>,
    windows_launcher: Option<&'static str>,
    style: Style,
}

const fn editor(id: &'static str, label: &'static str, commands: &'static [&'static str], style: Style) -> Target {
    Target { id, label, commands, mac_application: None, mac_launcher: None, windows_directory: None, windows_launcher: None, style }
}

const EDITORS: &[Target] = &[
    Target {
        mac_application: Some("Cursor"),
        mac_launcher: Some("Contents/Resources/app/bin/cursor"),
        windows_directory: Some("Cursor"),
        windows_launcher: Some("resources/app/bin/cursor.cmd"),
        ..editor("cursor", "Cursor", &["cursor"], Style::Goto)
    },
    editor("trae", "Trae", &["trae"], Style::Goto),
    editor("kiro", "Kiro", &["kiro"], Style::Kiro),
    Target {
        mac_application: Some("Visual Studio Code"),
        mac_launcher: Some("Contents/Resources/app/bin/code"),
        windows_directory: Some("Microsoft VS Code"),
        windows_launcher: Some("bin/code.cmd"),
        ..editor("vscode", "VS Code", &["code"], Style::Goto)
    },
    Target {
        mac_application: Some("Visual Studio Code - Insiders"),
        mac_launcher: Some("Contents/Resources/app/bin/code"),
        windows_directory: Some("Microsoft VS Code Insiders"),
        windows_launcher: Some("bin/code-insiders.cmd"),
        ..editor("vscode-insiders", "VS Code Insiders", &["code-insiders"], Style::Goto)
    },
    Target {
        mac_application: Some("VSCodium"),
        mac_launcher: Some("Contents/Resources/app/bin/codium"),
        windows_directory: Some("VSCodium"),
        windows_launcher: Some("bin/codium.cmd"),
        ..editor("vscodium", "VSCodium", &["codium"], Style::Goto)
    },
    Target { mac_application: Some("Zed"), ..editor("zed", "Zed", &["zed", "zeditor"], Style::Zed) },
    editor("antigravity", "Antigravity", &["agy", "antigravity"], Style::Goto),
    Target {
        mac_application: Some("Android Studio"),
        mac_launcher: Some("Contents/MacOS/studio"),
        ..editor("android-studio", "Android Studio", &["studio", "studio64"], Style::Studio)
    },
    editor("intellij-idea", "IntelliJ IDEA", &["idea", "idea64"], Style::JetBrains),
    editor("aqua", "Aqua", &["aqua", "aqua64"], Style::JetBrains),
    editor("clion", "CLion", &["clion", "clion64"], Style::JetBrains),
    editor("datagrip", "DataGrip", &["datagrip", "datagrip64"], Style::JetBrains),
    editor("dataspell", "DataSpell", &["dataspell", "dataspell64"], Style::JetBrains),
    editor("goland", "GoLand", &["goland", "goland64"], Style::JetBrains),
    editor("phpstorm", "PhpStorm", &["phpstorm", "phpstorm64"], Style::JetBrains),
    editor("pycharm", "PyCharm", &["pycharm", "pycharm64"], Style::JetBrains),
    editor("rider", "Rider", &["rider", "rider64"], Style::JetBrains),
    editor("rubymine", "RubyMine", &["rubymine", "rubymine64"], Style::JetBrains),
    editor("rustrover", "RustRover", &["rustrover", "rustrover64"], Style::JetBrains),
    editor("webstorm", "WebStorm", &["webstorm", "webstorm64"], Style::JetBrains),
];

fn file_manager() -> (&'static str, &'static str) {
    if cfg!(target_os = "macos") {
        ("finder", "Finder")
    } else if cfg!(windows) {
        ("explorer", "Explorer")
    } else {
        ("file-manager", "File Manager")
    }
}

fn mac_applications(target: &Target) -> Vec<PathBuf> {
    let Some(app) = target.mac_application else { return Vec::new() };
    let mut paths = vec![PathBuf::from(format!("/Applications/{app}.app"))];
    if let Some(home) = std::env::var_os("HOME") {
        paths.push(Path::new(&home).join("Applications").join(format!("{app}.app")));
    }
    paths.push(PathBuf::from(format!("/System/Applications/{app}.app")));
    paths
}

fn is_executable(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                meta.permissions().mode() & 0o111 != 0
            }
            #[cfg(not(unix))]
            true
        }
        _ => false,
    }
}

fn command(target: &Target) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = target.commands.iter().map(PathBuf::from).collect();
    if cfg!(target_os = "macos")
        && let Some(launcher) = target.mac_launcher {
            candidates.extend(mac_applications(target).into_iter().map(|app| app.join(launcher)));
        }
    if cfg!(windows)
        && let (Some(directory), Some(launcher)) = (target.windows_directory, target.windows_launcher) {
            let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
            let programs = std::env::var_os("ProgramFiles").map(PathBuf::from);
            if let Some(local) = &local {
                let folder = if target.id == "cursor" { "cursor" } else { directory };
                candidates.push(local.join("Programs").join(folder).join(launcher));
            }
            if let Some(programs) = &programs {
                candidates.push(programs.join(directory).join(launcher));
            }
            if target.id == "cursor" {
                if let Some(local) = &local {
                    candidates.push(local.join("Programs").join("cursor").join("Cursor.exe"));
                }
                if let Some(programs) = &programs {
                    candidates.push(programs.join("Cursor").join("Cursor.exe"));
                }
            }
        }
    let path_var = std::env::var_os("PATH").or_else(|| std::env::var_os("Path")).unwrap_or_default();
    let directories: Vec<PathBuf> = std::env::split_paths(&path_var).filter(|p| !p.as_os_str().is_empty()).collect();
    for candidate in candidates {
        let paths: Vec<PathBuf> = if candidate.is_absolute() {
            vec![candidate]
        } else {
            directories.iter().map(|dir| dir.join(&candidate)).collect()
        };
        for path in paths {
            let extensions: &[&str] = if cfg!(windows) && path.extension().is_none() {
                &[".exe", ".cmd", ".bat", ".com", ""]
            } else {
                &[""]
            };
            for extension in extensions {
                let mut name = path.clone().into_os_string();
                name.push(extension);
                let executable = PathBuf::from(name);
                if is_executable(&executable) {
                    return Some(executable);
                }
            }
        }
    }
    None
}

fn has_mac_app(target: &Target) -> bool {
    cfg!(target_os = "macos")
        && target.mac_application.is_some()
        && target.style != Style::Studio
        && mac_applications(target).iter().any(|app| app.is_dir())
}

/// Blocking: probes PATH and install locations.
pub(crate) fn list_editor_targets() -> Vec<EditorTarget> {
    let mut installed: Vec<EditorTarget> = EDITORS
        .iter()
        .filter(|target| command(target).is_some() || has_mac_app(target))
        .map(|target| EditorTarget { id: target.id.into(), label: target.label.into(), kind: "editor".into() })
        .collect();
    let (id, label) = file_manager();
    installed.push(EditorTarget { id: id.into(), label: label.into(), kind: "file-manager".into() });
    installed
}

/// Environment for launched tools, without this app's private daemon/Electron markers.
pub(crate) fn external_environment() -> HashMap<OsString, OsString> {
    let mut environment: HashMap<OsString, OsString> = std::env::vars_os().collect();
    for key in ["PASEO_NODE_ENV", "PASEO_DESKTOP_MANAGED", "PASEO_SUPERVISED", "ELECTRON_RUN_AS_NODE", "ELECTRON_NO_ATTACH_CONSOLE"] {
        environment.remove(&OsString::from(key));
    }
    environment
}

/// Runs a tool to completion, failing on a nonzero exit.
pub(crate) fn run(command: &str, args: &[OsString]) -> Result<(), PlatformError> {
    let output = std::process::Command::new(command)
        .args(args)
        .env_clear()
        .envs(external_environment())
        .output()
        .map_err(|e| PlatformError::native(format!("{command}: {e}")))?;
    if !output.status.success() {
        return Err(PlatformError::native(format!(
            "{command} failed ({}): {}",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

/// Starts a detached GUI program.
pub(crate) fn launch(command: &Path, args: &[OsString]) -> Result<(), PlatformError> {
    // Only Windows' cmd.exe path adds the PASEO_EDITOR_ARG_<n> variables.
    #[cfg(windows)]
    let mut environment = external_environment();
    #[cfg(not(windows))]
    let environment = external_environment();
    #[cfg(windows)]
    {
        let extension = command.extension().map(|e| e.to_string_lossy().to_ascii_lowercase());
        if matches!(extension.as_deref(), Some("cmd" | "bat")) {
            // Variables are expanded once by cmd. No path is interpolated into shell
            // syntax and delayed expansion is disabled, so ! and % stay literal.
            let mut values = vec![command.as_os_str().to_owned()];
            values.extend(args.iter().cloned());
            for (index, value) in values.iter().enumerate() {
                let text = value.to_string_lossy();
                if text.contains('"') || text.contains('\n') || text.contains('\r') {
                    return Err(PlatformError::invalid("Windows command arguments must not contain quotes or newlines"));
                }
                environment.insert(format!("PASEO_EDITOR_ARG_{index}").into(), value.clone());
            }
            return super::windows::launch_command_script(&environment, values.len());
        }
    }
    let mut child = std::process::Command::new(command);
    child
        .args(args)
        .env_clear()
        .envs(&environment)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Detach from this process group so the editor outlives the app.
        child.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        child.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    let mut spawned = child.spawn().map_err(|e| PlatformError::native(format!("{}: {e}", command.display())))?;
    // Reap in the background; the editor's lifetime is its own.
    std::thread::spawn(move || {
        let _ = spawned.wait();
    });
    Ok(())
}

/// Blocking.
pub(crate) fn open_editor(request: &OpenEditorRequest) -> Result<(), PlatformError> {
    for path in std::iter::once(&request.workspace_path).chain(request.file_path.as_ref()) {
        if !path.is_absolute() {
            return Err(PlatformError::invalid(format!("Editor paths must be absolute local paths: {}", path.display())));
        }
        if !path.exists() {
            return Err(PlatformError::new("io", format!("Path does not exist: {}", path.display())));
        }
    }
    if request.line == Some(0) {
        return Err(PlatformError::invalid("line must be at least 1"));
    }
    if request.column == Some(0) {
        return Err(PlatformError::invalid("column must be at least 1"));
    }
    if request.editor_id == file_manager().0 {
        return super::native::reveal_blocking(&request.workspace_path, request.file_path.as_deref());
    }
    let target = EDITORS
        .iter()
        .find(|t| t.id == request.editor_id)
        .ok_or_else(|| PlatformError::invalid(format!("Unknown editor target: {}", request.editor_id)))?;
    let Some(executable) = command(target) else {
        if has_mac_app(target) {
            if request.line.is_some() || request.column.is_some() {
                return Err(PlatformError::native(format!(
                    "{} is installed but its command-line launcher is required for opening a position",
                    target.label
                )));
            }
            let mut args: Vec<OsString> = vec!["-a".into(), target.mac_application.unwrap_or_default().into(), request.workspace_path.clone().into()];
            if let Some(file) = &request.file_path {
                args.push(file.clone().into());
            }
            return run("/usr/bin/open", &args);
        }
        return Err(PlatformError::native(format!("Editor target unavailable: {}", target.label)));
    };
    let location = request.file_path.as_ref().map(|file| {
        let mut location = file.clone().into_os_string();
        if let Some(line) = request.line {
            location.push(format!(":{line}"));
            if let Some(column) = request.column {
                location.push(format!(":{column}"));
            }
        }
        location
    });
    let mut args: Vec<OsString> = Vec::new();
    if target.style == Style::Kiro {
        args.push("ide".into());
    }
    if target.style == Style::JetBrains && request.file_path.is_some() {
        if let Some(line) = request.line {
            args.extend(["--line".into(), line.to_string().into()]);
        }
        if let Some(column) = request.column {
            args.extend(["--column".into(), column.to_string().into()]);
        }
    }
    args.push(request.workspace_path.clone().into());
    if let Some(file) = &request.file_path {
        match target.style {
            Style::Studio => {
                if let Some(line) = request.line {
                    args.extend(["--line".into(), line.to_string().into()]);
                }
                if let Some(column) = request.column {
                    args.extend(["--column".into(), column.to_string().into()]);
                }
                args.push(file.clone().into());
            }
            Style::JetBrains => args.push(file.clone().into()),
            Style::Zed => args.push(location.clone().unwrap()),
            Style::Goto | Style::Kiro => {
                if request.line.is_some() {
                    args.push("--goto".into());
                }
                args.push(location.clone().unwrap());
            }
        }
    }
    launch(&executable, &args)
}
