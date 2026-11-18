import ctypes as x11
import json, os, pathlib, re, subprocess, sys, time
import dbus, dbus.service, dbus.mainloop.glib
import gi
gi.require_version('Gdk', '3.0')
gi.require_version('GdkX11', '3.0')
from gi.repository import GLib, Gdk, GdkX11
Gdk.init([])

dbus.mainloop.glib.DBusGMainLoop(set_as_default=True)
bus = dbus.SessionBus()
proof = pathlib.Path(os.environ['PASEO_DESKTOP_SMOKE_PROOF'])
APP_WINDOW_CLASS = 'paseo-native-smoke'
logout = os.environ.get('PASEO_SMOKE_LOGOUT') == '1'
clients = []
screenshots = {
    'label-only-wait-command-sheet': 1,
    'primary-close-and-quit-cancellation': 2,
    'primary-reuse-and-secondary-teardown': 1,
    'multiwindow-close-retains-daemon': 1,
}
if not logout:
    screenshots['guarded-last-window-close'] = 1
def record(phase, **values):
    with (proof / 'bus.jsonl').open('a') as stream: stream.write(json.dumps({'phase': phase, **values}, default=str) + '\n')
class Client(dbus.service.Object):
    @dbus.service.method('org.gnome.SessionManager.ClientPrivate', in_signature='bs', out_signature='')
    def EndSessionResponse(self, okay, reason): record('session-response', okay=bool(okay), reason=str(reason))
    @dbus.service.signal('org.gnome.SessionManager.ClientPrivate', signature='u')
    def EndSession(self, flags): pass
class Session(dbus.service.Object):
    @dbus.service.method('org.gnome.SessionManager', in_signature='ss', out_signature='o')
    def RegisterClient(self, app, startup):
        path = '/org/gnome/SessionManager/Client' + str(len(clients) + 1)
        clients.append(Client(bus, path)); record('session-register', app=str(app), path=path)
        return dbus.ObjectPath(path)
    @dbus.service.method('org.gnome.SessionManager', in_signature='o', out_signature='')
    def UnregisterClient(self, path): record('session-unregister', path=str(path))
class Notifications(dbus.service.Object):
    @dbus.service.method('org.freedesktop.Notifications', in_signature='', out_signature='ssss')
    def GetServerInformation(self): return ('Paseo smoke desktop', 'Paseo', '1', '1.2')
    @dbus.service.method('org.freedesktop.Notifications', in_signature='', out_signature='as')
    def GetCapabilities(self): return ['actions', 'body']
class LoginManager(dbus.service.Object):
    @dbus.service.signal('org.freedesktop.login1.Manager', signature='b')
    def PrepareForShutdown(self, active): pass
names = [
    dbus.service.BusName(name, bus)
    for name in ['org.gnome.SessionManager', 'org.freedesktop.Notifications', 'org.freedesktop.login1']
]
services = [
    Session(bus, '/org/gnome/SessionManager'),
    Notifications(bus, '/org/freedesktop/Notifications'),
    LoginManager(bus, '/org/freedesktop/login1'),
]

def app_windows():
    # Openbox updates this EWMH list when a client is mapped or withdrawn.
    listing = subprocess.check_output(['xprop', '-root', '_NET_CLIENT_LIST'], text=True, timeout=3)
    windows = []
    for window in re.findall(r'0x[0-9a-f]+', listing):
        # A window may have already been withdrawn/destroyed since the root query.
        props = subprocess.run(['xprop', '-id', window, 'WM_CLASS'], capture_output=True, text=True, timeout=3)
        if f'"{APP_WINDOW_CLASS}"' in props.stdout:
            windows.append(window)
    return windows

class ClientMessage(x11.Structure):
    _fields_ = [
        ('type', x11.c_int), ('serial', x11.c_ulong), ('send_event', x11.c_int),
        ('display', x11.c_void_p), ('window', x11.c_ulong), ('message_type', x11.c_ulong),
        ('format', x11.c_int), ('data', x11.c_long * 5),
    ]

class XEvent(x11.Union):
    _fields_ = [('client', ClientMessage), ('pad', x11.c_long * 24)]

def close_window(window):
    protocols = subprocess.check_output(['xprop', '-id', window, 'WM_PROTOCOLS'], text=True, timeout=3)
    if 'WM_DELETE_WINDOW' not in protocols:
        raise RuntimeError(f'Window {window} does not support WM_DELETE_WINDOW')
    # Send the EWMH request to Openbox, not WM_DELETE_WINDOW directly to the
    # app. The manager converts it to a real WM_DELETE_WINDOW client message.
    lib = x11.CDLL('libX11.so.6')
    lib.XOpenDisplay.argtypes = [x11.c_char_p]
    lib.XOpenDisplay.restype = x11.c_void_p
    lib.XDefaultRootWindow.argtypes = [x11.c_void_p]
    lib.XDefaultRootWindow.restype = x11.c_ulong
    lib.XInternAtom.argtypes = [x11.c_void_p, x11.c_char_p, x11.c_int]
    lib.XInternAtom.restype = x11.c_ulong
    lib.XSendEvent.argtypes = [x11.c_void_p, x11.c_ulong, x11.c_int, x11.c_long, x11.POINTER(XEvent)]
    lib.XCloseDisplay.argtypes = [x11.c_void_p]
    display = lib.XOpenDisplay(None)
    if not display:
        raise RuntimeError('Unable to open the private Xvfb display')
    try:
        atom = lib.XInternAtom(display, b'_NET_CLOSE_WINDOW', True)
        if not atom:
            raise RuntimeError('The private Xvfb display has no _NET_CLOSE_WINDOW atom')
        event = XEvent(client=ClientMessage(
            type=33, display=display, window=int(window, 16), message_type=atom, format=32,
            data=(x11.c_long * 5)(0, 2),  # CurrentTime, source is a pager/tool.
        ))
        mask = (1 << 19) | (1 << 20)  # SubstructureNotifyMask | SubstructureRedirectMask
        if not lib.XSendEvent(display, lib.XDefaultRootWindow(display), False, mask, x11.byref(event)):
            raise RuntimeError(f'Could not send _NET_CLOSE_WINDOW for {window}')
    finally:
        lib.XCloseDisplay(display)

loop = GLib.MainLoop()
# A bare Xvfb server cannot deliver an EWMH close request or report managed clients.
wm_log = (proof / 'window-manager.log').open('w')
wm = subprocess.Popen([os.environ['PASEO_SMOKE_OPENBOX'], '--sm-disable'], stdout=wm_log, stderr=subprocess.STDOUT)
root = Gdk.get_default_root_window()
try:
    deadline = time.monotonic() + 10
    while GdkX11.X11Screen.get_window_manager_name(Gdk.Screen.get_default()) != 'Openbox':
        if wm.poll() is not None or time.monotonic() >= deadline:
            raise RuntimeError('Private Openbox did not start; see window-manager.log')
        while GLib.MainContext.default().pending():
            GLib.MainContext.default().iteration(False)
        time.sleep(0.05)
    record('window-manager-ready', executable=os.environ['PASEO_SMOKE_OPENBOX'])
except BaseException:
    if wm.poll() is None: wm.terminate()
    wm.wait(timeout=5); wm_log.close()
    raise
log = (proof / 'app.log').open('w')
# Keep the app's login1 watcher on the private bus as well as its GNOME client.
app_env = dict(os.environ, DBUS_SYSTEM_BUS_ADDRESS=os.environ['DBUS_SESSION_BUS_ADDRESS'], RESOURCE_NAME=APP_WINDOW_CLASS)
app = subprocess.Popen([sys.argv[1], str(proof)], env=app_env, stdout=log, stderr=subprocess.STDOUT)
start = time.monotonic()
last_command = None
failed = False
captured = set()
guarded_window = None

def fail(message):
    global failed
    failed = True; record('FAIL', error=message)
    if app.poll() is None: app.terminate()
    loop.quit()

def tick():
    global last_command, guarded_window
    try:
        events_path = proof / 'events.jsonl'
        phases = {json.loads(row)['phase'] for row in events_path.read_text().splitlines()} if events_path.exists() else set()
        if app.poll() is not None:
            expected = 'expect-session-logout-quit' if logout else 'expect-last-window-close-quit'
            expected_command = 'logout' if logout else 'close-last-window'
            if app.returncode != 0 or expected not in phases or last_command != expected_command or 'FAIL' in phases:
                fail(f'Desktop exited unexpectedly: {app.returncode} without completed {expected_command}'); return False
            record('native-app-exited', exitCode=app.returncode, expected=expected)
            loop.quit(); return False
        for phase, window_count in screenshots.items():
            if phase in phases and phase not in captured:
                windows = app_windows()
                if len(windows) != window_count:
                    continue
                pixels = Gdk.pixbuf_get_from_window(root, 0, 0, root.get_width(), root.get_height())
                if pixels is None:
                    raise RuntimeError('Private Xvfb screenshot unavailable')
                pixels.savev(str(proof / (phase + '.png')), 'png', [], [])
                record('screenshot-captured', screenshot=phase, windows=windows)
                captured.add(phase)
                (proof / ('captured-' + phase)).touch()
        if time.monotonic() - start > 150:
            fail('Native desktop scenario exceeded 150 seconds'); return False
        command_path = proof / 'session-command.json'
        if command_path.exists():
            command = json.loads(command_path.read_text())['action']
            if command != last_command:
                windows = app_windows()
                if len(windows) != 1:
                    raise RuntimeError(f'{command} requires exactly one managed user window, found {windows}')
                if command in {'close-last-window-with-guard', 'close-last-window'}:
                    window = windows[0]
                    if command == 'close-last-window-with-guard':
                        guarded_window = window
                    elif window != guarded_window:
                        raise RuntimeError(f'The last window changed during its guarded close: {windows}')
                    close_window(window)
                    record('window-manager-close-request', action=command, window=window)
                elif command == 'logout':
                    if not clients:
                        raise RuntimeError('App did not register with the private GNOME SessionManager')
                    record('emitting-session-end', clients=len(clients), window=windows[0])
                    for client in clients: client.EndSession(dbus.UInt32(0))
                else:
                    raise RuntimeError(f'Unexpected session command: {command!r}')
                last_command = command
    except Exception as error: fail(repr(error)); return False
    return True
GLib.timeout_add(100, tick)
try: loop.run()
finally:
    if app.poll() is None:
        app.terminate()
        try: app.wait(timeout=5)
        except subprocess.TimeoutExpired: app.kill(); app.wait()
    log.close()
    if wm.poll() is None: wm.terminate()
    try: wm.wait(timeout=5)
    except subprocess.TimeoutExpired: wm.kill(); wm.wait()
    wm_log.close()
sys.exit(1 if failed else 0)
