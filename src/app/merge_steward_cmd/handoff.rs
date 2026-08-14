use super::{
    CliFailure, GitHubActions, HANDOFF_CONTEXT, MANAGED_LABEL, Path, Value, Write, gh_json,
    is_full_sha, observation::encode_path_segment, resolve_repos, write_json_envelope,
};
use std::collections::BTreeMap;
use std::process::ExitCode;

pub(crate) struct StewardHandoffArgs {
    pub(crate) repo: Option<String>,
    pub(crate) pr: u64,
    pub(crate) head: String,
    pub(crate) workstream_id: String,
    pub(crate) context_url: Option<String>,
    pub(crate) apply: bool,
}

pub(crate) fn steward_handoff_command<W: Write>(
    args: &StewardHandoffArgs,
    cwd: &Path,
    actions: &GitHubActions,
    json_output: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    validate_args(args)?;
    let repo = resolve_repos(args.repo.clone().into_iter().collect(), cwd)?
        .into_iter()
        .next()
        .ok_or_else(|| CliFailure::new(1, "repository was not resolved"))?;
    verify_exact_open_pr(actions, &repo, args.pr, &args.head)?;

    if args.apply {
        write_handoff_status(actions, &repo, args)?;
        // A status written to a superseded commit is harmless. The management
        // label is not: re-read immediately before adding it so a newer head
        // cannot be adopted using the old receipt.
        verify_exact_open_pr(actions, &repo, args.pr, &args.head)?;
        ensure_label(
            actions,
            &repo,
            MANAGED_LABEL,
            "0E8A16",
            "Explicit Shipyard stewardship ownership",
        )?;
        add_label(actions, &repo, args.pr, MANAGED_LABEL)?;
    }

    render(args, &repo, json_output, stdout)?;
    Ok(ExitCode::SUCCESS)
}

fn validate_args(args: &StewardHandoffArgs) -> Result<(), CliFailure> {
    if args.pr == 0 {
        return Err(CliFailure::new(1, "pull-request number must be positive"));
    }
    if !is_full_sha(&args.head) {
        return Err(CliFailure::new(
            1,
            "--head must be a full 40-character SHA-1",
        ));
    }
    let workstream = args.workstream_id.trim();
    if workstream.is_empty()
        || workstream.len() > 128
        || workstream.chars().any(char::is_whitespace)
    {
        return Err(CliFailure::new(
            1,
            "--workstream-id must be 1-128 non-whitespace characters",
        ));
    }
    if let Some(url) = args.context_url.as_deref()
        && !(url.starts_with("https://") || url.starts_with("http://"))
    {
        return Err(CliFailure::new(
            1,
            "--context-url must use http:// or https://",
        ));
    }
    Ok(())
}

fn verify_exact_open_pr(
    actions: &GitHubActions,
    repo: &str,
    pr: u64,
    expected_head: &str,
) -> Result<(), CliFailure> {
    let value = gh_json(
        actions,
        &["api".to_owned(), format!("repos/{repo}/pulls/{pr}")],
        "pull-request handoff preflight",
    )
    .map_err(|error| CliFailure::new(1, error))?;
    let state = value
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if state != "open" {
        return Err(CliFailure::new(1, format!("PR #{pr} is not open")));
    }
    let current = value
        .pointer("/head/sha")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !current.eq_ignore_ascii_case(expected_head) {
        return Err(CliFailure::new(
            1,
            format!("PR #{pr} head drift: expected {expected_head}, current {current}"),
        ));
    }
    Ok(())
}

fn write_handoff_status(
    actions: &GitHubActions,
    repo: &str,
    args: &StewardHandoffArgs,
) -> Result<(), CliFailure> {
    let description = format!("Managed handoff {}", args.workstream_id)
        .chars()
        .take(140)
        .collect::<String>();
    let mut command = vec![
        "api".to_owned(),
        "-X".to_owned(),
        "POST".to_owned(),
        format!("repos/{repo}/statuses/{}", args.head),
        "-f".to_owned(),
        "state=success".to_owned(),
        "-f".to_owned(),
        format!("context={HANDOFF_CONTEXT}"),
        "-f".to_owned(),
        format!("description={description}"),
    ];
    if let Some(url) = args.context_url.as_deref() {
        command.push("-f".to_owned());
        command.push(format!("target_url={url}"));
    }
    actions
        .run_gh(&command)
        .map_err(|error| CliFailure::new(1, format!("could not write handoff receipt: {error}")))?;
    Ok(())
}

pub(super) fn ensure_label(
    actions: &GitHubActions,
    repo: &str,
    label: &str,
    color: &str,
    description: &str,
) -> Result<(), CliFailure> {
    let encoded = encode_path_segment(label);
    let inspect = actions.run_gh(&["api".to_owned(), format!("repos/{repo}/labels/{encoded}")]);
    match inspect {
        Ok(_) => Ok(()),
        Err(error) if error.to_string().contains("HTTP 404") => actions
            .run_gh(&[
                "api".to_owned(),
                "-X".to_owned(),
                "POST".to_owned(),
                format!("repos/{repo}/labels"),
                "-f".to_owned(),
                format!("name={label}"),
                "-f".to_owned(),
                format!("color={color}"),
                "-f".to_owned(),
                format!("description={description}"),
            ])
            .map(|_| ())
            .map_err(|error| CliFailure::new(1, format!("could not create label: {error}"))),
        Err(error) => Err(CliFailure::new(
            1,
            format!("could not inspect managed label: {error}"),
        )),
    }
}

pub(super) fn add_label(
    actions: &GitHubActions,
    repo: &str,
    pr: u64,
    label: &str,
) -> Result<(), CliFailure> {
    actions
        .run_gh(&[
            "api".to_owned(),
            "-X".to_owned(),
            "POST".to_owned(),
            format!("repos/{repo}/issues/{pr}/labels"),
            "-f".to_owned(),
            format!("labels[]={label}"),
        ])
        .map(|_| ())
        .map_err(|error| CliFailure::new(1, format!("could not add managed label: {error}")))
}

fn render<W: Write>(
    args: &StewardHandoffArgs,
    repo: &str,
    json_output: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    if json_output {
        let mut data = BTreeMap::new();
        data.insert("apply".to_owned(), Value::from(args.apply));
        data.insert("repo".to_owned(), Value::from(repo));
        data.insert("pr".to_owned(), Value::from(args.pr));
        data.insert("head_sha".to_owned(), Value::from(args.head.clone()));
        data.insert(
            "workstream_id".to_owned(),
            Value::from(args.workstream_id.clone()),
        );
        data.insert("managed_label".to_owned(), Value::from(MANAGED_LABEL));
        data.insert("handoff_context".to_owned(), Value::from(HANDOFF_CONTEXT));
        if let Some(url) = args.context_url.as_deref() {
            data.insert("context_url".to_owned(), Value::from(url));
        }
        return write_json_envelope(stdout, "runner.steward-handoff", data)
            .map_err(|error| CliFailure::new(1, error.to_string()));
    }
    writeln!(
        stdout,
        "steward handoff: mode={} repo={} pr=#{} head={} workstream={} label={}",
        if args.apply { "apply" } else { "dry-run" },
        repo,
        args.pr,
        args.head,
        args.workstream_id,
        MANAGED_LABEL
    )
    .map_err(|error| CliFailure::new(1, error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> StewardHandoffArgs {
        StewardHandoffArgs {
            repo: Some("owner/repo".to_owned()),
            pr: 7,
            head: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
            workstream_id: "GEN-7".to_owned(),
            context_url: Some("https://linear.app/example/GEN-7".to_owned()),
            apply: false,
        }
    }

    #[test]
    fn rejects_non_exact_head_and_non_http_context_before_transport() {
        let mut invalid = args();
        invalid.head = "abc".to_owned();
        assert!(validate_args(&invalid).is_err());
        invalid = args();
        invalid.context_url = Some("file:///tmp/private".to_owned());
        assert!(validate_args(&invalid).is_err());
    }

    #[test]
    fn workstream_identifier_is_small_and_single_token() {
        let mut invalid = args();
        invalid.workstream_id = "GEN 7".to_owned();
        assert!(validate_args(&invalid).is_err());
        assert!(validate_args(&args()).is_ok());
    }
}
