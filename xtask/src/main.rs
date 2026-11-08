mod assets;
mod build;
mod build_cache;
mod common;
mod dev;
mod gui;
mod libraries;
mod notices;
mod package;
mod relay_cloudflare;
mod release;
mod signing;

use anyhow::{Result, bail};

fn dispatch() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((command, args)) = args.split_first() else {
        return help();
    };
    let _build_lease = if matches!(
        command.as_str(),
        "build-daemon" | "build-relay" | "stage-runtime"
            | "container-runtime" | "package-desktop" | "package-daemon" | "relay-cloudflare"
    ) {
        Some(build_cache::acquire()?)
    } else {
        None
    };
    match command.as_str() {
        "build-daemon" => build::build_daemon(args),
        "build-gui" => gui::build_gui(args),
        "build-relay" => {
            if args.iter().any(|arg| arg != "--release") || args.len() > 1 {
                bail!("Usage: cargo xtask build-relay [--release]");
            }
            common::run(
                common::command(common::cargo())?
                    .args(["build", "--locked", "-p", "paseo-relay"])
                    .args(args),
            )
        }
        "stage-runtime" => build::stage_command(args),
        "container-runtime" => build::container_runtime(args),
        "package-desktop" => package::desktop(args),
        "package-daemon" => package::daemon(args),
        "appimage-tools" => {
            if !args.is_empty() {
                bail!("Usage: cargo xtask appimage-tools");
            }
            assets::appimage_tools()
        }
        "release" => release::run(args),
        "relay-cloudflare" => relay_cloudflare::run(args),
        "housekeeping" => build_cache::housekeeping(args),
        "dev" | "maintenance" => dev::run(command, args),
        "help" | "--help" | "-h" => help(),
        other => bail!("Unknown xtask command {other:?}; run cargo xtask --help"),
    }
}

fn help() -> Result<()> {
    println!(
        "Paseo native build and release tooling\n\n  build-daemon [--release | --profile dev-fast|debug|release]\n  build-relay [--release]\n  build-gui [linux|macos|windows|android|ios] [--profile dev-fast|debug|release] [--debug] [--run [--device ID]] [--arch arm64|x64|universal] [--format apk|aab]\n  build-gui [linux|macos|windows] --smoke [--profile dev-fast|debug|release]\n  package-desktop [linux|macos|windows]\n  package-daemon\n  stage-runtime DESTINATION [--profile dev-fast|debug|release] [--kind desktop-bundled|standalone-archive|standalone-local|container]\n      standalone-local packages the existing local build without recompiling\n  container-runtime DESTINATION\n  appimage-tools\n  housekeeping [--apply] [--staging] [--max-bytes N]\n      dry-run by default; never deletes current, running or recently used build generations\n  release <plan|record|verify|stamp|publish|notes|version|fdroid|...>\n  dev <daemon|app|cli|home> …\n  maintenance <acp-catalog|codex-titles|...>"
    );
    println!("  relay-cloudflare <build|deploy>");
    Ok(())
}

fn main() {
    if let Err(error) = dispatch() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}
