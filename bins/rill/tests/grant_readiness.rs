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
