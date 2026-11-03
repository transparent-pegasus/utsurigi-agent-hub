#[path = "common/temp.rs"]
mod temp;

use serde_json::{Value, json};
use std::fs;
use std::path::Path;
use std::process::Command;
use temp::TempDir;

#[test]
fn public_defaults_match_the_exact_onboarding_schedule_and_feature_contract() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let document = fs::read_to_string(root.join("config.yaml")).unwrap();
    let defaults: Value = serde_saphyr::from_str(&document).unwrap();
    for (keys, expected) in [
        (vec!["version"], json!(1)),
        (vec!["hub"], json!("default")),
        (vec!["language"], Value::Null),
        (vec!["timezone"], Value::Null),
        (vec!["bot", "id"], Value::Null),
        (vec!["bot", "platform"], Value::Null),
        (vec!["bot", "runtime_id"], Value::Null),
        (vec!["bot", "role"], Value::Null),
        (vec!["machine", "id"], Value::Null),
        (vec!["machine", "root"], Value::Null),
        (vec!["storage", "provider"], json!("r2")),
        (vec!["storage", "class"], json!("standard")),
        (vec!["storage", "credentials_ref"], Value::Null),
        (vec!["storage", "bot_transfer"], Value::Null),
        (vec!["secrets", "provider"], json!("1password")),
        (vec!["secrets", "confirm_each_access"], json!(false)),
        (vec!["sync", "interval_seconds"], json!(86_400)),
        (vec!["sync", "start_time"], json!("03:00")),
        (vec!["sync", "history_retention_seconds"], json!(2_592_000)),
        (vec!["budget", "free_tier_only"], json!(true)),
        (vec!["budget", "max_bots"], json!(50)),
        (vec!["budget", "storage_target_gb"], json!(5)),
        (vec!["tasks", "enabled"], json!(true)),
        (vec!["memory", "enabled"], json!(false)),
        (vec!["memory", "embedding_model"], Value::Null),
        (vec!["memory", "retrieval_bot_id"], Value::Null),
        (vec!["memory", "existing_source"], Value::Null),
        (vec!["skills", "caveman", "enabled"], json!(true)),
        (
            vec!["skills", "caveman", "source"],
            json!("https://github.com/JuliusBrussee/caveman.git"),
        ),
        (vec!["skills", "caveman", "ref"], Value::Null),
        (vec!["sharing", "bot_data"], json!(true)),
        (vec!["budget", "attestation_file"], Value::Null),
        (vec!["review", "reviewer_bot"], Value::Null),
        (vec!["onboarding", "config_blob_url"], Value::Null),
    ] {
        let actual = keys
            .iter()
            .try_fold(&defaults, |value, key| value.get(*key));
        assert_eq!(actual, Some(&expected), "{keys:?}");
    }
}

#[test]
fn public_template_validates_without_runtime_secrets_and_runtime_validation_fails() {
    let temp = TempDir::new();
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("config.yaml"),
        temp.path().join("config.yaml"),
    )
    .unwrap();
    for (args, expected_success) in [
        (vec!["validate", "--template"], true),
        (vec!["validate"], false),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_utsurigi-agent-hub"))
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .current_dir(temp.path())
            .args(args)
            .output()
            .unwrap();
        assert_eq!(
            output.status.success(),
            expected_success,
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn template_status_names_configuration_as_next_action_without_creating_state() {
    let temp = TempDir::new();
    let config = temp.path().join("config.yaml");
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("config.yaml"),
        &config,
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_utsurigi-agent-hub"))
        .env_clear()
        .arg("--config")
        .arg(&config)
        .arg("status")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let status: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(status["ready"], false);
    assert_eq!(status["next_action"], "configure");
    assert!(
        status["remediation"]
            .as_array()
            .is_some_and(|items| !items.is_empty() && items.iter().all(Value::is_string))
    );
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
}

#[test]
fn private_configs_validate_with_or_without_machine_metadata_without_creating_its_root() {
    for machine in [
        None,
        Some(json!({"id": "host supplied / opaque identity", "root": "management"})),
    ] {
        let temp = TempDir::new();
        let file = temp.path().join("config.yaml");
        let working = temp.path().join("working");
        fs::create_dir(&working).unwrap();
        let mut config = json!({
            "version": 1,
            "timezone": "UTC",
            "bot": {"id": "alpha", "platform": "test", "runtime_id": "alpha-1"},
            "storage": {
                "provider": "local",
                "bot_transfer": "Store complete quiescent encrypted repositories in private Google Drive folders; recover only verified generations."
            },
            "repositories": {
                "shared": {"url": temp.path().join("repositories/shared"), "password_env": "TEST_SHARED_PASSWORD"},
                "bot": {"url": temp.path().join("repositories/alpha"), "password_env": "TEST_BOT_PASSWORD"}
            },
            "paths": {
                "owned": "management/owned", "shared": "management/shared",
                "imports": "management/imports", "state": "management/state"
            },
            "data": {"include": ["notes"]}
        });
        if let Some(machine) = &machine {
            config["machine"] = machine.clone();
        }
        fs::write(&file, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_utsurigi-agent-hub"))
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .current_dir(&working)
            .arg("--config")
            .arg(&file)
            .arg("validate")
            .output()
            .unwrap();
        assert!(!temp.path().join("management").exists());
        assert!(!working.join("management").exists());
        assert!(!temp.path().join("repositories").exists());
        assert!(
            output.status.success(),
            "machine={machine:?}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
