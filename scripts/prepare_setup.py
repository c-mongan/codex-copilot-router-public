#!/usr/bin/env python3
"""Prepare private local router files for manual Codex configuration.

This command executes the explicitly selected Codex binary only to extract its
bundled model metadata. It never edits Codex configuration, authenticates, starts
a service, or builds software.
"""

import argparse
import copy
import json
import os
from pathlib import Path
import re
import secrets
import stat
import sys
import tempfile


SCRIPT_DIRECTORY = Path(__file__).resolve().parent
if str(SCRIPT_DIRECTORY) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIRECTORY))

import install_launchagent
import refresh_catalog


KEY_PATTERN = re.compile(r"[0-9a-f]{64}\Z", re.ASCII)
RUNTIME_RELATIVE = Path(".local/share/codex-code-router")
LAUNCHER_RELATIVE = Path(".local/bin/codex-copilot-router")
MAX_LAUNCHER_BYTES = 1024 * 1024


class SetupError(Exception):
    """A safe terminal diagnostic for an invalid or incomplete setup."""


def _safe_absolute_path(value, label):
    path = Path(value)
    if not path.is_absolute() or ".." in path.parts or any(char in str(path) for char in "\n\r\0"):
        raise SetupError(label + " must be an absolute physical path without parent traversal")
    return path


def _directory(path, home, uid, *, mode=None, missing=False):
    try:
        return install_launchagent._directory(path, home, uid, mode=mode, missing=missing)
    except install_launchagent.InstallError as error:
        raise SetupError(str(error)) from None


def _private_file(path, home, uid, *, missing=False):
    try:
        return install_launchagent._file(path, home, uid, missing=missing)
    except install_launchagent.InstallError as error:
        raise SetupError(str(error)) from None


def _executable_file(path, home, uid, *, exact_private=False, label="executable"):
    try:
        info = install_launchagent._file(path, home, uid, executable=True)
    except install_launchagent.InstallError as error:
        raise SetupError(str(error)) from None
    if exact_private and stat.S_IMODE(info.st_mode) != 0o700:
        raise SetupError(label + " must already have mode 0700: " + str(path))
    return info


def _external_executable(path, uid):
    path = _safe_absolute_path(path, "Codex binary")
    try:
        path = path.resolve(strict=True)
        info = path.lstat()
    except (OSError, RuntimeError):
        raise SetupError("Codex binary is not an accessible installed file") from None
    if (
        not stat.S_ISREG(info.st_mode)
        or info.st_nlink != 1
        or info.st_uid not in (0, uid)
        or info.st_size == 0
        or stat.S_IMODE(info.st_mode) & 0o022
        or stat.S_IMODE(info.st_mode) & 0o500 != 0o500
    ):
        raise SetupError("Codex binary must be a trusted, single-linked executable regular file")
    return path


def _read_file(path, limit):
    try:
        return refresh_catalog.read_regular_file(path, limit)
    except refresh_catalog.CatalogError as error:
        raise SetupError(str(error)) from None


def _read_private(path, home, uid, limit):
    info = _private_file(path, home, uid)
    if info.st_size > limit:
        raise SetupError("existing setup file exceeds its size limit: " + str(path))
    return _read_file(path, limit)


def _read_key(path, home, uid):
    contents = _read_private(path, home, uid, 66)
    try:
        text = contents.decode("ascii")
    except UnicodeError:
        raise SetupError("existing local key has invalid encoding") from None
    if text.endswith("\n"):
        text = text[:-1]
    if not KEY_PATTERN.fullmatch(text):
        raise SetupError("existing local key must contain one 64-character lowercase hexadecimal value")
    return text


def _catalog_bytes(document):
    try:
        contents = (json.dumps(document, ensure_ascii=False, indent=2, allow_nan=False) + "\n").encode("utf-8")
    except (TypeError, ValueError, UnicodeError):
        raise SetupError("bundled model metadata is not valid JSON") from None
    if len(contents) > refresh_catalog.MAX_BYTES:
        raise SetupError("bundled model metadata exceeds its size limit")
    return contents


def _read_catalog(path, home, uid, *, native_only=False):
    contents = _read_private(path, home, uid, refresh_catalog.MAX_BYTES)
    try:
        document = refresh_catalog.parse_json(contents)
        models = refresh_catalog.catalog_index(document)
    except refresh_catalog.CatalogError as error:
        raise SetupError("existing catalog is invalid: " + str(path)) from error
    if native_only and any(slug.lower().startswith("copilot/") for slug in models):
        raise SetupError("native catalog contains a Copilot alias: " + str(path))
    return document


def _native_catalog(document):
    try:
        models = refresh_catalog.catalog_index(document)
    except refresh_catalog.CatalogError as error:
        raise SetupError("bundled model metadata is invalid") from error
    filtered = [
        copy.deepcopy(row)
        for slug, row in models.items()
        if not slug.lower().startswith("copilot/")
    ]
    if not filtered:
        raise SetupError("bundled model metadata contains no native models")
    result = copy.deepcopy(document)
    result["models"] = filtered
    return result


def _toml_string(value):
    return json.dumps(str(value), ensure_ascii=False)


def _config_fragment(root, key):
    desktop_catalog = root / "desktop-models.json"
    key_path = root / "client-token"
    return (
        "# Review and merge this private fragment into Codex configuration manually.\n"
        "model_provider = \"desktop-router\"\n"
        "model_catalog_json = " + _toml_string(desktop_catalog) + "\n\n"
        "[model_providers.desktop-router]\n"
        "name = \"Native OpenAI + GitHub Copilot (local)\"\n"
        "base_url = \"http://127.0.0.1:60001/combined/v1\"\n"
        "wire_api = \"responses\"\n"
        "requires_openai_auth = true\n"
        "supports_websockets = false\n"
        "http_headers = { \"x-codex-router-key\" = " + _toml_string(key) + " }\n\n"
        "[model_providers.copilot-proxy]\n"
        "name = \"GitHub Copilot (local hardened)\"\n"
        "base_url = \"http://127.0.0.1:60001/v1\"\n"
        "wire_api = \"responses\"\n"
        "supports_websockets = false\n"
        "request_max_retries = 1\n"
        "stream_max_retries = 2\n"
        "stream_idle_timeout_ms = 300000\n\n"
        "[model_providers.copilot-proxy.auth]\n"
        "command = \"/bin/cat\"\n"
        "args = [" + _toml_string(key_path) + "]\n"
        "timeout_ms = 5000\n"
        "refresh_interval_ms = 300000\n"
    ).encode("utf-8")


def _inspect_optional_file(path, home, uid, kind, expected=None):
    _directory(path.parent, home, uid, missing=True)
    try:
        path.lstat()
    except FileNotFoundError:
        return False
    except OSError:
        raise SetupError("cannot inspect setup destination: " + str(path)) from None
    if kind == "launcher":
        _executable_file(path, home, uid, exact_private=True, label="launcher")
        contents = _read_file(path, MAX_LAUNCHER_BYTES)
    else:
        contents = _read_private(path, home, uid, refresh_catalog.MAX_BYTES)
    if expected is not None and contents != expected:
        raise SetupError("existing " + kind + " conflicts with the requested setup")
    return True


def _ensure_directories(parents, home, uid):
    created = []
    ordered = sorted(set(parents), key=lambda path: len(path.parts))
    try:
        for path in ordered:
            missing = []
            current = path
            while not current.exists():
                try:
                    current.lstat()
                except FileNotFoundError:
                    missing.append(current)
                    current = current.parent
                else:
                    raise SetupError("directory path is not physical: " + str(current))
            _directory(current, home, uid)
            for component in reversed(missing):
                try:
                    component.mkdir(mode=0o700)
                except FileExistsError:
                    raise SetupError("setup destination changed while creating directories") from None
                created.append(component)
                _directory(component, home, uid, mode=0o700)
            _directory(path, home, uid)
        return created
    except BaseException:
        for directory in reversed(created):
            try:
                directory.rmdir()
            except OSError:
                pass
        raise


def _stage(parent, contents, mode):
    descriptor, name = tempfile.mkstemp(prefix=".prepare-setup-", dir=parent)
    path = Path(name)
    try:
        with os.fdopen(descriptor, "wb") as destination:
            os.fchmod(destination.fileno(), mode)
            destination.write(contents)
            destination.flush()
            os.fsync(destination.fileno())
        return path
    except BaseException:
        try:
            path.unlink()
        except OSError:
            pass
        raise


def _commit_new_files(files, home, uid):
    created_directories = _ensure_directories([path.parent for path, _, _ in files], home, uid)
    staged = []
    committed = []
    rollback_incomplete = False
    try:
        for path, contents, mode in files:
            staged.append((path, _stage(path.parent, contents, mode), mode))
        for path, temporary, mode in staged:
            try:
                os.link(temporary, path, follow_symlinks=False)
            except (FileExistsError, OSError):
                raise SetupError("setup destination changed before commit") from None
            committed.append(path)
            temporary.unlink()
            if mode == 0o700:
                _executable_file(path, home, uid, exact_private=True, label="launcher")
            else:
                _private_file(path, home, uid)
    except BaseException as error:
        for path in reversed(committed):
            try:
                path.unlink()
            except OSError:
                rollback_incomplete = True
        for _, temporary, _ in staged:
            try:
                temporary.unlink()
            except FileNotFoundError:
                pass
            except OSError:
                rollback_incomplete = True
        for directory in reversed(created_directories):
            try:
                directory.rmdir()
            except OSError:
                if directory.exists():
                    rollback_incomplete = True
        if rollback_incomplete:
            raise SetupError("setup failed and rollback was incomplete; inspect private setup destinations") from error
        raise SetupError("setup failed; newly created files were rolled back") from error


def prepare(home, codex, *, uid=None, load_models=None):
    """Prepare missing private files using explicit, injectable local boundaries."""
    uid = os.getuid() if uid is None else uid
    load_models = refresh_catalog.load_bundled_models if load_models is None else load_models
    try:
        home = install_launchagent._home_path(home)
    except install_launchagent.InstallError as error:
        raise SetupError(str(error)) from None
    _directory(home, home, uid)
    codex = _external_executable(codex, uid)

    root = home / RUNTIME_RELATIVE
    router_binary = root / "bin/codex-code-router"
    if not _directory(root, home, uid, mode=0o700, missing=True):
        raise SetupError("installed router runtime directory is missing")
    _executable_file(router_binary, home, uid, label="installed router binary")

    launcher_source = SCRIPT_DIRECTORY / "codex-copilot-router"
    launcher_contents = _read_file(launcher_source, MAX_LAUNCHER_BYTES)
    if not launcher_contents:
        raise SetupError("repository launcher is empty")

    key_path = root / "client-token"
    native_path = root / "native-models.json"
    desktop_path = root / "desktop-models.json"
    fragment_path = root / "codex-router.config.toml"
    launcher_path = home / LAUNCHER_RELATIVE

    key_exists = _inspect_optional_file(key_path, home, uid, "local key")
    key = _read_key(key_path, home, uid) if key_exists else None
    launcher_exists = _inspect_optional_file(
        launcher_path, home, uid, "launcher", expected=launcher_contents
    )
    native_exists = _inspect_optional_file(native_path, home, uid, "native catalog")
    native_document = _read_catalog(native_path, home, uid, native_only=True) if native_exists else None
    desktop_exists = _inspect_optional_file(desktop_path, home, uid, "desktop catalog")
    if desktop_exists:
        _read_catalog(desktop_path, home, uid)

    fragment_exists = _inspect_optional_file(fragment_path, home, uid, "configuration fragment")
    if fragment_exists:
        if key is None:
            raise SetupError("existing configuration fragment conflicts with the missing local key")
        expected_fragment = _config_fragment(root, key)
        if _read_private(fragment_path, home, uid, refresh_catalog.MAX_BYTES) != expected_fragment:
            raise SetupError("existing configuration fragment conflicts with the requested setup")

    if native_document is None:
        try:
            bundled = load_models(str(codex))
        except refresh_catalog.CatalogError:
            raise SetupError("could not extract bundled model metadata from the selected Codex binary") from None
        native_document = _native_catalog(bundled)

    if key is None:
        key = secrets.token_hex(32)
    fragment_contents = _config_fragment(root, key)
    native_contents = _catalog_bytes(native_document)

    files = []
    if not key_exists:
        files.append((key_path, (key + "\n").encode("ascii"), 0o600))
    if not native_exists:
        files.append((native_path, native_contents, 0o600))
    if not desktop_exists:
        files.append((desktop_path, native_contents, 0o600))
    if not launcher_exists:
        files.append((launcher_path, launcher_contents, 0o700))
    if not fragment_exists:
        files.append((fragment_path, fragment_contents, 0o600))
    if files:
        _commit_new_files(files, home, uid)

    def entry(path, existed):
        return {"path": str(path), "status": "preserved" if existed else "created"}

    return {
        "runtime": {"path": str(root), "status": "preserved"},
        "key": entry(key_path, key_exists),
        "native_catalog": entry(native_path, native_exists),
        "desktop_catalog": entry(desktop_path, desktop_exists),
        "launcher": entry(launcher_path, launcher_exists),
        "config_fragment": entry(fragment_path, fragment_exists),
    }


def parse_args(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--codex", required=True, help="absolute path to the installed Codex executable")
    parser.add_argument("--home", help="absolute setup home (defaults to the current user's home)")
    return parser.parse_args(argv)


def main(argv=None):
    args = parse_args(argv)
    home = Path(args.home) if args.home is not None else Path.home()
    try:
        report = prepare(home, args.codex)
    except SetupError as error:
        print("prepare setup failed: " + str(error), file=sys.stderr)
        return 1
    for name in ("runtime", "key", "native_catalog", "desktop_catalog", "launcher", "config_fragment"):
        item = report[name]
        print(name + ": " + item["status"] + " " + item["path"])
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
