"""Filesystem regression tests; never use the user's home or launchd."""

import importlib.util
import os
from pathlib import Path
import plistlib
import stat
import subprocess
import tempfile
import unittest
from unittest import mock


SPEC = importlib.util.spec_from_file_location(
    "install_launchagent",
    Path(__file__).resolve().parents[1] / "scripts" / "install_launchagent.py",
)
installer = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(installer)


class LaunchAgentTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.home = Path(self.temporary.name).resolve() / "home"
        self.home.mkdir(mode=0o700)
        self.root = self.home / ".local/share/codex-code-router"
        self.root.mkdir(parents=True, mode=0o700)
        self.wrapper = self.home / ".local/bin/codex-copilot-router"
        self.wrapper.parent.mkdir(mode=0o700)
        self.wrapper.write_text("#!/bin/sh\nexit 0\n")
        self.wrapper.chmod(0o700)
        self.key = self.root / "client-token"
        self.key.write_text("fixture-local-key\n")
        self.key.chmod(0o600)
        self.plist = self.home / "Library/LaunchAgents" / (installer.LABEL + ".plist")
        self.commands = []
        self.registration = None
        self.print_error = None
        self.port_free = True

    def launchctl(self, command, **kwargs):
        self.commands.append(command)
        self.assertEqual(command[0], "/bin/launchctl")
        if command[1] == "print":
            if self.print_error:
                return subprocess.CompletedProcess(command, 1, "", self.print_error)
            if self.registration is None:
                return subprocess.CompletedProcess(
                    command, 113, "",
                    'Could not find service "' + installer.LABEL + '" in domain for user gui',
                )
            return subprocess.CompletedProcess(command, 0, self.registration, "")
        self.assertEqual(command[1:3], ["bootstrap", "gui/" + str(os.getuid())])
        self.assertEqual(command[3], str(self.plist))
        return subprocess.CompletedProcess(command, 0, "", "")

    def install(self, **kwargs):
        return installer.install(
            self.home, uid=os.getuid(), platform="darwin", run=self.launchctl,
            port_available=lambda: self.port_free, **kwargs,
        )

    def registered(self, program=None):
        program = program or str(self.wrapper)
        self.registration = (
            "gui/test = {\n\tpath = " + str(self.plist)
            + "\n\tprogram = " + program
            + "\n\targuments = {\n\t\t" + program
            + "\n\t\tserve\n\t}\n}\n"
        )

    def write_plist(self, value):
        self.plist.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        self.plist.write_bytes(plistlib.dumps(value))
        self.plist.chmod(0o600)

    def assert_no_install(self):
        self.assertFalse(self.plist.exists())
        self.assertFalse((self.root / "logs").exists())
        self.assertEqual(self.commands, [])

    def test_render_needs_no_files_and_cannot_expose_credentials(self):
        absent_home = self.home / "not-created"
        document = plistlib.loads(installer.render(absent_home))
        self.assertFalse(absent_home.exists())
        self.assertEqual(document["ProgramArguments"], [
            str(absent_home / ".local/bin/codex-copilot-router"), "serve",
        ])
        self.assertEqual(document["WorkingDirectory"], str(absent_home / ".local/share/codex-code-router"))
        self.assertEqual(document["KeepAlive"], {"SuccessfulExit": False})
        self.assertIs(document["RunAtLoad"], True)
        self.assertNotIn("EnvironmentVariables", document)
        self.assertNotIn(b"fixture-local-key", installer.render(self.home))

    def test_install_is_private_atomic_and_idempotent_without_loading(self):
        self.plist.parent.mkdir(parents=True, mode=0o755)
        self.plist.parent.chmod(0o755)
        result = self.install()
        self.assertTrue(result["changed"])
        self.assertEqual(result["load_state"], "not-requested")
        self.assertEqual(stat.S_IMODE(self.plist.stat().st_mode), 0o600)
        self.assertEqual(stat.S_IMODE(self.plist.parent.stat().st_mode), 0o755)
        document = plistlib.loads(self.plist.read_bytes())
        for key in ("StandardOutPath", "StandardErrorPath"):
            log = Path(document[key])
            self.assertEqual(log.parent, self.root / "logs")
            self.assertEqual(stat.S_IMODE(log.stat().st_mode), 0o600)
        self.assertEqual(stat.S_IMODE((self.root / "logs").stat().st_mode), 0o700)
        before = self.plist.stat()
        again = self.install()
        self.assertFalse(again["changed"])
        self.assertIsNone(again["backup"])
        self.assertEqual((self.plist.stat().st_ino, self.plist.stat().st_mtime_ns),
                         (before.st_ino, before.st_mtime_ns))
        self.assertEqual(self.commands, [])

    def test_owned_update_keeps_private_original_backup(self):
        previous = plistlib.loads(installer.render(self.home))
        previous["ThrottleInterval"] = 30
        self.write_plist(previous)
        original = self.plist.read_bytes()
        original_inode = self.plist.stat().st_ino
        result = self.install()
        backup = Path(result["backup"])
        self.assertEqual(backup.read_bytes(), original)
        self.assertEqual(stat.S_IMODE(backup.stat().st_mode), 0o600)
        self.assertNotEqual(self.plist.stat().st_ino, original_inode)
        self.assertEqual(plistlib.loads(self.plist.read_bytes())["ThrottleInterval"], 10)

    def test_replace_failure_preserves_existing_plist(self):
        previous = plistlib.loads(installer.render(self.home))
        previous["ThrottleInterval"] = 30
        self.write_plist(previous)
        original = self.plist.read_bytes()
        with mock.patch.object(installer.os, "replace", side_effect=OSError("simulated failure")):
            with self.assertRaises(OSError):
                self.install()
        self.assertEqual(self.plist.read_bytes(), original)
        self.assertFalse(list(self.plist.parent.glob("*.tmp")))

    def test_unsafe_permissions_fail_before_creating_anything(self):
        for target, unsafe, safe in (
            (self.root, 0o755, 0o700),
            (self.key, 0o644, 0o600),
            (self.wrapper, 0o777, 0o700),
            (self.wrapper.parent, 0o777, 0o700),
        ):
            with self.subTest(target=target):
                target.chmod(unsafe)
                with self.assertRaises(installer.InstallError):
                    self.install()
                self.assertEqual(stat.S_IMODE(target.stat().st_mode), unsafe)
                self.assert_no_install()
                target.chmod(safe)

    def test_symlinked_required_files_are_not_followed_or_modified(self):
        for target in (self.wrapper, self.key):
            with self.subTest(target=target):
                saved = target.with_name(target.name + ".original")
                target.rename(saved)
                original = saved.read_bytes()
                target.symlink_to(saved)
                with self.assertRaises(installer.InstallError):
                    self.install()
                self.assertEqual(saved.read_bytes(), original)
                self.assertTrue(target.is_symlink())
                self.assert_no_install()
                target.unlink()
                saved.rename(target)

    def test_symlinked_parent_and_log_are_rejected_without_mutating_targets(self):
        outside = self.home / "outside"
        outside.mkdir(mode=0o700)
        (self.home / "Library").symlink_to(outside, target_is_directory=True)
        with self.assertRaises(installer.InstallError):
            self.install()
        self.assertEqual(list(outside.iterdir()), [])
        self.assert_no_install()
        (self.home / "Library").unlink()
        logs = self.root / "logs"
        logs.mkdir(mode=0o700)
        victim = outside / "victim"
        victim.write_text("unchanged")
        victim.chmod(0o600)
        (logs / "stdout.log").symlink_to(victim)
        with self.assertRaises(installer.InstallError):
            self.install()
        self.assertEqual(victim.read_text(), "unchanged")
        self.assertFalse(self.plist.exists())
        self.assertFalse((logs / "stderr.log").exists())

    def test_hardlinked_key_and_wrong_owner_are_rejected(self):
        os.link(self.key, self.root / "other-key-name")
        with self.assertRaises(installer.InstallError):
            self.install()
        self.assert_no_install()
        (self.root / "other-key-name").unlink()
        with self.assertRaises(installer.InstallError):
            installer.install(self.home, uid=os.getuid() + 1, platform="darwin")
        self.assert_no_install()

    def test_foreign_plist_is_never_overwritten(self):
        for field, value in (
            ("Label", "someone.elses.agent"),
            ("ProgramArguments", ["/bin/sh", "-c", "echo other work"]),
            ("EnvironmentVariables", {"USER_SECRET": "do-not-touch"}),
        ):
            with self.subTest(field=field):
                document = plistlib.loads(installer.render(self.home))
                document[field] = value
                self.write_plist(document)
                original = self.plist.read_bytes()
                with self.assertRaises(installer.InstallError):
                    self.install()
                self.assertEqual(self.plist.read_bytes(), original)
                self.assertFalse((self.root / "logs").exists())

    def test_malformed_plist_is_preserved_without_creating_logs(self):
        self.write_plist({"Label": installer.LABEL})
        malformed = b'<?xml version="1.0"?><plist><dict>'
        self.plist.write_bytes(malformed)
        with self.assertRaises(installer.InstallError):
            self.install()
        self.assertEqual(self.plist.read_bytes(), malformed)
        self.assertFalse((self.root / "logs").exists())

    def test_symlinked_plist_is_never_replaced(self):
        original = self.home / "original.plist"
        original.write_bytes(installer.render(self.home))
        original.chmod(0o600)
        self.plist.parent.mkdir(parents=True, mode=0o700)
        self.plist.symlink_to(original)
        with self.assertRaises(installer.InstallError):
            self.install()
        self.assertTrue(self.plist.is_symlink())
        self.assertEqual(original.read_bytes(), installer.render(self.home))
        self.assertFalse((self.root / "logs").exists())

    def test_explicit_load_bootstraps_only_absent_service_on_free_port(self):
        result = self.install(load=True)
        self.assertEqual(result["load_state"], "bootstrapped")
        self.assertEqual([command[1] for command in self.commands], ["print", "bootstrap"])

    def test_busy_port_blocks_load_before_writes(self):
        self.port_free = False
        with self.assertRaises(installer.InstallError):
            self.install(load=True)
        self.assertFalse(self.plist.exists())
        self.assertFalse((self.root / "logs").exists())
        self.assertEqual([command[1] for command in self.commands], ["print"])

    def test_registered_matching_service_is_not_restarted(self):
        self.install()
        self.registered()
        self.port_free = False
        result = self.install(load=True)
        self.assertEqual(result["load_state"], "already-registered")
        self.assertEqual([command[1] for command in self.commands], ["print"])
        previous = plistlib.loads(self.plist.read_bytes())
        previous["ThrottleInterval"] = 30
        self.write_plist(previous)
        result = self.install(load=True)
        self.assertEqual(result["load_state"], "restart-required")
        self.assertEqual([command[1] for command in self.commands], ["print", "print"])

    def test_foreign_registration_and_inspection_failure_fail_closed(self):
        self.registered(program="/bin/foreign")
        with self.assertRaises(installer.InstallError):
            self.install(load=True)
        self.assertFalse(self.plist.exists())
        self.assertFalse((self.root / "logs").exists())
        self.print_error = "Not permitted to inspect this domain"
        with self.assertRaises(installer.InstallError):
            self.install(load=True)
        self.assertEqual([command[1] for command in self.commands], ["print", "print"])
        self.assertFalse(self.plist.exists())


if __name__ == "__main__":
    unittest.main()
