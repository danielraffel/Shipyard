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

/// The stamp `ghapp` adds on the reviewer's host.
const STAMP: &str = "<!-- session: Daniels-Mac-Studio-m3/review-1 -->";

fn app_approval(head: &str, signer: Option<&str>) -> Value {
    let line = signer.map_or_else(String::new, |name| format!("reviewer: {name}\n"));
    app_comment(&format!(
        "Reviewed.\n{line}{STAMP}\nverdict: APPROVE head:{head}"
    ))
}

fn app_comment(body: &str) -> Value {
    json!({"id": 5, "login": "shipyard-local[bot]", "type": "Bot", "association": "CONTRIBUTOR",
           "at": "2026-10-14T17:00:00Z", "body": body})
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

/// tartci#623's deciding APPROVE (comment 6321450475) with its
/// `Daniels-M5-Studio/unattributed` stamp, as posted, and with the stamp
/// removed, as it would be posted around `ghapp`'s stamper: the signer and
/// session lines were copied from infra-review2, so neither may count.
#[test]
fn tartci_623s_copied_approve_counts_neither_stamped_unattributed_nor_unstamped() {
    let copied = |stamp: &str| {
        format!(
            "Exact-head re-review completed.\n\nreviewer: infra-review2\n\
             session: infra-review2 (team-lead team, agent infra-review2, host m3)\n\
             {stamp}verdict: APPROVE head:{HEAD}"
        )
    };
    for (body, why) in [
        (
            copied("<!-- session: Daniels-M5-Studio/unattributed -->\n"),
            "its session stamp is unattributed",
        ),
        (copied(""), "it has no `ghapp` session stamp"),
    ] {
        let verdict = judged(Some(SIGNED), vec![app_comment(&body)]);
        assert!(!verdict.allowed(), "{why}: {verdict:?}");
        assert!(
            verdict
                .audit_events
                .contains(&format!("excluded_app_approval:{why}")),
            "{why}: {verdict:?}"
        );
    }
    // Control: the same body with a stamp from a named session counts.
    let stamped = copied(&format!("{STAMP}\n"));
    assert!(judged(Some(SIGNED), vec![app_comment(&stamped)]).allowed());
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

/// pulp#11117 through `shipyard review-verdict` (the repository-side rule):
/// a signed HOLD, then a later signed APPROVE of the same unchanged head from
/// another session. The head stays refused, the excluded APPROVE is named, and
/// the fix asks for a new head rather than another APPROVE.
#[test]
fn a_hold_then_a_later_approve_of_the_same_head_is_refused() {
    let mut hold = app_comment(&format!(
        "Hold.\nreviewer: pulp-review\n<!-- session: Daniels-Mac-Studio-m3/interactive -->\n\
         verdict: HOLD head:{HEAD}"
    ));
    hold["id"] = json!(4);
    hold["at"] = json!("2026-10-14T16:30:00Z");
    let approve = app_approval(HEAD, Some("pulp-review"));
    // Control: the APPROVE alone is allowed.
    assert!(judged(Some(SIGNED), vec![approve.clone()]).allowed());
    let verdict = judged(Some(SIGNED), vec![hold, approve]);
    assert!(!verdict.allowed(), "{verdict:?}");
    assert!(
        verdict
            .audit_events
            .contains(&"excluded_approval:head held by pulp-review at comment 4".to_owned()),
        "{verdict:?}"
    );
    let fix = verdict.fix.as_deref().expect("a refusal names its fix");
    assert!(fix.contains("pushes a new head"), "{fix}");
}
