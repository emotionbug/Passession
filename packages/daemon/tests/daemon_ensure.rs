#![cfg(target_os = "linux")]

use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

const PASEO: &str = env!("CARGO_BIN_EXE_paseo");

// The inherited PATH contains only this fixture. No command may contact the
// real user manager or write to the real HOME, XDG config, or daemon data.
const SYSTEMCTL: &str = r#"#!/bin/sh
set -eu
[ "$1" = --user ]
shift
printf '%s\n' "$*" >> "$FIXTURE/calls"
[ "$FIXTURE_SYSTEMD" = yes ] || exit 1
units="$XDG_CONFIG_HOME/systemd/user"
case "$1" in
    show-environment|daemon-reload) ;;
    list-unit-files)
        for path in "$units"/*.service; do
            if [ -f "$path" ]; then printf '%s disabled\n' "${path##*/}"; fi
        done ;;
    show)
        for unit; do
            case "$unit" in *.service) break ;; esac
        done
        path="$units/$unit"
        [ -f "$path" ] || exit 1
        if [ "$unit" = custom-paseo.service ] && [ ! -f "$units/$unit.d/zz-paseo-managed.conf" ]; then
            entry="$FIXTURE/runtime/paseo"
            args="daemon run --home $FIXTURE_HOME"
        else
            entry=$(/usr/bin/find "$PASEO_INSTALLATIONS_DIR" -path '*/bin/paseo' -type f)
            args="__launcher --home $FIXTURE_HOME"
        fi
        printf 'Id=%s\nType=exec\nLoadState=loaded\nFragmentPath=%s\nExecStart={ path=%s ; argv[]=%s %s ; }\nNeedDaemonReload=no\n' "$unit" "$path" "$entry" "$entry" "$args" ;;
    is-enabled) [ -f "$FIXTURE/enabled-$2" ] ;;
    enable) : > "$FIXTURE/enabled-$2" ;;
    start)
        [ "$2" = --no-block ]
        entry=$(/usr/bin/find "$PASEO_INSTALLATIONS_DIR" -path '*/bin/paseo' -type f)
        "$entry" __launcher --home "$FIXTURE_HOME" </dev/null >"$FIXTURE/service-log" 2>&1 & ;;
    *) exit 2 ;;
esac
"#;

struct Fixture {
    root: tempfile::TempDir,
    home: PathBuf,
    runtime: PathBuf,
    systemd: bool,
}

impl Fixture {
    fn new(systemd: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("daemon home");
        let runtime = root.path().join("runtime");
        fs::create_dir(&home).unwrap();
        fs::create_dir(&runtime).unwrap();
        fs::write(
            runtime.join("paseo-runtime.json"),
            b"{\"installationKind\":\"desktop-bundled\"}",
        )
        .unwrap();
        fs::write(runtime.join("complete-runtime-asset"), b"independent copy").unwrap();
        let image = runtime.join("paseo");
        if fs::hard_link(PASEO, &image).is_err() {
            fs::copy(PASEO, &image).unwrap();
        }
        fs::write(
            home.join("config.json"),
            serde_json::to_vec(&json!({"daemon":{
                "listen":"127.0.0.1:0", "relay":{"enabled":false}, "mcp":{"enabled":false},
                "serviceProxy":{"enabled":false}
            }}))
            .unwrap(),
        )
        .unwrap();
        let bin = root.path().join("bin");
        fs::create_dir(&bin).unwrap();
        fs::write(bin.join("systemctl"), SYSTEMCTL).unwrap();
        fs::set_permissions(bin.join("systemctl"), fs::Permissions::from_mode(0o700)).unwrap();
        // Agent creation checks Git context even for the development provider.
        // Allow that one executable without exposing the real systemctl.
        std::os::unix::fs::symlink(which::which("git").unwrap(), bin.join("git")).unwrap();
        Self {
            root,
            home,
            runtime,
            systemd,
        }
    }

    fn cli(&self) -> Command {
        let mut command = Command::new(PASEO);
        command
            .env_clear()
            .env("HOME", self.root.path())
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("PATH", self.root.path().join("bin"))
            .env("FIXTURE", self.root.path())
            .env("FIXTURE_HOME", &self.home)
            .env("FIXTURE_SYSTEMD", if self.systemd { "yes" } else { "no" })
            .env(
                "PASEO_INSTALLATIONS_DIR",
                self.root.path().join("installations"),
            )
            .current_dir(self.root.path());
        command
    }

    fn command(&self, action: &str) -> Command {
        let mut command = self.cli();
        command
            .args(["daemon", action, "--json", "--home"])
            .arg(&self.home);
        command
    }

    fn ensure(&self, bootstrap: bool) -> Command {
        let mut command = self.command("ensure");
        if bootstrap {
            command.arg("--runtime").arg(&self.runtime);
        }
        command
    }

    fn selection(&self) -> Value {
        let pointer = read_json(&self.home.join("native-installation.json"));
        read_json(&Path::new(pointer["root"].as_str().unwrap()).join("installation.json"))
    }

    fn units(&self) -> PathBuf {
        self.root.path().join("config/systemd/user")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self
            .command("stop")
            .args(["--timeout", "3", "--force"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn success(output: Output) -> Value {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("CLI output must be JSON")
}

#[test]
fn running_unmanaged_daemon_is_not_adopted_or_changed() {
    let fixture = Fixture::new(true);
    let started = success(fixture.command("start").output().unwrap());
    let previous_credential = fs::read(fixture.home.join("local-credential")).unwrap();
    let seen = success(fixture.ensure(false).output().unwrap());
    assert_eq!(seen["state"], "running");
    assert_eq!(seen["pid"], started["pid"]);
    assert_eq!(
        fs::read(fixture.home.join("local-credential")).unwrap(),
        previous_credential
    );
    for path in [
        fixture.home.join("native-installation.json"),
        fixture.root.path().join("calls"),
    ] {
        assert!(!path.exists(), "unexpected {}", path.display());
    }
}

#[test]
fn concurrent_first_ensures_share_one_detached_installation_and_restart_without_bundle() {
    let fixture = Fixture::new(false);
    let spawn = || {
        fixture
            .ensure(true)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let first = spawn();
    let second = spawn();
    let a = success(first.wait_with_output().unwrap());
    let b = success(second.wait_with_output().unwrap());
    assert_ne!(a["state"], b["state"]);
    assert!(matches!(
        (a["state"].as_str(), b["state"].as_str()),
        (Some("installed"), Some("running")) | (Some("running"), Some("installed"))
    ));
    assert_eq!(a["pid"], b["pid"]);
    assert_eq!(a["owner"], "detached");
    assert_eq!(b["owner"], "detached");
    let status = success(fixture.command("status").output().unwrap());
    assert_eq!(status["pid"], a["pid"]);
    assert_eq!(status["owner"], "detached");
    let selection = fixture.selection();
    let runtime = Path::new(selection["active"]["root"].as_str().unwrap());
    assert_eq!(
        fs::read(runtime.join("complete-runtime-asset")).unwrap(),
        b"independent copy"
    );
    let runtimes = runtime.parent().unwrap();
    assert_eq!(fs::read_dir(runtimes).unwrap().count(), 1);
    fs::remove_dir_all(&fixture.runtime).unwrap();
    success(fixture.command("stop").output().unwrap());
    let restarted = success(fixture.ensure(false).output().unwrap());
    assert_eq!(restarted["state"], "started");
    assert_eq!(restarted["owner"], "detached");
    assert_ne!(restarted["pid"], a["pid"]);
    assert_eq!(fixture.selection(), selection);
}

#[test]
fn fresh_systemd_install_enables_the_unit_and_restarts_through_it() {
    let fixture = Fixture::new(true);
    let started = success(fixture.ensure(true).output().unwrap());
    assert_eq!(started["state"], "installed");
    assert_eq!(started["owner"], "systemd-user");
    let unit = fixture.units().join("paseo.service");
    let content = fs::read_to_string(&unit).unwrap();
    let selection = fixture.selection();
    assert!(content.contains(selection["control"]["path"].as_str().unwrap()));
    assert!(content.contains("__launcher --home"));
    assert!(fixture.root.path().join("enabled-paseo.service").exists());
    success(fixture.command("stop").output().unwrap());
    let restarted = success(fixture.ensure(false).output().unwrap());
    assert_eq!(restarted["state"], "started");
    assert_eq!(restarted["owner"], "systemd-user");
    let calls = fs::read_to_string(fixture.root.path().join("calls")).unwrap();
    assert_eq!(
        calls
            .lines()
            .filter(|s| *s == "enable paseo.service")
            .count(),
        1
    );
    assert_eq!(
        calls
            .lines()
            .filter(|s| *s == "start --no-block paseo.service")
            .count(),
        2
    );
    assert_eq!(fs::read_to_string(unit).unwrap(), content);
}

#[test]
fn existing_systemd_unit_for_home_is_reused_without_overwriting_original() {
    let fixture = Fixture::new(true);
    fs::create_dir_all(fixture.units()).unwrap();
    let unit = fixture.units().join("custom-paseo.service");
    let original = format!(
        "[Service]\nExecStart={} daemon run --home \"{}\"\n",
        fixture.runtime.join("paseo").display(),
        fixture.home.display()
    );
    fs::write(&unit, &original).unwrap();
    let started = success(fixture.ensure(true).output().unwrap());
    assert_eq!(started["owner"], "systemd-user");
    assert_eq!(
        fixture.selection()["registration"]["unit"],
        "custom-paseo.service"
    );
    assert_eq!(fs::read_to_string(unit).unwrap(), original);
    assert!(!fixture.units().join("paseo.service").exists());
    assert!(
        fixture
            .units()
            .join("custom-paseo.service.d/zz-paseo-managed.conf")
            .exists()
    );
}

#[test]
fn legacy_daemon_waits_for_work_then_migrates_with_same_identity_and_credential() {
    let fixture = Fixture::new(false);
    let started = success(
        fixture
            .command("start")
            .env("PASEO_NODE_ENV", "development")
            .output()
            .unwrap(),
    );
    let agent = success(
        fixture
            .cli()
            .args([
                "run",
                "emit synthetic plan approval",
                "--no-wait",
                "--provider",
                "mock",
                "--json",
                "--home",
            ])
            .arg(&fixture.home)
            .output()
            .unwrap(),
    );
    let lock_path = fixture.home.join("paseo.pid");
    let mut lock = read_json(&lock_path);
    lock["desktopManaged"] = Value::Bool(true);
    // A pre-stage-1 supervisor differs on disk only by this ownership bit;
    // leave its inode intact so the running supervisor retains its lease.
    fs::write(&lock_path, serde_json::to_vec(&lock).unwrap()).unwrap();
    let credential = fs::read(fixture.home.join("local-credential")).unwrap();
    let public_key = fs::read(fixture.home.join("daemon-keypair.json")).unwrap();
    let pending = success(fixture.ensure(true).output().unwrap());
    assert_eq!(pending["state"], "running");
    assert_eq!(pending["owner"], "desktop-legacy");
    assert_eq!(pending["migration"], "pending");
    assert_eq!(pending["pid"], started["pid"]);
    assert_eq!(
        fs::read(fixture.home.join("local-credential")).unwrap(),
        credential
    );
    assert!(!fixture.home.join("native-update-operation.json").exists());
    success(
        fixture
            .cli()
            .args([
                "stop",
                agent["agentId"].as_str().unwrap(),
                "--json",
                "--home",
            ])
            .arg(&fixture.home)
            .output()
            .unwrap(),
    );
    // Reassert the old bit in case its heartbeat occurred while copying files.
    fs::write(&lock_path, serde_json::to_vec(&lock).unwrap()).unwrap();
    let migrated = success(fixture.ensure(false).output().unwrap());
    assert_eq!(migrated["state"], "started");
    assert_eq!(migrated["owner"], "detached");
    assert_ne!(started["pid"], migrated["pid"]);
    assert_eq!(read_json(&lock_path)["serverId"], lock["serverId"]);
    assert_eq!(
        fs::read(fixture.home.join("local-credential")).unwrap(),
        credential
    );
    assert_eq!(
        fs::read(fixture.home.join("daemon-keypair.json")).unwrap(),
        public_key
    );
    assert!(!fixture.home.join("native-update-operation.json").exists());
}

#[test]
fn missing_first_run_runtime_has_a_structured_failure() {
    let fixture = Fixture::new(false);
    let output = fixture.ensure(false).output().unwrap();
    assert!(!output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["state"], "failed");
    assert!(value["error"].as_str().unwrap().contains("--runtime"));
}
