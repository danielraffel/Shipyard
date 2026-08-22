use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::super::CliFailure;
use super::super::changed_surface_cmd::{ChangedSurfacePlanArgs, observe_changed_surface_plan};
use crate::changed_surface::{
    ExecutionCommandTransport, ExecutionDisposition, plan_authoritative_execution,
};
use crate::config::LoadedConfig;
use crate::executor::dispatch::{ResolvedTarget, ResolvedValidation};
use crate::queue_request::validation_contract_digest;

const MODE_KEY: &str = "changed_surface_execution.mode";

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum MachineMode {
    #[default]
    Off,
    ShadowCompare,
    Authoritative,
}

impl MachineMode {
    fn from_global(config: &LoadedConfig) -> Result<Self, CliFailure> {
        let trusted = LoadedConfig::load_machine_global_from_dir(config.global_dir.clone())
            .map_err(|error| {
                CliFailure::new(1, format!("load trusted selector policy: {error}"))
            })?;
        match trusted.get_str(MODE_KEY) {
            None | Some("off") => Ok(Self::Off),
            Some("shadow_compare") => Ok(Self::ShadowCompare),
            Some("authoritative") => Ok(Self::Authoritative),
            Some(value) => Err(CliFailure::new(
                2,
                format!("invalid trusted {MODE_KEY} value '{value}'"),
            )),
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::ShadowCompare => "shadow_compare",
            Self::Authoritative => "authoritative",
        }
    }
}

#[derive(Debug, Serialize)]
struct ActivationReceipt<'a> {
    schema_version: u32,
    machine_mode: MachineMode,
    plan: &'a crate::changed_surface::AuthoritativeExecutionPlan,
    original_test_command_sha256: String,
    substituted_test_command_sha256: String,
}

pub(super) fn apply_changed_surface_execution(
    config: &LoadedConfig,
    cwd: &Path,
    state_dir: &Path,
    repo: &str,
    pr: Option<u64>,
    targets: &mut [ResolvedTarget],
) -> Result<(), CliFailure> {
    let mode = MachineMode::from_global(config)?;
    if mode == MachineMode::Off {
        return Ok(());
    }
    let Some(pr) = pr.filter(|number| *number != 0) else {
        return Ok(());
    };

    for target in targets {
        // This merged-layer check is only a negative performance prefilter.
        // Authorization is always reparsed from the authenticated base below.
        if config
            .get("targets")
            .and_then(toml::Value::as_table)
            .and_then(|targets| targets.get(&target.name))
            .and_then(toml::Value::as_table)
            .and_then(|target| target.get("changed_surface_selection"))
            .is_none()
        {
            continue;
        }
        let ResolvedValidation::Local(validation) = &target.validation else {
            continue;
        };
        if validation.command.is_some() || !validation.stages.contains_key("test") {
            continue;
        }
        let Some(contract_digest) = validation_contract_digest(target) else {
            continue;
        };
        let observation = observe_changed_surface_plan(
            &ChangedSurfacePlanArgs {
                target: target.name.clone(),
                pr,
                repo: (!repo.is_empty()).then(|| repo.to_owned()),
            },
            config,
            cwd,
            state_dir,
        )?;
        let Ok(policy) = observation.policy.as_ref() else {
            continue;
        };
        let Ok(disposition) = plan_authoritative_execution(
            &observation.receipt,
            &observation.input,
            policy,
            true,
            ExecutionCommandTransport::PosixShell,
            &contract_digest,
            &observation.workflow_digest,
        ) else {
            continue;
        };
        let ExecutionDisposition::Bounded(plan) = disposition else {
            continue;
        };
        let original = validation
            .stages
            .get("test")
            .expect("checked local test stage")
            .clone();
        let result_dir = result_dir(state_dir, repo, pr, &plan.head_sha, &target.name);
        let compare = if mode == MachineMode::ShadowCompare {
            "1"
        } else {
            "0"
        };
        let substituted = format!(
            "SHIPYARD_CHANGED_SURFACE_RESULT_DIR={} SHIPYARD_CHANGED_SURFACE_COMPARE_FULL={} {}",
            shell_quote(&result_dir),
            compare,
            plan.command
        );
        persist_activation(
            &result_dir,
            &ActivationReceipt {
                schema_version: 1,
                machine_mode: mode,
                plan: &plan,
                original_test_command_sha256: sha256(original.as_bytes()),
                substituted_test_command_sha256: sha256(substituted.as_bytes()),
            },
        )?;
        let ResolvedValidation::Local(validation) = &mut target.validation else {
            unreachable!();
        };
        validation.stages.insert("test".to_owned(), substituted);
    }
    Ok(())
}

fn result_dir(state_dir: &Path, repo: &str, pr: u64, head: &str, target: &str) -> PathBuf {
    state_dir
        .join("changed-surface-results")
        .join(safe_component(repo))
        .join(pr.to_string())
        .join(safe_component(head))
        .join(safe_component(target))
}

fn safe_component(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-' => byte as char,
            _ => '_',
        })
        .collect()
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"))
}

fn persist_activation(path: &Path, receipt: &ActivationReceipt<'_>) -> Result<(), CliFailure> {
    fs::create_dir_all(path).map_err(|error| {
        CliFailure::new(1, format!("create selector evidence directory: {error}"))
    })?;
    let payload = serde_json::to_vec_pretty(receipt)
        .map_err(|error| CliFailure::new(1, format!("serialize selector activation: {error}")))?;
    let destination = path.join(format!("activation-{}.json", receipt.machine_mode.as_str()));
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&destination)
    {
        Ok(mut file) => {
            file.write_all(&payload)
                .and_then(|()| file.write_all(b"\n"))
                .and_then(|()| file.sync_all())
                .map_err(|error| {
                    CliFailure::new(1, format!("write selector activation: {error}"))
                })?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing = fs::read(&destination).map_err(|error| {
                CliFailure::new(1, format!("read existing selector activation: {error}"))
            })?;
            let mut expected = payload;
            expected.push(b'\n');
            if existing != expected {
                return Err(CliFailure::new(
                    1,
                    "immutable selector activation receipt already exists with different bytes",
                ));
            }
        }
        Err(error) => {
            return Err(CliFailure::new(
                1,
                format!("create selector activation receipt: {error}"),
            ));
        }
    }
    Ok(())
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::{MachineMode, safe_component, shell_quote};
    use crate::config::{LoadedConfig, LocalOverlaySource};
    use std::fs;

    #[test]
    fn machine_mode_is_default_off_and_ignores_repo_layers() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path()).unwrap();
        let mut config = LoadedConfig::load(
            Some(temp.path().to_path_buf()),
            None,
            None,
            LocalOverlaySource::None,
        )
        .unwrap();
        config.data.insert(
            "changed_surface_execution".to_owned(),
            toml::toml! { mode = "authoritative" }.into(),
        );
        assert_eq!(MachineMode::from_global(&config).unwrap(), MachineMode::Off);
    }

    #[test]
    fn path_inputs_are_bounded_before_shell_use() {
        assert_eq!(safe_component("Generous-Corp/pulp"), "Generous-Corp_pulp");
        assert_eq!(shell_quote(std::path::Path::new("a'b")), "'a'\"'\"'b'");
    }

    #[test]
    fn invalid_global_mode_fails_closed_instead_of_enabling() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("config.toml"),
            "[changed_surface_execution]\nmode = 'fast'\n",
        )
        .unwrap();
        let config = LoadedConfig::load(
            Some(temp.path().to_path_buf()),
            None,
            None,
            LocalOverlaySource::None,
        )
        .unwrap();
        let error = MachineMode::from_global(&config).unwrap_err();
        assert!(error.message.contains("invalid trusted"));
    }
}
