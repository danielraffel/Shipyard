//! `shipyard review-verdict --pr N`: whether a pull request's head may land,
//! by the repository-side review rule. See [`crate::review_verdict`].

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use super::CliFailure;
use crate::config::LoadedConfig;
use crate::identity::RuntimeMode;
use crate::landability::gate;
use crate::output::write_json_envelope;
use crate::review_verdict::{Verdict, review_verdict};

/// Each GitHub read's own deadline.
const GITHUB_READ_TIMEOUT: Duration = Duration::from_mins(1);

pub(super) struct ReviewVerdictArgs {
    pub(super) pr: u64,
    pub(super) repo: Option<String>,
    pub(super) head: Option<String>,
    pub(super) rebase_carry: bool,
    pub(super) json: bool,
}

/// Run `shipyard review-verdict`: exit 0 allowed, 1 refused, 2 unreadable.
pub(super) fn review_verdict_command<W: Write>(
    mode: RuntimeMode,
    cwd: &Path,
    args: ReviewVerdictArgs,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let config = LoadedConfig::load_from_cwd(mode, cwd)
        .map_err(|error| CliFailure::new(2, format!("config error: {error}")))?;
    let repo = args
        .repo
        .filter(|value| !value.trim().is_empty())
        .or_else(|| gate::resolve_repo(&config, cwd))
        .ok_or_else(|| CliFailure::new(2, "No repo detected. Pass --repo OWNER/REPO."))?;
    if let Some(head) = &args.head
        && (head.len() != 40 || !head.chars().all(|ch| ch.is_ascii_hexdigit()))
    {
        return Err(CliFailure::new(
            2,
            format!("--head must be a full 40-character commit sha, not {head:?}"),
        ));
    }
    let actions = crate::cloud::GitHubActions::from_loaded_config(cwd, &config)
        // Route the App token to the repository asked about, not the checkout's.
        .with_repo_override(&repo);
    let run_gh = |gh_args: &[String]| {
        actions
            .run_gh_with_timeout(gh_args, GITHUB_READ_TIMEOUT)
            .map_err(|error| error.to_string())
    };
    let verdict = review_verdict(
        &run_gh,
        &repo,
        args.pr,
        args.head.as_deref(),
        args.rebase_carry,
    );
    finish(&repo, args.pr, verdict, args.json, stdout)
}

/// Print `verdict` and choose the exit: 0 allowed, 1 refused, 2 when it could
/// not be judged (a caller treats 2 as refused).
fn finish<W: Write>(
    repo: &str,
    pr: u64,
    verdict: Result<Verdict, String>,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let verdict = verdict.map_err(|error| {
        CliFailure::new(
            2,
            format!("review-verdict: {repo}#{pr}: cannot be judged: {error}"),
        )
    })?;
    if json {
        let mut data = BTreeMap::new();
        data.insert(
            "verdict".to_owned(),
            serde_json::to_value(&verdict)
                .map_err(|error| CliFailure::new(2, error.to_string()))?,
        );
        write_json_envelope(stdout, "review-verdict", data)
            .map_err(|error| CliFailure::new(2, error.to_string()))?;
    } else {
        render_text(&verdict, stdout).map_err(|error| CliFailure::new(2, error.to_string()))?;
    }
    Ok(if verdict.allowed() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

fn render_text<W: Write>(verdict: &Verdict, stdout: &mut W) -> std::io::Result<()> {
    writeln!(
        stdout,
        "{}#{} head {}: {}",
        verdict.repo, verdict.pr, verdict.head, verdict.verdict
    )?;
    writeln!(stdout, "  {}", verdict.reason)?;
    if let Some(live) = &verdict.live_head {
        writeln!(stdout, "  the pull request's head is now {live}")?;
    }
    if !verdict.excluded_approvals.is_empty() {
        writeln!(
            stdout,
            "  ignored the approval of {}: only the fleet App's verdict, or a login the base \
             lists in `[auto_merge] human_reviewers`, approves",
            verdict.excluded_approvals.join(", ")
        )?;
    }
    if let Some(fix) = &verdict.fix {
        writeln!(stdout, "  fix: {fix}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::process::ExitCode;

    use super::finish;
    use crate::review_verdict::Verdict;

    fn verdict(allowed: bool) -> Verdict {
        Verdict {
            repo: "o/r".to_owned(),
            pr: 7,
            head: "a".repeat(40),
            live_head: None,
            base: "main".to_owned(),
            verdict: if allowed { "allowed" } else { "refused" },
            required: true,
            reason: "why".to_owned(),
            approval: None,
            excluded_approvals: Vec::new(),
            audit_events: Vec::new(),
            fix: (!allowed).then(|| "fix".to_owned()),
        }
    }

    #[test]
    fn the_exit_is_0_allowed_1_refused_2_unreadable() {
        let code = |result| format!("{result:?}");
        for json in [true, false] {
            let mut out = Vec::new();
            let allowed = finish("o/r", 7, Ok(verdict(true)), json, &mut out).expect("judged");
            assert_eq!(code(allowed), code(ExitCode::SUCCESS));
            let refused = finish("o/r", 7, Ok(verdict(false)), json, &mut out).expect("judged");
            assert_eq!(code(refused), code(ExitCode::from(1)));
            assert!(String::from_utf8_lossy(&out).contains("refused"));
            let failure = finish("o/r", 7, Err("HTTP 502".to_owned()), json, &mut out)
                .expect_err("unreadable");
            assert_eq!(failure.code, 2);
        }
    }
}
