//! Node-free Cloudflare build/upload. Upload updates the existing Worker only;
//! it neither creates routes nor changes domains, TLS, DNS or DO migrations.
use std::{collections::BTreeMap, fs, path::Path, process::Command, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use reqwest::blocking::{Client, Response, multipart};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::common::{root, sha256_file};

const BINDGEN_VERSION: &str = "0.2.108";
const OUTPUT: &str = "artifacts/relay-cloudflare";
const MODULES: [(&str, &str); 3] = [
    ("worker.mjs", "application/javascript+module"),
    ("paseo_relay_cloudflare.js", "application/javascript+module"),
    ("paseo_relay_cloudflare_bg.wasm", "application/wasm"),
];
// Generated platform ABI glue only. Request routing and environment handling are
// implemented in the Rust WASM module; the JS file never inspects traffic.
const LOADER: &str = r#"import wasm from './paseo_relay_cloudflare_bg.wasm';
import { initSync, proxy_fetch } from './paseo_relay_cloudflare.js';
export { RelayDurableObject } from './paseo_relay_cloudflare.js';
initSync({ module: wasm });
export default { fetch(request, env) { return proxy_fetch(request, env); } };
"#;

#[derive(Deserialize, Serialize)]
struct Deployment {
    account_id: String,
    script_name: String,
    hostname: String,
    upstream: String,
    compatibility_date: String,
}

pub fn run(args: &[String]) -> Result<()> {
    match args {
        [command] if command == "build" => build(),
        [command, confirm] if command == "deploy" && confirm == "--confirm-upload" => deploy(),
        _ => bail!(
            "usage: cargo xtask relay-cloudflare build | deploy --confirm-upload; build requires wasm32-unknown-unknown and wasm-bindgen-cli {BINDGEN_VERSION}; deploy requires CLOUDFLARE_API_TOKEN and explicit deployment approval"
        ),
    }
}

fn build() -> Result<()> {
    let root = root()?;
    let version = Command::new("wasm-bindgen")
        .arg("--version")
        .output()
        .context(
            "install pinned tooling with cargo install wasm-bindgen-cli --version 0.2.108 --locked",
        )?;
    ensure!(
        version.status.success()
            && String::from_utf8_lossy(&version.stdout).trim()
                == format!("wasm-bindgen {BINDGEN_VERSION}"),
        "wasm-bindgen-cli must be exactly {BINDGEN_VERSION}"
    );
    let target = root.join("target/relay-cloudflare");
    let status = Command::new("cargo")
        .current_dir(&root)
        .args([
            "build",
            "--locked",
            "--release",
            "--target",
            "wasm32-unknown-unknown",
            "--package",
            "paseo-relay-cloudflare",
        ])
        .env("CARGO_TARGET_DIR", &target)
        .status()?;
    ensure!(
        status.success(),
        "relay WASM compilation failed; install target with rustup target add wasm32-unknown-unknown"
    );
    let staging = tempfile::tempdir()?;
    let status = Command::new("wasm-bindgen")
        .arg(target.join("wasm32-unknown-unknown/release/paseo_relay_cloudflare.wasm"))
        .args([
            "--target",
            "web",
            "--no-typescript",
            "--out-name",
            "paseo_relay_cloudflare",
            "--out-dir",
        ])
        .arg(staging.path())
        .status()?;
    ensure!(status.success(), "wasm-bindgen generation failed");
    fs::write(staging.path().join("worker.mjs"), LOADER)?;
    let manifest = artifact_manifest(staging.path())?;
    let output = root.join(OUTPUT);
    fs::create_dir_all(&output)?;
    for (name, _) in MODULES {
        fs::copy(staging.path().join(name), output.join(name))?;
    }
    fs::write(
        output.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    println!("Built Cloudflare relay modules in {OUTPUT}; no upload performed");
    Ok(())
}

fn artifact_manifest(directory: &Path) -> Result<BTreeMap<String, String>> {
    MODULES
        .into_iter()
        .map(|(name, _)| Ok((name.to_owned(), sha256_file(&directory.join(name))?)))
        .collect()
}

fn metadata(settings: &Value, config: &Deployment) -> Result<Value> {
    let bindings = settings["bindings"]
        .as_array()
        .context("Worker settings missing bindings")?;
    let upstream = bindings
        .iter()
        .find(|binding| binding["name"] == "PASEO_RELAY_UPSTREAM")
        .context(
            "existing Worker has no upstream binding; refusing an unreviewed architecture change",
        )?;
    ensure!(
        upstream["type"] == "plain_text" && upstream["text"] == config.upstream,
        "existing upstream differs from deployment configuration; refusing change"
    );
    let mut preserved = Vec::new();
    let mut keep_types = Vec::new();
    for binding in bindings {
        if binding["type"] == "secret_text" {
            if keep_types.is_empty() {
                keep_types.push("secret_text");
            }
        } else {
            preserved.push(binding.clone());
        }
    }
    let mut result = json!({
        "main_module": "worker.mjs",
        "compatibility_date": config.compatibility_date,
        "bindings": preserved,
        "keep_bindings": keep_types,
    });
    // Preserve deployed runtime/observability settings; no migration field means
    // no namespace deletion, rename or recreation during this code-only upload.
    for name in [
        "compatibility_flags",
        "observability",
        "logpush",
        "tail_consumers",
        "placement",
        "limits",
        "tags",
    ] {
        if let Some(value) = settings.get(name) {
            result[name] = value.clone();
        }
    }
    Ok(result)
}

fn api_result(response: Response) -> Result<Value> {
    let status = response.status();
    let envelope: Value = response
        .json()
        .context("Cloudflare returned a non-JSON response")?;
    // API errors can echo request values; avoid printing response bodies/bindings.
    ensure!(
        status.is_success() && envelope["success"] == true,
        "Cloudflare API request failed (HTTP {status}); inspect the account dashboard for details"
    );
    envelope
        .get("result")
        .cloned()
        .context("Cloudflare response has no result")
}

fn deploy() -> Result<()> {
    let root = root()?;
    let config: Deployment = serde_json::from_slice(&fs::read(
        root.join("packages/relay-cloudflare/deployment.json"),
    )?)?;
    ensure!(
        config.account_id.len() == 32 && config.account_id.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid account id"
    );
    ensure!(
        !config.script_name.is_empty()
            && config
                .script_name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-'),
        "invalid script name"
    );
    for (name, expected) in [
        ("CLOUDFLARE_ACCOUNT_ID", &config.account_id),
        ("PASEO_RELAY_UPSTREAM", &config.upstream),
    ] {
        if let Ok(value) = std::env::var(name) {
            ensure!(
                value == *expected,
                "{name} differs from reviewed deployment.json"
            );
        }
    }
    let token =
        std::env::var("CLOUDFLARE_API_TOKEN").context("CLOUDFLARE_API_TOKEN is required")?;
    ensure!(!token.is_empty(), "CLOUDFLARE_API_TOKEN is empty");
    let output = root.join(OUTPUT);
    let expected: BTreeMap<String, String> = serde_json::from_slice(
        &fs::read(output.join("manifest.json"))
            .context("run cargo xtask relay-cloudflare build before uploading")?,
    )?;
    ensure!(
        artifact_manifest(&output)? == expected,
        "Worker artifacts changed after build; rebuild before upload"
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(120))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let account = format!(
        "https://api.cloudflare.com/client/v4/accounts/{}",
        config.account_id
    );
    let script = format!("{account}/workers/scripts/{}", config.script_name);
    // A code upload must not silently create a new script or claim an unbound
    // hostname. Domain association is read only and survives updating the script.
    let domains = api_result(
        client
            .get(format!("{account}/workers/domains"))
            .bearer_auth(&token)
            .send()?,
    )?;
    ensure!(
        domains.as_array().is_some_and(|domains| domains
            .iter()
            .any(|domain| domain["hostname"] == config.hostname
                && domain["service"] == config.script_name
                && domain["environment"] == "production")),
        "reviewed custom domain is not attached to the existing production Worker; no changes made"
    );
    let settings = api_result(
        client
            .get(format!("{script}/settings"))
            .bearer_auth(&token)
            .send()?,
    )?;
    let metadata = metadata(&settings, &config)?;
    let mut form = multipart::Form::new().part(
        "metadata",
        multipart::Part::text(serde_json::to_string(&metadata)?).mime_str("application/json")?,
    );
    for (name, mime) in MODULES {
        form = form.part(
            name,
            multipart::Part::bytes(fs::read(output.join(name))?)
                .file_name(name)
                .mime_str(mime)?,
        );
    }
    api_result(
        client
            .put(script)
            .bearer_auth(&token)
            .multipart(form)
            .send()?,
    )?;
    println!(
        "Uploaded {} preserving {} and upstream {}; DNS, TLS and Durable Object migrations unchanged",
        config.script_name, config.hostname, config.upstream
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Deployment {
        Deployment {
            account_id: "a".repeat(32),
            script_name: "paseo-relay".into(),
            hostname: "relay.paseo.sh".into(),
            upstream: "https://paseo-relay-next.fly.dev".into(),
            compatibility_date: "2024-12-01".into(),
        }
    }

    #[test]
    fn upload_preserves_namespace_secrets_and_runtime_settings() {
        let settings = json!({"bindings": [
            {"name":"PASEO_RELAY_UPSTREAM", "type":"plain_text", "text":"https://paseo-relay-next.fly.dev"},
            {"name":"RELAY", "type":"durable_object_namespace", "class_name":"RelayDurableObject", "namespace_id":"existing"},
            {"name":"OTHER_SECRET", "type":"secret_text"}
        ], "compatibility_flags":["streams_enable_constructors"], "observability":{"enabled":true}});
        let metadata = metadata(&settings, &config()).unwrap();
        assert_eq!(metadata["bindings"].as_array().unwrap().len(), 2);
        assert_eq!(metadata["bindings"][1], settings["bindings"][1]);
        assert_eq!(metadata["keep_bindings"], json!(["secret_text"]));
        assert_eq!(metadata["observability"], settings["observability"]);
        assert_eq!(
            metadata["compatibility_flags"],
            settings["compatibility_flags"]
        );
        assert!(metadata.get("migrations").is_none());
    }

    #[test]
    fn refuses_missing_or_changed_upstream_before_upload() {
        assert!(metadata(&json!({"bindings":[]}), &config()).is_err());
        assert!(metadata(&json!({"bindings":[{"name":"PASEO_RELAY_UPSTREAM","type":"plain_text","text":"https://elsewhere.example"}]}), &config()).is_err());
    }
}
