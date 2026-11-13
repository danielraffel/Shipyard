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
    // An explicit --gh-binary names the identity. Otherwise the identity comes
    // from the config, and a config that cannot be read stops the plan: falling
    // back to ambient gh would silently change who the governance reads run as.
    let gh = if let Some(binary) = gh_binary {
        GovernanceGh::ambient(cwd, Some(binary))
    } else {
        let config = LoadedConfig::load_from_cwd(mode, cwd).map_err(|error| {
            CliFailure::new(
                1,
                format!(
                    "governance onboard: cannot read the Shipyard config ({error}); fix it \
                     or pass --gh-binary. Nothing was read from GitHub."
                ),
            )
        })?;
        GovernanceGh::from_loaded_config(cwd, &config, None)
            .map_err(|error| CliFailure::new(9, error))?
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
