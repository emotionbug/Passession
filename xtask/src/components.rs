//! Independent raw compilation, plan-bound handoff and native release finalization.
mod build;
pub(crate) mod plan;

use anyhow::{Context, Result, bail, ensure};
use paseo_protocol::components::{ComponentIdentity, validate_identity};

pub(crate) fn run(args: &[String]) -> Result<()> {
    let (command, args) = args
        .split_first()
        .context("Usage: cargo xtask components <plan|ci-plan|build>")?;
    match command.as_str() {
        "plan" | "ci-plan" => plan::dispatch(command, args),
        "build" => build::run(args),
        "compile" => build::compile(args),
        _ => bail!("Unknown components command: {command}"),
    }
}

/// All three stamps are either absent (developer build) or one validated identity.
pub(crate) fn compiler_stamps() -> Result<Option<ComponentIdentity>> {
    let version = std::env::var("PASEO_COMPONENT_VERSION").ok();
    let source = std::env::var("PASEO_COMPONENT_SOURCE_SHA").ok();
    let input = std::env::var("PASEO_COMPONENT_INPUT_HASH").ok();
    if version.is_none() && source.is_none() && input.is_none() {
        return Ok(None);
    }
    let identity = ComponentIdentity {
        version: version.context("Missing component version")?,
        source_sha: source.context("Missing component source SHA")?,
        input_hash: input.context("Missing component input hash")?,
        abi_version: std::env::var("PASEO_COMPONENT_ABI")
            .context("Missing component ABI")?
            .parse()?,
    };
    validate_identity(&identity)?;
    ensure!(
        identity.source_sha == std::env::var("PASEO_BUILD_SOURCE_SHA")?,
        "Compiler source stamps disagree"
    );
    Ok(Some(identity))
}

pub(crate) fn go_stamps(identity: &Option<ComponentIdentity>) -> String {
    match identity {
        Some(identity) => format!(
            "-X main.componentVersion={} -X main.componentSourceSHA={} -X main.componentInputHash={}",
            identity.version, identity.source_sha, identity.input_hash
        ),
        None => String::new(),
    }
}
