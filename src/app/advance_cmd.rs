//! `shipyard advance OWNER/REPO#N --retrigger|--release-hold`: perform one
//! routine release-movement act after checking its policy. See
//! [`crate::advance`] for the decisions and `docs/advance.md` for the policy.
//!
//! Reads go through Shipyard's App client. Writes go through the `ghapp`
//! wrapper, so every guard that protects a hand-typed `ghapp` write judges
//! this one too, and no `GHAPP_ALLOW_*` override reaches it: the child's
//! queue authority and overrides are stripped. A guard refusal is reported
//! as a refusal; nothing here steps around one.
//!
//! Exit codes: 0 acted (or, under `--dry-run`, would act); 1 refused; 2 a fact
//! could not be read, or the audit log cannot be written; 3 the act started
//! and did not finish cleanly (for example closed and not reopened), which
//! the audit comment and log both record.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::{Value, json};

use super::CliFailure;
use crate::advance::{
    AUDIT_LOG, Act, Actor, CheckFacts, Decision, DecisionRef, HOLD_LABEL, HoldCondition,
    LabelEvent, PrFacts, QueueFacts, Refusal, ReleaseHoldFacts, RetriggerFacts, Target,
    audit_comment, audit_record, decide_release_hold, decide_retrigger, hold_conditions,
};
use crate::config::LoadedConfig;
use crate::identity::RuntimeMode;
use crate::merge_steward::{RequiredCheck, StewardCheck};
use crate::output::write_json_envelope;

/// Each GitHub read's own deadline.
const GITHUB_READ_TIMEOUT: Duration = Duration::from_mins(1);
/// A comment as the approval gate reads it (`head_approval`'s record).
const COMMENT_RECORD_JQ: &str = ".[] | {id: .id, login: .user.login, type: .user.type, association: .author_association, at: .created_at, body: .body, performed_via_github_app_id: .performed_via_github_app.id}";
/// A label event.
const EVENT_RECORD_JQ: &str =
    ".[] | {event: .event, label: .label.name, login: .actor.login, at: .created_at}";

/// What the caller asked for, as the CLI parsed it.
#[allow(clippy::struct_excessive_bools)]
pub(super) struct AdvanceArgs {
    pub(super) target: String,
    pub(super) retrigger: bool,
    pub(super) release_hold: bool,
    pub(super) reason: Option<String>,
    pub(super) decision: Option<String>,
    pub(super) head: Option<String>,
    pub(super) dry_run: bool,
    pub(super) json: bool,
}

/// A GitHub call scoped to a repository: `(repo, argv)`.
pub(super) type Gh<'a> = &'a dyn Fn(&str, &[String]) -> Result<String, String>;

/// Everything outside the decision.
pub(super) struct Io<'a> {
    /// Reads, through the App client.
    pub(super) read: Gh<'a>,
    /// Writes, through `ghapp`.
    pub(super) write: Gh<'a>,
    /// The time decisions are made at.
    pub(super) now: DateTime<Utc>,
    /// Where the audit log lives.
    pub(super) state_dir: &'a Path,
    /// This host's owner record for the target, when it has one.
    pub(super) owner_record: Option<crate::owner_registry::OwnerRecord>,
}

/// Run `shipyard advance`.
pub(super) fn advance_command<W: Write>(
    mode: RuntimeMode,
    cwd: &Path,
    state_dir: &Path,
    args: &AdvanceArgs,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let target = Target::parse(&args.target).map_err(|error| CliFailure::new(2, error))?;
    let config = LoadedConfig::load_from_cwd(mode, cwd)
        .map_err(|error| CliFailure::new(2, format!("config error: {error}")))?;
    let read = |repo: &str, gh_args: &[String]| {
        crate::cloud::GitHubActions::from_loaded_config(cwd, &config)
            .with_repo_override(repo)
            .run_gh_with_timeout(gh_args, GITHUB_READ_TIMEOUT)
            .map_err(|error| error.to_string())
    };
    let write = |repo: &str, gh_args: &[String]| ghapp(cwd, repo, gh_args);
    let env = |name: &str| std::env::var(name).ok();
    let actor = Actor::from_env(crate::owner_registry::resolve_host_name(None, &env), &env);
    let io = Io {
        read: &read,
        write: &write,
        now: Utc::now(),
        state_dir,
        owner_record: crate::owner_registry::load(state_dir, &target.repo, target.pr),
    };
    run(&io, &target, args, &actor, stdout)
}

/// One `ghapp` call, with no queue authority or guard override inherited.
fn ghapp(cwd: &Path, repo: &str, args: &[String]) -> Result<String, String> {
    let mut command = Command::new("ghapp");
    command
        .args(args)
        .current_dir(cwd)
        .env("GH_REPO", repo)
        .stdin(Stdio::null());
    crate::queue_arm_gate::strip_queue_authority(&mut command);
    let output = command
        .output()
        .map_err(|error| format!("ghapp could not run: {error}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(format!(
            "ghapp exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// How a run ends before acting.
enum Stop {
    /// A condition failed.
    Refused(Refusal),
    /// A fact could not be read.
    Unread(String),
    /// The act started and did not finish cleanly; the text says how far it
    /// got, for the audit comment and log.
    Partial(String),
}

impl From<Refusal> for Stop {
    fn from(refusal: Refusal) -> Self {
        Self::Refused(refusal)
    }
}

/// The testable body of the command.
pub(super) fn run<W: Write>(
    io: &Io<'_>,
    target: &Target,
    args: &AdvanceArgs,
    actor: &Actor,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let act = match (args.retrigger, args.release_hold) {
        (true, false) => Act::Retrigger,
        (false, true) => Act::ReleaseHold,
        _ => {
            return Err(CliFailure::new(
                2,
                "name exactly one act: --retrigger or --release-hold",
            ));
        }
    };
    if act == Act::Retrigger && args.decision.is_some() {
        return Err(CliFailure::new(2, "--decision applies to --release-hold"));
    }
    if let Some(head) = &args.head
        && !(head.len() == 40 && head.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        return Err(CliFailure::new(
            2,
            format!("--head must be a full 40-character sha, not {head:?}"),
        ));
    }
    let decided = match act {
        Act::Retrigger => decide_retrigger_live(io, target, args, actor),
        Act::ReleaseHold => decide_release_hold_live(io, target, args, actor),
    };
    let decision = match decided {
        Ok(decision) => decision,
        Err(Stop::Refused(refusal)) => return refused(target, act, &refusal, args.json, stdout),
        Err(Stop::Unread(detail) | Stop::Partial(detail)) => {
            return unread(target, act, &detail, args.json, stdout);
        }
    };
    if args.dry_run {
        report(
            &decision,
            "dry-run: would act; nothing changed",
            args.json,
            stdout,
        )?;
        return Ok(ExitCode::SUCCESS);
    }
    act_and_audit(io, target, &decision, actor, args.json, stdout)
}

/// Perform the decided act, bound to its head, then audit it once: one
/// comment on the pull request and one line in the local log.
fn act_and_audit<W: Write>(
    io: &Io<'_>,
    target: &Target,
    decision: &Decision,
    actor: &Actor,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let act = decision.act;
    let log = io.state_dir.join(AUDIT_LOG);
    if let Err(error) = log_writable(&log) {
        return unread(
            target,
            act,
            &format!(
                "audit log {} cannot be written ({error}); an act nobody can audit is not taken",
                log.display()
            ),
            json,
            stdout,
        );
    }
    // Bind to the head the decision was made at: re-read just before writing.
    match read_pr(io, target) {
        Ok(pr) if pr.head == decision.head && pr.state == "open" && !pr.merged => {}
        Ok(pr) => {
            let refusal = Refusal {
                condition: "head_moved",
                detail: format!(
                    "decided at head {} but the pull request now reads {} ({})",
                    decision.head, pr.head, pr.state
                ),
            };
            return refused(target, act, &refusal, json, stdout);
        }
        Err(detail) => return unread(target, act, &detail, json, stdout),
    }
    // A close drops a queue entry and disarms auto-merge: re-check both.
    if act == Act::Retrigger {
        match read_queue(io, target) {
            Ok((queue, _))
                if queue.head == decision.head && !queue.in_queue && !queue.auto_merge_armed => {}
            Ok((queue, _)) => {
                let refusal = Refusal {
                    condition: "queue_changed",
                    detail: format!(
                        "since the decision the merge-queue state reads head {}, queued {}, \
                         armed {}",
                        queue.head, queue.in_queue, queue.auto_merge_armed
                    ),
                };
                return refused(target, act, &refusal, json, stdout);
            }
            Err(detail) => return unread(target, act, &detail, json, stdout),
        }
    }
    let performed = match act {
        Act::Retrigger => retrigger(io, decision),
        Act::ReleaseHold => release_hold(io, decision),
    };
    let (outcome, clean) = match performed {
        Ok(outcome) => (outcome, true),
        Err(Stop::Refused(refusal)) => return refused(target, act, &refusal, json, stdout),
        Err(Stop::Partial(outcome) | Stop::Unread(outcome)) => (outcome, false),
    };
    let body = audit_comment(decision, actor, &outcome);
    let comment = post_comment(io, target, &body);
    let record = audit_record(decision, actor, &outcome, &comment, io.now);
    let logged = append_line(&log, &record);
    report(decision, &outcome, json, stdout)?;
    if let Err(error) = &comment {
        eprintln!("advance: the audit comment could not be posted: {error}");
    }
    if let Err(error) = &logged {
        eprintln!(
            "advance: the audit log {} could not be written: {error}",
            log.display()
        );
    }
    Ok(if clean && comment.is_ok() && logged.is_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(3)
    })
}

fn decide_retrigger_live(
    io: &Io<'_>,
    target: &Target,
    args: &AdvanceArgs,
    actor: &Actor,
) -> Result<Decision, Stop> {
    let reason = args.reason.as_deref().unwrap_or_default();
    let pr = read_pr(io, target).map_err(Stop::Unread)?;
    let (queue, _) = read_queue(io, target).map_err(Stop::Unread)?;
    let checks = read_checks(io, &target.repo, &pr.base, &pr.head).map_err(Stop::Unread)?;
    let comments = read_comments(io, target).map_err(Stop::Unread)?;
    let facts = RetriggerFacts {
        pr,
        queue,
        checks,
        comments,
        owner_record: io.owner_record.clone(),
    };
    Ok(decide_retrigger(
        target,
        &facts,
        actor,
        reason,
        args.head.as_deref(),
        io.now,
    )?)
}

fn decide_release_hold_live(
    io: &Io<'_>,
    target: &Target,
    args: &AdvanceArgs,
    actor: &Actor,
) -> Result<Decision, Stop> {
    let decision_ref = args
        .decision
        .as_deref()
        .map(DecisionRef::parse)
        .transpose()
        .map_err(|detail| {
            Stop::Refused(Refusal {
                condition: "decision_invalid",
                detail,
            })
        })?;
    let pr = read_pr(io, target).map_err(Stop::Unread)?;
    let (_, queue_value) = read_queue(io, target).map_err(Stop::Unread)?;
    let checks = read_checks(io, &target.repo, &pr.base, &pr.head).map_err(Stop::Unread)?;
    let comments = read_comments(io, target).map_err(Stop::Unread)?;
    let events = read_events(io, target).map_err(Stop::Unread)?;
    let gate = read_gate(io, target, &pr, &queue_value).map_err(Stop::Unread)?;
    let decision = match decision_ref {
        Some(reference) => {
            let content = read_decision(io, &reference).map_err(Stop::Unread)?;
            Some((reference, content))
        }
        None => None,
    };
    let mut merged = BTreeMap::new();
    if decision.is_none() {
        for wanted in merged_targets(&events, &comments) {
            let other = Target::parse(&wanted).map_err(Stop::Unread)?;
            let state = read_pr(io, &other).map_err(Stop::Unread)?;
            merged.insert(wanted, state.merged);
        }
    }
    let facts = ReleaseHoldFacts {
        pr,
        checks,
        gate,
        comments,
        events,
        merged,
        decision,
        owner_record: io.owner_record.clone(),
    };
    Ok(decide_release_hold(
        target,
        &facts,
        actor,
        args.head.as_deref(),
        io.now,
    )?)
}

/// The pull requests a `merged` condition names, in any comment that could
/// be the hold comment. Reading a few extra is cheaper than a second pass.
fn merged_targets(events: &[LabelEvent], comments: &[Value]) -> BTreeSet<String> {
    let holders: BTreeSet<&str> = events
        .iter()
        .filter(|event| event.label == HOLD_LABEL && event.event == "labeled")
        .map(|event| event.login.as_str())
        .collect();
    comments
        .iter()
        .filter(|comment| {
            comment
                .get("login")
                .and_then(Value::as_str)
                .is_some_and(|login| holders.contains(login))
        })
        .filter_map(|comment| comment.get("body").and_then(Value::as_str))
        .filter(|body| body.contains(HOLD_LABEL))
        .flat_map(hold_conditions)
        .filter_map(|condition| match condition {
            HoldCondition::Merged { target } => Some(target),
            _ => None,
        })
        .collect()
}

fn retrigger(io: &Io<'_>, decision: &Decision) -> Result<String, Stop> {
    let repo = &decision.target.repo;
    let pull = format!("repos/{repo}/pulls/{}", decision.target.pr);
    let patch = |state: &str| {
        (io.write)(
            repo,
            &[
                "api".to_owned(),
                "-X".to_owned(),
                "PATCH".to_owned(),
                pull.clone(),
                "-f".to_owned(),
                format!("state={state}"),
            ],
        )
    };
    let closed = patch("closed").map_err(|error| {
        Stop::Refused(Refusal {
            condition: "close_refused",
            detail: format!(
                "the close was refused, so nothing changed: {error}. If pr-close-guard refused \
                 it, see docs/advance.md (the guard needs the change proposed there)"
            ),
        })
    })?;
    if !response_state_is(&closed, "closed") {
        return Err(Stop::Partial(format!(
            "the close returned no closed state; the pull request may be closed, check \
             it: {}",
            closed.trim()
        )));
    }
    let reopened = patch("open").or_else(|_| patch("open"));
    match reopened {
        Ok(raw) if response_state_is(&raw, "open") => Ok(format!(
            "closed and reopened at head {}; GitHub rebuilds the merge ref",
            decision.head
        )),
        Ok(raw) => Err(Stop::Partial(format!(
            "closed, and the reopen returned no open state ({}); the pull request may be left \
             closed: reopen it",
            raw.trim()
        ))),
        Err(error) => Err(Stop::Partial(format!(
            "closed, and the reopen failed twice ({error}); the pull request is left closed: \
             reopen it"
        ))),
    }
}

fn release_hold(io: &Io<'_>, decision: &Decision) -> Result<String, Stop> {
    let repo = &decision.target.repo;
    let remaining = (io.write)(
        repo,
        &[
            "api".to_owned(),
            "-X".to_owned(),
            "DELETE".to_owned(),
            format!(
                "repos/{repo}/issues/{}/labels/{}",
                decision.target.pr,
                crate::required_check_policy::encode_path_segment(HOLD_LABEL)
            ),
        ],
    )
    .map_err(|error| {
        Stop::Refused(Refusal {
            condition: "label_removal_refused",
            detail: format!("the label removal was refused, so nothing changed: {error}"),
        })
    })?;
    let labels: Value = serde_json::from_str(remaining.trim()).map_err(|error| {
        Stop::Partial(format!(
            "the label removal returned unreadable labels ({error}); check whether \
             {HOLD_LABEL} is gone"
        ))
    })?;
    let still = labels.as_array().is_none_or(|labels| {
        labels
            .iter()
            .any(|label| label.get("name").and_then(Value::as_str) == Some(HOLD_LABEL))
    });
    if still {
        return Err(Stop::Partial(format!(
            "the label removal returned, but {HOLD_LABEL} is still listed"
        )));
    }
    Ok(format!("removed {HOLD_LABEL} at head {}", decision.head))
}

fn response_state_is(raw: &str, state: &str) -> bool {
    serde_json::from_str::<Value>(raw.trim())
        .ok()
        .and_then(|value| {
            value
                .get("state")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .is_some_and(|found| found == state)
}

fn post_comment(io: &Io<'_>, target: &Target, body: &str) -> Result<u64, String> {
    let raw = (io.write)(
        &target.repo,
        &[
            "api".to_owned(),
            format!("repos/{}/issues/{}/comments", target.repo, target.pr),
            "-f".to_owned(),
            format!("body={body}"),
        ],
    )?;
    serde_json::from_str::<Value>(raw.trim())
        .ok()
        .and_then(|value| value.get("id").and_then(Value::as_u64))
        .ok_or_else(|| "the comment post returned no id".to_owned())
}

fn log_writable(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map(drop)
}

fn append_line(path: &Path, record: &Value) -> Result<(), String> {
    let mut line = serde_json::to_string(record).map_err(|error| error.to_string())?;
    line.push('\n');
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| error.to_string())?;
    file.write_all(line.as_bytes())
        .and_then(|()| file.sync_data())
        .map_err(|error| error.to_string())
}

fn read_json(io: &Io<'_>, repo: &str, args: &[String], what: &str) -> Result<Value, String> {
    let raw = (io.read)(repo, args).map_err(|error| format!("{what} unreadable: {error}"))?;
    serde_json::from_str(raw.trim()).map_err(|error| format!("{what} unreadable: {error}"))
}

fn read_pr(io: &Io<'_>, target: &Target) -> Result<PrFacts, String> {
    let value = read_json(
        io,
        &target.repo,
        &[
            "api".to_owned(),
            format!("repos/{}/pulls/{}", target.repo, target.pr),
        ],
        &format!("{} pull request", target.label()),
    )?;
    PrFacts::parse(&value).map_err(|error| format!("{} unreadable: {error}", target.label()))
}

fn read_queue(io: &Io<'_>, target: &Target) -> Result<(QueueFacts, Value), String> {
    let (owner, name) = target
        .repo
        .split_once('/')
        .ok_or_else(|| "invalid repository".to_owned())?;
    let value = read_json(
        io,
        &target.repo,
        &[
            "api".to_owned(),
            "graphql".to_owned(),
            "-f".to_owned(),
            format!("query={}", crate::pr_queue_state::PR_QUEUE_STATE_QUERY),
            "-F".to_owned(),
            format!("owner={owner}"),
            "-F".to_owned(),
            format!("name={name}"),
            "-F".to_owned(),
            format!("number={}", target.pr),
        ],
        "merge-queue state",
    )?;
    let facts = QueueFacts::parse(&value).map_err(|error| error.clone())?;
    Ok((facts, value))
}

fn read_records(io: &Io<'_>, repo: &str, path: &str, jq: &str) -> Result<Vec<Value>, String> {
    let raw = (io.read)(
        repo,
        &[
            "api".to_owned(),
            "--paginate".to_owned(),
            path.to_owned(),
            "--jq".to_owned(),
            jq.to_owned(),
        ],
    )
    .map_err(|error| format!("{path} unreadable: {error}"))?;
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line).map_err(|error| format!("malformed {path} record: {error}"))
        })
        .collect()
}

fn read_comments(io: &Io<'_>, target: &Target) -> Result<Vec<Value>, String> {
    read_records(
        io,
        &target.repo,
        &format!("repos/{}/issues/{}/comments", target.repo, target.pr),
        COMMENT_RECORD_JQ,
    )
}

fn read_events(io: &Io<'_>, target: &Target) -> Result<Vec<LabelEvent>, String> {
    read_records(
        io,
        &target.repo,
        &format!("repos/{}/issues/{}/events", target.repo, target.pr),
        EVENT_RECORD_JQ,
    )?
    .iter()
    .filter_map(|record| LabelEvent::parse(record).transpose())
    .collect()
}

/// The required rule set for `base`: classic protection and rulesets, read
/// as the steward reads them.
fn read_required(
    io: &Io<'_>,
    repo: &str,
    base: &str,
) -> Result<(Vec<RequiredCheck>, Option<String>), String> {
    let encoded = crate::required_check_policy::encode_path_segment(base);
    let rules = read_json(
        io,
        repo,
        &[
            "api".to_owned(),
            format!("repos/{repo}/rules/branches/{encoded}"),
            "--paginate".to_owned(),
            "--slurp".to_owned(),
        ],
        "evaluated branch rules",
    )?;
    let mut rulesets_only = None;
    let mut checks = match (io.read)(
        repo,
        &[
            "api".to_owned(),
            format!("repos/{repo}/branches/{encoded}/protection/required_status_checks"),
        ],
    ) {
        Ok(raw) => crate::required_check_policy::classic_required_checks(
            &serde_json::from_str(raw.trim())
                .map_err(|error| format!("required-check policy unreadable: {error}"))?,
        )?,
        // GitHub's own "not protected" answers: nothing to read.
        Err(error)
            if error.contains("HTTP 404")
                || super::merge_steward_cmd::is_private_free_entitlement(&error) =>
        {
            Vec::new()
        }
        // No administration access: the steward reads rulesets only here,
        // and so does this, saying so in the evidence.
        Err(error) if super::merge_steward_cmd::is_admin_protection_denied(&error) => {
            rulesets_only = Some("HTTP 403 without administration access".to_owned());
            Vec::new()
        }
        Err(error) => return Err(format!("required-check policy unreadable: {error}")),
    };
    checks.extend(crate::required_check_policy::evaluated_required_checks(
        &rules,
    )?);
    Ok((
        crate::required_check_policy::normalize_required_checks(checks),
        rulesets_only,
    ))
}

/// Every check run and commit status at `head`, refusing a partial read.
fn read_checks(io: &Io<'_>, repo: &str, base: &str, head: &str) -> Result<CheckFacts, String> {
    let (required, rulesets_only) = read_required(io, repo, base)?;
    let pages = read_json(
        io,
        repo,
        &[
            "api".to_owned(),
            format!("repos/{repo}/commits/{head}/check-runs?per_page=100"),
            "--paginate".to_owned(),
            "--slurp".to_owned(),
        ],
        "check runs at head",
    )?;
    let pages = pages
        .as_array()
        .ok_or_else(|| "check runs at head were not paginated pages".to_owned())?;
    let total = pages
        .first()
        .and_then(|page| page.get("total_count"))
        .and_then(Value::as_u64)
        .ok_or_else(|| "check runs at head carried no total_count".to_owned())?;
    let mut checks: Vec<StewardCheck> = Vec::new();
    for page in pages {
        for run in page
            .get("check_runs")
            .and_then(Value::as_array)
            .ok_or_else(|| "a check-run page carried no check_runs".to_owned())?
        {
            checks.push(
                super::merge_steward_cmd::parse_rest_check(run)
                    .ok_or_else(|| "a check run at head is malformed".to_owned())?,
            );
        }
    }
    if u64::try_from(checks.len()).ok() != Some(total) {
        return Err(format!(
            "read {} of {total} check runs at head; refusing a partial read",
            checks.len()
        ));
    }
    let statuses = read_json(
        io,
        repo,
        &[
            "api".to_owned(),
            format!("repos/{repo}/commits/{head}/statuses?per_page=100"),
            "--paginate".to_owned(),
            "--slurp".to_owned(),
        ],
        "commit statuses at head",
    )?;
    for page in statuses
        .as_array()
        .ok_or_else(|| "commit statuses at head were not paginated pages".to_owned())?
    {
        for status in page
            .as_array()
            .ok_or_else(|| "a commit-status page was not an array".to_owned())?
        {
            checks.push(
                super::merge_steward_cmd::parse_rest_status(status)
                    .ok_or_else(|| "a commit status at head is malformed".to_owned())?,
            );
        }
    }
    Ok(CheckFacts {
        required,
        checks,
        rulesets_only,
    })
}

/// The steward's own exact-head approval evaluation: the base's policy read
/// strictly, judged as a dry run (a carry is judged, never logged).
fn read_gate(
    io: &Io<'_>,
    target: &Target,
    pr: &PrFacts,
    queue: &Value,
) -> Result<crate::head_approval::HeadGate, String> {
    let run = |args: &[String]| (io.read)(&target.repo, args);
    let mut policy = crate::head_approval::read_policy_required(&run, &target.repo, &pr.base)
        .map_err(|error| format!("head approval policy unreadable: {error}"))?;
    policy.dry_run = true;
    crate::head_approval::evaluate_with_policy_for_author_with_events(
        &run,
        &target.repo,
        target.pr,
        &pr.head,
        &policy,
        queue,
        Some(pr.author.as_str()),
    )
    .map(|(gate, _)| gate)
    .map_err(|error| format!("head approval unreadable: {error}"))
}

fn read_decision(io: &Io<'_>, reference: &DecisionRef) -> Result<String, String> {
    let mut path = format!(
        "repos/{}/contents/{}",
        reference.repo,
        reference
            .path
            .split('/')
            .map(crate::required_check_policy::encode_path_segment)
            .collect::<Vec<_>>()
            .join("/")
    );
    if let Some(git_ref) = &reference.git_ref {
        path.push_str("?ref=");
        path.push_str(&crate::required_check_policy::encode_path_segment(git_ref));
    }
    (io.read)(
        &reference.repo,
        &[
            "api".to_owned(),
            "-H".to_owned(),
            "Accept: application/vnd.github.raw+json".to_owned(),
            path,
        ],
    )
    .map_err(|error| format!("decision {} unreadable: {error}", reference.text))
}

fn report<W: Write>(
    decision: &Decision,
    outcome: &str,
    json: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    if json {
        let mut data = BTreeMap::new();
        data.insert("acted".to_owned(), Value::Bool(true));
        data.insert(
            "decision".to_owned(),
            serde_json::to_value(decision)
                .map_err(|error| CliFailure::new(2, error.to_string()))?,
        );
        data.insert("outcome".to_owned(), Value::String(outcome.to_owned()));
        return write_json_envelope(stdout, "advance", data)
            .map_err(|error| CliFailure::new(2, error.to_string()));
    }
    let mut text = format!(
        "{} {}: {outcome}\n  why: {}\n",
        decision.act.as_str(),
        decision.target.label(),
        decision.reason
    );
    for line in &decision.evidence {
        text.push_str("  checked: ");
        text.push_str(line);
        text.push('\n');
    }
    stdout
        .write_all(text.as_bytes())
        .map_err(|error| CliFailure::new(2, error.to_string()))
}

fn refused<W: Write>(
    target: &Target,
    act: Act,
    refusal: &Refusal,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    stop(
        target,
        act,
        &json!({"refused": refusal}),
        &format!("refused: {refusal}"),
        json,
        stdout,
    )?;
    Ok(ExitCode::from(1))
}

fn unread<W: Write>(
    target: &Target,
    act: Act,
    detail: &str,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    stop(
        target,
        act,
        &json!({"unread": detail}),
        &format!("refused, a fact could not be read: {detail}"),
        json,
        stdout,
    )?;
    Ok(ExitCode::from(2))
}

fn stop<W: Write>(
    target: &Target,
    act: Act,
    value: &Value,
    text: &str,
    json: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    if json {
        let mut data = BTreeMap::new();
        data.insert("acted".to_owned(), Value::Bool(false));
        data.insert("act".to_owned(), Value::String(act.as_str().to_owned()));
        data.insert("target".to_owned(), Value::String(target.label()));
        if let Some(object) = value.as_object() {
            for (key, value) in object {
                data.insert(key.clone(), value.clone());
            }
        }
        return write_json_envelope(stdout, "advance", data)
            .map_err(|error| CliFailure::new(2, error.to_string()));
    }
    writeln!(stdout, "{} {}: {text}", act.as_str(), target.label())
        .map_err(|error| CliFailure::new(2, error.to_string()))
}

#[cfg(test)]
mod tests;
