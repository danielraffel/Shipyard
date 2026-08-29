use std::collections::VecDeque;

use super::*;
fn config(path: PathBuf, digest: Sha256Digest) -> ParallelProofCanaryAdapterConfig {
    ParallelProofCanaryAdapterConfig {
        executable_path: path,
        executable_sha256: digest,
        deadline_seconds: 3,
        max_stdout_bytes: 1024,
        max_stderr_bytes: 1024,
        invocation_authority_sha256: Sha256Digest::of_bytes(b"invocation"),
        repository_id: 42,
        repository: "example/project".to_owned(),
        target: "mac".to_owned(),
        target_triple: "aarch64-apple-darwin".to_owned(),
        builder_host_id: "builder".to_owned(),
        worker_host_id: "worker".to_owned(),
    }
}

fn authority() -> CanaryAdapterAuthority {
    CanaryAdapterAuthority {
        repository_id: 42,
        repository: "example/project".to_owned(),
        target: "mac".to_owned(),
        target_triple: "aarch64-apple-darwin".to_owned(),
        builder_host_id: "builder".to_owned(),
        worker_host_id: "worker".to_owned(),
        correlation_id: "canary-1".to_owned(),
        manifest_digest: Sha256Digest::of_bytes(b"manifest"),
        invocation_authority_sha256: Sha256Digest::of_bytes(b"invocation"),
    }
}

#[derive(Default)]
struct EchoRunner {
    payloads: VecDeque<serde_json::Value>,
    requests: Vec<serde_json::Value>,
    corrupt_authority: bool,
}

impl CanaryProtocolRunner for EchoRunner {
    fn invoke(&mut self, request: &[u8]) -> Result<Vec<u8>, ParallelProofError> {
        let request: serde_json::Value = serde_json::from_slice(request).unwrap();
        let authority = if self.corrupt_authority {
            serde_json::Value::String(Sha256Digest::of_bytes(b"wrong").as_str().to_owned())
        } else {
            request["authority_sha256"].clone()
        };
        let response = serde_json::json!({
            "schema_version": PROTOCOL_SCHEMA,
            "operation": request["operation"].clone(),
            "idempotency_key": request["idempotency_key"].clone(),
            "authority_sha256": authority,
            "payload_sha256": request["payload_sha256"].clone(),
            "result": self.payloads.pop_front().unwrap(),
            "model_calls": 0,
        });
        self.requests.push(request);
        Ok(serde_json::to_vec(&response).unwrap())
    }
}

struct UnknownFieldRunner;

impl CanaryProtocolRunner for UnknownFieldRunner {
    fn invoke(&mut self, request: &[u8]) -> Result<Vec<u8>, ParallelProofError> {
        let request: serde_json::Value = serde_json::from_slice(request).unwrap();
        Ok(serde_json::to_vec(&serde_json::json!({
            "schema_version": PROTOCOL_SCHEMA,
            "operation": request["operation"],
            "idempotency_key": request["idempotency_key"],
            "authority_sha256": request["authority_sha256"],
            "payload_sha256": request["payload_sha256"],
            "result": {"kind":"host_observations","observations":[]},
            "model_calls": 0,
            "unexpected": true
        }))
        .unwrap())
    }
}

fn executor(runner: EchoRunner) -> ProductionParallelProofCanaryExecutor<EchoRunner> {
    let authority = authority();
    let authority_sha256 =
        protocol_digest("shipyard.canary-adapter.authority.v1", &authority).unwrap();
    ProductionParallelProofCanaryExecutor {
        runner,
        authority,
        authority_sha256,
        observation_count: 0,
        manifest: None,
        inventory: None,
        plan: None,
        pre_execution_hosts: Vec::new(),
    }
}

#[test]
fn host_observation_phases_are_exact_and_idempotent() {
    let payload = serde_json::json!({"kind":"host_observations","observations":[]});
    let mut first = executor(EchoRunner {
        payloads: VecDeque::from([payload.clone(), payload.clone(), payload.clone()]),
        ..EchoRunner::default()
    });
    assert!(first.authenticated_host_observations().unwrap().is_empty());
    assert!(first.authenticated_host_observations().unwrap().is_empty());
    assert!(first.authenticated_host_observations().unwrap().is_empty());
    assert!(matches!(
        first.authenticated_host_observations(),
        Err(ParallelProofError::InvalidAttemptSequence(_))
    ));
    let operations: Vec<_> = first
        .runner
        .requests
        .iter()
        .map(|request| request["operation"].as_str().unwrap())
        .collect();
    assert_eq!(
        operations,
        [
            "observe_initial_hosts",
            "observe_pre_execution_hosts",
            "observe_final_hosts"
        ]
    );
    let keys: Vec<_> = first
        .runner
        .requests
        .iter()
        .map(|request| request["idempotency_key"].as_str().unwrap())
        .collect();
    assert_ne!(keys[0], keys[1]);
    assert_ne!(keys[1], keys[2]);

    let mut replay = executor(EchoRunner {
        payloads: VecDeque::from([payload]),
        ..EchoRunner::default()
    });
    replay.authenticated_host_observations().unwrap();
    assert_eq!(
        first.runner.requests[0]["idempotency_key"],
        replay.runner.requests[0]["idempotency_key"]
    );
    assert_eq!(
        first.runner.requests[0]["authority"],
        replay.runner.requests[0]["authority"]
    );
    assert_eq!(first.runner.requests[0]["model_calls"], 0);
}

#[test]
fn response_authority_and_strict_shape_fail_closed() {
    let payload = serde_json::json!({"kind":"host_observations","observations":[]});
    let mut wrong = executor(EchoRunner {
        payloads: VecDeque::from([payload]),
        corrupt_authority: true,
        ..EchoRunner::default()
    });
    assert!(matches!(
        wrong.authenticated_host_observations(),
        Err(ParallelProofError::BindingMismatch(
            "canary adapter response authority"
        ))
    ));

    let authority = authority();
    let authority_sha256 =
        protocol_digest("shipyard.canary-adapter.authority.v1", &authority).unwrap();
    let mut strict = ProductionParallelProofCanaryExecutor {
        runner: UnknownFieldRunner,
        authority,
        authority_sha256,
        observation_count: 0,
        manifest: None,
        inventory: None,
        plan: None,
        pre_execution_hosts: Vec::new(),
    };
    assert!(matches!(
        strict.authenticated_host_observations(),
        Err(ParallelProofError::CorruptRecord(_))
    ));
}

#[test]
fn trusted_config_is_absent_by_default_and_partial_activation_refuses() {
    let global = tempfile::tempdir().unwrap();
    let loaded = LoadedConfig::load_machine_global_from_dir(global.path().to_path_buf()).unwrap();
    assert!(
        trusted_parallel_proof_canary_config(&loaded)
            .unwrap()
            .is_none()
    );

    fs::write(
        global.path().join("config.toml"),
        "[parallel_proof_canary]\nactivation_enabled = false\nrepository_id = 42\n",
    )
    .unwrap();
    let loaded = LoadedConfig::load_machine_global_from_dir(global.path().to_path_buf()).unwrap();
    assert!(trusted_parallel_proof_canary_config(&loaded).is_err());

    fs::write(
        global.path().join("config.toml"),
        format!(
            "[parallel_proof_canary]\n\
             activation_enabled = true\n\
             apply_enabled = true\n\
             executable_path = \"/usr/bin/true\"\n\
             executable_sha256 = \"{}\"\n\
             deadline_seconds = 30\n\
             max_stdout_bytes = 4096\n\
             max_stderr_bytes = 4096\n\
             invocation_authority_sha256 = \"{}\"\n\
             repository_id = 42\n\
             repository = \"example/project\"\n\
             target = \"mac\"\n\
             target_triple = \"aarch64-apple-darwin\"\n\
             builder_host_id = \"builder\"\n\
             worker_host_id = \"worker\"\n",
            "a".repeat(64),
            "b".repeat(64)
        ),
    )
    .unwrap();
    let loaded = LoadedConfig::load_machine_global_from_dir(global.path().to_path_buf()).unwrap();
    let activation = trusted_parallel_proof_canary_config(&loaded)
        .unwrap()
        .expect("enabled config");
    assert!(activation.apply_enabled);
    assert_eq!(activation.adapter.repository_id, 42);
    assert_eq!(activation.adapter.builder_host_id, "builder");
}

#[cfg(unix)]
fn executable(root: &Path, name: &str, body: &str) -> (PathBuf, Sha256Digest) {
    use std::os::unix::fs::PermissionsExt as _;
    let source = root.join(format!("{name}.c"));
    let path = root.join(name);
    fs::write(&source, body).unwrap();
    let status = Command::new("cc")
        .args([
            source.as_os_str(),
            std::ffi::OsStr::new("-o"),
            path.as_os_str(),
        ])
        .status()
        .unwrap();
    assert!(status.success());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    let bytes = fs::read(&path).unwrap();
    (path, Sha256Digest::of_bytes(&bytes))
}

#[cfg(unix)]
#[test]
fn pinned_runner_rejects_symlink_digest_timeout_and_output_limit() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let (path, digest) = executable(root.path(), "adapter", "int main(void) { return 0; }");
    let link = root.path().join("adapter-link");
    symlink(&path, &link).unwrap();
    let mut linked = DigestPinnedCanaryProtocolRunner::new(config(link, digest.clone()));
    assert!(linked.invoke(b"{}").is_err());

    let mut mismatched = DigestPinnedCanaryProtocolRunner::new(config(
        path.clone(),
        Sha256Digest::of_bytes(b"wrong"),
    ));
    assert!(matches!(
        mismatched.invoke(b"{}"),
        Err(ParallelProofError::BindingMismatch(
            "canary adapter executable digest"
        ))
    ));

    let (loud_path, loud_digest) = executable(
        root.path(),
        "loud",
        "#include <stdio.h>\nint main(void) { fputs(\"12345\", stdout); return 0; }",
    );
    let mut loud_config = config(loud_path, loud_digest);
    // This assertion is about the output bound, not scheduler latency. Keep
    // enough wall-clock slack for a heavily parallel full-suite run.
    loud_config.deadline_seconds = 30;
    loud_config.max_stdout_bytes = 4;
    let mut loud = DigestPinnedCanaryProtocolRunner::new(loud_config);
    let loud_result = loud.invoke(b"{}");
    assert!(
        matches!(
            &loud_result,
            Err(ParallelProofError::LimitExceeded {
                field: "canary adapter output bytes",
                ..
            })
        ),
        "{loud_result:?}"
    );

    let (slow_path, slow_digest) = executable(
        root.path(),
        "slow",
        "#include <unistd.h>\nint main(void) { sleep(5); return 0; }",
    );
    let mut slow = DigestPinnedCanaryProtocolRunner::new(config(slow_path, slow_digest));
    assert!(matches!(
        slow.invoke(b"{}"),
        Err(ParallelProofError::CorruptRecord(_))
    ));
}
