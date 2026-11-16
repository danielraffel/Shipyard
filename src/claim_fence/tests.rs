use std::cell::{Cell, RefCell};

use chrono::TimeZone;
use serde_json::json;

use super::*;

/// The claim from Generous-Corp/vellum#462, verbatim.
const INCIDENT: &str = "repair-claim: team-lead (repair-462-allowlist) until 2026-11-16T22:00Z";
const REPO: &str = "Generous-Corp/vellum";

fn at(hour: u32, minute: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 16, hour, minute, 0)
        .single()
        .expect("time")
}

fn record(id: u64, at: &str, body: &str) -> String {
    json!({"id": id, "type": "Bot", "association": "NONE", "at": at, "body": body}).to_string()
}

/// A GitHub that has PR 462 open on `fix` with `comments`.
struct FakeGitHub {
    comments: Vec<String>,
    fail_reads: Cell<u32>,
    calls: Cell<u32>,
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
        }
    }

    fn read(&self, repo: &str, args: &[String]) -> Result<String, String> {
        self.calls.set(self.calls.get() + 1);
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

fn check(github: &FakeGitHub, owner: Option<&str>, now: DateTime<Utc>) -> Fence {
    check_counting(github, owner, now).0
}

/// [`check`], with how often the fence slept before a retry.
fn check_counting(github: &FakeGitHub, owner: Option<&str>, now: DateTime<Utc>) -> (Fence, u32) {
    let read = |repo: &str, args: &[String]| github.read(repo, args);
    let slept = Cell::new(0);
    let sleep = |delay: Duration| {
        assert_eq!(delay, RETRY_DELAY);
        slept.set(slept.get() + 1);
    };
    let io = FenceIo {
        read: &read,
        sleep: &sleep,
        now,
    };
    let env = FenceEnv {
        owner: owner.map(str::to_owned),
        disabled: false,
    };
    let fence = check_push(&io, &env, REPO, &["fix".to_owned()]);
    (fence, slept.get())
}

fn refused(fence: &Fence) -> &str {
    match fence {
        Fence::Refuse(text) => text,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_foreign_live_claim_refuses_the_push() {
    let github = FakeGitHub::new(&[(1, "2026-11-16T18:17:00Z", INCIDENT)]);
    let fence = check(&github, Some("vellum-lead"), at(18, 36));
    let text = refused(&fence);
    assert!(text.contains("team-lead until 2026-11-16T22:00Z"), "{text}");
    assert!(text.contains("pull request #462"), "{text}");
}

#[test]
fn an_unset_owner_is_refused_with_both_ways_forward() {
    let github = FakeGitHub::new(&[(1, "2026-11-16T18:17:00Z", INCIDENT)]);
    let fence = check(&github, None, at(18, 36));
    let text = refused(&fence);
    assert!(text.contains("SHIPYARD_CLAIM_OWNER is unset"), "{text}");
    assert!(
        text.contains("set SHIPYARD_CLAIM_OWNER=team-lead if this is your claim"),
        "{text}"
    );
    assert!(
        text.contains(
            "shipyard claim take --repo Generous-Corp/vellum --pr 462 --from team-lead --reason"
        ),
        "{text}"
    );
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
fn a_garbled_time_counts_as_live() {
    let github = FakeGitHub::new(&[(
        1,
        "2026-11-16T18:17:00Z",
        "repair-claim: team-lead until tonight-ish",
    )]);
    refused(&check(&github, None, at(23, 59)));
}

#[test]
fn an_outsider_s_claim_does_not_count() {
    let github = FakeGitHub {
        comments: vec![
            json!({"id": 1, "type": "User", "association": "NONE",
                   "at": "2026-11-16T18:17:00Z", "body": INCIDENT})
            .to_string(),
        ],
        fail_reads: Cell::new(0),
        calls: Cell::new(0),
    };
    assert_eq!(check(&github, None, at(18, 36)), Fence::Allow);
}

#[test]
fn a_branch_without_an_open_pull_request_is_allowed() {
    let github = FakeGitHub::new(&[(1, "2026-11-16T18:17:00Z", INCIDENT)]);
    let read = |repo: &str, args: &[String]| github.read(repo, args);
    let io = FenceIo {
        read: &read,
        sleep: &|_| {},
        now: at(18, 36),
    };
    let fence = check_push(&io, &FenceEnv::default(), REPO, &["new-branch".to_owned()]);
    assert_eq!(fence, Fence::Allow);
}

#[test]
fn an_unreadable_github_fails_open_loudly_after_one_retry() {
    let github = FakeGitHub::new(&[(1, "2026-11-16T18:17:00Z", INCIDENT)]);
    github.fail_reads.set(2);
    let (fence, slept) = check_counting(&github, None, at(18, 36));
    assert_eq!(github.calls.get(), 2, "one read and one retry");
    assert_eq!(slept, 1, "one pause before the retry");
    let Fence::FailOpen(reason) = &fence else {
        panic!("expected fail-open, got {fence:?}");
    };
    assert!(reason.contains("after one retry"), "{reason}");
    assert_eq!(
        fence.message().as_deref(),
        Some(
            "claim-fence: could not read claims (gh: HTTP 503 (after one retry)); pushing without the check"
        )
    );
}

#[test]
fn one_failed_read_is_retried_and_still_refuses() {
    let github = FakeGitHub::new(&[(1, "2026-11-16T18:17:00Z", INCIDENT)]);
    github.fail_reads.set(1);
    let (fence, slept) = check_counting(&github, None, at(18, 36));
    refused(&fence);
    assert_eq!(slept, 1);
}

#[test]
fn the_kill_switch_turns_the_fence_off() {
    let env = FenceEnv::from_env(&|name| (name == SWITCH_ENV).then(|| "off".to_owned()));
    assert!(env.disabled);
    let io = FenceIo {
        read: &|_, _| panic!("no read when off"),
        sleep: &|_| {},
        now: at(18, 36),
    };
    assert_eq!(
        check_push(&io, &env, REPO, &["fix".to_owned()]),
        Fence::Disabled
    );
}

fn take_with(
    github: &FakeGitHub,
    owner: Option<&str>,
    from: &str,
    posted: &RefCell<Vec<Vec<String>>>,
) -> Result<String, TakeError> {
    let read = |repo: &str, args: &[String]| github.read(repo, args);
    let io = FenceIo {
        read: &read,
        sleep: &|_| {},
        now: at(18, 36),
    };
    let write = |repo: &str, args: &[String]| {
        assert_eq!(repo, REPO);
        posted.borrow_mut().push(args.to_vec());
        let body = args[3].strip_prefix("body=").expect("body field");
        Ok(json!({"body": body, "html_url": "https://github.com/x/1"}).to_string())
    };
    let env = FenceEnv {
        owner: owner.map(str::to_owned),
        disabled: false,
    };
    take(
        &io,
        &write,
        &env,
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
