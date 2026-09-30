use std::collections::BTreeSet;
use std::process::ExitCode;

use toml::Table;

#[cfg(unix)]
use super::test_support::{fake_gh, seed_repo_with_local_origin};
use super::test_support::{git_capture, seed_repo};
use super::{ShipCommandArgs, ShipInvocation, select_targets, ship_command};
use crate::app::cli::MergeResult;
use crate::config::{LoadedConfig, LocalOverlaySource};
use crate::executor::dispatch::{opt_in_target_names, resolve_targets_from_table};
use crate::identity::RuntimeMode;
use crate::job::ValidationMode;
use crate::paths::RuntimePaths;

fn config_from(root: &std::path::Path, toml: &str) -> LoadedConfig {
    LoadedConfig {
        data: toml.parse::<Table>().expect("config TOML"),
        global_dir: root.join("global"),
        project_dir: None,
        local_dir: None,
        local_overlay_source: LocalOverlaySource::None,
    }
}

const OPT_IN_MAC: &str = r#"
    [validation.default]
    command = "rustc --version"

    [targets.mac]
    backend = "local"
    platform = "macos-arm64"
    default = false
"#;

const DEFAULT_MAC_OPT_IN_LINUX: &str = r#"
    [validation.default]
    command = "rustc --version"

    [targets.mac]
    backend = "local"
    platform = "macos-arm64"

    [targets.linux]
    backend = "local"
    platform = "linux-x64"
    default = false
"#;

fn names(targets: &[crate::executor::dispatch::ResolvedTarget]) -> Vec<&str> {
    targets.iter().map(|target| target.name.as_str()).collect()
}

fn select(toml: &str, requested: &[&str], skip: &[&str]) -> Result<Vec<String>, super::CliFailure> {
    let data = toml.parse::<Table>().expect("config TOML");
    let resolved = resolve_targets_from_table(&data, ValidationMode::Full).expect("resolve");
    let opt_in = opt_in_target_names(&data).expect("opt-in names");
    let requested = requested
        .iter()
        .map(|name| (*name).to_owned())
        .collect::<Vec<_>>();
    let skip = skip
        .iter()
        .map(|name| (*name).to_owned())
        .collect::<Vec<_>>();
    select_targets(resolved, &opt_in, &requested, &skip)
        .map(|targets| names(&targets).into_iter().map(ToOwned::to_owned).collect())
}

#[test]
fn opt_in_target_is_left_out_of_the_default_set() {
    assert_eq!(
        select(DEFAULT_MAC_OPT_IN_LINUX, &[], &[]).expect("select"),
        vec!["mac"]
    );
}

#[test]
fn requested_opt_in_target_joins_the_default_set() {
    assert_eq!(
        select(DEFAULT_MAC_OPT_IN_LINUX, &["linux"], &[]).expect("select"),
        vec!["linux", "mac"]
    );
}

#[test]
fn all_opt_in_without_a_request_selects_nothing_rather_than_failing() {
    assert!(select(OPT_IN_MAC, &[], &[]).expect("select").is_empty());
    assert_eq!(
        select(OPT_IN_MAC, &["mac"], &[]).expect("select"),
        vec!["mac"]
    );
}

#[test]
fn skipping_every_default_target_still_exits_two() {
    let error = select(DEFAULT_MAC_OPT_IN_LINUX, &[], &["mac"]).expect_err("skip-empty");
    assert_eq!(error.code, 2);
    assert!(
        error
            .message
            .contains("No targets remain after --skip-target")
    );
}

#[test]
fn unknown_or_contradictory_target_requests_exit_two() {
    let unknown = select(OPT_IN_MAC, &["windows"], &[]).expect_err("unknown");
    assert_eq!(unknown.code, 2);
    assert!(unknown.message.contains("windows"));
    let both = select(OPT_IN_MAC, &["mac"], &["mac"]).expect_err("contradiction");
    assert_eq!(both.code, 2);
}

#[test]
fn opt_in_names_reject_a_non_boolean_default() {
    let data = r#"
        [targets.mac]
        backend = "local"
        default = "no"
    "#
    .parse::<Table>()
    .expect("config TOML");
    let error = opt_in_target_names(&data).expect_err("non-boolean default");
    assert!(error.to_string().contains("`default` must be a boolean"));
}

#[test]
fn an_active_profile_selection_overrides_opt_in() {
    let data = format!(
        "{OPT_IN_MAC}\n[project]\nprofile = \"local\"\n\n[profiles.local]\ntargets = [\"mac\"]\n"
    )
    .parse::<Table>()
    .expect("config TOML");
    assert_eq!(
        opt_in_target_names(&data).expect("opt-in names"),
        BTreeSet::new()
    );
    let bare = OPT_IN_MAC.parse::<Table>().expect("config TOML");
    assert_eq!(
        opt_in_target_names(&bare).expect("opt-in names"),
        BTreeSet::from(["mac".to_owned()])
    );
}

fn ship_args(pr: Option<u64>, gh: Option<std::path::PathBuf>) -> ShipCommandArgs {
    ShipCommandArgs {
        allow_unserved_lanes: Vec::new(),
        allow_unreachable_triggers: Vec::new(),
        skip_landability: true,
        pr,
        base: "main".to_owned(),
        auto_create_base: None,
        no_warm: true,
        resume_from: None,
        merge_command: None,
        merge_result: Some(MergeResult::Success),
        gh_command: gh,
        pr_snapshot_file: None,
        allow_unreachable_targets: false,
        allow_fleet_epoch_drift: false,
        skip_targets: Vec::new(),
        targets: Vec::new(),
        adopt_head: false,
        steward_handoff: None,
        invocation: ShipInvocation::Direct,
        foreground: true,
        arm_auto_merge: false,
        body_append: None,
    }
}

#[test]
#[cfg(unix)]
fn ship_with_only_opt_in_targets_pushes_and_delegates_to_required_checks() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    let remote = temp.path().join("remote.git");
    seed_repo_with_local_origin(&repo, &remote);
    let gh = temp.path().join("gh");
    fake_gh(
        &gh,
        &format!(
            r#"
if [ "$1" = "pr" ] && [ "$2" = "list" ]; then
  head=$(git -C "{}" rev-parse HEAD)
  echo '[{{"number":88,"url":"https://github.com/o/r/pull/88","title":"Existing PR","state":"OPEN","headRefName":"feature/test","headRefOid":"'"$head"'","baseRefName":"main"}}]'
  exit 0
fi
echo "unexpected gh args: $@" >&2
exit 2
"#,
            repo.display()
        ),
    );
    let paths = RuntimePaths::current_with_overrides(
        RuntimeMode::Isolated,
        Some(temp.path().join("global")),
        Some(temp.path().join("state")),
    );
    let mut stdout = Vec::new();

    let code = ship_command(
        ship_args(None, Some(gh)),
        &config_from(temp.path(), OPT_IN_MAC),
        &repo,
        &paths,
        true,
        &mut stdout,
    )
    .expect("ship command");

    assert_eq!(
        code,
        ExitCode::SUCCESS,
        "{}",
        String::from_utf8_lossy(&stdout)
    );
    let output: serde_json::Value = serde_json::from_slice(&stdout).expect("json");
    assert_eq!(output["pr"], 88);
    assert_eq!(output["validation"], "delegated");
    assert_eq!(output["verdict_owner"], "required-checks");
    assert_eq!(output["opt_in_targets"], serde_json::json!(["mac"]));
    let pushed = git_capture(&["rev-parse", "refs/heads/feature/test"], &remote);
    assert_eq!(pushed, git_capture(&["rev-parse", "HEAD"], &repo));
    assert!(!paths.state_dir.join("queue.json").exists());
    assert!(!paths.state_dir.join("ship").join("88.json").exists());
}

#[test]
fn ship_with_requested_opt_in_target_validates_it() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    seed_repo(&repo);
    let head = git_capture(&["rev-parse", "HEAD"], &repo);
    let snapshot = temp.path().join("pr.json");
    std::fs::write(
        &snapshot,
        format!(r#"{{"headRefName":"feature/test","headRefOid":"{head}"}}"#),
    )
    .expect("write snapshot");
    let paths = RuntimePaths::current_with_overrides(
        RuntimeMode::Isolated,
        Some(temp.path().join("global")),
        Some(temp.path().join("state")),
    );
    let mut args = ship_args(Some(42), None);
    args.pr_snapshot_file = Some(snapshot);
    args.targets = vec!["mac".to_owned()];
    let mut stdout = Vec::new();

    let code = ship_command(
        args,
        &config_from(temp.path(), OPT_IN_MAC),
        &repo,
        &paths,
        true,
        &mut stdout,
    )
    .expect("ship command");

    assert_eq!(
        code,
        ExitCode::SUCCESS,
        "{}",
        String::from_utf8_lossy(&stdout)
    );
    let output: serde_json::Value = serde_json::from_slice(&stdout).expect("json");
    assert_eq!(output["ship_state"]["evidence_snapshot"]["mac"], "pass");
    assert!(output.get("validation").is_none());
}

#[test]
fn ship_skipping_the_only_default_target_exits_two_before_any_push() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    seed_repo(&repo);
    let paths = RuntimePaths::current_with_overrides(
        RuntimeMode::Isolated,
        Some(temp.path().join("global")),
        Some(temp.path().join("state")),
    );
    let mut args = ship_args(Some(42), None);
    args.skip_targets = vec!["mac".to_owned()];
    let mut stdout = Vec::new();

    let error = ship_command(
        args,
        &config_from(temp.path(), DEFAULT_MAC_OPT_IN_LINUX),
        &repo,
        &paths,
        true,
        &mut stdout,
    )
    .expect_err("skip-empty stays an error");

    assert_eq!(error.code, 2);
    assert!(stdout.is_empty());
    assert!(!paths.state_dir.join("queue.json").exists());
}
