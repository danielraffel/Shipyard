use serde_json::{Value, json};

use super::history_from_records;
use crate::head_approval::{ApprovalPolicy, SignerSource, guard::ApprovalSource};

const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const OLD: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn policy() -> ApprovalPolicy {
    ApprovalPolicy {
        required: true,
        reviewer_logins: vec!["someone".to_owned()],
        any_reviewer_but_author: false,
        human_reviewer_logins: Vec::new(),
        signers: None,
        signer_identities: std::collections::BTreeMap::new(),
        stamp_sessions: None,
        rebase_carry: false,
        signer_source: SignerSource::Host,
        carry_excluded: None,
        dry_run: false,
        source: ApprovalSource::Comments,
        trusted_signals: Vec::new(),
    }
}

fn comment(id: u64, login: &str, at: &str, updated: &str, body: &str) -> Value {
    json!({"id": id, "login": login, "type": "User", "at": at, "updated_at": updated, "body": body})
}

#[test]
fn holds_on_every_head_are_read_and_an_edit_dates_them_later() {
    let comments = [
        comment(
            1,
            "someone",
            "2026-11-27T10:00:00Z",
            "2026-11-27T10:00:00Z",
            &format!("no\nverdict: HOLD head:{OLD}"),
        ),
        comment(
            2,
            "someone",
            "2026-11-27T09:00:00Z",
            "2026-11-27T12:00:00Z",
            &format!("no\nverdict: HOLD head:{HEAD}"),
        ),
        // Not a counting reviewer.
        comment(
            3,
            "stranger",
            "2026-11-27T13:00:00Z",
            "2026-11-27T13:00:00Z",
            &format!("verdict: HOLD head:{HEAD}"),
        ),
    ];
    let history = history_from_records(7, &[], &comments, &policy(), None).expect("read");
    let times: Vec<_> = history
        .holds
        .iter()
        .map(|hold| (hold.head.clone(), hold.at.to_rfc3339()))
        .collect();
    assert_eq!(
        times,
        vec![
            (OLD.to_owned(), "2026-11-27T10:00:00+00:00".to_owned()),
            (HEAD.to_owned(), "2026-11-27T12:00:00+00:00".to_owned()),
        ]
    );
}

#[test]
fn an_edited_approval_never_dates_the_head() {
    let comments = [
        comment(
            1,
            "someone",
            "2026-11-27T10:00:00Z",
            "2026-11-27T10:00:00Z",
            &format!("ok\nverdict: APPROVE head:{HEAD}"),
        ),
        comment(
            2,
            "someone",
            "2026-11-27T11:00:00Z",
            "2026-11-27T11:30:00Z",
            &format!("ok\nverdict: APPROVE head:{HEAD}"),
        ),
    ];
    let history = history_from_records(7, &[], &comments, &policy(), None).expect("read");
    assert_eq!(
        history.approved_at(HEAD).map(|at| at.to_rfc3339()),
        Some("2026-11-27T10:00:00+00:00".to_owned())
    );
    assert_eq!(history.approved_at(OLD), None);
}

#[test]
fn a_hold_review_with_no_time_is_an_error() {
    let reviews = [json!({"id": 9, "login": "someone", "type": "User",
        "state": "CHANGES_REQUESTED", "commit_id": HEAD})];
    assert!(history_from_records(7, &reviews, &[], &policy(), None).is_err());

    // Control: the same review with a time is read.
    let reviews = [
        json!({"id": 9, "login": "someone", "type": "User", "at": "2026-11-27T10:00:00Z",
        "state": "CHANGES_REQUESTED", "commit_id": HEAD}),
    ];
    let history = history_from_records(7, &reviews, &[], &policy(), None).expect("read");
    assert_eq!(history.holds.len(), 1);
}

#[test]
fn a_self_stamped_approval_never_dates_the_head() {
    let stamped =
        format!("ok\n<!-- session: host-m3/lane-1 agent: me -->\nverdict: APPROVE head:{HEAD}");
    let comments = [comment(
        1,
        "someone",
        "2026-11-27T10:00:00Z",
        "2026-11-27T10:00:00Z",
        &stamped,
    )];
    let mine = crate::head_approval::with_arming_agent(Some("me"), || {
        history_from_records(7, &[], &comments, &policy(), None).expect("read")
    });
    assert_eq!(mine.approved_at(HEAD), None);

    // Control: armed by another agent, the same approval dates the head.
    let other = crate::head_approval::with_arming_agent(Some("someone-else"), || {
        history_from_records(7, &[], &comments, &policy(), None).expect("read")
    });
    assert!(other.approved_at(HEAD).is_some());
}

#[test]
fn an_approval_graphql_says_was_edited_never_dates_the_head() {
    let comments = [
        json!({"id": 1, "node_id": "IC_1", "login": "someone", "type": "User",
            "at": "2026-11-27T10:00:00Z", "updated_at": "2026-11-27T10:00:00Z",
            "body": format!("ok\nverdict: APPROVE head:{HEAD}")}),
        json!({"id": 2, "node_id": "IC_2", "login": "someone", "type": "User",
            "at": "2026-11-27T12:00:00Z", "updated_at": "2026-11-27T12:00:00Z",
            "body": format!("ok\nverdict: APPROVE head:{HEAD}")}),
    ];
    let history = history_from_records(7, &[], &comments, &policy(), None).expect("read");
    // GraphQL says the newer comment (IC_2) was edited; the older was not.
    let run_gh = |argv: &[String]| -> Result<String, String> {
        let joined = argv.join(" ");
        let edited = if joined.contains("id=IC_2") {
            json!("2026-11-27T12:30:00Z")
        } else {
            Value::Null
        };
        Ok(json!({"data": {"node": {"lastEditedAt": edited}}}).to_string())
    };
    let dated = history
        .clone()
        .without_edited_approvals(&run_gh, HEAD)
        .expect("read");
    assert_eq!(
        dated.approved_at(HEAD).map(|at| at.to_rfc3339()),
        Some("2026-11-27T10:00:00+00:00".to_owned())
    );
    // Control: unedited, the newer approval dates the head.
    assert_eq!(
        history.approved_at(HEAD).map(|at| at.to_rfc3339()),
        Some("2026-11-27T12:00:00+00:00".to_owned())
    );
    // An unreadable edit history is an error, never a date.
    let failing = |_: &[String]| -> Result<String, String> { Err("HTTP 502".to_owned()) };
    assert!(history.without_edited_approvals(&failing, HEAD).is_err());
}
