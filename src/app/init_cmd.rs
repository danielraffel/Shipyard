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
