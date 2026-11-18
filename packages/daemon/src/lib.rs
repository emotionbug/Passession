/// The independently released daemon component, not the collection or GUI version.
pub const VERSION: &str = match option_env!("PASEO_DAEMON_VERSION") {
    Some(version) => version,
    None => env!("CARGO_PKG_VERSION"),
};

pub fn component_identity() -> paseo_protocol::components::ComponentIdentity {
    paseo_protocol::components::ComponentIdentity {
        version: VERSION.into(),
        source_sha: option_env!("PASEO_BUILD_SOURCE_SHA").unwrap_or("").into(),
        input_hash: option_env!("PASEO_COMPONENT_INPUT_HASH")
            .unwrap_or("")
            .into(),
        abi_version: 1,
    }
}

pub mod activity;
pub mod agent;
mod allocator;
pub mod automation;
pub mod cli;
pub mod client;
pub mod config;
pub mod daemon_admin;
pub mod daemon_control;
pub mod directory_sync;
pub mod files;
pub mod git;
pub mod json;
pub mod logging;
pub mod mcp;
pub mod mcp_client;
pub mod plugins;
pub mod projects;
pub mod providers;
pub mod state;
pub mod terminals;
pub mod transport;
pub mod usage;
