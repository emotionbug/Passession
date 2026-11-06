//! Windows executable resources (icon, VERSIONINFO, PerMonitorV2 DPI manifest) from
//! `packaging/windows/paseo-sessions.rc`; nothing on other targets.

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=PASEO_VERSION");
    println!("cargo:rerun-if-env-changed=PASEO_BUILD_NUMBER");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"));
    let root = manifest_dir.join("../..");
    let windows = root.join("packaging/windows");
    let desktop = root.join("packaging/desktop");
    let rc = windows.join("paseo-sessions.rc");
    for file in [&rc, &windows.join("paseo-sessions.exe.manifest"), &desktop.join("icon.ico")] {
        println!("cargo:rerun-if-changed={}", file.display());
    }

    let version = std::env::var("PASEO_VERSION")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| std::env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION"));
    // Numeric core of "x.y.z[-pre]"; a malformed part reads as 0.
    let core = version.split(['-', '+']).next().unwrap_or("");
    let mut parts = core.split('.').map(|part| part.parse::<u32>().unwrap_or(0));
    let major = parts.next().unwrap_or(0);
    let minor = parts.next().unwrap_or(0);
    let patch = parts.next().unwrap_or(0);
    let build = std::env::var("PASEO_BUILD_NUMBER")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map_or(0, |v| v.min(65535));

    let macros = [
        format!("PASEO_VERSION={version}"),
        format!("PASEO_VERSION_MAJOR={major}"),
        format!("PASEO_VERSION_MINOR={minor}"),
        format!("PASEO_VERSION_PATCH={patch}"),
        format!("PASEO_VERSION_BUILD={build}"),
    ];
    let includes = [desktop, windows];
    embed_resource::compile(&rc, embed_resource::ParamsMacrosAndIncludeDirs(&macros, &includes))
        .manifest_required()
        .expect("compile packaging/windows/paseo-sessions.rc");
}
