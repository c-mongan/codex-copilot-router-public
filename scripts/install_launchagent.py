#!/usr/bin/env python3
"""Prepare a private user LaunchAgent; only --load registers it with launchd.

--render prints the plist without inspecting or changing the filesystem.
The default install writes it for the next GUI login, without starting a process.
--load bootstraps only an absent service on a free port. Existing matching services
are left running; a changed plist requires a separately controlled restart.
"""

import argparse
import json
import os
from pathlib import Path
import plistlib
import re
import socket
import stat
import subprocess
import sys
import tempfile
from xml.parsers.expat import ExpatError


LABEL = "local.codex-copilot-router"
PORT = 60001


class InstallError(Exception):
    """An unsafe path or service state prevented installation."""


def _home_path(home):
    home = Path(home)
    if not home.is_absolute() or ".." in home.parts or any(c in str(home) for c in "\n\r\0"):
        raise InstallError("Home must be an absolute, physical path without parent traversal.")
    return home


def _document(home):
    root = home / ".local/share/codex-code-router"
    return {
        "Label": LABEL,
        "ProgramArguments": [str(home / ".local/bin/codex-copilot-router"), "serve"],
        "WorkingDirectory": str(root),
        "RunAtLoad": True,
        "KeepAlive": {"SuccessfulExit": False},
        "ThrottleInterval": 10,
        "ProcessType": "Background",
        "StandardOutPath": str(root / "logs/stdout.log"),
        "StandardErrorPath": str(root / "logs/stderr.log"),
    }


def render(home):
    """Return plist bytes without filesystem, credential, or launchd access."""
    return plistlib.dumps(_document(_home_path(home)), sort_keys=True)


def _directory(path, home, uid, *, mode=None, missing=False):
    """Inspect every physical component; never repair an existing directory."""
    for component in reversed((path, *path.parents)):
        try:
            info = component.lstat()
        except FileNotFoundError:
            if missing:
                return False
            raise InstallError("Required directory is missing: " + str(component)) from None
        if not stat.S_ISDIR(info.st_mode):
            raise InstallError("Directory path is not physical: " + str(component))
        within_home = component == home or home in component.parents
        if info.st_uid not in ({uid} if within_home else {0, uid}):
            raise InstallError("Directory has an unexpected owner: " + str(component))
        # Root-owned sticky ancestors outside HOME cannot let other users replace
        # this user's existing directory (for example under /private/tmp).
        trusted_sticky = not within_home and info.st_uid == 0 and info.st_mode & stat.S_ISVTX
        if info.st_mode & 0o022 and not trusted_sticky:
            raise InstallError("Directory is writable by another user: " + str(component))
        if component == path and mode is not None and stat.S_IMODE(info.st_mode) != mode:
            raise InstallError("Directory must already have mode %04o: %s" % (mode, component))
    return True


def _file(path, home, uid, *, executable=False, missing=False):
    _directory(path.parent, home, uid, missing=missing)
    try:
        info = path.lstat()
    except FileNotFoundError:
        if missing:
            return None
        raise InstallError("Required file is missing: " + str(path)) from None
    if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or info.st_uid != uid:
        raise InstallError("File must be an owned, single-linked regular file: " + str(path))
    mode = stat.S_IMODE(info.st_mode)
    if executable:
        if mode & 0o7022 or mode & 0o500 != 0o500 or info.st_size == 0:
            raise InstallError("Wrapper must be readable/executable and not writable by others: " + str(path))
    elif mode != 0o600:
        raise InstallError("File must already have mode 0600: " + str(path))
    return info


def _read_plist(path, home, uid, expected):
    if _file(path, home, uid, missing=True) is None:
        return None
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(fd, "rb") as stream:
        data = stream.read()
    try:
        existing = plistlib.loads(data)
    except (ValueError, TypeError, OverflowError, ExpatError, plistlib.InvalidFileException):
        raise InstallError("Existing LaunchAgent is not a valid owned plist.") from None
    identity = ("Label", "ProgramArguments", "WorkingDirectory", "StandardOutPath", "StandardErrorPath")
    if (not isinstance(existing, dict) or set(existing) - set(expected)
            or any(existing.get(key) != expected[key] for key in identity)):
        raise InstallError("Existing LaunchAgent belongs to another configuration; refusing to overwrite it.")
    return data


def _ensure_directory(path, home, uid, *, mode=None):
    for component in reversed((path, *path.parents)):
        try:
            component.mkdir(mode=0o700)
        except FileExistsError:
            pass
        _directory(component, home, uid)
    _directory(path, home, uid, mode=mode)


def _ensure_log(path, home, uid):
    try:
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    except FileExistsError:
        _file(path, home, uid)
    else:
        os.close(fd)
        _file(path, home, uid)


def _private_temporary(parent, data, suffix):
    fd, name = tempfile.mkstemp(prefix=LABEL + ".", suffix=suffix, dir=parent)
    path = Path(name)
    try:
        with os.fdopen(fd, "wb") as stream:
            os.fchmod(stream.fileno(), 0o600)
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
    except BaseException:
        path.unlink()
        raise
    return path


def _sync_directory(path):
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def _port_available():
    # Do not connect to, stop, or claim ownership of an existing listener.
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
        try:
            probe.bind(("127.0.0.1", PORT))
        except OSError:
            return False
    return True


def _registered(run, uid, path, expected):
    result = run(
        ["/bin/launchctl", "print", "gui/%s/%s" % (uid, LABEL)],
        capture_output=True, text=True, check=False,
    )
    if result.returncode:
        if 'Could not find service "' + LABEL + '"' in result.stderr:
            return False
        raise InstallError("Cannot safely inspect the GUI LaunchAgent; nothing was loaded.")
    # launchctl print is a diagnostic format, not an API. Unknown output fails
    # closed rather than treating an unrecognised registration as ours.
    fields = {}
    for key in ("path", "program"):
        matches = re.findall(r"^\s*" + key + r" = (.+)$", result.stdout, re.MULTILINE)
        if len(matches) == 1:
            fields[key] = matches[0]
    arguments = re.findall(r"^\s*arguments = \{\n(.*?)^\s*\}", result.stdout, re.MULTILINE | re.DOTALL)
    argv = [line.strip() for line in arguments[0].splitlines()] if len(arguments) == 1 else None
    if (fields.get("path") != str(path)
            or fields.get("program") != expected["ProgramArguments"][0]
            or argv != expected["ProgramArguments"]):
        raise InstallError("LaunchAgent label is already registered to an unverified configuration; leaving it alone.")
    return True


def install(home, *, load=False, uid=None, platform=None, run=None, port_available=None):
    """Install using explicit paths and injectable launchd/port boundaries."""
    uid = os.getuid() if uid is None else uid
    if (sys.platform if platform is None else platform) != "darwin":
        raise InstallError("Installation requires macOS; --render is available on any platform.")
    if uid == 0 or os.geteuid() == 0:
        raise InstallError("Run as the regular GUI user, never with sudo or as root.")
    home = _home_path(home)
    root = home / ".local/share/codex-code-router"
    path = home / "Library/LaunchAgents" / (LABEL + ".plist")
    expected = _document(home)
    data = render(home)
    run = subprocess.run if run is None else run
    port_available = _port_available if port_available is None else port_available

    # Preflight every existing destination before creating directories or logs.
    _directory(home, home, uid)
    _directory(root, home, uid, mode=0o700)
    _file(Path(expected["ProgramArguments"][0]), home, uid, executable=True)
    key = _file(root / "client-token", home, uid)
    if key.st_size == 0:
        raise InstallError("The private local client key must not be empty.")
    _directory(path.parent, home, uid, missing=True)
    _directory(root / "logs", home, uid, mode=0o700, missing=True)
    logs = [Path(expected[key]) for key in ("StandardOutPath", "StandardErrorPath")]
    for log in logs:
        _file(log, home, uid, missing=True)
    previous = _read_plist(path, home, uid, expected)
    changed = previous != data
    registered = _registered(run, uid, path, expected) if load else False
    if load and not registered and not port_available():
        raise InstallError("Port 60001 is occupied or unavailable; transfer supervision before using --load.")

    _ensure_directory(path.parent, home, uid)
    _ensure_directory(root / "logs", home, uid, mode=0o700)
    for log in logs:
        _ensure_log(log, home, uid)
    backup = None
    if changed:
        if _read_plist(path, home, uid, expected) != previous:
            raise InstallError("LaunchAgent changed during preparation; refusing to overwrite it.")
        if previous is not None:
            backup = _private_temporary(path.parent, previous, ".backup")
            _sync_directory(path.parent)
        temporary = _private_temporary(path.parent, data, ".tmp")
        try:
            os.replace(temporary, path)
            _sync_directory(path.parent)
        finally:
            if temporary.exists():
                temporary.unlink()

    state = "not-requested"
    if load:
        if registered:
            state = "restart-required" if changed else "already-registered"
        else:
            result = run(
                ["/bin/launchctl", "bootstrap", "gui/%s" % uid, str(path)],
                capture_output=True, text=True, check=False,
            )
            if result.returncode:
                raise InstallError("Plist installed, but bootstrap failed; no existing service was stopped.")
            state = "bootstrapped"
    return {"plist": str(path), "changed": changed, "backup": str(backup) if backup else None, "load_state": state}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--render", action="store_true", help="print plist without filesystem or launchd changes")
    mode.add_argument("--load", action="store_true", help="install and bootstrap only an absent service on a free port")
    args = parser.parse_args(argv)
    try:
        if args.render:
            sys.stdout.buffer.write(render(Path.home()))
        else:
            print(json.dumps(install(Path.home(), load=args.load), separators=(",", ":")))
    except (InstallError, OSError) as error:
        print("LaunchAgent installation refused: " + str(error), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
