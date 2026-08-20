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
    let pr_snapshot = verify_exact_open_pr(actions, &repo, args.pr, &args.head)?;

    if args.apply {
        if handoff_receipt_is_valid(actions, &repo, args, &pr_snapshot).unwrap_or(false) {
            render(args, &repo, json_output, stdout)?;
            return Ok(ExitCode::SUCCESS);
        }
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
        if !existing_handoff_receipt_is_valid(actions, &repo, args)? {
            return Err(CliFailure::new(
                1,
                "steward handoff did not converge on the final open exact head and labels",
            ));
        }
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
) -> Result<Value, CliFailure> {
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
    Ok(value)
}

/// Return true only when the exact requested receipt and management label are
/// already present. A status on another head or for another workstream is not
/// reusable, and an unreadable status endpoint never becomes proof.
pub(crate) fn existing_handoff_receipt_is_valid(
    actions: &GitHubActions,
    repo: &str,
    args: &StewardHandoffArgs,
) -> Result<bool, CliFailure> {
    validate_args(args)?;
    let snapshot = verify_exact_open_pr(actions, repo, args.pr, &args.head)?;
    handoff_receipt_is_valid(actions, repo, args, &snapshot)
}

fn handoff_receipt_is_valid(
    actions: &GitHubActions,
    repo: &str,
    args: &StewardHandoffArgs,
    pr_snapshot: &Value,
) -> Result<bool, CliFailure> {
    if !handoff_labels_are_converged(pr_snapshot) {
        return Ok(false);
    }
    let combined = gh_json(
        actions,
        &[
            "api".to_owned(),
            format!("repos/{repo}/commits/{}/status", args.head),
        ],
        "steward handoff receipt inspection",
    )
    .map_err(|error| CliFailure::new(1, error))?;
    let expected_description = format!("Managed handoff {}", args.workstream_id);
    let expected_url = args.context_url.as_deref();
    let receipt_matches = combined
        .get("statuses")
        .and_then(Value::as_array)
        .and_then(|statuses| {
            statuses.iter().find(|status| {
                status.get("context").and_then(Value::as_str) == Some(HANDOFF_CONTEXT)
            })
        })
        .is_some_and(|status| receipt_status_matches(status, &expected_description, expected_url));
    if !receipt_matches {
        return Ok(false);
    }

    // The status read is a network boundary. Re-read the open PR immediately
    // before declaring terminal ownership so a moved head or restored opt-out
    // label cannot inherit an old exact-head receipt.
    let final_snapshot = verify_exact_open_pr(actions, repo, args.pr, &args.head)?;
    Ok(handoff_labels_are_converged(&final_snapshot))
}

fn receipt_status_matches(
    status: &Value,
    expected_description: &str,
    expected_url: Option<&str>,
) -> bool {
    status.get("state").and_then(Value::as_str) == Some("success")
        && status.get("description").and_then(Value::as_str) == Some(expected_description)
        && status.get("target_url").and_then(Value::as_str) == expected_url
}

fn handoff_labels_are_converged(pr_snapshot: &Value) -> bool {
    let Some(labels) = pr_snapshot.get("labels").and_then(Value::as_array) else {
        return false;
    };
    let has = |name| {
        labels.iter().any(|label| {
            label
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|actual| actual.eq_ignore_ascii_case(name))
        })
    };
    has(MANAGED_LABEL) && !has(UNMANAGED_LABEL)
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
            eprintln!(
                "shipyard: configured GitHub App cannot write {purpose}; falling back to ambient gh auth for this steward mutation only."
            );
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
        use std::os::unix::fs::PermissionsExt;

        let count = temp.path().join("count");
        let script = temp.path().join("gh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\ncount=0\n[ ! -f '{0}' ] || count=$(cat '{0}')\ncount=$((count + 1))\nprintf '%s' \"$count\" > '{0}'\nif [ \"$count\" -eq 1 ]; then echo '{1}' >&2; exit 1; fi\necho '{{}}'\n",
                count.display(),
                first_error
            ),
        )
        .expect("fake gh");
        let mut permissions = std::fs::metadata(&script).expect("metadata").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).expect("chmod");
        (
            GitHubActions::new(temp.path()).with_gh_binary_for_tests(script),
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
    fn valid_exact_receipt_short_circuits_all_writes() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("temp");
        let log = temp.path().join("calls");
        let gh = temp.path().join("gh");
        let head = "a".repeat(40);
        std::fs::write(
            &gh,
            format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{log}'
case "$*" in
  *"pulls/7"*) printf '%s\n' '{{"state":"open","head":{{"sha":"{head}"}},"labels":[{{"name":"{MANAGED_LABEL}"}}]}}' ;;
  *"commits/{head}/status"*) printf '%s\n' '{{"statuses":[{{"context":"{HANDOFF_CONTEXT}","state":"success","description":"Managed handoff GEN-7","target_url":"https://linear.app/example/GEN-7"}}]}}' ;;
  *) echo 'unexpected mutation' >&2; exit 9 ;;
esac
"#,
                log = log.display()
            ),
        )
        .expect("fake gh");
        let mut permissions = std::fs::metadata(&gh).expect("metadata").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&gh, permissions).expect("chmod");
        let actions = GitHubActions::new(temp.path()).with_gh_binary_for_tests(gh);
        let mut requested = args();
        requested.apply = true;

        steward_handoff_command(&requested, temp.path(), &actions, false, &mut Vec::new())
            .expect("existing receipt");

        let calls = std::fs::read_to_string(log).expect("calls");
        assert_eq!(calls.lines().count(), 3, "receipt reuse must not write");
        assert!(!calls.contains("-X POST"));
    }

    #[test]
    fn receipt_reuse_requires_managed_without_unmanaged_opt_out() {
        let converged = serde_json::json!({"labels": [{"name": MANAGED_LABEL}]});
        let opted_out = serde_json::json!({
            "labels": [{"name": "Shipyard:Managed"}, {"name": "SHIPYARD:UNMANAGED"}]
        });
        assert!(handoff_labels_are_converged(&converged));
        assert!(!handoff_labels_are_converged(&opted_out));
        assert!(!handoff_labels_are_converged(&serde_json::json!({})));
    }

    #[test]
    fn omitted_context_requires_an_absent_receipt_target_url() {
        let with_url = serde_json::json!({
            "state": "success",
            "description": "Managed handoff GEN-7",
            "target_url": "https://example.invalid/stale"
        });
        let without_url = serde_json::json!({
            "state": "success",
            "description": "Managed handoff GEN-7",
            "target_url": null
        });
        assert!(!receipt_status_matches(
            &with_url,
            "Managed handoff GEN-7",
            None
        ));
        assert!(receipt_status_matches(
            &without_url,
            "Managed handoff GEN-7",
            None
        ));
    }

    #[cfg(unix)]
    #[test]
    fn receipt_reuse_revalidates_exact_head_after_status_read() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("temp");
        let count = temp.path().join("count");
        let gh = temp.path().join("gh");
        let head = "a".repeat(40);
        let moved = "b".repeat(40);
        std::fs::write(
            &gh,
            format!(
                r#"#!/bin/sh
case "$*" in
  *"pulls/7"*)
    n=$(cat '{count}' 2>/dev/null || echo 0)
    n=$((n + 1))
    printf '%s' "$n" > '{count}'
    if [ "$n" -eq 1 ]; then sha='{head}'; else sha='{moved}'; fi
    printf '%s\n' "{{\"state\":\"open\",\"head\":{{\"sha\":\"$sha\"}},\"labels\":[{{\"name\":\"{MANAGED_LABEL}\"}}]}}"
    ;;
  *) printf '%s\n' '{{"statuses":[{{"context":"{HANDOFF_CONTEXT}","state":"success","description":"Managed handoff GEN-7","target_url":"https://linear.app/example/GEN-7"}}]}}' ;;
esac
"#,
                count = count.display()
            ),
        )
        .expect("fake gh");
        let mut permissions = std::fs::metadata(&gh).expect("metadata").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&gh, permissions).expect("chmod");
        let actions = GitHubActions::new(temp.path()).with_gh_binary_for_tests(gh);

        let error = existing_handoff_receipt_is_valid(&actions, "owner/repo", &args())
            .expect_err("moved head must fail closed");
        assert!(error.message.contains("head drift"), "{}", error.message);
    }

    #[cfg(unix)]
    #[test]
    fn newest_failed_receipt_is_not_hidden_by_older_success() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("temp");
        let gh = temp.path().join("gh");
        let head = "a".repeat(40);
        std::fs::write(
            &gh,
            format!(
                r#"#!/bin/sh
case "$*" in
  *"pulls/7"*) printf '%s\n' '{{"state":"open","head":{{"sha":"{head}"}},"labels":[{{"name":"{MANAGED_LABEL}"}}]}}' ;;
  *) printf '%s\n' '{{"statuses":[{{"context":"{HANDOFF_CONTEXT}","state":"failure","description":"Managed handoff GEN-7","target_url":"https://linear.app/example/GEN-7"}},{{"context":"{HANDOFF_CONTEXT}","state":"success","description":"Managed handoff GEN-7","target_url":"https://linear.app/example/GEN-7"}}]}}' ;;
esac
"#,
            ),
        )
        .expect("fake gh");
        let mut permissions = std::fs::metadata(&gh).expect("metadata").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&gh, permissions).expect("chmod");
        let actions = GitHubActions::new(temp.path()).with_gh_binary_for_tests(gh);

        assert!(
            !existing_handoff_receipt_is_valid(&actions, "owner/repo", &args())
                .expect("receipt inspection")
        );
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
