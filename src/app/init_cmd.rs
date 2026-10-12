use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::process::{Command, ExitCode};

use serde_json::Value;

use super::CliFailure;
use crate::identity::RuntimeMode;
use crate::init_config::run_init;
use crate::output::{write_json_envelope, write_pretty_json};

pub(super) fn init_command<W: Write>(
    discover_only: bool,
    mode: RuntimeMode,
    cwd: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let result = run_init(cwd, mode).map_err(|error| CliFailure::new(1, error.to_string()))?;
    install_repo_hooks(cwd)?;
    if json {
        let mut data = BTreeMap::new();
        let config_value = serde_json::to_value(&result.config)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        let Value::Object(config) = config_value else {
            return Err(CliFailure::new(
                1,
                "generated init config was not an object",
            ));
        };
        data.extend(config);
        write_json_envelope(stdout, "init", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else if discover_only {
        writeln!(stdout, "Detected config (not written):")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        write_pretty_json(stdout, &result.config)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        writeln!(stdout, "Shipyard configured. Try: shipyard run")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(ExitCode::SUCCESS)
}

fn install_repo_hooks(cwd: &Path) -> Result<(), CliFailure> {
    let script = cwd.join("scripts/install-githooks.sh");
    if !script.is_file() || !cwd.join(".githooks").is_dir() {
        return Ok(());
    }
    let status = Command::new("bash")
        .arg(&script)
        .current_dir(cwd)
        .status()
        .map_err(|error| CliFailure::new(1, format!("installing git hooks: {error}")))?;
    if !status.success() {
        return Err(CliFailure::new(
            1,
            format!("git hook installation failed with {status}"),
        ));
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::install_repo_hooks;
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    fn git(repo: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(repo)
            .args(args)
            .output()
            .expect("git");
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn repo_with_hooks(installer: &str) -> tempfile::TempDir {
        let temp = tempfile::tempdir().expect("temp");
        git(temp.path(), &["init", "-q"]);
        fs::create_dir_all(temp.path().join(".githooks")).expect("hooks");
        fs::write(temp.path().join(".githooks/pre-push"), "#!/bin/sh\n").expect("hook");
        fs::create_dir_all(temp.path().join("scripts")).expect("scripts");
        fs::write(temp.path().join("scripts/install-githooks.sh"), installer).expect("installer");
        temp
    }

    #[test]
    fn init_points_git_at_the_repository_hooks() {
        let installer = fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/install-githooks.sh"),
        )
        .expect("installer");
        let repo = repo_with_hooks(&installer);
        install_repo_hooks(repo.path()).expect("install");
        assert_eq!(
            git(repo.path(), &["config", "--get", "core.hooksPath"]),
            ".githooks"
        );
    }

    #[test]
    fn init_without_repository_hooks_installs_nothing() {
        let temp = tempfile::tempdir().expect("temp");
        git(temp.path(), &["init", "-q"]);
        install_repo_hooks(temp.path()).expect("nothing to install");
        assert_eq!(git(temp.path(), &["config", "--get", "core.hooksPath"]), "");
    }

    #[test]
    fn a_failing_hook_installer_fails_init() {
        let repo = repo_with_hooks("exit 3\n");
        let error = install_repo_hooks(repo.path()).expect_err("refused");
        assert!(
            error.message.contains("git hook installation failed"),
            "{}",
            error.message
        );
    }
}
