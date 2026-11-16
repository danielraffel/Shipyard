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
