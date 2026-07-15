use std::process::Command;

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_copilot-api")
}

fn temp_home(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "copilot-api-{label}-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ))
}

#[test]
fn auth_help_and_shell_completions_discover_all_providers() {
    let help = Command::new(binary())
        .args(["auth", "--help"])
        .output()
        .unwrap();
    assert!(help.status.success());
    let help = String::from_utf8_lossy(&help.stdout);
    for value in [
        "copilot",
        "codex",
        "opencode-go",
        "deepseek",
        "dashscope",
        "openrouter",
        "custom",
        "--api-key-env",
        "--capability",
        "--probe",
    ] {
        assert!(help.contains(value), "auth help is missing {value}");
    }

    let completion = Command::new(binary())
        .args(["completions", "bash"])
        .output()
        .unwrap();
    assert!(completion.status.success());
    let completion = String::from_utf8_lossy(&completion.stdout);
    assert!(completion.contains("copilot-api"));
    assert!(completion.contains("--provider"));
}

#[test]
fn non_interactive_custom_setup_separates_and_protects_credentials() {
    let home = temp_home("guided-auth");
    let secret = "credential-only-in-protected-store";
    let output = Command::new(binary())
        .args([
            "--api-home",
            home.to_str().unwrap(),
            "auth",
            "--provider",
            "custom",
            "--name",
            "fixture-provider",
            "--type",
            "openai-responses",
            "--base-url",
            "https://provider.example.com/api",
            "--api-key-env",
            "FIXTURE_PROVIDER_KEY",
            "--model",
            "gpt-fixture",
            "--capability",
            "responses,responses_compact,models,alpha_search",
        ])
        .env("FIXTURE_PROVIDER_KEY", secret)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains(secret));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(secret));

    let config = std::fs::read_to_string(home.join("config.json")).unwrap();
    assert!(!config.contains(secret));
    let config: serde_json::Value = serde_json::from_str(&config).unwrap();
    let provider = &config["providers"]["fixture-provider"];
    assert_eq!(provider["type"], "openai-responses");
    assert!(provider.get("apiKey").is_none());
    assert_eq!(
        provider["capabilities"],
        serde_json::json!(["responses", "responses_compact", "models", "alpha_search"])
    );

    let credential_path = home.join("provider_credentials.json");
    let credentials = std::fs::read_to_string(&credential_path).unwrap();
    assert!(credentials.contains(secret));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&credential_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    std::fs::remove_dir_all(home).unwrap();
}

#[test]
fn non_interactive_quick_provider_without_secret_fails_without_waiting() {
    let home = temp_home("missing-key");
    let output = Command::new(binary())
        .args([
            "--api-home",
            home.to_str().unwrap(),
            "auth",
            "--provider",
            "openrouter",
        ])
        .env_remove("COPILOT_API_PROVIDER_OPENROUTER_API_KEY")
        .env_remove("COPILOT_API_PROVIDER_API_KEY")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.contains("non-interactive"));
    assert!(output.contains("COPILOT_API_PROVIDER_OPENROUTER_API_KEY"));
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn debug_json_reports_client_and_plugin_versions_without_credentials() {
    let home = temp_home("debug-integrations");
    let output = Command::new(binary())
        .args(["--api-home", home.to_str().unwrap(), "debug", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["integrations"]["agentInjectPlugin"], "1.0.0");
    assert_eq!(value["integrations"]["toolSearchPlugin"], "1.0.0");
    assert_eq!(value["integrations"]["openCodeMarker"], "1.0.0");
    for client in ["claudeCode", "codexCli", "openCode"] {
        assert!(
            value["integrations"][client].is_null() || value["integrations"][client].is_string()
        );
    }
    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(!rendered.contains("credential-only-in-protected-store"));
    let _ = std::fs::remove_dir_all(home);
}
