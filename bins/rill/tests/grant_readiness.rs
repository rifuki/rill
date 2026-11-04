//! Check actual user-visible status rather than inspecting the source text.
#[test]
fn missing_global_run_set_distinguishes_granted_and_generic_execution() {
    let dir = std::env::temp_dir().join(format!("rill-readiness-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("config.json");
    std::fs::write(&config, r#"{"network":"testnet"}"#).unwrap();
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_rill-wallet"));
    for (name, _) in std::env::vars() {
        if name.starts_with("RILL_") || name.starts_with("SUI_") || name.starts_with("AGENT_") {
            command.env_remove(name);
        }
    }
    let result = command
        .arg("status")
        .env("RILL_CONFIG", &config)
        .output()
        .unwrap();
    let text = String::from_utf8(result.stdout).unwrap();
    assert!(text.contains("Owner-signed rill_run_action remains available with a valid grant"));
    assert!(text.contains("generic rill_execute requires RILL_RUN_SET_PATH"));
    assert!(!text.contains("none, so execution will refuse"));
}

/// With no config path there is no state directory, so the grant revision floor has nowhere to
/// live. That is said once, at startup, where the operator reads stderr, and the server still
/// serves its read-only tools.
#[test]
fn a_signer_without_a_state_directory_says_so_once_at_startup() {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_rill-wallet"));
    for (name, _) in std::env::vars() {
        if name.starts_with("RILL_") || name.starts_with("SUI_") || name.starts_with("AGENT_") {
            command.env_remove(name);
        }
    }
    let result = command
        .arg("mcp")
        .env_remove("HOME")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let stderr = String::from_utf8(result.stderr).unwrap();
    assert!(result.status.success(), "{stderr}");
    assert_eq!(
        stderr.matches("no state directory").count(),
        1,
        "the missing state directory is reported exactly once: {stderr}"
    );
}
