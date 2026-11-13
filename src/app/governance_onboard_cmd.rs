use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use super::CliFailure;
use crate::config::LoadedConfig;
use crate::governance::GovernanceGh;
use crate::governance_onboard::{OnboardRequest, build_plan};
use crate::identity::RuntimeMode;
use crate::output::write_json_envelope;

/// Run `governance onboard`: print the read-only plan. `--apply` is refused
/// before any read; the write path is not implemented.
#[allow(clippy::too_many_arguments)]
pub(super) fn governance_onboard<W: Write>(
    request: &OnboardRequest,
    gh_binary: Option<&PathBuf>,
    apply: bool,
    mode: RuntimeMode,
    cwd: &Path,
    json_mode: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    if apply {
        return Err(CliFailure::new(
            1,
            "governance onboard --apply is not implemented; run without --apply for the \
             read-only plan. Nothing was read or written.",
        ));
    }
    let gh = match (gh_binary, LoadedConfig::load_from_cwd(mode, cwd).ok()) {
        (Some(binary), _) => GovernanceGh::ambient(cwd, Some(binary)),
        (None, Some(config)) => GovernanceGh::from_loaded_config(cwd, &config, None)
            .map_err(|error| CliFailure::new(9, error))?,
        (None, None) => GovernanceGh::ambient(cwd, None),
    };
    let plan = build_plan(&gh, request)
        .map_err(|error| CliFailure::new(error.exit_code(), error.message().to_owned()))?;
    if json_mode {
        let data = plan
            .as_object()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect();
        write_json_envelope(stdout, "governance.onboard", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        serde_json::to_writer_pretty(&mut *stdout, &plan)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        writeln!(stdout).map_err(|error| CliFailure::new(1, error.to_string()))?;
        writeln!(
            stdout,
            "plan-only: no GitHub write was attempted (--apply is not implemented)"
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(ExitCode::SUCCESS)
}
