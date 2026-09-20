#!/usr/bin/env python3
"""Build staging Codex catalogs from policy-enabled local Copilot discovery."""

import argparse
import copy
import http.client
import ipaddress
import json
import os
from pathlib import Path
import re
import selectors
import socket
import stat
import subprocess
import sys
import tempfile
import threading
import time
from urllib.parse import urlsplit, urlunsplit


MAX_BYTES = 8 * 1024 * 1024
TIMEOUT_SECONDS = 20
REASONING_LEVELS = ("none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra")
MODEL_ID = re.compile(r"[A-Za-z0-9][A-Za-z0-9._:-]*\Z", re.ASCII)
BEARER_TOKEN = re.compile(r"[A-Za-z0-9._~+/-]+=*\Z", re.ASCII)


class CatalogError(Exception):
    """A safe, content-free error suitable for a terminal diagnostic."""


def validate_router_url(value: str) -> str:
    """Return a normalized loopback /v1 base URL without DNS or credentials."""
    if not isinstance(value, str) or any(ord(char) <= 32 or ord(char) == 127 for char in value):
        raise CatalogError("router URL must be a loopback HTTP(S) /v1 URL")
    try:
        parts = urlsplit(value)
        if (
            parts.scheme not in ("http", "https")
            or parts.username is not None
            or parts.password is not None
            or "?" in value
            or "#" in value
            or parts.path not in ("/v1", "/v1/")
            or not parts.hostname
        ):
            raise ValueError
        host = parts.hostname
        if host == "localhost":
            host = "127.0.0.1"
        address = ipaddress.ip_address(host)
        if not address.is_loopback or "%" in host:
            raise ValueError
        port = parts.port
        if port is not None and not 1 <= port <= 65535:
            raise ValueError
        authority = f"[{address.compressed}]" if address.version == 6 else address.compressed
        if port is not None:
            authority += f":{port}"
        return urlunsplit((parts.scheme, authority, "/v1", "", ""))
    except ValueError:
        raise CatalogError("router URL must be a loopback HTTP(S) /v1 URL") from None


def read_regular_file(path: Path, limit: int, *, private: bool = False) -> bytes:
    try:
        before = path.lstat()
        if not stat.S_ISREG(before.st_mode):
            raise CatalogError("input must be a regular file, not a symlink")
        flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_NONBLOCK", 0)
        with os.fdopen(os.open(path, flags), "rb") as source:
            opened = os.fstat(source.fileno())
            if (
                not stat.S_ISREG(opened.st_mode)
                or (opened.st_dev, opened.st_ino) != (before.st_dev, before.st_ino)
            ):
                raise CatalogError("input file changed while opening")
            if private and (opened.st_mode & 0o077 or opened.st_uid != os.getuid()):
                raise CatalogError("client token must be owner-owned with owner-only permissions")
            if opened.st_size > limit:
                raise CatalogError("input exceeds its size limit")
            contents = source.read(limit + 1)
            if len(contents) > limit:
                raise CatalogError("input exceeds its size limit")
            return contents
    except OSError:
        raise CatalogError("cannot safely read input file") from None


def read_client_token(path: Path) -> str:
    """Read a private local token without logging its path or contents."""
    try:
        token = read_regular_file(Path(path), 4098, private=True).decode("ascii")
    except UnicodeError:
        raise CatalogError("client token has invalid encoding") from None
    if token.endswith("\r\n"):
        token = token[:-2]
    elif token.endswith("\n"):
        token = token[:-1]
    if not 32 <= len(token) <= 4096 or not BEARER_TOKEN.fullmatch(token):
        raise CatalogError("client token has invalid bearer-token syntax")
    return token


def parse_json(contents: bytes):
    def unique_object(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise CatalogError("JSON contains duplicate object keys")
            result[key] = value
        return result

    def invalid_number(_):
        raise CatalogError("JSON contains a nonfinite number")

    try:
        return json.loads(contents, object_pairs_hook=unique_object, parse_constant=invalid_number)
    except (UnicodeError, ValueError, RecursionError):
        raise CatalogError("invalid catalog JSON") from None


def fetch_models(router_url: str, token: str):
    """Fetch only the local HTTP Responses adapter; never use proxies or redirects."""
    parts = urlsplit(validate_router_url(router_url))
    connection_type = http.client.HTTPSConnection if parts.scheme == "https" else http.client.HTTPConnection
    connection = connection_type(parts.hostname, parts.port, timeout=TIMEOUT_SECONDS)
    deadline = time.monotonic() + TIMEOUT_SECONDS
    timer = None
    try:
        connection.connect()
        connection_socket = connection.sock
        connection_socket.settimeout(max(0.001, deadline - time.monotonic()))
        # A socket timeout alone restarts for every header line/read. Shutdown at
        # the absolute deadline also bounds a peer that drips header bytes.
        def expire():
            try:
                connection_socket.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
        timer = threading.Timer(max(0.001, deadline - time.monotonic()), expire)
        timer.daemon = True
        timer.start()
        connection.request("GET", "/v1/models", headers={
            "Authorization": f"Bearer {token}",
            "Accept": "application/json",
            "Accept-Encoding": "identity",
            "Connection": "close",
        })
        response = connection.getresponse()
        if response.status != 200:
            raise CatalogError("local model discovery failed; redirects are not allowed")
        if response.getheader("Content-Encoding", "identity").lower() != "identity":
            raise CatalogError("encoded discovery responses are not supported")
        chunks = bytearray()
        while not response.isclosed():
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise CatalogError("local model discovery timed out")
            connection_socket.settimeout(remaining)
            chunk = response.read1(min(65536, MAX_BYTES + 1 - len(chunks)))
            if not chunk:
                break
            chunks.extend(chunk)
            if len(chunks) > MAX_BYTES:
                raise CatalogError("model discovery exceeds its size limit")
        return parse_json(bytes(chunks))
    except (OSError, http.client.HTTPException):
        raise CatalogError("local model discovery failed") from None
    finally:
        if timer is not None:
            timer.cancel()
            timer.join()
        connection.close()


def load_bundled_models(codex: str):
    """Read local bundled descriptors with bounded stdout and execution time."""
    process = None
    try:
        process = subprocess.Popen(
            [codex, "debug", "models", "--bundled"],
            stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
        )
        deadline = time.monotonic() + TIMEOUT_SECONDS
        contents = bytearray()
        with selectors.DefaultSelector() as selector:
            selector.register(process.stdout, selectors.EVENT_READ)
            while True:
                remaining = deadline - time.monotonic()
                if remaining <= 0 or not selector.select(remaining):
                    raise CatalogError("bundled descriptor command timed out")
                chunk = os.read(process.stdout.fileno(), min(65536, MAX_BYTES + 1 - len(contents)))
                if not chunk:
                    break
                contents.extend(chunk)
                if len(contents) > MAX_BYTES:
                    raise CatalogError("bundled descriptors exceed their size limit")
        if process.wait(timeout=max(0.001, deadline - time.monotonic())) != 0:
            raise CatalogError("bundled descriptor command failed")
        return parse_json(bytes(contents))
    except (OSError, subprocess.TimeoutExpired):
        raise CatalogError("cannot read local bundled descriptors") from None
    finally:
        if process is not None:
            if process.poll() is None:
                process.kill()
            process.wait()
            process.stdout.close()


def catalog_index(document):
    if not isinstance(document, dict) or not isinstance(document.get("models"), list):
        raise CatalogError("catalog must contain a models array")
    models = {}
    for row in document["models"]:
        slug = row.get("slug") if isinstance(row, dict) else None
        if (
            not isinstance(slug, str) or not slug or slug != slug.strip()
            or any(ord(char) < 32 for char in slug) or slug in models
        ):
            raise CatalogError("catalog contains invalid or duplicate model slugs")
        models[slug] = row
    return models


def positive_integer(value):
    return isinstance(value, int) and not isinstance(value, bool) and value > 0


def ineligible_reason(row):
    policy = row.get("policy")
    if not isinstance(policy, dict) or policy.get("state") != "enabled":
        return "policy_not_enabled"
    if row.get("model_picker_enabled") is not True:
        return "not_picker_visible"
    endpoints = row.get("supported_endpoints")
    if not isinstance(endpoints, list) or not any(endpoint in ("/responses", "/v1/responses") for endpoint in endpoints):
        return "no_http_responses"
    capabilities = row.get("capabilities")
    if not isinstance(capabilities, dict):
        return "missing_capabilities"
    supports = capabilities.get("supports")
    if not isinstance(supports, dict) or supports.get("tool_calls") is not True:
        return "no_tool_calls"
    if supports.get("streaming") is not True:
        return "no_streaming"
    limits = capabilities.get("limits")
    if not isinstance(limits, dict) or not all(
        positive_integer(limits.get(key)) for key in ("max_context_window_tokens", "max_prompt_tokens")
    ):
        return "invalid_context_limits"
    return None


def copilot_descriptor(live, template, *, fallback):
    row = copy.deepcopy(template)
    row["slug"] = live["id"]
    name = live.get("name")
    row["display_name"] = "Copilot · " + (name if isinstance(name, str) and name.strip() else live["id"])
    row["description"] = "GitHub Copilot HTTP Responses model."
    row["visibility"] = "list"
    row["supported_in_api"] = True
    row["upgrade"] = None
    row["availability_nux"] = None
    row["additional_speed_tiers"] = []
    row["service_tiers"] = []
    row["use_responses_lite"] = False
    # This is client-executed MCP/tool discovery, not hosted web search.
    # Preserve the descriptor's capability so large registries remain deferred.
    if row.get("supports_search_tool") is True:
        # Older Copilot models reject hosted `tool_search` but can use Codex's
        # client-owned tools/ALL_TOOLS registry through the exec tool.
        row["tool_mode"] = "code_mode_only"
    row.pop("web_search_tool_type", None)
    row["supports_experimental_context"] = False
    row["supports_image_detail_original"] = False
    supports = live["capabilities"]["supports"]
    row["input_modalities"] = ["text", "image"] if supports.get("vision") is True else ["text"]
    limits = live["capabilities"]["limits"]
    cap = min(limits["max_context_window_tokens"], limits["max_prompt_tokens"])
    for field in ("context_window", "max_context_window"):
        value = row.get(field)
        row[field] = min(value, cap) if positive_integer(value) else cap
    row["context_window"] = min(row["context_window"], row["max_context_window"])
    advertised = supports.get("reasoning_effort", [])
    advertised = advertised if isinstance(advertised, list) else []
    template_levels = row.get("supported_reasoning_levels")
    template_levels = template_levels if isinstance(template_levels, list) else []
    known_descriptions = {
        level["effort"]: level for level in template_levels
        if isinstance(level, dict) and isinstance(level.get("effort"), str)
    }
    levels = [effort for effort in REASONING_LEVELS if effort in advertised]
    row["supported_reasoning_levels"] = [
        copy.deepcopy(known_descriptions.get(effort, {"effort": effort, "description": f"{effort.capitalize()} reasoning"}))
        for effort in levels
    ]
    if row.get("default_reasoning_level") not in levels:
        row["default_reasoning_level"] = "medium" if "medium" in levels else (levels[0] if levels else None)
    if fallback:
        row["experimental_supported_tools"] = []
        old_intro = "You are Codex, a coding agent based on GPT-5."
        neutral_intro = "You are Codex, a coding agent."
        instructions = row.get("base_instructions")
        if isinstance(instructions, str):
            row["base_instructions"] = instructions.replace(old_intro, neutral_intro)
        messages = row.get("model_messages")
        if isinstance(messages, dict) and isinstance(messages.get("instructions_template"), str):
            messages["instructions_template"] = messages["instructions_template"].replace(old_intro, neutral_intro)
    return row


def build_catalogs(native, bundled, discovery):
    native_models = catalog_index(native)
    bundled_models = catalog_index(bundled)
    if not isinstance(discovery, dict) or not isinstance(discovery.get("data"), list):
        raise CatalogError("model discovery must contain a data array")
    native_rows = [row for slug, row in native_models.items() if not slug.startswith("copilot/")]
    selected, skipped, descriptors, seen = [], [], {}, set()
    for live in discovery["data"]:
        model_id = live.get("id") if isinstance(live, dict) else None
        if not isinstance(model_id, str) or len(model_id) > 256 or not MODEL_ID.fullmatch(model_id):
            skipped.append({"id": "<invalid>", "reason": "invalid_model_id"})
            continue
        if model_id in seen:
            raise CatalogError("model discovery contains duplicate IDs")
        seen.add(model_id)
        reason = ineligible_reason(live)
        if reason:
            skipped.append({"id": model_id, "reason": reason})
            continue
        if model_id in native_models:
            source, template = "native", native_models[model_id]
        elif model_id in bundled_models:
            source, template = "bundled", bundled_models[model_id]
        elif "copilot/" + model_id in native_models:
            source, template = "existing", native_models["copilot/" + model_id]
        elif "gpt-5.4" in bundled_models:
            source, template = "fallback", bundled_models["gpt-5.4"]
        else:
            raise CatalogError("local generic bundled descriptor is unavailable")
        descriptors[model_id] = {"source": source, "template": template["slug"]}
        selected.append(copilot_descriptor(live, template, fallback=source == "fallback"))
    combined = copy.deepcopy(native)
    combined["models"] = copy.deepcopy(native_rows)
    for row in selected:
        alias = copy.deepcopy(row)
        alias["slug"] = "copilot/" + row["slug"]
        combined["models"].append(alias)
    return combined, {"models": selected}, {
        "ids": [row["slug"] for row in selected], "skipped": skipped, "descriptors": descriptors,
    }


def inspect_output(path):
    try:
        metadata = path.lstat()
    except FileNotFoundError:
        return False
    if not stat.S_ISREG(metadata.st_mode):
        raise CatalogError("output must be a regular file, not a symlink")
    return True


def stage_bytes(path, contents):
    descriptor, name = tempfile.mkstemp(prefix=".catalog-", dir=path.parent)
    temporary = Path(name)
    try:
        with os.fdopen(descriptor, "wb") as destination:
            os.fchmod(destination.fileno(), 0o600)
            destination.write(contents)
            destination.flush()
            os.fsync(destination.fileno())
        return temporary
    except BaseException:
        temporary.unlink(missing_ok=True)
        raise


def write_outputs(outputs):
    """Stage all files privately before replacing any; roll back commit failures."""
    recovery_needed = False
    prepared, committed = [], []
    try:
        paths = [Path(path).absolute() for path, _ in outputs]
        if len({path.resolve() for path in paths}) != len(paths):
            raise CatalogError("catalog outputs must be different paths")
        for path in paths:
            inspect_output(path)
        for path, (_, document) in zip(paths, outputs):
            contents = (json.dumps(document, ensure_ascii=False, indent=2, allow_nan=False) + "\n").encode("utf-8")
            if len(contents) > MAX_BYTES:
                raise CatalogError("generated catalog exceeds its size limit")
            backup = stage_bytes(path, read_regular_file(path, MAX_BYTES)) if inspect_output(path) else None
            prepared.append((path, None, backup))
            staged = stage_bytes(path, contents)
            prepared[-1] = (path, staged, backup)
        for path, staged, backup in prepared:
            inspect_output(path)
            os.replace(staged, path)
            committed.append((path, backup))
    except (OSError, ValueError, CatalogError):
        for path, backup in reversed(committed):
            try:
                if backup is None:
                    path.unlink()
                else:
                    os.replace(backup, path)
            except OSError:
                recovery_needed = True
                raise CatalogError("catalog output rollback failed; private recovery backups retained") from None
        raise CatalogError("catalog output failed; previous outputs were preserved") from None
    finally:
        for _, staged, backup in prepared:
            for temporary in (staged, backup):
                if temporary is not None and not (recovery_needed and temporary == backup):
                    temporary.unlink(missing_ok=True)


def parse_args(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--native-catalog", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path, help="explicit staging combined-catalog path")
    parser.add_argument("--copilot-output", type=Path, help="optional staging raw-ID Copilot catalog")
    parser.add_argument("--router-url", default="http://127.0.0.1:60001/v1")
    parser.add_argument("--client-token-file", type=Path, default=Path.home() / ".local/share/codex-code-router/client-token")
    parser.add_argument("--codex", default="codex")
    return parser.parse_args(argv)


def refresh(args):
    router_url = validate_router_url(args.router_url)
    output_paths = [args.output] + ([args.copilot_output] if args.copilot_output is not None else [])
    protected = {args.native_catalog.resolve(), args.client_token_file.resolve()}
    if any(path.resolve() in protected for path in output_paths):
        raise CatalogError("staging outputs must not overwrite input catalogs or credentials")
    for path in output_paths:
        inspect_output(path)
    native = parse_json(read_regular_file(args.native_catalog, MAX_BYTES))
    token = read_client_token(args.client_token_file)
    discovery = fetch_models(router_url, token)
    bundled = load_bundled_models(args.codex)
    combined, copilot, report = build_catalogs(native, bundled, discovery)
    outputs = [(args.output, combined)]
    if args.copilot_output is not None:
        outputs.append((args.copilot_output, copilot))
    write_outputs(outputs)
    return report


def main(argv=None):
    try:
        report = refresh(parse_args(argv))
    except (CatalogError, OSError):
        print("catalog refresh failed; no catalog was activated", file=sys.stderr)
        return 1
    print(json.dumps(report, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    sys.exit(main())
