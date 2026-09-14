//! The submission gate: the one production caller that turns a lane
//! assessment into a refusal.
//!
//! `assess_lane_service` has existed in this crate, fully tested, with **zero
//! production callers**. A classifier nothing calls is indistinguishable from
//! one that does not exist — which is precisely what the 2026-09-13 outage
//! demonstrated, six hours at a time. This module is the wire.
//!
//! ## Where the gate refuses, and where it declines to
//!
//! It refuses only on [`Schedulability::Unserved`]: a self-hosted label set
//! that no runner in either scope advertises and that no fresh host
//! attestation declares. Everything else — an unreadable census, an
//! unparsable expression, a starved lane, a missing attestation — is printed
//! loudly and allowed through, because each of those is a statement about the
//! *instrument*, and an instrument that cannot see must not be able to stop
//! the fleet.
//!
//! ## Opt-out, and why it is shaped this way
//!
//! `[landability] enabled = false` turns the gate off for a repository whose
//! self-hosted lanes are served by hosts that do not write an attestation. The
//! per-run escape is `--allow-unserved-lane <label>`, which prints the full
//! diagnosis as a warning and proceeds — the same shape as
//! `--allow-unreachable-targets`, and equally never set by automation.

use std::path::Path;

use chrono::Utc;

use super::assess::{AssessInput, assess};
use super::attestation::{AttestationSet, local_attestation_paths};
use super::gather::{FleetFacts, gather, read_workflow_at_ref};
use super::reach::{
    ContextReachability, HeadEvidence, ReachInput, RequiredContext, WorkflowUnderTest,
    assess_reachability,
};
use super::trigger::parse_workflow_triggers;
use super::workflow::{WorkflowJob, parse_workflow_jobs};
use super::{LandabilityReport, Schedulability};
use crate::cloud::GitHubActions;
use crate::config::LoadedConfig;
use crate::fleet_service::LaneServiceThresholds;

/// Workflow files the gate reads when the config names none.
///
/// Deliberately a short default rather than "every file in the directory": a
/// gate that parsed forty workflows would spend its credibility on expressions
/// it cannot resolve, and the required contexts on this fleet are produced by
/// one file.
pub const DEFAULT_WORKFLOWS: &[&str] = &[".github/workflows/build.yml"];

/// Options the caller controls per invocation.
#[derive(Clone, Debug, Default)]
pub struct GateOptions {
    /// Labels the operator has explicitly waived for this run.
    pub allow_unserved_lanes: Vec<String>,
    /// Workflow paths whose trigger fault the operator has waived.
    ///
    /// Deliberately separate from `allow_unserved_lanes`: a lane fault is
    /// fixed on the fleet and a trigger fault on the pull request, so the
    /// bypass for one must not wave through the other.
    pub allow_unreachable_triggers: Vec<String>,
    /// Skip the gate entirely.
    pub skip: bool,
    /// Ignore the fact cache (used by the live control).
    pub ignore_cache: bool,
    /// Post-open evidence, when the caller already read the pull request.
    pub evidence: Option<HeadEvidence>,
}

/// Outcome of the gate.
#[derive(Clone, Debug)]
pub struct GateOutcome {
    /// The full report, when one was produced.
    pub report: Option<LandabilityReport>,
    /// Lines to print as warnings.
    pub warnings: Vec<String>,
    /// Refusal text when the gate blocks on an unschedulable lane (exit 7).
    pub refusal: Option<String>,
    /// Refusal text when the gate blocks on an unreachable trigger (exit 8).
    ///
    /// Separate field, not a merged string: the caller has to pick an exit
    /// code, and a single blob would force it to parse prose to do so.
    pub trigger_refusal: Option<String>,
    /// API calls this gate actually made.
    pub api_calls: u32,
}

impl GateOutcome {
    /// An outcome that neither blocks nor warns.
    #[must_use]
    pub fn skipped(reason: &str) -> Self {
        Self {
            report: None,
            warnings: vec![reason.to_owned()],
            refusal: None,
            trigger_refusal: None,
            api_calls: 0,
        }
    }
}

/// Whether the repository has opted into the gate. Default on.
#[must_use]
pub fn enabled(config: &LoadedConfig) -> bool {
    config
        .get("landability.enabled")
        .and_then(toml::Value::as_bool)
        .unwrap_or(true)
}

/// Workflow paths to read, from `[landability] workflows` or the default.
#[must_use]
pub fn workflow_paths(config: &LoadedConfig) -> Vec<String> {
    config
        .get("landability.workflows")
        .and_then(toml::Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(toml::Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .filter(|paths| !paths.is_empty())
        .unwrap_or_else(|| {
            DEFAULT_WORKFLOWS
                .iter()
                .map(|path| (*path).to_owned())
                .collect()
        })
}

/// Required contexts from `[governance] required_status_checks`, the fallback
/// when branch protection is unreadable.
#[must_use]
pub fn configured_contexts(config: &LoadedConfig) -> Vec<String> {
    config
        .get("governance.required_status_checks")
        .and_then(toml::Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(toml::Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Run the gate.
///
/// Returns an outcome rather than a `Result` because *every* path here has to
/// produce something printable: a gate that returns an opaque error on its own
/// failure teaches its operator to ignore it.
#[must_use]
pub fn run(
    config: &LoadedConfig,
    cwd: &Path,
    state_dir: &Path,
    repo: &str,
    base: &str,
    options: &GateOptions,
) -> GateOutcome {
    if options.skip {
        return GateOutcome::skipped("landability gate skipped by request");
    }
    if !enabled(config) {
        return GateOutcome::skipped("landability gate disabled by [landability] enabled = false");
    }

    let actions = GitHubActions::from_loaded_config(cwd, config);
    let now = Utc::now();
    let facts: FleetFacts = gather(&actions, state_dir, repo, base, now, options.ignore_cache);

    let mut warnings = facts.warnings.clone();

    let configured = configured_contexts(config);
    let contexts = match &facts.required_contexts {
        Some(live) => {
            // Control: when both sources are readable the live set must cover
            // the configured one. A configured context missing from branch
            // protection means one of the two is wrong, and silently trusting
            // either is how a gate ends up checking nothing.
            for context in &configured {
                if !live.contains(context) {
                    warnings.push(format!(
                        "[governance] required_status_checks names `{context}`, which branch \
                         protection does not require; one of the two is stale"
                    ));
                }
            }
            live.clone()
        }
        None => configured.clone(),
    };

    // Link (1)-(3): will these contexts ever be REQUESTED? This runs before
    // the lane assessment because a refusal here makes the lane question
    // pointless — an unschedulable lane for a run that is never created is
    // not the fault worth reporting.
    // Zero additional API calls: the protection read is the one `gather`
    // already made, and workflows, base and diff are all local.
    let (reachability, reach_warnings) = assess_triggers(
        config,
        cwd,
        base,
        &facts,
        &contexts,
        &configured,
        options,
        &mut warnings,
    );
    warnings.extend(reach_warnings);

    let trigger_refusal = {
        let text = super::reach::render_refusal(&reachability);
        if text.is_empty() { None } else { Some(text) }
    };

    if contexts.is_empty() {
        warnings.push(
            "landability: no required contexts from branch protection or config - nothing to \
             check, which is not the same as nothing wrong"
                .to_owned(),
        );
        return GateOutcome {
            report: Some(LandabilityReport {
                contexts: Vec::new(),
                contexts_source: facts.contexts_source.clone(),
                lanes: Vec::new(),
                contexts_without_producer: Vec::new(),
                fresh_attesters: Vec::new(),
                warnings: Vec::new(),
                reachability,
            }),
            warnings,
            refusal: None,
            trigger_refusal,
            api_calls: facts.api_calls,
        };
    }

    let jobs = read_jobs(config, cwd, base, &mut warnings);
    if jobs.is_empty() {
        warnings.push(
            "landability: no workflow jobs were parsed - the gate measured nothing, which reads \
             identically to a clean result and is not one"
                .to_owned(),
        );
        return GateOutcome {
            report: Some(LandabilityReport {
                contexts,
                contexts_source: facts.contexts_source.clone(),
                lanes: Vec::new(),
                contexts_without_producer: Vec::new(),
                fresh_attesters: Vec::new(),
                warnings: Vec::new(),
                reachability,
            }),
            warnings,
            refusal: None,
            trigger_refusal,
            api_calls: facts.api_calls,
        };
    }

    assess_lanes(
        &facts,
        &contexts,
        &jobs,
        options,
        reachability,
        trigger_refusal,
        warnings,
        now,
    )
}

/// Link (4): the lane assessment #591 owns, unchanged.
#[allow(clippy::too_many_arguments)]
fn assess_lanes(
    facts: &FleetFacts,
    contexts: &[String],
    jobs: &[WorkflowJob],
    options: &GateOptions,
    reachability: Vec<ContextReachability>,
    trigger_refusal: Option<String>,
    mut warnings: Vec<String>,
    now: chrono::DateTime<Utc>,
) -> GateOutcome {
    let attestations = AttestationSet::read_from(&local_attestation_paths());
    let input = AssessInput {
        contexts,
        contexts_source: &facts.contexts_source,
        jobs,
        variables: &facts.variables,
        census: &facts.census,
        census_boundary: facts.census_boundary,
        variables_boundary: facts.variables_boundary,
        attestations: &attestations,
        thresholds: LaneServiceThresholds::default(),
        allow_unserved: &options.allow_unserved_lanes,
    };
    let mut report = assess(&input, now);
    report.reachability = reachability;

    describe_report(&report, &mut warnings);

    let refusal = if report.blocking().is_empty() {
        None
    } else {
        Some(report.render_refusal())
    };

    GateOutcome {
        report: Some(report),
        warnings,
        refusal,
        trigger_refusal,
        api_calls: facts.api_calls,
    }
}

/// Build the required-context list for reachability and classify each one.
///
/// Three sources, because two of them are empty on real repositories:
/// branch protection (authoritative where it exists), `[governance]
/// required_status_checks` (a hand-edited copy), and `[merge]
/// require_platforms` x `[targets.*].workflow` (what Shipyard itself will wait
/// for, which is the only source spectr has).
#[allow(clippy::too_many_arguments)]
fn assess_triggers(
    config: &LoadedConfig,
    cwd: &Path,
    base: &str,
    facts: &FleetFacts,
    live: &[String],
    configured: &[String],
    options: &GateOptions,
    warnings: &mut Vec<String>,
) -> (Vec<ContextReachability>, Vec<String>) {
    let protected: Vec<String> = facts.required_contexts.clone().unwrap_or_default();
    let shipyard_derived = shipyard_required_contexts(config, cwd, base);

    let mut required: Vec<RequiredContext> = Vec::new();
    let mut push = |name: &str, source: Option<String>| {
        if let Some(existing) = required
            .iter_mut()
            .find(|entry: &&mut RequiredContext| entry.name == name)
        {
            if existing.shipyard_source.is_none() {
                existing.shipyard_source = source;
            }
            return;
        }
        required.push(RequiredContext {
            name: name.to_owned(),
            protected: protected.iter().any(|entry| entry == name),
            shipyard_source: source,
        });
    };
    for context in live {
        push(context, None);
    }
    for context in configured {
        push(
            context,
            Some("[governance] required_status_checks".to_owned()),
        );
    }
    for (context, _) in &shipyard_derived {
        push(context, Some("[merge] require_platforms".to_owned()));
    }

    if required.is_empty() {
        return (Vec::new(), Vec::new());
    }

    let extra_workflows: Vec<String> = shipyard_derived
        .iter()
        .map(|(_, path)| path.clone())
        .collect();
    let workflows = read_workflows_under_test(config, cwd, base, &extra_workflows, warnings);

    let changed = match changed_paths(cwd, base) {
        Ok(changed) => changed,
        Err(error) => {
            warnings.push(format!(
                "landability: could not diff against origin/{base} ({error}); every path filter \
                 is reported Unknown rather than assumed to admit"
            ));
            Vec::new()
        }
    };

    let (listed, parsed, refused, refusals) = parse_all_workflows(cwd);
    if listed > 0 {
        let mut summary = format!(
            "landability: `on:` reader parsed {parsed} / refused {refused} of {listed} workflow \
             file(s) in .github/workflows"
        );
        if parsed + refused != listed {
            summary.push_str(
                " - the two do not add up to the directory listing, so the reader lost files it \
                 never reported",
            );
        }
        warnings.push(summary);
        for refusal in refusals.iter().take(10) {
            warnings.push(format!("landability: `on:` refused {refusal}"));
        }
    }

    let input = ReachInput {
        contexts: &required,
        workflows: &workflows,
        base,
        changed_paths: &changed,
        protection_readable: facts.required_contexts.is_some() || facts.protection_absent,
        evidence: options.evidence.as_ref(),
        allow_unreachable: &options.allow_unreachable_triggers,
    };
    let assessments = assess_reachability(&input);
    let reach_warnings = super::reach::warnings(&assessments);
    (assessments, reach_warnings)
}

/// Read the workflow jobs the gate analyses, from the local checkout.
///
/// Zero API calls, and the local checkout is the correct source anyway: the
/// workflow that runs on `pull_request` is the one on the base ref.
fn read_jobs(
    config: &LoadedConfig,
    cwd: &Path,
    base: &str,
    warnings: &mut Vec<String>,
) -> Vec<WorkflowJob> {
    let mut jobs: Vec<WorkflowJob> = Vec::new();
    for path in workflow_paths(config) {
        match read_workflow_at_ref(cwd, &format!("origin/{base}"), &path)
            .or_else(|_| read_workflow_at_ref(cwd, "HEAD", &path))
        {
            Ok(source) => jobs.extend(parse_workflow_jobs(&source)),
            Err(error) => warnings.push(format!(
                "landability: could not read {path} at origin/{base} or HEAD ({error}); the jobs \
                 in it were not checked"
            )),
        }
    }
    jobs
}

/// Turn the non-blocking half of a report into printable warnings.
///
/// Every one of these is a statement about the *instrument* or about a
/// problem with a different owner, and each says which — an unknown that
/// cannot name its boundary sends its reader to the wrong subsystem.
fn describe_report(report: &LandabilityReport, warnings: &mut Vec<String>) {
    warnings.extend(report.warnings.iter().cloned());
    for context in &report.contexts_without_producer {
        warnings.push(format!(
            "landability: required context `{context}` has no producing job in the workflows \
             read - it may come from another workflow or an external App, and was NOT checked"
        ));
    }
    for lane in &report.lanes {
        if lane.verdict == Schedulability::Starved {
            warnings.push(format!(
                "landability: lane `{}` for context `{}` is STARVED - {} (a scheduling problem: \
                 runner-group access, ephemeral consumption, or a workflow permission; not a \
                 runner restore)",
                lane.source, lane.context, lane.report.detail
            ));
        }
        if lane.verdict == Schedulability::Unknown {
            warnings.push(format!(
                "landability: lane `{}` for context `{}` is UNKNOWN - {}",
                lane.source, lane.context, lane.report.detail
            ));
        }
    }
}

/// Required contexts Shipyard itself enforces, from `[merge] require_platforms`
/// crossed with `[targets.*].workflow`.
///
/// The third source of "what must arrive", and on some repositories the only
/// one. spectr's `main` is not branch-protected and its config names no
/// `[governance] required_status_checks`, so both of the sources #591 reads
/// come back empty — and the gate would print "nothing to check, which is not
/// the same as nothing wrong" and proceed, about a repository whose one gate
/// was never going to run. This resolves `require_platforms = ["macos"]` ->
/// `[targets.mac] workflow = "m5-product-acceptance.yml"` -> the rendered job
/// name `Spectr M5 product-acceptance gate`.
///
/// Config-only: zero API calls.
#[must_use]
pub fn shipyard_required_contexts(
    config: &LoadedConfig,
    cwd: &Path,
    base: &str,
) -> Vec<(String, String)> {
    let platforms: Vec<String> = config
        .get("merge.require_platforms")
        .and_then(toml::Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(toml::Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    if platforms.is_empty() {
        return Vec::new();
    }
    let Some(targets) = config.get("targets").and_then(toml::Value::as_table) else {
        return Vec::new();
    };

    let mut out: Vec<(String, String)> = Vec::new();
    for (name, value) in targets {
        let Some(table) = value.as_table() else {
            continue;
        };
        let platform = table
            .get("platform")
            .and_then(toml::Value::as_str)
            .unwrap_or(name);
        let matched = platforms.iter().any(|required| {
            platform == required
                || platform.starts_with(&format!("{required}-"))
                || name == required
        });
        if !matched {
            continue;
        }
        let Some(workflow) = table.get("workflow").and_then(toml::Value::as_str) else {
            continue;
        };
        let path = normalize_workflow_path(workflow);
        let Ok(source) = read_workflow_at_ref(cwd, &format!("origin/{base}"), &path)
            .or_else(|_| read_workflow_at_ref(cwd, "HEAD", &path))
        else {
            continue;
        };
        // Which job in the workflow produces the context Shipyard waits for?
        //
        // With exactly one job the answer is unambiguous, and that is the
        // shape this source exists for: a repository whose whole CI is one
        // gate, with no branch protection to name it. With many jobs, guessing
        // would enumerate every job name in the file as a "required context" —
        // on a 40-job workflow that is 40 confident claims about requirements
        // nobody declared. So a multi-job workflow is narrowed to the job
        // whose rendered name matches the platform or the target, and produces
        // nothing when none does.
        let jobs = parse_workflow_jobs(&source);
        let rendered: Vec<(String, &WorkflowJob)> = jobs
            .iter()
            .filter_map(|job| {
                // A job with an expression name is skipped rather than guessed
                // at: this source exists to name a context exactly, and half a
                // name is worse than none.
                let name = match &job.name_expr {
                    Some(expr) if expr.contains("${{") => return None,
                    Some(expr) => expr.trim().trim_matches(['"', '\'']).to_owned(),
                    None => job.id.clone(),
                };
                Some((name, job))
            })
            .collect();
        let selected: Vec<String> = if rendered.len() == 1 {
            rendered.into_iter().map(|(name, _)| name).collect()
        } else {
            rendered
                .into_iter()
                .filter(|(rendered_name, job)| {
                    let lowered = rendered_name.to_ascii_lowercase();
                    platforms.iter().any(|required| {
                        lowered == required.to_ascii_lowercase() || job.id == *required
                    }) || job.id == *name
                })
                .map(|(rendered_name, _)| rendered_name)
                .collect()
        };
        for context in selected {
            if !out.iter().any(|(existing, _)| *existing == context) {
                out.push((context, path.clone()));
            }
        }
    }
    out
}

/// `m5-product-acceptance.yml` -> `.github/workflows/m5-product-acceptance.yml`.
#[must_use]
pub fn normalize_workflow_path(value: &str) -> String {
    if value.contains('/') {
        value.to_owned()
    } else if std::path::Path::new(value)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("yml") || ext.eq_ignore_ascii_case("yaml"))
    {
        format!(".github/workflows/{value}")
    } else {
        format!(".github/workflows/{value}.yml")
    }
}

/// Files this pull request changes, as GitHub evaluates a `paths:` filter.
///
/// Three-dot: GitHub compares the head against the **merge base**, not against
/// the base tip, so a two-dot diff would report files a sibling pull request
/// changed and admit a workflow the real filter excludes.
pub fn changed_paths(cwd: &Path, base: &str) -> Result<Vec<String>, String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["diff", "--name-only", &format!("origin/{base}...HEAD")])
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect())
}

/// Read each configured workflow at BOTH the base ref and the head.
///
/// For `pull_request` GitHub runs the **head's** copy of the workflow; for
/// `pull_request_target` and `merge_group` it runs the **base's**. A pull
/// request that edits its own trigger is exactly the one whose reachability is
/// in doubt, so both are read and a divergence is reported rather than
/// silently resolved.
/// [`read_workflows_under_test`] for callers outside this module.
///
/// `shipyard pr`'s stacked-base guard runs before any pull request exists and
/// therefore before the gate does, so it needs the same reader.
pub fn read_workflows_under_test_public(
    config: &LoadedConfig,
    cwd: &Path,
    base: &str,
    extra: &[String],
    warnings: &mut Vec<String>,
) -> Vec<WorkflowUnderTest> {
    read_workflows_under_test(config, cwd, base, extra, warnings)
}

fn read_workflows_under_test(
    config: &LoadedConfig,
    cwd: &Path,
    base: &str,
    extra: &[String],
    warnings: &mut Vec<String>,
) -> Vec<WorkflowUnderTest> {
    let mut paths = workflow_paths(config);
    for path in extra {
        if !paths.contains(path) {
            paths.push(path.clone());
        }
    }
    let mut out = Vec::new();
    for path in paths {
        let head = read_workflow_at_ref(cwd, "HEAD", &path);
        let at_base = read_workflow_at_ref(cwd, &format!("origin/{base}"), &path);
        let (source, base_source) = match (&head, &at_base) {
            (Ok(head), base_source) => (head.clone(), base_source.as_ref().ok().cloned()),
            (Err(_), Ok(base_source)) => (base_source.clone(), None),
            (Err(error), Err(_)) => {
                warnings.push(format!(
                    "landability: could not read {path} at HEAD or origin/{base} ({error}); its \
                     trigger was NOT checked"
                ));
                continue;
            }
        };
        out.push(WorkflowUnderTest {
            path: path.clone(),
            jobs: parse_workflow_jobs(&source),
            triggers: parse_workflow_triggers(&source),
            base_triggers: base_source
                .filter(|base_source| *base_source != source)
                .map(|base_source| parse_workflow_triggers(&base_source)),
        });
    }
    out
}

/// Parse every file in `.github/workflows/` and report the split.
///
/// The corpus control for the `on:` reader. `parsed + refused` is compared
/// against a directory listing, so a reader that silently stopped seeing files
/// is visible rather than quietly clean.
#[must_use]
pub fn parse_all_workflows(cwd: &Path) -> (usize, usize, usize, Vec<String>) {
    let dir = cwd.join(".github/workflows");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return (0, 0, 0, Vec::new());
    };
    let mut listed = 0usize;
    let mut parsed = 0usize;
    let mut refused = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let is_workflow = path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext == "yml" || ext == "yaml");
        if !is_workflow || !path.is_file() {
            continue;
        }
        listed += 1;
        let Ok(source) = std::fs::read_to_string(&path) else {
            refused.push(format!("{}: unreadable", path.display()));
            continue;
        };
        match parse_workflow_triggers(&source) {
            Ok(_) => parsed += 1,
            Err(error) => refused.push(format!(
                "{}: {error}",
                path.file_name().unwrap_or_default().to_string_lossy()
            )),
        }
    }
    (listed, parsed, refused.len(), refused)
}

/// Resolve the repository slug for the gate.
///
/// `[repository]` in config wins; otherwise the `origin` remote. Returns
/// `None` rather than erroring: a checkout with no GitHub remote is not a
/// landability failure, it is a repository the gate has nothing to say about.
#[must_use]
pub fn resolve_repo(config: &LoadedConfig, cwd: &Path) -> Option<String> {
    if let Some(repo) = config
        .get("repository")
        .and_then(toml::Value::as_str)
        .filter(|value| !value.trim().is_empty())
    {
        return Some(repo.to_owned());
    }
    let output = std::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    crate::gh::parse_github_remote_slug(String::from_utf8_lossy(&output.stdout).trim())
}

/// Base branch the gate reads protection and workflows from.
///
/// `[ship] base_branch` is in the list because it is the key real
/// configurations actually set — spectr's is the only base declaration in its
/// file — and a gate that read only the two keys nobody writes would silently
/// fall back to the literal `main` on every repository whose base is
/// something else.
#[must_use]
pub fn resolve_base(config: &LoadedConfig) -> String {
    for key in ["governance.base_branch", "ship.base_branch", "base_branch"] {
        if let Some(value) = config
            .get(key)
            .and_then(toml::Value::as_str)
            .filter(|value| !value.trim().is_empty())
        {
            return value.to_owned();
        }
    }
    "main".to_owned()
}
