//! Environment boundary for an externally provisioned desktop cross toolchain.
use crate::common::{BuildTarget, read_json};
use anyhow::{Context, Result, ensure};
use std::{env, path::PathBuf, process::Command};

pub fn apply(command: &mut Command, target: &BuildTarget) -> Result<()> {
    if !target.is_cross()? {
        return Ok(());
    }
    let root = PathBuf::from(env::var_os("PASEO_CROSS_ROOT").with_context(|| {
        format!("Cross-compiling {} requires PASEO_CROSS_ROOT pointing to a provisioned toolchain with cross.json", target.id())
    })?);
    ensure!(
        root.is_dir(),
        "PASEO_CROSS_ROOT is not a provisioned toolchain directory: {}",
        root.display()
    );
    let manifest = read_json(&root.join("cross.json"))
        .context("Reading provisioned cross toolchain manifest (PASEO_CROSS_ROOT/cross.json)")?;
    ensure!(
        manifest["schemaVersion"] == 1 && manifest["target"].as_str() == Some(target.id().as_str()),
        "Cross toolchain manifest schema or target does not match {}",
        target.id()
    );
    let environment = manifest["env"]
        .as_object()
        .context("Cross toolchain manifest needs an env map")?;
    for (name, value) in environment {
        ensure!(
            !name.is_empty() && !name.contains(['=', '\0']),
            "Invalid cross toolchain environment name"
        );
        let value = value
            .as_str()
            .context("Cross toolchain environment values must be strings")?;
        ensure!(
            !value.contains('\0'),
            "Invalid cross toolchain environment value"
        );
        command.env(name, value);
    }
    Ok(())
}
