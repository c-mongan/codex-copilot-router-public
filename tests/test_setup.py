"""Offline setup regression tests; never inspect or modify the user's HOME."""

import importlib.util
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import sys
import tomllib
import tempfile
import unittest
from unittest import mock


SCRIPTS = Path(__file__).resolve().parents[1] / "scripts"
SPEC = importlib.util.spec_from_file_location("prepare_setup", SCRIPTS / "prepare_setup.py")
setup = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(setup)


def descriptor(slug):
    return {
        "slug": slug,
        "display_name": slug,
        "context_window": 128000,
        "supported_reasoning_levels": [],
    }


class PrepareSetupTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.home = Path(self.temporary.name).resolve() / "home"
        self.home.mkdir(mode=0o700)
        self.root = self.home / ".local/share/codex-code-router"
        self.root.mkdir(parents=True, mode=0o700)
        (self.root / "bin").mkdir(mode=0o700)
        self.router = self.root / "bin/codex-code-router"
        self.router.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
        self.router.chmod(0o700)
        self.codex = self.home / "installed-codex"
        self.codex.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
        self.codex.chmod(0o700)
        self.bundled = {
            "models": [descriptor("native-one"), descriptor("copilot/old-alias")],
            "metadata": {"source": "installed-codex"},
        }
        self.loaded = []

    def load_bundled(self, codex):
        self.loaded.append(codex)
        return self.bundled

    def prepare(self):
        return setup.prepare(
            self.home,
            self.codex,
            uid=os.getuid(),
            load_models=self.load_bundled,
        )

    def test_fresh_setup_is_private_native_only_and_does_not_touch_codex_config(self):
        codex_config = self.home / ".codex/config.toml"
        codex_config.parent.mkdir(mode=0o700)
        codex_config.write_text("model = \"keep-user-setting\"\n", encoding="utf-8")
        before = codex_config.read_bytes()

        report = self.prepare()

        key_path = self.root / "client-token"
        key = key_path.read_text(encoding="ascii").rstrip("\n")
        self.assertRegex(key, re.compile(r"[0-9a-f]{64}\Z"))
        self.assertEqual(stat.S_IMODE(self.root.stat().st_mode), 0o700)
        self.assertEqual(stat.S_IMODE(key_path.stat().st_mode), 0o600)
        for name in ("native-models.json", "desktop-models.json"):
            path = self.root / name
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)
            document = json.loads(path.read_text(encoding="utf-8"))
            self.assertEqual([row["slug"] for row in document["models"]], ["native-one"])
            self.assertEqual(document["metadata"], self.bundled["metadata"])
        launcher = self.home / ".local/bin/codex-copilot-router"
        self.assertEqual(stat.S_IMODE(launcher.stat().st_mode), 0o700)
        fragment = self.root / "codex-router.config.toml"
        fragment_text = fragment.read_text(encoding="utf-8")
        self.assertEqual(stat.S_IMODE(fragment.stat().st_mode), 0o600)
        config = tomllib.loads(fragment_text)
        combined = config["model_providers"]["desktop-router"]
        self.assertEqual(combined["name"], "Native OpenAI + GitHub Copilot (local)")
        self.assertEqual(combined["http_headers"]["x-codex-router-key"], key)
        self.assertTrue(combined["requires_openai_auth"])
        self.assertFalse(combined["supports_websockets"])
        self.assertEqual(Path(config["model_catalog_json"]), self.root / "desktop-models.json")
        local_auth = config["model_providers"]["copilot-proxy"]["auth"]
        self.assertEqual(local_auth["args"], [str(key_path)])
        self.assertNotIn("model", config)
        self.assertEqual(codex_config.read_bytes(), before)
        self.assertEqual(self.loaded, [str(self.codex)])
        self.assertNotIn(key, repr(report))

    def test_repeated_setup_preserves_key_and_every_existing_output(self):
        first = self.prepare()
        paths = [Path(first[name]["path"]) for name in first if name != "runtime"]
        snapshots = {path: (path.read_bytes(), path.stat().st_ino, path.stat().st_mtime_ns) for path in paths}

        second = self.prepare()

        for path, snapshot in snapshots.items():
            self.assertEqual((path.read_bytes(), path.stat().st_ino, path.stat().st_mtime_ns), snapshot)
        self.assertTrue(all(second[name]["status"] == "preserved" for name in second if name != "runtime"))
        self.assertEqual(self.loaded, [str(self.codex)])

    def test_existing_catalogs_and_unrelated_codex_settings_are_preserved(self):
        native = self.root / "native-models.json"
        desktop = self.root / "desktop-models.json"
        native.write_text(json.dumps({"models": [descriptor("existing-native")]}), encoding="utf-8")
        desktop.write_text(json.dumps({"models": [descriptor("existing-native"), descriptor("copilot/verified")]}), encoding="utf-8")
        native.chmod(0o600)
        desktop.chmod(0o600)
        config = self.home / ".codex/config.toml"
        config.parent.mkdir(mode=0o700)
        config.write_text("approval_policy = \"on-request\"\n", encoding="utf-8")
        before = {path: path.read_bytes() for path in (native, desktop, config)}

        report = self.prepare()

        self.assertEqual({path: path.read_bytes() for path in before}, before)
        self.assertEqual(report["native_catalog"]["status"], "preserved")
        self.assertEqual(report["desktop_catalog"]["status"], "preserved")

    def test_unsafe_paths_and_conflicting_owned_outputs_fail_before_writes(self):
        launcher = self.home / ".local/bin/codex-copilot-router"
        launcher.parent.mkdir(mode=0o700)
        launcher.write_text("#!/bin/sh\necho foreign\n", encoding="utf-8")
        launcher.chmod(0o700)
        loader = mock.Mock(side_effect=AssertionError("metadata extraction must not run"))
        with self.assertRaises(setup.SetupError):
            setup.prepare(self.home, self.codex, uid=os.getuid(), load_models=loader)
        loader.assert_not_called()
        self.assertFalse((self.root / "client-token").exists())
        self.assertEqual(launcher.read_text(encoding="utf-8"), "#!/bin/sh\necho foreign\n")

        launcher.unlink()
        launcher.parent.rmdir()
        outside = Path(self.temporary.name) / "outside"
        outside.mkdir(mode=0o700)
        (self.home / ".local/bin").symlink_to(outside, target_is_directory=True)
        with self.assertRaises(setup.SetupError):
            self.prepare()
        self.assertEqual(list(outside.iterdir()), [])
        self.assertFalse((self.root / "client-token").exists())

    def test_conflicting_config_fragment_is_preserved_and_blocks_setup(self):
        key = self.root / "client-token"
        key.write_text("a" * 64 + "\n", encoding="ascii")
        key.chmod(0o600)
        fragment = self.root / "codex-router.config.toml"
        fragment.write_text("model_provider = \"someone-else\"\n", encoding="utf-8")
        fragment.chmod(0o600)
        loader = mock.Mock(side_effect=AssertionError("metadata extraction must not run"))

        with self.assertRaises(setup.SetupError):
            setup.prepare(self.home, self.codex, uid=os.getuid(), load_models=loader)

        loader.assert_not_called()
        self.assertEqual(fragment.read_text(encoding="utf-8"), "model_provider = \"someone-else\"\n")
        self.assertEqual(key.read_text(encoding="ascii"), "a" * 64 + "\n")
        self.assertFalse((self.home / ".local/bin/codex-copilot-router").exists())

    def test_metadata_failure_has_no_destructive_effects(self):
        settings = self.home / ".codex/config.toml"
        settings.parent.mkdir(mode=0o700)
        settings.write_text("model = \"unchanged\"\n", encoding="utf-8")
        with self.assertRaises(setup.SetupError):
            setup.prepare(
                self.home,
                self.codex,
                uid=os.getuid(),
                load_models=mock.Mock(side_effect=setup.refresh_catalog.CatalogError("failed")),
            )
        self.assertEqual(settings.read_text(encoding="utf-8"), "model = \"unchanged\"\n")
        for name in ("client-token", "native-models.json", "desktop-models.json", "codex-router.config.toml"):
            self.assertFalse((self.root / name).exists())
        self.assertFalse((self.home / ".local/bin/codex-copilot-router").exists())

    def test_package_manager_codex_symlink_uses_validated_target(self):
        alias = self.home / "codex-link"
        alias.symlink_to(self.codex)
        setup.prepare(self.home, alias, uid=os.getuid(), load_models=self.load_bundled)
        catalog = json.loads((self.root / "desktop-models.json").read_text())
        self.assertEqual([row["slug"] for row in catalog["models"]], ["native-one"])
        self.assertEqual(stat.S_IMODE((self.root / "client-token").stat().st_mode), 0o600)

    def test_failure_after_link_removes_the_new_destination(self):
        original_unlink = Path.unlink
        failed = False

        def fail_once(path, *args, **kwargs):
            nonlocal failed
            if not failed and path.name.startswith(".prepare-setup-"):
                failed = True
                raise OSError("synthetic staging unlink failure")
            return original_unlink(path, *args, **kwargs)

        with mock.patch.object(Path, "unlink", fail_once):
            with self.assertRaises(setup.SetupError):
                self.prepare()
        for name in ("client-token", "native-models.json", "desktop-models.json", "codex-router.config.toml"):
            self.assertFalse((self.root / name).exists(), name)
        self.assertFalse((self.home / ".local/bin/codex-copilot-router").exists())

    def test_commit_failure_removes_only_files_and_directories_created_by_this_run(self):
        original_link = setup.os.link
        committed = []

        def fail_after_first(source, destination, **kwargs):
            if committed:
                raise OSError("synthetic commit failure")
            original_link(source, destination, **kwargs)
            committed.append(Path(destination))

        with mock.patch.object(setup.os, "link", side_effect=fail_after_first):
            with self.assertRaises(setup.SetupError):
                self.prepare()
        for name in ("client-token", "native-models.json", "desktop-models.json", "codex-router.config.toml"):
            self.assertFalse((self.root / name).exists())
        self.assertFalse((self.home / ".local/bin").exists())
        self.assertTrue(self.router.exists())

    def test_cli_reports_only_paths_and_statuses(self):
        self.codex.write_text("#!/bin/sh\nprintf '%s\\n' '" + json.dumps(self.bundled) + "'\n")
        result = subprocess.run(
            [sys.executable, str(SCRIPTS / "prepare_setup.py"), "--codex", str(self.codex), "--home", str(self.home)],
            env={"HOME": str(self.home), "PATH": "/usr/bin:/bin"},
            capture_output=True, text=True, timeout=20,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        key = (self.root / "client-token").read_text().strip()
        self.assertIn(str(self.root / "codex-router.config.toml"), result.stdout)
        self.assertNotIn(key, result.stdout + result.stderr)


if __name__ == "__main__":
    unittest.main()
