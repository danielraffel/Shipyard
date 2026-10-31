use serde_json::{Value, json};

use super::{ApprovalBlocker, Records, diagnose, not_approved};
use crate::head_approval::tests::by_default;
use crate::head_approval::{ApprovalPolicy, Ejection, HeadGate, decide_for_author};

const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const OLD_HEAD: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn fixture(name: &str) -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/github/review_before_arm")
        .join(name);
    serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("json")
}

/// A REST comment as the gate's records read it.
fn record(rest: &Value) -> Value {
    json!({"id": rest["id"], "login": rest["user"]["login"], "type": rest["user"]["type"],
           "association": rest["author_association"], "at": rest["created_at"],
           "body": rest["body"]})
}

/// A `--slurp` comments capture, flattened to gate records.
fn slurped_comments(name: &str) -> Vec<Value> {
    fixture(name)
        .as_array()
        .expect("pages")
        .iter()
        .flat_map(|page| page.as_array().cloned().unwrap_or_default())
        .map(|rest| record(&rest))
        .collect()
}

fn pull_head(name: &str) -> String {
    fixture(name)["head"]["sha"]
        .as_str()
        .expect("head")
        .to_owned()
}

/// The gate's verdict and the diagnosis, the way `evaluate_events` pairs them.
fn judge(
    policy: &ApprovalPolicy,
    head: &str,
    reviews: &[Value],
    comments: &[Value],
    author: Option<&str>,
) -> Option<ApprovalBlocker> {
    let queue = json!({"data": {"repository": {"pullRequest": {
        "timelineItems": {"nodes": []}}}}});
    let gate = decide_for_author(policy, head, reviews, comments, &queue, false, author);
    diagnose(
        &gate,
        &Records {
            head,
            reviews,
            comments,
            all_comments: comments,
        },
        policy,
        author,
    )
}

fn fleet(at: &str, body: &str) -> Value {
    json!({"id": 7, "login": "shipyard-local[bot]", "type": "Bot", "association": "CONTRIBUTOR",
           "at": at, "body": body})
}

/// Generous-Corp/pulp#11175 sat 3.9 h reported as "waiting for approval"
/// while it carried an APPROVE of its exact head: the App comment
/// 6544869514 is stamped `unattributed`, so the gate excludes it. That is
/// not "no verdict" — a reviewer did review it — and must not read as one.
#[test]
fn pulp_11175s_unattributed_approval_is_excluded_not_no_verdict() {
    let comment = record(&fixture("pulp-11175-unattributed-approve.json"));
    let head = "38615951c673da7532801491d21081aea8d25d35";
    let policy = by_default("Generous-Corp/pulp");
    let blocker = judge(
        &policy,
        head,
        &[],
        std::slice::from_ref(&comment),
        Some("shipyard-local[bot]"),
    )
    .expect("the gate refuses the head");
    assert_ne!(blocker, ApprovalBlocker::NoVerdict);
    assert_eq!(blocker.token(), "approval_excluded:unattributed");
    assert_eq!(blocker.since(), Some("2026-10-31T14:43:31Z"));
    let ApprovalBlocker::Excluded { id, .. } = blocker else {
        panic!("excluded");
    };
    assert_eq!(id, Some(6_544_869_514));
}

/// danielraffel/tartci#513: the only comment carries no verdict line.
#[test]
fn tartci_513_has_no_verdict() {
    let comments = slurped_comments("tartci-513-comments.json");
    let head = pull_head("tartci-513-pull.json");
    let blocker = judge(
        &by_default("danielraffel/tartci"),
        &head,
        &[],
        &comments,
        None,
    );
    assert_eq!(blocker, Some(ApprovalBlocker::NoVerdict));
    assert_eq!(blocker.expect("refused").token(), "no_verdict");
}

/// danielraffel/tartci#490: a HOLD names the current head. The token names
/// the holder and the comment that holds it.
#[test]
fn tartci_490_is_held_with_holder_and_comment() {
    let comments = slurped_comments("tartci-490-comments.json");
    let head = pull_head("tartci-490-pull.json");
    let blocker = judge(
        &by_default("danielraffel/tartci"),
        &head,
        &[],
        &comments,
        None,
    )
    .expect("refused");
    assert_eq!(blocker.token(), "held");
    let ApprovalBlocker::Held {
        holder,
        record,
        id,
        at,
    } = blocker
    else {
        panic!("held: {blocker:?}");
    };
    assert_eq!(record, "comment");
    assert_eq!(id, Some(6_133_569_336));
    assert_eq!(at.as_deref(), Some("2026-10-12T22:12:59Z"));
    assert!(!holder.is_empty());
}

/// danielraffel/tartci#511: the only APPROVE names an earlier head.
#[test]
fn tartci_511_approval_names_an_older_head() {
    let comments = slurped_comments("tartci-511-comments.json");
    let head = pull_head("tartci-511-pull.json");
    let blocker = judge(
        &by_default("danielraffel/tartci"),
        &head,
        &[],
        &comments,
        None,
    )
    .expect("refused");
    assert_eq!(blocker.token(), "approval_excluded:stale_head");
    assert_eq!(
        blocker,
        ApprovalBlocker::StaleHead {
            approved_head: "0d7179d1ffd76a84ae4617d7db48843ef6c47ade".to_owned(),
            at: Some("2026-10-12T22:00:33Z".to_owned()),
        }
    );
}

/// An App APPROVE signed by a reviewer no signer list counts.
#[test]
fn an_uncounted_signer_is_named() {
    let mut policy = by_default("Generous-Corp/pulp");
    policy.signers = Some(vec!["infra-review2".to_owned()]);
    let approval = fleet(
        "2026-10-31T10:00:00Z",
        &format!(
            "reviewer: pulp-review2\n<!-- session: Daniels-Mac-Studio-m3/08fb81d8 -->\nverdict: APPROVE head:{HEAD}"
        ),
    );
    let blocker =
        judge(&policy, HEAD, &[], &[approval], Some("shipyard-local[bot]")).expect("refused");
    assert_eq!(blocker.token(), "approval_excluded:uncounted_signer");
    assert_eq!(blocker.since(), Some("2026-10-31T10:00:00Z"));
}

/// The pull request's own author cannot approve it.
#[test]
fn the_authors_own_approval_is_named() {
    let policy = by_default("Generous-Corp/pulp");
    let own = json!({"id": 9, "login": "danielraffel", "type": "User", "association": "OWNER",
                     "at": "2026-10-31T11:00:00Z",
                     "body": format!("verdict: APPROVE head:{HEAD}")});
    let blocker = judge(&policy, HEAD, &[], &[own], Some("danielraffel")).expect("refused");
    assert_eq!(blocker.token(), "approval_excluded:pr_author");
}

/// An APPROVE naming the head but posted before the head's commit is
/// dropped by the gate; the diagnosis says so instead of "no verdict".
#[test]
fn an_approval_older_than_the_head_commit_is_named() {
    let policy = by_default("Generous-Corp/pulp");
    let early = fleet(
        "2026-10-31T09:00:00Z",
        &format!("verdict: APPROVE head:{HEAD}"),
    );
    let blocker = not_approved(
        &Records {
            head: HEAD,
            reviews: &[],
            comments: &[],
            all_comments: std::slice::from_ref(&early),
        },
        &policy,
        None,
    );
    assert_eq!(blocker.token(), "approval_excluded:predates_head");
    assert_eq!(blocker.since(), Some("2026-10-31T09:00:00Z"));
}

/// A HOLD outranks an excluded approval: only a push can unhold it.
#[test]
fn a_hold_outranks_an_excluded_approval_and_an_older_head() {
    let policy = by_default("Generous-Corp/pulp");
    let comments = [
        fleet(
            "2026-10-31T08:00:00Z",
            &format!("verdict: APPROVE head:{OLD_HEAD}"),
        ),
        fleet(
            "2026-10-31T09:00:00Z",
            &format!(
                "<!-- session: Daniels-Mac-Studio-m3/unattributed -->\nverdict: APPROVE head:{HEAD}"
            ),
        ),
        fleet(
            "2026-10-31T10:00:00Z",
            &format!("reviewer: infra-review2\nverdict: HOLD head:{HEAD}"),
        ),
    ];
    let blocker =
        judge(&policy, HEAD, &[], &comments, Some("shipyard-local[bot]")).expect("refused");
    assert_eq!(blocker.token(), "held");
    let ApprovalBlocker::Held { holder, .. } = blocker else {
        panic!("held");
    };
    assert_eq!(holder, "infra-review2");
}

#[test]
fn approved_and_not_required_heads_have_no_blocker() {
    let records = Records {
        head: HEAD,
        reviews: &[],
        comments: &[],
        all_comments: &[],
    };
    let policy = by_default("Generous-Corp/pulp");
    assert_eq!(
        diagnose(&HeadGate::NotRequired, &records, &policy, None),
        None
    );
}

#[test]
fn ejection_cap_and_self_approval_map_to_their_own_tokens() {
    let records = Records {
        head: HEAD,
        reviews: &[],
        comments: &[],
        all_comments: &[],
    };
    let policy = by_default("Generous-Corp/pulp");
    let cap = diagnose(
        &HeadGate::EjectionCap {
            head: HEAD.to_owned(),
            ejections: vec![Ejection {
                reason: "failed_checks".to_owned(),
                cause: "checks failed",
                at: Some("2026-10-31T12:00:00Z".to_owned()),
            }],
        },
        &records,
        &policy,
        None,
    )
    .expect("refused");
    assert_eq!(cap.token(), "ejection_cap");
    assert_eq!(cap.since(), Some("2026-10-31T12:00:00Z"));
    let own = diagnose(
        &HeadGate::SelfApproved {
            head: HEAD.to_owned(),
            agent: "m3:infra".to_owned(),
        },
        &records,
        &policy,
        None,
    )
    .expect("refused");
    assert_eq!(own.token(), "approval_excluded:self_approved");
}
