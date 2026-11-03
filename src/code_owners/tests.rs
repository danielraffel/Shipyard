use std::collections::BTreeSet;

use serde_json::json;

use super::{
    CodeOwnerStatus, CodeOwners, Review, exact_head_approvers, judge,
    rules_require_code_owner_review,
};

const HEAD: &str = "019f774a00000000000000000000000000000000";
const OLD: &str = "0000000000000000000000000000000000000001";

fn owners(codeowners: &CodeOwners, path: &str) -> Option<Vec<String>> {
    codeowners.owners_of(path).map(<[String]>::to_vec)
}

fn set(logins: &[&str]) -> BTreeSet<String> {
    logins.iter().map(|login| (*login).to_owned()).collect()
}

fn files(paths: &[&str]) -> Vec<String> {
    paths.iter().map(|path| (*path).to_owned()).collect()
}

fn review(login: &str, state: &str, commit: &str) -> Review {
    Review {
        login: login.to_owned(),
        state: state.to_owned(),
        commit_id: commit.to_owned(),
    }
}

#[test]
fn the_last_matching_rule_wins_and_an_ownerless_rule_unowns() {
    let codeowners = CodeOwners::parse(
        "# comment\n\
         *       @danielraffel\n\
         /docs/  @docs-owner   # trailing comment\n\
         /docs/generated/\n",
    );
    assert_eq!(
        owners(&codeowners, "src/main.rs"),
        Some(vec!["danielraffel".to_owned()])
    );
    assert_eq!(
        owners(&codeowners, "docs/guide/intro.md"),
        Some(vec!["docs-owner".to_owned()])
    );
    assert_eq!(owners(&codeowners, "docs/generated/api.md"), None);
}

#[test]
fn patterns_follow_github_semantics() {
    let codeowners = CodeOwners::parse(
        "*.js @js\n\
         apps/ @apps\n\
         /build/logs/ @logs\n\
         docs/* @docs\n\
         **/fixtures @fixtures\n\
         /scripts/**/run.sh @runner\n",
    );
    assert_eq!(
        owners(&codeowners, "web/app.js"),
        Some(vec!["js".to_owned()])
    );
    assert_eq!(
        owners(&codeowners, "a/apps/b/c.rs"),
        Some(vec!["apps".to_owned()])
    );
    // A trailing slash names a directory only.
    assert_eq!(owners(&codeowners, "apps"), None);
    assert_eq!(
        owners(&codeowners, "build/logs/x/y.log"),
        Some(vec!["logs".to_owned()])
    );
    assert_eq!(owners(&codeowners, "x/build/logs/y.log"), None);
    // `docs/*` owns direct children only, and is anchored by its slash.
    assert_eq!(
        owners(&codeowners, "docs/start.md"),
        Some(vec!["docs".to_owned()])
    );
    assert_eq!(owners(&codeowners, "docs/deep/start.md"), None);
    assert_eq!(owners(&codeowners, "lib/docs/start.md"), None);
    assert_eq!(
        owners(&codeowners, "a/b/fixtures/c.json"),
        Some(vec!["fixtures".to_owned()])
    );
    assert_eq!(
        owners(&codeowners, "scripts/run.sh"),
        Some(vec!["runner".to_owned()])
    );
    assert_eq!(
        owners(&codeowners, "scripts/a/b/run.sh"),
        Some(vec!["runner".to_owned()])
    );
}

#[test]
fn lines_github_rejects_are_skipped() {
    let codeowners = CodeOwners::parse("* @all\n!src/ @nobody\nsrc/[ab].rs @nobody\n");
    assert_eq!(
        owners(&codeowners, "src/a.rs"),
        Some(vec!["all".to_owned()])
    );
}

#[test]
fn only_the_latest_deciding_review_on_the_exact_head_approves() {
    let reviews = [
        review("Owner", "APPROVED", OLD),
        review("owner", "APPROVED", HEAD),
        review("owner", "COMMENTED", HEAD),
        review("stale", "APPROVED", OLD),
        review("flipper", "APPROVED", HEAD),
        review("flipper", "CHANGES_REQUESTED", HEAD),
        review("dismissed", "APPROVED", HEAD),
        review("dismissed", "DISMISSED", HEAD),
    ];
    assert_eq!(exact_head_approvers(&reviews, HEAD), set(&["owner"]));
}

#[test]
fn an_owned_file_without_its_owners_approval_is_missing() {
    let codeowners = CodeOwners::parse("* @danielraffel\n");
    let status = judge(
        &codeowners,
        &files(&["src/lib.rs"]),
        &set(&["pulp-review"]),
        Some("app/shipyard-local"),
    );
    assert_eq!(
        status,
        CodeOwnerStatus::Missing {
            owners: vec!["danielraffel".to_owned()],
            files: vec!["src/lib.rs".to_owned()],
        }
    );
}

#[test]
fn an_owners_exact_head_approval_satisfies_the_rule() {
    let codeowners = CodeOwners::parse("* @DanielRaffel\n");
    let status = judge(
        &codeowners,
        &files(&["src/lib.rs"]),
        &set(&["danielraffel"]),
        Some("app/shipyard-local"),
    );
    assert_eq!(
        status,
        CodeOwnerStatus::Satisfied {
            approvers: vec!["danielraffel".to_owned()]
        }
    );
}

#[test]
fn unowned_files_require_nothing() {
    let codeowners = CodeOwners::parse("/docs/ @docs\n");
    assert_eq!(
        judge(&codeowners, &files(&["src/lib.rs"]), &set(&[]), None),
        CodeOwnerStatus::NotRequired
    );
}

#[test]
fn what_only_github_can_judge_is_undetermined() {
    let team = CodeOwners::parse("* @org/team\n");
    assert!(matches!(
        judge(&team, &files(&["a"]), &set(&[]), None),
        CodeOwnerStatus::Undetermined(_)
    ));
    let authored = CodeOwners::parse("* @danielraffel\n");
    assert!(matches!(
        judge(&authored, &files(&["a"]), &set(&[]), Some("danielraffel")),
        CodeOwnerStatus::Undetermined(_)
    ));
    // A file only a listed user can approve still waits, whatever GitHub
    // decides about the team-owned one.
    let mixed = CodeOwners::parse("* @org/team\n/src/ @danielraffel\n");
    assert!(matches!(
        judge(&mixed, &files(&["README.md", "src/a.rs"]), &set(&[]), None),
        CodeOwnerStatus::Missing { .. }
    ));
}

#[test]
fn only_a_pull_request_rule_with_code_owner_review_requires_it() {
    // vellum: zero required approvals, code-owner review required.
    let vellum = json!([[
        {"type": "required_status_checks", "parameters": {}},
        {"type": "pull_request", "parameters": {
            "required_approving_review_count": 0,
            "require_code_owner_review": true
        }}
    ]]);
    assert!(rules_require_code_owner_review(&vellum));
    assert!(!super::rules_dismiss_stale_reviews(&vellum));
    let strict = json!([[{"type": "pull_request", "parameters": {
        "require_code_owner_review": true,
        "dismiss_stale_reviews_on_push": true
    }}]]);
    assert!(super::rules_dismiss_stale_reviews(&strict));
    let plain = json!([{"type": "pull_request", "parameters": {
        "required_approving_review_count": 1,
        "require_code_owner_review": false
    }}]);
    assert!(!rules_require_code_owner_review(&plain));
    assert!(!rules_require_code_owner_review(&json!([[]])));
}

/// A `gh` for `read_status` over the vellum shape, whose CODEOWNERS errors
/// read as `errors`.
fn read_with_errors(errors: &str) -> CodeOwnerStatus {
    let run_gh = |argv: &[String]| -> Result<String, String> {
        let joined = argv.join(" ");
        if joined.contains("rules/branches/main") {
            Ok(r#"[[{"type":"pull_request","parameters":{"required_approving_review_count":0,"require_code_owner_review":true}}]]"#.to_owned())
        } else if joined.contains("contents/.github/CODEOWNERS") {
            Ok("* @danielraffel\n".to_owned())
        } else if joined.contains("pulls/7/files") {
            Ok("src/lib.rs\n".to_owned())
        } else if joined.contains("pulls/7/reviews") {
            Ok(format!("pulp-review[bot]\tAPPROVED\t{HEAD}\n"))
        } else if joined.contains("codeowners/errors") {
            Ok(errors.to_owned())
        } else {
            Err(format!("unexpected: {joined}"))
        }
    };
    super::read_status(&run_gh, "Generous-Corp/vellum", "main", 7, HEAD, None).expect("status")
}

#[test]
fn a_head_is_held_only_when_github_reports_codeowners_clean() {
    assert_eq!(
        read_with_errors(r#"{"errors":[]}"#),
        CodeOwnerStatus::Missing {
            owners: vec!["danielraffel".to_owned()],
            files: vec!["src/lib.rs".to_owned()],
        }
    );
    // GitHub skips an owner it rejects; its verdict may then differ.
    assert!(matches!(
        read_with_errors(r#"{"errors":[{"line":1,"kind":"Unknown owner"}]}"#),
        CodeOwnerStatus::Undetermined(_)
    ));
}
