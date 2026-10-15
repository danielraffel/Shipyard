use serde_json::{Value, json};

use super::review_verdict;

const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const NEWER: &str = "cccccccccccccccccccccccccccccccccccccccc";

/// A fake `gh` for `repo#7`: the pull request at `live`, no config file, and
/// these reviews and comments.
fn gh(
    live: &'static str,
    reviews: Vec<Value>,
    comments: Vec<Value>,
) -> impl Fn(&[String]) -> Result<String, String> {
    move |args: &[String]| {
        let joined = args.join(" ");
        if joined.contains("contents/.shipyard/config.toml") {
            return Err("gh: Not Found (HTTP 404)".to_owned());
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

fn app_approval(head: &str) -> Value {
    json!({"id": 5, "login": "shipyard-local[bot]", "type": "Bot", "association": "CONTRIBUTOR",
           "at": "2026-10-14T17:00:00Z", "body": format!("Reviewed.\nverdict: APPROVE head:{head}")})
}

fn owner_review(head: &str) -> Value {
    json!({"login": "danielraffel", "type": "User", "association": "OWNER",
           "at": "2026-10-14T17:50:05Z", "state": "APPROVED", "commit_id": head})
}

#[test]
fn the_fleet_apps_approval_of_the_head_allows_it() {
    let gh = gh(HEAD, vec![], vec![app_approval(HEAD)]);
    let verdict = review_verdict(&gh, "danielraffel/tartci", 7, None, true).expect("readable");
    assert!(verdict.allowed(), "{verdict:?}");
    assert!(verdict.required);
    assert!(verdict.approval.is_some());
    assert_eq!(verdict.head, HEAD);
    assert!(verdict.fix.is_none());
    let rendered = serde_json::to_value(&verdict).expect("json");
    assert_eq!(rendered["verdict"], "allowed");
}

/// pulp#10293: only the owner's personal login approved the head.
#[test]
fn an_unlisted_humans_review_is_refused_and_named() {
    let gh = gh(HEAD, vec![owner_review(HEAD)], vec![]);
    let verdict = review_verdict(&gh, "danielraffel/tartci", 7, None, true).expect("readable");
    assert!(!verdict.allowed(), "{verdict:?}");
    assert_eq!(verdict.excluded_approvals, vec!["danielraffel".to_owned()]);
    assert!(
        verdict.reason.contains("carries no approval"),
        "{}",
        verdict.reason
    );
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
    let gh = gh(NEWER, vec![], vec![app_approval(HEAD)]);
    let verdict = review_verdict(&gh, "danielraffel/tartci", 7, None, false).expect("readable");
    assert!(!verdict.allowed(), "{verdict:?}");
    assert_eq!(verdict.head, NEWER);
    // The same pull request judged at the approved head is allowed, and the
    // answer says the pull request has moved on.
    let at_old =
        review_verdict(&gh, "danielraffel/tartci", 7, Some(HEAD), false).expect("readable");
    assert!(at_old.allowed(), "{at_old:?}");
    assert_eq!(at_old.live_head.as_deref(), Some(NEWER));
}

#[test]
fn no_host_switch_turns_the_repository_side_rule_off() {
    let gh = gh(HEAD, vec![owner_review(HEAD)], vec![]);
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
    let gh = gh(HEAD, vec![], vec![]);
    let verdict = review_verdict(&gh, "someone/else", 7, None, true).expect("readable");
    assert!(verdict.allowed());
    assert!(!verdict.required);
}
