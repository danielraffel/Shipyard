//! The production glue after the approval gate ([`judge_after_gate`]), the
//! code the `refusal_for` judge runs: which reads it makes, how a failed read
//! fails, the fleet-guard bypass, and that P is dated only by an approval
//! GraphQL says was never edited. Records are served as `gh --jq` would emit
//! them; every other read fails.

use std::cell::Cell;

use serde_json::{Value, json};

use super::{Judgement, OpenPr, RecencyReads, judge_after_gate};
use crate::head_approval::{ApprovalPolicy, SignerSource, guard::ApprovalSource};
use crate::merge_steward::StewardPullRequest;

const P_HEAD: &str = "a100000000000000000000000000000000000000";
const Q_HEAD: &str = "a200000000000000000000000000000000000000";
const Q_OLD: &str = "a300000000000000000000000000000000000000";

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
        dry_run: true,
        source: ApprovalSource::Comments,
        trusted_signals: Vec::new(),
    }
}

fn p() -> StewardPullRequest {
    StewardPullRequest {
        number: 10,
        changed_files: None,
        commit_count: None,
        head_sha: P_HEAD.to_owned(),
        author: Some("author".to_owned()),
        head_branch: "feature/p".to_owned(),
        draft: false,
        merge_state: "CLEAN".to_owned(),
        auto_merge_active: false,
        queue_position: None,
        labels: Vec::new(),
        checks: Vec::new(),
    }
}

fn q() -> OpenPr {
    OpenPr {
        number: 11,
        head_ref: "feature/q".to_owned(),
        base_ref: "main".to_owned(),
        head_sha: Q_HEAD.to_owned(),
        cross_repository: false,
        author: Some("author".to_owned()),
        commits: super::CommitList::Complete(std::iter::once(Q_HEAD.to_owned()).collect()),
        stacked_on: None,
    }
}

fn review(state: &str, head: &str, at: &str) -> Value {
    json!({"id": 1, "login": "someone", "type": "User", "at": at, "state": state, "commit_id": head})
}

fn comment(id: u64, verdict: &str, head: &str, at: &str) -> Value {
    json!({"id": id, "node_id": format!("IC_{id}"), "login": "someone", "type": "User",
        "at": at, "updated_at": at, "body": format!("x\nverdict: {verdict} head:{head}")})
}

fn lines(records: &[Value]) -> String {
    records
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

/// What the fake serves.
struct Served {
    p_reviews: Vec<Value>,
    p_comments: Vec<Value>,
    q_comments: Vec<Value>,
    /// Node ids GraphQL reports as edited.
    edited: Vec<&'static str>,
    /// Fail Q's comment read.
    q_comments_fail: bool,
}

fn judge(served: &Served, gate: Judgement, comment_source: bool) -> (Judgement, usize) {
    let reads = Cell::new(0);
    let run_gh = |argv: &[String]| -> Result<String, String> {
        reads.set(reads.get() + 1);
        let joined = argv.join(" ");
        if joined.contains("pulls/10/reviews") {
            return Ok(lines(&served.p_reviews));
        }
        if joined.contains("issues/10/comments") {
            return Ok(lines(&served.p_comments));
        }
        if joined.contains("pulls/11/reviews") {
            return Ok(String::new());
        }
        if joined.contains("issues/11/comments") {
            if served.q_comments_fail {
                return Err("HTTP 502".to_owned());
            }
            return Ok(lines(&served.q_comments));
        }
        if joined.contains("graphql") {
            let edited = served
                .edited
                .iter()
                .any(|node| joined.contains(&format!("id={node}")));
            let at = if edited {
                json!("2026-11-27T13:30:00Z")
            } else {
                Value::Null
            };
            return Ok(json!({"data": {"node": {"lastEditedAt": at}}}).to_string());
        }
        Err(format!("not served: {joined}"))
    };
    let judgement = judge_after_gate(
        &run_gh,
        "o/r",
        &policy(),
        comment_source,
        &p(),
        &q(),
        gate,
        &mut RecencyReads::default(),
    );
    (judgement, reads.get())
}

fn unreviewed() -> Judgement {
    Judgement::Unapproved("no approval".to_owned())
}

/// P approved by review at 10:00; Q held at its current head at 11:00.
fn held_after_approval() -> Served {
    Served {
        p_reviews: vec![review("APPROVED", P_HEAD, "2026-11-27T10:00:00Z")],
        p_comments: Vec::new(),
        q_comments: vec![comment(5, "HOLD", Q_HEAD, "2026-11-27T11:00:00Z")],
        edited: Vec::new(),
        q_comments_fail: false,
    }
}

#[test]
fn an_unreadable_history_is_unreadable_never_cleared() {
    let served = Served {
        q_comments_fail: true,
        ..held_after_approval()
    };
    let (judgement, _) = judge(&served, unreviewed(), true);
    assert!(
        matches!(judgement, Judgement::Unreadable(_)),
        "{judgement:?}"
    );

    // Control: the same history read is judged (here held).
    let (judgement, _) = judge(&held_after_approval(), unreviewed(), true);
    assert!(matches!(judgement, Judgement::Held(_)), "{judgement:?}");
}

#[test]
fn under_fleet_guard_a_refused_q_still_refuses_with_no_reads() {
    // P approved after every HOLD (there is none): by recency Q is cleared.
    let clear = Served {
        q_comments: Vec::new(),
        ..held_after_approval()
    };
    let gate = Judgement::Held("admission check refused".to_owned());
    let (judgement, reads) = judge(&clear, gate.clone(), false);
    assert_eq!(judgement, gate);
    assert_eq!(
        reads, 0,
        "no verdict timeline is read under fleet-guard admission"
    );

    // Control: the same records under comment admission clear Q.
    let (judgement, _) = judge(&clear, gate, true);
    assert!(judgement.clears(), "{judgement:?}");
}

#[test]
fn an_approval_graphql_says_was_edited_never_dates_p() {
    // P's gate is satisfied by a review at 10:00, Q is held at 11:00, and P
    // also has a marker APPROVE at 12:00 that REST shows unedited and
    // GraphQL shows edited: P is dated 10:00 and refused.
    let served = Served {
        p_comments: vec![comment(7, "APPROVE", P_HEAD, "2026-11-27T12:00:00Z")],
        edited: vec!["IC_7"],
        ..held_after_approval()
    };
    let (judgement, _) = judge(&served, unreviewed(), true);
    assert!(matches!(judgement, Judgement::Held(_)), "{judgement:?}");

    // Control: unedited, the 12:00 approval dates P after the HOLD.
    let unedited = Served {
        edited: Vec::new(),
        ..served
    };
    let (judgement, _) = judge(&unedited, unreviewed(), true);
    assert!(judgement.clears(), "{judgement:?}");
}

#[test]
fn a_hold_on_an_older_head_of_q_counts() {
    let served = Served {
        q_comments: vec![comment(5, "HOLD", Q_OLD, "2026-11-27T11:00:00Z")],
        ..held_after_approval()
    };
    let (judgement, _) = judge(&served, unreviewed(), true);
    assert!(matches!(judgement, Judgement::Held(_)), "{judgement:?}");

    // Control: the same HOLD before P's approval does not.
    let older = Served {
        q_comments: vec![comment(5, "HOLD", Q_OLD, "2026-11-27T09:00:00Z")],
        ..held_after_approval()
    };
    let (judgement, _) = judge(&older, unreviewed(), true);
    assert!(judgement.clears(), "{judgement:?}");
}
