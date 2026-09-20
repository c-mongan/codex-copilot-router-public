use std::fs;
use std::process::Command;
use tempfile::{tempdir, NamedTempFile};

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_codex-code-router")
}

#[test]
fn print_token_writes_only_the_bearer_token_to_stdout() {
    let file = NamedTempFile::new().unwrap();
    fs::write(file.path(), r#"{"copilotToken":"secret-token"}"#).unwrap();

    let output = Command::new(binary())
        .arg("print-token")
        .env("COPILOT_TOKEN_FILE", file.path())
        .env_remove("COPILOT_BEARER_TOKEN")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "print-token should succeed with a valid Copilot token file; stderr was {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "secret-token",
        "Codex command-backed auth expects stdout to contain only the bearer token."
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "",
        "Successful token printing should not add diagnostics that Codex could mistake for auth data."
    );
}

#[test]
fn print_token_reports_missing_auth_on_stderr_without_stdout() {
    let dir = tempdir().unwrap();
    let missing_file = dir.path().join("missing-copilot-tokens.json");

    let output = Command::new(binary())
        .arg("print-token")
        .env("COPILOT_TOKEN_FILE", missing_file)
        .env_remove("COPILOT_BEARER_TOKEN")
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "print-token should fail loudly when no service-owned token is configured."
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "",
        "Failed token lookup must not print partial or placeholder auth data to stdout."
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("missing Copilot bearer token"),
        "The stderr diagnostic should explain the missing auth requirement."
    );
}
