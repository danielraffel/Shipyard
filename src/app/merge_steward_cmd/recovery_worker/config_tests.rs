use super::*;
use crate::config::LocalOverlaySource;

fn required_check(context: &str) -> RecoveryFailureFact {
    RecoveryFailureFact::RequiredCheck {
        context: context.to_owned(),
        app_id: None,
        conclusion: "FAILURE".to_owned(),
        run_id: None,
    }
}

fn required_policy(context: &str) -> Vec<RecoveryRequiredCheck> {
    vec![RecoveryRequiredCheck {
        context: context.to_owned(),
        app_id: None,
    }]
}

fn config(contents: &str) -> LoadedConfig {
    LoadedConfig {
        data: contents.parse().expect("valid TOML fixture"),
        global_dir: PathBuf::from("/trusted"),
        project_dir: None,
        local_dir: None,
        local_overlay_source: LocalOverlaySource::None,
    }
}

fn valid_policy() -> LoadedConfig {
    config(&recovery_test_policy_toml(&recovery_test_repo_path()))
}

#[test]
fn prompt_contains_literal_closed_output_schema() {
    let request = RecoveryRequest::new(
        "Generous-Corp/pulp",
        42,
        "main",
        "0123456789abcdef0123456789abcdef01234567",
        "failure-fingerprint",
        "required check failed",
        required_policy("macos"),
        vec![required_check("macos")],
        "steward-policy",
        "worker-config",
    )
    .expect("request");

    let prompt = recovery_prompt("codex", &request).expect("prompt");
    let schema = &prompt["output_schema"];
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(schema["properties"]["schema_version"]["const"], 1);
    assert_eq!(schema["properties"]["verdict"]["const"], "escalate");
    assert_eq!(
        schema["properties"]["category"]["enum"],
        serde_json::json!([
            "compile", "test", "conflict", "security", "workflow", "infra", "unknown"
        ])
    );
    assert_eq!(
        schema["properties"]["confidence"]["enum"],
        serde_json::json!(["low", "medium", "high"])
    );
    for field in ["evidence", "candidate_paths", "focused_tests"] {
        assert_eq!(schema["properties"][field]["type"], "array");
        assert_eq!(schema["properties"][field]["maxItems"], 0);
    }
    assert_eq!(schema["required"].as_array().map(Vec::len), Some(7));
    assert!(
        prompt["task"]
            .as_str()
            .is_some_and(|task| task.contains("output_schema"))
    );
}

#[test]
fn policy_defaults_to_spark_and_constructs_exact_tool_disabled_argv() {
    let policy = RecoveryWorkerPolicy::from_config(&valid_policy()).expect("policy");
    assert_eq!(policy.first_line_model, DEFAULT_FIRST_LINE_MODEL);
    assert_eq!(
        policy.argv(),
        std::iter::once(recovery_test_codex_binary().display().to_string())
            .chain(
                [
                    "exec",
                    "-c",
                    FORCED_REASONING_CONFIG,
                    "--ephemeral",
                    "--ignore-user-config",
                    "--ignore-rules",
                    "--strict-config",
                    "--sandbox",
                    "read-only",
                    "--skip-git-repo-check",
                    "--color",
                    "never",
                ]
                .into_iter()
                .map(str::to_owned),
            )
            .chain(
                DISABLED_CODEX_FEATURES
                    .iter()
                    .copied()
                    .flat_map(|feature| ["--disable".to_owned(), feature.to_owned()])
            )
            .chain(
                ["--model", DEFAULT_FIRST_LINE_MODEL, "-"]
                    .into_iter()
                    .map(str::to_owned),
            )
            .collect::<Vec<_>>()
    );
    assert!(policy.allowed_repositories.contains("generous-corp/pulp"));
    assert!(policy.repo_paths.contains_key("generous-corp/pulp"));
    for forbidden in [
        "--approve-for-me",
        "--output-schema",
        "--output-last-message",
        "--image",
        "--profile",
        "resume",
    ] {
        assert!(!policy.argv().iter().any(|argument| argument == forbidden));
    }
}

#[test]
fn enqueue_policy_is_disabled_when_absent_or_explicitly_off() {
    assert!(
        enqueue_policy(&config("[merge_steward]\nauto_handoff = true\n"))
            .expect("absent policy")
            .is_none()
    );
    assert!(
        enqueue_policy(&config(
            "[merge_steward.recovery_worker]\nenabled = false\n",
        ))
        .expect("disabled policy")
        .is_none()
    );

    let malformed_shape = config("[merge_steward]\nrecovery_worker = \"disabled\"\n");
    assert!(enqueue_policy(&malformed_shape).is_err());

    let disabled_unknown =
        config("[merge_steward.recovery_worker]\nenabled = false\nprofile = \"unsafe-profile\"\n");
    let error = enqueue_policy(&disabled_unknown).expect_err("disabled unknown key rejected");
    assert!(error.message().contains("unsupported field(s): profile"));

    for malformed in [
        "provider = 7",
        "provider = \"openrouter\"",
        "first_line_model = \"unsafe model\"",
        "codex_binary = \"relative/codex\"",
        "codex_binary = \"/usr/local/bin/wrapper\"",
        "codex_home = \"relative/home\"",
        "timeout_seconds = \"120\"",
        "max_attempts_per_head = 2",
        "max_log_tail_bytes = 999999",
        "allowed_repositories = [7]",
        "repo_paths = \"not-a-table\"",
    ] {
        let disabled = config(&format!(
            "[merge_steward.recovery_worker]\nenabled = false\n{malformed}\n"
        ));
        assert!(
            enqueue_policy(&disabled).is_err(),
            "disabled malformed field must fail closed: {malformed}"
        );
    }

    let mut fully_configured_disabled = valid_policy();
    fully_configured_disabled
        .data
        .get_mut("merge_steward")
        .and_then(toml::Value::as_table_mut)
        .and_then(|table| table.get_mut("recovery_worker"))
        .and_then(toml::Value::as_table_mut)
        .expect("section")
        .insert("enabled".to_owned(), toml::Value::Boolean(false));
    assert!(
        enqueue_policy(&fully_configured_disabled)
            .expect("fully configured disabled policy")
            .is_none()
    );
}

#[test]
fn policy_rejects_non_global_shape_and_attempt_expansion() {
    let missing = config("[merge_steward]\nauto_handoff = true\n");
    assert!(RecoveryWorkerPolicy::from_config(&missing).is_err());

    let mut attempts = valid_policy();
    attempts
        .data
        .get_mut("merge_steward")
        .and_then(toml::Value::as_table_mut)
        .and_then(|table| table.get_mut("recovery_worker"))
        .and_then(toml::Value::as_table_mut)
        .expect("section")
        .insert("max_attempts_per_head".to_owned(), toml::Value::Integer(2));
    let error = RecoveryWorkerPolicy::from_config(&attempts).expect_err("attempts rejected");
    assert!(error.message().contains("1..=1"));
}

#[test]
fn policy_rejects_configurable_command_and_repo_map_drift() {
    let mut unknown = valid_policy();
    unknown
        .data
        .get_mut("merge_steward")
        .and_then(toml::Value::as_table_mut)
        .and_then(|table| table.get_mut("recovery_worker"))
        .and_then(toml::Value::as_table_mut)
        .expect("section")
        .insert(
            "command".to_owned(),
            toml::Value::Array(vec![
                toml::Value::String("agent".to_owned()),
                toml::Value::String("{model}".to_owned()),
                toml::Value::String("{request}".to_owned()),
                toml::Value::String("{token}".to_owned()),
            ]),
        );
    assert!(RecoveryWorkerPolicy::from_config(&unknown).is_err());

    let mut unknown_field = valid_policy();
    unknown_field
        .data
        .get_mut("merge_steward")
        .and_then(toml::Value::as_table_mut)
        .and_then(|table| table.get_mut("recovery_worker"))
        .and_then(toml::Value::as_table_mut)
        .expect("section")
        .insert(
            "profile".to_owned(),
            toml::Value::String("unsafe-profile".to_owned()),
        );
    let error = RecoveryWorkerPolicy::from_config(&unknown_field).expect_err("unknown key");
    assert!(error.message().contains("unsupported field(s): profile"));

    let mut drift = valid_policy();
    drift
        .data
        .get_mut("merge_steward")
        .and_then(toml::Value::as_table_mut)
        .and_then(|table| table.get_mut("recovery_worker"))
        .and_then(toml::Value::as_table_mut)
        .expect("section")
        .get_mut("repo_paths")
        .and_then(toml::Value::as_table_mut)
        .expect("paths")
        .insert(
            "Generous-Corp/forge".to_owned(),
            toml::Value::String("/Volumes/Workshop/Code/forge".to_owned()),
        );
    assert!(RecoveryWorkerPolicy::from_config(&drift).is_err());
}

#[test]
fn policy_rejects_non_codex_provider() {
    let mut wrong_provider = valid_policy();
    wrong_provider
        .data
        .get_mut("merge_steward")
        .and_then(toml::Value::as_table_mut)
        .and_then(|table| table.get_mut("recovery_worker"))
        .and_then(toml::Value::as_table_mut)
        .expect("section")
        .insert(
            "provider".to_owned(),
            toml::Value::String("openrouter".to_owned()),
        );
    let error = RecoveryWorkerPolicy::from_config(&wrong_provider).expect_err("provider rejected");
    assert!(error.message().contains("provider=\"codex\""));
}

#[test]
fn policy_rejects_a_script_wrapper_named_codex() {
    let temp = tempfile::tempdir().expect("tempdir");
    let wrapper = temp
        .path()
        .join(if cfg!(windows) { "codex.exe" } else { "codex" });
    fs::write(&wrapper, "#!/bin/sh\nexit 0\n").expect("wrapper fixture");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&wrapper).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&wrapper, permissions).expect("executable wrapper");
    }
    let mut configured = valid_policy();
    configured
        .data
        .get_mut("merge_steward")
        .and_then(toml::Value::as_table_mut)
        .and_then(|table| table.get_mut("recovery_worker"))
        .and_then(toml::Value::as_table_mut)
        .expect("policy")
        .insert(
            "codex_binary".to_owned(),
            toml::Value::String(wrapper.to_string_lossy().into_owned()),
        );

    let error = RecoveryWorkerPolicy::from_config(&configured).expect_err("wrapper rejected");

    assert!(error.message().contains("script or wrapper"));
}

#[test]
fn claim_policy_refresh_observes_machine_config_drift() {
    let temp = tempfile::tempdir().expect("tempdir");
    let global_dir = temp.path().join("global");
    fs::create_dir_all(&global_dir).expect("global dir");
    let original = valid_policy();
    fs::write(
        global_dir.join("config.toml"),
        toml::to_string(&original.data).expect("serialize original policy"),
    )
    .expect("write original policy");
    let (_, expected_signature, _) =
        RecoveryWorkerPolicy::load(&global_dir).expect("load original policy");
    assert!(matches!(
        RecoveryWorkerPolicy::refresh_for_claim(&global_dir, &expected_signature)
            .expect("refresh current policy"),
        ClaimPolicyRefresh::Current(_)
    ));

    let mut drifted = original;
    drifted
        .data
        .get_mut("merge_steward")
        .and_then(toml::Value::as_table_mut)
        .and_then(|table| table.get_mut("recovery_worker"))
        .and_then(toml::Value::as_table_mut)
        .expect("policy section")
        .insert(
            "first_line_model".to_owned(),
            toml::Value::String("gpt-5.3-codex-spark-next".to_owned()),
        );
    fs::write(
        global_dir.join("config.toml"),
        toml::to_string(&drifted.data).expect("serialize drifted policy"),
    )
    .expect("write drifted policy");
    assert!(matches!(
        RecoveryWorkerPolicy::refresh_for_claim(&global_dir, &expected_signature)
            .expect("refresh drifted policy"),
        ClaimPolicyRefresh::Drifted { .. }
    ));
}
