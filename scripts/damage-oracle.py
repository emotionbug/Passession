#!/usr/bin/env python3
"""Compare partial and full rendering of the same app frame on an X11 display.

Run under dbus-run-session (and, optionally, xvfb-run). See docs/development.md.
"""

import argparse
import ctypes as c
import json
import os
from pathlib import Path
import re
import signal
import sqlite3
import subprocess
import tempfile
import time
from datetime import datetime, timezone
from hashlib import sha256

from PIL import Image

ROOT = Path(__file__).resolve().parents[1]
SERVER = 'srv_damage_oracle'
PARENT = 'damage-parent'
CHILD = 'damage-child'

X11 = c.CDLL('libX11.so.6')
U, I, P = c.c_ulong, c.c_int, c.c_void_p


def x(name, result, *parameters):
    function = getattr(X11, name)
    function.restype, function.argtypes = result, parameters
    return function


open_display = x('XOpenDisplay', P, c.c_char_p)
close_display = x('XCloseDisplay', I, P)
root_window = x('XDefaultRootWindow', U, P)
query_tree = x('XQueryTree', I, P, U, c.POINTER(U), c.POINTER(U), c.POINTER(c.POINTER(U)), c.POINTER(c.c_uint))
translate = x('XTranslateCoordinates', I, P, U, U, I, I, c.POINTER(I), c.POINTER(I), c.POINTER(U))
free = x('XFree', I, P)
fetch_name = x('XFetchName', I, P, U, c.POINTER(c.c_char_p))
resize = x('XMoveResizeWindow', I, P, U, I, I, c.c_uint, c.c_uint)
raise_window = x('XRaiseWindow', I, P, U)
set_focus = x('XSetInputFocus', I, P, U, I, U)
get_focus = x('XGetInputFocus', I, P, c.POINTER(U), c.POINTER(I))
sync = x('XSync', I, P, I)
keysym = x('XStringToKeysym', U, c.c_char_p)
keycode = x('XKeysymToKeycode', c.c_ubyte, P, U)
intern_atom = x('XInternAtom', U, P, c.c_char_p, I)
send_event = x('XSendEvent', I, P, U, I, c.c_long, P)


class ClientMessage(c.Structure):
    _fields_ = [('type', I), ('serial', U), ('send_event', I), ('display', P),
                ('window', U), ('message_type', U), ('format', I), ('data', c.c_long * 5)]


class InputEvent(c.Structure):
    # XKeyEvent, XButtonEvent and XMotionEvent have the same layout here;
    # XMotionEvent's `is_hint` is the low byte of a zero detail field.
    _fields_ = [('type', I), ('serial', U), ('send_event', I), ('display', P),
                ('window', U), ('root', U), ('subwindow', U), ('time', U),
                ('x', I), ('y', I), ('x_root', I), ('y_root', I),
                ('state', c.c_uint), ('detail', c.c_uint), ('same_screen', I)]


class XEvent(c.Union):
    _fields_ = [('client', ClientMessage), ('input', InputEvent), ('pad', c.c_long * 24)]


class XImage(c.Structure):
    _fields_ = [('width', I), ('height', I), ('xoffset', I), ('format', I), ('data', P),
                ('byte_order', I), ('bitmap_unit', I), ('bitmap_bit_order', I), ('bitmap_pad', I),
                ('depth', I), ('bytes_per_line', I), ('bits_per_pixel', I),
                ('red_mask', U), ('green_mask', U), ('blue_mask', U)]


get_image = x('XGetImage', c.POINTER(XImage), P, U, I, I, c.c_uint, c.c_uint, U, I)
destroy_image = x('XDestroyImage', I, c.POINTER(XImage))



class Display:
    def __init__(self):
        self.d = open_display(None)
        if not self.d:
            raise RuntimeError('Cannot open DISPLAY')
        self.root = root_window(self.d)
        self.prior_focus = self.focus()
        self.existing = set(self.windows())
        self.window = None
        self.size = None

    def windows(self):
        queue = [self.root]
        while queue:
            window = queue.pop()
            parent, root, children, count = U(), U(), c.POINTER(U)(), c.c_uint()
            if query_tree(self.d, window, c.byref(root), c.byref(parent), c.byref(children), c.byref(count)):
                queue.extend(children[:count.value] if children else ())
                if children:
                    free(children)
            yield window

    def find_app(self):
        matches = []
        for window in self.windows():
            if window in self.existing:
                continue
            name = c.c_char_p()
            if fetch_name(self.d, window, c.byref(name)) and name:
                if name.value in (b'Paseo', b'Passession'):
                    matches.append(window)
                free(c.cast(name, P))
        if len(matches) > 1:
            # A reparenting WM may name both its frame and the client.
            frames = {ancestor for child in matches for ancestor in self.ancestors(child)}
            matches = [window for window in matches if window not in frames]
        if len(matches) > 1:
            raise RuntimeError('More than one new Paseo window; refusing to inject input')
        return next(iter(matches), None)

    def ancestors(self, window):
        while window not in (0, self.root):
            root, parent, children, count = U(), U(), c.POINTER(U)(), c.c_uint()
            query_tree(self.d, window, c.byref(root), c.byref(parent), c.byref(children), c.byref(count))
            if children:
                free(children)
            window = parent.value
            yield window

    def focus(self):
        current, revert = U(), I()
        get_focus(self.d, c.byref(current), c.byref(revert))
        return current.value

    def origin(self):
        x, y, child = I(), I(), U()
        if not translate(self.d, self.window, self.root, 0, 0, c.byref(x), c.byref(y), c.byref(child)):
            raise RuntimeError('App is not on the target screen')
        return x.value, y.value

    def geometry(self, width, height):
        resize(self.d, self.window, 60, 65, width, height)
        raise_window(self.d, self.window)
        sync(self.d, 0)
        set_focus(self.d, self.window, 1, 0)
        sync(self.d, 0)
        if self.focus() != self.window:
            raise RuntimeError('App did not acquire focus')
        self.size = width, height

    def send(self, event):
        if self.focus() != self.window:
            raise RuntimeError('Focus changed to another window; refusing to inject input')
        if not send_event(self.d, self.window, 0, 0, c.byref(event)):
            raise RuntimeError('Could not send a fixture event')
        sync(self.d, 0)

    def input(self, kind, x=0, y=0, detail=0, state=0):
        # Address the fixture directly. XTEST requires compositor cooperation
        # on Xwayland and would also move the user's real pointer.
        ox, oy = self.origin()
        ms = int(time.monotonic() * 1000) & 0xffffffff
        self.send(XEvent(input=InputEvent(kind, 0, 1, self.d, self.window, self.root, 0, ms,
                                          x, y, ox + x, oy + y, state, detail, 1)))

    def move(self, x, y):
        self.input(6, x, y)

    def click(self, x, y, button=1):
        self.move(x, y)
        self.input(4, x, y, button)
        self.input(5, x, y, button, 1 << (7 + button))

    def wheel(self, x, y, button, count, interval=0.015):
        for _ in range(count):
            self.click(x, y, button)
            time.sleep(interval)

    def fling(self, x, y):
        self.move(x, y)
        atom = intern_atom(self.d, b'PASEO_DAMAGE_ORACLE_TOUCH', 1)
        if not atom:
            raise RuntimeError('The debug client does not expose the damage oracle touch mover')
        for phase, distance in [(0, 0), *((1, i * 28) for i in range(1, 10)), (2, 270)]:
            event = XEvent(client=ClientMessage(33, 0, 1, self.d, self.window, atom, 32,
                                                (c.c_long * 5)(phase, x, y - distance, 0, 0)))
            self.send(event)
            time.sleep(0.014)

    def key(self, name):
        code = keycode(self.d, keysym(name.encode()))
        if not code:
            raise RuntimeError(f'No keycode for {name}')
        self.input(2, detail=code)
        self.input(3, detail=code)

    def screenshot(self):
        # Read only the fixture window. Xwayland may deny access to the root.
        w, h = self.size
        image = get_image(self.d, self.window, 0, 0, w, h, U(-1), 2)
        if not image:
            raise RuntimeError('Could not capture the fixture window')
        try:
            info = image.contents
            if (info.bits_per_pixel, info.byte_order, info.red_mask, info.green_mask, info.blue_mask) != (32, 0, 0xff0000, 0xff00, 0xff):
                raise RuntimeError('Fixture screenshots require a little-endian X11 TrueColor visual')
            data = c.string_at(info.data, info.bytes_per_line * h)
            return Image.frombytes('RGB', (w, h), data, 'raw', 'BGRX', info.bytes_per_line, 1)
        finally:
            destroy_image(image)

    def restore(self):
        if self.window and self.focus() == self.window:
            if self.prior_focus in set(self.windows()) or self.prior_focus in (0, 1):
                set_focus(self.d, self.prior_focus, 1, 0)
        close_display(self.d)


def write_json(path, data):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data))


def seed(home, project):
    config = {'version': 1, 'daemon': {'listen': '127.0.0.1:0', 'relay': {'enabled': False}, 'mcp': {'enabled': False}, 'pluginsEnabled': False, 'enableTerminalAgentHooks': False}, 'agents': {'providers': {p: {'enabled': p == 'omp'} for p in ('omp', 'claude', 'codex', 'copilot', 'opencode', 'pi', 'mock-slow')}}}
    write_json(home / 'config.json', config)
    (home / 'server-id').write_text(SERVER + '\n')
    dates = dict.fromkeys(('createdAt', 'updatedAt', 'lastUserMessageAt'), '2026-01-01T12:00:00.000Z')
    write_json(home / 'projects/projects.json', [{'projectId': 'damage-proj', 'rootPath': str(project), 'kind': 'git', 'displayName': 'Damage fixture', 'archivedAt': None, **dates}])
    write_json(home / 'projects/workspaces.json', [{'workspaceId': 'damage-work', 'projectId': 'damage-proj', 'cwd': str(project), 'kind': 'local_checkout', 'displayName': 'Damage workspace', 'archivedAt': None, **dates}])
    for i in range(52):
        identity = PARENT if i == 0 else f'damage-sidebar-{i:02}'
        title = 'Damage parent transcript' if i == 0 else f'Damage row {i:02}'
        write_json(home / f'agents/damage/{identity}.json', {'id': identity, 'provider': 'omp', 'cwd': str(project), 'title': title, 'workspaceId': 'damage-work', 'lastStatus': 'idle', **dates})
    now = datetime.now(timezone.utc).isoformat(timespec='milliseconds').replace('+00:00', 'Z')
    descriptor = {'id': CHILD, 'parentAgentId': PARENT, 'title': 'Damage child transcript', 'description': 'Synthetic data', 'status': 'running', 'createdAt': now, 'updatedAt': now}
    index = home / f'agent-subagents/{PARENT}/index.sqlite3'
    index.parent.mkdir(parents=True)
    with sqlite3.connect(index) as db:
        db.executescript('CREATE TABLE identity(version INTEGER NOT NULL, parent_id TEXT NOT NULL); CREATE TABLE children(id TEXT PRIMARY KEY, descriptor TEXT);')
        db.execute('INSERT INTO identity VALUES (1, ?)', (PARENT,))
        db.execute('INSERT INTO children VALUES (?, ?)', (CHILD, json.dumps(descriptor)))
    identity = json.dumps([PARENT, CHILD], separators=(',', ':'))
    transcript(home / f'agent-timelines/{PARENT}.sqlite3', PARENT, now)
    transcript(index.parent / (sha256(identity.encode()).hexdigest() + '.sqlite3'), identity, now)


def transcript(path, identity, timestamp):
    path.parent.mkdir(parents=True, exist_ok=True)
    with sqlite3.connect(path) as db:
        db.executescript('''
            CREATE TABLE timeline_metadata(id INTEGER PRIMARY KEY, version INTEGER NOT NULL, agent_id TEXT NOT NULL, epoch TEXT NOT NULL, next_seq INTEGER NOT NULL, min_seq INTEGER NOT NULL);
            CREATE TABLE timeline_rows(position INTEGER PRIMARY KEY, row_json TEXT NOT NULL);
            CREATE TABLE timeline_native_revision(id INTEGER PRIMARY KEY CHECK(id = 1), revision_json TEXT NOT NULL);
            CREATE INDEX timeline_rows_page ON timeline_rows(position, json_extract(row_json,'$.seqStart'), json_extract(row_json,'$.seqEnd'), json_extract(row_json,'$.sourceSeqRanges'));
            CREATE INDEX timeline_rows_identity ON timeline_rows(position, json_extract(row_json,'$.item.callId'));
            CREATE INDEX timeline_rows_submitted ON timeline_rows(position,json_extract(row_json,'$.item.clientMessageId'));
            CREATE TRIGGER timeline_revision_insert AFTER INSERT ON timeline_rows BEGIN DELETE FROM timeline_native_revision; END;
            CREATE TRIGGER timeline_revision_update AFTER UPDATE ON timeline_rows BEGIN DELETE FROM timeline_native_revision; END;
            CREATE TRIGGER timeline_revision_delete AFTER DELETE ON timeline_rows BEGIN DELETE FROM timeline_native_revision; END;
        ''')
        db.execute('INSERT INTO timeline_metadata VALUES (1, 1, ?, ?, 145, 0)', (identity, 'damage-fixture'))
        for i in range(1, 145):
            message = {'type': 'user_message' if i % 4 == 1 else 'assistant_message', 'text': f'Fixture paint row {i}: retained text and **styled text**.', 'messageId': f'damage-{i}'}
            db.execute('INSERT INTO timeline_rows VALUES (?, ?)', (i - 1, json.dumps({'seq': i, 'seqStart': i, 'seqEnd': i, 'sourceSeqRanges': [{'startSeq': i, 'endSeq': i}], 'timestamp': timestamp, 'item': message})))


def slog(log, offset):
    with log.open('rb') as stream:
        stream.seek(offset)
        return stream.read().decode(errors='replace')


def phase(name, log, display, run, output, settle=0.65):
    start = log.stat().st_size
    before = display.screenshot().tobytes() if output else None
    run()
    time.sleep(settle)
    if output:
        image = display.screenshot()
        image.save(output / f'{name}.png')
        if before == image.tobytes():
            print(f'{name}: unchanged final pixels', flush=True)
    text = slog(log, start)
    oracle = re.findall(r'result=(full|equal|mismatch)', text)
    # The first one-second bucket can begin before this phase.
    buckets = re.findall(r'PASEO_FRAME_TRACE .*?render_frames=(\d+).*?render_ms=([\d.]+)', text)[1:]
    frames = sum(int(count) for count, _ in buckets)
    timing = (f'{sum(float(ms) for _, ms in buckets) / frames:.3f} ms/frame'
              if frames else 'no complete timing samples')
    print(f'{name}: {len(oracle)} presentations, {oracle.count("equal")} partial matches, '
          f'{frames} timed frames, {timing}', flush=True)
    if 'mismatch' in oracle:
        raise AssertionError(f'Pixel mismatch in {name}')


def exercise(log, d, args):
    def step(name, func, settle=0.65):
        phase(name, log, d, func, args.screenshots, settle)

    d.geometry(850, 900)
    time.sleep(0.7)
    step('spinner', lambda: d.move(400, 365), 3.2)
    if args.profile:
        def sheets():
            for _ in range(7):
                d.click(*args.pill)
                time.sleep(0.32)
                d.key('Escape')
                time.sleep(0.32)
        step('sheet-transitions', sheets, 0.2)
        return
    step('hover', lambda: d.move(390, 90))
    step('typing', lambda: (d.click(165, 237), [d.key(key) for key in 'damage']))
    step('typing-reset', lambda: [d.key('BackSpace') for _ in 'damage'])
    step('scroll', lambda: d.wheel(620, 400, 4, 10))
    step('scroll-back', lambda: d.wheel(620, 400, 5, 20))
    step('sidebar-fling', lambda: d.fling(145, 685))
    step('sidebar-reset', lambda: d.wheel(145, 440, 4, 35, 0.004))
    step('subagent-sheet-open', lambda: d.click(*args.pill))
    step('child-sheet-open', lambda: d.click(*args.child))
    step('child-sheet-close', lambda: d.key('Escape'))
    step('subagent-sheet-close', lambda: d.key('Escape'))
    step('resize-wide', lambda: d.geometry(1050, 900))
    step('popover-open', lambda: d.click(*args.wide_pill))
    step('popover-close', lambda: d.key('Escape'))
    step('resize-compact', lambda: d.geometry(420, 760))
    step('compact-pop', lambda: d.click(30, 34))
    step('compact-push', lambda: d.click(*args.compact_row))
    step('compact-escape-pop', lambda: d.key('Escape'))


def cleanup_bus(bus):
    # dbus-run-session closes the bus itself. Dispose only processes launched
    # from that private bus (keyring checks and desktop portal helpers).
    tag = f'DBUS_SESSION_BUS_ADDRESS={bus}'.encode()
    for sig in (signal.SIGTERM, signal.SIGKILL):
        for proc in Path('/proc').glob('[0-9]*'):
            pid = int(proc.name)
            if pid in (1, os.getpid(), os.getppid()):
                continue
            try:
                if tag in (proc / 'environ').read_bytes().split(b'\0'):
                    os.kill(pid, sig)
            except OSError:
                pass
        time.sleep(0.12)


def run(args, case):
    # Never pass provider credentials, desktop launch overrides, or user-specific PATH.
    keys = ('DISPLAY', 'XAUTHORITY', 'DBUS_SESSION_BUS_ADDRESS', 'LIBGL_ALWAYS_SOFTWARE',
            'MESA_LOADER_DRIVER_OVERRIDE', 'GALLIUM_DRIVER')
    env = {key: os.environ[key] for key in keys if key in os.environ}
    env.update(PATH='/usr/bin:/bin', LANG='C.UTF-8', LC_ALL='C.UTF-8')
    for key, name in {'HOME': 'home', 'PASEO_HOME': 'paseo', 'XDG_CONFIG_HOME': 'config', 'XDG_DATA_HOME': 'data', 'XDG_STATE_HOME': 'state', 'XDG_CACHE_HOME': 'cache', 'XDG_RUNTIME_DIR': 'runtime', 'TMPDIR': 'tmp'}.items():
        directory = case / name
        directory.mkdir(mode=0o700)
        env[key] = str(directory)
    env.update(GDK_BACKEND='x11', XDG_SESSION_TYPE='x11', XDG_CURRENT_DESKTOP='PaseoDamageOracle', GIO_USE_VFS='local', GTK_USE_PORTAL='0', GSETTINGS_BACKEND='memory', NO_AT_BRIDGE='1', PASEO_NODE_ENV='development', PASEO_FRAME_TRACE='1')
    env['PASEO_DAMAGE_ORACLE_TOUCH'] = '1'
    if not args.profile:
        env['PASEO_VERIFY_DAMAGE'] = '1'
    if args.full:
        env['PASEO_FULL_REDRAW'] = '1'
    settings = {'daemon': {'startDaemonIfNotRunning': False}}
    write_json(case / 'data/sh.paseo.sessions.desktop/desktop-settings.json', settings)
    manifest = case / 'native-runtime.json'
    write_json(manifest, {'executable': str(args.cli), 'workingDirectory': str(args.cli.parent)})
    env['PASEO_DESKTOP_RUNTIME'] = str(manifest)
    project = case / 'project'
    subprocess.run(['git', 'init', '-q', str(project)], check=True, env=env)
    seed(case / 'paseo', project)
    log = case / 'app.log'
    app = None
    display = Display()
    with (case / 'daemon-cli.log').open('w') as cli_log:
        def cli(*arguments, check=True):
            subprocess.run([args.cli, *arguments, '--home', env['PASEO_HOME'], '--json'], timeout=20, env=env, cwd=project, check=check, stdout=cli_log, stderr=cli_log)

        try:
            cli('daemon', 'start')
            cli('run', '--provider', 'mock', '--title', 'Damage live spinner', '--cwd', str(project), '--detach', 'Hold this synthetic turn indefinitely')
            with log.open('w') as output:
                app = subprocess.Popen([args.app, f'paseo://h/{SERVER}/agent/{PARENT}'], cwd=args.app.parent, env=env, stdout=output, stderr=output)
            for _ in range(250):
                display.window = display.find_app()
                if display.window is not None:
                    break
                if app.poll() is not None:
                    raise RuntimeError('App exited while waiting for its window')
                time.sleep(0.04)
            if not display.window:
                raise RuntimeError('Fixture window did not appear')
            display.geometry(850, 900)
            # Give daemon attach, font discovery, and initial shaders time to settle.
            time.sleep(2.0)
            exercise(log, display, args)
            text = log.read_text(errors='replace')
            if 'result=mismatch' in text:
                raise AssertionError('Framebuffer pixel mismatch (see app log)')
            if not args.profile and not args.full and 'result=equal' not in text:
                raise AssertionError('Only full fallbacks observed; no damage pixels were verified')
            print(f'Total: {text.count("result=equal")} comparable partial frames, {text.count("result=full")} full fallbacks', flush=True)
        finally:
            display.restore()
            if app and app.poll() is None:
                app.terminate()
                try:
                    app.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    app.kill()
                    app.wait()
            cli('daemon', 'stop', check=False)
            if args.log:
                args.log.write_text((log.read_text(errors='replace') if log.exists() else '') + '\n' + (case / 'daemon-cli.log').read_text(errors='replace'))


def point(value):
    x, y = value.split(',')
    return int(x), int(y)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--app', type=Path, required=True)
    parser.add_argument('--cli', type=Path, required=True)
    parser.add_argument('--pill', type=point, default=(385, 671))
    parser.add_argument('--child', type=point, default=(430, 868))
    parser.add_argument('--wide-pill', type=point, default=(385, 688))
    parser.add_argument('--compact-row', type=point, default=(175, 437))
    parser.add_argument('--log', type=Path)
    parser.add_argument('--screenshots', type=Path)
    parser.add_argument('--profile', action='store_true', help='Do not read back; repeat the sheet transition for timings')
    parser.add_argument('--full', action='store_true', help='Force full redraw (timing control)')
    args = parser.parse_args()
    args.app, args.cli = args.app.resolve(), args.cli.resolve()
    if args.screenshots:
        args.screenshots.mkdir(parents=True, exist_ok=True)
    bus = os.environ.get('DBUS_SESSION_BUS_ADDRESS')
    # dbus-run-session sets this sentinel; never enumerate/terminate processes
    # on the user's desktop bus, even if a wrapper passes an address by mistake.
    if not bus or not os.environ.get('PASEO_ORACLE_PRIVATE_BUS') or bus.startswith('unix:path=/run/user/'):
        parser.error('Use dbus-run-session -- env PASEO_ORACLE_PRIVATE_BUS=1 ...')
    try:
        with tempfile.TemporaryDirectory(prefix='damage-oracle-', dir=ROOT / 'target') as case:
            run(args, Path(case))
    finally:
        cleanup_bus(bus)


if __name__ == '__main__':
    main()
