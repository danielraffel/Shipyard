//! `shipyard landing` — report how work actually merges in a repository.
//!
//! Placed beside [`super::landability_cmd`] rather than inside `status`
//! because the two answer adjacent questions about the same repository and
//! neither is Shipyard's own state: `landability` asks whether a pull
//! request's required contexts can be *scheduled*, and this asks what happens
//! to a pull request whose contexts are green. `shipyard status` reports
//! Shipyard's queue, targets and evidence; folding a GitHub-policy reading
//! into it would blur the one boundary that keeps that feed trustworthy,
//! which is that everything in it is a fact about Shipyard.
//!
//! The command is read-only. It exits non-zero when the merge mechanism could
//! not be determined, because a caller that scripts around this needs to tell
//! "no queue" from "could not see whether there is a queue", and an exit code
//! is the only part of the output that every caller reads.

use std::io::Write;
use std::path::Path;

use serde_json::Value;

use super::CliFailure;
use crate::config::LoadedConfig;
use crate::gate_cost::ReadCache;
use crate::identity::RuntimeMode;
use crate::landability::gate;
use crate::landing::gather::{DEFAULT_MAX_JOB_READS, DEFAULT_RUN_SAMPLE, GatherOptions};
use crate::landing::{EXIT_LANDING_UNKNOWN, gather, pr_state, render};

struct RepositoryTrainConfigError {
    surface: crate::landing::SurfaceRead,
    message: String,
    api_calls: u32,
}

fn repository_train_query(repo: &str, base: &str) -> String {
    format!(
        "repos/{repo}/contents/.shipyard/config.toml?ref={}",
        crate::required_check_policy::encode_path_segment(base)
    )
}

/// Determine whether the exact config path is absent from a complete tree
/// response. A successful repository metadata read is not enough: an App can
/// read metadata while lacking Contents permission, and a contents 404 would
/// then be indistinguishable from an absent file.
fn config_path_in_tree(raw: &str) -> Result<bool, String> {
    let payload: Value = serde_json::from_str(raw)
        .map_err(|error| format!("the base tree response is not valid JSON: {error}"))?;
    match payload.get("truncated").and_then(Value::as_bool) {
        Some(false) => {}
        Some(true) => {
            return Err(
                "the base tree response was truncated; config absence cannot be proven".to_owned(),
            );
        }
        None => {
            return Err(
                "the base tree response did not state whether it was truncated; config absence cannot be proven".to_owned(),
            );
        }
    }
    let tree = payload
        .get("tree")
        .and_then(Value::as_array)
        .ok_or_else(|| "the base tree response did not contain a complete tree".to_owned())?;
    Ok(tree
        .iter()
        .any(|entry| entry.get("path").and_then(Value::as_str) == Some(".shipyard/config.toml")))
}

fn verify_config_absent(
    actions: &crate::cloud::GitHubActions,
    repo: &str,
    base: &str,
    contents_error: &str,
    surface_query: &str,
) -> Result<(crate::rollup::Config, crate::landing::SurfaceRead, u32), RepositoryTrainConfigError> {
    let tree_query = format!(
        "repos/{repo}/git/trees/{}?recursive=1",
        crate::required_check_policy::encode_path_segment(base)
    );
    let verified_query = format!("{surface_query} (404); GET /{tree_query}");
    let probe = actions.run_gh(&["api".to_owned(), tree_query.clone()]);
    match probe {
        Ok(raw_tree) => match config_path_in_tree(&raw_tree) {
            Ok(true) => Err(RepositoryTrainConfigError {
                surface: crate::landing::SurfaceRead {
                    surface: "repository_train_config".to_owned(),
                    query: verified_query,
                    outcome: crate::landing::SurfaceOutcome::Unreadable {
                        boundary: crate::fleet_service::Boundary::Permission,
                        detail: format!(
                            ".shipyard/config.toml on {repo}@{base} returned 404, but the base tree contains that path"
                        ),
                    },
                },
                message: format!(
                    ".shipyard/config.toml on {repo}@{base} could not be read: the contents request returned {contents_error}, but the base tree contains that path"
                ),
                api_calls: 2,
            }),
            Ok(false) => Ok((
                crate::rollup::Config::default(),
                crate::landing::SurfaceRead {
                    surface: "repository_train_config".to_owned(),
                    query: verified_query,
                    outcome: crate::landing::SurfaceOutcome::Absent {
                        detail: "the complete base tree was readable and did not contain .shipyard/config.toml; train defaults apply".to_owned(),
                    },
                },
                2,
            )),
            Err(tree_error) => Err(RepositoryTrainConfigError {
                surface: crate::landing::SurfaceRead {
                    surface: "repository_train_config".to_owned(),
                    query: verified_query,
                    outcome: crate::landing::SurfaceOutcome::Unreadable {
                        boundary: crate::fleet_service::Boundary::Parse,
                        detail: format!(
                            ".shipyard/config.toml on {repo}@{base} could not be proven absent: {tree_error}"
                        ),
                    },
                },
                message: format!(
                    ".shipyard/config.toml on {repo}@{base} could not be proven absent: the contents request returned {contents_error}, and the base tree response was unusable: {tree_error}"
                ),
                api_calls: 2,
            }),
        },
        Err(probe_error) => {
            let detail = format!(
                ".shipyard/config.toml on {repo}@{base} could not be proven absent: the contents request returned {contents_error}, and exact-base tree verification failed: {probe_error}"
            );
            Err(RepositoryTrainConfigError {
                surface: crate::landing::SurfaceRead {
                    surface: "repository_train_config".to_owned(),
                    query: verified_query,
                    outcome: crate::landing::SurfaceOutcome::Unreadable {
                        boundary: crate::landing::gather::classify(
                            &probe_error.to_string(),
                        ),
                        detail: detail.clone(),
                    },
                },
                message: detail,
                api_calls: 2,
            })
        }
    }
}

fn repository_train_config(
    actions: &crate::cloud::GitHubActions,
    repo: &str,
    base: &str,
) -> Result<(crate::rollup::Config, crate::landing::SurfaceRead, u32), RepositoryTrainConfigError> {
    let query = repository_train_query(repo, base);
    let surface_query = format!("GET /{query}");
    let raw = match actions.run_gh(&[
        "api".to_owned(),
        "-H".to_owned(),
        "Accept: application/vnd.github.raw+json".to_owned(),
        query.clone(),
    ]) {
        Ok(raw) => raw,
        Err(error)
            if error.to_string().contains("404") || error.to_string().contains("Not Found") =>
        {
            return verify_config_absent(actions, repo, base, &error.to_string(), &surface_query);
        }
        Err(error) => {
            let detail = format!(".shipyard/config.toml on {repo}@{base} is unreadable: {error}");
            return Err(RepositoryTrainConfigError {
                surface: crate::landing::SurfaceRead {
                    surface: "repository_train_config".to_owned(),
                    query: surface_query,
                    outcome: crate::landing::SurfaceOutcome::Unreadable {
                        boundary: crate::fleet_service::Boundary::Transport,
                        detail: detail.clone(),
                    },
                },
                message: detail,
                api_calls: 1,
            });
        }
    };
    let table = raw.parse::<toml::Table>().map_err(|error| {
        let detail = format!(".shipyard/config.toml on {repo}@{base} does not parse: {error}");
        RepositoryTrainConfigError {
            surface: crate::landing::SurfaceRead {
                surface: "repository_train_config".to_owned(),
                query: surface_query.clone(),
                outcome: crate::landing::SurfaceOutcome::Unreadable {
                    boundary: crate::fleet_service::Boundary::Parse,
                    detail: detail.clone(),
                },
            },
            message: detail,
            api_calls: 1,
        }
    })?;
    let config = crate::rollup::Config::from_table(&table).map_err(|error| {
        let detail = format!("[train] in {repo}@{base} is invalid: {error}");
        RepositoryTrainConfigError {
            surface: crate::landing::SurfaceRead {
                surface: "repository_train_config".to_owned(),
                query: surface_query.clone(),
                outcome: crate::landing::SurfaceOutcome::Unreadable {
                    boundary: crate::fleet_service::Boundary::Parse,
                    detail: detail.clone(),
                },
            },
            message: detail,
            api_calls: 1,
        }
    })?;
    Ok((
        config,
        crate::landing::SurfaceRead {
            surface: "repository_train_config".to_owned(),
            query: surface_query,
            outcome: crate::landing::SurfaceOutcome::Read,
        },
        1,
    ))
}

/// What the caller asked for, as parsed from the command line.
pub(super) struct LandingArgs {
    /// Exact `OWNER/REPO`, when given.
    pub repo: Option<String>,
    /// Base branch, when given.
    pub base: Option<String>,
    /// How many recent completed runs to consider for placement.
    pub run_sample: Option<usize>,
    /// Upper bound on per-run job reads.
    pub max_job_reads: Option<usize>,
    /// Classify one pull request's queue state instead of the repo model.
    pub pr: Option<u64>,
    /// Emit the machine-readable form.
    pub json: bool,
    /// Shipyard's state directory, for the read cache that serves completed
    /// job-log signatures to the `--pr` verdict. `None` disables caching.
    pub state_dir: Option<std::path::PathBuf>,
}

/// Run `shipyard landing`.
#[allow(clippy::too_many_lines)] // Keep the read-only landing command's evidence order auditable.
pub(super) fn landing_command<W: Write>(
    mode: RuntimeMode,
    cwd: &Path,
    args: LandingArgs,
    stdout: &mut W,
) -> Result<std::process::ExitCode, CliFailure> {
    let LandingArgs {
        repo: repo_arg,
        base: base_arg,
        run_sample,
        max_job_reads,
        pr,
        json,
        state_dir,
    } = args;
    let config = LoadedConfig::load_from_cwd(mode, cwd)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let repo = repo_arg
        .filter(|value| !value.trim().is_empty())
        .or_else(|| gate::resolve_repo(&config, cwd))
        .ok_or_else(|| {
            CliFailure::new(
                1,
                "No repo detected. Pass --repo OWNER/REPO or run inside a git clone with a \
                 GitHub origin.",
            )
        })?;
    let base = base_arg
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| gate::resolve_base(&config));

    let actions = crate::cloud::GitHubActions::from_loaded_config(cwd, &config)
        // Route the App token to the repository asked about, not the checkout's.
        .with_repo_override(&repo);
    if let Some(pr) = pr {
        let environment_opt_in = config
            .get(crate::environment_requeue::CONFIG_KEY)
            .and_then(toml::Value::as_bool)
            == Some(true);
        let cache = state_dir.map_or_else(ReadCache::disabled, |dir| {
            // The PR watch cache: its signature entries are keyed by job id
            // and never change once a job completed, so both commands share.
            ReadCache::open(
                &dir.join("pr-watch")
                    .join("cache")
                    .join(repo.replace('/', "__").to_ascii_lowercase()),
                std::time::SystemTime::now(),
            )
        });
        let report = pr_state::gather_cached(&actions, &repo, pr, environment_opt_in, &cache);
        if json {
            pr_state::write_json(stdout, &report)
        } else {
            pr_state::write_human(stdout, &report)
        }
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
        if report.is_unknown() {
            return Err(CliFailure::new(
                EXIT_LANDING_UNKNOWN,
                format!(
                    "PR #{pr}'s merge-queue state or landing verdict could not be determined; \
                     do not act on it as if it were unarmed, green, or flaky"
                ),
            ));
        }
        return Ok(std::process::ExitCode::SUCCESS);
    }
    let mut report = gather::gather(
        &actions,
        &GatherOptions {
            repo: &repo,
            base: &base,
            run_sample: run_sample.unwrap_or(DEFAULT_RUN_SAMPLE).clamp(1, 100),
            max_job_reads: max_job_reads.unwrap_or(DEFAULT_MAX_JOB_READS).clamp(0, 50),
            base_health_workflow: config
                .get_str(crate::base_health::WORKFLOW_CONFIG_KEY)
                .unwrap_or(crate::base_health::DEFAULT_WORKFLOW),
        },
    );

    match repository_train_config(&actions, &repo, &base) {
        Ok((train, surface, api_calls)) => {
            report.api_calls += api_calls;
            report.surfaces.push(surface);
            report.batching =
                crate::landing::strategy::batching_guidance(&report.merge_queue, &train);
        }
        Err(error) => {
            report.api_calls += error.api_calls;
            report.surfaces.push(error.surface);
            report.batching = crate::landing::strategy::BatchingGuidance {
                action: crate::landing::strategy::BatchingAction::Unknown,
                command: None,
                max_members: None,
                rationale: error.message,
            };
        }
    }

    report.opt_in_targets = crate::opt_in_targets::from_config(&config);

    if json {
        render::write_json(stdout, &report)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        render::write_human(stdout, &report)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }

    if report.has_unknown() {
        return Err(CliFailure::new(
            EXIT_LANDING_UNKNOWN,
            "the landing mechanism could not be fully determined; treat the UNKNOWN fields above \
             as unmeasured rather than as absent",
        ));
    }
    Ok(std::process::ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::{config_path_in_tree, repository_train_query};

    #[test]
    fn repository_train_query_encodes_the_base_ref() {
        assert_eq!(
            repository_train_query("owner/repo", "release/1.0?x=y&z"),
            "repos/owner/repo/contents/.shipyard/config.toml?ref=release%2F1.0%3Fx%3Dy%26z"
        );
    }

    #[test]
    fn complete_tree_proves_config_absent_only_when_path_is_missing() {
        assert!(
            !config_path_in_tree(
                r#"{"truncated":false,"tree":[{"path":"README.md","type":"blob"}]}"#
            )
            .expect("tree")
        );
        assert!(
            config_path_in_tree(
                r#"{"truncated":false,"tree":[{"path":".shipyard/config.toml","type":"blob"}]}"#
            )
            .expect("tree")
        );
    }

    #[test]
    fn incomplete_tree_cannot_prove_config_absent() {
        let error = config_path_in_tree(r#"{"truncated":true,"tree":[]}"#)
            .expect_err("truncated tree must stay unknown");
        assert!(error.contains("truncated"), "{error}");
        assert!(config_path_in_tree(r#"{"truncated":false}"#).is_err());
    }
}
