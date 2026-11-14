//! One positive control per invariant, each reproducing the incident it was
//! written for, plus a clean fleet repository that passes every check.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use chrono::{DateTime, Utc};

use super::clones::{self, CloneArgs, RemoteForm};
use super::{
    ADMISSION, Facts, Invariant, NEVER_RAN, PRODUCER, VERDICTS, evaluate, record, summary_token,
};
use crate::app::merge_steward_cmd::PrReport;
use crate::app::merge_steward_cmd::blocked_on::Stall;
use crate::merge_steward::{RequiredCheck, StewardDecision};

const ACTIONS: u64 = 15368;
const NOW: &str = "2026-11-12T12:00:00Z";

/// What every healthy fleet base carries.
const FLEET_CONFIG: &str = "[auto_merge]\n\
reviewer_signers = [\"pulp-review\", \"infra-review2\"]\n\
reviewer_stamp_sessions = [\"fleet-pass-reviewer-*\"]\n";

const REVIEW_VERDICT_WORKFLOW: &str = "name: review-verdict\non:\n  pull_request:\njobs:\n  \
review-verdict:\n    runs-on: ubuntu-latest\n    steps:\n      - run: shipyard review-verdict\n";
const TEST_WORKFLOW: &str =
    "name: test\non: [pull_request]\njobs:\n  test:\n    runs-on: ubuntu-latest\n";

fn now() -> DateTime<Utc> {
    NOW.parse().expect("now")
}

fn required(context: &str) -> RequiredCheck {
    RequiredCheck {
        context: context.to_owned(),
        app_id: Some(ACTIONS),
    }
}

fn row(number: u64, blocked_on: &str, since: &str, error: Option<&str>) -> PrReport {
    PrReport {
        number,
        head_sha: "a".repeat(40),
        decision: StewardDecision::Unmanaged,
        mutation: None,
        error: error.map(str::to_owned),
        blocked: Stall {
            blocked_on: blocked_on.to_owned(),
            since: Some(since.to_owned()),
            ..Stall::default()
        },
    }
}

#[allow(clippy::unnecessary_wraps)] // Matches `Facts::base_config`.
fn config(text: &str) -> Result<Option<toml::Table>, String> {
    Ok(Some(text.parse().expect("config")))
}

struct Fixture {
    repo: &'static str,
    merge_path: &'static str,
    required: Vec<RequiredCheck>,
    head_checks: BTreeSet<String>,
    rows: Vec<PrReport>,
    base_config: Result<Option<toml::Table>, String>,
    workflows: Result<Vec<String>, String>,
}

impl Fixture {
    /// A healthy queue-less fleet repository: app-bound `review-verdict` and
    /// `test`, both with a workflow, the fleet's `[auto_merge]` table, one
    /// approved head admitted this pass.
    fn clean() -> Self {
        Self {
            repo: "danielraffel/tartci",
            merge_path: "server_enforced_exact_head_merge",
            required: vec![required("review-verdict"), required("test")],
            head_checks: ["review-verdict", "test"].map(str::to_owned).into(),
            rows: vec![
                row(1, "admitted", "2026-11-12T11:00:00Z", None),
                row(2, "no_verdict", "2026-11-12T09:00:00Z", None),
            ],
            base_config: config(FLEET_CONFIG),
            workflows: Ok(vec![
                REVIEW_VERDICT_WORKFLOW.to_owned(),
                TEST_WORKFLOW.to_owned(),
            ]),
        }
    }

    fn evaluate(&self) -> Vec<Invariant> {
        evaluate(&Facts {
            repo: self.repo,
            base: "main",
            merge_path: self.merge_path,
            approval_context: "review-verdict",
            required: &self.required,
            head_checks: self.head_checks.clone(),
            rows: &self.rows,
            base_config: &self.base_config,
            workflows: &self.workflows,
            now: now(),
        })
    }
}

fn find<'a>(invariants: &'a [Invariant], id: &str) -> &'a Invariant {
    invariants
        .iter()
        .find(|invariant| invariant.id == id)
        .unwrap_or_else(|| panic!("no {id} in {invariants:?}"))
}

#[test]
fn a_clean_fleet_repository_passes_every_invariant() {
    let invariants = Fixture::clean().evaluate();
    assert_eq!(invariants.len(), 4);
    for invariant in &invariants {
        assert!(invariant.ok && !invariant.skipped, "{invariant:?}");
        assert!(invariant.fix.is_none(), "{invariant:?}");
    }
    assert_eq!(summary_token(&invariants), "ok");
}

/// I1: the watchdog repository required `review-verdict` with no workflow
/// on main producing it, so every pull request waited 10+ hours on
/// `required_never_ran:review-verdict`.
fn watchdog_without_a_verdict_workflow() -> Fixture {
    Fixture {
        repo: "danielraffel/cmux-capacity-watchdog",
        head_checks: ["test"].map(str::to_owned).into(),
        rows: vec![
            row(
                8,
                "required_never_ran:review-verdict,test",
                "2026-11-11T18:46:51Z",
                None,
            ),
            row(
                7,
                "required_never_ran:review-verdict",
                "2026-11-11T20:11:53Z",
                None,
            ),
        ],
        workflows: Ok(vec![TEST_WORKFLOW.to_owned()]),
        ..Fixture::clean()
    }
}

#[test]
fn a_required_context_no_workflow_produces_is_an_incident() {
    let invariants = watchdog_without_a_verdict_workflow().evaluate();
    let producer = find(&invariants, PRODUCER);
    assert!(producer.incident(), "{producer:?}");
    assert!(producer.detail.contains("`review-verdict`"), "{producer:?}");
    assert!(
        !producer.detail.contains("`test`"),
        "test has a producer: {producer:?}"
    );
    let fix = producer.fix.as_deref().expect("fix");
    assert!(fix.contains("add a workflow on main"), "{fix}");
    assert!(fix.contains("external_producers"), "{fix}");
}

#[test]
fn a_producer_is_a_workflow_a_head_check_an_external_app_or_a_declaration() {
    // Workflow named only through a matrix job: `build (macos)` from `build (`.
    let mut fixture = Fixture::clean();
    fixture.required = vec![required("build (macos)")];
    fixture.head_checks = BTreeSet::new();
    fixture.workflows = Ok(vec![
        "jobs:\n  b:\n    name: build (${{ matrix.os }})\n".to_owned(),
    ]);
    assert!(find(&fixture.evaluate(), PRODUCER).ok);
    // A check of that name on an open head.
    let mut fixture = watchdog_without_a_verdict_workflow();
    fixture.head_checks.insert("review-verdict".to_owned());
    assert!(find(&fixture.evaluate(), PRODUCER).ok);
    // Bound to an app other than GitHub Actions.
    let mut fixture = watchdog_without_a_verdict_workflow();
    fixture.required[0].app_id = Some(99);
    assert!(find(&fixture.evaluate(), PRODUCER).ok);
    // Declared external in the base config.
    let mut fixture = watchdog_without_a_verdict_workflow();
    fixture.base_config = config(&format!(
        "{FLEET_CONFIG}[merge_steward.invariants]\nexternal_producers = [\"review-verdict\"]\n"
    ));
    assert!(find(&fixture.evaluate(), PRODUCER).ok);
    // Only a comment or a longer word names it: still no producer.
    let mut fixture = watchdog_without_a_verdict_workflow();
    fixture.workflows = Ok(vec![
        "# review-verdict comes later\njobs:\n  review-verdicts:\n".to_owned(),
    ]);
    assert!(find(&fixture.evaluate(), PRODUCER).incident());
}

#[test]
fn unreadable_workflows_skip_the_producer_check_instead_of_failing_it() {
    let mut fixture = watchdog_without_a_verdict_workflow();
    fixture.workflows = Err("listing failed".to_owned());
    let producer = find(&fixture.evaluate(), PRODUCER).clone();
    assert!(producer.ok && producer.skipped, "{producer:?}");
}

/// fleet-ssh #18 and #19: the base required only `check`, had no merge
/// queue and no `.shipyard/config.toml`, so the steward refused direct
/// merge of every head and reported nothing else.
#[test]
fn a_base_with_no_automatic_admission_is_an_incident() {
    let fixture = Fixture {
        repo: "danielraffel/fleet-ssh",
        merge_path: "direct_merge_refused_server_enforcement_required",
        required: vec![required("check")],
        head_checks: ["check"].map(str::to_owned).into(),
        rows: vec![
            row(19, "direct_merge_refused", "2026-11-12T07:14:36Z", None),
            row(18, "direct_merge_refused", "2026-11-12T08:05:13Z", None),
        ],
        base_config: Ok(None),
        workflows: Ok(vec!["jobs:\n  check:\n".to_owned()]),
    };
    let invariants = fixture.evaluate();
    let admission = find(&invariants, ADMISSION);
    assert!(admission.incident(), "{admission:?}");
    assert!(
        admission.detail.contains("refuses direct merge of #19,#18"),
        "{admission:?}"
    );
    assert!(
        admission.detail.contains("reviewer_signers"),
        "{admission:?}"
    );
    let fix = admission.fix.as_deref().expect("fix");
    assert!(fix.contains("require the `review-verdict` check"), "{fix}");
    assert!(fix.contains("reviewer_stamp_sessions"), "{fix}");
    assert_eq!(summary_token(&invariants), format!("FAIL:{ADMISSION}"));
}

/// The base needs exact-head approval but its config lists no signers or
/// stamp sessions.
#[test]
fn approval_without_signers_or_stamp_sessions_is_an_incident() {
    let mut fixture = Fixture::clean();
    fixture.base_config = config("[auto_merge]\nreviewer_signers = [\"pulp-review\"]\n");
    let admission = find(&fixture.evaluate(), ADMISSION).clone();
    assert!(admission.incident(), "{admission:?}");
    assert!(
        admission.detail.contains("reviewer_stamp_sessions"),
        "{admission:?}"
    );
    assert!(
        !admission.detail.contains("reviewer_signers and"),
        "{admission:?}"
    );
}

/// I2: the watchdog required app-bound `review-verdict`, but it was not in
/// `REVIEW_REQUIRED_REPOS` and its config did not set
/// `arm_requires_head_approval`, so approved green heads read `ready` and
/// the steward wrote "head approval is not required here, so nothing was
/// merged" on every pass.
#[test]
fn an_approval_rule_that_does_not_apply_is_an_incident() {
    let mut fixture = Fixture::clean();
    fixture.repo = "danielraffel/not-in-the-list";
    fixture.rows = vec![row(
        8,
        "ready",
        "2026-11-12T06:29:27Z",
        Some(
            "head fcae5007e045 needs an exact-head approval: this base merges only approved \
             heads, and head approval is not required here, so nothing was merged",
        ),
    )];
    let admission = find(&fixture.evaluate(), ADMISSION).clone();
    assert!(admission.incident(), "{admission:?}");
    assert!(
        admission.detail.contains("REVIEW_REQUIRED_REPOS"),
        "{admission:?}"
    );
    assert!(
        admission.detail.contains("#8 wait approved"),
        "{admission:?}"
    );
    assert!(
        admission
            .fix
            .as_deref()
            .expect("fix")
            .contains("fleet_review_required = true"),
        "{admission:?}"
    );
    // The base config saying so is enough.
    fixture.base_config = config(&format!(
        "[auto_merge]\narm_requires_head_approval = true\n{}",
        FLEET_CONFIG.trim_start_matches("[auto_merge]\n")
    ));
    fixture.rows.clear();
    assert!(find(&fixture.evaluate(), ADMISSION).ok);
}

/// A base that sets `fleet_review_required = true` has the semantics of
/// `REVIEW_REQUIRED_REPOS` membership, so the approval rule applies to a repo
/// off the compiled list, even beside an explicit
/// `arm_requires_head_approval = false`. The same fixture without the switch is
/// still an incident.
#[test]
fn the_base_fleet_review_switch_makes_the_approval_rule_apply() {
    let mut fixture = Fixture::clean();
    fixture.repo = "danielraffel/not-in-the-list";
    fixture.base_config = config(FLEET_CONFIG);
    assert!(
        find(&fixture.evaluate(), ADMISSION).incident(),
        "control: without the switch the rule does not apply"
    );
    for strict in ["", "arm_requires_head_approval = false\n"] {
        fixture.base_config = config(&format!(
            "[auto_merge]\n{strict}fleet_review_required = true\n{}",
            FLEET_CONFIG.trim_start_matches("[auto_merge]\n")
        ));
        let admission = find(&fixture.evaluate(), ADMISSION).clone();
        assert!(admission.ok, "{strict:?}: {admission:?}");
    }
}

/// Before the ruleset requires `review-verdict`, the base switch alone still
/// means approval is needed, so a missing signer list is reported. The same
/// base without the switch needs no signers.
#[test]
fn the_base_fleet_review_switch_needs_signers_before_the_ruleset_requires_review() {
    let mut fixture = Fixture::clean();
    fixture.repo = "danielraffel/not-in-the-list";
    fixture.required = vec![required("test")];
    fixture.head_checks = ["test"].map(str::to_owned).into();
    fixture.base_config = config("[auto_merge]\nqueue = true\n");
    assert!(
        find(&fixture.evaluate(), ADMISSION).ok,
        "control: without the switch no approval is needed"
    );
    fixture.base_config = config("[auto_merge]\nfleet_review_required = true\n");
    let admission = find(&fixture.evaluate(), ADMISSION).clone();
    assert!(admission.incident(), "{admission:?}");
    assert!(
        admission
            .detail
            .contains("reviewer_signers and reviewer_stamp_sessions"),
        "{admission:?}"
    );
}

/// `required_never_ran` for over an hour on a context no open head carries
/// (the watchdog's #8 and #7 waited from 18:46Z and 20:11Z).
#[test]
fn a_required_context_that_never_runs_for_an_hour_is_an_incident() {
    let mut fixture = watchdog_without_a_verdict_workflow();
    // Give it a producer, so only the never-ran check speaks.
    fixture.workflows = Ok(vec![
        REVIEW_VERDICT_WORKFLOW.to_owned(),
        TEST_WORKFLOW.to_owned(),
    ]);
    let invariants = fixture.evaluate();
    assert!(find(&invariants, PRODUCER).ok);
    let never = find(&invariants, NEVER_RAN);
    assert!(never.incident(), "{never:?}");
    assert!(
        never
            .detail
            .contains("`review-verdict` has run on no open head"),
        "{never:?}"
    );
    assert!(never.detail.contains("oldest #8 for 1033 min"), "{never:?}");
    assert!(
        !never.detail.contains("`test`"),
        "test runs on some head: {never:?}"
    );
    // Under the limit it holds.
    fixture.rows = vec![row(
        9,
        "required_never_ran:review-verdict",
        "2026-11-12T11:30:00Z",
        None,
    )];
    assert!(find(&fixture.evaluate(), NEVER_RAN).ok);
}

/// 2026-11-12 03:42Z: a pass-runner install posted every verdict
/// comment-only, so each APPROVE was excluded and none counted for hours.
#[test]
fn approvals_excluded_for_an_hour_with_none_counted_are_an_incident() {
    let mut fixture = Fixture::clean();
    fixture.rows = vec![
        row(
            11,
            "approval_excluded:no_stamp",
            "2026-11-12T10:30:00Z",
            None,
        ),
        row(
            12,
            "approval_excluded:unattributed",
            "2026-11-12T10:45:00Z",
            None,
        ),
        // A moved head is not a counting failure.
        row(
            13,
            "approval_excluded:stale_head",
            "2026-11-12T08:00:00Z",
            None,
        ),
        row(14, "no_verdict", "2026-11-12T08:00:00Z", None),
    ];
    let verdicts = find(&fixture.evaluate(), VERDICTS).clone();
    assert!(verdicts.incident(), "{verdicts:?}");
    assert!(
        verdicts.detail.contains("#11 no_stamp 90 min"),
        "{verdicts:?}"
    );
    assert!(!verdicts.detail.contains("#13"), "{verdicts:?}");
    assert!(
        verdicts
            .fix
            .as_deref()
            .expect("fix")
            .contains("no_stamp, unattributed"),
        "{verdicts:?}"
    );
    // One head counted this pass means counting works.
    fixture
        .rows
        .push(row(15, "ready", "2026-11-12T11:50:00Z", None));
    assert!(find(&fixture.evaluate(), VERDICTS).ok);
}

#[test]
fn a_repeated_incident_keeps_its_first_failing_time_and_is_cleared_on_recovery() {
    let temp = tempfile::tempdir().expect("temp");
    let state = temp.path().join("merge-steward.invariants.json");
    let mut first = watchdog_without_a_verdict_workflow().evaluate();
    record(&state, "o/r", &mut first, None, now());
    let since = find(&first, PRODUCER).since.clone().expect("since");
    let later = now() + chrono::Duration::minutes(10);
    let mut second = watchdog_without_a_verdict_workflow().evaluate();
    record(&state, "o/r", &mut second, None, later);
    assert_eq!(
        find(&second, PRODUCER).since.as_deref(),
        Some(since.as_str())
    );
    let mut clean = Fixture::clean().evaluate();
    record(&state, "o/r", &mut clean, None, later);
    let mut again = watchdog_without_a_verdict_workflow().evaluate();
    record(&state, "o/r", &mut again, None, later);
    assert_eq!(
        find(&again, PRODUCER).since.as_deref(),
        Some(
            later
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
                .as_str()
        )
    );
}

/// A fake `gh` that records its argv, answers `issue list` from a file and
/// prints a URL for `issue create`.
#[cfg(unix)]
fn followup_gh(temp: &Path, listed: &str) -> crate::cloud::GitHubActions {
    let log = temp.join("gh.log");
    std::fs::write(temp.join("listed.json"), listed).expect("listed");
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{log}'\ncase \"$*\" in\n  \
         *\"issue list\"*) cat '{listed}' ;;\n  *\"issue create\"*) echo https://github.com/o/h/issues/9 ;;\n  \
         *) : ;;\nesac\n",
        log = log.display(),
        listed = temp.join("listed.json").display(),
    );
    let path = temp.join("gh");
    crate::test_support::write_executable_script(&path, &script);
    let config = crate::config::LoadedConfig {
        data: toml::Table::new(),
        global_dir: temp.join("global"),
        project_dir: None,
        local_dir: None,
        local_overlay_source: crate::config::LocalOverlaySource::None,
    };
    crate::cloud::GitHubActions::from_loaded_config(temp, &config).with_gh_binary_for_tests(path)
}

#[cfg(unix)]
#[test]
fn a_followup_is_filed_once_per_finding_and_comments_on_an_open_issue() {
    let temp = tempfile::tempdir().expect("temp");
    let state = temp.path().join("merge-steward.invariants.json");
    let actions = followup_gh(temp.path(), "[]");
    let followup = super::Followup {
        actions: &actions,
        repo: "o/handbook",
    };
    let mut first = watchdog_without_a_verdict_workflow().evaluate();
    record(&state, "o/r", &mut first, Some(&followup), now());
    assert_eq!(
        find(&first, PRODUCER).followup.as_deref(),
        Some("opened https://github.com/o/h/issues/9")
    );
    let mut repeat = watchdog_without_a_verdict_workflow().evaluate();
    record(&state, "o/r", &mut repeat, Some(&followup), now());
    assert_eq!(
        find(&repeat, PRODUCER).followup,
        None,
        "a repeat is not refiled"
    );
    let log = std::fs::read_to_string(temp.path().join("gh.log")).expect("log");
    assert_eq!(log.matches("issue create").count(), 2, "{log}"); // producer + never-ran
    assert!(log.contains("fleet-followup"), "{log}");

    // A changed finding on a repository whose issue is already open: comment.
    let title = super::followup_title("o/r", PRODUCER);
    std::fs::write(
        temp.path().join("listed.json"),
        format!("[{{\"number\":5,\"title\":\"{title}\"}}]"),
    )
    .expect("listed");
    let mut changed = watchdog_without_a_verdict_workflow();
    changed.required.push(required("lint"));
    let mut changed = changed.evaluate();
    record(&state, "o/r", &mut changed, Some(&followup), now());
    assert_eq!(
        find(&changed, PRODUCER).followup.as_deref(),
        Some("commented on o/handbook#5")
    );
}

// --- fleet-level config clones ----------------------------------------------

fn run_git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A bare "remote" at `<remotes>/<repo>.git` whose main carries
/// `.shipyard/config.toml` (unless `config` is `None`) and a workflow.
fn remote(remotes: &Path, repo: &str, config: Option<&str>) {
    let work = remotes.join("work").join(repo);
    std::fs::create_dir_all(&work).expect("work");
    run_git(&work, &["init", "-q", "-b", "main"]);
    if let Some(config) = config {
        std::fs::create_dir_all(work.join(".shipyard")).expect("shipyard");
        std::fs::write(work.join(".shipyard/config.toml"), config).expect("config");
    }
    std::fs::create_dir_all(work.join(".github/workflows")).expect("workflows");
    std::fs::write(work.join(".github/workflows/test.yml"), TEST_WORKFLOW).expect("workflow");
    std::fs::create_dir_all(work.join("src")).expect("src");
    std::fs::write(work.join("src/lib.rs"), "x").expect("source");
    run_git(&work, &["add", "-A"]);
    run_git(&work, &["commit", "-q", "-m", "init"]);
    let bare = remotes.join(format!("{repo}.git"));
    std::fs::create_dir_all(bare.parent().expect("parent")).expect("bare parent");
    run_git(
        remotes,
        &[
            "clone",
            "-q",
            "--bare",
            work.to_str().expect("utf8"),
            bare.to_str().expect("utf8"),
        ],
    );
}

/// Fleet layout: `o/a` cloned sparse to `.shipyard` only, `o/b` listed with
/// no clone (the fleet-ssh incident), and its remote reachable in the same
/// form as `o/a`'s.
fn fleet(temp: &Path) -> CloneArgs {
    let remotes = temp.join("remotes");
    remote(&remotes, "o/a", Some(FLEET_CONFIG));
    remote(&remotes, "o/b", Some(FLEET_CONFIG));
    let clones = temp.join("clones");
    std::fs::create_dir_all(&clones).expect("clones");
    let url = format!("file://{}/o/a.git", remotes.display());
    run_git(
        &clones,
        &["clone", "-q", "--filter=blob:none", "--sparse", &url, "a"],
    );
    run_git(&clones.join("a"), &["sparse-checkout", "set", ".shipyard"]);
    let list = temp.join("fleet-repos.txt");
    std::fs::write(&list, "# fleet\no/a\n\no/b\n").expect("list");
    CloneArgs {
        fleet_repos: list,
        clones,
        heal: false,
    }
}

#[test]
fn the_remote_form_follows_the_existing_clones() {
    let ssh = RemoteForm::of(
        "git@github.com:danielraffel/tartci.git",
        "danielraffel/tartci",
    )
    .expect("ssh");
    assert_eq!(
        ssh.url("danielraffel/fleet-ssh"),
        "git@github.com:danielraffel/fleet-ssh.git"
    );
    let https = RemoteForm::of(
        "https://github.com/Generous-Corp/pulp",
        "generous-corp/pulp",
    )
    .expect("https");
    assert_eq!(https.url("o/x"), "https://github.com/o/x");
    assert_eq!(
        RemoteForm::of("git@github.com:other/tartci.git", "danielraffel/tartci"),
        None
    );
    assert_eq!(
        RemoteForm::of(
            "git@github.com:xdanielraffel/tartci.git",
            "danielraffel/tartci"
        ),
        None
    );
}

#[test]
fn a_fleet_repository_without_a_config_clone_is_an_incident_without_heal() {
    let temp = tempfile::tempdir().expect("temp");
    let args = fleet(temp.path());
    let results = clones::check(&args).expect("check");
    assert_eq!(results.len(), 2);
    assert!(results[0].invariant.ok, "{:?}", results[0]);
    let missing = &results[1].invariant;
    assert!(missing.incident(), "{missing:?}");
    assert!(
        missing.detail.contains("no steward config clone"),
        "{missing:?}"
    );
    assert!(
        missing.fix.as_deref().expect("fix").contains("/o/b.git"),
        "the fix names the clone url in the existing form: {missing:?}"
    );
    assert_eq!(clones::summary_token(&results), "FAIL:config_clone@o/b");
    assert!(!args.clones.join("b").exists(), "no heal without --heal");
}

#[test]
fn heal_clones_the_missing_repository_and_widens_sparse_checkouts_idempotently() {
    let temp = tempfile::tempdir().expect("temp");
    let mut args = fleet(temp.path());
    args.heal = true;
    let results = clones::check(&args).expect("check");
    assert_eq!(clones::summary_token(&results), "ok", "{results:?}");
    let healed_b = &results[1].invariant.healed;
    assert_eq!(healed_b.len(), 1, "{healed_b:?}");
    assert!(healed_b[0].starts_with("cloned file://"), "{healed_b:?}");
    let b = args.clones.join("b");
    assert!(b.join(".shipyard/config.toml").is_file());
    assert!(b.join(".github/workflows/test.yml").is_file());
    assert!(!b.join("src").exists(), "sparse: config and workflows only");
    let healed_a = &results[0].invariant.healed;
    assert!(
        healed_a.len() == 1 && healed_a[0].contains("added .github/workflows"),
        "{healed_a:?}"
    );
    assert!(args.clones.join("a/.github/workflows/test.yml").is_file());
    // A second run has nothing left to do.
    let again = clones::check(&args).expect("check");
    assert_eq!(clones::summary_token(&again), "ok");
    assert!(
        again
            .iter()
            .all(|result| result.invariant.healed.is_empty()),
        "{again:?}"
    );
}

#[test]
fn a_clone_without_a_shipyard_config_is_an_incident_heal_cannot_fix() {
    let temp = tempfile::tempdir().expect("temp");
    let mut args = fleet(temp.path());
    remote(&temp.path().join("remotes"), "o/c", None);
    std::fs::write(&args.fleet_repos, "o/a\no/c\n").expect("list");
    args.heal = true;
    let results = clones::check(&args).expect("check");
    let c = &results[1].invariant;
    assert!(c.incident(), "{c:?}");
    assert_eq!(c.healed.len(), 1, "it was cloned: {c:?}");
    assert!(
        c.detail.contains(".shipyard/config.toml is unreadable"),
        "{c:?}"
    );
}

// --- the pass's side: reads, caching, the JSON shape ---------------------------

#[test]
fn an_invariant_serializes_as_id_ok_detail_fix() {
    let value = serde_json::to_value(Invariant::fails(PRODUCER, "d", "f")).expect("json");
    assert_eq!(
        value,
        serde_json::json!({"id": PRODUCER, "ok": false, "detail": "d", "fix": "f"})
    );
    let value = serde_json::to_value(Invariant::holds(PRODUCER, "d")).expect("json");
    assert_eq!(
        value,
        serde_json::json!({"id": PRODUCER, "ok": true, "detail": "d"})
    );
}

#[test]
fn followup_issues_are_off_unless_the_machine_global_config_turns_them_on() {
    use super::Settings;
    let temp = tempfile::tempdir().expect("temp");
    let off = Settings::from_machine_global(temp.path(), None).expect("no config");
    assert_eq!(off.followup_repo, None);

    std::fs::write(
        temp.path().join("config.toml"),
        "[merge_steward.invariants]\nfollowup_repo = \"acme/handbook\"\n",
    )
    .expect("write");
    let repo_only = Settings::from_machine_global(temp.path(), None).expect("repo only");
    assert_eq!(repo_only.followup_repo, None, "a repo alone does not file");

    std::fs::write(
        temp.path().join("config.toml"),
        "[merge_steward.invariants]\nfollowup_issues = true\n",
    )
    .expect("write");
    let on = Settings::from_machine_global(temp.path(), None).expect("on");
    assert_eq!(
        on.followup_repo.as_deref(),
        Some("danielraffel/fleet-handbook")
    );

    std::fs::write(
        temp.path().join("config.toml"),
        "[merge_steward.invariants]\nfollowup_issues = \"yes\"\n",
    )
    .expect("write");
    assert!(Settings::from_machine_global(temp.path(), None).is_err());
}

/// The watchdog's observation: `review-verdict` and `test` required, two
/// heads waiting on `review-verdict`, one of them already running `test`.
fn watchdog_observation() -> crate::app::merge_steward_cmd::RepoObservation {
    use crate::merge_steward::{StewardCheck, StewardCheckSource, StewardPullRequest};
    let pr = |number: u64, checks: Vec<&str>| crate::app::merge_steward_cmd::ObservedPr {
        node_id: format!("PR_{number}"),
        fact: StewardPullRequest {
            number,
            head_sha: format!("{number:040}"),
            head_branch: format!("b{number}"),
            author: None,
            draft: false,
            merge_state: "BLOCKED".to_owned(),
            auto_merge_active: false,
            queue_position: None,
            labels: Vec::new(),
            checks: checks
                .into_iter()
                .map(|name| StewardCheck {
                    name: name.to_owned(),
                    source: StewardCheckSource::CheckRun,
                    app_id: Some(ACTIONS),
                    status: "COMPLETED".to_owned(),
                    conclusion: Some("SUCCESS".to_owned()),
                    run_id: None,
                    observed_at: None,
                    check_id: None,
                    description: None,
                })
                .collect(),
        },
        owner: None,
        check_rollup_maybe_truncated: false,
    };
    crate::app::merge_steward_cmd::RepoObservation {
        repo: "danielraffel/cmux-capacity-watchdog".to_owned(),
        base: "main".to_owned(),
        allow_auto_merge: true,
        merge_queue: false,
        required_checks: vec![required("review-verdict"), required("test")],
        prs: vec![pr(8, vec![]), pr(7, vec!["test"])],
        runs: Vec::new(),
        merge_group_heads: std::collections::BTreeMap::new(),
        merge_group_enqueued_at: std::collections::BTreeMap::new(),
        capacity_preemption_policy: crate::merge_steward::CapacityPreemptionPolicy::disabled(),
        preemption_error: None,
    }
}

#[cfg(unix)]
fn api_gh(temp: &Path) -> crate::cloud::GitHubActions {
    let log = temp.join("api.log");
    let sha = "b".repeat(40);
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{log}'\ncase \"$*\" in\n  \
         *contents/.shipyard/config.toml*) printf '%s' '{config}' ;;\n  \
         *contents/.github/workflows*) printf 'test.yml\\t{sha}\\nREADME.md\\t{sha}\\n' ;;\n  \
         *git/blobs/{sha}*) printf '%s' 'jobs:\n  test:\n' ;;\n  \
         *) echo unexpected >&2; exit 1 ;;\nesac\n",
        log = log.display(),
        config = FLEET_CONFIG,
    );
    let path = temp.join("gh");
    crate::test_support::write_executable_script(&path, &script);
    let config = crate::config::LoadedConfig {
        data: toml::Table::new(),
        global_dir: temp.join("global"),
        project_dir: None,
        local_dir: None,
        local_overlay_source: crate::config::LocalOverlaySource::None,
    };
    crate::cloud::GitHubActions::from_loaded_config(temp, &config).with_gh_binary_for_tests(path)
}

#[cfg(unix)]
#[test]
fn a_pass_reads_workflows_through_the_api_once_and_caches_blobs_by_sha() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = api_gh(temp.path());
    let observation = watchdog_observation();
    let rows = vec![
        row(
            8,
            "required_never_ran:review-verdict,test",
            "2026-11-11T18:46:51Z",
            None,
        ),
        row(
            7,
            "required_never_ran:review-verdict",
            "2026-11-11T20:11:53Z",
            None,
        ),
    ];
    let state = temp.path().join("state/merge-steward.invariants.json");
    let run = || {
        super::run(
            &actions,
            &super::Settings::default(),
            &observation,
            &rows,
            "server_enforced_exact_head_merge",
            "review-verdict",
            &state,
            true,
            now(),
        )
    };
    let first = run();
    assert!(find(&first, PRODUCER).incident(), "{first:?}");
    assert!(find(&first, PRODUCER).detail.contains("`review-verdict`"));
    assert!(find(&first, NEVER_RAN).incident(), "{first:?}");
    assert!(find(&first, ADMISSION).ok, "{first:?}");
    let second = run();
    assert_eq!(find(&second, PRODUCER).since, find(&first, PRODUCER).since);
    let log = std::fs::read_to_string(temp.path().join("api.log")).expect("log");
    assert_eq!(
        log.matches("git/blobs/").count(),
        1,
        "blob cached by sha: {log}"
    );
    assert_eq!(
        log.matches("contents/.github/workflows").count(),
        2,
        "{log}"
    );
}

#[cfg(unix)]
#[test]
fn a_pass_in_a_checkout_of_the_base_reads_workflows_from_disk() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = api_gh(temp.path());
    // The checkout's origin names the repository; main carries a
    // review-verdict workflow the API fixture does not.
    let checkout = temp.path().join("checkout");
    std::fs::create_dir_all(checkout.join(".github/workflows")).expect("dirs");
    run_git(&checkout, &["init", "-q", "-b", "main"]);
    run_git(
        &checkout,
        &[
            "remote",
            "add",
            "origin",
            "git@github.com:danielraffel/cmux-capacity-watchdog.git",
        ],
    );
    std::fs::write(
        checkout.join(".github/workflows/rv.yml"),
        REVIEW_VERDICT_WORKFLOW,
    )
    .expect("workflow");
    std::fs::write(checkout.join(".github/workflows/test.yml"), TEST_WORKFLOW).expect("workflow");
    run_git(&checkout, &["add", "-A"]);
    run_git(&checkout, &["commit", "-q", "-m", "init"]);
    let invariants = super::run(
        &actions,
        &super::Settings {
            checkout: Some(checkout.clone()),
            followup_repo: None,
        },
        &watchdog_observation(),
        &[],
        "server_enforced_exact_head_merge",
        "review-verdict",
        &temp.path().join("state.json"),
        false,
        now(),
    );
    assert!(find(&invariants, PRODUCER).ok, "{invariants:?}");
    let log = std::fs::read_to_string(temp.path().join("api.log")).expect("log");
    assert!(!log.contains("workflows"), "no workflow API read: {log}");
    // A checkout of another repository is not trusted.
    run_git(
        &checkout,
        &["remote", "set-url", "origin", "git@github.com:o/other.git"],
    );
    let invariants = super::run(
        &actions,
        &super::Settings {
            checkout: Some(checkout),
            followup_repo: None,
        },
        &watchdog_observation(),
        &[],
        "server_enforced_exact_head_merge",
        "review-verdict",
        &temp.path().join("state.json"),
        false,
        now(),
    );
    assert!(find(&invariants, PRODUCER).incident(), "{invariants:?}");
}
