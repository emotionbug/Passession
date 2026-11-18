#!/usr/bin/env python3
"""Compare Makepad's partial glyph uploads with full atlas uploads on the same X11 EGL renderer."""

import argparse
import array
import ctypes as c
import os
from pathlib import Path
import re
import subprocess
import tempfile
import time

TITLE = "Makepad glyph upload probe"
FULL_PAGE = 16 * 1024 * 1024
TRACE = re.compile(r"\btexture_uploads=(\d+) texture_upload_bytes=(\d+)\b")


def capture():
    window = subprocess.check_output(["xwininfo", "-name", TITLE], text=True)
    drawable = int(re.search(r"Window id: (0x[0-9a-f]+)", window)[1], 16)
    width = int(re.search(r"Width: (\d+)", window)[1])
    height = int(re.search(r"Height: (\d+)", window)[1])
    lib = c.CDLL("libX11.so.6")
    lib.XOpenDisplay.argtypes, lib.XOpenDisplay.restype = (c.c_char_p,), c.c_void_p
    lib.XGetImage.argtypes = (c.c_void_p, c.c_ulong, c.c_int, c.c_int,
                              c.c_uint, c.c_uint, c.c_ulong, c.c_int)
    lib.XGetImage.restype = c.c_void_p
    lib.XGetPixel.argtypes = (c.c_void_p, c.c_int, c.c_int)
    lib.XGetPixel.restype = c.c_ulong
    lib.XDestroyImage.argtypes = (c.c_void_p,)
    lib.XCloseDisplay.argtypes = (c.c_void_p,)
    display = lib.XOpenDisplay(None)
    assert display, "no X11 display"
    try:
        image = lib.XGetImage(display, drawable, 0, 0, width, height, -1, 2)  # ZPixmap
        assert image, "could not read the probe window"
        try:
            # Do not compare alpha/padding in the X11 drawable.
            pixels = array.array("I", (lib.XGetPixel(image, x, y) & 0xFFFFFF
                                        for y in range(height) for x in range(width)))
            return width, height, pixels
        finally:
            lib.XDestroyImage(image)
    finally:
        lib.XCloseDisplay(display)


def run(binary, emoji, profile, reference):
    with tempfile.TemporaryDirectory(prefix="glyph-probe-", dir="target") as temporary:
        state = Path(temporary).resolve()
        env = os.environ.copy()
        for variable in ("PASEO_HOME", "XDG_DATA_HOME", "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_STATE_HOME"):
            directory = state / variable.lower()
            directory.mkdir()
            env[variable] = str(directory)
        desktop_data = Path(env["XDG_DATA_HOME"]) / "sh.paseo.sessions.desktop"
        desktop_data.mkdir()
        (desktop_data / "desktop-settings.json").write_text('{"daemon":{"startDaemonIfNotRunning":false}}')
        env.pop("SESSION_MANAGER", None)
        env["PASEO_FRAME_TRACE"] = "1"
        env["GLYPH_PROBE_EMOJI"] = str(emoji)
        env.pop("GLYPH_PROBE_FULL", None)
        if reference:
            env["GLYPH_PROBE_FULL"] = "1"
        if profile:
            env["MESA_GLES_VERSION_OVERRIDE"] = profile

        log = state / "probe.log"
        with log.open("w") as stderr, log.open() as stream:
            process = subprocess.Popen(["dbus-run-session", "--", str(binary)], env=env, stdout=stderr, stderr=stderr)
            try:
                frames = {}
                stage = None
                deadline = time.monotonic() + 12
                while len(frames) != 3 and time.monotonic() < deadline:
                    line = stream.readline()
                    if not line:
                        if process.poll() is not None:
                            break
                        time.sleep(0.02)
                        continue
                    if line.startswith("GLYPH_PROBE_STAGE="):
                        stage = int(line.split("=", 1)[1])
                    if match := TRACE.search(line):
                        uploads, count = map(int, match.groups())
                        if uploads:
                            assert stage not in frames, "unexpected further upload at the same stage"
                            frames[stage] = (capture(), uploads, count)
                assert set(frames) == {0, 1, 2}, "missing probe stages"
                assert process.wait(timeout=4) == 0, "probe did not exit normally"
                return frames, log.read_text()
            except Exception:
                print(log.read_text(), end="")
                raise
            finally:
                if process.poll() is None:
                    # dbus-run-session cleans up its private daemon on child exit.
                    # Allow the fixture's short timeout to quit it first.
                    try:
                        process.wait(timeout=6)
                    except subprocess.TimeoutExpired:
                        process.terminate()
                        process.wait(timeout=3)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--emoji-font", type=Path, default=Path("/usr/share/fonts/truetype/noto/NotoColorEmoji.ttf"))
    parser.add_argument("--mesa-gles", choices=("2.0", "3.2"))
    args = parser.parse_args()
    sets = [run(args.binary.resolve(), args.emoji_font.resolve(), args.mesa_gles, reference)
            for reference in (True, False)]
    (reference, _), (actual, log) = sets
    assert reference[0][1:] == actual[0][1:] == (2, 2 * FULL_PAGE), "first upload must populate both pages"
    assert reference[1][1:] == reference[2][1:] == (1, FULL_PAGE)
    assert actual[1][1] == actual[2][1] == 1
    assert 0 < actual[1][2] < 16 * 1024, "new grayscale glyph did not use a small region"
    assert 0 < actual[2][2] < 256 * 1024, "new color glyph did not use a small region"
    for stage in range(3):
        assert actual[stage][0] == reference[stage][0], f"pixel mismatch in stage {stage}"
    frames = [actual[stage][0][2] for stage in range(3)]
    assert len(set(frames[0])) > 10, "initial text and emoji must be visible"
    for stage, label in [(1, "grayscale Q"), (2, "color 🤓")]:
        changed = sum(a != b for a, b in zip(frames[stage - 1], frames[stage], strict=True))
        assert changed > 30, f"new glyph not visible: {label}"
        print(f"{label}: {changed} changed pixels, full {reference[stage][2]:,} B, partial {actual[stage][2]:,} B; exact pixel match")
    for line in log.splitlines():
        if "GPU renderer:" in line or "texture_uploads=1 " in line:
            print(line)


if __name__ == "__main__":
    main()
