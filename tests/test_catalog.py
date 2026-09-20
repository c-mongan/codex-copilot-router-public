import copy
import importlib.util
import json
import os
from pathlib import Path
import stat
import tempfile
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from unittest.mock import patch


SPEC = importlib.util.spec_from_file_location(
    "refresh_catalog", Path(__file__).resolve().parents[1] / "scripts/refresh_catalog.py"
)
catalog = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(catalog)


def descriptor(slug="gpt-5.4"):
    return {
        "slug": slug,
        "display_name": "Native model",
        "description": "Native description",
        "visibility": "hide",
        "supported_in_api": False,
        "upgrade": {"model": "replacement", "retirement_at": "2026-08-31"},
        "availability_nux": {"message": "Native only"},
        "context_window": 272000,
        "max_context_window": 1000000,
        "default_reasoning_level": "xhigh",
        "supported_reasoning_levels": [
            {"effort": "low", "description": "Low"},
            {"effort": "xhigh", "description": "Extra high"},
        ],
        "input_modalities": ["text", "image"],
        "supports_image_detail_original": True,
        "supports_search_tool": True,
        "web_search_tool_type": "text_and_image",
        "additional_speed_tiers": ["fast"],
        "service_tiers": [{"id": "priority"}],
        "use_responses_lite": True,
        "supports_experimental_context": True,
        "experimental_supported_tools": ["clock"],
        "apply_patch_tool_type": "freeform",
        "shell_type": "unified_exec",
        "base_instructions": "You are Codex, a coding agent based on GPT-5. Keep working.",
        "model_messages": {
            "instructions_template": "You are Codex, a coding agent based on GPT-5.\n{{ personality }}",
            "instructions_variables": {"personality": "Preserve this"},
        },
    }


def live_model(model_id="future-model"):
    return {
        "id": model_id,
        "name": "Future model",
        "model_picker_enabled": True,
        "policy": {"state": "enabled"},
        "supported_endpoints": ["/responses", "ws:/responses"],
        "capabilities": {
            "limits": {"max_context_window_tokens": 256000, "max_prompt_tokens": 128000},
            "supports": {
                "tool_calls": True,
                "streaming": True,
                "vision": False,
                "reasoning_effort": ["low", "medium", "not-a-codex-effort"],
            },
        },
    }


class CatalogMetadataTests(unittest.TestCase):
    def build(self, live, native=None, bundled=None):
        return catalog.build_catalogs(
            native if native is not None else {"models": [descriptor("native")]},
            bundled if bundled is not None else {"models": [descriptor()]},
            {"data": live},
        )

    def test_only_enabled_visible_http_responses_tools_streaming_models_are_selected(self):
        variants = [
            ("disabled", lambda x: x["policy"].update(state="disabled")),
            ("unknown-policy", lambda x: x["policy"].update(state=None)),
            ("invisible", lambda x: x.update(model_picker_enabled=False)),
            ("truthy-not-boolean", lambda x: x.update(model_picker_enabled=1)),
            ("websocket-only", lambda x: x.update(supported_endpoints=["ws:/responses"])),
            ("other-protocol", lambda x: x.update(supported_endpoints=["/v1/messages", "/chat/completions"])),
            ("no-tools", lambda x: x["capabilities"]["supports"].update(tool_calls=False)),
            ("no-streaming", lambda x: x["capabilities"]["supports"].update(streaming=False)),
            ("zero-prompt", lambda x: x["capabilities"]["limits"].update(max_prompt_tokens=0)),
            ("invalid-context", lambda x: x["capabilities"]["limits"].update(max_context_window_tokens=True)),
        ]
        rows = [live_model("eligible")]
        for name, mutate in variants:
            row = live_model(name)
            mutate(row)
            rows.append(row)
        combined, raw, report = self.build(rows)
        self.assertEqual([row["slug"] for row in raw["models"]], ["eligible"])
        self.assertEqual(combined["models"][-1]["slug"], "copilot/eligible")
        self.assertEqual({row["id"] for row in report["skipped"]}, {name for name, _ in variants})

    def test_native_rows_are_unchanged_and_only_copilot_retirement_is_cleared(self):
        native_row = descriptor("same-model")
        native = {"models": [native_row, descriptor("copilot/old")], "metadata": {"keep": True}}
        original = copy.deepcopy(native)
        combined, raw, report = self.build([live_model("same-model")], native=native)
        self.assertEqual(native, original)
        self.assertEqual(combined["models"][0], original["models"][0])
        self.assertEqual(combined["metadata"], {"keep": True})
        self.assertEqual(len(combined["models"]), 2)
        copilot = raw["models"][0]
        self.assertIsNone(copilot["upgrade"])
        self.assertIsNone(copilot["availability_nux"])
        self.assertEqual(copilot["visibility"], "list")
        self.assertTrue(copilot["supported_in_api"])
        self.assertEqual(copilot["base_instructions"], native_row["base_instructions"])
        self.assertEqual(copilot["apply_patch_tool_type"], "freeform")
        self.assertEqual(report["descriptors"]["same-model"], {"source": "native", "template": "same-model"})
        self.assertIn("clock", copilot["experimental_supported_tools"])

    def test_bundled_exact_descriptor_wins_over_generic_fallback(self):
        bundled = {"models": [descriptor(), descriptor("other-exact")]}
        bundled["models"][1]["shell_type"] = "shell_command"
        _, raw, report = self.build([live_model("other-exact")], bundled=bundled)
        self.assertEqual(raw["models"][0]["shell_type"], "shell_command")
        self.assertEqual(report["descriptors"]["other-exact"], {"source": "bundled", "template": "other-exact"})

    def test_existing_copilot_descriptor_keeps_its_verified_tool_protocol(self):
        previous = descriptor("copilot/fast-model")
        previous["tool_mode"] = "code_mode_only"
        _, raw, _ = self.build(
            [live_model("fast-model")],
            native={"models": [descriptor("native"), previous]},
        )
        self.assertEqual(raw["models"][0]["apply_patch_tool_type"], "freeform")
        self.assertEqual(raw["models"][0]["tool_mode"], "code_mode_only")

    def test_fallback_has_neutral_identity_and_no_native_only_capability_claims(self):
        _, raw, report = self.build([live_model()])
        row = raw["models"][0]
        self.assertNotIn("based on GPT-5", row["base_instructions"])
        self.assertNotIn("based on GPT-5", row["model_messages"]["instructions_template"])
        self.assertFalse(row["use_responses_lite"])
        self.assertFalse(row["supports_image_detail_original"])
        self.assertFalse(row["supports_experimental_context"])
        self.assertEqual(row["experimental_supported_tools"], [])
        self.assertEqual(row["additional_speed_tiers"], [])
        self.assertEqual(row["service_tiers"], [])
        self.assertNotIn("web_search_tool_type", row)
        self.assertEqual(report["descriptors"]["future-model"], {"source": "fallback", "template": "gpt-5.4"})

    def test_copilot_agent_protocol_does_not_inherit_native_v2(self):
        native_row = descriptor("same-model")
        native_row["multi_agent_version"] = "v2"
        original = copy.deepcopy(native_row)
        combined, raw, _ = self.build(
            [live_model("same-model")], native={"models": [native_row]}
        )
        self.assertEqual(combined["models"][0], original)
        self.assertEqual(native_row, original)
        self.assertEqual(raw["models"][0]["multi_agent_version"], "v1")
        self.assertEqual(combined["models"][1]["multi_agent_version"], "v1")

    def test_tool_discovery_capability_is_not_disabled_as_web_search(self):
        for source in ("native", "bundled", "existing", "fallback"):
            native = {"models": [descriptor("native")]}
            bundled = {"models": [descriptor()]}
            if source == "native":
                native["models"].append(descriptor("search-model"))
            elif source == "bundled":
                bundled["models"].append(descriptor("search-model"))
            elif source == "existing":
                native["models"].append(descriptor("copilot/search-model"))
            with self.subTest(source=source):
                _, raw, _ = self.build([live_model("search-model")], native=native, bundled=bundled)
                self.assertTrue(raw["models"][0]["supports_search_tool"])
                self.assertNotIn("web_search_tool_type", raw["models"][0])

    def test_explicitly_unsupported_tool_discovery_is_not_invented(self):
        native = descriptor("search-model")
        native["supports_search_tool"] = False
        _, raw, _ = self.build([live_model("search-model")], native={"models": [native]})
        self.assertFalse(raw["models"][0]["supports_search_tool"])

    def test_deferred_discovery_uses_code_mode_instead_of_hosted_search(self):
        native = descriptor("search-model")
        native["tool_mode"] = "direct"
        combined, raw, _ = self.build([live_model("search-model")], native={"models": [native]})
        self.assertEqual(raw["models"][0].get("tool_mode"), "code_mode_only")
        self.assertEqual(combined["models"][0]["tool_mode"], "direct")

    def test_live_limits_reasoning_and_vision_bound_the_descriptor(self):
        live = live_model()
        _, raw, _ = self.build([live])
        row = raw["models"][0]
        self.assertEqual(row["context_window"], 128000)
        self.assertEqual(row["max_context_window"], 128000)
        self.assertEqual(row["input_modalities"], ["text"])
        self.assertEqual([level["effort"] for level in row["supported_reasoning_levels"]], ["low", "medium"])
        self.assertEqual(row["default_reasoning_level"], "medium")
        live["capabilities"]["supports"]["vision"] = True
        live["capabilities"]["supports"]["reasoning_effort"] = []
        _, raw, _ = self.build([live])
        self.assertEqual(raw["models"][0]["input_modalities"], ["text", "image"])
        self.assertEqual(raw["models"][0]["supported_reasoning_levels"], [])
        self.assertIsNone(raw["models"][0]["default_reasoning_level"])

    def test_larger_copilot_context_does_not_widen_native_descriptor_limits(self):
        native = descriptor("same-model")
        native["context_window"] = 64000
        native["max_context_window"] = 96000
        combined, raw, _ = self.build([live_model("same-model")], native={"models": [native]})
        self.assertEqual(raw["models"][0]["context_window"], 64000)
        self.assertEqual(raw["models"][0]["max_context_window"], 96000)
        self.assertEqual(combined["models"][0], native)

    def test_duplicate_live_ids_fail_instead_of_overriding_policy(self):
        disabled = live_model()
        disabled["policy"]["state"] = "disabled"
        with self.assertRaises(catalog.CatalogError):
            self.build([live_model(), disabled])


class CatalogSafetyTests(unittest.TestCase):
    def test_router_urls_never_send_credentials_to_remote_or_ambiguous_targets(self):
        for url in [
            "https://example.com/v1", "http://127.0.0.1.evil/v1", "http://user@127.0.0.1/v1",
            "http://127.0.0.1/v1?key=value", "http://127.0.0.1/v1#fragment", "http://127.0.0.1/v1?",
            "http://127.0.0.1/v1#", "file:///v1", "http://2130706433/v1", "http://127.0.0.1:0/v1",
            "http://127.0.0.1/combined/v1", " http://127.0.0.1/v1", "http://127.0.0.1/v1\n",
        ]:
            with self.subTest(url=url), self.assertRaises(catalog.CatalogError):
                catalog.validate_router_url(url)
        self.assertEqual(catalog.validate_router_url("http://localhost:60001/v1/"), "http://127.0.0.1:60001/v1")
        self.assertEqual(catalog.validate_router_url("http://[::1]:60001/v1"), "http://[::1]:60001/v1")

    def test_token_requires_private_regular_file_and_never_leaks_value_in_error(self):
        with tempfile.TemporaryDirectory() as root:
            path = Path(root) / "token"
            secret = "0123456789abcdef" * 4
            path.write_text(secret + "\n")
            path.chmod(0o600)
            self.assertEqual(catalog.read_client_token(path), secret)
            path.chmod(0o644)
            with self.assertRaises(catalog.CatalogError) as error:
                catalog.read_client_token(path)
            self.assertNotIn(secret, str(error.exception))
            path.chmod(0o600)
            link = Path(root) / "link"
            link.symlink_to(path)
            with self.assertRaises(catalog.CatalogError):
                catalog.read_client_token(link)

    def test_redirect_is_not_followed_even_to_another_loopback_path(self):
        requests = []
        class Handler(BaseHTTPRequestHandler):
            def do_GET(self):
                requests.append(self.path)
                self.send_response(302)
                self.send_header("Location", "/stolen")
                self.end_headers()
            def log_message(self, *args):
                pass
        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            with self.assertRaises(catalog.CatalogError):
                catalog.fetch_models(f"http://127.0.0.1:{server.server_port}/v1", "a" * 64)
            self.assertEqual(requests, ["/v1/models"])
        finally:
            server.shutdown()
            server.server_close()
            thread.join()

    def test_discovery_reads_complete_bounded_authenticated_json(self):
        observed = []
        class Handler(BaseHTTPRequestHandler):
            def do_GET(self):
                observed.append((self.path, self.headers.get("Authorization")))
                body = b'{"data":[]}'
                self.send_response(200)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
            def log_message(self, *args):
                pass
        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            result = catalog.fetch_models(f"http://127.0.0.1:{server.server_port}/v1", "a" * 64)
            self.assertEqual(result, {"data": []})
            self.assertEqual(observed, [("/v1/models", "Bearer " + "a" * 64)])
            with patch.object(catalog, "MAX_BYTES", 8), self.assertRaises(catalog.CatalogError):
                catalog.fetch_models(f"http://127.0.0.1:{server.server_port}/v1", "a" * 64)
        finally:
            server.shutdown()
            server.server_close()
            thread.join()

    def test_slow_drip_headers_cannot_extend_the_total_discovery_deadline(self):
        stop = threading.Event()
        class Handler(BaseHTTPRequestHandler):
            def do_GET(self):
                try:
                    for byte in b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\n{\"data\":[]}":
                        if stop.wait(0.02):
                            break
                        self.wfile.write(bytes([byte]))
                        self.wfile.flush()
                except OSError:
                    pass
            def log_message(self, *args):
                pass
        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            started = time.monotonic()
            with patch.object(catalog, "TIMEOUT_SECONDS", 0.1), self.assertRaises(catalog.CatalogError):
                catalog.fetch_models(f"http://127.0.0.1:{server.server_port}/v1", "a" * 64)
            self.assertLess(time.monotonic() - started, 0.75)
        finally:
            stop.set()
            server.shutdown()
            server.server_close()
            thread.join()

    def test_duplicate_json_keys_cannot_override_policy(self):
        with self.assertRaises(catalog.CatalogError):
            catalog.parse_json(b'{"data":[{"policy":{"state":"disabled","state":"enabled"}}]}')

    def test_atomic_outputs_are_private_and_symlinks_cannot_change_either_output(self):
        with tempfile.TemporaryDirectory() as root:
            first, second, victim = (Path(root) / name for name in ["first", "second", "victim"])
            catalog.write_outputs([(first, {"models": []}), (second, {"models": []})])
            self.assertEqual(stat.S_IMODE(first.stat().st_mode), 0o600)
            self.assertEqual(stat.S_IMODE(second.stat().st_mode), 0o600)
            before = first.read_bytes()
            second.unlink()
            victim.write_text("untouched")
            second.symlink_to(victim)
            with self.assertRaises(catalog.CatalogError):
                catalog.write_outputs([(first, {"changed": True}), (second, {"changed": True})])
            self.assertEqual(first.read_bytes(), before)
            self.assertEqual(victim.read_text(), "untouched")

    def test_failed_second_output_replacement_restores_first_output(self):
        with tempfile.TemporaryDirectory() as root:
            first, second = Path(root) / "first", Path(root) / "second"
            first.write_bytes(b"first previous")
            second.write_bytes(b"second previous")
            replace = os.replace
            def fail_second(source, destination):
                if Path(destination) == second:
                    raise OSError("synthetic replace failure")
                return replace(source, destination)
            with patch.object(catalog.os, "replace", side_effect=fail_second):
                with self.assertRaises(catalog.CatalogError):
                    catalog.write_outputs([(first, {"new": 1}), (second, {"new": 2})])
            self.assertEqual(first.read_bytes(), b"first previous")
            self.assertEqual(second.read_bytes(), b"second previous")

    def test_staging_output_cannot_overwrite_native_catalog_or_client_token(self):
        with tempfile.TemporaryDirectory() as root:
            native, token = Path(root) / "native", Path(root) / "token"
            native.write_text(json.dumps({"models": [descriptor("native")]}))
            token.write_text("a" * 64)
            token.chmod(0o600)
            for target in (native, token):
                before = target.read_bytes()
                args = catalog.parse_args(["--native-catalog", str(native), "--output", str(target), "--client-token-file", str(token)])
                with patch.object(catalog, "fetch_models", side_effect=AssertionError("unexpected discovery")):
                    with self.assertRaises(catalog.CatalogError):
                        catalog.refresh(args)
                self.assertEqual(target.read_bytes(), before)

    def test_discovery_and_bundle_failure_leave_staging_outputs_intact(self):
        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            native, token, output = root / "native", root / "token", root / "output"
            native.write_text(json.dumps({"models": [descriptor("native")]}))
            token.write_text("a" * 64)
            token.chmod(0o600)
            output.write_bytes(b"previous output")
            argv = ["--native-catalog", str(native), "--output", str(output), "--client-token-file", str(token)]
            with patch.object(catalog, "fetch_models", side_effect=catalog.CatalogError("discovery failed")):
                with self.assertRaises(catalog.CatalogError):
                    catalog.refresh(catalog.parse_args(argv))
            self.assertEqual(output.read_bytes(), b"previous output")
            with patch.object(catalog, "fetch_models", return_value={"data": [live_model()]}):
                with patch.object(catalog, "load_bundled_models", side_effect=catalog.CatalogError("bundle failed")):
                    with self.assertRaises(catalog.CatalogError):
                        catalog.refresh(catalog.parse_args(argv))
            self.assertEqual(output.read_bytes(), b"previous output")


if __name__ == "__main__":
    unittest.main()
