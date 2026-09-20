#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use tempfile::TempDir;

#[test]
fn inherited_update_cannot_run_an_installer_or_replace_the_fork() {
    let temporary = TempDir::new().unwrap();
    let bin = temporary.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let marker = temporary.path().join("installer-ran");
    let cargo = bin.join("cargo");
    fs::write(&cargo, "#!/bin/sh\n: > \"$INSTALL_MARKER\"\nexit 0\n").unwrap();
    fs::set_permissions(&cargo, fs::Permissions::from_mode(0o700)).unwrap();

    for executable in [
        env!("CARGO_BIN_EXE_ccrx"),
        env!("CARGO_BIN_EXE_codex-code-router"),
    ] {
        let result = Command::new(executable)
            .args(["update", "--no-restart"])
            .env_clear()
            .env("HOME", temporary.path())
            .env("PATH", &bin)
            .env("INSTALL_MARKER", &marker)
            .output()
            .unwrap();

        assert!(!marker.exists(), "update invoked an installer");
        assert!(!result.status.success(), "update must be rejected");
    }
}
