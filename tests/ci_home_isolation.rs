//! Production-shaped binary proof for CI-only machine-home isolation.

#![cfg(feature = "ci-test-home")]

use std::fs;
use std::process::Command;

use serde_json::Value;
use shipyard::identity::RuntimeMode;
use shipyard::paths::RuntimePaths;
use shipyard::platform::Platform;

#[test]
fn production_shaped_binary_uses_ci_test_home_without_moving_runner_home() {
    let temp = tempfile::tempdir().expect("tempdir");
    let runner_home = temp.path().join("runner-home");
    let test_home = temp.path().join("test-home");
    let cwd = temp.path().join("cwd");
    fs::create_dir_all(&cwd).expect("cwd");

    let runner_paths =
        RuntimePaths::for_platform(Platform::current(), &runner_home, RuntimeMode::Shipyard);
    let test_paths =
        RuntimePaths::for_platform(Platform::current(), &test_home, RuntimeMode::Shipyard);
    fs::create_dir_all(&runner_paths.global_dir).expect("runner global dir");
    fs::create_dir_all(&test_paths.global_dir).expect("test global dir");
    fs::write(
        runner_paths.global_dir.join("config.toml"),
        "this is not valid toml = [",
    )
    .expect("hostile runner config");
    fs::write(
        test_paths.global_dir.join("config.toml"),
        "[ci_home_probe]\nsource = \"isolated\"\n",
    )
    .expect("isolated test config");

    let binary = env!("CARGO_BIN_EXE_shipyard");
    let inherited = Command::new(binary)
        .args(["--json", "--cwd"])
        .arg(&cwd)
        .args(["config", "show"])
        .env("HOME", &runner_home)
        .env("USERPROFILE", &runner_home)
        .env_remove("SHIPYARD_TEST_HOME")
        .output()
        .expect("run control binary");
    assert!(
        !inherited.status.success(),
        "control must observe invalid runner config: stdout={} stderr={}",
        String::from_utf8_lossy(&inherited.stdout),
        String::from_utf8_lossy(&inherited.stderr)
    );

    let isolated = Command::new(binary)
        .args(["--json", "--cwd"])
        .arg(&cwd)
        .args(["config", "show"])
        .env("HOME", &runner_home)
        .env("USERPROFILE", &runner_home)
        .env("SHIPYARD_TEST_HOME", &test_home)
        .output()
        .expect("run isolated binary");
    assert!(
        isolated.status.success(),
        "isolated binary failed: stdout={} stderr={}",
        String::from_utf8_lossy(&isolated.stdout),
        String::from_utf8_lossy(&isolated.stderr)
    );
    let output: Value = serde_json::from_slice(&isolated.stdout).expect("config JSON");
    assert_eq!(output["config"]["ci_home_probe"]["source"], "isolated");
}
