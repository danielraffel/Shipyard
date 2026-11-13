use std::cell::RefCell;

use super::*;

const HEAD: &str = "1111111111111111111111111111111111111111";
const OTHER: &str = "2222222222222222222222222222222222222222";

fn actor() -> Actor {
    Actor::from_env("m3".to_owned(), &|name| match name {
        "CLAUDE_CODE_SESSION_ID" => Some("caller-sid".to_owned()),
        "SHIPYARD_AGENT_ID" => Some("caller-agent".to_owned()),
        _ => None,
    })
}

fn target() -> Target {
    Target::parse("acme/widgets#7").expect("target")
}

fn args(act: Act) -> AdvanceArgs {
    AdvanceArgs {
        target: "acme/widgets#7".to_owned(),
        retrigger: act == Act::Retrigger,
        release_hold: act == Act::ReleaseHold,
        reason: Some("merge ref is stale after the base moved".to_owned()),
        decision: None,
        head: None,
        dry_run: false,
        json: false,
    }
}

fn pr_json(head: &str, labels: &[&str]) -> String {
    json!({
        "state": "open", "merged": false, "head": {"sha": head}, "base": {"ref": "main"},
        "user": {"login": "dev-human"},
        "body": "<!-- shipyard-owner: host=m5 client=codex sid=author-sid at=2026-11-13T10:00:00Z -->",
        "labels": labels.iter().map(|name| json!({"name": name})).collect::<Vec<_>>(),
    })
    .to_string()
}

fn queue_json() -> String {
    json!({"data": {"repository": {"pullRequest": {
        "number": 7, "state": "OPEN", "headRefOid": HEAD, "isInMergeQueue": false,
        "mergeQueueEntry": null, "autoMergeRequest": null,
        "timelineItems": {"pageInfo": {"hasPreviousPage": false}, "nodes": []}}}}})
    .to_string()
}

fn rules_json() -> String {
    json!([[{"type": "required_status_checks", "parameters": {"required_status_checks": [
        {"context": "build"}, {"context": "test"}]}}]])
    .to_string()
}

fn runs_json(total: u64) -> String {
    let run = |id: u64, name: &str| {
        json!({"id": id, "name": name, "status": "completed", "conclusion": "success",
               "completed_at": "2026-11-13T11:00:00Z", "app": {"id": 15368}})
    };
    json!([{"total_count": total, "check_runs": [run(1, "build"), run(2, "test")]}]).to_string()
}

/// A fake GitHub: answers each read by its path, records every write.
struct Fake {
    heads: RefCell<Vec<String>>,
    runs_total: u64,
    comments: Vec<Value>,
    events: Vec<Value>,
    labels: Vec<&'static str>,
    writes: RefCell<Vec<Vec<String>>>,
    close_error: Option<String>,
    reopen_fails: bool,
}

impl Fake {
    fn new() -> Self {
        Self {
            heads: RefCell::new(vec![HEAD.to_owned()]),
            runs_total: 2,
            comments: Vec::new(),
            events: Vec::new(),
            labels: Vec::new(),
            writes: RefCell::new(Vec::new()),
            close_error: None,
            reopen_fails: false,
        }
    }

    fn read(&self, repo: &str, args: &[String]) -> Result<String, String> {
        assert_eq!(repo, "acme/widgets");
        let has = |needle: &str| args.iter().any(|arg| arg == needle);
        let lines = |records: &[Value]| {
            records
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        };
        if has("repos/acme/widgets/pulls/7") {
            let mut heads = self.heads.borrow_mut();
            let head = if heads.len() > 1 {
                heads.remove(0)
            } else {
                heads[0].clone()
            };
            return Ok(pr_json(&head, &self.labels));
        }
        if has("graphql") {
            return Ok(queue_json());
        }
        if has("repos/acme/widgets/rules/branches/main") {
            return Ok(rules_json());
        }
        if has("repos/acme/widgets/branches/main/protection/required_status_checks") {
            return Err("gh: Branch not protected (HTTP 404)".to_owned());
        }
        if has(&format!(
            "repos/acme/widgets/commits/{HEAD}/check-runs?per_page=100"
        )) {
            return Ok(runs_json(self.runs_total));
        }
        if has(&format!(
            "repos/acme/widgets/commits/{HEAD}/statuses?per_page=100"
        )) {
            return Ok("[[]]".to_owned());
        }
        if has("repos/acme/widgets/issues/7/comments") {
            return Ok(lines(&self.comments));
        }
        if has("repos/acme/widgets/issues/7/events") {
            return Ok(lines(&self.events));
        }
        if has("repos/acme/widgets/contents/.shipyard/config.toml?ref=main") {
            return Ok(
                "[auto_merge]\narm_requires_head_approval = true\nreviewer_logins = [\"rev-human\"]\n"
                    .to_owned(),
            );
        }
        if has("repos/acme/widgets/pulls/7/reviews") {
            return Ok(String::new());
        }
        if has(&format!("repos/acme/widgets/commits/{HEAD}")) {
            return Ok("2026-11-13T08:00:00Z".to_owned());
        }
        Err(format!("unexpected read: {}", args.join(" ")))
    }

    fn write(&self, repo: &str, args: &[String]) -> Result<String, String> {
        assert_eq!(repo, "acme/widgets");
        self.writes.borrow_mut().push(args.to_vec());
        let has = |needle: &str| args.iter().any(|arg| arg == needle);
        if has("state=closed") {
            return self
                .close_error
                .clone()
                .map_or_else(|| Ok(json!({"state": "closed"}).to_string()), Err);
        }
        if has("state=open") {
            if self.reopen_fails {
                return Err("HTTP 502".to_owned());
            }
            return Ok(json!({"state": "open"}).to_string());
        }
        if has("DELETE") {
            return Ok("[]".to_owned());
        }
        if has("repos/acme/widgets/issues/7/comments") {
            return Ok(json!({"id": 999}).to_string());
        }
        Err(format!("unexpected write: {}", args.join(" ")))
    }

    fn writes(&self) -> Vec<Vec<String>> {
        self.writes.borrow().clone()
    }
}

fn run_with(fake: &Fake, state_dir: &Path, args: &AdvanceArgs) -> (ExitCode, String) {
    let read = |repo: &str, gh: &[String]| fake.read(repo, gh);
    let write = |repo: &str, gh: &[String]| fake.write(repo, gh);
    let io = Io {
        read: &read,
        write: &write,
        now: DateTime::parse_from_rfc3339("2026-11-13T12:00:00Z")
            .expect("time")
            .with_timezone(&Utc),
        state_dir,
        owner_record: None,
    };
    let mut out = Vec::new();
    let code = run(&io, &target(), args, &actor(), &mut out).expect("ran");
    (code, String::from_utf8(out).expect("utf8"))
}

fn log_lines(state_dir: &Path) -> Vec<Value> {
    std::fs::read_to_string(state_dir.join(AUDIT_LOG))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).expect("json line"))
        .collect()
}

fn comment_bodies(writes: &[Vec<String>]) -> Vec<String> {
    writes
        .iter()
        .filter(|write| write.iter().any(|arg| arg.ends_with("/issues/7/comments")))
        .filter_map(|write| {
            write
                .iter()
                .find_map(|arg| arg.strip_prefix("body=").map(str::to_owned))
        })
        .collect()
}

#[test]
fn dry_run_prints_the_decision_and_changes_nothing() {
    let temp = tempfile::tempdir().expect("tempdir");
    let fake = Fake::new();
    let mut request = args(Act::Retrigger);
    request.dry_run = true;
    let (code, out) = run_with(&fake, temp.path(), &request);
    assert_eq!(code, ExitCode::SUCCESS, "{out}");
    assert!(out.contains("dry-run: would act; nothing changed"), "{out}");
    assert!(fake.writes().is_empty());
    assert!(!temp.path().join(AUDIT_LOG).exists());
}

#[test]
fn a_retrigger_closes_reopens_and_audits_once() {
    let temp = tempfile::tempdir().expect("tempdir");
    let fake = Fake::new();
    let (code, out) = run_with(&fake, temp.path(), &args(Act::Retrigger));
    assert_eq!(code, ExitCode::SUCCESS, "{out}");
    let writes = fake.writes();
    assert_eq!(writes.len(), 3, "{writes:?}");
    assert!(writes[0].contains(&"state=closed".to_owned()));
    assert!(writes[1].contains(&"state=open".to_owned()));
    let bodies = comment_bodies(&writes);
    assert_eq!(bodies.len(), 1);
    assert!(
        bodies[0].contains("shipyard advance retrigger"),
        "{}",
        bodies[0]
    );
    assert!(bodies[0].contains(HEAD), "{}", bodies[0]);
    let lines = log_lines(temp.path());
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["act"], "retrigger");
    assert_eq!(lines[0]["head"], HEAD);
    assert_eq!(lines[0]["audit_comment_id"], 999);
    assert!(
        writes
            .iter()
            .flatten()
            .all(|arg| !arg.starts_with("GHAPP_ALLOW")),
        "no override is ever passed"
    );
}

#[test]
fn a_guard_refusing_the_close_changes_and_logs_nothing() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut fake = Fake::new();
    fake.close_error = Some(
        "ghapp exited 1: pr-close-guard: refusing PR #7; current-base...PR-head is diverged"
            .to_owned(),
    );
    let (code, out) = run_with(&fake, temp.path(), &args(Act::Retrigger));
    assert_eq!(code, ExitCode::from(1), "{out}");
    assert!(out.contains("close_refused"), "{out}");
    assert!(out.contains("pr-close-guard"), "{out}");
    assert_eq!(fake.writes().len(), 1, "only the refused close");
    assert!(log_lines(temp.path()).is_empty());
}

#[test]
fn a_head_that_moves_before_the_write_is_refused() {
    let temp = tempfile::tempdir().expect("tempdir");
    let fake = Fake::new();
    fake.heads.replace(vec![HEAD.to_owned(), OTHER.to_owned()]);
    let (code, out) = run_with(&fake, temp.path(), &args(Act::Retrigger));
    assert_eq!(code, ExitCode::from(1), "{out}");
    assert!(out.contains("head_moved"), "{out}");
    assert!(fake.writes().is_empty());
    assert!(log_lines(temp.path()).is_empty());
}

#[test]
fn an_expected_head_that_differs_is_refused_before_any_write() {
    let temp = tempfile::tempdir().expect("tempdir");
    let fake = Fake::new();
    let mut request = args(Act::Retrigger);
    request.head = Some(OTHER.to_owned());
    let (code, out) = run_with(&fake, temp.path(), &request);
    assert_eq!(code, ExitCode::from(1), "{out}");
    assert!(out.contains("head_moved"), "{out}");
    assert!(fake.writes().is_empty());
}

#[test]
fn a_reopen_that_fails_is_audited_and_exits_three() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut fake = Fake::new();
    fake.reopen_fails = true;
    let (code, out) = run_with(&fake, temp.path(), &args(Act::Retrigger));
    assert_eq!(code, ExitCode::from(3), "{out}");
    let writes = fake.writes();
    assert_eq!(
        writes
            .iter()
            .filter(|write| write.contains(&"state=open".to_owned()))
            .count(),
        2,
        "the reopen is retried once"
    );
    let bodies = comment_bodies(&writes);
    assert_eq!(bodies.len(), 1);
    assert!(bodies[0].contains("left closed"), "{}", bodies[0]);
    let lines = log_lines(temp.path());
    assert_eq!(lines.len(), 1);
    assert!(
        lines[0]["outcome"]
            .as_str()
            .is_some_and(|outcome| outcome.contains("left closed"))
    );
}

#[test]
fn a_partial_check_read_is_refused_as_unread() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut fake = Fake::new();
    fake.runs_total = 3;
    let (code, out) = run_with(&fake, temp.path(), &args(Act::Retrigger));
    assert_eq!(code, ExitCode::from(2), "{out}");
    assert!(out.contains("refusing a partial read"), "{out}");
    assert!(fake.writes().is_empty());
}

#[test]
fn an_unwritable_audit_log_refuses_before_acting() {
    let temp = tempfile::tempdir().expect("tempdir");
    let state = temp.path().join("state");
    std::fs::write(&state, "a file, not a directory").expect("write");
    let fake = Fake::new();
    let (code, out) = run_with(&fake, &state, &args(Act::Retrigger));
    assert_eq!(code, ExitCode::from(2), "{out}");
    assert!(out.contains("audit log"), "{out}");
    assert!(fake.writes().is_empty());
}

#[test]
fn exactly_one_act_is_required() {
    let temp = tempfile::tempdir().expect("tempdir");
    let fake = Fake::new();
    let read = |repo: &str, gh: &[String]| fake.read(repo, gh);
    let write = |repo: &str, gh: &[String]| fake.write(repo, gh);
    let io = Io {
        read: &read,
        write: &write,
        now: Utc::now(),
        state_dir: temp.path(),
        owner_record: None,
    };
    for (retrigger, release_hold) in [(false, false), (true, true)] {
        let mut request = args(Act::Retrigger);
        request.retrigger = retrigger;
        request.release_hold = release_hold;
        let error =
            run(&io, &target(), &request, &actor(), &mut Vec::new()).expect_err("usage error");
        assert_eq!(error.code, 2);
    }
    assert!(fake.writes().is_empty());
}

fn hold_fake() -> Fake {
    let mut fake = Fake::new();
    fake.labels = vec![HOLD_LABEL];
    fake.events = vec![json!({"event": "labeled", "label": HOLD_LABEL,
        "login": "shipyard-local[bot]", "at": "2026-11-13T09:00:00Z"})];
    fake.comments = vec![
        json!({"id": 40, "login": "shipyard-local[bot]", "type": "Bot", "association": "NONE",
            "at": "2026-11-13T09:01:00Z",
            "body": format!("Holding with {HOLD_LABEL}.\nhold-condition: green\nhold-condition: approved")}),
        json!({"id": 50, "login": "rev-human", "type": "User", "association": "MEMBER",
            "at": "2026-11-13T11:30:00Z",
            "body": format!("ok\n<!-- session: m1/reviewer-run-1 agent: reviewer -->\nverdict: APPROVE head:{HEAD}")}),
    ];
    fake
}

#[test]
fn a_release_hold_uses_the_steward_gate_removes_the_label_and_audits_once() {
    let temp = tempfile::tempdir().expect("tempdir");
    let guards = tempfile::tempdir().expect("guards");
    let fake = hold_fake();
    let (code, out) = crate::head_approval::with_guards_dir(guards.path(), || {
        crate::head_approval::with_host_review_default(true, || {
            run_with(&fake, temp.path(), &args(Act::ReleaseHold))
        })
    });
    assert_eq!(code, ExitCode::SUCCESS, "{out}");
    assert!(out.contains("comment 50 (stamped reviewer-run-1)"), "{out}");
    let writes = fake.writes();
    assert_eq!(writes.len(), 2, "{writes:?}");
    assert!(writes[0].contains(&"DELETE".to_owned()));
    assert!(
        writes[0]
            .iter()
            .any(|arg| arg.ends_with("/labels/shipyard%3Ano-auto-merge"))
    );
    assert_eq!(comment_bodies(&writes).len(), 1);
    assert_eq!(log_lines(temp.path()).len(), 1);
}

#[test]
fn a_release_hold_without_an_approval_is_refused_without_writing() {
    let temp = tempfile::tempdir().expect("tempdir");
    let guards = tempfile::tempdir().expect("guards");
    let mut fake = hold_fake();
    fake.comments.pop();
    let (code, out) = crate::head_approval::with_guards_dir(guards.path(), || {
        crate::head_approval::with_host_review_default(true, || {
            run_with(&fake, temp.path(), &args(Act::ReleaseHold))
        })
    });
    assert_eq!(code, ExitCode::from(1), "{out}");
    assert!(out.contains("approval_missing"), "{out}");
    assert!(fake.writes().is_empty());
    assert!(log_lines(temp.path()).is_empty());
}
