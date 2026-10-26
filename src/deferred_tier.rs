//! Refuse to arm, enqueue or merge a head whose tiered required check has not
//! proven its full lane ran.
//!
//! ## Why a green check is not enough
//!
//! A repository can gate an expensive lane behind a fleet-App dispatch: on a
//! pull request run, the required check runs only a cheap contract lane and
//! concludes `success`, and the full lane runs when the fleet App dispatches
//! the workflow on the head. GitHub cannot tell that success from the full
//! lane's, so native auto-merge would enqueue the head as soon as it is armed.
//!
//! ## Opting in
//!
//! The rule applies only to checks the base branch's `.shipyard/config.toml`
//! names, read through the GitHub API (never from the head):
//!
//! ```toml
//! [queue]
//! tiered_required_checks = ["macos"]
//! tier_record_app = "pulp-tier-records"
//! ```
//!
//! A repository without the key pays that one read and nothing else.
//!
//! ## Where the tier is read from
//!
//! Not from annotations: any step of the job, including the code under test,
//! can print a `::notice`, so a later annotation can claim a wider tier and
//! enough annotations push the real one past GitHub's caps. The tier comes
//! from a **tier record**: a check run named [`TIER_RECORD_PREFIX`]`<check>`
//! (for example `shipyard-test-tier/macos`) on the head, created by the
//! GitHub App the base names in `tier_record_app`, whose `output.summary` is
//! one JSON object:
//!
//! ```json
//! {"schema":"shipyard-test-tier/v1","check":"macos","tier":"deferred",
//!  "run_id":123,"run_attempt":1,"trigger":"pull_request","actor":"author",
//!  "full_lane_workflow":"build.yml","tested_ref":"refs/pull/42/merge",
//!  "tested_sha":"<merge commit>","merged_base":"<its base parent>",
//!  "full_suite_runs_in":"merge_group"}
//! ```
//!
//! `tested_ref`, `tested_sha` and `merged_base` say what the lane checked
//! out. A full lane counts only for this pull request's merge ref at the
//! current head ([`FULL_LANE_PR_INPUT`] names the pull request in the
//! dispatch).
//!
//! `trigger` and `actor` say which event and account started the run (a full
//! lane is a `workflow_dispatch` by `shipyard-local[bot]`); they are reported,
//! not trusted, since the record App decided the tier. `full_lane_workflow`
//! names the workflow whose dispatch on the head, by the fleet App, runs the
//! full lane; the refusal's fix says to dispatch it. A label is never a
//! full-lane path: a `labeled` run shadows the required check the merge queue
//! reads.
//!
//! `run_id` and `run_attempt` bind the record to one attempt of the workflow
//! run whose job is the check, so a record from an earlier run never speaks
//! for a rerun.
//!
//! ## Why the record's app may not be `github-actions`
//!
//! GitHub lets a check run be updated only by the app that created it. Every
//! workflow job's `GITHUB_TOKEN` is the `github-actions` app, and a pull
//! request job may hold `checks: write` (the default under permissive token
//! settings), so a record that app created could be PATCHed from `deferred`
//! to `full` by the code under test, with nothing on the check run to show
//! it: check runs carry no update time. A record created by a separate App,
//! whose credentials only a `workflow_run` workflow on the default branch
//! holds, cannot be touched by pull request code at all. A config naming
//! tiered checks without `tier_record_app`, or naming `github-actions`,
//! refuses every landing, and so does one naming the fleet App
//! (`shipyard-local`), whose credentials every agent holds.
//!
//! ## The tiered job may not hold `checks: write` either
//!
//! Defence in depth beside the App rule: a tiered job holding `checks: write`
//! could rewrite check runs on its head, and a check run carries no update
//! time to show it. A same-repo pull
//! request can grant that in its own workflow file whatever the repository
//! default is. So before a head is allowed, the tiered job's log is read for
//! the `GITHUB_TOKEN Permissions` group the runner prints in "Set up job":
//! `Checks` must be `read` or `none` (absent from the group is `none`). A log
//! that cannot be read, or a group that is missing or does not parse, refuses.
//!
//! ## The rule
//!
//! For each tiered check, take its latest check run on the head (highest id):
//!
//! 1. Missing or not completed: refuse. Arming before checks run is the
//!    normal arm-on-open flow, and a head armed then would be enqueued the
//!    moment a deferred lane goes green. The steward's native-arm backstop
//!    arms it once it is green and full.
//! 2. Not a GitHub Actions job, or its job's run cannot be read: refuse.
//! 3. Among the tier records bound to that run attempt, any `deferred`
//!    refuses, and none at all refuses. A later record cannot widen an
//!    earlier `deferred` one, so a forged record can only refuse.
//! 4. Every remaining record must name `tested_ref` `refs/pull/<n>/merge`
//!    for this pull request, a `tested_sha` whose parents are exactly
//!    `[merged_base, head]` (GitHub's order), and a `merged_base` on the base
//!    branch: a bare branch, a stale merge or the reversed order refuses.
//! 5. The tiered job's token may not have held `checks: write`.
//!
//! The `ghapp` queue-arm guard applies the same rule (`tier_blockers` in
//! `scripts/ghapp_queue_arm_guard.py`); keep the two in lockstep. No override
//! skips it.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

/// Tier a check reports when it ran only a cheap lane and the full lane waits
/// on a fleet-App dispatch.
pub const DEFERRED_TIER: &str = "deferred";
/// The log group the runner prints listing the job's token scopes.
pub const TOKEN_PERMISSIONS_HEADER: &str = "GITHUB_TOKEN Permissions";
/// The `workflow_dispatch` input naming the pull request whose merge ref
/// (`refs/pull/<n>/merge`) the full lane checks out.
pub const FULL_LANE_PR_INPUT: &str = "pr_number";
/// Name prefix of a tier record check run; the check's name follows it.
pub const TIER_RECORD_PREFIX: &str = "shipyard-test-tier/";
/// Schema identifier inside a tier record's `output.summary`.
pub const TIER_RECORD_SCHEMA: &str = "shipyard-test-tier/v1";
/// The app every workflow job's `GITHUB_TOKEN` acts as. Tiered checks are
/// its jobs; tier records may never be its check runs.
pub const ACTIONS_APP: &str = "github-actions";

/// The fleet App every agent acts as through `ghapp`; it may not write tier
/// records either.
const FLEET_APP: &str = "shipyard-local";

/// What a base's config says about tiered checks.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TierPolicy {
    /// `[queue] tiered_required_checks`.
    pub checks: Vec<String>,
    /// `[queue] tier_record_app`: the only app whose tier records count.
    pub record_app: String,
}

/// A `gh` transport: stdout, or a message that includes stderr.
pub type RunGh<'a> = &'a dyn Fn(&[String]) -> Result<String, String>;

/// One tiered check that holds a head back.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DeferredCheck {
    /// Check name, as the base's config lists it.
    pub check: String,
    /// The pull request the head belongs to.
    pub pr: u64,
    /// The head the check was read on.
    pub head: String,
    /// Why it holds the head back.
    pub blocker: Blocker,
}

/// Why a [`DeferredCheck`] holds its head back.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "blocker", rename_all = "snake_case")]
pub enum Blocker {
    /// The check has not run, or has not completed, on the head.
    Pending {
        /// `missing`, or the check run's status.
        status: String,
    },
    /// A tier record bound to the check's run reports [`DEFERRED_TIER`].
    Deferred {
        /// The selector the cheap lane ran, when given.
        selector: Option<String>,
        /// The workflow whose dispatch on the head runs the full lane, when
        /// given (`full_lane_workflow`).
        full_lane_workflow: Option<String>,
        /// Where the full suite runs instead, when given.
        full_suite_runs_in: Option<String>,
    },
    /// No tier record is bound to the check's run attempt.
    MissingRecord {
        /// The workflow run of the check's job.
        run_id: u64,
        /// Its attempt.
        run_attempt: u64,
    },
    /// A full-lane record that does not prove it tested this pull request's
    /// merge with its base at the current head: a bare branch, or a merge of
    /// an older head.
    UntestedMerge {
        /// What the record tested instead.
        reason: String,
        /// The workflow whose dispatch runs the full lane, when given.
        full_lane_workflow: Option<String>,
    },
    /// The tiered job's token could write check runs, so it could have
    /// rewritten its own tier record.
    ChecksWritable {
        /// The job's `Checks` scope, as its log printed it.
        level: String,
    },
    /// The check's run could not be bound to a tier record.
    Unreadable {
        /// What could not be read.
        reason: String,
    },
}

impl DeferredCheck {
    /// One line for the ship transcript or the steward report.
    #[must_use]
    pub fn explain(&self) -> String {
        let head = short(&self.head);
        let check = &self.check;
        let why = match &self.blocker {
            Blocker::Pending { status } if status == "missing" => {
                format!("tiered required check `{check}` has not run on {head}")
            }
            Blocker::Pending { status } => {
                format!("tiered required check `{check}` on {head} is {status}, not completed")
            }
            Blocker::Deferred {
                selector,
                full_suite_runs_in,
                ..
            } => format!(
                "required check `{check}` on {head} ran only the deferred tier (selector {}{}); \
                 its success is not a full-suite result",
                selector.as_deref().unwrap_or("unknown"),
                full_suite_runs_in
                    .as_deref()
                    .map_or_else(String::new, |runs_in| format!(
                        "; full suite runs in {runs_in}"
                    )),
            ),
            Blocker::MissingRecord {
                run_id,
                run_attempt,
            } => format!(
                "tiered required check `{check}` on {head} has no `{TIER_RECORD_PREFIX}{check}` \
                 record for run {run_id} attempt {run_attempt}, so its tier is unproven"
            ),
            Blocker::UntestedMerge { reason, .. } => format!(
                "the full lane of tiered required check `{check}` on {head} did not test this \
                 pull request's merge with its base ({reason})"
            ),
            Blocker::ChecksWritable { level } => format!(
                "the job of tiered required check `{check}` on {head} ran with `checks: {level}`, \
                 so it could have rewritten its own tier record"
            ),
            Blocker::Unreadable { reason } => {
                format!("the tier of required check `{check}` on {head} cannot be read ({reason})")
            }
        };
        format!("{why}. Fix: {}", self.fix())
    }

    /// What to do next.
    #[must_use]
    pub fn fix(&self) -> String {
        match &self.blocker {
            Blocker::Pending { .. } => format!(
                "wait for `{}` to complete; the steward's native-arm backstop arms the head \
                 once it is green and its tier is full",
                self.check
            ),
            Blocker::Deferred {
                full_lane_workflow, ..
            }
            | Blocker::UntestedMerge {
                full_lane_workflow, ..
            } => self.dispatch(full_lane_workflow.as_deref()),
            Blocker::MissingRecord { .. } => format!(
                "the workflow publishes a `{TIER_RECORD_PREFIX}{}` check run for this run \
                 attempt (docs/ghapp-guards.md)",
                self.check
            ),
            Blocker::ChecksWritable { .. } => format!(
                "give the `{}` job `checks: read` or `checks: none` in its workflow \
                 permissions and rerun it",
                self.check
            ),
            Blocker::Unreadable { .. } => "read it again once GitHub answers".to_owned(),
        }
    }
}

impl DeferredCheck {
    /// The full-lane request: a fleet-App dispatch that tests this pull
    /// request's merge ref at the current head.
    fn dispatch(&self, workflow: Option<&str>) -> String {
        let what = workflow.map_or_else(
            || format!("the workflow of `{}`", self.check),
            |workflow| format!("`{workflow}`"),
        );
        format!(
            "dispatch {what} with `{FULL_LANE_PR_INPUT}={}` as the fleet App and wait for the \
             full lane",
            self.pr
        )
    }
}

/// Every [`DeferredCheck`] joined into one line, or `None` when there is none.
#[must_use]
pub fn explain_all(checks: &[DeferredCheck]) -> Option<String> {
    (!checks.is_empty()).then(|| {
        checks
            .iter()
            .map(DeferredCheck::explain)
            .collect::<Vec<_>>()
            .join("; ")
    })
}

/// The tiered checks of `base` that hold `head` back.
///
/// # Errors
///
/// When the base's config, or the head's check runs once a check is tiered,
/// cannot be read. Callers must not arm on an error.
pub fn deferred_tier_checks(
    run_gh: RunGh<'_>,
    repo: &str,
    pr: u64,
    base: &str,
    head: &str,
) -> Result<Vec<DeferredCheck>, String> {
    let policy = read_tier_policy(run_gh, repo, base)?;
    if policy.checks.is_empty() {
        return Ok(Vec::new());
    }
    tier_blockers(
        run_gh,
        &Landing {
            repo,
            pr,
            base,
            head,
        },
        &policy,
    )
}

/// The [`TierPolicy`] of `base`'s `.shipyard/config.toml`. A missing file or
/// key is no tiered check.
///
/// # Errors
///
/// When the file exists but cannot be read or parsed, or its tier keys are
/// malformed.
pub fn read_tier_policy(run_gh: RunGh<'_>, repo: &str, base: &str) -> Result<TierPolicy, String> {
    let raw = match run_gh(&[
        "api".to_owned(),
        "-H".to_owned(),
        "Accept: application/vnd.github.raw+json".to_owned(),
        format!("repos/{repo}/contents/.shipyard/config.toml?ref={base}"),
    ]) {
        Ok(raw) => raw,
        Err(detail) if detail.contains("404") || detail.contains("Not Found") => {
            return Ok(TierPolicy::default());
        }
        Err(detail) => {
            return Err(format!(
                ".shipyard/config.toml on {base} unreadable: {detail}"
            ));
        }
    };
    tier_policy_from_config(&raw)
        .map_err(|error| format!(".shipyard/config.toml on {base}: {error}"))
}

/// [`read_tier_policy`] over already-read config text.
///
/// # Errors
///
/// When the text does not parse, the check list is not a list of names, or
/// it names checks without a `tier_record_app` other than `github-actions`.
pub fn tier_policy_from_config(text: &str) -> Result<TierPolicy, String> {
    let table = text
        .parse::<toml::Table>()
        .map_err(|error| format!("does not parse: {error}"))?;
    let queue = table.get("queue");
    let Some(value) = queue.and_then(|queue| queue.get("tiered_required_checks")) else {
        return Ok(TierPolicy::default());
    };
    let checks = value
        .as_array()
        .and_then(|names| {
            names
                .iter()
                .map(|name| {
                    name.as_str()
                        .filter(|name| !name.trim().is_empty())
                        .map(str::to_owned)
                })
                .collect::<Option<Vec<_>>>()
        })
        .ok_or_else(|| "`queue.tiered_required_checks` is not a list of check names".to_owned())?;
    if checks.is_empty() {
        return Ok(TierPolicy::default());
    }
    let record_app = queue
        .and_then(|queue| queue.get("tier_record_app"))
        .and_then(toml::Value::as_str)
        .map(str::trim)
        .filter(|app| !app.is_empty())
        .ok_or_else(|| {
            "`queue.tiered_required_checks` is set but `queue.tier_record_app` names no app"
                .to_owned()
        })?;
    if record_app.eq_ignore_ascii_case(ACTIONS_APP) {
        return Err(format!(
            "`queue.tier_record_app` may not be `{ACTIONS_APP}`: pull request jobs act as that \
             app and could rewrite its records"
        ));
    }
    if record_app.eq_ignore_ascii_case(FLEET_APP) {
        return Err(format!(
            "`queue.tier_record_app` may not be `{FLEET_APP}`: every agent posts as the fleet \
             App, so it could write a record"
        ));
    }
    Ok(TierPolicy {
        checks,
        record_app: record_app.to_owned(),
    })
}

/// The blockers of `head` for the named tiered checks.
///
/// # Errors
///
/// When the head's check runs cannot be read in full.
pub fn tier_blockers(
    run_gh: RunGh<'_>,
    landing: &Landing<'_>,
    policy: &TierPolicy,
) -> Result<Vec<DeferredCheck>, String> {
    let listed = check_runs(run_gh, landing.repo, landing.head)?;
    let mut latest = BTreeMap::<&str, &Value>::new();
    for run in &listed {
        let (Some(name), Some(id)) = (
            run.get("name").and_then(Value::as_str),
            run.get("id").and_then(Value::as_u64),
        ) else {
            continue;
        };
        if latest
            .get(name)
            .and_then(|existing| existing.get("id").and_then(Value::as_u64))
            .is_none_or(|existing| id > existing)
        {
            latest.insert(name, run);
        }
    }
    let mut blockers = Vec::new();
    for check in &policy.checks {
        let blocker = match latest.get(check.as_str()) {
            None => Some(Blocker::Pending {
                status: "missing".to_owned(),
            }),
            Some(run) => blocker_for(run_gh, landing, check, run, &listed, &policy.record_app),
        };
        if let Some(blocker) = blocker {
            blockers.push(DeferredCheck {
                check: check.clone(),
                pr: landing.pr,
                head: landing.head.to_owned(),
                blocker,
            });
        }
    }
    Ok(blockers)
}

/// The pull request a landing would land, at the head it would land.
pub struct Landing<'a> {
    /// `OWNER/REPO`.
    pub repo: &'a str,
    /// The pull request number.
    pub pr: u64,
    /// Its base branch.
    pub base: &'a str,
    /// Its current head.
    pub head: &'a str,
}

/// What holds back one tiered check whose latest run is `run`, if anything.
fn blocker_for(
    run_gh: RunGh<'_>,
    landing: &Landing<'_>,
    check: &str,
    run: &Value,
    listed: &[Value],
    record_app: &str,
) -> Option<Blocker> {
    let status = run.get("status").and_then(Value::as_str).unwrap_or("");
    if status != "completed" {
        return Some(Blocker::Pending {
            status: if status.is_empty() { "unknown" } else { status }.to_owned(),
        });
    }
    if app_slug(run) != Some(ACTIONS_APP) {
        return Some(Blocker::Unreadable {
            reason: "it is not a GitHub Actions job, so no tier record can be bound to it"
                .to_owned(),
        });
    }
    let repo = landing.repo;
    let (run_id, run_attempt) = match job_run(run_gh, repo, run) {
        Ok(bound) => bound,
        Err(reason) => return Some(Blocker::Unreadable { reason }),
    };
    let name = format!("{TIER_RECORD_PREFIX}{check}");
    let records: Vec<serde_json::Map<String, Value>> = listed
        .iter()
        .filter(|record| {
            record.get("name").and_then(Value::as_str) == Some(name.as_str())
                // Never a github-actions record, which a pull request job
                // could have PATCHed: only the base's named App counts.
                && app_slug(record) == Some(record_app)
                && record.get("status").and_then(Value::as_str) == Some("completed")
        })
        .filter_map(|record| {
            serde_json::from_str::<Value>(record.pointer("/output/summary")?.as_str()?).ok()
        })
        .filter_map(|summary| match summary {
            Value::Object(object) => Some(object),
            _ => None,
        })
        .filter(|summary| {
            summary.get("schema").and_then(Value::as_str) == Some(TIER_RECORD_SCHEMA)
                && summary.get("check").and_then(Value::as_str) == Some(check)
                && summary.get("run_id").and_then(Value::as_u64) == Some(run_id)
                && summary.get("run_attempt").and_then(Value::as_u64) == Some(run_attempt)
                && summary
                    .get("tier")
                    .and_then(Value::as_str)
                    .is_some_and(|tier| !tier.trim().is_empty())
        })
        .collect();
    if records.is_empty() {
        return Some(Blocker::MissingRecord {
            run_id,
            run_attempt,
        });
    }
    let text = |summary: &serde_json::Map<String, Value>, field: &str| {
        summary
            .get(field)
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(str::to_owned)
    };
    if let Some(summary) = records.iter().find(|summary| {
        summary
            .get("tier")
            .and_then(Value::as_str)
            .is_some_and(|tier| tier.trim().eq_ignore_ascii_case(DEFERRED_TIER))
    }) {
        return Some(Blocker::Deferred {
            selector: text(summary, "selector"),
            full_lane_workflow: text(summary, "full_lane_workflow"),
            full_suite_runs_in: text(summary, "full_suite_runs_in"),
        });
    }
    // Every full-lane record must show it tested this pull request's merge
    // with its base at the current head, never the bare branch.
    for summary in &records {
        if let Err(reason) = tested_merge(run_gh, landing, summary) {
            return Some(Blocker::UntestedMerge {
                reason,
                full_lane_workflow: text(summary, "full_lane_workflow"),
            });
        }
    }
    // Read only when the records would allow: a refusal needs no log.
    let Some(id) = run.get("id").and_then(Value::as_u64) else {
        return Some(Blocker::Unreadable {
            reason: "the check run has no id".to_owned(),
        });
    };
    match run_gh(&[
        "api".to_owned(),
        format!("repos/{repo}/actions/jobs/{id}/logs"),
    ])
    .map_err(|error| format!("the log of job {id} is unreadable: {error}"))
    .and_then(|log| checks_scope(&log).map_err(|error| format!("job {id}: {error}")))
    {
        Ok(level) if level == "read" || level == "none" => None,
        Ok(level) => Some(Blocker::ChecksWritable { level }),
        Err(reason) => Some(Blocker::Unreadable { reason }),
    }
}

/// The `Checks` scope of a job's token, from the [`TOKEN_PERMISSIONS_HEADER`]
/// group of its log: `none` when the group does not list it.
///
/// # Errors
///
/// When the group is missing, never closes, or holds a line that is not
/// `<Scope>: read|write|none`.
pub fn checks_scope(log: &str) -> Result<String, String> {
    // Each log line is `<RFC 3339 timestamp> <text>`.
    let mut lines = log.lines().map(|line| {
        let line = line.trim_end_matches('\r');
        line.split_once(' ')
            .filter(|(stamp, _)| stamp.ends_with('Z') && stamp.contains('T'))
            .map_or(line, |(_, text)| text)
    });
    lines
        .find(|text| text.trim_start_matches("##[group]").trim() == TOKEN_PERMISSIONS_HEADER)
        .ok_or_else(|| format!("its log has no `{TOKEN_PERMISSIONS_HEADER}` group"))?;
    let mut checks = None;
    for text in lines {
        let text = text.trim();
        if text == "##[endgroup]" {
            return Ok(checks.unwrap_or_else(|| "none".to_owned()));
        }
        let (scope, level) = text
            .split_once(':')
            .map(|(scope, level)| (scope.trim(), level.trim().to_ascii_lowercase()))
            .filter(|(scope, level)| {
                !scope.is_empty() && matches!(level.as_str(), "read" | "write" | "none")
            })
            .ok_or_else(|| {
                format!("its `{TOKEN_PERMISSIONS_HEADER}` group has an unparseable line `{text}`")
            })?;
        if scope.eq_ignore_ascii_case("Checks") {
            if checks.is_some() {
                return Err(format!(
                    "its `{TOKEN_PERMISSIONS_HEADER}` group lists `Checks` twice"
                ));
            }
            checks = Some(level);
        }
    }
    Err(format!(
        "its `{TOKEN_PERMISSIONS_HEADER}` group never closes"
    ))
}

/// Whether a full-lane record proves it tested this pull request's merge ref
/// at the current head. All of these must hold: `tested_ref` is
/// `refs/pull/<n>/merge` for this pull request; `tested_sha`'s parents are
/// exactly `[merged_base, head]`, in GitHub's order (base first, head
/// second); and `merged_base` is on the base branch. A bare branch (the head
/// itself), a merge of an older head, the reversed order, another ref, or a
/// record missing any of the three fields is not.
fn tested_merge(
    run_gh: RunGh<'_>,
    landing: &Landing<'_>,
    summary: &serde_json::Map<String, Value>,
) -> Result<(), String> {
    let Landing {
        repo,
        pr,
        base,
        head,
    } = *landing;
    let tested = summary
        .get("tested_sha")
        .and_then(Value::as_str)
        .filter(|sha| is_sha(sha))
        .ok_or_else(|| "its record names no `tested_sha`".to_owned())?;
    if tested.eq_ignore_ascii_case(head) {
        return Err(format!(
            "it tested the bare branch at {}, not a merge with the base",
            short(head)
        ));
    }
    let merge_ref = format!("refs/pull/{pr}/merge");
    match summary.get("tested_ref").and_then(Value::as_str) {
        Some(tested_ref) if tested_ref == merge_ref => {}
        Some(other) => {
            return Err(format!(
                "it tested `{other}`, not this pull request's `{merge_ref}`"
            ));
        }
        None => return Err(format!("its record names no `tested_ref` ({merge_ref})")),
    }
    let merged_base = summary
        .get("merged_base")
        .and_then(Value::as_str)
        .filter(|sha| is_sha(sha))
        .ok_or_else(|| "its record names no `merged_base`".to_owned())?;
    let commit: Value = run_gh(&["api".to_owned(), format!("repos/{repo}/commits/{tested}")])
        .and_then(|raw| serde_json::from_str(&raw).map_err(|error| error.to_string()))
        .map_err(|error| format!("tested commit {} is unreadable: {error}", short(tested)))?;
    let parents: Vec<&str> = commit
        .get("parents")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|parent| parent.get("sha").and_then(Value::as_str))
        .collect();
    let [first, second] = parents[..] else {
        return Err(format!(
            "tested commit {} has {} parent(s), so it is not a merge",
            short(tested),
            parents.len()
        ));
    };
    // GitHub's `refs/pull/<n>/merge` puts the base first and the head second.
    if !second.eq_ignore_ascii_case(head) {
        return Err(if first.eq_ignore_ascii_case(head) {
            format!(
                "tested commit {} has the head as its first parent, the reverse of GitHub's \
                 merge ref, so it is not {merge_ref}",
                short(tested)
            )
        } else {
            format!(
                "tested commit {} is not a merge of the current head {} (its parents are {} \
                 and {}): a stale merge",
                short(tested),
                short(head),
                short(first),
                short(second)
            )
        });
    }
    if !first.eq_ignore_ascii_case(merged_base) {
        return Err(format!(
            "its record names `merged_base` {} but tested commit {} merged {}",
            short(merged_base),
            short(tested),
            short(first)
        ));
    }
    let compare: Value = run_gh(&[
        "api".to_owned(),
        format!("repos/{repo}/compare/{merged_base}...{base}"),
    ])
    .and_then(|raw| serde_json::from_str(&raw).map_err(|error| error.to_string()))
    .map_err(|error| {
        format!(
            "whether {} is on `{base}` is unreadable: {error}",
            short(merged_base)
        )
    })?;
    match compare.get("status").and_then(Value::as_str) {
        Some("identical" | "ahead") => Ok(()),
        _ => Err(format!(
            "tested commit {} merged {}, which is not on `{base}`",
            short(tested),
            short(merged_base)
        )),
    }
}

fn is_sha(text: &str) -> bool {
    text.len() == 40 && text.chars().all(|c| c.is_ascii_hexdigit())
}

/// `(run_id, run_attempt)` of the Actions job behind a check run (a job's id
/// is its check run's id).
fn job_run(run_gh: RunGh<'_>, repo: &str, run: &Value) -> Result<(u64, u64), String> {
    let id = run
        .get("id")
        .and_then(Value::as_u64)
        .ok_or_else(|| "the check run has no id".to_owned())?;
    let raw = run_gh(&["api".to_owned(), format!("repos/{repo}/actions/jobs/{id}")])
        .map_err(|error| format!("job {id} unreadable: {error}"))?;
    let job: Value =
        serde_json::from_str(&raw).map_err(|error| format!("job {id} was not JSON: {error}"))?;
    match (
        job.get("run_id").and_then(Value::as_u64),
        job.get("run_attempt").and_then(Value::as_u64),
    ) {
        (Some(run_id), Some(run_attempt)) => Ok((run_id, run_attempt)),
        _ => Err(format!("job {id} names no run_id and run_attempt")),
    }
}

/// Every check run on `head`, across every page.
fn check_runs(run_gh: RunGh<'_>, repo: &str, head: &str) -> Result<Vec<Value>, String> {
    let path = format!("repos/{repo}/commits/{head}/check-runs?per_page=100");
    let raw = run_gh(&[
        "api".to_owned(),
        "--paginate".to_owned(),
        "--slurp".to_owned(),
        path.clone(),
    ])
    .map_err(|error| format!("the check runs of {} are unreadable: {error}", short(head)))?;
    let pages = match serde_json::from_str::<Value>(&raw)
        .map_err(|error| format!("{path} was not JSON: {error}"))?
    {
        Value::Array(pages) => pages,
        other => vec![other],
    };
    let mut listed = Vec::new();
    let mut total = None;
    for page in pages {
        let Some(Value::Array(runs)) = page.get("check_runs").cloned() else {
            return Err(format!("the check runs of {} are unreadable", short(head)));
        };
        total = page.get("total_count").and_then(Value::as_u64);
        listed.extend(runs);
    }
    if total != Some(listed.len() as u64) {
        return Err(format!("the check runs of {} are partial", short(head)));
    }
    Ok(listed)
}

fn app_slug(run: &Value) -> Option<&str> {
    run.pointer("/app/slug").and_then(Value::as_str)
}

fn short(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

#[cfg(test)]
mod tests;
