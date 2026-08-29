use toml::Table;

use super::provenance::configured_pr_provenance_hook;
#[cfg(unix)]
use super::provenance::{apply_requested_steward_handoff_with_actions, run_pr_provenance_hook};
#[cfg(not(unix))]
use super::test_support::loaded_config;
#[cfg(unix)]
use super::test_support::{fake_gh, loaded_config};
#[cfg(unix)]
use super::{ResolvedPrContext, ShipStewardHandoff};
#[cfg(unix)]
use crate::cloud::GitHubActions;
#[cfg(unix)]
use crate::identity::RuntimeMode;
#[cfg(unix)]
use crate::paths::RuntimePaths;

#[test]
fn provenance_hook_config_is_argv_only_and_required_by_default() {
    let mut config = loaded_config(std::path::Path::new("."));
    let extra = r#"
        [pr.provenance]
        command = ["whence", "--pr", "{pr}", "--auto"]
    "#
    .parse::<Table>()
    .expect("hook TOML");
    config.data.extend(extra);
    let hook = configured_pr_provenance_hook(&config)
        .expect("valid config")
        .expect("configured hook");
    assert_eq!(hook.command, ["whence", "--pr", "{pr}", "--auto"]);
    assert!(hook.required);

    config.data.insert(
        "pr".to_owned(),
        toml::Value::Table(
            r#"[provenance]
               command = "whence --auto""#
                .parse::<Table>()
                .expect("invalid-shape fixture"),
        ),
    );
    let error = configured_pr_provenance_hook(&config).expect_err("string must fail");
    assert!(error.message().contains("TOML string array"));
}

#[cfg(unix)]
#[test]
fn provenance_hook_gets_exact_pr_facts_before_handoff() {
    let temp = tempfile::tempdir().expect("tempdir");
    let hook = temp.path().join("provenance-hook");
    let log = temp.path().join("hook.log");
    fake_gh(
        &hook,
        r#"log=$1
shift
printf '%s\n' "$SHIPYARD_PR_NUMBER|$SHIPYARD_PR_REPO|$SHIPYARD_PR_HEAD|$SHIPYARD_PR_BRANCH|$SHIPYARD_PR_BASE|$SHIPYARD_PR_URL|$*" > "$log""#,
    );
    let mut config = loaded_config(temp.path());
    let extra = format!(
        r#"[pr.provenance]
command = [{hook:?}, {log:?}, "{{pr}}", "{{repo}}", "{{head}}", "{{branch}}", "{{base}}", "{{url}}"]
required = true
"#,
        hook = hook.display().to_string(),
        log = log.display().to_string(),
    )
    .parse::<Table>()
    .expect("hook config");
    config.data.extend(extra);
    let pr = super::ResolvedPrContext {
        number: 42,
        base_branch: "main".to_owned(),
        pr_url: Some("https://github.com/danielraffel/pulp/pull/42".to_owned()),
        pr_title: Some("Fix".to_owned()),
    };
    let head = "a".repeat(40);
    let mut stdout = Vec::new();
    run_pr_provenance_hook(
        &config,
        temp.path(),
        &mut stdout,
        "danielraffel/pulp",
        "feature/provenance",
        "main",
        &head,
        &pr,
    )
    .expect("hook succeeds");
    let recorded = std::fs::read_to_string(log).expect("hook log");
    assert_eq!(
        recorded.trim(),
        format!(
            "42|danielraffel/pulp|{head}|feature/provenance|main|https://github.com/danielraffel/pulp/pull/42|42 danielraffel/pulp {head} feature/provenance main https://github.com/danielraffel/pulp/pull/42"
        )
    );
    assert!(
        String::from_utf8(stdout)
            .expect("utf8")
            .contains("completed for #42")
    );
}

#[cfg(unix)]
#[test]
fn required_provenance_hook_fails_closed() {
    let temp = tempfile::tempdir().expect("tempdir");
    let hook = temp.path().join("provenance-hook");
    fake_gh(&hook, "printf 'missing provenance' >&2\nexit 7");
    let mut config = loaded_config(temp.path());
    let extra = format!(
        r"[pr.provenance]
command = [{hook:?}]
",
        hook = hook.display().to_string(),
    )
    .parse::<Table>()
    .expect("hook config");
    config.data.extend(extra);
    let pr = super::ResolvedPrContext {
        number: 42,
        base_branch: "main".to_owned(),
        pr_url: None,
        pr_title: None,
    };
    let error = run_pr_provenance_hook(
        &config,
        temp.path(),
        &mut Vec::new(),
        "danielraffel/pulp",
        "feature/provenance",
        "main",
        &"a".repeat(40),
        &pr,
    )
    .expect_err("required hook must fail");
    assert_eq!(error.code, 1);
    assert!(error.message().contains("exit 7"));
    assert!(error.message().contains("missing provenance"));
}

#[cfg(unix)]
#[test]
fn atomic_handoff_uses_pr_fallback_and_writes_status_before_label() {
    let temp = tempfile::tempdir().expect("tempdir");
    let gh = temp.path().join("gh");
    let log = temp.path().join("gh.log");
    let head = "a".repeat(40);
    fake_gh(
        &gh,
        &format!(
            r#"printf '%s\n' "$*" >> '{}'
case "$*" in
  *"repos/danielraffel/pulp/pulls/42"*)
printf '%s\n' '{{"state":"open","head":{{"sha":"{head}"}}}}'
;;
  *"repos/danielraffel/pulp/commits/"*"/statuses?"*) printf '%s\n' '[]' ;;
  *) printf '%s\n' '{{}}' ;;
esac"#,
            log.display()
        ),
    );
    let config = loaded_config(temp.path());
    let actions =
        GitHubActions::from_loaded_config(temp.path(), &config).with_gh_binary_for_tests(&gh);
    let runtime_paths = RuntimePaths::current_with_overrides(
        RuntimeMode::Isolated,
        Some(temp.path().join("global")),
        Some(temp.path().join("state")),
    );
    let request = ShipStewardHandoff {
        workstream_id: None,
        context_url: None,
        launch_profile: None,
        after_handoff: "continue".to_owned(),
        task_graph: None,
    };
    let pr = ResolvedPrContext {
        number: 42,
        base_branch: String::from("main"),
        pr_url: Some(String::from("https://github.com/danielraffel/pulp/pull/42")),
        pr_title: Some(String::from("Fix")),
    };

    let receipt = apply_requested_steward_handoff_with_actions(
        Some(&request),
        "danielraffel/pulp",
        &head,
        &pr,
        temp.path(),
        &runtime_paths,
        &actions,
        true,
        &mut Vec::new(),
    )
    .expect("handoff")
    .expect("receipt");

    assert_eq!(receipt.workstream_id, "danielraffel/pulp#42");
    assert!(!receipt.monitoring_transferred);
    assert!(receipt.publication_work_id.is_none());
    assert!(receipt.publication_route_ref.is_none());
    assert!(receipt.publication_wake_id.is_none());
    assert_eq!(
        receipt.context_url.as_deref(),
        Some(pr.pr_url.as_deref().unwrap())
    );
    let calls = std::fs::read_to_string(log).expect("gh log");
    let status = calls
        .find("-X POST repos/danielraffel/pulp/statuses/")
        .expect("status POST");
    let label = calls.find("issues/42/labels").expect("label call");
    assert!(status < label, "status receipt must precede managed label");
}
