use std::cell::{Cell, RefCell};

use chrono::TimeZone;
use serde_json::json;

use super::*;

/// The claim from Generous-Corp/vellum#462, verbatim.
const INCIDENT: &str = "repair-claim: team-lead (repair-462-allowlist) until 2026-11-16T22:00Z";
const REPO: &str = "Generous-Corp/vellum";
const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";

fn at(hour: u32, minute: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 16, hour, minute, 0)
        .single()
        .expect("time")
}

fn record(id: u64, at: &str, body: &str) -> String {
    json!({"id": id, "type": "Bot", "association": "NONE", "at": at, "body": body}).to_string()
}

/// A GitHub that has PR 462 open on `fix` with `comments`. Each read takes
/// `read_cost` of the fence's clock.
struct FakeGitHub {
    comments: Vec<String>,
    /// PR 463, open on `other`.
    other: Vec<String>,
    fail_reads: Cell<u32>,
    calls: Cell<u32>,
    clock: Cell<Duration>,
    read_cost: Duration,
    timeouts: RefCell<Vec<Duration>>,
}

impl FakeGitHub {
    fn new(comments: &[(u64, &str, &str)]) -> Self {
        Self {
            comments: comments
                .iter()
                .map(|(id, at, body)| record(*id, at, body))
                .collect(),
            other: Vec::new(),
            fail_reads: Cell::new(0),
            calls: Cell::new(0),
            clock: Cell::new(Duration::ZERO),
            read_cost: Duration::from_millis(100),
            timeouts: RefCell::new(Vec::new()),
        }
    }

    fn read(&self, repo: &str, args: &[String], timeout: Duration) -> Result<String, String> {
        self.calls.set(self.calls.get() + 1);
        self.timeouts.borrow_mut().push(timeout);
        self.clock
            .set(self.clock.get() + self.read_cost.min(timeout));
        assert_eq!(repo, REPO);
        if self.fail_reads.get() > 0 {
            self.fail_reads.set(self.fail_reads.get() - 1);
            return Err("gh: HTTP 503".to_owned());
        }
        let path = &args[if args[1] == "--paginate" { 2 } else { 1 }];
        if path.starts_with(&format!(
            "repos/{REPO}/pulls?state=open&head=Generous-Corp:fix"
        )) {
            return Ok("462\n".to_owned());
        }
        if path.starts_with(&format!(
            "repos/{REPO}/pulls?state=open&head=Generous-Corp:other"
        )) {
            return Ok("463\n".to_owned());
        }
        if path == &format!("repos/{REPO}/issues/463/comments") {
            return Ok(self.other.join("\n"));
        }
        if path.starts_with(&format!("repos/{REPO}/pulls?")) {
            return Ok(String::new());
        }
        if path == &format!("repos/{REPO}/issues/462/comments") {
            return Ok(self.comments.join("\n"));
        }
        panic!("unexpected read {args:?}");
    }
}

fn on(owner: Option<&str>) -> FenceEnv {
    FenceEnv {
        owner: owner.map(str::to_owned),
        enabled: true,
        records: Vec::new(),
    }
}

/// Run the fence; the outcome and how often it slept before a retry.
fn run(github: &FakeGitHub, env: &FenceEnv, now: DateTime<Utc>) -> (Outcome, u32) {
    let read = |repo: &str, args: &[String], timeout: Duration| github.read(repo, args, timeout);
    let slept = Cell::new(0);
    let sleep = |delay: Duration| {
        assert_eq!(delay, RETRY_DELAY);
        slept.set(slept.get() + 1);
        github.clock.set(github.clock.get() + delay);
    };
    let elapsed = || github.clock.get();
    let io = FenceIo {
        read: &read,
        sleep: &sleep,
        elapsed: &elapsed,
        now,
    };
    let outcome = check_push(&io, env, REPO, &["fix".to_owned()]);
    (outcome, slept.get())
}

fn check(github: &FakeGitHub, owner: Option<&str>, now: DateTime<Utc>) -> Fence {
    run(github, &on(owner), now).0.fence
}

fn refused(fence: &Fence) -> &str {
    match fence {
        Fence::Refuse(text) => text,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn the_fence_is_off_unless_switched_on() {
    let github = FakeGitHub::new(&[(1, "2026-11-16T18:17:00Z", INCIDENT)]);
    for value in [None, Some("off"), Some("1"), Some("yes")] {
        let env = FenceEnv::from_env(&|name| {
            (name == SWITCH_ENV)
                .then(|| value.map(str::to_owned))
                .flatten()
        });
        assert!(!env.enabled, "{value:?}");
        assert_eq!(run(&github, &env, at(18, 36)).0.fence, Fence::Disabled);
    }
    assert_eq!(github.calls.get(), 0, "no read when off");
    assert!(FenceEnv::from_env(&|name| (name == SWITCH_ENV).then(|| "on".to_owned())).enabled);
}

#[test]
fn switched_on_a_foreign_live_claim_refuses_the_push() {
    let github = FakeGitHub::new(&[(1, "2026-11-16T18:17:00Z", INCIDENT)]);
    let env = FenceEnv::from_env(&|name| match name {
        SWITCH_ENV => Some("on".to_owned()),
        OWNER_ENV => Some("vellum-lead".to_owned()),
        _ => None,
    });
    let fence = run(&github, &env, at(18, 36)).0.fence;
    let text = refused(&fence);
    assert!(text.contains("team-lead until 2026-11-16T22:00Z"), "{text}");
    assert!(text.contains("pull request #462"), "{text}");
}

#[test]
fn a_refusal_names_only_the_take_never_an_override() {
    let github = FakeGitHub::new(&[(1, "2026-11-16T18:17:00Z", INCIDENT)]);
    let fence = check(&github, None, at(18, 36));
    let text = refused(&fence);
    assert!(
        text.contains(
            "shipyard claim take --repo Generous-Corp/vellum --pr 462 --from team-lead --reason"
        ),
        "{text}"
    );
    assert!(text.contains("owner names can collide"), "{text}");
    for named in [OWNER_ENV, SWITCH_ENV, DONE_ENV, "=off", "--no-verify"] {
        assert!(!text.contains(named), "a refusal names {named}: {text}");
    }
}

#[test]
fn the_owner_s_own_claim_allows_the_push() {
    let github = FakeGitHub::new(&[(1, "2026-11-16T18:17:00Z", INCIDENT)]);
    assert_eq!(check(&github, Some("team-lead"), at(18, 36)), Fence::Allow);
}

#[test]
fn an_expired_claim_allows_the_push() {
    let github = FakeGitHub::new(&[(1, "2026-11-16T18:17:00Z", INCIDENT)]);
    assert_eq!(check(&github, None, at(22, 0)), Fence::Allow);
}

#[test]
fn a_released_claim_allows_the_push_until_a_new_claim_lands() {
    let mut github = FakeGitHub::new(&[
        (1, "2026-11-16T18:17:00Z", INCIDENT),
        (
            2,
            "2026-11-16T18:30:00Z",
            "done\nrepair-claim-released: team-lead",
        ),
    ]);
    assert_eq!(check(&github, None, at(18, 36)), Fence::Allow);
    github.comments.push(record(
        3,
        "2026-11-16T18:40:00Z",
        "repair-claim: team-lead until 2026-11-16T23:00Z",
    ));
    refused(&check(&github, None, at(18, 41)));
}

#[test]
fn a_release_by_someone_else_does_not_end_the_claim() {
    let github = FakeGitHub::new(&[
        (1, "2026-11-16T18:17:00Z", INCIDENT),
        (
            2,
            "2026-11-16T18:30:00Z",
            "repair-claim-released: vellum-lead",
        ),
    ]);
    refused(&check(&github, None, at(18, 36)));
}

#[test]
fn a_bare_release_ends_every_earlier_claim_as_preclaim_reads_it() {
    let github = FakeGitHub::new(&[
        (1, "2026-11-16T18:17:00Z", INCIDENT),
        (2, "2026-11-16T18:30:00Z", "repair-claim-released:"),
    ]);
    assert_eq!(check(&github, None, at(18, 36)), Fence::Allow);
}

#[test]
fn an_unreadable_time_is_live_for_four_hours_after_its_comment_and_is_named() {
    let github = FakeGitHub::new(&[(
        7,
        "2026-11-16T18:17:00Z",
        "repair-claim: team-lead until tonight-ish",
    )]);
    let (outcome, _) = run(&github, &on(None), at(22, 16));
    refused(&outcome.fence);
    assert_eq!(
        outcome.notes,
        vec![
            "claim-fence: comment 7 claims for team-lead with an unreadable time `tonight-ish`; \
             it counts until 2026-11-16T22:17:00+00:00 (4h after it was posted)"
                .to_owned()
        ]
    );
    let (outcome, _) = run(&github, &on(None), at(22, 17));
    assert_eq!(outcome.fence, Fence::Allow, "four hours on, it has expired");
    assert_eq!(
        outcome.notes.len(),
        1,
        "still named when it no longer blocks"
    );
}

#[test]
fn an_outsider_s_claim_does_not_count() {
    let mut github = FakeGitHub::new(&[]);
    github.comments = vec![
        json!({"id": 1, "type": "User", "association": "NONE",
               "at": "2026-11-16T18:17:00Z", "body": INCIDENT})
        .to_string(),
    ];
    assert_eq!(check(&github, None, at(18, 36)), Fence::Allow);
}

#[test]
fn a_branch_without_an_open_pull_request_is_allowed() {
    let github = FakeGitHub::new(&[(1, "2026-11-16T18:17:00Z", INCIDENT)]);
    let read = |repo: &str, args: &[String], timeout: Duration| github.read(repo, args, timeout);
    let io = FenceIo {
        read: &read,
        sleep: &|_| {},
        elapsed: &|| Duration::ZERO,
        now: at(18, 36),
    };
    let outcome = check_push(&io, &on(None), REPO, &["new-branch".to_owned()]);
    assert_eq!(outcome.fence, Fence::Allow);
}

#[test]
fn an_unreadable_github_fails_open_loudly_after_one_retry() {
    let github = FakeGitHub::new(&[(1, "2026-11-16T18:17:00Z", INCIDENT)]);
    github.fail_reads.set(2);
    let (outcome, slept) = run(&github, &on(None), at(18, 36));
    assert_eq!(github.calls.get(), 2, "one read and one retry");
    assert_eq!(slept, 1, "one pause before the retry");
    assert_eq!(
        outcome.fence.message().as_deref(),
        Some(
            "claim-fence: could not read claims (gh: HTTP 503 (after one retry)); pushing without the check"
        )
    );
}

#[test]
fn one_failed_read_is_retried_and_still_refuses() {
    let github = FakeGitHub::new(&[(1, "2026-11-16T18:17:00Z", INCIDENT)]);
    github.fail_reads.set(1);
    let (outcome, slept) = run(&github, &on(None), at(18, 36));
    refused(&outcome.fence);
    assert_eq!(slept, 1);
}

#[test]
fn the_whole_check_fails_open_at_its_deadline() {
    let mut github = FakeGitHub::new(&[(1, "2026-11-16T18:17:00Z", INCIDENT)]);
    // The PR lookup alone takes 10 s; the comments read gets only the 5 s left.
    github.read_cost = Duration::from_secs(10);
    let (outcome, slept) = run(&github, &on(None), at(18, 36));
    refused(&outcome.fence);
    assert_eq!(
        *github.timeouts.borrow(),
        vec![DEADLINE, Duration::from_secs(5)],
        "each read gets what is left of the one deadline"
    );
    assert_eq!(slept, 0);

    // A first read that fails at 14 s leaves no time for the retry.
    let github = FakeGitHub::new(&[(1, "2026-11-16T18:17:00Z", INCIDENT)]);
    github.clock.set(Duration::from_secs(14));
    github.fail_reads.set(1);
    let (outcome, slept) = run(&github, &on(None), at(18, 36));
    let Fence::FailOpen(reason) = &outcome.fence else {
        panic!("expected fail-open, got {:?}", outcome.fence);
    };
    assert!(reason.contains("no time left to retry"), "{reason}");
    assert_eq!((github.calls.get(), slept), (1, 0));

    // Past the deadline, nothing more is read.
    let github = FakeGitHub::new(&[(1, "2026-11-16T18:17:00Z", INCIDENT)]);
    github.clock.set(DEADLINE);
    let (outcome, _) = run(&github, &on(None), at(18, 36));
    assert_eq!(
        outcome.fence,
        Fence::FailOpen("the check passed its 15s deadline".to_owned())
    );
    assert_eq!(github.calls.get(), 0);
}

#[test]
fn the_hook_skips_a_push_shipyard_pr_already_checked() {
    let pushed = format!("refs/heads/fix {HEAD} refs/heads/fix {}\n", "0".repeat(40));
    assert!(already_checked(&pushed, Some(HEAD)));
    let other = "f".repeat(40);
    assert!(
        !already_checked(&pushed, Some(&other)),
        "another head is checked"
    );
    assert!(!already_checked(&pushed, None));
    assert!(
        !already_checked(&pushed, Some("0123456")),
        "only a full SHA"
    );
    let two = format!("{pushed}refs/heads/x {other} refs/heads/x {other}\n");
    assert!(
        !already_checked(&two, Some(HEAD)),
        "every pushed ref must match"
    );
    assert!(!already_checked("", Some(HEAD)));
}

fn take_with(
    github: &FakeGitHub,
    owner: Option<&str>,
    from: &str,
    posted: &RefCell<Vec<Vec<String>>>,
) -> Result<String, TakeError> {
    let read = |repo: &str, args: &[String], timeout: Duration| github.read(repo, args, timeout);
    let io = FenceIo {
        read: &read,
        sleep: &|_| {},
        elapsed: &|| Duration::ZERO,
        now: at(18, 36),
    };
    let write = |repo: &str, args: &[String]| {
        assert_eq!(repo, REPO);
        posted.borrow_mut().push(args.to_vec());
        let body = args[3].strip_prefix("body=").expect("body field");
        Ok(json!({"body": body, "html_url": "https://github.com/x/1"}).to_string())
    };
    take(
        &io,
        &write,
        &on(owner),
        REPO,
        462,
        from,
        "lead\n  idle for an hour",
    )
}

#[test]
fn a_take_posts_the_taken_comment_and_then_the_push_is_allowed() {
    let mut github = FakeGitHub::new(&[(1, "2026-11-16T18:17:00Z", INCIDENT)]);
    let posted = RefCell::new(Vec::new());
    let out = take_with(&github, Some("vellum-lead"), "team-lead", &posted).expect("take");
    let line = "repair-claim-taken: vellum-lead from team-lead reason=lead idle for an hour";
    assert!(out.starts_with(line), "{out}");
    assert_eq!(
        posted.borrow()[0],
        vec![
            "api".to_owned(),
            format!("repos/{REPO}/issues/462/comments"),
            "--raw-field".to_owned(),
            format!("body={line}"),
        ]
    );
    github
        .comments
        .push(record(2, "2026-11-16T18:37:00Z", line));
    assert_eq!(
        check(&github, Some("vellum-lead"), at(18, 38)),
        Fence::Allow
    );
    // The claim moved, with its expiry: the old owner is now the foreigner.
    let fence = check(&github, Some("team-lead"), at(18, 38));
    let text = refused(&fence);
    assert!(
        text.contains("by vellum-lead until 2026-11-16T22:00Z"),
        "{text}"
    );
}

#[test]
fn a_take_needs_an_owner_and_a_live_claim_to_take() {
    let github = FakeGitHub::new(&[(1, "2026-11-16T18:17:00Z", INCIDENT)]);
    let posted = RefCell::new(Vec::new());
    let unset = take_with(&github, None, "team-lead", &posted).expect_err("no owner");
    assert!(
        unset.0.contains("SHIPYARD_CLAIM_OWNER is unset"),
        "{unset:?}"
    );
    let nothing = take_with(&github, Some("vellum-lead"), "someone", &posted).expect_err("none");
    assert!(nothing.0.contains("nothing to take"), "{nothing:?}");
    assert!(
        posted.borrow().is_empty(),
        "nothing posted on a refused take"
    );
}

#[test]
fn pushed_branches_reads_the_hook_s_stdin() {
    let stdin = "refs/heads/fix 1111 refs/heads/fix 2222\n\
                 refs/tags/v1 3333 refs/tags/v1 0000\n\
                 (delete) 0000 refs/heads/old 4444\n";
    assert_eq!(
        pushed_branches(stdin),
        vec!["fix".to_owned(), "old".to_owned()]
    );
}

// A session's own claim, proven by the record `shipyard claim post` wrote in
// its worktree rather than by an environment variable.

const MINE: &str = "repair-claim: lane-a until 2026-11-16T22:00:00Z";

/// The record `post` writes for comment `comment_id` on `repo#pr`.
fn mine(pr: u64, comment_id: u64) -> FenceEnv {
    FenceEnv {
        owner: None,
        enabled: true,
        records: vec![ClaimRecord {
            repo: REPO.to_ascii_lowercase(),
            pr,
            owner: "lane-a".to_owned(),
            until: "2026-11-16T22:00:00Z".to_owned(),
            comment_id,
        }],
    }
}

fn push(github: &FakeGitHub, env: &FenceEnv, branch: &str) -> Outcome {
    let read = |repo: &str, args: &[String], timeout: Duration| github.read(repo, args, timeout);
    let io = FenceIo {
        read: &read,
        sleep: &|_| {},
        elapsed: &|| Duration::ZERO,
        now: at(18, 36),
    };
    check_push(&io, env, REPO, &[branch.to_owned()])
}

#[test]
fn a_record_naming_the_live_claim_allows_its_own_push() {
    let github = FakeGitHub::new(&[(10, "2026-11-16T18:00:00Z", MINE)]);
    let outcome = push(&github, &mine(462, 10), "fix");
    assert_eq!(outcome.fence, Fence::Allow);
    assert!(outcome.notes.is_empty(), "{:?}", outcome.notes);
    // The same comments without the record: the claim is someone else's.
    refused(&push(&github, &on(None), "fix").fence);
}

#[test]
fn a_record_naming_another_comment_than_the_live_claim_is_refused() {
    // Another session claimed under the same owner name: its comment, not ours.
    let github = FakeGitHub::new(&[
        (10, "2026-11-16T18:00:00Z", MINE),
        (12, "2026-11-16T18:10:00Z", MINE),
    ]);
    let outcome = push(&github, &mine(462, 10), "fix");
    let text = refused(&outcome.fence);
    assert!(text.contains("comment 12"), "{text}");
    assert!(
        outcome.notes[0].contains("names comment 10 by lane-a"),
        "{:?}",
        outcome.notes
    );
}

#[test]
fn a_record_for_an_expired_released_or_taken_claim_does_not_count() {
    let reclaimed = "repair-claim: lane-a until 2026-11-16T23:00:00Z";
    let cases: [&[(u64, &str, &str)]; 3] = [
        // Expired at 18:30; the name is claimed again by another session.
        &[
            (
                10,
                "2026-11-16T18:00:00Z",
                "repair-claim: lane-a until 2026-11-16T18:30:00Z",
            ),
            (12, "2026-11-16T18:31:00Z", reclaimed),
        ],
        // Released; the name is claimed again by another session.
        &[
            (10, "2026-11-16T18:00:00Z", MINE),
            (11, "2026-11-16T18:20:00Z", "repair-claim-released: lane-a"),
            (12, "2026-11-16T18:21:00Z", reclaimed),
        ],
        // Taken by lane-b: the claim is lane-b's now.
        &[
            (10, "2026-11-16T18:00:00Z", MINE),
            (
                11,
                "2026-11-16T18:20:00Z",
                "repair-claim-taken: lane-b from lane-a reason=idle",
            ),
        ],
    ];
    for comments in cases {
        let github = FakeGitHub::new(comments);
        let outcome = push(&github, &mine(462, 10), "fix");
        refused(&outcome.fence);
        assert!(
            outcome
                .notes
                .iter()
                .any(|note| note.contains("not a live claim")),
            "{:?}",
            outcome.notes
        );
    }
    // Expired and nothing else live: there is no claim to respect.
    let github = FakeGitHub::new(&[(
        10,
        "2026-11-16T18:00:00Z",
        "repair-claim: lane-a until 2026-11-16T18:30:00Z",
    )]);
    assert_eq!(push(&github, &mine(462, 10), "fix").fence, Fence::Allow);
}

#[test]
fn a_record_does_not_exempt_another_session_s_claim() {
    let github = FakeGitHub::new(&[
        (10, "2026-11-16T18:00:00Z", MINE),
        (11, "2026-11-16T18:05:00Z", INCIDENT),
    ]);
    let text = refused(&push(&github, &mine(462, 10), "fix").fence).to_owned();
    assert!(text.contains("by team-lead"), "{text}");
}

#[test]
fn a_record_for_another_pull_request_or_branch_is_refused() {
    let mut github = FakeGitHub::new(&[(10, "2026-11-16T18:00:00Z", MINE)]);
    // The same owner and comment id, but recorded for #463.
    let outcome = push(&github, &mine(463, 10), "fix");
    refused(&outcome.fence);
    // A push of `other` (#463, claimed by lane-a in comment 10) with the
    // record for #462.
    github.other = vec![record(10, "2026-11-16T18:00:00Z", MINE)];
    let text = refused(&push(&github, &mine(462, 10), "other").fence).to_owned();
    assert!(text.contains("pull request #463"), "{text}");
    // The repository counts too, case aside.
    let mut env = mine(462, 10);
    env.records[0].repo = "generous-corp/pulp".to_owned();
    refused(&push(&github, &env, "fix").fence);
    env.records[0].repo = "GENEROUS-CORP/VELLUM".to_ascii_lowercase();
    assert_eq!(push(&github, &env, "fix").fence, Fence::Allow);
}

#[test]
fn a_record_reads_back_from_its_config_line() {
    let record = ClaimRecord {
        repo: "generous-corp/x.github.io".to_owned(),
        pr: 462,
        owner: "repair-lane/repair-2".to_owned(),
        until: "2026-11-27T22:00:00Z".to_owned(),
        comment_id: 123,
    };
    let key = ClaimRecord::key("Generous-Corp/x.github.io", 462);
    assert_eq!(key, "shipyard.claim.generous-corp/x.github.io#462.record");
    assert_eq!(
        ClaimRecord::parse(&format!("{key} {}", record.value())),
        Some(record)
    );
    assert_eq!(
        ClaimRecord::parse(&format!("{key} lane-a 2026-11-27T22:00:00Z")),
        None
    );
    assert_eq!(ClaimRecord::parse(&format!("{key} lane-a t 1 extra")), None);
    assert_eq!(ClaimRecord::parse("shipyard.other.x#1.record a b 1"), None);
    let pattern = regex::Regex::new(RECORD_KEY_PATTERN).expect("pattern");
    assert!(pattern.is_match(&key));
}

fn post_with(
    github: &FakeGitHub,
    owner: &str,
    posted: &RefCell<Vec<Vec<String>>>,
) -> Result<(ClaimRecord, String), TakeError> {
    let read = |repo: &str, args: &[String], timeout: Duration| github.read(repo, args, timeout);
    let io = FenceIo {
        read: &read,
        sleep: &|_| {},
        elapsed: &|| Duration::ZERO,
        now: at(18, 36),
    };
    let write = |repo: &str, args: &[String]| {
        assert_eq!(repo, REPO);
        posted.borrow_mut().push(args.to_vec());
        let body = args[3].strip_prefix("body=").expect("body field");
        // A guard may stamp the comment below the line.
        Ok(
            json!({"id": 99, "body": format!("{body}\n<!-- session: x -->"),
                  "html_url": "https://github.com/x/99"})
            .to_string(),
        )
    };
    post(&io, &write, REPO, 462, owner, at(22, 0))
}

#[test]
fn a_post_writes_the_claim_both_checkers_read_and_its_record() {
    let mut github = FakeGitHub::new(&[]);
    let posted = RefCell::new(Vec::new());
    let (record, out) = post_with(&github, "repair-lane/repair-2", &posted).expect("post");
    let line = "repair-claim: repair-lane/repair-2 until 2026-11-16T22:00:00Z";
    assert!(out.starts_with(line), "{out}");
    assert_eq!(posted.borrow()[0][3], format!("body={line}"));
    assert_eq!(
        record,
        ClaimRecord {
            repo: "generous-corp/vellum".to_owned(),
            pr: 462,
            owner: "repair-lane/repair-2".to_owned(),
            until: "2026-11-16T22:00:00Z".to_owned(),
            comment_id: 99,
        }
    );
    // This crate's parser reads it with the same owner and time.
    let lines = crate::work_claim::claim_lines(line);
    assert_eq!(lines[0].owner, "repair-lane/repair-2");
    assert_eq!(
        crate::work_claim::claim_expiry(&lines[0].until),
        Some(at(22, 0))
    );
    // And the posted comment, with its record, is this session's own.
    github.comments.push(record_json(99, line));
    let env = FenceEnv {
        records: vec![record],
        ..on(None)
    };
    assert_eq!(push(&github, &env, "fix").fence, Fence::Allow);
}

fn record_json(id: u64, body: &str) -> String {
    record(id, "2026-11-16T18:36:00Z", body)
}

#[test]
fn a_post_is_refused_over_another_live_claim_and_for_an_owner_pulp_cannot_read() {
    let github = FakeGitHub::new(&[(1, "2026-11-16T18:17:00Z", INCIDENT)]);
    let posted = RefCell::new(Vec::new());
    let error = post_with(&github, "lane-a", &posted).expect_err("held");
    assert!(error.0.contains("team-lead holds it"), "{error:?}");
    assert!(error.0.contains("shipyard claim take"), "{error:?}");
    let github = FakeGitHub::new(&[]);
    for owner in ["a:b", "a/b/c", "a b", "", "x@y", &"a".repeat(65)] {
        post_with(&github, owner, &posted).expect_err(owner);
    }
    assert!(posted.borrow().is_empty(), "nothing posted on a refusal");
    // The same owner claiming again is a renewal.
    let github = FakeGitHub::new(&[(1, "2026-11-16T18:00:00Z", MINE)]);
    post_with(&github, "lane-a", &posted).expect("renewal");
}

fn release_with(github: &FakeGitHub, record: &ClaimRecord) -> (String, Vec<Vec<String>>) {
    let read = |repo: &str, args: &[String], timeout: Duration| github.read(repo, args, timeout);
    let io = FenceIo {
        read: &read,
        sleep: &|_| {},
        elapsed: &|| Duration::ZERO,
        now: at(18, 36),
    };
    let posted = RefCell::new(Vec::new());
    let write = |_: &str, args: &[String]| {
        posted.borrow_mut().push(args.to_vec());
        let body = args[3].strip_prefix("body=").expect("body field");
        Ok(json!({"id": 100, "body": body}).to_string())
    };
    let out = release(&io, &write, REPO, record).expect("release");
    (out, posted.into_inner())
}

#[test]
fn a_release_posts_the_named_release_while_its_comment_holds_the_claim() {
    let github = FakeGitHub::new(&[(10, "2026-11-16T18:00:00Z", MINE)]);
    let record = mine(462, 10).records.remove(0);
    let (out, posted) = release_with(&github, &record);
    assert!(out.starts_with("repair-claim-released: lane-a"), "{out}");
    assert_eq!(posted[0][1], format!("repos/{REPO}/issues/462/comments"));
}

#[test]
fn a_stale_record_s_release_posts_nothing_over_another_session_s_reclaim() {
    // Comment 12 is another session's claim under the same owner name.
    let github = FakeGitHub::new(&[
        (
            10,
            "2026-11-16T18:00:00Z",
            "repair-claim: lane-a until 2026-11-16T18:30:00Z",
        ),
        (12, "2026-11-16T18:31:00Z", MINE),
    ]);
    let record = mine(462, 10).records.remove(0);
    let (out, posted) = release_with(&github, &record);
    assert!(posted.is_empty(), "posted {posted:?}");
    assert!(out.contains("posted nothing"), "{out}");
}

#[test]
fn a_record_exempts_only_its_own_owner_s_line_in_a_shared_comment() {
    // One comment carries two owners' claim lines; lane-a's record names it.
    let github = FakeGitHub::new(&[(
        10,
        "2026-11-16T18:00:00Z",
        "repair-claim: lane-a until 2026-11-16T22:00:00Z\nrepair-claim: lane-b until 2026-11-16T22:00:00Z",
    )]);
    let text = refused(&push(&github, &mine(462, 10), "fix").fence).to_owned();
    assert!(text.contains("by lane-b"), "{text}");
}

#[test]
fn until_reads_a_time_or_a_duration_and_stays_within_a_day() {
    let now = at(18, 36);
    assert_eq!(parse_until("2h", now), Ok(at(20, 36)));
    assert_eq!(parse_until("90m", now), Ok(at(20, 6)));
    assert_eq!(parse_until("1h30m", now), Ok(at(20, 6)));
    assert_eq!(parse_until("2026-11-16T22:00Z", now), Ok(at(22, 0)));
    for bad in ["m", "h", "", "soon", "2026-11-16T18:00Z", "25h", "-5m"] {
        assert!(parse_until(bad, now).is_err(), "{bad}");
    }
    assert_eq!(claim_time(at(22, 0)), "2026-11-16T22:00:00Z");
}
