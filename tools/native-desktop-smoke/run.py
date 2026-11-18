import argparse, json, os, pathlib, platform, shutil, signal, subprocess, tempfile, time
repo = pathlib.Path(__file__).resolve().parents[2]
target = pathlib.Path(os.environ.get('CARGO_TARGET_DIR', repo / 'target'))
arch = {'x86_64': 'x64', 'aarch64': 'arm64'}.get(platform.machine(), platform.machine())
parser = argparse.ArgumentParser(description='Exercise native last-window close and graphical logout with an independent daemon in private Xvfb/D-Bus sessions.')
parser.add_argument('--bundle', type=pathlib.Path, default=(repo / f'.dev/build/gui/desktop/linux-{arch}/smoke-debug/bundle'), help='Client bundle from `cargo xtask build-gui linux --smoke --debug` (lifecycle driver compiled in)')
parser.add_argument('--runtime', type=pathlib.Path, help='Native desktop runtime containing desktop-runtime.json (default: the bundle\'s runtime/)')
parser.add_argument('--openbox', default=shutil.which('openbox'), help='Openbox executable for the private Xvfb session (default: PATH)')
options = parser.parse_args()
if not options.openbox:
    parser.error('Openbox is required for native window-manager close; install it or pass --openbox PATH')
options.openbox = str(pathlib.Path(options.openbox).resolve())
if not shutil.which('xprop', path='/usr/bin:/bin'):
    parser.error('xprop is required to inspect the private window manager; install x11-utils')
bundle = options.bundle.resolve()
runtime = (options.runtime or bundle / 'runtime').resolve()
if not (runtime / 'desktop-runtime.json').is_file() or not (bundle / 'paseo_sessions').is_file():
    parser.error('Build the smoke bundle first: cargo xtask build-gui linux --smoke --debug')
daemon_cli = runtime / 'paseo'

def daemon_status(environment):
    command = [str(daemon_cli), 'daemon', 'status', '--home', environment['PASEO_HOME'], '--json']
    result = subprocess.run(command, env=environment, capture_output=True, text=True, timeout=15)
    if result.returncode != 0:
        raise RuntimeError(f'Private daemon status failed: {result.stderr or result.stdout}')
    return json.loads(result.stdout)

def wait_for_daemon(daemon, environment):
    deadline = time.monotonic() + 35
    while daemon.poll() is None:
        status = daemon_status(environment)
        if status.get('connectedDaemon') == 'reachable' and status.get('workerPid') is not None:
            return status
        if time.monotonic() >= deadline:
            raise RuntimeError(f'Private daemon did not become ready: {status}')
        time.sleep(0.2)
    raise RuntimeError(f'Private foreground daemon exited with code {daemon.returncode}')

def daemon_identity(status, group, home):
    if status['localDaemon'] != 'running' or status['connectedDaemon'] != 'reachable' or status['home'] != home:
        raise RuntimeError(f'Private daemon is not running and reachable: {status}')
    host, _, port = status['listen'].rpartition(':')
    if host != '127.0.0.1' or not port.isdecimal() or int(port) == 0:
        raise RuntimeError(f'Expected a private loopback listener, got {status["listen"]!r}')
    identity = {'listen': status['listen'], 'startedAt': status['startedAt']}
    for key in ('pid', 'workerPid'):
        pid = status[key]
        # Linux /proc stat fields after the parenthesized executable name:
        # state (3), process group (5), and process start time (22).
        fields = pathlib.Path(f'/proc/{pid}/stat').read_text().rsplit(')', 1)[1].split()
        if fields[0] in {'Z', 'X'} or int(fields[2]) != group:
            raise RuntimeError(f'Daemon {key} {pid} is not alive in the private daemon process group')
        identity[key] = pid
        identity[key + 'StartTimeTicks'] = int(fields[19])
    return identity

def stop_daemon(daemon):
    forced = False
    if daemon.poll() is None:
        # The foreground CLI forwards cancellation to its supervisor and waits
        # for the worker; kill only this private process group if that stalls.
        daemon.terminate()
        try:
            daemon.wait(timeout=25)
        except subprocess.TimeoutExpired:
            forced = True
            os.killpg(daemon.pid, signal.SIGKILL)
            daemon.wait()
    return {'exitCode': daemon.returncode, 'forced': forced}

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
    for name in ('last-window-close', 'logout'):
        case = proof / name
        case.mkdir()
        with tempfile.TemporaryDirectory(prefix='paseo-native-desktop-state-') as temporary:
            state = pathlib.Path(temporary).resolve()
            # Start with a clean environment, not the caller's deployment, DBus
            # or graphics session. In particular do not inherit PASEO_LISTEN or PORT.
            environment = {'LANG': 'C.UTF-8', 'LC_ALL': 'C.UTF-8'}
            for key, child in [('HOME', 'home'), ('PASEO_HOME', 'paseo'), ('XDG_CONFIG_HOME', 'config'), ('XDG_DATA_HOME', 'data'), ('XDG_STATE_HOME', 'state'), ('XDG_CACHE_HOME', 'cache'), ('XDG_RUNTIME_DIR', 'runtime'), ('CODEX_HOME', 'codex'), ('CLAUDE_CONFIG_DIR', 'claude'), ('TMPDIR', 'tmp')]:
                directory = state / child
                directory.mkdir(mode=0o700)
                environment[key] = str(directory)
            bin_dir = state / 'bin'
            bin_dir.mkdir()
            systemctl = bin_dir / 'systemctl'
            systemctl.write_text('#!/bin/sh\nexit 1\n')
            systemctl.chmod(0o700)
            environment['PATH'] = f'{bin_dir}:/usr/bin:/bin'

            config = {'version': 1, 'daemon': {'listen': '127.0.0.1:0', 'relay': {'enabled': False}, 'mcp': {'enabled': False}, 'enableTerminalAgentHooks': False}, 'pluginsEnabled': False, 'agents': {'providers': {provider: {'enabled': False} for provider in ['claude', 'codex', 'copilot', 'opencode', 'pi', 'omp']}}}
            (state / 'paseo/config.json').write_text(json.dumps(config))
            gui_state = state / 'gui'
            gui_state.mkdir()
            (gui_state / 'desktop-settings.json').write_text(json.dumps({
                'version': 1, 'settings': {'daemon': {'startDaemonIfNotRunning': False}},
            }))
            desktop_env = dict(
                environment, PASEO_GUI_STATE_DIR=str(gui_state),
                PASEO_DESKTOP_SMOKE_PROOF=str(case), PASEO_SMOKE_LOGOUT=str(int(name == 'logout')),
                PASEO_SMOKE_OPENBOX=options.openbox, PASEO_DESKTOP_RUNTIME=str(runtime / 'desktop-runtime.json'),
                GDK_BACKEND='x11', XDG_SESSION_TYPE='x11', XDG_CURRENT_DESKTOP='PaseoSmoke',
                GIO_USE_VFS='local', GTK_USE_PORTAL='0', GSETTINGS_BACKEND='memory', NO_AT_BRIDGE='1',
            )
            home = environment['PASEO_HOME']
            with (case / 'foreground-daemon.log').open('w') as log:
                daemon = subprocess.Popen(
                    [str(daemon_cli), 'daemon', 'run', '--no-tray', '--home', home],
                    cwd=state, env=environment, stdin=subprocess.DEVNULL, stdout=log,
                    stderr=subprocess.STDOUT, start_new_session=True,
                )
            try:
                initial = wait_for_daemon(daemon, environment)
                (case / 'initial-daemon-status.json').write_text(json.dumps(initial, indent=2))
                identity = daemon_identity(initial, daemon.pid, home)
                command = [
                    '/usr/bin/xvfb-run', '-a', '-s', '-screen 0 1440x1000x24 -nolisten tcp',
                    '/usr/bin/dbus-run-session', '/usr/bin/python3',
                    str(pathlib.Path(__file__).resolve().with_name('bus.py')), str(app / 'paseo_sessions'),
                ]
                with (case / 'harness.log').open('w') as log:
                    with subprocess.Popen(command, cwd=state, env=desktop_env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True) as result:
                        try:
                            result.wait(timeout=180)
                        except BaseException:
                            try:
                                os.killpg(result.pid, signal.SIGTERM)
                                result.wait(timeout=5)
                            except ProcessLookupError:
                                pass
                            except subprocess.TimeoutExpired:
                                os.killpg(result.pid, signal.SIGKILL)
                            raise
                final = daemon_status(environment)
                (case / 'final-daemon-status.json').write_text(json.dumps(final, indent=2))
                if result.returncode != 0:
                    raise RuntimeError(f'{name} failed; see {case}')
                observed = json.loads((case / 'app-daemon-status.json').read_text())
                if observed['status'] != 'running' or any(observed[key] != identity[key] for key in ('pid', 'listen')):
                    raise RuntimeError(f'{name} did not observe the independent daemon: {observed}')
                if daemon.poll() is not None or daemon_identity(final, daemon.pid, home) != identity:
                    raise RuntimeError(f'{name} stopped or replaced the independent daemon: {final}')
                results.append({'scenario': name, 'status': 'passed', 'daemon': final, 'processIdentity': identity})
                print(json.dumps(results[-1]), flush=True)
            finally:
                cleanup = stop_daemon(daemon)
                (case / 'cleanup.json').write_text(json.dumps(cleanup))
                daemon_log = state / 'paseo/daemon.log'
                if daemon_log.exists():
                    shutil.copy2(daemon_log, case / 'daemon.log')
finally:
    (proof / 'results.json').write_text(json.dumps(results, indent=2))
    shutil.rmtree(app)
    print(json.dumps({'proof': str(proof), 'temporaryAppRemoved': True}), flush=True)
