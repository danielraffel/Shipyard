#[cfg(unix)]
use std::{
    io::Write,
    process::{Command, Stdio},
};

#[cfg(unix)]
use super::command::{
    auth_token_command, github_api_auth_header_setup, resolver_auth_token_command,
};
use super::*;

fn host(ssh: Option<&str>, shipyard_bin: Option<&str>) -> HostClassConfig {
    let github_cli = match (ssh, shipyard_bin) {
        (Some(_), Some(binary)) => binary.rsplit_once('/').map_or_else(
            || "ghapp".to_owned(),
            |(parent, _)| format!("{parent}/ghapp"),
        ),
        (_, Some(binary)) => Path::new(binary).parent().map_or_else(
            || "ghapp".to_owned(),
            |parent| parent.join("ghapp").display().to_string(),
        ),
        _ => "/Users/ci/.local/bin/ghapp".to_owned(),
    };
    HostClassConfig {
        class: "m5".to_owned(),
        ssh: ssh.map(str::to_owned),
        cap: 2,
        tart_bin: "/opt/homebrew/bin/tart".to_owned(),
        tartci_bin: "/Users/ci/.local/bin/tartci".to_owned(),
        shipyard_bin: shipyard_bin.map(str::to_owned),
        shipyard_mode: Some("shipyard".to_owned()),
        shipyard_global_dir: Some("/Users/ci/Library/Application Support/shipyard".to_owned()),
        shipyard_state_dir: Some("/Users/ci/Library/Application Support/shipyard".to_owned()),
        github_cli: Some(github_cli),
        github_token_helper: Some(
            "/Users/ci/.config/shipyard/bin/shipyard-github-app-token".to_owned(),
        ),
        tart_home: Some("/Users/ci/VMs".to_owned()),
        labels: Vec::new(),
    }
}

#[test]
fn remote_plan_uses_absolute_binary_and_minimal_canonical_path() {
    let plan = host_update_plan(
        &host(Some("m5-lan"), Some("/Users/ci/.local/bin/shipyard")),
        "v0.137.0",
    )
    .expect("plan");
    assert!(plan.command.starts_with("/usr/bin/env -i HOME=\"$HOME\""));
    assert!(plan.command.contains("/opt/homebrew/bin"));
    assert!(plan.command.contains("/usr/bin/perl -e"));
    assert!(plan.command.contains("TERM"));
    assert!(plan.command.contains("KILL"));
    assert!(plan.command.contains("waitpid"));
    assert!(plan.command.contains(&format!(
        " {} /bin/bash -c",
        REMOTE_UPDATE_TIMEOUT.as_secs()
    )));
    assert!(plan.command.contains("/Users/ci/.local/bin/shipyard"));
    assert!(plan.command.contains("/Users/ci/.local/bin/ghapp"));
    assert!(plan.command.contains("auth helper-argv"));
    assert!(plan.command.contains("/usr/sbin/lsof"));
    assert!(plan.command.contains("/bin/ps -ww"));
    assert!(
        plan.command
            .contains("json.loads(os.environ[\"DAEMON_AUTH_PROBE\"])")
    );
    assert!(plan.command.contains("credential_argv"));
    assert!(!plan.command.contains("test -n \"$daemon_auth_probe\""));
    assert!(
        plan.command
            .contains("test \"$auth_generation_share\" = \"$HOME/.local/share\"")
    );
    assert!(plan.command.contains(
        "test /Users/ci/.config/shipyard/bin/shipyard-github-app-token = \"$HOME/.config/shipyard/bin/shipyard-github-app-token\""
    ));
    assert!(
        plan.command
            .contains(&format!("Shipyard/{}/install.sh", "2".repeat(40)))
    );
    assert!(plan.command.contains("releases/assets/11"));
    assert!(plan.command.contains("@$auth_header"));
    assert!(plan.command.contains("application/octet-stream"));
    assert!(!plan.command.contains("/releases/download/"));
    assert!(!plan.command.contains("-H @-"));
    assert!(!plan.command.contains("/usr/bin/printf 'Authorization:"));
    assert!(plan.command.contains(&"a".repeat(64)));
    assert!(plan.command.contains(&"6".repeat(64)));
    assert!(plan.command.contains("--mode shipyard"));
    assert!(
        plan.command
            .contains("/Users/ci/Library/Application Support/shipyard")
    );
    assert!(
        plan.command
            .contains("update --to v0.137.0 --check --unattended-fleet")
    );
    assert_eq!(
        plan.companion_binary,
        PathBuf::from("/Users/ci/.local/bin/shipyard-workstream-provider")
    );
    assert!(plan.companion_required);
    assert_eq!(plan.source_identity, "8".repeat(64));
    assert!(plan.command.contains("/usr/bin/shasum -a 256"));
    assert!(plan.command.contains(REMOTE_BEFORE_STATUS_PREFIX));
    assert!(plan.command.contains(REMOTE_REFRESH_PREFIX));
    assert!(plan.command.contains(REMOTE_AFTER_STATUS_PREFIX));
    assert!(plan.command.contains(REMOTE_AUTHORITY_ID_PREFIX));
    assert!(plan.command.contains(REMOTE_RELEASE_ASSET_SHA256_PREFIX));
    let status_probe = plan.command.find("before_status=").expect("status probe");
    let auth = plan.command.find("token=").expect("auth boundary");
    assert!(
        status_probe < auth,
        "status must fail before auth and install"
    );
    assert!(!plan.command.contains("observed_before="));
    let preflight = plan
        .command
        .find("staged_binary")
        .expect("staged authenticated preflight");
    let replacement = plan
        .command
        .find("SHIPYARD_INSTALL_DIR=\"$auth_generation_stage\"")
        .expect("immutable generation install destination");
    assert!(
        preflight < replacement,
        "governed config and helper must pass before binary replacement"
    );
}

#[cfg(unix)]
#[test]
fn remote_daemon_auth_probe_executes_the_typed_parser_in_a_scrubbed_environment() {
    let plan = host_update_plan(
        &host(Some("m5-lan"), Some("/Users/ci/.local/bin/shipyard")),
        "v0.137.0",
    )
    .expect("plan");
    let quoted_remote_script = plan
        .command
        .rsplit_once(" /bin/bash -c ")
        .map(|(_, script)| script)
        .expect("quoted remote script");
    let decoded = Command::new("/bin/bash")
        .args(["-c", &format!("printf '%s' {quoted_remote_script}")])
        .env_clear()
        .env("HOME", "/Users/ci")
        .env("PATH", "/usr/bin:/bin")
        .output()
        .expect("decode exact remote script");
    assert!(decoded.status.success());
    let remote_script = String::from_utf8(decoded.stdout).expect("UTF-8 remote script");
    for probe in [
        "auth_generation_selector=\"$(/usr/bin/readlink /Users/ci/.local/bin/ghapp.shipyard-generation)\"",
        "auth_generation_selector_recheck=\"$(/usr/bin/readlink /Users/ci/.local/bin/ghapp.shipyard-generation)\"",
        "test \"$(/usr/bin/readlink /Users/ci/.local/bin/ghapp.shipyard-generation)\" = \"$auth_generation_selector\"",
    ] {
        assert!(
            remote_script.contains(probe),
            "remote generation and daemon receipt probes must read the stable trampoline selector",
        );
    }
    let parser_start = remote_script
        .find("DAEMON_AUTH_PROBE=\"$daemon_auth_probe\" /usr/bin/python3")
        .expect("remote daemon auth parser start");
    let parser_tail = &remote_script[parser_start..];
    let parser_end = parser_tail
        .find("\nPY\n")
        .map(|offset| offset + "\nPY\n".len())
        .expect("remote daemon auth parser end");
    let parser = &parser_tail[..parser_end];
    let run = |payload: &str| {
        Command::new("/bin/bash")
            .args([
                "-c",
                &format!("daemon_auth_probe={}\n{parser}", shlex_quote(payload)),
            ])
            .env_clear()
            .env("HOME", "/Users/ci")
            .env("PATH", "/usr/bin:/bin")
            .status()
            .expect("remote daemon auth parser probe")
    };

    let valid = serde_json::json!({
        "schema_version": 1,
        "command": "auth.helper-argv",
        "wrapper": "/Users/ci/.local/bin/ghapp",
        "repo": "danielraffel/Shipyard",
        "credential_argv": [
            "--app-id",
            "123456",
            "--private-key",
            "/Users/ci/.config/shipyard/github-app.pem",
        ],
    })
    .to_string();
    assert!(run(&valid).success(), "valid typed receipt must pass");

    let long_app_id = serde_json::json!({
        "schema_version": 1,
        "command": "auth.helper-argv",
        "wrapper": "/Users/ci/.local/bin/ghapp",
        "repo": "danielraffel/Shipyard",
        "credential_argv": [
            "--app-id",
            "123456789012345678901",
            "--private-key",
            "/Users/ci/.config/shipyard/github-app.pem",
        ],
    })
    .to_string();
    assert!(!run(&long_app_id).success(), "oversized app ID must refuse");

    assert!(
        !run(r#"{"token":"nonempty-but-untyped"}"#).success(),
        "nonempty JSON without the typed receipt contract must refuse"
    );
    assert!(
        !run("nonempty-not-json").success(),
        "nonempty malformed JSON must refuse"
    );
}

#[cfg(unix)]
#[test]
fn remote_generation_context_probe_executes_exact_mode_and_global_dir_checks() {
    let plan = host_update_plan(
        &host(Some("m5-lan"), Some("/Users/ci/.local/bin/shipyard")),
        "v0.137.0",
    )
    .expect("plan");
    let quoted_remote_script = plan
        .command
        .rsplit_once(" /bin/bash -c ")
        .map(|(_, script)| script)
        .expect("quoted remote script");
    let decoded = Command::new("/bin/bash")
        .args(["-c", &format!("printf '%s' {quoted_remote_script}")])
        .env_clear()
        .env("HOME", "/Users/ci")
        .env("PATH", "/usr/bin:/bin")
        .output()
        .expect("decode exact remote script");
    assert!(decoded.status.success());
    let remote_script = String::from_utf8(decoded.stdout).expect("UTF-8 remote script");
    let parser_start = remote_script
        .find("import json, sys\npath, expected_mode, expected_global_dir")
        .expect("remote generation context parser start");
    let parser_tail = &remote_script[parser_start..];
    let parser_end = parser_tail.find("\nPY\n").expect("context parser end");
    let parser = &parser_tail[..parser_end];
    let temp = tempfile::tempdir().expect("temp dir");
    let context = temp.path().join("ghapp.shipyard-context.json");
    let generation = "7".repeat(64);
    let authority = "8".repeat(64);
    let run = |mode: &str, global_dir: &str| {
        let value = serde_json::json!({
            "schema_version": 2,
            "mode": mode,
            "global_dir": global_dir,
            "generation_id": generation,
            "authority_identity": authority,
        });
        std::fs::write(&context, value.to_string()).expect("context fixture");
        let mut child = Command::new("/usr/bin/python3")
            .args([
                "-",
                context.to_str().expect("context path"),
                "shipyard",
                "/Users/ci/Library/Application Support/shipyard",
                &generation,
                &authority,
            ])
            .env_clear()
            .env("HOME", "/Users/ci")
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::piped())
            .spawn()
            .expect("generation context parser");
        child
            .stdin
            .as_mut()
            .expect("parser stdin")
            .write_all(parser.as_bytes())
            .expect("parser body");
        child.wait().expect("parser status")
    };

    assert!(
        run("shipyard", "/Users/ci/Library/Application Support/shipyard").success(),
        "valid exact context must pass"
    );
    assert!(
        !run("direct", "/Users/ci/Library/Application Support/shipyard").success(),
        "wrong mode must refuse"
    );
    assert!(
        !run("shipyard", "/different/governed/root").success(),
        "wrong global directory must refuse"
    );
}

#[test]
fn fleet_plan_requires_ghapp_and_shipyard_to_be_siblings() {
    let mut class = host(Some("m5-lan"), Some("/Users/ci/.local/bin/shipyard"));
    class.github_cli = Some("/Users/ci/bin/ghapp".to_owned());
    let error = host_update_plan(&class, "v0.137.0").expect_err("foreign wrapper directory");
    assert!(error.message.contains("ghapp sibling of shipyard_bin"));

    class.github_cli = Some("/Users/ci/.local/bin/renamed-ghapp".to_owned());
    let error = host_update_plan(&class, "v0.137.0").expect_err("foreign wrapper name");
    assert!(error.message.contains("ghapp sibling of shipyard_bin"));
}

#[test]
fn fleet_resolver_probe_uses_exact_global_dir_before_commit() {
    let mut class = host(Some("m5-lan"), Some("/Users/ci/.local/bin/shipyard"));
    class.shipyard_global_dir = Some("/Users/ci/governed global".to_owned());
    class.shipyard_state_dir = Some("/Users/ci/governed state".to_owned());

    let plan = host_update_plan(&class, "v0.137.0").expect("differing governed dirs");
    assert!(plan.command.contains("auth_resolver_required=1"));
    assert!(!plan.command.contains("ghapp auth token"));

    assert!(plan.command.contains("/Users/ci/governed global"));
    assert!(plan.command.contains("/Users/ci/governed state"));
    assert!(plan.command.contains("$auth_wrapper.shipyard-context.json"));
    assert!(
        plan.command
            .contains("--global-dir \"$auth_global_dir\" auth helper-argv")
    );
    assert_eq!(plan.command.matches("auth helper-argv").count(), 3);
    let bootstrap_probe = plan
        .command
        .find("auth helper-argv")
        .expect("bootstrap resolver probe");
    let target_selected = plan
        .command
        .find("auth_write_phase target-selected")
        .expect("atomic selector marker");
    let committed = plan
        .command
        .find("auth_write_phase committed")
        .expect("commit marker");
    let post_install_probe = plan
        .command
        .match_indices("auth helper-argv")
        .map(|(offset, _)| offset)
        .filter(|offset| *offset < committed)
        .last()
        .expect("post-install resolver probe");
    assert!(
        bootstrap_probe < target_selected,
        "bootstrap resolver must acquire auth before artifact installation"
    );
    assert!(target_selected < post_install_probe);
    assert!(post_install_probe < committed);
}

#[cfg(unix)]
#[test]
fn auth_token_command_binds_verified_repo_in_a_scrubbed_non_checkout() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("tempdir");
    let wrapper = temp.path().join("auth wrapper");
    std::fs::write(
        &wrapper,
        "#!/bin/sh\n\
         test \"$#\" -eq 2\n\
         test \"$1\" = auth\n\
         test \"$2\" = token\n\
         test \"$GH_REPO\" = danielraffel/Shipyard\n\
         test -z \"${SHIPYARD_GHAPP_REPO:-}${SHIPYARD_GH_APP_REPO:-}\"\n\
         printf '%s\\n' exact-token\n",
    )
    .expect("wrapper fixture");
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700))
        .expect("wrapper executable");

    let token_command = auth_token_command("danielraffel/Shipyard", &wrapper);
    let probe = format!(
        "token=\"$({token_command})\"; test \"$token\" = exact-token; test -z \"${{GH_REPO:-}}\""
    );
    let status = Command::new("/bin/bash")
        .args(["-c", &probe])
        .current_dir(temp.path())
        .env_clear()
        .status()
        .expect("scrubbed token probe");
    assert!(status.success());
}

#[cfg(unix)]
#[test]
#[allow(clippy::too_many_lines)] // One end-to-end fixture covers the complete fail-closed parser boundary.
fn resolver_auth_token_command_uses_typed_machine_credentials_in_a_scrubbed_environment() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("tempdir");
    let binary = temp.path().join("shipyard");
    let wrapper = temp.path().join("ghapp");
    let global_dir = temp.path().join("global dir");
    let private_key = temp.path().join("private-key.pem");
    let wrapper_invoked = temp.path().join("wrapper-invoked");
    std::fs::create_dir(&global_dir).expect("global dir");
    std::fs::write(&private_key, "fixture-only").expect("private key fixture");

    let resolver_payload = serde_json::json!({
        "schema_version": 1,
        "command": "auth.helper-argv",
        "wrapper": wrapper.display().to_string(),
        "repo": "danielraffel/Shipyard",
        "credential_argv": [
            "--app-id",
            "000123456",
            "--private-key",
            private_key.display().to_string(),
        ],
    })
    .to_string();
    std::fs::write(
        &binary,
        format!(
            "#!/bin/sh\n\
             test \"$1\" = --mode\n\
             test \"$2\" = shipyard\n\
             test \"$3\" = --global-dir\n\
             test \"$4\" = '{}'\n\
             test \"$5\" = auth\n\
             test \"$6\" = helper-argv\n\
             test \"$7\" = --wrapper\n\
             test \"$8\" = '{}'\n\
             test \"$9\" = --repo\n\
             test \"${{10}}\" = danielraffel/Shipyard\n\
             printf '%s\\n' '{}'\n",
            global_dir.display(),
            wrapper.display(),
            resolver_payload,
        ),
    )
    .expect("resolver fixture");
    let legacy_token = format!(
        "ghs_APP-ID_{}.{}.{}",
        "a".repeat(120),
        "b".repeat(120),
        "c".repeat(120)
    );
    assert!(legacy_token.len() > 255);
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\n\
             /usr/bin/touch '{}'\n\
             test \"$#\" -eq 7\n\
             test \"$1\" = token\n\
             test \"$2\" = --app-id\n\
             test \"$3\" = 000123456\n\
             test \"$4\" = --private-key\n\
             test \"$5\" = '{}'\n\
             test \"$6\" = --repo\n\
             test \"$7\" = danielraffel/Shipyard\n\
             test -z \"${{GH_REPO:-}}${{SHIPYARD_GHAPP_REPO:-}}${{SHIPYARD_GH_APP_REPO:-}}\"\n\
             printf '%s\\n' '{{\"token\":\"exact-token\"}}'\n",
            wrapper_invoked.display(),
            private_key.display(),
        ),
    )
    .expect("wrapper fixture");
    for executable in [&binary, &wrapper] {
        std::fs::set_permissions(executable, std::fs::Permissions::from_mode(0o700))
            .expect("fixture executable");
    }

    let token_command = resolver_auth_token_command(
        &binary,
        "shipyard",
        &global_dir,
        "v0.131.0",
        "danielraffel/Shipyard",
        &wrapper,
    );
    let probe = format!("token=\"$({token_command})\"; test \"$token\" = exact-token");
    let status = Command::new("/bin/bash")
        .args(["-c", &probe])
        .current_dir(temp.path())
        .env_clear()
        .env("HOME", temp.path())
        .env("PATH", "/usr/bin:/bin")
        .status()
        .expect("scrubbed resolver probe");
    assert!(status.success());
    assert!(wrapper_invoked.exists());
    std::fs::remove_file(&wrapper_invoked).expect("clear wrapper marker");

    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\n\
             /usr/bin/touch '{}'\n\
             test \"$#\" -eq 7\n\
             test \"$1\" = token\n\
             test \"$2\" = --app-id\n\
             test \"$3\" = 000123456\n\
             test \"$4\" = --private-key\n\
             test \"$5\" = '{}'\n\
             test \"$6\" = --repo\n\
             test \"$7\" = danielraffel/Shipyard\n\
             test -z \"${{GH_REPO:-}}${{SHIPYARD_GHAPP_REPO:-}}${{SHIPYARD_GH_APP_REPO:-}}\"\n\
             printf '%s\\n' '{}'\n",
            wrapper_invoked.display(),
            private_key.display(),
            legacy_token,
        ),
    )
    .expect("legacy wrapper fixture");
    let probe = format!("token=\"$({token_command})\"; test \"$token\" = '{legacy_token}'");
    let status = Command::new("/bin/bash")
        .args(["-c", &probe])
        .current_dir(temp.path())
        .env_clear()
        .env("HOME", temp.path())
        .env("PATH", "/usr/bin:/bin")
        .status()
        .expect("legacy wrapper bootstrap probe");
    assert!(status.success());
    assert!(wrapper_invoked.exists());
    std::fs::remove_file(&wrapper_invoked).expect("clear legacy wrapper marker");

    for legacy_output in [
        "ghp_0123456789abcdefghijklmnopqrstuv\\n",
        "ghs_too-short\\n",
        "ghs_0123456789abcdefghijklmnopqrstuv\\nextra\\n",
        "ghs_0123456789abcdefghijklmnopqrstuv",
    ] {
        std::fs::write(
            &wrapper,
            format!("#!/bin/sh\nprintf '%b' '{legacy_output}'\n"),
        )
        .expect("invalid legacy wrapper fixture");
        let output = Command::new("/bin/bash")
            .args(["-c", &token_command])
            .current_dir(temp.path())
            .env_clear()
            .env("HOME", temp.path())
            .env("PATH", "/usr/bin:/bin")
            .output()
            .expect("invalid legacy wrapper probe");
        assert!(!output.status.success(), "legacy output must refuse");
        assert!(output.stdout.is_empty());
        assert!(!String::from_utf8_lossy(&output.stderr).contains(legacy_output));
        assert!(String::from_utf8_lossy(&output.stderr).contains("malformed JSON or legacy token"));
    }

    for valid_but_wrong_json in ["\"ghs_0123456789abcdefghijklmnopqrstuvwxyz\"", "[]"] {
        std::fs::write(
            &wrapper,
            format!("#!/bin/sh\nprintf '%s\\n' '{valid_but_wrong_json}'\n"),
        )
        .expect("wrong-shape JSON wrapper fixture");
        let output = Command::new("/bin/bash")
            .args(["-c", &token_command])
            .current_dir(temp.path())
            .env_clear()
            .env("HOME", temp.path())
            .env("PATH", "/usr/bin:/bin")
            .output()
            .expect("wrong-shape JSON wrapper probe");
        assert!(!output.status.success(), "wrong-shape JSON must refuse");
        assert!(output.stdout.is_empty());
        assert!(!String::from_utf8_lossy(&output.stderr).contains(valid_but_wrong_json));
        assert!(String::from_utf8_lossy(&output.stderr).contains("malformed token"));
    }

    std::fs::write(
        &wrapper,
        format!("#!/bin/sh\nprintf '%s\\n' '{legacy_token}'\nexit 2\n"),
    )
    .expect("failed legacy wrapper fixture");
    let output = Command::new("/bin/bash")
        .args(["-c", &token_command])
        .current_dir(temp.path())
        .env_clear()
        .env("HOME", temp.path())
        .env("PATH", "/usr/bin:/bin")
        .output()
        .expect("failed legacy wrapper probe");
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(!String::from_utf8_lossy(&output.stderr).contains(&legacy_token));
    assert!(String::from_utf8_lossy(&output.stderr).contains("wrapper refused"));

    std::fs::write(&binary, "#!/bin/sh\nprintf '{'\n").expect("malformed resolver fixture");
    let output = Command::new("/bin/bash")
        .args(["-c", &token_command])
        .env_clear()
        .env("HOME", temp.path())
        .env("PATH", "/usr/bin:/bin")
        .output()
        .expect("malformed resolver probe");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("shipyard fleet auth resolver returned malformed JSON")
    );
    assert!(!wrapper_invoked.exists());

    std::fs::write(
        &binary,
        format!("#!/bin/sh\nprintf '%s\\n' '{resolver_payload}'\nexit 2\n"),
    )
    .expect("failed resolver with plausible output fixture");
    let output = Command::new("/bin/bash")
        .args(["-o", "pipefail", "-c", &token_command])
        .env_clear()
        .env("HOME", temp.path())
        .env("PATH", "/usr/bin:/bin")
        .output()
        .expect("failed resolver with plausible output probe");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(
            "predeploy v0.131.0 with ordinary shipyard update and migrate machine-global"
        )
    );
    assert!(!wrapper_invoked.exists());

    let invalid_payloads = [
        (
            serde_json::json!({
                "schema_version": true,
                "command": "auth.helper-argv",
                "wrapper": wrapper.display().to_string(),
                "repo": "danielraffel/Shipyard",
                "credential_argv": ["--app-id", "123456", "--private-key", private_key.display().to_string()],
            }),
            "shipyard fleet auth resolver returned an unsupported contract",
        ),
        (
            serde_json::json!({
                "schema_version": 1,
                "command": "auth.helper-argv",
                "wrapper": wrapper.display().to_string(),
                "repo": "danielraffel/Shipyard",
                "credential_argv": ["--app-id", "123456", "--private-key", private_key.display().to_string()],
                "extra": "refuse",
            }),
            "shipyard fleet auth resolver returned an unsupported contract",
        ),
        (
            serde_json::json!({
                "schema_version": 1,
                "command": "auth.helper-argv",
                "wrapper": "/foreign/ghapp",
                "repo": "danielraffel/Shipyard",
                "credential_argv": ["--app-id", "123456", "--private-key", private_key.display().to_string()],
            }),
            "shipyard fleet auth resolver returned mismatched authority",
        ),
        (
            serde_json::json!({
                "schema_version": 1,
                "command": "auth.helper-argv",
                "wrapper": wrapper.display().to_string(),
                "repo": "Generous-Corp/pulp",
                "credential_argv": ["--app-id", "123456", "--private-key", private_key.display().to_string()],
            }),
            "shipyard fleet auth resolver returned mismatched authority",
        ),
        (
            serde_json::json!({
                "schema_version": 1,
                "command": "auth.helper-argv",
                "wrapper": wrapper.display().to_string(),
                "repo": "danielraffel/Shipyard",
                "credential_argv": ["--app-id", "123456", "--private-key", format!("/{}", "x".repeat(4096))],
            }),
            "shipyard fleet auth resolver returned an invalid private-key path",
        ),
        (
            serde_json::json!({
                "schema_version": 1,
                "command": "auth.helper-argv",
                "wrapper": wrapper.display().to_string(),
                "repo": "danielraffel/Shipyard",
                "credential_argv": ["--app-id", "123456", "--private-key", "/private\nkey"],
            }),
            "shipyard fleet auth resolver returned an invalid private-key path",
        ),
    ];
    for (payload, expected_error) in invalid_payloads {
        std::fs::write(&binary, format!("#!/bin/sh\nprintf '%s\\n' '{payload}'\n"))
            .expect("invalid resolver fixture");
        let output = Command::new("/bin/bash")
            .args(["-o", "pipefail", "-c", &token_command])
            .env_clear()
            .env("HOME", temp.path())
            .env("PATH", "/usr/bin:/bin")
            .output()
            .expect("invalid resolver probe");
        assert!(!output.status.success(), "payload must refuse: {payload}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected_error),
            "unexpected refusal for payload {payload}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!wrapper_invoked.exists());
    }

    std::fs::write(
        &binary,
        "#!/bin/sh\n/usr/bin/python3 -c 'import sys; sys.stdout.write(\"x\" * 16385)'\n",
    )
    .expect("oversized resolver fixture");
    let output = Command::new("/bin/bash")
        .args(["-o", "pipefail", "-c", &token_command])
        .env_clear()
        .env("HOME", temp.path())
        .env("PATH", "/usr/bin:/bin")
        .output()
        .expect("oversized resolver probe");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("shipyard fleet auth resolver response exceeds 16384 bytes")
    );
    assert!(!wrapper_invoked.exists());
}

#[cfg(target_os = "macos")]
#[test]
fn remote_pair_probe_rejects_mixed_or_malformed_preinstall_state() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("temp dir");
    let primary = temp.path().join("shipyard");
    let companion = temp.path().join(COMPANION_BINARY_NAME);
    let write_binary = |path: &Path, label: &str, version: &str| {
        std::fs::write(
            path,
            format!("#!/bin/sh\nprintf '%s\\n' '{label} {version}'\n"),
        )
        .expect("fixture");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("executable");
    };
    write_binary(&primary, "shipyard", "0.126.2");
    let legacy_probe = remote_pair_probe(&primary, &companion, "before", None, false);
    assert!(
        Command::new("/bin/bash")
            .args(["-c", &legacy_probe])
            .status()
            .expect("legacy probe")
            .success()
    );

    write_binary(&companion, COMPANION_BINARY_NAME, "0.137.0");
    assert!(
        !Command::new("/bin/bash")
            .args(["-c", &legacy_probe])
            .status()
            .expect("mixed probe")
            .success()
    );

    write_binary(&primary, "shipyard", "0.137.0");
    let paired_probe = remote_pair_probe(&primary, &companion, "before", None, false);
    assert!(
        Command::new("/bin/bash")
            .args(["-c", &paired_probe])
            .status()
            .expect("paired probe")
            .success()
    );

    for malformed in [
        "0.126.",
        "0.0126.3",
        "0.137.0.1",
        "18446744073709551616.0.0",
    ] {
        write_binary(&primary, "shipyard", malformed);
        write_binary(&companion, COMPANION_BINARY_NAME, malformed);
        let malformed_probe = remote_pair_probe(&primary, &companion, "before", None, false);
        assert!(
            !Command::new("/bin/bash")
                .args(["-c", &malformed_probe])
                .status()
                .expect("malformed probe")
                .success(),
            "malformed preinstall version {malformed:?} must fail before rollout"
        );
    }
}

#[cfg(unix)]
#[test]
fn local_plan_preserves_host_class_daemon_context() {
    let mut class = host(None, Some("/Users/ci/.local/bin/shipyard"));
    class.shipyard_mode = Some("isolated".to_owned());
    class.shipyard_global_dir = Some("/tmp/governed config".to_owned());
    class.shipyard_state_dir = Some("/tmp/governed state".to_owned());
    let plan = host_update_plan(&class, "v0.137.0").expect("plan");
    let command = local_update_command(&plan);

    assert!(command.contains("auth_mode=isolated"));
    assert!(command.contains("auth_global_dir='/tmp/governed config'"));
    assert!(command.contains("auth_state_dir='/tmp/governed state'"));
    assert!(command.contains("SHIPYARD_INSTALL_DIR=\"$auth_generation_stage\""));
    assert!(!command.contains("--refresh-daemon"));
    let resolver_probe = command
        .find("auth helper-argv --wrapper")
        .expect("resolver probe");
    let committed = command
        .find("auth_write_phase committed")
        .expect("transaction commit");
    let daemon_refresh = command
        .find("--json daemon refresh")
        .expect("daemon refresh");
    let lock_release = command.rfind("auth_release_lock").expect("lock release");
    assert_eq!(command.matches("--json daemon refresh").count(), 1);
    assert!(command.contains("--json daemon refresh 9>&-"));
    assert!(!command[..committed].contains("--json daemon refresh"));
    assert!(resolver_probe < committed);
    assert!(committed < daemon_refresh);
    assert!(daemon_refresh < lock_release);
    assert!(command.contains("fleet-auth-support.transaction"));
    assert!(command.contains("-H \"@$auth_header\" -H 'Accept: application/octet-stream'"));
    assert!(
        command.contains("https://api.github.com/repos/danielraffel/Shipyard/releases/assets/11")
    );
    assert!(!command.contains("/releases/download/"));
    assert!(!command.contains("-H @-"));
    assert!(!command.contains("/usr/bin/printf 'Authorization:"));
}

#[cfg(unix)]
#[test]
fn rendered_plans_contain_only_the_token_resolver_not_token_material() {
    let plan = host_update_plan(
        &host(None, Some("/Users/ci/.local/bin/shipyard")),
        "v0.137.0",
    )
    .expect("plan");
    let mut json = Vec::new();
    render_plan(&mut json, true, "v0.137.0", &[plan], false).expect("rendered plan");
    let rendered = String::from_utf8(json).expect("UTF-8 plan");

    assert!(rendered.contains("auth helper-argv"));
    assert!(rendered.contains("releases/assets/11"));
    assert!(!rendered.contains("ghs_top_secret_fixture"));
    assert!(!rendered.contains("Authorization: Bearer ghs_"));
    assert!(!rendered.contains("/releases/download/"));
}

#[cfg(unix)]
#[test]
fn github_api_header_is_private_and_token_never_reaches_output() {
    let temp = tempfile::tempdir().expect("temp");
    let script = format!(
        "set -euo pipefail; staging_dir={}; token=\"$SECRET_FIXTURE\"; {}; test \"$(/bin/cat \"$auth_header\")\" = \"Authorization: Bearer $SECRET_FIXTURE\"; printf '%s\\n' safe-output",
        shlex_quote(&temp.path().display().to_string()),
        github_api_auth_header_setup(),
    );
    let output = Command::new("/bin/bash")
        .args(["-c", &script])
        .env("SECRET_FIXTURE", "ghs_top_secret_fixture")
        .output()
        .expect("header setup");
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout), "safe-output\n");
    assert!(output.stderr.is_empty());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("ghs_top_secret_fixture"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("ghs_top_secret_fixture"));

    let setup = github_api_auth_header_setup();
    assert!(setup.contains("printf 'Authorization: Bearer %s\\n' \"$token\""));
    assert!(!setup.contains("/usr/bin/printf"));
    assert!(!setup.contains("curl"));
}

#[cfg(unix)]
#[test]
fn github_api_header_rejects_missing_or_malformed_tokens_before_download() {
    let temp = tempfile::tempdir().expect("temp");
    for token_assignment in ["token=''", "token='token with space'"] {
        let script = format!(
            "set -euo pipefail; staging_dir={}; {token_assignment}; {}; touch {}/unexpected-download",
            shlex_quote(&temp.path().display().to_string()),
            github_api_auth_header_setup(),
            shlex_quote(&temp.path().display().to_string()),
        );
        let output = Command::new("/bin/bash")
            .args(["-c", &script])
            .output()
            .expect("rejected token");
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
        assert!(!temp.path().join("unexpected-download").exists());
        assert!(!temp.path().join("github-api-header").exists());
    }
}

#[cfg(unix)]
#[test]
fn exact_asset_shim_serves_authenticated_and_unauthenticated_installer_urls() {
    let temp = tempfile::tempdir().expect("temp");
    let shim = temp.path().join("curl-shim");
    std::fs::write(&shim, exact_asset_curl_shim("shipyard-macos-arm64")).expect("shim");
    let asset = temp.path().join("verified-asset");
    let output = Command::new("/bin/bash")
        .arg(&shim)
        .arg("https://api.github.com/repos/example/shipyard/releases/tags/v1.2.3")
        .env("SHIPYARD_FLEET_ASSET_PATH", &asset)
        .output()
        .expect("run shim");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 response");
    let payload: Value =
        serde_json::from_str(stdout.lines().next().expect("release response JSON line"))
            .expect("release response JSON");
    let expected = format!("file://{}", asset.display());
    assert_eq!(
        payload.pointer("/assets/0/url").and_then(Value::as_str),
        Some(expected.as_str())
    );
    assert_eq!(
        payload
            .pointer("/assets/0/browser_download_url")
            .and_then(Value::as_str),
        Some(expected.as_str())
    );
}

#[test]
fn stripped_path_is_launch_environment_drift_not_absence() {
    let error = host_update_plan(&host(Some("m5"), None), "v0.137.0")
        .expect_err("remote relative lookup must fail closed");
    assert!(error.message.contains("launch-environment drift"));
    assert!(!error.message.contains("not installed"));
    assert!(!error.message.contains("absent"));
}

#[test]
fn option_like_ssh_destination_is_rejected_before_spawn() {
    let error = host_update_plan(
        &host(
            Some("-oProxyCommand=/tmp/untrusted"),
            Some("/Users/ci/.local/bin/shipyard"),
        ),
        "v0.137.0",
    )
    .expect_err("SSH option injection");
    assert!(error.message.contains("not a valid SSH destination"));
}

#[test]
fn auth_support_paths_reject_dot_and_parent_components() {
    let mut class = host(Some("m5"), Some("/Users/ci/.local/bin/shipyard"));
    class.github_token_helper =
        Some("/Users/ci/.config/shipyard/bin/../shipyard-github-app-token".to_owned());
    let error = host_update_plan(&class, "v0.137.0")
        .expect_err("parent component must fail before command construction");
    assert!(
        error
            .message
            .contains("must not contain dot or parent components")
    );

    class.github_token_helper =
        Some("/Users/ci/.config/shipyard/./bin/shipyard-github-app-token".to_owned());
    let error = host_update_plan(&class, "v0.137.0")
        .expect_err("dot component must fail before command construction");
    assert!(
        error
            .message
            .contains("must not contain dot or parent components")
    );
}

#[test]
fn daemon_context_paths_reject_controls_dot_and_parent_components() {
    for global_dir in ["/Users/ci/global/../foreign", "/Users/ci/global\nforeign"] {
        let mut class = host(Some("m5-lan"), Some("/Users/ci/.local/bin/shipyard"));
        class.shipyard_global_dir = Some(global_dir.to_owned());
        let error = host_update_plan(&class, "v0.137.0").expect_err("unsafe global dir");
        assert!(error.message.contains("normalized absolute paths"));
    }
}

#[test]
fn auth_support_paths_reject_managed_binary_and_transaction_collisions() {
    let mut class = host(Some("m5"), Some("/Users/ci/.local/bin/shipyard"));
    class.github_token_helper = Some("/Users/ci/.local/bin/shipyard".to_owned());
    let error = host_update_plan(&class, "v0.137.0")
        .expect_err("primary binary collision must fail before rollout");
    assert!(error.message.contains("must not overlap managed binaries"));

    class.github_token_helper =
        Some("/Users/ci/.local/bin/shipyard-workstream-provider".to_owned());
    let error = host_update_plan(&class, "v0.137.0")
        .expect_err("companion binary collision must fail before rollout");
    assert!(error.message.contains("must not overlap managed binaries"));

    class.github_token_helper =
        Some("/Users/ci/.local/bin/shipyard.shipyard-rollback.tmp".to_owned());
    let error = host_update_plan(&class, "v0.137.0")
        .expect_err("atomic backup temp collision must fail before rollout");
    assert!(error.message.contains("must not overlap managed binaries"));

    class.github_token_helper = Some(
        "/Users/ci/Library/Application Support/shipyard/fleet-auth-support.transaction".to_owned(),
    );
    let error = host_update_plan(&class, "v0.137.0")
        .expect_err("journal collision must fail before rollout");
    assert!(error.message.contains("or transaction state"));

    class.github_token_helper =
        Some("/Users/ci/Library/Application Support/shipyard/fleet-auth-support.guard".to_owned());
    let error = host_update_plan(&class, "v0.137.0")
        .expect_err("advisory guard collision must fail before rollout");
    assert!(error.message.contains("or transaction state"));
}

#[test]
fn remote_bootstrap_requires_absolute_governed_auth_helper() {
    let mut config = host(Some("m5-lan"), Some("/Users/ci/.local/bin/shipyard"));
    config.github_cli = Some("ghapp".to_owned());
    let error = host_update_plan(&config, "v0.137.0").expect_err("relative helper");
    assert!(
        error
            .message
            .contains("auth helper and wrapper paths must be distinct absolute paths")
    );
}

#[test]
fn remote_rollout_requires_an_explicit_daemon_context() {
    let mut config = host(Some("m5-lan"), Some("/Users/ci/.local/bin/shipyard"));
    config.shipyard_state_dir = None;
    let error = host_update_plan(&config, "v0.137.0").expect_err("missing context");
    assert!(
        error
            .message
            .contains("shipyard_state_dir is required to identify the daemon context")
    );
}

#[test]
fn remote_bootstrap_rejects_a_filename_the_installer_cannot_replace() {
    let error = host_update_plan(
        &host(Some("m5-lan"), Some("/Users/ci/.local/bin/current")),
        "v0.137.0",
    )
    .expect_err("renamed binary");
    assert!(error.message.contains("must end in /shipyard"));
}

#[cfg(unix)]
#[test]
fn local_rollout_rejects_a_filename_the_installer_cannot_replace() {
    let error = host_update_plan(
        &host(None, Some("/Users/ci/.local/bin/current")),
        "v0.137.0",
    )
    .expect_err("renamed local binary");
    assert!(error.message.contains("must end in /shipyard"));
}

#[cfg(unix)]
#[test]
fn one_stalled_host_is_terminated_at_its_bound() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("temp dir");
    let binary = temp.path().join("shipyard");
    std::fs::write(
        &binary,
        "#!/bin/sh\ncase \"$*\" in *\"daemon status\"*) printf '%s\\n' '{\"command\":\"daemon:status\",\"running\":false}' ;; *) sleep 60 ;; esac\n",
    )
    .expect("fixture");
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).expect("executable");
    let plan = host_update_plan(&host(None, binary.to_str()), "v0.137.0").expect("plan");
    assert!(matches!(
        execute_plan_with_timeout(&plan, Duration::from_millis(50)),
        Err(PlanExecutionError::TimedOut(_))
    ));
}

#[cfg(unix)]
#[test]
fn configured_absolute_tool_survives_a_stripped_non_login_path() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("temp dir");
    let homebrew_bin = temp.path().join("opt/homebrew/bin");
    std::fs::create_dir_all(&homebrew_bin).expect("homebrew bin");
    let tart = homebrew_bin.join("tart");
    std::fs::write(&tart, "#!/bin/sh\nexit 0\n").expect("tool fixture");
    std::fs::set_permissions(&tart, std::fs::Permissions::from_mode(0o755))
        .expect("executable fixture");
    let shipyard = homebrew_bin.join("shipyard");
    std::fs::write(&shipyard, "#!/bin/sh\nexit 0\n").expect("Shipyard fixture");
    std::fs::set_permissions(&shipyard, std::fs::Permissions::from_mode(0o755))
        .expect("executable Shipyard fixture");

    let hidden = Command::new("/usr/bin/env")
        .args([
            "-i",
            "PATH=/usr/bin:/bin",
            "/bin/sh",
            "-c",
            "command -v tart",
        ])
        .output()
        .expect("stripped-path probe");
    assert!(
        !hidden.status.success(),
        "ambient lookup must miss the fixture"
    );

    let plan = host_update_plan(&host(Some("m5-lan"), shipyard.to_str()), "v0.137.0")
        .expect("an absolute profile path remains authoritative");
    assert_eq!(plan.binary, shipyard);
    assert!(
        plan.command
            .contains(&shlex_quote(&shipyard.display().to_string()))
    );
}

#[test]
fn exact_release_tag_is_required() {
    assert!(normalize_exact_tag("v0.130.0").is_err());
    assert!(normalize_exact_tag("v0.131.0").is_err());
    assert!(normalize_exact_tag("v0.136.0").is_err());
    assert_eq!(normalize_exact_tag("0.137.0").expect("tag"), "v0.137.0");
    assert!(normalize_exact_tag("v0.99.0").is_err());
    assert!(normalize_exact_tag("v0.98.1").is_err());
    assert!(normalize_exact_tag("latest").is_err());
    assert!(normalize_exact_tag("v0.98").is_err());
    assert!(normalize_exact_tag("v0.98.1-rc1").is_err());
    assert!(normalize_exact_tag("v18446744073709551616.0.0").is_err());
    assert!(!tag_requires_companion("v0.126.2"));
    assert!(tag_requires_companion("v0.137.0"));
    assert!(!tag_supports_auth_resolver("v0.128.9"));
    assert!(!tag_supports_auth_resolver("v0.129.0"));
    assert!(!tag_supports_auth_resolver("v0.130.0"));
    assert!(!tag_supports_auth_resolver("v0.130.1"));
    assert!(tag_supports_auth_resolver("v0.131.0"));
}

#[test]
fn companion_pairing_applies_to_a_bounded_tag_range() {
    const REMOVED_AT: [u64; 3] = [0, 200, 0];
    let bounded =
        |tag: &str| companion_required_for_tag(tag, MIN_PAIRED_BINARY_TARGET, Some(REMOVED_AT));

    // Below the pairing floor the companion was never published.
    assert!(!bounded("v0.126.2"));
    // Inside the paired range those releases really do ship a companion, so it
    // must still be present, mode-checked, and digest-matched.
    assert!(bounded("v0.127.0"));
    assert!(bounded("v0.199.0"));
    assert!(bounded("v0.199.7"));
    // At or above the removal tag the companion was never published either, so
    // requiring it would verify a file the release cannot contain.
    assert!(!bounded("v0.200.0"));
    assert!(!bounded("v0.201.3"));
    assert!(!bounded("v1.0.0"));
    // An open upper bound keeps every paired target paired.
    assert!(companion_required_for_tag(
        "v0.200.0",
        MIN_PAIRED_BINARY_TARGET,
        None
    ));

    // Tripwire: the shipped gate is still open-ended because releases continue
    // to publish the companion. Arming `FIRST_TAG_WITHOUT_COMPANION` reddens
    // these two, and must land with the packaging and installer changes that
    // actually stop publishing it.
    assert!(tag_requires_companion("v0.199.0"));
    assert!(tag_requires_companion("v0.200.0"));
}

#[test]
fn fleet_plan_refuses_pre_atomic_generation_targets_before_rendering() {
    let class = host(Some("m5-lan"), Some("/Users/ci/.local/bin/shipyard"));
    for target in ["v0.130.0", "v0.131.0", "v0.132.0", "v0.136.0"] {
        let error = host_update_plan(&class, target).expect_err("pre-contract target");
        assert!(
            error
                .message
                .contains("sibling-guard auth generation capability")
        );
    }
    host_update_plan(&class, "v0.137.0").expect("atomic generation target");
}

#[cfg(target_os = "macos")]
#[test]
fn real_auth_transaction_publishes_the_atomic_generation_contract() {
    use std::os::unix::fs::PermissionsExt;

    use sha2::{Digest, Sha256};

    let temp = tempfile::tempdir().expect("transaction home");
    let bin = temp.path().join(".local/bin");
    let helper_dir = temp.path().join(".config/shipyard/bin");
    let state = temp.path().join("Library/Application Support/shipyard");
    std::fs::create_dir_all(&bin).expect("bin");
    std::fs::create_dir_all(&helper_dir).expect("helper dir");
    std::fs::create_dir_all(&state).expect("state");

    let helper = helper_dir.join("shipyard-github-app-token");
    let wrapper = bin.join("ghapp");
    let binary = bin.join("shipyard");
    let companion = bin.join(COMPANION_BINARY_NAME);
    let helper_source = temp.path().join("release-helper");
    let wrapper_source = temp.path().join("release-ghapp");
    let close_guard_source = temp.path().join("release-close-guard");
    let binary_source = temp.path().join("release-shipyard");
    let executable = |path: &Path, bytes: &[u8]| {
        std::fs::write(path, bytes).expect("write executable");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .expect("executable mode");
    };
    executable(
        &helper_source,
        b"#!/bin/sh\n/usr/bin/printf '{\"token\":\"fixture\"}\\n'\n",
    );
    executable(&wrapper_source, include_bytes!("../../../scripts/ghapp"));
    executable(
        &close_guard_source,
        include_bytes!("../../../scripts/ghapp_pr_close_guard.py"),
    );
    executable(&binary_source, b"#!/bin/sh\nexit 0\n");

    let digest = |path: &Path| {
        let bytes = std::fs::read(path).expect("digest input");
        format!("{:x}", Sha256::digest(bytes))
    };
    let mut authority = test_release_authority("v0.137.0");
    authority.auth_helper.sha256 = digest(&helper_source);
    authority.auth_wrapper.sha256 = digest(&wrapper_source);
    authority.pr_close_guard.sha256 = digest(&close_guard_source);
    let install_binary = format!(
        "/bin/cp {} \"$auth_generation_stage/shipyard\"; /bin/chmod 700 \"$auth_generation_stage/shipyard\"; /bin/cp \"$auth_generation_stage/shipyard\" \"$auth_generation_stage/{COMPANION_BINARY_NAME}\"",
        shlex_quote(&binary_source.display().to_string())
    );
    let script = auth_support::install_transaction(
        &helper,
        &wrapper,
        &binary,
        &companion,
        true,
        true,
        &shlex_quote(&helper_source.display().to_string()),
        &shlex_quote(&wrapper_source.display().to_string()),
        &shlex_quote(&close_guard_source.display().to_string()),
        &install_binary,
        "shipyard",
        &state,
        &state,
        "danielraffel/Shipyard",
        &authority,
        "",
        false,
    );
    let status = Command::new("/bin/bash")
        .args(["-c", &format!("set -Eeuo pipefail\n{script}")])
        .env_clear()
        .env("HOME", temp.path())
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .status()
        .expect("real auth transaction");
    assert!(status.success());

    assert!(!wrapper.is_symlink());
    let wrapper_target = std::fs::read_link(wrapper.with_extension("shipyard-generation"))
        .expect("wrapper generation selector");
    let generation = wrapper_target.parent().expect("generation directory");
    assert_eq!(
        std::fs::read(&wrapper).expect("stable public trampoline"),
        std::fs::read(generation.join("ghapp.public-trampoline"))
            .expect("generation public trampoline")
    );
    assert_eq!(
        std::fs::read_link(&binary).expect("binary selector"),
        generation.join("shipyard")
    );
    assert_eq!(
        std::fs::read_link(&helper).expect("helper selector"),
        generation.join("shipyard-github-app-token")
    );
    let manifest = std::fs::read_to_string(generation.join("generation.manifest"))
        .expect("generation manifest");
    assert_eq!(manifest.lines().count(), 17);
    assert!(generation.join("pr-close-guard").is_file());
    assert!(
        manifest
            .lines()
            .any(|line| line == "generation_contract=auth-selector-v2")
    );
}

/// Scripted host side effects for [`apply_plans`].
struct TestOps<E, V, R> {
    execute: E,
    verify: V,
    rollback: R,
    /// What `installed_version` reports for every host.
    installed: Option<String>,
    rollbacks: Vec<(String, Option<String>)>,
}

impl<E, V, R> HostOps for TestOps<E, V, R>
where
    E: FnMut(&HostUpdatePlan) -> Result<HostUpdateEvidence, PlanExecutionError>,
    V: FnMut(&HostUpdatePlan, u32) -> verify::HostVerification,
    R: FnMut(&HostUpdatePlan, Option<&str>) -> rollback::RollbackOutcome,
{
    fn installed_version(&mut self, _plan: &HostUpdatePlan) -> Option<String> {
        self.installed.clone()
    }
    fn execute(&mut self, plan: &HostUpdatePlan) -> Result<HostUpdateEvidence, PlanExecutionError> {
        (self.execute)(plan)
    }
    fn verify(&mut self, plan: &HostUpdatePlan, daemon_pid: u32) -> verify::HostVerification {
        (self.verify)(plan, daemon_pid)
    }
    fn rollback(
        &mut self,
        plan: &HostUpdatePlan,
        previous: Option<&str>,
    ) -> rollback::RollbackOutcome {
        self.rollbacks
            .push((plan.class.clone(), previous.map(ToOwned::to_owned)));
        (self.rollback)(plan, previous)
    }
}

fn rolled_back_ok(plan: &HostUpdatePlan, previous: Option<&str>) -> rollback::RollbackOutcome {
    rollback::RollbackOutcome::RolledBack {
        to: format!("v{}", previous.unwrap_or("0.136.0")),
        verification: Box::new(verified_ok(plan, 1)),
    }
}

fn apply_plans_for_test<F>(
    plans: &[HostUpdatePlan],
    target: &str,
    json: bool,
    output: &mut Vec<u8>,
    execute: F,
) -> Result<ExitCode, RolloutFailure>
where
    F: FnMut(&HostUpdatePlan) -> Result<HostUpdateEvidence, PlanExecutionError>,
{
    let mut ops = TestOps {
        execute,
        verify: verified_ok,
        rollback: rolled_back_ok,
        installed: Some("0.136.0".to_owned()),
        rollbacks: Vec::new(),
    };
    apply_plans(plans, &[], target, json, output, &mut ops)
}

fn verified_ok(plan: &HostUpdatePlan, pid: u32) -> verify::HostVerification {
    let version = plan.target.trim_start_matches('v');
    verify::judge(
        plan,
        pid,
        &format!(
            "SHIPYARD_VERIFY_CLI=shipyard {version}\n\
             SHIPYARD_VERIFY_DAEMON={{\"command\":\"daemon:status\",\"running\":true,\"shipyard_version\":\"{version}\"}}\n\
             SHIPYARD_VERIFY_DAEMON_PID={pid}\n\
             SHIPYARD_VERIFY_DAEMON_PID_ALIVE=1\n\
             SHIPYARD_VERIFY_GUARDS=unsupported\n"
        ),
    )
}

fn named_host(name: &str) -> HostClassConfig {
    let mut class = host(Some(name), Some("/Users/ci/.local/bin/shipyard"));
    name.clone_into(&mut class.class);
    class
}

fn pair(version: &str, verified: bool) -> BinaryPairEvidence {
    let source_identity = verified.then(|| "8".repeat(64));
    let source_identity_basis = if verified {
        SourceIdentityBasis::VerifiedReleaseAuthority
    } else {
        SourceIdentityBasis::UnverifiedPreinstall
    };
    let primary = BinaryEvidence {
        path: PathBuf::from("/Users/ci/.local/bin/shipyard"),
        semantic_version: version.to_owned(),
        sha256: "a".repeat(64),
        source_identity: source_identity.clone(),
        source_identity_basis,
    };
    let companion = tag_requires_companion(&format!("v{version}")).then(|| BinaryEvidence {
        path: PathBuf::from("/Users/ci/.local/bin/shipyard-workstream-provider"),
        semantic_version: version.to_owned(),
        sha256: "b".repeat(64),
        source_identity,
        source_identity_basis,
    });
    BinaryPairEvidence { primary, companion }
}

fn evidence(version: &str) -> HostUpdateEvidence {
    let mut auth_support_after = auth_support(true);
    if !tag_requires_companion(&format!("v{version}")) {
        auth_support_after
            .generation
            .as_mut()
            .expect("verified generation")
            .companion = None;
    }
    HostUpdateEvidence {
        release_authority_identity: "8".repeat(64),
        release_asset_sha256: "6".repeat(64),
        executable_sha256: "a".repeat(64),
        cli_version: format!("shipyard {version}"),
        before_pair: pair(version, false),
        after_pair: pair(version, true),
        auth_support_before: auth_support(false),
        auth_support_after,
        daemon_version: version.to_owned(),
        daemon_pid: 42,
        daemon_runtime: daemon_runtime(),
        configured_repos_before: Some(vec!["owner/repo".to_owned()]),
        configured_repos_after: vec!["owner/repo".to_owned()],
        configured_repos_preserved: Some(true),
    }
}

fn daemon_runtime() -> DaemonRuntimeEvidence {
    DaemonRuntimeEvidence {
        pid: 42,
        loaded_executable_path: PathBuf::from("/Users/ci/.local/share/shipyard/auth-generations")
            .join("7".repeat(64))
            .join("shipyard"),
        loaded_executable_sha256: "a".repeat(64),
        rendered_launch_sha256: "1".repeat(64),
        loaded_launch_sha256: "1".repeat(64),
        machine_auth_probe_sha256: "2".repeat(64),
        machine_auth_generation_id: "7".repeat(64),
    }
}

fn auth_support(verified: bool) -> AuthSupportEvidence {
    let generation_dir =
        PathBuf::from("/Users/ci/.local/share/shipyard/auth-generations").join("7".repeat(64));
    let file = |path: &str, digest: char, blob: char| SupportFileEvidence {
        path: PathBuf::from(path),
        generation_target: verified.then(|| {
            PathBuf::from("/Users/ci/.local/share/shipyard/auth-generations")
                .join("7".repeat(64))
                .join(Path::new(path).file_name().expect("support file name"))
        }),
        sha256: Some(digest.to_string().repeat(64)),
        mode: Some(if verified { 0o700 } else { 0o755 }),
        source_blob_oid: verified.then(|| blob.to_string().repeat(40)),
        source_identity: verified.then(|| "8".repeat(64)),
        source_identity_basis: if verified {
            SourceIdentityBasis::VerifiedReleaseAuthority
        } else {
            SourceIdentityBasis::UnverifiedPreinstall
        },
    };
    AuthSupportEvidence {
        helper: file(
            "/Users/ci/.config/shipyard/bin/shipyard-github-app-token",
            'c',
            'b',
        ),
        wrapper: file("/Users/ci/.local/bin/ghapp", 'e', 'd'),
        generation: verified.then(|| GenerationEvidence {
            generation_contract: "auth-selector-v2".to_owned(),
            generation_id: "7".repeat(64),
            authority_identity: "8".repeat(64),
            selector_path: PathBuf::from("/Users/ci/.local/bin/ghapp.shipyard-generation"),
            selector_target: generation_dir.join("ghapp"),
            selector_recheck_target: generation_dir.join("ghapp"),
            manifest: generation_member(&generation_dir, "generation.manifest", '9', 0o600),
            helper: generation_member(&generation_dir, "shipyard-github-app-token", 'c', 0o700),
            wrapper: generation_member(&generation_dir, "ghapp", 'e', 0o700),
            public_trampoline: generation_member(
                &generation_dir,
                "ghapp.public-trampoline",
                'f',
                0o700,
            ),
            close_guard: generation_member(&generation_dir, "pr-close-guard", '0', 0o700),
            binary: generation_member(&generation_dir, "shipyard", 'a', 0o700),
            companion: Some(generation_member(
                &generation_dir,
                COMPANION_BINARY_NAME,
                'b',
                0o700,
            )),
            context: Some(generation_member(
                &generation_dir,
                "ghapp.shipyard-context.json",
                'd',
                0o600,
            )),
        }),
    }
}

fn generation_member(
    generation_dir: &Path,
    name: &str,
    digest: char,
    mode: u32,
) -> GenerationMemberEvidence {
    GenerationMemberEvidence {
        path: generation_dir.join(name),
        sha256: digest.to_string().repeat(64),
        mode,
    }
}

#[test]
fn host_selection_is_explicit_ordered_and_fail_closed() {
    let classes = vec![named_host("m1"), named_host("m3"), named_host("m5")];
    let error = select_host_classes(&classes, &[], false).expect_err("implicit fleet");
    assert!(error.message.contains("explicit --all-hosts"));

    let selected = select_host_classes(&classes, &["m5".to_owned(), "m1".to_owned()], false)
        .expect("selected subset");
    assert_eq!(
        selected
            .iter()
            .map(|class| class.class.as_str())
            .collect::<Vec<_>>(),
        ["m5", "m1"]
    );

    let duplicate = select_host_classes(&classes, &["m1".to_owned(), "m1".to_owned()], false)
        .expect_err("duplicate");
    assert!(duplicate.message.contains("more than once"));
    let unknown =
        select_host_classes(&classes, &["studio".to_owned()], false).expect_err("unknown");
    assert!(unknown.message.contains("configured classes: m1, m3, m5"));
    assert!(select_host_classes(&classes, &["m1".to_owned()], true).is_err());
    assert_eq!(
        select_host_classes(&classes, &[], true)
            .expect("explicit all")
            .len(),
        3
    );
}

#[test]
fn apply_stops_before_every_later_host_after_first_failure() {
    let plans = ["m1", "m3", "m5"]
        .iter()
        .map(|name| host_update_plan(&named_host(name), "v0.137.0").expect("plan"))
        .collect::<Vec<_>>();
    let mut attempted = Vec::new();
    let mut output = Vec::new();
    let error = apply_plans_for_test(&plans, "v0.137.0", true, &mut output, |plan| {
        attempted.push(plan.class.clone());
        if plan.class == "m3" {
            Err(PlanExecutionError::Failed("controlled failure".to_owned()))
        } else {
            Ok(evidence("0.137.0"))
        }
    })
    .expect_err("apply stops");
    assert_eq!(attempted, ["m1", "m3"]);
    assert!(error.failure.message.contains("stopped after m3"));
    let rendered = String::from_utf8(output).expect("UTF-8");
    let receipts = serde_json::Deserializer::from_str(&rendered)
        .into_iter::<Value>()
        .collect::<Result<Vec<_>, _>>()
        .expect("typed receipts");
    // m1: update receipt + verification; m3: failed update; then the summary.
    assert_eq!(receipts.len(), 4);
    assert_eq!(receipts[0]["host_class"], "m1");
    assert_eq!(receipts[1]["event"], "host_verification");
    assert_eq!(receipts[1]["verdict"], "verified");
    assert_eq!(receipts[3]["event"], "fleet_summary");
    assert_eq!(receipts[3]["verified_hosts"], serde_json::json!(["m1"]));
    assert_eq!(receipts[3]["failed_host"]["host_class"], "m3");
    assert_eq!(
        receipts[3]["not_attempted_hosts"],
        serde_json::json!(["m5"])
    );
    assert!(
        error
            .failure
            .message
            .contains("hosts not verified at v0.137.0: m3, m5")
    );
    assert_eq!(receipts[0]["target"], "v0.137.0");
    assert_eq!(receipts[0]["executable_sha256"], "a".repeat(64));
    assert_eq!(
        receipts[0]["binary_pair_before"]["primary"]["source_identity"],
        Value::Null
    );
    assert_eq!(
        receipts[0]["binary_pair_before"]["primary"]["source_identity_basis"],
        "unverified_preinstall"
    );
    assert!(receipts[0]["binary_pair_after"]["companion"].is_object());
    assert_eq!(receipts[0]["daemon_pid"], 42);
    assert_eq!(receipts[0]["configured_repos_preserved"], true);
    assert_eq!(receipts[2]["host_class"], "m3");
    assert_eq!(receipts[2]["ok"], false);
    assert!(!rendered.contains("\"host_class\": \"m5\""));
}

#[test]
fn authority_receipt_mismatch_stops_before_the_next_host() {
    let plans = ["m1", "m3"]
        .iter()
        .map(|name| host_update_plan(&named_host(name), "v0.137.0").expect("plan"))
        .collect::<Vec<_>>();
    let mut attempted = Vec::new();
    let mut output = Vec::new();
    let error = apply_plans_for_test(&plans, "v0.137.0", true, &mut output, |plan| {
        attempted.push(plan.class.clone());
        let mut observed = evidence("0.137.0");
        if plan.class == "m3" {
            observed.release_authority_identity = "f".repeat(64);
        }
        Ok(observed)
    })
    .expect_err("drift must stop rollout");
    assert_eq!(attempted, ["m1", "m3"]);
    assert!(
        error
            .failure
            .message
            .contains("stopped after m3 evidence failed")
    );
    assert!(error.failure.message.contains("frozen release authority"));
}

#[test]
fn cross_host_binary_pair_hash_drift_stops_rollout() {
    let plans = ["m1", "m3", "m5"]
        .iter()
        .map(|name| host_update_plan(&named_host(name), "v0.137.0").expect("plan"))
        .collect::<Vec<_>>();
    let mut attempted = Vec::new();
    let mut output = Vec::new();
    let error = apply_plans_for_test(&plans, "v0.137.0", true, &mut output, |plan| {
        attempted.push(plan.class.clone());
        let mut observed = evidence("0.137.0");
        if plan.class == "m3" {
            observed.after_pair.primary.sha256 = "d".repeat(64);
            observed.executable_sha256 = "d".repeat(64);
            observed.daemon_runtime.loaded_executable_sha256 = "d".repeat(64);
            observed
                .auth_support_after
                .generation
                .as_mut()
                .expect("generation")
                .binary
                .sha256 = "d".repeat(64);
        }
        Ok(observed)
    })
    .expect_err("cross-host drift must stop rollout");
    assert_eq!(attempted, ["m1", "m3"]);
    assert!(error.failure.message.contains("hashes disagreed"));
}

#[test]
fn paired_host_receipt_exposes_reconcilable_before_and_after_identities() {
    let plan = host_update_plan(&named_host("m1"), "v0.137.0").expect("plan");
    let evidence = evidence("0.137.0");
    let mut output = Vec::new();
    render_host_result(
        &mut output,
        true,
        "v0.137.0",
        &plan,
        true,
        Some(&evidence),
        None,
    )
    .expect("receipt");
    let receipt: Value = serde_json::from_slice(&output).expect("json");
    for phase in ["binary_pair_before", "binary_pair_after"] {
        assert_eq!(receipt[phase]["primary"]["semantic_version"], "0.137.0");
        assert_eq!(receipt[phase]["companion"]["semantic_version"], "0.137.0");
        assert_eq!(
            receipt[phase]["primary"]["source_identity"],
            receipt[phase]["companion"]["source_identity"]
        );
    }
    assert_eq!(
        receipt["binary_pair_before"]["primary"]["source_identity_basis"],
        "unverified_preinstall"
    );
    assert_eq!(
        receipt["binary_pair_after"]["primary"]["source_identity_basis"],
        "verified_release_authority"
    );
    assert_eq!(receipt["release_authority"]["commit_oid"], "2".repeat(40));
    assert_eq!(receipt["release_authority"]["tree_oid"], "3".repeat(40));
    assert_eq!(
        receipt["release_authority"]["checksum_manifest"]["sha256"],
        "4".repeat(64)
    );
    assert_eq!(
        receipt["release_authority"]["platform_asset"]["attestation_statement_sha256"],
        "7".repeat(64)
    );
}

// ---------------------------------------------------------------------------
// Post-rollout verification
// ---------------------------------------------------------------------------

fn verify_transcript(
    cli: &str,
    daemon_version: &str,
    pid: &str,
    alive: &str,
    guards: &str,
) -> String {
    format!(
        "SHIPYARD_VERIFY_CLI={cli}\n\
         SHIPYARD_VERIFY_DAEMON={{\"command\":\"daemon:status\",\"running\":true,\"shipyard_version\":\"{daemon_version}\"}}\n\
         SHIPYARD_VERIFY_DAEMON_PID={pid}\n\
         SHIPYARD_VERIFY_DAEMON_PID_ALIVE={alive}\n\
         SHIPYARD_VERIFY_GUARDS_BEFORE={GUARDS_BEFORE_STALE}\n\
         SHIPYARD_VERIFY_GUARDS_INSTALL={INSTALL_OK}\n\
         SHIPYARD_VERIFY_GUARDS_INSTALL_EXIT=0\n\
         SHIPYARD_VERIFY_GUARDS={guards}\n"
    )
}

const GUARDS_BEFORE_STALE: &str = r#"{"guards":[{"name":"queue-arm-guard","status":"stale","installed_sha256":"abc123"},{"name":"queue-removal-guard","status":"current"}]}"#;
const INSTALL_OK: &str = r#"{"guards":[{"name":"queue-removal-guard","action":"unchanged"},{"name":"queue-arm-guard","action":"replaced"}]}"#;
const INSTALL_REFUSED: &str = r#"{"guards":[{"name":"queue-removal-guard","action":"unchanged"},{"name":"queue-arm-guard","action":"refused","detail":"a symlink"}]}"#;
const GUARDS_CURRENT: &str = r#"{"guards":[{"name":"queue-removal-guard","status":"current"},{"name":"queue-arm-guard","status":"current"}]}"#;
const GUARDS_STALE: &str = r#"{"guards":[{"name":"queue-removal-guard","status":"stale"},{"name":"queue-arm-guard","status":"missing"}]}"#;

#[test]
fn verification_accepts_only_the_target_binary_daemon_and_current_guards() {
    let plan = host_update_plan(&named_host("m1"), "v0.137.0").expect("plan");
    let good = verify::judge(
        &plan,
        42,
        &verify_transcript("shipyard 0.137.0", "0.137.0", "42", "1", GUARDS_CURRENT),
    );
    assert!(good.verified(), "{good:?}");
    assert_eq!(good.guards, "current");
    assert!(good.guards_installed);
    assert_eq!(
        good.guards_replaced,
        ["queue-arm-guard (replaced sha256 abc123)"]
    );

    let cases = [
        (
            verify_transcript("shipyard 0.136.0", "0.137.0", "42", "1", GUARDS_CURRENT),
            "installed CLI reports",
        ),
        (
            verify_transcript("shipyard 0.137.0", "0.136.0", "42", "1", GUARDS_CURRENT),
            "daemon answers as version",
        ),
        (
            verify_transcript("shipyard 0.137.0", "0.137.0", "41", "1", GUARDS_CURRENT),
            "not the refreshed pid 42",
        ),
        (
            verify_transcript("shipyard 0.137.0", "0.137.0", "42", "0", GUARDS_CURRENT),
            "is not alive",
        ),
        (
            verify_transcript("shipyard 0.137.0", "0.137.0", "42", "1", GUARDS_STALE),
            "queue-removal-guard=stale, queue-arm-guard=missing",
        ),
        (
            "SHIPYARD_VERIFY_CLI=shipyard 0.137.0\n".to_owned(),
            "daemon status could not be read",
        ),
        (
            verify_transcript("shipyard 0.137.0", "0.137.0", "42", "1", GUARDS_CURRENT)
                .replace(INSTALL_OK, INSTALL_REFUSED)
                .replace("INSTALL_EXIT=0", "INSTALL_EXIT=1"),
            "ghapp guards install: exited 1: queue-arm-guard refused (a symlink)",
        ),
        (
            verify_transcript("shipyard 0.137.0", "0.137.0", "42", "1", GUARDS_CURRENT)
                .replace(INSTALL_OK, INSTALL_REFUSED),
            "ghapp guards install: queue-arm-guard refused",
        ),
        (
            verify_transcript("shipyard 0.137.0", "0.137.0", "42", "1", GUARDS_CURRENT)
                .replace("INSTALL_EXIT=0", "INSTALL_EXIT=2"),
            "ghapp guards install: exited 2",
        ),
        (
            verify_transcript("shipyard 0.137.0", "0.137.0", "42", "1", GUARDS_CURRENT)
                .replace(INSTALL_OK, "not json"),
            "ghapp guards install: its receipt was not readable",
        ),
        (
            verify_transcript("shipyard 0.137.0", "0.137.0", "42", "1", GUARDS_CURRENT)
                .replace("\"running\":true", "\"running\":false"),
            "daemon is not running",
        ),
    ];
    for (transcript, expected) in cases {
        let verdict = verify::judge(&plan, 42, &transcript);
        assert!(!verdict.verified(), "{transcript}");
        assert!(
            verdict
                .failures
                .iter()
                .any(|failure| failure.contains(expected)),
            "{expected}: {:?}",
            verdict.failures
        );
    }
}

#[test]
fn a_release_without_guards_is_verified_without_them() {
    let plan = host_update_plan(&named_host("m1"), "v0.137.0").expect("plan");
    let verdict = verify::judge(
        &plan,
        42,
        &verify_transcript("shipyard 0.137.0", "0.137.0", "42", "1", "unsupported").replace(
            "SHIPYARD_VERIFY_GUARDS_INSTALL={\"command\":\"guards install\"}\n",
            "",
        ),
    );
    assert!(verdict.verified(), "{verdict:?}");
    assert_eq!(verdict.guards, "unsupported");
    assert!(!verdict.guards_installed);
}

fn failing_m3_verification(plan: &HostUpdatePlan, pid: u32) -> verify::HostVerification {
    if plan.class == "m3" {
        verify::judge(
            plan,
            pid,
            &verify_transcript(
                "shipyard 0.136.0",
                "0.137.0",
                &pid.to_string(),
                "1",
                "unsupported",
            ),
        )
    } else {
        verified_ok(plan, pid)
    }
}

fn rollout_receipts(output: Vec<u8>) -> Vec<Value> {
    let rendered = String::from_utf8(output).expect("UTF-8");
    serde_json::Deserializer::from_str(&rendered)
        .into_iter::<Value>()
        .collect::<Result<Vec<_>, _>>()
        .expect("typed receipts")
}

#[test]
fn failed_verification_rolls_the_host_back_and_stops_the_rollout() {
    let plans = ["m1", "m3", "m5"]
        .iter()
        .map(|name| host_update_plan(&named_host(name), "v0.137.0").expect("plan"))
        .collect::<Vec<_>>();
    let mut attempted = Vec::new();
    let mut output = Vec::new();
    let mut ops = TestOps {
        execute: |plan: &HostUpdatePlan| {
            attempted.push(plan.class.clone());
            Ok(evidence("0.137.0"))
        },
        verify: failing_m3_verification,
        rollback: rolled_back_ok,
        installed: Some("0.136.0".to_owned()),
        rollbacks: Vec::new(),
    };
    let error = apply_plans(&plans, &[], "v0.137.0", true, &mut output, &mut ops)
        .expect_err("a host that does not verify must fail the rollout");
    let rollbacks = ops.rollbacks.clone();
    drop(ops);
    assert_eq!(attempted, ["m1", "m3"]);
    assert_eq!(
        rollbacks,
        [("m3".to_owned(), Some("0.136.0".to_owned()))],
        "only the failing host is rolled back, to its pre-update version"
    );
    let message = &error.failure.message;
    assert!(error.rollback_failed_host.is_none());
    assert!(
        message.contains("m3 failed post-rollout verification"),
        "{message}"
    );
    assert!(
        message.contains("rolled back to v0.136.0 (verified)"),
        "{message}"
    );
    assert!(
        message.contains("hosts not verified at v0.137.0: m3, m5"),
        "{message}"
    );
    let receipts = rollout_receipts(output);
    let rollback = receipts
        .iter()
        .find(|receipt| receipt["event"] == "host_rollback")
        .expect("rollback receipt");
    assert_eq!(rollback["host_class"], "m3");
    assert_eq!(rollback["outcome"]["rollback"], "rolled_back");
    assert_eq!(rollback["outcome"]["to"], "v0.136.0");
    let summary = receipts.last().expect("summary");
    assert_eq!(summary["event"], "fleet_summary");
    assert_eq!(summary["verdict"], "failed");
    assert_eq!(summary["verified_hosts"], serde_json::json!(["m1"]));
    assert_eq!(summary["not_attempted_hosts"], serde_json::json!(["m5"]));
}

fn failed_rollback(_: &HostUpdatePlan, _: Option<&str>) -> rollback::RollbackOutcome {
    rollback::RollbackOutcome::Failed {
        to: Some("v0.136.0".to_owned()),
        reason: "rollback did not verify: daemon is not running".to_owned(),
    }
}

#[test]
fn a_failed_rollback_is_reported_loudly_and_still_stops() {
    let plans = ["m3", "m5"]
        .iter()
        .map(|name| host_update_plan(&named_host(name), "v0.137.0").expect("plan"))
        .collect::<Vec<_>>();
    let mut attempted = Vec::new();
    let mut output = Vec::new();
    let mut ops = TestOps {
        execute: |plan: &HostUpdatePlan| {
            attempted.push(plan.class.clone());
            Ok(evidence("0.137.0"))
        },
        verify: failing_m3_verification,
        rollback: failed_rollback,
        installed: Some("0.136.0".to_owned()),
        rollbacks: Vec::new(),
    };
    let error = apply_plans(&plans, &[], "v0.137.0", true, &mut output, &mut ops)
        .expect_err("failed rollback");
    drop(ops);
    assert_eq!(attempted, ["m3"], "no later host after a failed rollback");
    assert_eq!(error.rollback_failed_host.as_deref(), Some("m3"));
    assert_eq!(error.failure.code, EXIT_ROLLBACK_FAILED);
    assert!(
        error.failure.message.contains("ROLLBACK TO v0.136.0 FAILED: rollback did not verify: daemon is not running; the host needs an operator"),
        "{}",
        error.failure.message
    );
    let receipts = rollout_receipts(output);
    assert!(
        receipts
            .iter()
            .any(|receipt| receipt["event"] == "host_rollback"
                && receipt["outcome"]["rollback"] == "failed")
    );
    let summary = receipts.last().expect("summary");
    assert_eq!(summary["verdict"], "rollback_failed");
    assert_eq!(summary["failed_host"]["needs_operator"], true);
}

#[test]
fn a_committed_transaction_is_rolled_back_after_evidence_or_timeout_failures() {
    let plans = vec![host_update_plan(&named_host("m3"), "v0.137.0").expect("plan")];
    // Evidence the validator rejects: the transaction committed, then lied.
    let mut ops = TestOps {
        execute: |_: &HostUpdatePlan| {
            let mut observed = evidence("0.137.0");
            observed.release_authority_identity = "f".repeat(64);
            Ok(observed)
        },
        verify: verified_ok,
        rollback: rolled_back_ok,
        installed: Some("0.136.0".to_owned()),
        rollbacks: Vec::new(),
    };
    let error = apply_plans(&plans, &[], "v0.137.0", true, &mut Vec::new(), &mut ops)
        .expect_err("evidence failure");
    assert!(error.failure.message.contains("evidence failed"));
    assert!(error.failure.message.contains("rolled back to v0.136.0"));
    assert_eq!(
        ops.rollbacks,
        [("m3".to_owned(), Some("0.136.0".to_owned()))]
    );

    // A timeout: no evidence, but the pre-update probe names the version.
    let mut ops = TestOps {
        execute: |_: &HostUpdatePlan| Err(PlanExecutionError::TimedOut("deadline".to_owned())),
        verify: verified_ok,
        rollback: rolled_back_ok,
        installed: Some("0.135.2".to_owned()),
        rollbacks: Vec::new(),
    };
    let error =
        apply_plans(&plans, &[], "v0.137.0", true, &mut Vec::new(), &mut ops).expect_err("timeout");
    assert!(error.failure.message.contains("m3 timed out"));
    assert_eq!(
        ops.rollbacks,
        [("m3".to_owned(), Some("0.135.2".to_owned()))]
    );

    // A refusal before mutation is not rolled back.
    let mut ops = TestOps {
        execute: |_: &HostUpdatePlan| {
            Err(PlanExecutionError::Failed(
                "ssh: connection refused".to_owned(),
            ))
        },
        verify: verified_ok,
        rollback: rolled_back_ok,
        installed: Some("0.136.0".to_owned()),
        rollbacks: Vec::new(),
    };
    apply_plans(&plans, &[], "v0.137.0", true, &mut Vec::new(), &mut ops).expect_err("failed");
    assert!(ops.rollbacks.is_empty());
}

#[test]
fn a_pair_hash_disagreement_rolls_the_disagreeing_host_back() {
    let plans = ["m1", "m3"]
        .iter()
        .map(|name| host_update_plan(&named_host(name), "v0.137.0").expect("plan"))
        .collect::<Vec<_>>();
    let mut ops = TestOps {
        execute: |plan: &HostUpdatePlan| {
            let mut observed = evidence("0.137.0");
            if plan.class == "m3" {
                observed.after_pair.primary.sha256 = "d".repeat(64);
                observed.executable_sha256 = "d".repeat(64);
                observed.daemon_runtime.loaded_executable_sha256 = "d".repeat(64);
                observed
                    .auth_support_after
                    .generation
                    .as_mut()
                    .expect("generation")
                    .binary
                    .sha256 = "d".repeat(64);
            }
            Ok(observed)
        },
        verify: verified_ok,
        rollback: rolled_back_ok,
        installed: Some("0.136.0".to_owned()),
        rollbacks: Vec::new(),
    };
    let error = apply_plans(&plans, &[], "v0.137.0", true, &mut Vec::new(), &mut ops)
        .expect_err("pair drift");
    assert!(error.failure.message.contains("hashes disagreed"));
    assert_eq!(ops.rollbacks.len(), 1);
    assert_eq!(ops.rollbacks[0].0, "m3");
}

#[test]
fn lagging_only_skips_current_and_ahead_hosts_and_fails_closed_on_unknown() {
    let plans = ["m1", "m3", "m5"]
        .iter()
        .map(|name| host_update_plan(&named_host(name), "v0.137.0").expect("plan"))
        .collect::<Vec<_>>();
    let versions = [
        ("m1", Some("0.137.0")),
        ("m3", Some("0.136.1")),
        ("m5", Some("0.138.0")),
    ];
    let probe = |plan: &HostUpdatePlan| {
        let version = versions
            .iter()
            .find(|(name, _)| *name == plan.class)
            .and_then(|(_, version)| *version);
        reconcile::HostVersion {
            host_class: plan.class.clone(),
            ssh: plan.ssh.clone(),
            version: version.map(ToOwned::to_owned),
            error: None,
            lagging: None,
            declares_host_classes: None,
        }
    };
    let (lagging, current) =
        partition_lagging(plans.clone(), "v0.137.0", probe).expect("partition");
    assert_eq!(
        lagging
            .iter()
            .map(|plan| plan.class.as_str())
            .collect::<Vec<_>>(),
        ["m3"]
    );
    assert_eq!(
        current,
        ["m1", "m5"],
        "current and ahead hosts are never reinstalled"
    );

    let unreadable = |plan: &HostUpdatePlan| reconcile::HostVersion {
        host_class: plan.class.clone(),
        ssh: plan.ssh.clone(),
        version: None,
        error: Some("ssh: timed out".to_owned()),
        lagging: None,
        declares_host_classes: None,
    };
    let error = partition_lagging(plans, "v0.137.0", unreadable).expect_err("unknown");
    assert!(
        error
            .message
            .contains("cannot read the installed version on m1")
    );

    let mut output = Vec::new();
    apply_plans(
        &[],
        &["m1".to_owned(), "m5".to_owned()],
        "v0.137.0",
        true,
        &mut output,
        &mut TestOps {
            execute: |_: &HostUpdatePlan| -> Result<HostUpdateEvidence, PlanExecutionError> {
                panic!("nothing lags")
            },
            verify: verified_ok,
            rollback: rolled_back_ok,
            installed: None,
            rollbacks: Vec::new(),
        },
    )
    .expect("nothing to do is success");
    let summary = rollout_receipts(output).pop().expect("summary");
    assert_eq!(summary["verdict"], "verified");
    assert_eq!(
        summary["already_current_hosts"],
        serde_json::json!(["m1", "m5"])
    );
}

#[test]
fn fleet_update_apply_refuses_while_another_rollout_holds_the_lock() {
    let temp = tempfile::tempdir().expect("temp");
    let paths = RuntimePaths::current_with_overrides(
        RuntimeMode::Shipyard,
        Some(temp.path().join("global")),
        Some(temp.path().join("state")),
    );
    let held = controller_lock::try_acquire(&paths.state_dir)
        .expect("lock")
        .expect("free");
    let args = FleetUpdateArgs {
        to: "v0.209.0".to_owned(),
        host_classes: Vec::new(),
        all_hosts: true,
        apply: true,
        lagging_only: true,
    };
    let mut output = Vec::new();
    let error = fleet_update_command(
        &args,
        RuntimeMode::Shipyard,
        temp.path(),
        &paths,
        true,
        &mut output,
    )
    .expect_err("held lock");
    assert_eq!(error.code, EXIT_CONTROLLER_BUSY);
    assert!(
        error
            .message
            .contains("another fleet rollout holds the controller lock")
    );
    assert!(output.is_empty(), "nothing was planned or touched");
    drop(held);
}

#[test]
fn rollback_target_is_the_pre_update_version_when_a_governed_one_exists() {
    let plan = host_update_plan(&named_host("m1"), "v0.140.0").expect("plan");
    assert_eq!(
        rollback::rollback_target(&plan, Some("0.138.2")).as_deref(),
        Ok("v0.138.2")
    );
    assert_eq!(
        rollback::rollback_target(&plan, Some("v0.138.2")).as_deref(),
        Ok("v0.138.2")
    );
    assert!(
        rollback::rollback_target(&plan, Some("0.140.0"))
            .expect_err("same")
            .contains("no earlier version")
    );
    assert!(
        rollback::rollback_target(&plan, Some("0.120.0"))
            .expect_err("old")
            .contains("predates")
    );
    assert!(rollback::rollback_target(&plan, None).is_err());
}

#[test]
fn the_controller_host_is_always_updated_last() {
    let mut local = named_host("studio");
    local.ssh = None;
    local.shipyard_bin = Some("/Users/ci/.local/bin/shipyard".to_owned());
    let mut plans = vec![
        host_update_plan(&local, "v0.137.0").expect("local plan"),
        host_update_plan(&named_host("m1"), "v0.137.0").expect("plan"),
        host_update_plan(&named_host("m5"), "v0.137.0").expect("plan"),
        host_update_plan(&named_host("m7-new-rack"), "v0.137.0").expect("plan"),
    ];
    order_controller_last(&mut plans);
    let order = plans
        .iter()
        .map(|plan| plan.class.as_str())
        .collect::<Vec<_>>();
    assert_eq!(order, ["m1", "m5", "m7-new-rack", "studio"]);
}

#[test]
fn a_fully_verified_rollout_ends_with_a_verified_summary() {
    let plans = ["m1", "m3"]
        .iter()
        .map(|name| host_update_plan(&named_host(name), "v0.137.0").expect("plan"))
        .collect::<Vec<_>>();
    let mut output = Vec::new();
    apply_plans_for_test(&plans, "v0.137.0", true, &mut output, |_| {
        Ok(evidence("0.137.0"))
    })
    .expect("verified rollout");
    let rendered = String::from_utf8(output).expect("UTF-8");
    let summary = serde_json::Deserializer::from_str(&rendered)
        .into_iter::<Value>()
        .last()
        .expect("summary")
        .expect("json");
    assert_eq!(summary["verdict"], "verified");
    assert_eq!(summary["verified_hosts"], serde_json::json!(["m1", "m3"]));
}

/// Run the real verification script locally against a fake installed binary,
/// so the shell the host executes (not just the judge) is covered.
#[cfg(unix)]
#[test]
fn verification_script_installs_guards_and_reads_every_fact_on_the_host() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().expect("temp");
    let bin = temp.path().join("shipyard");
    let log = temp.path().join("calls");
    let state_dir = temp.path().join("state");
    std::fs::create_dir_all(state_dir.join("daemon")).expect("daemon dir");
    let live_pid = std::process::id();
    std::fs::write(state_dir.join("daemon/daemon.pid"), format!("{live_pid}\n")).expect("pid");
    std::fs::write(
        &bin,
        format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> '{log}'\n\
             case \"$*\" in\n\
             \x20 --version) echo 'shipyard 0.137.0' ;;\n\
             \x20 *'daemon status'*) printf '%s' '{{\"command\":\"daemon:status\",\"running\":true,\"shipyard_version\":\"0.137.0\"}}' ;;\n\
             \x20 'guards --help') exit 0 ;;\n\
             \x20 '--json guards install') printf '%s' '{INSTALL_OK}' ;;\n\
             \x20 '--json guards status') if [ -f '{marker}' ]; then printf '%s' '{GUARDS_CURRENT}'; else : > '{marker}'; printf '%s' '{GUARDS_BEFORE_STALE}'; fi ;;\n\
             \x20 *) exit 9 ;;\n\
             esac\n",
            log = log.display(),
            marker = temp.path().join("status-read").display()
        ),
    )
    .expect("fake binary");
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let mut plan = host_update_plan(&named_host("studio"), "v0.137.0").expect("plan");
    plan.ssh = None;
    plan.binary = bin;
    plan.state_dir = state_dir;

    let verdict = verify::verify_host(&plan, live_pid);

    assert!(verdict.verified(), "{verdict:?}");
    assert_eq!(verdict.guards, "current");
    assert!(verdict.guards_installed);
    assert_eq!(
        verdict.guards_replaced,
        ["queue-arm-guard (replaced sha256 abc123)"]
    );
    let calls = std::fs::read_to_string(log).expect("calls");
    assert!(calls.contains("--json guards install"), "{calls}");
    assert!(calls.contains("--json guards status"), "{calls}");
    assert!(calls.contains("--json daemon status"), "{calls}");
}

#[test]
fn doctor_flags_a_host_only_once_it_lags_past_the_soak() {
    let now = chrono::Utc::now();
    let latest = reconcile::PublishedRelease {
        tag: "v0.208.0".to_owned(),
        published_at: now - chrono::Duration::minutes(45),
    };
    let row = |version: Option<&str>, lagging: Option<bool>| reconcile::HostVersion {
        host_class: "m5".to_owned(),
        ssh: Some("blackbook".to_owned()),
        version: version.map(ToOwned::to_owned),
        error: version.is_none().then(|| "ssh: timed out".to_owned()),
        lagging,
        declares_host_classes: None,
    };
    let lagging =
        reconcile_cmd::fleet_version_row(&row(Some("0.205.0"), Some(true)), Some(&latest), now, 30);
    assert!(!lagging.ok);
    assert!(
        lagging
            .detail
            .as_deref()
            .unwrap_or_default()
            .contains("lags v0.208.0 past the 30-minute soak")
    );
    let soaking =
        reconcile_cmd::fleet_version_row(&row(Some("0.205.0"), Some(true)), Some(&latest), now, 60);
    assert!(soaking.ok, "{soaking:?}");
    let current = reconcile_cmd::fleet_version_row(
        &row(Some("0.208.0"), Some(false)),
        Some(&latest),
        now,
        30,
    );
    assert!(current.ok);
    let unknown = reconcile_cmd::fleet_version_row(&row(None, None), Some(&latest), now, 30);
    assert!(!unknown.ok);
    assert!(
        unknown
            .error
            .as_deref()
            .unwrap_or_default()
            .starts_with("UNKNOWN: ssh: timed out")
    );
    let no_release = reconcile_cmd::fleet_version_row(&row(Some("0.205.0"), None), None, now, 30);
    assert!(!no_release.ok);
}
