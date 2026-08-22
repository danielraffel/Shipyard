use super::{
    CliFailure, GitHubActions, HANDOFF_CONTEXT, MANAGED_LABEL, Path, UNMANAGED_LABEL, Value, Write,
    gh_json, is_full_sha, observation::encode_path_segment, resolve_repos, write_json_envelope,
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
        remove_label(actions, &repo, args.pr, UNMANAGED_LABEL)?;
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
        || workstream.len() > 124
        || workstream.chars().any(char::is_whitespace)
    {
        return Err(CliFailure::new(
            1,
            "--workstream-id must be 1-124 non-whitespace characters",
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

pub(super) fn verify_exact_open_pr(
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
    let description = format!("Managed handoff {}", args.workstream_id);
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
    run_steward_write(actions, &command, "handoff receipt")
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
        Err(error) if error.to_string().contains("HTTP 404") => run_steward_write(
            actions,
            &[
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
            ],
            "steward label creation",
        )
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
    run_steward_write(
        actions,
        &[
            "api".to_owned(),
            "-X".to_owned(),
            "POST".to_owned(),
            format!("repos/{repo}/issues/{pr}/labels"),
            "-f".to_owned(),
            format!("labels[]={label}"),
        ],
        "steward label attachment",
    )
    .map(|_| ())
    .map_err(|error| CliFailure::new(1, format!("could not add label {label}: {error}")))
}

pub(super) fn remove_label(
    actions: &GitHubActions,
    repo: &str,
    pr: u64,
    label: &str,
) -> Result<(), CliFailure> {
    let encoded = encode_path_segment(label);
    match run_steward_write(
        actions,
        &[
            "api".to_owned(),
            "-X".to_owned(),
            "DELETE".to_owned(),
            format!("repos/{repo}/issues/{pr}/labels/{encoded}"),
        ],
        "steward label removal",
    ) {
        Ok(_) => Ok(()),
        Err(error) if error.to_string().contains("HTTP 404") => Ok(()),
        Err(error) => Err(CliFailure::new(
            1,
            format!("could not remove label {label}: {error}"),
        )),
    }
}

pub(super) fn run_steward_write(
    actions: &GitHubActions,
    args: &[String],
    purpose: &str,
) -> Result<String, crate::cloud::GitHubError> {
    match actions.run_gh(args) {
        Ok(value) => Ok(value),
        Err(error) if error.is_integration_permission_denial() => {
            let _ = crate::writer_domain_lease::write_stderr(format_args!(
                "shipyard: configured GitHub App cannot write {purpose}; falling back to ambient gh auth for this steward mutation only."
            ));
            actions.run_gh_ambient(args)
        }
        Err(error) => Err(error),
    }
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

    #[cfg(unix)]
    fn sequenced_gh(
        temp: &tempfile::TempDir,
        first_error: &str,
    ) -> (GitHubActions, std::path::PathBuf) {
        let count = temp.path().join("count");
        let source = format!(
            r#"
use std::path::Path;

fn main() {{
    let path = Path::new({count:?});
    let previous = std::fs::read_to_string(path)
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0);
    let current = previous + 1;
    std::fs::write(path, current.to_string()).expect("write invocation count");
    if current == 1 {{
        eprintln!({first_error:?});
        std::process::exit(1);
    }}
    println!("{{{{}}}}");
}}
"#,
            count = count.to_string_lossy(),
        );
        let binary = crate::test_support::compile_native_test_program(temp.path(), "gh", &source);
        (
            GitHubActions::new(temp.path()).with_gh_binary_for_tests(binary),
            count,
        )
    }

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

    #[cfg(unix)]
    #[test]
    fn exact_integration_permission_error_uses_one_ambient_fallback() {
        let temp = tempfile::tempdir().expect("temp");
        let (actions, count) = sequenced_gh(&temp, "Resource not accessible by integration");
        run_steward_write(&actions, &["api".to_owned(), "test".to_owned()], "test")
            .expect("ambient fallback");
        assert_eq!(std::fs::read_to_string(count).expect("count"), "2");
    }

    #[cfg(unix)]
    #[test]
    fn generic_write_failure_does_not_escape_to_ambient_auth() {
        let temp = tempfile::tempdir().expect("temp");
        let (actions, count) = sequenced_gh(&temp, "HTTP 403 generic forbidden");
        assert!(
            run_steward_write(&actions, &["api".to_owned(), "test".to_owned()], "test").is_err()
        );
        assert_eq!(std::fs::read_to_string(count).expect("count"), "1");
    }

    #[cfg(unix)]
    #[test]
    fn removing_an_absent_explanatory_label_is_idempotent() {
        let temp = tempfile::tempdir().expect("temp");
        let (actions, count) = sequenced_gh(&temp, "HTTP 404 label not found");
        remove_label(&actions, "owner/repo", 7, UNMANAGED_LABEL)
            .expect("absent label is already clear");
        assert_eq!(std::fs::read_to_string(count).expect("count"), "1");
    }
}
