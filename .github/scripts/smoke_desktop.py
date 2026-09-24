#!/usr/bin/env python3
"""Installs the desktop app from its installers and checks that it starts.

    smoke_desktop.py DIR VERSION

For every installer for this OS under DIR: install it as a user would, start the
installed app, and check that within INSTALL_TIMEOUT seconds it
  - answers its health check as stream-delay VERSION,
  - serves the dashboard, with the web UI included,
  - accepts RTMP connections (OBS's side),
  - opened the dashboard window,
then stop it. Linux: the .deb (with apt, which also installs what it depends
on) and the AppImage. Windows: the setup .exe and the .msi. macOS: the app in
the .dmg, and the one in the updater bundle (.app.tar.gz).

Linux needs a display and a session bus: run it under xvfb-run and
dbus-run-session. Only the Python standard library is used.
"""

import json
import os
import signal
import socket
import subprocess
import sys
import tempfile
import time
import urllib.request
from pathlib import Path

START_TIMEOUT = 90
# The app moves to the next of these when a port is taken (apps/desktop main.rs).
API_PORTS = [7788, 17788, 27788]
INGEST_PORTS = [1935, 19350, 29350]


class Failed(Exception):
    pass


def log_file() -> Path:
    """Where the app writes its log (next to config.toml, see Config::default_path)."""
    home = Path.home()
    if sys.platform == "win32":
        base = Path(os.environ["APPDATA"]) / "stream-delay" / "stream-delay" / "config"
    elif sys.platform == "darwin":
        base = home / "Library" / "Application Support" / "dev.stream-delay.stream-delay"
    else:
        base = Path(os.environ.get("XDG_CONFIG_HOME") or home / ".config") / "stream-delay"
    return base / "logs" / "stream-delay.log"


def get(url: str):
    with urllib.request.urlopen(url, timeout=3) as r:
        return r.status, r.read().decode("utf-8", "replace")


def health(port: int):
    try:
        status, body = get(f"http://127.0.0.1:{port}/healthz")
        return json.loads(body) if status == 200 else None
    except (OSError, ValueError):
        return None


def rtmp_answers(port: int) -> bool:
    """True if an RTMP server on `port` answers the start of a handshake."""
    try:
        with socket.create_connection(("127.0.0.1", port), timeout=3) as s:
            s.settimeout(3)
            # C0 (version 3) and C1 (time, zero, 1528 bytes of anything).
            s.sendall(b"\x03" + bytes(8) + os.urandom(1528))
            return s.recv(1) == b"\x03"
    except OSError:
        return False


def start(command, env=None) -> subprocess.Popen:
    print(f"starting {' '.join(map(str, command))}", flush=True)
    kwargs = {}
    if sys.platform == "win32":
        kwargs["creationflags"] = subprocess.CREATE_NEW_PROCESS_GROUP
    else:
        kwargs["start_new_session"] = True
    return subprocess.Popen(
        [str(c) for c in command],
        env={**os.environ, **(env or {})},
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        **kwargs,
    )


def stop(proc: subprocess.Popen) -> None:
    """Stops the app and everything it started (WebView processes, an AppImage's
    extracted copy)."""
    if proc.poll() is None:
        if sys.platform == "win32":
            subprocess.run(["taskkill", "/T", "/F", "/PID", str(proc.pid)], capture_output=True)
        else:
            try:
                os.killpg(proc.pid, signal.SIGTERM)
                proc.wait(10)
            except subprocess.TimeoutExpired:
                os.killpg(proc.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
    try:
        proc.wait(20)
    except subprocess.TimeoutExpired:
        pass
    # Its ports must be free before the next one starts.
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline and any(health(p) for p in API_PORTS):
        time.sleep(0.5)


def check_running(proc: subprocess.Popen, version: str) -> None:
    deadline = time.monotonic() + START_TIMEOUT
    port = None
    while port is None:
        if proc.poll() is not None:
            raise Failed(f"the app exited with code {proc.returncode}")
        if time.monotonic() > deadline:
            raise Failed(f"no health check answered within {START_TIMEOUT} s")
        port = next((p for p in API_PORTS if health(p)), None)
        time.sleep(0.5)
    h = health(port)
    if h.get("app") != "stream-delay" or h.get("version") != version:
        raise Failed(f"the health check on port {port} says {h}, not stream-delay {version}")
    print(f"ok  health check on port {port}: {h}")

    status, page = get(f"http://127.0.0.1:{port}/")
    if status != 200 or "<html" not in page.lower() or "was not included in this build" in page:
        raise Failed(f"the dashboard page is not the web UI (HTTP {status})")
    print("ok  dashboard served, web UI included")

    ingest = next((p for p in INGEST_PORTS if rtmp_answers(p)), None)
    if ingest is None:
        raise Failed(f"no RTMP server answers on {INGEST_PORTS}")
    print(f"ok  RTMP ingest answers on port {ingest}")

    # The window opens once the core has started; give it a moment.
    log = log_file()
    for _ in range(40):
        text = log.read_text("utf-8", "replace") if log.is_file() else ""
        if "dashboard opened" in text:
            break
        time.sleep(0.5)
    else:
        raise Failed(f"the dashboard window did not open (no 'dashboard opened' in {log})")
    if "panicked" in text:
        raise Failed("the app panicked")
    print("ok  dashboard window opened")
    if proc.poll() is not None:
        raise Failed(f"the app exited with code {proc.returncode}")


def run_app(name: str, command, version: str, env=None) -> None:
    print(f"\n== {name}", flush=True)
    proc = start(command, env)
    try:
        check_running(proc, version)
    except Failed:
        log = log_file()
        if log.is_file():
            print(f"--- {log}\n{log.read_text('utf-8', 'replace')}---")
        raise
    finally:
        stop(proc)


def one(root: Path, pattern: str, recursive: bool = True) -> Path:
    found = sorted(root.rglob(pattern) if recursive else root.glob(pattern))
    if len(found) != 1:
        raise Failed(f"expected one {pattern} under {root}, found {[str(f) for f in found]}")
    return found[0]


def installed_exe(folder: Path) -> Path:
    exes = [p for p in folder.glob("*.exe") if not p.name.lower().startswith("uninstall")]
    if len(exes) != 1:
        raise Failed(f"expected the app in {folder}, found {[p.name for p in exes]}")
    return exes[0]


def app_binary(app: Path) -> Path:
    binaries = list((app / "Contents" / "MacOS").iterdir())
    if len(binaries) != 1:
        raise Failed(f"expected one program in {app}, found {[b.name for b in binaries]}")
    return binaries[0]


def linux(root: Path, version: str) -> None:
    # WebKitGTK cannot use the GPU under Xvfb.
    env = {"WEBKIT_DISABLE_COMPOSITING_MODE": "1", "WEBKIT_DISABLE_DMABUF_RENDERER": "1"}
    deb = one(root, f"*_{version}_amd64.deb")
    print(f"installing {deb.name}", flush=True)
    # Also installs what the package says it needs, as on a user's computer.
    subprocess.run(["sudo", "apt-get", "install", "-y", "--no-install-recommends", str(deb.resolve())], check=True)
    run_app(deb.name, ["stream-delay"], version, env)
    subprocess.run(["sudo", "apt-get", "remove", "-y", "stream-delay"], check=True)

    appimage = one(root, f"*_{version}_amd64.AppImage")
    appimage.chmod(0o755)
    # Without FUSE, which runners do not have; users' systems mount it instead.
    run_app(appimage.name, [appimage], version, {**env, "APPIMAGE_EXTRACT_AND_RUN": "1"})


def windows(root: Path, version: str) -> None:
    setup = one(root, f"*_{version}_x64-setup.exe")
    print(f"installing {setup.name}", flush=True)
    subprocess.run([str(setup), "/S"], check=True)
    run_app(setup.name, [installed_exe(Path(os.environ["LOCALAPPDATA"]) / "stream-delay")], version)

    msi = one(root, f"*_{version}_x64_en-US.msi")
    print(f"installing {msi.name}", flush=True)
    r = subprocess.run(["msiexec", "/i", str(msi), "/qn", "/norestart"])
    if r.returncode not in (0, 3010):
        raise Failed(f"msiexec failed with code {r.returncode}")
    run_app(msi.name, [installed_exe(Path(os.environ["ProgramFiles"]) / "stream-delay")], version)


def macos(root: Path, version: str) -> None:
    work = Path(tempfile.mkdtemp())
    dmg = one(root, f"*_{version}_universal.dmg")
    mount = work / "mnt"
    mount.mkdir()
    subprocess.run(["hdiutil", "attach", "-nobrowse", "-readonly", "-mountpoint", str(mount), str(dmg)], check=True)
    try:
        # Not recursive: the image also holds a link to /Applications.
        app = one(mount, "*.app", recursive=False)
        # Dragged to Applications, as the disk image asks.
        installed = work / "Applications" / app.name
        installed.parent.mkdir()
        subprocess.run(["ditto", str(app), str(installed)], check=True)
    finally:
        subprocess.run(["hdiutil", "detach", str(mount)], check=True)
    run_app(dmg.name, [app_binary(installed)], version)

    bundle = one(root, "*.app.tar.gz")
    update = work / "update"
    update.mkdir()
    subprocess.run(["tar", "xzf", str(bundle), "-C", str(update)], check=True)
    run_app(bundle.name, [app_binary(one(update, "*.app", recursive=False))], version)


def main(argv) -> int:
    if len(argv) != 3:
        print(__doc__, file=sys.stderr)
        return 2
    root, version = Path(argv[1]), argv[2].removeprefix("v")
    try:
        if sys.platform == "win32":
            windows(root, version)
        elif sys.platform == "darwin":
            macos(root, version)
        else:
            linux(root, version)
    except (Failed, subprocess.CalledProcessError) as e:
        print(f"::error::{e}")
        return 1
    print("\nall installers work")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
