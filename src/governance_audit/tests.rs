use std::cell::RefCell;
use std::collections::BTreeMap;

use serde_json::json;

use super::*;

const FIXTURES: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/governance_audit"
);

fn compliant() -> RepoSnapshot {
    let text = std::fs::read_to_string(format!("{FIXTURES}/compliant.json"))
        .expect("compliant fixture is readable");
    serde_json::from_str(&text).expect("compliant fixture parses")
}

fn audit_row(snapshot: &RepoSnapshot, policy: &AuditPolicy, setting: Setting) -> AuditRow {
    audit_snapshot(snapshot, policy)
        .row(setting)
        .cloned()
        .expect("every setting has a row")
}

fn assert_status(
    snapshot: &RepoSnapshot,
    policy: &AuditPolicy,
    setting: Setting,
    want: AuditStatus,
) {
    let row = audit_row(snapshot, policy, setting);
    assert_eq!(row.status, want, "{setting:?}: {}", row.detail);
}

fn assert_drift(snapshot: &RepoSnapshot, setting: Setting, needle: &str) {
    assert_drift_with(snapshot, &AuditPolicy::default(), setting, needle);
}

fn assert_drift_with(
    snapshot: &RepoSnapshot,
    policy: &AuditPolicy,
    setting: Setting,
    needle: &str,
) {
    let report = audit_snapshot(snapshot, policy);
    let row = report.row(setting).expect("row");
    assert_eq!(
        row.status,
        AuditStatus::Drift,
        "{setting:?}: {}",
        row.detail
    );
    assert!(
        row.detail.contains(needle),
        "{setting:?} detail {:?} should mention {needle:?}",
        row.detail
    );
    assert!(report.has_drift());
}

fn workflow_mut<'a>(snapshot: &'a mut RepoSnapshot, name: &str) -> &'a mut WorkflowFile {
    snapshot
        .workflows
        .iter_mut()
        .find(|workflow| workflow.path.ends_with(name))
        .expect("fixture workflow")
}

fn replace_in(snapshot: &mut RepoSnapshot, name: &str, from: &str, to: &str) {
    let workflow = workflow_mut(snapshot, name);
    assert!(
        workflow.content.contains(from),
        "{name} should contain {from:?}"
    );
    workflow.content = workflow.content.replace(from, to);
}

fn replace_in_dependabot(snapshot: &mut RepoSnapshot, from: &str, to: &str) {
    let text = snapshot.dependabot.take().expect("fixture dependabot");
    assert!(text.contains(from), "dependabot should contain {from:?}");
    snapshot.dependabot = Some(text.replace(from, to));
}

#[test]
fn compliant_fixture_passes_every_row() {
    let report = audit_snapshot(&compliant(), &AuditPolicy::default());
    assert_eq!(report.rows.len(), Setting::ALL.len());
    for row in &report.rows {
        assert_eq!(
            row.status,
            AuditStatus::Pass,
            "{:?}: {}",
            row.setting,
            row.detail
        );
    }
    assert!(!report.has_drift());
    assert!(!report.has_unknown());
}

#[test]
fn row_keys_are_unique_kebab_names() {
    let keys: Vec<&str> = Setting::ALL.iter().map(|setting| setting.key()).collect();
    let mut unique = keys.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), keys.len());
    assert!(
        keys.iter()
            .all(|key| key.chars().all(|c| c.is_ascii_lowercase() || c == '-'))
    );
}

#[test]
fn classify_uses_distinguishes_every_ref_kind() {
    let sha = "actions/checkout@0123456789abcdef0123456789abcdef01234567";
    assert_eq!(classify_uses(sha, Some("# v7.0.1")), RefKind::Sha);
    assert_eq!(classify_uses(sha, Some("#v7")), RefKind::Sha);
    assert_eq!(classify_uses(sha, None), RefKind::BareSha);
    assert_eq!(classify_uses(sha, Some("# pinned")), RefKind::BareSha);
    assert_eq!(
        classify_uses("actions/checkout@v7.0.1", None),
        RefKind::ExactTag
    );
    assert_eq!(
        classify_uses("actions/checkout@v7", None),
        RefKind::Floating
    );
    assert_eq!(
        classify_uses("actions/checkout@v7.0", None),
        RefKind::Floating
    );
    assert_eq!(
        classify_uses("actions/checkout@main", None),
        RefKind::Floating
    );
    assert_eq!(classify_uses("actions/checkout", None), RefKind::Floating);
    assert_eq!(
        classify_uses("org/repo/.github/workflows/x.yml@v1", None),
        RefKind::Floating
    );
    assert_eq!(
        classify_uses("./.github/actions/setup", None),
        RefKind::Local
    );
    assert_eq!(classify_uses("docker://alpine:3", None), RefKind::Local);
}

#[test]
fn action_refs_skip_comments_and_read_trailing_version_comments() {
    let refs = action_refs(&compliant().workflows);
    // ci.yml: checkout, rust-toolchain, local; lockfile.yml + zizmor.yml: checkout.
    assert_eq!(refs.len(), 5);
    assert!(refs.iter().all(|item| !item.uses.contains("cache")));
    assert!(refs.iter().any(|item| item.kind == RefKind::Sha));
}

#[test]
fn floating_ref_is_exact_refs_drift() {
    let mut snapshot = compliant();
    workflow_mut(&mut snapshot, "ci.yml")
        .content
        .push_str("      - uses: actions/cache@v4\n");
    assert_drift(&snapshot, Setting::ExactRefs, "actions/cache@v4");
}

#[test]
fn bare_sha_without_version_comment_is_exact_refs_drift() {
    let mut snapshot = compliant();
    replace_in(&mut snapshot, "ci.yml", " # v1.0.0", "");
    assert_drift(&snapshot, Setting::ExactRefs, "1 bare SHA");
}

#[test]
fn missing_lockfile_is_drift() {
    let mut snapshot = compliant();
    snapshot.lockfile = None;
    assert_drift(&snapshot, Setting::Lockfile, "actions.lock");
    // Exact tags stay exact without the lockfile: that row is separate.
    assert_status(
        &snapshot,
        &AuditPolicy::default(),
        Setting::ExactRefs,
        AuditStatus::Pass,
    );
}

#[test]
fn lockfile_without_verify_job_is_drift() {
    let mut snapshot = compliant();
    snapshot
        .workflows
        .retain(|workflow| !workflow.path.ends_with("lockfile.yml"));
    assert_drift(&snapshot, Setting::Lockfile, "--verify");
}

#[test]
fn verify_local_alone_is_not_a_verify_job() {
    let mut snapshot = compliant();
    replace_in(
        &mut snapshot,
        "lockfile.yml",
        "gh actions-lock --verify",
        "gh actions-lock --verify-local",
    );
    assert_drift(&snapshot, Setting::Lockfile, "--verify");
}

#[test]
fn extension_pinned_by_tag_is_lockfile_drift() {
    let mut snapshot = compliant();
    replace_in(
        &mut snapshot,
        "lockfile.yml",
        "--pin 0123456789abcdef0123456789abcdef01234567",
        "--pin v0.4.1",
    );
    assert_drift(&snapshot, Setting::Lockfile, "`v0.4.1`");
}

#[test]
fn unpinned_extension_is_lockfile_drift() {
    let mut snapshot = compliant();
    replace_in(
        &mut snapshot,
        "lockfile.yml",
        " --pin 0123456789abcdef0123456789abcdef01234567",
        "",
    );
    assert_drift(&snapshot, Setting::Lockfile, "--pin <sha>");
}

#[test]
fn required_mode_needs_the_lockfile_context() {
    let policy = AuditPolicy {
        lockfile_mode: LockfileMode::Required,
        ..AuditPolicy::default()
    };
    let mut snapshot = compliant();
    assert_drift_with(&snapshot, &policy, Setting::Lockfile, "NOT required");
    snapshot
        .required_contexts
        .as_mut()
        .expect("fixture contexts")
        .push("lockfile".to_owned());
    assert_status(&snapshot, &policy, Setting::Lockfile, AuditStatus::Pass);
    snapshot.required_contexts = None;
    assert_status(&snapshot, &policy, Setting::Lockfile, AuditStatus::Unknown);
}

#[test]
fn missing_dependabot_is_drift_and_names_a_renovate_config() {
    let mut snapshot = compliant();
    snapshot.dependabot = None;
    assert_drift(&snapshot, Setting::UpdatesBot, "no .github/dependabot.yml");
    snapshot.renovate = Some(WorkflowFile {
        path: "renovate.json".to_owned(),
        content: r#"{"minimumReleaseAge": "7 days"}"#.to_owned(),
    });
    assert_drift(&snapshot, Setting::UpdatesBot, "renovate.json present");
}

#[test]
fn dependabot_without_github_actions_is_drift() {
    let mut snapshot = compliant();
    snapshot.dependabot =
        Some("version: 2\nupdates:\n  - package-ecosystem: cargo\n    directory: /\n".to_owned());
    assert_drift(&snapshot, Setting::UpdatesBot, "no github-actions entry");
}

#[test]
fn missing_cooldown_is_drift() {
    let mut snapshot = compliant();
    replace_in_dependabot(&mut snapshot, "    cooldown:\n      default-days: 7\n", "");
    assert_drift(&snapshot, Setting::UpdatesBot, "no cooldown");
}

#[test]
fn short_cooldown_is_drift() {
    let mut snapshot = compliant();
    replace_in_dependabot(&mut snapshot, "default-days: 7", "default-days: 3");
    assert_drift(&snapshot, Setting::UpdatesBot, "cooldown 3d");
}

#[test]
fn non_weekly_interval_is_drift_and_other_ecosystems_do_not_leak() {
    let mut snapshot = compliant();
    // The cargo entry (daily, limit 5) sits before the actions entry.
    assert_status(
        &snapshot,
        &AuditPolicy::default(),
        Setting::UpdatesBot,
        AuditStatus::Pass,
    );
    replace_in_dependabot(&mut snapshot, "interval: weekly", "interval: daily");
    assert_drift(&snapshot, Setting::UpdatesBot, "interval daily");
}

#[test]
fn ungrouped_updates_are_drift() {
    let mut snapshot = compliant();
    replace_in_dependabot(
        &mut snapshot,
        "    groups:\n      github-actions:\n        patterns:\n          - \"*\"\n",
        "",
    );
    assert_drift(&snapshot, Setting::UpdatesBot, "no group");
}

#[test]
fn pr_limit_above_one_or_absent_is_drift() {
    let mut snapshot = compliant();
    replace_in_dependabot(
        &mut snapshot,
        "    open-pull-requests-limit: 1\n    cooldown",
        "    open-pull-requests-limit: 2\n    cooldown",
    );
    assert_drift(&snapshot, Setting::UpdatesBot, "limit 2");
    replace_in_dependabot(
        &mut snapshot,
        "    open-pull-requests-limit: 2\n    cooldown",
        "    cooldown",
    );
    // Dependabot's default limit is 5.
    assert_drift(&snapshot, Setting::UpdatesBot, "limit 5");
}

#[test]
fn renovate_is_one_config_value_with_the_same_semantics() {
    let config: Table =
        toml::from_str("[project]\nprofile = \"solo\"\n[governance.updates]\nbot = \"renovate\"\n")
            .expect("toml");
    let policy = AuditPolicy::from_config(&config).expect("policy");
    assert_eq!(policy.updates_bot, UpdatesBot::Renovate);
    assert_eq!(policy.cooldown_days, 7);
    assert_eq!(policy.open_pull_requests_limit, 1);

    let mut snapshot = compliant();
    assert_drift_with(
        &snapshot,
        &policy,
        Setting::UpdatesBot,
        "dependabot.yml present",
    );
    snapshot.renovate = Some(WorkflowFile {
        path: ".github/renovate.json".to_owned(),
        content: r#"{"schedule": ["weekly"], "packageRules": [{"matchManagers": ["github-actions"], "groupName": "actions"}], "minimumReleaseAge": "7 days"}"#.to_owned(),
    });
    assert_status(&snapshot, &policy, Setting::UpdatesBot, AuditStatus::Pass);
    let renovate = snapshot.renovate.as_mut().expect("renovate");
    renovate.content = renovate.content.replace("weekly", "before 4am on monday");
    assert_status(&snapshot, &policy, Setting::UpdatesBot, AuditStatus::Pass);
    let renovate = snapshot.renovate.as_mut().expect("renovate");
    renovate.content = renovate.content.replace("7 days", "3 days");
    assert_drift_with(&snapshot, &policy, Setting::UpdatesBot, "3 days");
    let renovate = snapshot.renovate.as_mut().expect("renovate");
    renovate.content = r#"{"schedule": ["weekly"], "minimumReleaseAge": "7 days"}"#.to_owned();
    assert_drift_with(&snapshot, &policy, Setting::UpdatesBot, "no groupName");
}

#[test]
fn renovate_requires_a_weekly_schedule() {
    let policy = AuditPolicy {
        updates_bot: UpdatesBot::Renovate,
        ..AuditPolicy::default()
    };
    let mut snapshot = compliant();
    snapshot.renovate = Some(WorkflowFile {
        path: "renovate.json".to_owned(),
        content: r#"{"packageRules": [{"matchManagers": ["github-actions"], "groupName": "actions"}], "minimumReleaseAge": "7 days"}"#.to_owned(),
    });
    assert_drift_with(&snapshot, &policy, Setting::UpdatesBot, "schedule");

    let renovate = snapshot.renovate.as_mut().expect("renovate");
    renovate.content =
        r#"{"schedule": ["daily"], "packageRules": [{"matchManagers": ["github-actions"], "groupName": "actions"}], "minimumReleaseAge": "7 days"}"#.to_owned();
    assert_drift_with(&snapshot, &policy, Setting::UpdatesBot, "schedule");
}

#[test]
fn policy_config_overrides_and_rejects_bad_values() {
    let config: Table = toml::from_str(
        "[project]\nprofile = \"multi\"\n[governance.updates]\ncooldown_days = 10\n[governance.lockfile]\nmode = \"required\"\ncontext = \"verify-lock\"\n",
    )
    .expect("toml");
    let policy = AuditPolicy::from_config(&config).expect("policy");
    assert_eq!(policy.profile, "multi");
    assert_eq!(policy.updates_bot, UpdatesBot::Dependabot);
    assert_eq!(policy.cooldown_days, 10);
    assert_eq!(policy.lockfile_mode, LockfileMode::Required);
    assert_eq!(policy.lockfile_context, "verify-lock");

    for bad in [
        "[governance.updates]\nbot = \"snyk\"\n",
        "[governance.lockfile]\nmode = \"strict\"\n",
        "[governance.updates]\ncooldown_days = -1\n",
    ] {
        let config: Table = toml::from_str(bad).expect("toml");
        assert!(AuditPolicy::from_config(&config).is_err(), "{bad}");
    }
    assert_eq!(
        AuditPolicy::from_config(&Table::new()).expect("empty"),
        AuditPolicy::default()
    );
}

#[test]
fn missing_zizmor_drifts_both_rows_and_a_comment_does_not_count() {
    let mut snapshot = compliant();
    snapshot
        .workflows
        .retain(|workflow| !workflow.path.ends_with("zizmor.yml"));
    assert_drift(&snapshot, Setting::ZizmorReport, "zizmor");
    assert_drift(&snapshot, Setting::ZizmorEnforce, "zizmor");
    workflow_mut(&mut snapshot, "ci.yml")
        .content
        .push_str("# TODO: add zizmor\n");
    assert_drift(&snapshot, Setting::ZizmorReport, "zizmor");
}

#[test]
fn report_only_zizmor_passes_report_and_drifts_enforce() {
    let mut snapshot = compliant();
    replace_in(
        &mut snapshot,
        "zizmor.yml",
        "--min-severity high --min-confidence high",
        "--no-exit-codes",
    );
    assert_status(
        &snapshot,
        &AuditPolicy::default(),
        Setting::ZizmorReport,
        AuditStatus::Pass,
    );
    assert_drift(&snapshot, Setting::ZizmorEnforce, "report mode only");

    // A severity floor with exit codes switched off is still report mode.
    let mut floor_only = compliant();
    replace_in(
        &mut floor_only,
        "zizmor.yml",
        "--min-confidence high",
        "--min-confidence high --no-exit-codes",
    );
    assert_drift(&floor_only, Setting::ZizmorEnforce, "report mode only");

    // Exit codes on but no severity floor is not the profile's enforce mode.
    let mut no_floor = compliant();
    replace_in(
        &mut no_floor,
        "zizmor.yml",
        " --min-severity high --min-confidence high",
        "",
    );
    assert_drift(&no_floor, Setting::ZizmorEnforce, "report mode only");
}

#[test]
fn zizmor_enforce_requires_high_severity_and_confidence() {
    let mut low_severity = compliant();
    replace_in(
        &mut low_severity,
        "zizmor.yml",
        "--min-severity high",
        "--min-severity medium",
    );
    assert_status(
        &low_severity,
        &AuditPolicy::default(),
        Setting::ZizmorEnforce,
        AuditStatus::Drift,
    );

    let mut low_confidence = compliant();
    replace_in(
        &mut low_confidence,
        "zizmor.yml",
        "--min-confidence high",
        "--min-confidence medium",
    );
    assert_status(
        &low_confidence,
        &AuditPolicy::default(),
        Setting::ZizmorEnforce,
        AuditStatus::Drift,
    );
}

#[test]
fn workflow_without_top_level_permissions_is_drift() {
    let mut snapshot = compliant();
    // A job-level block does not count as top-level.
    replace_in(
        &mut snapshot,
        "zizmor.yml",
        "permissions: {}\njobs:",
        "jobs:",
    );
    assert_drift(&snapshot, Setting::WorkflowPermissions, "zizmor.yml");
    assert_drift(&snapshot, Setting::PermissionsEmpty, "2 of 3");
}

#[test]
fn non_empty_top_level_permissions_is_permissions_empty_drift() {
    let mut snapshot = compliant();
    replace_in(
        &mut snapshot,
        "ci.yml",
        "permissions: {}\njobs:",
        "permissions:\n  contents: read\njobs:",
    );
    assert_status(
        &snapshot,
        &AuditPolicy::default(),
        Setting::WorkflowPermissions,
        AuditStatus::Pass,
    );
    assert_drift(&snapshot, Setting::PermissionsEmpty, "2 of 3");
}

#[test]
fn untrusted_triggers_are_drift_but_job_expressions_are_not() {
    let snapshot = compliant();
    // ci.yml mentions workflow_run in a job `if:`, zizmor.yml in a comment.
    assert_status(
        &snapshot,
        &AuditPolicy::default(),
        Setting::UntrustedTriggers,
        AuditStatus::Pass,
    );

    let mut target = compliant();
    replace_in(
        &mut target,
        "lockfile.yml",
        "on:\n  pull_request:\n",
        "on:\n  pull_request_target:\n",
    );
    assert_drift(
        &target,
        Setting::UntrustedTriggers,
        "lockfile.yml (pull_request_target)",
    );

    let mut run = compliant();
    replace_in(
        &mut run,
        "ci.yml",
        "on: [pull_request, push]",
        "on:\n  workflow_run:\n    workflows: [Build]",
    );
    assert_drift(&run, Setting::UntrustedTriggers, "ci.yml (workflow_run)");
    assert_drift(
        &run,
        Setting::UntrustedTriggers,
        "per-workflow event policy",
    );
    let multi = AuditPolicy::for_profile("multi");
    let row = audit_row(&run, &multi, Setting::UntrustedTriggers);
    assert_eq!(row.status, AuditStatus::Drift);
    assert!(!row.detail.contains("event policy"));
}

#[test]
fn tag_protection_drift_kinds() {
    let ruleset = |enforcement: &str, include: &[&str], rules: &[&str]| TagRuleset {
        name: "tags".to_owned(),
        enforcement: enforcement.to_owned(),
        include: include.iter().map(|item| (*item).to_owned()).collect(),
        rules: rules.iter().map(|item| (*item).to_owned()).collect(),
    };
    let all = ["update", "deletion", "non_fast_forward"];
    let mut snapshot = compliant();

    snapshot.tag_rulesets = Some(Vec::new());
    assert_drift(
        &snapshot,
        Setting::TagProtection,
        "no active ruleset on refs/tags/v*",
    );

    snapshot.tag_rulesets = Some(vec![ruleset("evaluate", &["refs/tags/v*"], &all)]);
    assert_drift(&snapshot, Setting::TagProtection, "no active ruleset");

    snapshot.tag_rulesets = Some(vec![ruleset("active", &["refs/tags/nightly-*"], &all)]);
    assert_drift(
        &snapshot,
        Setting::TagProtection,
        "no active ruleset on refs/tags/v*",
    );

    snapshot.tag_rulesets = Some(vec![ruleset("active", &["~ALL"], &all)]);
    assert_status(
        &snapshot,
        &AuditPolicy::default(),
        Setting::TagProtection,
        AuditStatus::Pass,
    );

    snapshot.tag_rulesets = Some(vec![ruleset(
        "active",
        &["refs/tags/v*"],
        &["update", "non_fast_forward"],
    )]);
    assert_drift(&snapshot, Setting::TagProtection, "missing deletion");

    let multi = AuditPolicy::for_profile("multi");
    assert_drift_with(
        &compliant(),
        &multi,
        Setting::TagProtection,
        "missing creation",
    );

    snapshot.tag_rulesets = None;
    assert_status(
        &snapshot,
        &AuditPolicy::default(),
        Setting::TagProtection,
        AuditStatus::Unknown,
    );
}

#[test]
fn immutable_releases_off_is_drift_and_unread_is_unknown() {
    let mut snapshot = compliant();
    snapshot.immutable_releases = Some(false);
    assert_drift(&snapshot, Setting::ImmutableReleases, "disabled");
    snapshot.immutable_releases = None;
    assert_status(
        &snapshot,
        &AuditPolicy::default(),
        Setting::ImmutableReleases,
        AuditStatus::Unknown,
    );
}

#[test]
fn write_default_token_is_drift_and_unread_is_unknown() {
    let mut snapshot = compliant();
    snapshot.default_workflow_permissions = Some("write".to_owned());
    assert_drift(&snapshot, Setting::DefaultToken, "write");
    snapshot.default_workflow_permissions = None;
    assert_status(
        &snapshot,
        &AuditPolicy::default(),
        Setting::DefaultToken,
        AuditStatus::Unknown,
    );
}

#[test]
fn unreadable_workflows_are_unknown_not_pass_or_drift() {
    let mut snapshot = compliant();
    snapshot
        .read_errors
        .insert("workflows".to_owned(), "HTTP 403".to_owned());
    let report = audit_snapshot(&snapshot, &AuditPolicy::default());
    for setting in Setting::ALL {
        let want = if setting.reads_workflows() {
            AuditStatus::Unknown
        } else {
            AuditStatus::Pass
        };
        assert_eq!(
            report.row(setting).expect("row").status,
            want,
            "{setting:?}"
        );
    }
    assert!(report.has_unknown());
}

/// A GET-only fake keyed by path. Unlisted paths answer 404; listed errors fail.
struct FakeReader {
    json: BTreeMap<String, serde_json::Value>,
    raw: BTreeMap<String, String>,
    errors: BTreeMap<String, String>,
    calls: RefCell<Vec<String>>,
}

impl FakeReader {
    fn answer<T: Clone>(&self, map: &BTreeMap<String, T>, path: &str) -> Result<Option<T>, String> {
        self.calls.borrow_mut().push(path.to_owned());
        if let Some(error) = self.errors.get(path) {
            return Err(error.clone());
        }
        Ok(map.get(path).cloned())
    }
}

impl GithubReader for FakeReader {
    fn get_json(&self, path: &str) -> Result<Option<JsonValue>, String> {
        self.answer(&self.json, path)
    }

    fn get_raw(&self, path: &str) -> Result<Option<String>, String> {
        self.answer(&self.raw, path)
    }
}

fn reader_for(expected: &RepoSnapshot, repo: &str) -> FakeReader {
    let contents = |path: &str| format!("repos/{repo}/contents/{path}?ref=trunk");
    let mut json = BTreeMap::new();
    let mut raw = BTreeMap::new();
    json.insert(format!("repos/{repo}"), json!({"default_branch": "trunk"}));
    let mut listing = vec![json!({"name": "actions.lock", "path": LOCKFILE_PATH})];
    for workflow in &expected.workflows {
        listing.push(json!({"name": file_name(&workflow.path), "path": workflow.path}));
        raw.insert(contents(&workflow.path), workflow.content.clone());
    }
    listing.push(json!({"name": "README.md", "path": ".github/workflows/README.md"}));
    json.insert(contents(".github/workflows"), json!(listing));
    raw.insert(
        contents(LOCKFILE_PATH),
        expected.lockfile.clone().expect("lockfile"),
    );
    raw.insert(
        contents(".github/dependabot.yml"),
        expected.dependabot.clone().expect("dependabot"),
    );
    json.insert(
        format!("repos/{repo}/rules/branches/trunk"),
        json!([{"type": "required_status_checks", "parameters": {"required_status_checks": [{"context": "test"}]}}]),
    );
    json.insert(
        format!("repos/{repo}/rulesets?targets=tag&includes_parents=true&per_page=100"),
        json!([
            {"id": 7, "name": "protect-release-tags", "target": "tag", "enforcement": "active"},
            {"id": 8, "name": "main", "target": "branch", "enforcement": "active"}
        ]),
    );
    json.insert(
        format!("repos/{repo}/rulesets/7"),
        json!({
            "conditions": {"ref_name": {"include": ["refs/tags/v*"], "exclude": []}},
            "rules": [{"type": "update"}, {"type": "deletion"}, {"type": "non_fast_forward"}]
        }),
    );
    json.insert(
        format!("repos/{repo}/immutable-releases"),
        json!({"enabled": true, "enforced_by_owner": false}),
    );
    json.insert(
        format!("repos/{repo}/actions/permissions/workflow"),
        json!({"default_workflow_permissions": "read"}),
    );
    FakeReader {
        json,
        raw,
        errors: BTreeMap::new(),
        calls: RefCell::new(Vec::new()),
    }
}

#[test]
fn fetch_builds_the_snapshot_the_fixture_describes() {
    let expected = compliant();
    let repo = "example/compliant";
    let reader = reader_for(&expected, repo);
    let snapshot = fetch_snapshot(repo, &reader);
    assert_eq!(snapshot.default_branch, "trunk");
    assert_eq!(snapshot.workflows, expected.workflows);
    assert_eq!(snapshot.lockfile, expected.lockfile);
    assert_eq!(snapshot.dependabot, expected.dependabot);
    assert_eq!(snapshot.renovate, None);
    assert_eq!(snapshot.required_contexts, Some(vec!["test".to_owned()]));
    assert_eq!(snapshot.tag_rulesets, expected.tag_rulesets);
    assert_eq!(snapshot.immutable_releases, Some(true));
    assert_eq!(
        snapshot.default_workflow_permissions.as_deref(),
        Some("read")
    );
    assert!(
        snapshot.read_errors.is_empty(),
        "{:?}",
        snapshot.read_errors
    );
    assert!(!audit_snapshot(&snapshot, &AuditPolicy::default()).has_drift());
    let calls = reader.calls.borrow();
    assert!(!calls.iter().any(|call| call.ends_with("/rulesets/8")));
    assert!(!calls.iter().any(|call| call.contains("README.md")));
}

#[test]
fn fetch_reports_unreadable_settings_as_unknown() {
    let expected = compliant();
    let repo = "example/compliant";
    let mut reader = reader_for(&expected, repo);
    reader
        .json
        .remove(&format!("repos/{repo}/immutable-releases"));
    reader.errors.insert(
        format!("repos/{repo}/actions/permissions/workflow"),
        "HTTP 403: Resource not accessible by integration".to_owned(),
    );
    reader.errors.insert(
        format!("repos/{repo}/contents/.github/workflows/ci.yml?ref=trunk"),
        "HTTP 502".to_owned(),
    );
    let report = audit_snapshot(&fetch_snapshot(repo, &reader), &AuditPolicy::default());
    for setting in [
        Setting::ImmutableReleases,
        Setting::DefaultToken,
        Setting::ExactRefs,
        Setting::PermissionsEmpty,
    ] {
        assert_eq!(
            report.row(setting).expect("row").status,
            AuditStatus::Unknown,
            "{setting:?}"
        );
    }
    assert_eq!(
        report.row(Setting::UpdatesBot).expect("row").status,
        AuditStatus::Pass
    );

    let missing = fetch_snapshot("example/missing", &reader);
    let report = audit_snapshot(&missing, &AuditPolicy::default());
    assert!(
        report
            .rows
            .iter()
            .all(|row| row.status == AuditStatus::Unknown)
    );
}

#[test]
fn malformed_workflow_listing_is_unknown() {
    let expected = compliant();
    let repo = "example/compliant";
    let mut reader = reader_for(&expected, repo);
    reader.json.insert(
        format!("repos/{repo}/contents/.github/workflows?ref=trunk"),
        json!({"message": "not a directory"}),
    );
    let snapshot = fetch_snapshot(repo, &reader);
    assert!(snapshot.read_errors.contains_key("workflows"));
    assert_eq!(
        audit_row(&snapshot, &AuditPolicy::default(), Setting::ExactRefs).status,
        AuditStatus::Unknown
    );
}

fn pad_workflow_listing(reader: &mut FakeReader, repo: &str, total: usize) {
    let path = format!("repos/{repo}/contents/.github/workflows?ref=trunk");
    let mut listing = reader.json.remove(&path).expect("workflow listing");
    let entries = listing.as_array_mut().expect("array listing");
    for index in 0..total.saturating_sub(entries.len()) {
        let file = format!(".github/workflows/filler-{index}.yml");
        entries.push(json!({"name": format!("filler-{index}.yml"), "path": file}));
        reader.raw.insert(
            format!("repos/{repo}/contents/{file}?ref=trunk"),
            "name: filler\n".to_owned(),
        );
    }
    reader.json.insert(path, listing);
}

#[test]
fn large_workflow_listing_is_read_once_without_duplicates() {
    let expected = compliant();
    let repo = "example/compliant";
    let mut reader = reader_for(&expected, repo);
    pad_workflow_listing(&mut reader, repo, 104);
    // The contents API ignores `page`: any paged request would return the
    // same full listing, so the fetch must not page at all.
    let path = format!("repos/{repo}/contents/.github/workflows?ref=trunk");
    let full = reader.json.get(&path).cloned().expect("listing");
    for page in 2..=4 {
        reader
            .json
            .insert(format!("{path}&page={page}"), full.clone());
    }

    let snapshot = fetch_snapshot(repo, &reader);
    assert!(!snapshot.read_errors.contains_key("workflows"));
    let mut paths: Vec<_> = snapshot.workflows.iter().map(|w| w.path.clone()).collect();
    let before = paths.len();
    paths.sort();
    paths.dedup();
    assert_eq!(before, paths.len(), "workflows must not be duplicated");
    assert!(before >= 100);
}

#[test]
fn workflow_listing_at_the_api_cap_is_unknown() {
    let expected = compliant();
    let repo = "example/compliant";
    let mut reader = reader_for(&expected, repo);
    pad_workflow_listing(&mut reader, repo, 1000);
    let snapshot = fetch_snapshot(repo, &reader);
    assert!(snapshot.read_errors.contains_key("workflows"));
    assert_eq!(
        audit_row(&snapshot, &AuditPolicy::default(), Setting::ExactRefs).status,
        AuditStatus::Unknown
    );
}
