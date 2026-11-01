mod assets;
mod build;
mod common;
mod dev;
mod flutter_sdk;
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
    match command.as_str() {
        "build-daemon" => build::build_daemon(args),
        "build-flutter" => build::build_flutter(args),
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
        "flutter-sdk" => flutter_sdk::run(args),
        "dev" | "flutter" | "maintenance" => dev::run(command, args),
        "help" | "--help" | "-h" => help(),
        other => bail!("Unknown xtask command {other:?}; run cargo xtask --help"),
    }
}

fn help() -> Result<()> {
    println!(
        "Paseo native build and release tooling\n\n  build-daemon [--release]\n  build-relay [--release]\n  build-flutter [linux|macos|windows|android|ios] [--debug] [--run [--device ID]] [--arch universal|arm64]\n  package-desktop [linux|macos|windows]\n  package-daemon\n  stage-runtime DESTINATION [--profile debug|release] [--kind desktop-bundled|standalone-archive|container]\n  container-runtime DESTINATION\n  appimage-tools\n  release <plan|record|verify|stamp|publish|notes|version|fdroid|...>\n  dev ...\n  flutter <analyze|test|...>\n  maintenance <acp-catalog|codex-titles|...>"
    );
    println!("  relay-cloudflare <build|deploy>");
    println!("  flutter-sdk <build|publish|install>");
    Ok(())
}

fn main() {
    if let Err(error) = dispatch() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}
