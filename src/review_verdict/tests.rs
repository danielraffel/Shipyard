use serde_json::{Value, json};

use super::review_verdict;

const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const NEWER: &str = "cccccccccccccccccccccccccccccccccccccccc";
const SIGNED: &str = "[auto_merge]\nreviewer_signers = [\"pulp-review\", \"infra-review2\"]\n";

/// A fake `gh` for `repo#7`: the pull request at `live`, the base's config
/// (`None`: no file), and these reviews and comments.
fn gh(
    live: &'static str,
    config: Option<&'static str>,
    reviews: Vec<Value>,
    comments: Vec<Value>,
) -> impl Fn(&[String]) -> Result<String, String> {
    move |args: &[String]| {
        let joined = args.join(" ");
        if joined.contains("contents/.shipyard/config.toml") {
            return config
                .map(str::to_owned)
                .ok_or_else(|| "gh: Not Found (HTTP 404)".to_owned());
        }
        if joined.contains("/reviews") {
            return Ok(reviews
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"));
        }
        if joined.contains("/comments") {
            return Ok(comments
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"));
        }
        if joined.contains("pulls/7 --jq {head") {
            return Ok(
                json!({"head": live, "base": "main", "author": "shipyard-local[bot]"}).to_string(),
            );
        }
        if joined.contains("/commits/") && joined.contains(".commit.committer.date") {
            return Ok("2026-10-14T16:00:00Z\n".to_owned());
        }
        if joined.contains("pulls/7 --jq .user.login") {
            return Ok("shipyard-local[bot]\n".to_owned());
        }
        Err(format!("unexpected gh call: {joined}"))
    }
}

fn app_approval(head: &str, signer: Option<&str>) -> Value {
    let line = signer.map_or_else(String::new, |name| format!("reviewer: {name}\n"));
    json!({"id": 5, "login": "shipyard-local[bot]", "type": "Bot", "association": "CONTRIBUTOR",
           "at": "2026-10-14T17:00:00Z",
           "body": format!("Reviewed.\n{line}verdict: APPROVE head:{head}")})
}

fn owner_review(head: &str) -> Value {
    json!({"login": "danielraffel", "type": "User", "association": "OWNER",
           "at": "2026-10-14T17:50:05Z", "state": "APPROVED", "commit_id": head})
}

/// The repository-side verdict, on a host whose own signers file would allow
/// anyone involved: the rule must not read it.
fn judged(config: Option<&'static str>, comments: Vec<Value>) -> super::Verdict {
    let gh = gh(HEAD, config, vec![], comments);
    crate::head_approval::with_host_signers(&["codex-g2h", "pulp-review", "infra-review2"], || {
        review_verdict(&gh, "danielraffel/tartci", 7, None, true)
    })
    .expect("readable")
}

#[test]
fn a_listed_signers_app_approval_allows_the_head() {
    let verdict = judged(Some(SIGNED), vec![app_approval(HEAD, Some("pulp-review"))]);
    assert!(verdict.allowed(), "{verdict:?}");
    assert!(verdict.required);
    assert!(verdict.approval.is_some());
    assert_eq!(verdict.head, HEAD);
    assert!(verdict.fix.is_none());
    assert_eq!(
        serde_json::to_value(&verdict).expect("json")["verdict"],
        "allowed"
    );
}

/// pulp#10320: an App APPROVE with no reviewer of record.
#[test]
fn an_unsigned_app_approval_is_refused() {
    let verdict = judged(Some(SIGNED), vec![app_approval(HEAD, None)]);
    assert!(!verdict.allowed(), "{verdict:?}");
    assert!(
        verdict
            .audit_events
            .contains(&"excluded_app_approval:it has no `reviewer: <name>` line".to_owned()),
        "{verdict:?}"
    );
}

/// The host's file lists codex-g2h; the base does not.
#[test]
fn a_signer_the_base_does_not_list_is_refused_whatever_the_host_lists() {
    let verdict = judged(Some(SIGNED), vec![app_approval(HEAD, Some("codex-g2h"))]);
    assert!(!verdict.allowed(), "{verdict:?}");
}

/// A merge-group runner, or a base that never set a list: the host's file
/// must not switch the rule off, and nothing it lists counts.
#[test]
fn a_required_repository_with_no_signer_list_refuses_every_app_approval() {
    for config in [None, Some("[auto_merge]\nqueue = true\n")] {
        let verdict = judged(config, vec![app_approval(HEAD, Some("pulp-review"))]);
        assert!(!verdict.allowed(), "{config:?}: {verdict:?}");
        assert!(
            verdict
                .audit_events
                .iter()
                .any(|event| event.contains("no signer list is configured on the base branch")),
            "{verdict:?}"
        );
    }
}

/// pulp#10293: only the owner's personal login approved the head.
#[test]
fn an_unlisted_humans_review_is_refused_and_named() {
    let gh = gh(HEAD, Some(SIGNED), vec![owner_review(HEAD)], vec![]);
    let verdict = review_verdict(&gh, "danielraffel/tartci", 7, None, true).expect("readable");
    assert!(!verdict.allowed(), "{verdict:?}");
    assert_eq!(verdict.excluded_approvals, vec!["danielraffel".to_owned()]);
    assert!(
        verdict
            .fix
            .as_deref()
            .is_some_and(|fix| fix.contains("`[auto_merge] human_reviewers`")),
        "{verdict:?}"
    );
}

/// pulp#10219: armed on an approved head, then pushed; the queue lands the
/// new head. Judging that head finds no verdict naming it.
#[test]
fn a_head_no_verdict_names_is_refused_even_when_an_earlier_head_was_approved() {
    let gh = gh(
        NEWER,
        Some(SIGNED),
        vec![],
        vec![app_approval(HEAD, Some("pulp-review"))],
    );
    let verdict = review_verdict(&gh, "danielraffel/tartci", 7, None, false).expect("readable");
    assert!(!verdict.allowed(), "{verdict:?}");
    assert_eq!(verdict.head, NEWER);
    let at_old =
        review_verdict(&gh, "danielraffel/tartci", 7, Some(HEAD), false).expect("readable");
    assert!(at_old.allowed(), "{at_old:?}");
    assert_eq!(at_old.live_head.as_deref(), Some(NEWER));
}

#[test]
fn no_host_switch_turns_the_repository_side_rule_off() {
    let gh = gh(HEAD, Some(SIGNED), vec![owner_review(HEAD)], vec![]);
    let verdict = crate::head_approval::with_host_review_default(false, || {
        review_verdict(&gh, "danielraffel/tartci", 7, None, true)
    })
    .expect("readable");
    assert!(
        verdict.required,
        "a host opt-out never reaches the repository-side rule"
    );
    assert!(!verdict.allowed());
}

#[test]
fn a_repository_that_requires_no_review_is_allowed() {
    let gh = gh(HEAD, None, vec![], vec![]);
    let verdict = review_verdict(&gh, "someone/else", 7, None, true).expect("readable");
    assert!(verdict.allowed());
    assert!(!verdict.required);
}
