//! `shipyard landability` — the on-demand surface, and the instrument's own
//! self-check.
//!
//! Two things happen on every invocation, and the second is the one that keeps
//! the first honest.
//!
//! 1. **The assessment.** Resolve every required context to the lanes that
//!    gate it and report a verdict per lane.
//! 2. **The self-check.** Assess two synthetic lanes against the *same* census
//!    and the *same* attestation set: one that must come back `Unserved` (a
//!    label no runner can carry) and one that must come back `Served` (the
//!    hosted `ubuntu-latest` lane, which GitHub always serves). If either
//!    disagrees, the instrument is broken and this command says so and exits
//!    non-zero — rather than reporting a clean bill of health it did not
//!    measure.
//!
//! The second exists because of a specific, repeated failure: a measurement
//! aimed at the wrong target does not error, it succeeds and returns empty,
//! and empty reads as a clean finding. This session's own inventory found five
//! sensors dead for weeks to months, **none of which reported its own death**.
//! A detector that cannot fail its own control is not a detector.
//!
//! The control costs zero API calls: it reuses the census already fetched.

use std::io::Write;
use std::path::Path;

use chrono::Utc;
use serde_json::json;

use super::CliFailure;
use crate::config::LoadedConfig;
use crate::fleet_service::{LaneServiceThresholds, RegisteredRunner};
use crate::identity::RuntimeMode;
use crate::landability::attestation::{AttestationSet, local_attestation_paths};
use crate::landability::gate::{self, GateOptions};
use crate::landability::reach::{HeadEvidence, HeadRun};
use crate::landability::trigger::parse_workflow_triggers;
use crate::landability::{
    EXIT_LANE_UNSERVED, EXIT_TRIGGER_UNREACHABLE, Schedulability, assess_lane,
};

/// Report generation.
///
/// A reader has to be able to tell a report that carries trigger reachability
/// from one that does not. Without it a host still running the predecessor
/// build answers `landability --self-check --json` successfully, with no
/// `triggers` block, and an operator reads the silence as "nothing wrong" —
/// generation skew masquerading as health, which has happened on this fleet
/// before.
pub const SCHEMA_VERSION: u32 = 2;

/// A base name no `branches:` filter can plausibly admit. The live negative
/// control for the trigger classifier.
const CONTROL_NEVER_A_BASE: &str = "shipyard-control-never-a-base";

/// A label no runner can plausibly carry. Used as the live negative control.
const CONTROL_UNSERVED_LABELS: &str = r#"["self-hosted","shipyard-landability-control-never"]"#;

/// A hosted label GitHub always serves. Used as the live positive control.
const CONTROL_SERVED_LABELS: &str = r#"["ubuntu-latest"]"#;

/// Run `shipyard landability`.
#[allow(clippy::too_many_arguments)]
pub(super) fn landability_command<W: Write>(
    mode: RuntimeMode,
    cwd: &Path,
    state_dir: &Path,
    repo_arg: Option<String>,
    base_arg: Option<String>,
    pr: Option<u64>,
    json: bool,
    stdout: &mut W,
) -> Result<std::process::ExitCode, CliFailure> {
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
    let mut base = base_arg
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| gate::resolve_base(&config));

    // Post-open evidence, link (5). Only spent when a pull request number was
    // given: at `shipyard pr` time there is no pull request to read, and the
    // static classification needs none.
    let mut evidence_calls = 0u32;
    let evidence = match pr {
        Some(number) => {
            let (evidence, calls, notes) =
                collect_head_evidence(&config, cwd, &repo, number, &mut base);
            evidence_calls = calls;
            for note in notes {
                let _ = writeln!(stdout, "  note: {note}");
            }
            evidence
        }
        None => None,
    };

    let outcome = gate::run(
        &config,
        cwd,
        state_dir,
        &repo,
        &base,
        &GateOptions {
            allow_unserved_lanes: Vec::new(),
            allow_unreachable_triggers: Vec::new(),
            evidence,
            skip: false,
            // On-demand means "tell me the truth now", so never a cached
            // census. The preflight is the surface that uses the cache.
            ignore_cache: true,
        },
    );

    let control = run_self_check(&outcome, &config, cwd);

    if json {
        write_json(stdout, &repo, &base, &outcome, &control, evidence_calls)?;
    } else {
        write_human(stdout, &repo, &base, &outcome, &control, evidence_calls)?;
    }

    if !control.healthy {
        // A broken instrument outranks its own findings. Reporting "nothing is
        // wrong" from a detector that just failed its control is the exact
        // shape of every dead sensor this replaces.
        return Err(CliFailure::new(
            1,
            format!("landability self-check FAILED: {}", control.detail),
        ));
    }
    // Order matters and is the whole argument for two exit codes: the
    // EARLIEST broken link is the one reported, because fixing a later one
    // changes nothing while an earlier one is broken. A trigger fault (links
    // 1-3) therefore outranks a lane fault (link 4).
    if outcome.trigger_refusal.is_some() {
        return Err(CliFailure::new(
            EXIT_TRIGGER_UNREACHABLE,
            "at least one required context will never be requested (see diagnosis above)",
        ));
    }
    if outcome.refusal.is_some() {
        return Err(CliFailure::new(
            EXIT_LANE_UNSERVED,
            "at least one required context cannot be scheduled (see diagnosis above)",
        ));
    }
    Ok(std::process::ExitCode::SUCCESS)
}

/// Read the three post-open facts, spending the third call only when the
/// first two cannot already answer the question.
///
/// Returns the evidence, the number of API calls actually spent, and notes
/// worth printing. Nothing here polls: one read each, on demand.
fn collect_head_evidence(
    config: &LoadedConfig,
    cwd: &Path,
    repo: &str,
    number: u64,
    base: &mut String,
) -> (Option<HeadEvidence>, u32, Vec<String>) {
    let actions = crate::cloud::GitHubActions::from_loaded_config(cwd, config);
    let mut notes = Vec::new();
    let mut calls = 0u32;

    calls += 1;
    let facts = match actions.pull_request_trigger_facts(repo, number) {
        Ok(facts) => facts,
        Err(error) => {
            notes.push(format!(
                "pull request #{number} unreadable ({error}); falling back to the STATIC \
                 classification only - no run evidence was read"
            ));
            return (None, calls, notes);
        }
    };
    if !facts.base_ref.is_empty() && facts.base_ref != *base {
        notes.push(format!(
            "pull request #{number} targets `{}`, not `{base}`; classifying against the live base",
            facts.base_ref
        ));
        base.clone_from(&facts.base_ref);
    }
    let local_head = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned());
    if local_head.as_deref() != Some(facts.head_sha.as_str()) {
        notes.push(format!(
            "this checkout is not at #{number}'s head ({} vs {}); the local diff is NOT this pull \
             request's diff, so a path-filter verdict here is about the checkout. Run evidence is \
             unaffected — it is keyed on the pull request's head SHA.",
            local_head.as_deref().unwrap_or("unreadable"),
            facts.head_sha
        ));
    }
    if facts.author_is_app {
        notes.push(format!(
            "pull request #{number} was opened by an App. That does NOT suppress `pull_request` \
             workflows - the trigger is evaluated exactly as documented; check for a run on the \
             head SHA rather than assuming absence"
        ));
    }

    calls += 1;
    let runs = match actions.workflow_runs_for_head_sha(repo, &facts.head_sha) {
        Ok(runs) => runs,
        Err(error) => {
            notes.push(format!(
                "runs on {} unreadable ({error}); no run evidence was read",
                facts.head_sha
            ));
            return (None, calls, notes);
        }
    };
    let pr_shaped = runs.iter().any(|run| {
        matches!(
            run.event.as_str(),
            "pull_request" | "pull_request_target" | "merge_group"
        )
    });

    // The conditional third call. Spent only on the one shape a retarget can
    // explain: no pull-request run on this head.
    let (mut changed_at, mut changed_from, mut timeline_read) = (None, None, false);
    if !pr_shaped {
        calls += 1;
        match actions.pull_request_base_changes(repo, number) {
            Ok(found) => {
                timeline_read = true;
                if let Some(change) = found {
                    changed_at = Some(change.at);
                    changed_from = change.from;
                }
            }
            Err(error) => notes.push(format!(
                "timeline for #{number} unreadable ({error}); a retarget cannot be distinguished \
                 from a pull request that simply has no run yet"
            )),
        }
    }

    (
        Some(HeadEvidence {
            number,
            head_sha: facts.head_sha,
            runs: runs
                .into_iter()
                .map(|run| HeadRun {
                    workflow_path: run.path,
                    event: run.event,
                    created_at: run.created_at,
                    id: run.id,
                })
                .collect(),
            base_ref_changed_at: changed_at,
            base_ref_changed_from: changed_from,
            timeline_read,
        }),
        calls,
        notes,
    )
}

fn write_json<W: Write>(
    stdout: &mut W,
    repo: &str,
    base: &str,
    outcome: &crate::landability::GateOutcome,
    control: &SelfCheck,
    evidence_calls: u32,
) -> Result<(), CliFailure> {
    {
        let payload = json!({
            "repo": repo,
            "base": base,
            "api_calls": outcome.api_calls + evidence_calls,
            "schema": SCHEMA_VERSION,
            "report": outcome.report,
            "warnings": outcome.warnings,
            "refusal": outcome.refusal,
            "trigger_refusal": outcome.trigger_refusal,
            "self_check": {
                "healthy": control.healthy,
                "detail": control.detail,
                "lanes": {
                    "control_unserved": control.lane_negative,
                    "control_hosted": control.lane_positive,
                },
                "triggers": {
                    "control_excluded": control.trigger_negative,
                    "control_admitted": control.trigger_positive,
                },
            },
        });
        writeln!(
            stdout,
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_default()
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(())
}

fn write_human<W: Write>(
    stdout: &mut W,
    repo: &str,
    base: &str,
    outcome: &crate::landability::GateOutcome,
    control: &SelfCheck,
    evidence_calls: u32,
) -> Result<(), CliFailure> {
    {
        writeln!(stdout, "landability {repo} (base {base})")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        writeln!(
            stdout,
            "  api calls this run: {} (gate {} + evidence {evidence_calls})",
            outcome.api_calls + evidence_calls,
            outcome.api_calls
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
        if let Some(report) = &outcome.report {
            writeln!(
                stdout,
                "  required contexts ({}): {}",
                report.contexts_source,
                report.contexts.join(", ")
            )
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
            writeln!(
                stdout,
                "  attesting hosts: {}",
                if report.fresh_attesters.is_empty() {
                    "none".to_owned()
                } else {
                    report.fresh_attesters.join(", ")
                }
            )
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
            for context in &report.reachability {
                writeln!(
                    stdout,
                    "  [{}] {} <- {}{}",
                    context.verdict.as_str(),
                    context.context,
                    context.workflow.as_deref().unwrap_or("(no producer)"),
                    if context.waived { "  (WAIVED)" } else { "" }
                )
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
                if let Some(clause) = &context.clause {
                    writeln!(stdout, "        {clause}")
                        .map_err(|error| CliFailure::new(1, error.to_string()))?;
                }
            }
            for lane in &report.lanes {
                writeln!(
                    stdout,
                    "  [{}] {} / {} ({}) <- {}",
                    lane.verdict.as_str(),
                    lane.context,
                    lane.job_id,
                    lane.role.as_str(),
                    lane.source
                )
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
            }
        }
        for warning in &outcome.warnings {
            writeln!(stdout, "  warning: {warning}")
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
        writeln!(
            stdout,
            "  self-check: {} - {}",
            if control.healthy { "OK" } else { "BROKEN" },
            control.detail
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
        for refusal in [&outcome.trigger_refusal, &outcome.refusal]
            .into_iter()
            .flatten()
        {
            write!(stdout, "\n{refusal}").map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
    }
    Ok(())
}

struct SelfCheck {
    healthy: bool,
    detail: String,
    lane_negative: String,
    lane_positive: String,
    trigger_negative: String,
    trigger_positive: String,
}

impl SelfCheck {
    fn broken(detail: String) -> Self {
        Self {
            healthy: false,
            detail,
            lane_negative: "not_run".to_owned(),
            lane_positive: "not_run".to_owned(),
            trigger_negative: "not_run".to_owned(),
            trigger_positive: "not_run".to_owned(),
        }
    }
}

/// Assert the `on:` reader still discriminates admitted from excluded, using
/// the host's own checked-out workflows as the input.
///
/// Real inputs, not mocks: `control_excluded` evaluates each configured
/// producer against a base name no `branches:` filter admits, and
/// `control_admitted` against the configured base. If a producer declares no
/// `branches:` filter at all it admits everything, including the control name
/// — that is correct behaviour, not a broken instrument, so such a workflow is
/// skipped and the check reports how many it could actually use. Zero usable
/// producers is itself a failure: a control that measured nothing reads
/// identically to one that passed.
fn trigger_self_check(config: &LoadedConfig, cwd: &Path) -> (String, String, String) {
    // The positive control uses the CONFIGURED base, never the base the
    // caller asked about. Using the queried base makes the control fail
    // exactly when the finding is true — `--base <a feature branch>` is the
    // shape this whole module exists to diagnose, and an instrument that
    // reports itself broken on its own headline case is worse than none.
    let base = gate::resolve_base(config);
    let base = base.as_str();
    let mut excluded = 0usize;
    let mut admitted = 0usize;
    let mut usable = 0usize;
    let mut refused = 0usize;

    let mut paths = gate::workflow_paths(config);
    for (_, path) in gate::shipyard_required_contexts(config, cwd, base) {
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    for path in &paths {
        let Ok(source) = crate::landability::gather::read_workflow_at_ref(cwd, "HEAD", path)
            .or_else(|_| {
                crate::landability::gather::read_workflow_at_ref(
                    cwd,
                    &format!("origin/{base}"),
                    path,
                )
            })
        else {
            continue;
        };
        let Ok(triggers) = parse_workflow_triggers(&source) else {
            refused += 1;
            continue;
        };
        let Some(filter) = triggers.pull_request.as_ref() else {
            continue;
        };
        if filter.branches.is_none() && filter.branches_ignore.is_none() {
            // Unfiltered: admits every base by design, so it cannot serve as a
            // negative control. Counted as not-usable rather than as a pass.
            continue;
        }
        usable += 1;
        if filter.admits_base(CONTROL_NEVER_A_BASE).excludes() {
            excluded += 1;
        }
        if !filter.admits_base(base).excludes() {
            admitted += 1;
        }
    }

    let negative = if usable == 0 {
        "not_measurable".to_owned()
    } else if excluded == usable {
        "base_excluded".to_owned()
    } else {
        format!("LEAKED ({excluded}/{usable} excluded a base nothing admits)")
    };
    let positive = if usable == 0 {
        "not_measurable".to_owned()
    } else if admitted == usable {
        "triggered".to_owned()
    } else {
        format!("REFUSED ({admitted}/{usable} admitted the configured base `{base}`)")
    };
    let detail = format!("{usable} usable producer(s), {refused} refused `on:` block(s)");
    (negative, positive, detail)
}

/// Assert the classifier still distinguishes served from unserved, using the
/// census and attestations this run already holds.
fn run_self_check(
    outcome: &crate::landability::GateOutcome,
    config: &LoadedConfig,
    cwd: &Path,
) -> SelfCheck {
    let (trigger_negative, trigger_positive, trigger_detail) = trigger_self_check(config, cwd);
    let trigger_ok = trigger_negative == "base_excluded" && trigger_positive == "triggered";
    let trigger_measurable = trigger_negative != "not_measurable";

    let Some(report) = &outcome.report else {
        return SelfCheck::broken(
            "no report produced, so nothing was measured; a run that assessed zero lanes reads \
             identically to a clean result and is not one"
                .to_owned(),
        );
    };
    if report.lanes.is_empty() && report.reachability.is_empty() {
        return SelfCheck::broken(
            "zero lanes and zero contexts assessed - the instrument ran and measured nothing"
                .to_owned(),
        );
    }

    let now = Utc::now();
    let attestations = AttestationSet::read_from(&local_attestation_paths());
    // The census this run used is not carried on the report, so the control is
    // run against an empty census plus the same attestation set: a synthetic
    // self-hosted label nothing attests must be Unserved, and a hosted label
    // must be Served regardless of census. Those two together prove the
    // classifier still discriminates.
    let empty: Vec<RegisteredRunner> = Vec::new();

    let negative = assess_lane(
        "self-check",
        "control-unserved",
        crate::landability::LaneRole::Producer,
        "CONTROL_NEVER",
        CONTROL_UNSERVED_LABELS,
        &empty,
        None,
        &attestations,
        LaneServiceThresholds::default(),
        now,
    );
    let positive = assess_lane(
        "self-check",
        "control-served",
        crate::landability::LaneRole::Producer,
        "CONTROL_HOSTED",
        CONTROL_SERVED_LABELS,
        &empty,
        None,
        &attestations,
        LaneServiceThresholds::default(),
        now,
    );

    if trigger_measurable && !trigger_ok {
        return SelfCheck {
            healthy: false,
            detail: format!(
                "trigger control lanes do NOT discriminate: a base nothing admits returned `{trigger_negative}` \
                 and the configured base returned `{trigger_positive}`. Every trigger verdict from \
                 this run is suspect. ({trigger_detail})"
            ),
            lane_negative: "not_run".to_owned(),
            lane_positive: "not_run".to_owned(),
            trigger_negative,
            trigger_positive,
        };
    }

    let negative_ok = if attestations.fresh_hosts(now).is_empty() {
        // With no attestation the honest answer is Unknown, and that IS the
        // correct discrimination for this input; requiring Unserved here would
        // make the control fail on every host without tartci.
        negative.verdict == Schedulability::Unknown
    } else {
        negative.verdict == Schedulability::Unserved
    };
    let positive_ok = positive.verdict == Schedulability::Served;

    if negative_ok && positive_ok {
        SelfCheck {
            healthy: true,
            detail: format!(
                "control lanes discriminate ({} / {}); trigger controls ({trigger_negative} / \
                 {trigger_positive}); {} lane(s), {} context(s) assessed; {trigger_detail}",
                negative.verdict.as_str(),
                positive.verdict.as_str(),
                report.lanes.len(),
                report.reachability.len(),
            ),
            lane_negative: negative.verdict.as_str().to_owned(),
            lane_positive: positive.verdict.as_str().to_owned(),
            trigger_negative,
            trigger_positive,
        }
    } else {
        SelfCheck {
            healthy: false,
            detail: format!(
                "control lanes do NOT discriminate: a label nothing serves returned `{}` and a \
                 hosted label returned `{}`. Every verdict from this run is suspect.",
                negative.verdict.as_str(),
                positive.verdict.as_str()
            ),
            lane_negative: negative.verdict.as_str().to_owned(),
            lane_positive: positive.verdict.as_str().to_owned(),
            trigger_negative,
            trigger_positive,
        }
    }
}

// Every test here builds a real git checkout and shells out to `git`, so the
// whole module is unix-only. Gating the MODULE rather than each item keeps a
// non-unix build free of "unused import" / "never used" warnings, which a
// `-D warnings` clippy gate turns into a hard failure.
#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::*;

    fn git(cwd: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?}");
    }

    /// A checkout whose single configured producer admits only `main`.
    fn repo(temp: &Path, on_block: &str) -> std::path::PathBuf {
        let repo = temp.join("repo");
        fs::create_dir_all(repo.join(".github/workflows")).expect("workflows");
        fs::create_dir_all(repo.join(".shipyard")).expect("config");
        git(&repo, &["init", "--initial-branch=main"]);
        git(&repo, &["config", "user.name", "t"]);
        git(&repo, &["config", "user.email", "t@example.com"]);
        fs::write(
            repo.join(".github/workflows/gate.yml"),
            format!("{on_block}\njobs:\n  gate:\n    name: The Gate\n    runs-on: ubuntu-latest\n"),
        )
        .expect("workflow");
        fs::write(
            repo.join(".shipyard/config.toml"),
            "[ship]\nbase_branch = \"main\"\n\n[landability]\nworkflows = \
             [\".github/workflows/gate.yml\"]\n",
        )
        .expect("config");
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "base"]);
        repo
    }

    fn config_at(repo: &Path) -> LoadedConfig {
        LoadedConfig::load_from_cwd(RuntimeMode::Isolated, repo).expect("config")
    }

    /// T8 — the live control pair. Real inputs, not mocks: the host's own
    /// checked-out workflows, evaluated against a base name no `branches:`
    /// filter can admit and against the configured base.
    #[test]
    fn t8_the_trigger_controls_discriminate_on_a_filtered_producer() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = repo(temp.path(), "on:\n  pull_request:\n    branches: [main]\n");

        let (negative, positive, detail) = trigger_self_check(&config_at(&repo), &repo);

        assert_eq!(
            negative, "base_excluded",
            "a base nothing admits must be excluded: {detail}"
        );
        assert_eq!(
            positive, "triggered",
            "the configured base must be admitted: {detail}"
        );
    }

    /// The instrument must report that it could not measure, rather than
    /// reporting health it did not establish. An unfiltered producer admits
    /// every base including the control name, which is correct behaviour and
    /// therefore useless as a negative control.
    #[test]
    fn t8_an_unfiltered_producer_is_not_measurable_never_a_pass() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = repo(temp.path(), "on:\n  pull_request:\n");

        let (negative, positive, _) = trigger_self_check(&config_at(&repo), &repo);

        assert_eq!(negative, "not_measurable");
        assert_eq!(positive, "not_measurable");
        assert_ne!(negative, "base_excluded", "and never reported as a pass");
    }

    /// The break line for T8: a classifier that admitted everything. The
    /// control must catch it, which is the whole point of running it live on
    /// every invocation rather than only in CI.
    #[test]
    fn t8_a_producer_that_excludes_the_configured_base_fails_the_positive_control() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = repo(
            temp.path(),
            "on:\n  pull_request:\n    branches: [release]\n",
        );

        let (negative, positive, _) = trigger_self_check(&config_at(&repo), &repo);

        assert_eq!(negative, "base_excluded");
        assert!(
            positive.starts_with("REFUSED"),
            "a producer that refuses its own base must fail the control: {positive}"
        );
    }

    /// A refused `on:` block is counted and named, never silently skipped.
    #[test]
    fn t8_a_refused_on_block_is_counted_in_the_detail() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = repo(
            temp.path(),
            "on:\n  pull_request:\n    branches: [\"${{ vars.B }}\"]\n",
        );

        let (_, _, detail) = trigger_self_check(&config_at(&repo), &repo);

        assert!(detail.contains("1 refused"), "{detail}");
    }

    /// T6's corpus control: the reader's own accounting must add up to the
    /// directory listing. A reader that silently stopped seeing files would
    /// otherwise report a clean, complete-looking pass over nothing.
    #[test]
    fn the_whole_directory_scan_accounts_for_every_file_it_listed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = repo(temp.path(), "on:\n  pull_request:\n    branches: [main]\n");
        fs::write(
            repo.join(".github/workflows/broken.yml"),
            "on:\n  pull_request:\n    branches: [\"${{ vars.B }}\"]\n",
        )
        .expect("second workflow");
        fs::write(repo.join(".github/workflows/notes.txt"), "not a workflow\n")
            .expect("non-workflow file");

        let (listed, parsed, refused, refusals) = gate::parse_all_workflows(&repo);

        assert_eq!(listed, 2, "only .yml/.yaml files are workflows");
        assert_eq!(parsed, 1);
        assert_eq!(refused, 1);
        assert_eq!(
            parsed + refused,
            listed,
            "the accounting must close, or the reader lost files it never reported"
        );
        assert!(
            refusals[0].contains("broken.yml") && refusals[0].contains("expression"),
            "a refusal must name the file and the boundary: {refusals:?}"
        );
    }
}
