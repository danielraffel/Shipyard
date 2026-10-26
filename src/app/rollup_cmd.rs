//! `shipyard rollup`: form, check and land rollups ([`crate::rollup`]).
//!
//! Every subcommand reads; only `build --apply` and `land --apply` write, and
//! a landing goes through the same guarded `gh` as any other.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::CliFailure;
use super::cli::RollupCommand;
use crate::config::LoadedConfig;
use crate::head_approval;
use crate::identity::RuntimeMode;
use crate::landability::gate;
use crate::output::write_json_envelope;
use crate::rollup::{self, Config, Landing};

/// Each GitHub call's own deadline.
const GITHUB_TIMEOUT: Duration = Duration::from_mins(2);
/// How often a direct landing re-reads whether GitHub marked members merged.
const SETTLE_POLL: Duration = Duration::from_secs(15);
/// How many open pull requests `plan` judges.
const PLAN_LIMIT: usize = 30;

fn unreadable(error: impl std::fmt::Display) -> CliFailure {
    CliFailure::new(2, format!("rollup: {error}"))
}

fn refused(error: impl std::fmt::Display) -> CliFailure {
    CliFailure::new(1, format!("rollup: {error}"))
}

/// Run `shipyard rollup`: exit 0 done, 1 refused, 2 unreadable.
pub(super) fn rollup_command<W: Write>(
    command: RollupCommand,
    mode: RuntimeMode,
    cwd: &Path,
    state_dir: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let loaded = LoadedConfig::load_from_cwd(mode, cwd)
        .map_err(|error| CliFailure::new(2, format!("config error: {error}")))?;
    let repo = command
        .repo()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| gate::resolve_repo(&loaded, cwd))
        .ok_or_else(|| CliFailure::new(2, "No repo detected. Pass --repo OWNER/REPO."))?;
    let actions = crate::cloud::GitHubActions::from_loaded_config(cwd, &loaded);
    let run_gh = |args: &[String]| {
        actions
            .run_gh_with_timeout(args, GITHUB_TIMEOUT)
            .map_err(|error| error.to_string())
    };
    let run_gh: head_approval::RunGh<'_> = &run_gh;
    let base = |given: Option<String>| -> Result<String, CliFailure> {
        match given {
            Some(base) => Ok(base),
            None => run_gh(&[
                "api".to_owned(),
                format!("repos/{repo}"),
                "--jq".to_owned(),
                ".default_branch".to_owned(),
            ])
            .map(|text| text.trim().to_owned())
            .map_err(unreadable),
        }
    };
    let (name, data) = match command {
        RollupCommand::Plan { base: given, .. } => {
            let base = base(given)?;
            ("rollup plan", plan(run_gh, &repo, &base)?)
        }
        RollupCommand::Build {
            base: given,
            members,
            apply,
            ..
        } => {
            let base = base(given)?;
            (
                "rollup build",
                build(run_gh, cwd, &repo, &base, &members, apply)?,
            )
        }
        RollupCommand::Verify { pr, .. } => ("rollup verify", verify(run_gh, &repo, pr)?.1),
        RollupCommand::Land { pr, apply, .. } => {
            ("rollup land", land(run_gh, &repo, pr, apply, state_dir)?)
        }
        RollupCommand::Reconcile {
            base: given, apply, ..
        } => {
            let base = base(given)?;
            if !apply {
                return Err(CliFailure::new(
                    2,
                    "rollup reconcile closes pull requests; pass --apply",
                ));
            }
            let lines =
                rollup::reconcile(run_gh, &repo, &base, chrono::Utc::now()).map_err(unreadable)?;
            (
                "rollup reconcile",
                json!({"repo": repo, "base": base, "members": lines}),
            )
        }
        RollupCommand::Split {
            members,
            passed,
            first_passed,
            second_passed,
            ..
        } => (
            "rollup split",
            split(&members, passed, first_passed, second_passed)?,
        ),
    };
    // A landing that went through untested still landed: report it, then
    // fail so nothing reads it as a clean landing.
    let untested = data.get("untested").is_some_and(|value| !value.is_null());
    if json {
        let mut envelope = BTreeMap::new();
        envelope.insert("rollup".to_owned(), data);
        write_json_envelope(stdout, name, envelope).map_err(unreadable)?;
    } else {
        writeln!(
            stdout,
            "{}",
            serde_json::to_string_pretty(&data).map_err(unreadable)?
        )
        .map_err(unreadable)?;
    }
    Ok(if untested {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

fn base_config(
    run_gh: head_approval::RunGh<'_>,
    repo: &str,
    base: &str,
) -> Result<Config, CliFailure> {
    let text = rollup::base_config_text(run_gh, repo, base).map_err(unreadable)?;
    let config = Config::from_text(text.as_deref()).map_err(refused)?;
    if !config.rolls_up() {
        return Err(refused(format!(
            "{repo}'s {base} does not take rollups: its .shipyard/config.toml needs [train] \
             enabled = true and mode = \"rollup\""
        )));
    }
    if let Some(why) = rollup::host_stopped(repo) {
        return Err(refused(why));
    }
    Ok(config)
}

fn plan(run_gh: head_approval::RunGh<'_>, repo: &str, base: &str) -> Result<Value, CliFailure> {
    let config = base_config(run_gh, repo, base)?;
    let policy = head_approval::read_policy(run_gh, repo, base).map_err(unreadable)?;
    let candidates =
        rollup::candidates(run_gh, repo, base, &policy, &config, PLAN_LIMIT).map_err(unreadable)?;
    let (chosen, skipped) = rollup::form(&candidates, &config, &policy.carry_excluded_paths());
    Ok(json!({
        "repo": repo,
        "base": base,
        "members": chosen,
        "skipped": skipped.iter().map(|(pr, why)| json!({"pr": pr, "why": why})).collect::<Vec<_>>(),
        "next": if chosen.len() < 2 {
            "fewer than two members are ready; nothing to roll up".to_owned()
        } else {
            format!(
                "shipyard rollup build --repo {repo} --base {base} --members {} --apply",
                chosen.iter().map(u64::to_string).collect::<Vec<_>>().join(",")
            )
        },
    }))
}

fn git_in(cwd: &Path) -> impl Fn(&[&str]) -> Result<String, String> + '_ {
    move |args: &[&str]| {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .map_err(|error| format!("git: {error}"))?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            Err(format!(
                "git {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    }
}

#[allow(clippy::too_many_lines)]
fn build(
    run_gh: head_approval::RunGh<'_>,
    cwd: &Path,
    repo: &str,
    base: &str,
    members: &[u64],
    apply: bool,
) -> Result<Value, CliFailure> {
    let config = base_config(run_gh, repo, base)?;
    if members.is_empty() || members.len() > config.max_members {
        return Err(refused(format!(
            "a rollup has 1 to {} members (max_members); got {}",
            config.max_members,
            members.len()
        )));
    }
    let git = git_in(cwd);
    let base_sha = rollup::base_tip(run_gh, repo, base).map_err(unreadable)?;
    git(&["fetch", "--no-tags", "--quiet", "origin", base]).map_err(unreadable)?;
    let fetched = git(&["rev-parse", "FETCH_HEAD"]).map_err(unreadable)?;
    if fetched.trim() != base_sha {
        return Err(refused(format!(
            "{base} moved while the rollup was formed; run it again"
        )));
    }
    let mut heads = Vec::new();
    for pr in members {
        let head = run_gh(&[
            "api".to_owned(),
            format!("repos/{repo}/pulls/{pr}"),
            "--jq".to_owned(),
            ".head.sha".to_owned(),
        ])
        .map_err(unreadable)?
        .trim()
        .to_ascii_lowercase();
        git(&[
            "fetch",
            "--no-tags",
            "--quiet",
            "origin",
            &format!("refs/pull/{pr}/head"),
        ])
        .map_err(unreadable)?;
        let fetched = git(&["rev-parse", "FETCH_HEAD"]).map_err(unreadable)?;
        if fetched.trim() != head {
            return Err(refused(format!(
                "PR #{pr}'s head moved while it was fetched"
            )));
        }
        heads.push((*pr, head));
    }
    let merges = rollup::build_commits(&git, &base_sha, &heads).map_err(refused)?;
    let head = merges.last().cloned().unwrap_or_default();
    let branch = format!("{}{}", rollup::BRANCH_PREFIX, members[0]);
    let mut report = json!({
        "repo": repo,
        "base": base,
        "base_sha": base_sha,
        "branch": branch,
        "head": head,
        "members": heads.iter().zip(&merges).map(|((pr, member), merge)| json!({
            "pr": pr, "head": member, "merge": merge,
        })).collect::<Vec<_>>(),
        "applied": apply,
    });
    if !apply {
        return Ok(report);
    }
    let remote =
        git(&["ls-remote", "origin", &format!("refs/heads/{branch}")]).map_err(unreadable)?;
    let lease = remote
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_owned();
    git(&[
        "push",
        "--quiet",
        &format!("--force-with-lease=refs/heads/{branch}:{lease}"),
        "origin",
        &format!("{head}:refs/heads/{branch}"),
    ])
    .map_err(unreadable)?;
    let owner = repo.split_once('/').map_or(repo, |(owner, _)| owner);
    let open = run_gh(&[
        "api".to_owned(),
        format!("repos/{repo}/pulls?state=open&head={owner}:{branch}"),
        "--jq".to_owned(),
        "[.[].number]".to_owned(),
    ])
    .map_err(unreadable)?;
    let open: Vec<u64> = serde_json::from_str(open.trim()).map_err(unreadable)?;
    let pr = if let Some(pr) = open.first() {
        *pr
    } else {
        let created = run_gh(&[
            "api".to_owned(),
            "-X".to_owned(),
            "POST".to_owned(),
            format!("repos/{repo}/pulls"),
            "-f".to_owned(),
            format!(
                "title=Rollup of {}",
                members
                    .iter()
                    .map(|pr| format!("#{pr}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            "-f".to_owned(),
            format!("head={branch}"),
            "-f".to_owned(),
            format!("base={base}"),
            "-f".to_owned(),
            "body=Rollup formed by `shipyard rollup build`. See the notice comment for its \
                  members."
                .to_owned(),
            "--jq".to_owned(),
            ".number".to_owned(),
        ])
        .map_err(unreadable)?;
        created.trim().parse().map_err(unreadable)?
    };
    run_gh(&[
        "api".to_owned(),
        "-X".to_owned(),
        "POST".to_owned(),
        format!("repos/{repo}/issues/{pr}/comments"),
        "-f".to_owned(),
        format!("body={}", rollup::notice(&heads)),
    ])
    .map_err(unreadable)?;
    report["pr"] = json!(pr);
    Ok(report)
}

fn verify(
    run_gh: head_approval::RunGh<'_>,
    repo: &str,
    pr: u64,
) -> Result<(rollup::Rollup, Value), CliFailure> {
    let head = run_gh(&[
        "api".to_owned(),
        format!("repos/{repo}/pulls/{pr}"),
        "--jq".to_owned(),
        ".head.sha".to_owned(),
    ])
    .map_err(unreadable)?
    .trim()
    .to_ascii_lowercase();
    let base = run_gh(&[
        "api".to_owned(),
        format!("repos/{repo}/pulls/{pr}"),
        "--jq".to_owned(),
        ".base.ref".to_owned(),
    ])
    .map_err(unreadable)?
    .trim()
    .to_owned();
    let policy = head_approval::read_policy(run_gh, repo, &base).map_err(unreadable)?;
    let member_gate = |number: u64, member_head: &str| {
        head_approval::evaluate_member(run_gh, repo, number, member_head, &policy)
    };
    let rollup = rollup::guard_accept(run_gh, repo, pr, &head, &policy, &member_gate)
        .map_err(refused)?
        .ok_or_else(|| {
            refused(format!(
                "PR #{pr} is not a rollup of a base that takes them"
            ))
        })?;
    let value = serde_json::to_value(&rollup).map_err(unreadable)?;
    Ok((rollup, value))
}

fn land(
    run_gh: head_approval::RunGh<'_>,
    repo: &str,
    pr: u64,
    apply: bool,
    state_dir: &Path,
) -> Result<Value, CliFailure> {
    let (rollup, accepted) = verify(run_gh, repo, pr)?;
    let config = base_config(run_gh, repo, &rollup.base)?;
    if let Some(why) = rollup::merge_method_refusal(run_gh, repo, &rollup.base, config.landing)
        .map_err(unreadable)?
    {
        return Err(refused(why));
    }
    // The rollup pull request is the full-tier dispatch target: its merge ref
    // at this head is what a full lane must have tested.
    let tiers =
        crate::deferred_tier::deferred_tier_checks(run_gh, repo, pr, &rollup.base, &rollup.head)
            .map_err(unreadable)?;
    if let Some(why) = crate::deferred_tier::explain_all(&tiers) {
        return Err(refused(why));
    }
    if config.landing == Landing::MergeQueue {
        return Ok(json!({
            "rollup": accepted,
            "landing": "merge-queue",
            "next": format!(
                "shipyard merge-steward steward-handoff --repo {repo} --pr {pr} --head {} \
                 --workstream-id rollup-{pr} --apply",
                rollup.head
            ),
        }));
    }
    let required = rollup::required_checks(run_gh, repo, &rollup.base).map_err(unreadable)?;
    if let Some(why) =
        rollup::checks_refusal(run_gh, repo, &rollup.head, &required).map_err(unreadable)?
    {
        return Err(refused(why));
    }
    if !apply {
        return Ok(json!({
            "rollup": accepted,
            "landing": "direct",
            "applied": false,
            "next": format!("shipyard rollup land --repo {repo} --pr {pr} --apply"),
        }));
    }
    let log = state_dir.join(rollup::LANDINGS_LOG);
    let mut record = |line: &Value| -> Result<(), String> {
        std::fs::create_dir_all(state_dir).map_err(|error| error.to_string())?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .map_err(|error| format!("{}: {error}", log.display()))?;
        writeln!(file, "{line}").map_err(|error| format!("{}: {error}", log.display()))
    };
    let landed = rollup::land_direct(run_gh, repo, &rollup, &mut record).map_err(refused)?;
    let members: Vec<rollup::LandedMember> = rollup
        .members
        .iter()
        .map(|member| rollup::LandedMember {
            pr: member.pr,
            head: member.head.clone(),
            merge: member.merge.clone(),
        })
        .collect();
    let deadline = Instant::now() + rollup::MERGED_SETTLE;
    let mut settle = || {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(SETTLE_POLL);
        true
    };
    let (closed, left) =
        rollup::close_landed_members(run_gh, repo, &rollup.base, pr, &members, &mut settle)
            .map_err(unreadable)?;
    Ok(json!({
        "rollup": accepted,
        "landing": "direct",
        "applied": true,
        "merge_commit": landed.merge_commit,
        "untested": landed.untested,
        "closed_members": closed,
        "left_open": left.iter().map(|(pr, why)| json!({"pr": pr, "why": why})).collect::<Vec<_>>(),
        "record": log.display().to_string(),
    }))
}

fn split(
    members: &[u64],
    passed: bool,
    first_passed: Option<bool>,
    second_passed: Option<bool>,
) -> Result<Value, CliFailure> {
    let next = match (first_passed, second_passed) {
        (None, None) => rollup::split(members, passed),
        (Some(first_passed), Some(second_passed)) => match rollup::split(members, false) {
            rollup::Split::Halves { first, second } => {
                rollup::after_halves(&first, first_passed, &second, second_passed)
            }
            other => other,
        },
        _ => {
            return Err(CliFailure::new(
                2,
                "pass both --first-passed and --second-passed, or neither",
            ));
        }
    };
    serde_json::to_value(next).map_err(unreadable)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::split;
    use crate::app::cli::{Cli, Command, RollupCommand};

    #[test]
    fn cli_surface_parses_rollup_build_members() {
        let cli = Cli::parse_from([
            "shipyard",
            "rollup",
            "build",
            "--members",
            "11,12",
            "--base",
            "main",
            "--apply",
        ]);
        let Command::Rollup {
            command:
                RollupCommand::Build {
                    members,
                    base,
                    apply,
                    ..
                },
        } = cli.command
        else {
            panic!("expected rollup build");
        };
        assert_eq!(members, vec![11, 12]);
        assert_eq!(base.as_deref(), Some("main"));
        assert!(apply);
    }

    #[test]
    fn split_walks_a_failed_rollup_down_to_its_culprit() {
        assert_eq!(
            split(&[1, 2, 3, 4], false, None, None).expect("plan"),
            json!({"next": "halves", "first": [1, 2], "second": [3, 4]})
        );
        assert_eq!(
            split(&[1, 2, 3, 4], false, Some(true), Some(false)).expect("plan"),
            json!({"next": "halves", "first": [3], "second": [4]})
        );
        assert_eq!(
            split(&[1, 2], false, Some(true), Some(true)).expect("plan"),
            json!({"next": "shared_cause"})
        );
        assert_eq!(
            split(&[7], false, None, None).expect("plan"),
            json!({"next": "eject", "pr": 7})
        );
        assert!(split(&[1, 2], false, Some(true), None).is_err());
    }
}
