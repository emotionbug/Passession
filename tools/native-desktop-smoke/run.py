import argparse, json, os, pathlib, platform, shutil, signal, subprocess, tempfile
repo = pathlib.Path(__file__).resolve().parents[2]
target = pathlib.Path(os.environ.get('CARGO_TARGET_DIR', repo / 'target'))
arch = {'x86_64': 'x64', 'aarch64': 'arm64'}.get(platform.machine(), platform.machine())
parser = argparse.ArgumentParser(description='Exercise the real Linux desktop, native tray, and Rust daemon in a private Xvfb/D-Bus session.')
parser.add_argument('--bundle', type=pathlib.Path, default=(repo / f'.dev/build/gui/desktop/linux-{arch}/smoke-debug/bundle'), help='Client bundle from `cargo xtask build-gui linux --smoke --debug` (lifecycle driver compiled in)')
parser.add_argument('--runtime', type=pathlib.Path, help='Native desktop runtime containing desktop-runtime.json (default: the bundle\'s runtime/)')
parser.add_argument('--openbox', default=shutil.which('openbox'), help='Openbox executable for the private Xvfb session (default: PATH)')
options = parser.parse_args()
if not options.openbox:
    parser.error('Openbox is required for real window hide/restore; install it or pass --openbox PATH')
options.openbox = str(pathlib.Path(options.openbox).resolve())
bundle = options.bundle.resolve()
runtime = (options.runtime or bundle / 'runtime').resolve()
if not (runtime / 'desktop-runtime.json').is_file() or not (bundle / 'paseo_sessions').is_file():
    parser.error('Build the smoke bundle first: cargo xtask build-gui linux --smoke --debug')
target.mkdir(parents=True, exist_ok=True)
# Keep the private executable on disk; /tmp may be a small RAM-backed filesystem.
proof = pathlib.Path(tempfile.mkdtemp(prefix='paseo-native-desktop-proof-', dir=target)).resolve()
app = proof / 'app'
app.mkdir()
shutil.copy2(bundle / 'paseo_sessions', app / 'paseo_sessions')
for resource in bundle.iterdir():
    if resource.name not in {'paseo_sessions', 'runtime'}:
        (app / resource.name).symlink_to(resource, target_is_directory=resource.is_dir())
(app / 'runtime').symlink_to(runtime, target_is_directory=True)
print(json.dumps({'phase': 'start', 'proof': str(proof)}), flush=True)
results = []
try:
    for name, panel, logout in [('tray', '1', '0'), ('no-panel', '0', '0'), ('logout', '1', '1')]:
        case = proof / name
        case.mkdir()
        state = pathlib.Path(tempfile.mkdtemp(prefix='paseo-native-desktop-state-'))
        environment = dict(os.environ)
        environment.pop('WAYLAND_DISPLAY', None)
        environment.pop('SESSION_MANAGER', None)
        for key, child in [('HOME', 'home'), ('PASEO_HOME', 'paseo'), ('XDG_CONFIG_HOME', 'config'), ('XDG_DATA_HOME', 'data'), ('XDG_STATE_HOME', 'state'), ('XDG_CACHE_HOME', 'cache'), ('XDG_RUNTIME_DIR', 'runtime'), ('CODEX_HOME', 'codex'), ('CLAUDE_CONFIG_DIR', 'claude'), ('TMPDIR', 'tmp')]:
            directory = state / child
            directory.mkdir(mode=0o700)
            environment[key] = str(directory)
        environment.update({'PATH': '/usr/bin:/bin', 'GDK_BACKEND': 'x11', 'XDG_SESSION_TYPE': 'x11', 'XDG_CURRENT_DESKTOP': 'PaseoSmoke', 'GIO_USE_VFS': 'local', 'GTK_USE_PORTAL': '0', 'GSETTINGS_BACKEND': 'memory', 'NO_AT_BRIDGE': '1', 'LANG': 'C.UTF-8', 'LC_ALL': 'C.UTF-8', 'PASEO_DESKTOP_SMOKE_PROOF': str(case), 'PASEO_SMOKE_PANEL': panel, 'PASEO_SMOKE_LOGOUT': logout, 'PASEO_DESKTOP_RUNTIME': str(runtime / 'desktop-runtime.json')})
        environment['PASEO_SMOKE_OPENBOX'] = options.openbox
        config = {'version': 1, 'daemon': {'listen': '127.0.0.1:0', 'relay': {'enabled': False}, 'mcp': {'enabled': False}, 'enableTerminalAgentHooks': False}, 'pluginsEnabled': False, 'agents': {'providers': {name: {'enabled': False} for name in ['claude', 'codex', 'copilot', 'opencode', 'pi', 'omp']}}, 'features': {'dictation': {'enabled': False}, 'voiceMode': {'enabled': False}}}
        (state / 'paseo/config.json').write_text(json.dumps(config))
        try:
            with (case / 'harness.log').open('w') as log:
                with subprocess.Popen(['/usr/bin/xvfb-run', '-a', '-s', '-screen 0 1440x1000x24 -nolisten tcp', '/usr/bin/dbus-run-session', '/usr/bin/python3', str(pathlib.Path(__file__).with_name('bus.py')), str(app / 'paseo_sessions')], env=environment, stdout=log, stderr=subprocess.STDOUT, start_new_session=True) as result:
                    try:
                        result.wait(timeout=180)
                    except subprocess.TimeoutExpired:
                        os.killpg(result.pid, signal.SIGTERM)
                        try:
                            result.wait(timeout=5)
                        finally:
                            try: os.killpg(result.pid, signal.SIGKILL)
                            except ProcessLookupError: pass
                        raise
            status = subprocess.run([str(runtime / 'paseo'), 'daemon', 'status', '--home', environment['PASEO_HOME'], '--json'], env=environment, capture_output=True, text=True, timeout=15)
            assert status.returncode == 0, status.stderr
            value = json.loads(status.stdout)
            (case / 'final-daemon-status.json').write_text(json.dumps(value, indent=2))
            assert result.returncode == 0, f'{name} failed; see {case}'
            assert value['localDaemon'] == 'stopped', f'{name} left the native daemon running'
            results.append({'scenario': name, 'status': 'passed', 'daemon': value})
            print(json.dumps(results[-1]), flush=True)
        finally:
            daemon_log = state / 'paseo/daemon.log'
            if daemon_log.exists(): shutil.copy2(daemon_log, case / 'daemon.log')
            cleanup = subprocess.run([str(runtime / 'paseo'), 'daemon', 'stop', '--home', environment['PASEO_HOME'], '--timeout', '5', '--json'], env=environment, capture_output=True, text=True, timeout=15)
            (case / 'cleanup.json').write_text(json.dumps({'exitCode': cleanup.returncode, 'stdout': cleanup.stdout, 'stderr': cleanup.stderr}))
            shutil.rmtree(state)
finally:
    (proof / 'results.json').write_text(json.dumps(results, indent=2))
    shutil.rmtree(app)
    print(json.dumps({'proof': str(proof), 'temporaryAppRemoved': True}), flush=True)
