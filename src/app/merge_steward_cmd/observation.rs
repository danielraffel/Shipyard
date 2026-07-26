use super::{
    BTreeMap, BTreeSet, CapacityPreemptionPolicy, CliFailure, Duration, GitHubActions, Instant,
    MergeQueueSnapshot, ObservedPr, Path, RepoObservation, RequiredCheck, StewardCheck, StewardJob,
    StewardPullRequest, StewardRun, Value, is_admin_protection_denied,
    is_capacity_preemption_workflow, is_full_sha, is_private_free_entitlement,
};

pub(super) fn resolve_repos(mut repos: Vec<String>, cwd: &Path) -> Result<Vec<String>, CliFailure> {
    if repos.is_empty() {
        repos.push(super::super::runner_cmd::resolve_repo_slug(None, cwd)?);
    }
    repos.sort();
    repos.dedup();
    for repo in &repos {
        let Some((owner, name)) = repo.split_once('/') else {
            return Err(CliFailure::new(
                1,
                format!("invalid repository slug `{repo}`"),
            ));
        };
        if owner.is_empty() || name.is_empty() || name.contains('/') {
            return Err(CliFailure::new(
                1,
                format!("invalid repository slug `{repo}`"),
            ));
        }
    }
    Ok(repos)
}

pub(super) fn observe_repo(
    actions: &GitHubActions,
    repo: &str,
    base: &str,
) -> Result<RepoObservation, String> {
    let settings = gh_json(
        actions,
        &["api".to_owned(), format!("repos/{repo}")],
        "repository settings",
    )?;
    let repo = canonical_repo_name(&settings)?;
    let allow_auto_merge = settings
        .get("allow_auto_merge")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let evaluated_rules = evaluated_branch_rules(actions, &repo, base)?;
    let merge_method = effective_merge_method(&settings, &evaluated_rules)?;
    let required_checks =
        required_checks_with_evaluated_rules(actions, &repo, base, &evaluated_rules)?;
    let (merge_queue, queue_positions, merge_group_heads, merge_group_enqueued_at) =
        merge_queue_snapshot(actions, &repo, base)?;
    let mut prs = pull_requests(actions, &repo, base, &queue_positions)?;
    hydrate_required_check_identities(actions, &repo, &required_checks, &mut prs)?;
    let mut runs = active_runs(actions, &repo)?;
    let capacity_preemption_policy = CapacityPreemptionPolicy::for_repository(&repo);
    let front_head = queue_positions
        .iter()
        .min_by_key(|(_, position)| **position)
        .and_then(|(number, _)| merge_group_heads.get(number))
        .map(String::as_str);
    let preemption_error = hydrate_preemption_jobs(
        actions,
        &repo,
        front_head,
        &capacity_preemption_policy,
        &mut runs,
    )
    .err();
    Ok(RepoObservation {
        repo,
        base: base.to_owned(),
        allow_auto_merge,
        merge_queue,
        merge_method,
        required_checks,
        prs,
        runs,
        merge_group_heads,
        merge_group_enqueued_at,
        capacity_preemption_policy,
        preemption_error,
    })
}

pub(super) fn canonical_repo_name(settings: &Value) -> Result<String, String> {
    let full_name = settings
        .get("full_name")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "repository settings missing canonical full_name".to_owned())?;
    let Some((owner, name)) = full_name.split_once('/') else {
        return Err("repository settings returned malformed canonical full_name".to_owned());
    };
    if owner.is_empty() || name.is_empty() || name.contains('/') {
        return Err("repository settings returned malformed canonical full_name".to_owned());
    }
    Ok(full_name.to_owned())
}

pub(super) fn gh_json(
    actions: &GitHubActions,
    args: &[String],
    purpose: &str,
) -> Result<Value, String> {
    let raw = actions
        .run_gh(args)
        .map_err(|error| format!("{purpose} failed: {error}"))?;
    serde_json::from_str(&raw)
        .map_err(|error| format!("{purpose} returned malformed JSON: {error}"))
}

pub(super) fn gh_json_timeout(
    actions: &GitHubActions,
    args: &[String],
    purpose: &str,
    timeout: Duration,
) -> Result<Value, String> {
    let raw = actions
        .run_gh_with_timeout(args, timeout)
        .map_err(|error| format!("{purpose} failed: {error}"))?;
    serde_json::from_str(&raw)
        .map_err(|error| format!("{purpose} returned malformed JSON: {error}"))
}

#[cfg(all(test, unix))]
pub(super) fn required_checks(
    actions: &GitHubActions,
    repo: &str,
    base: &str,
) -> Result<Vec<RequiredCheck>, String> {
    let evaluated_rules = evaluated_branch_rules(actions, repo, base)?;
    required_checks_with_evaluated_rules(actions, repo, base, &evaluated_rules)
}

fn required_checks_with_evaluated_rules(
    actions: &GitHubActions,
    repo: &str,
    base: &str,
    evaluated_rules: &Value,
) -> Result<Vec<RequiredCheck>, String> {
    let encoded_base = encode_path_segment(base);
    let args = vec![
        "api".to_owned(),
        format!("repos/{repo}/branches/{encoded_base}/protection/required_status_checks"),
    ];
    let raw = match actions.run_gh(&args) {
        Ok(raw) => raw,
        Err(error)
            if is_private_free_entitlement(&error.to_string())
                || is_admin_protection_denied(&error.to_string()) =>
        {
            return evaluated_required_checks(evaluated_rules);
        }
        Err(error) if error.to_string().contains("HTTP 404") => {
            return evaluated_required_checks(evaluated_rules);
        }
        Err(error) => return Err(format!("required-check policy read failed: {error}")),
    };
    let value: Value = serde_json::from_str(&raw)
        .map_err(|error| format!("required-check policy returned malformed JSON: {error}"))?;
    let mut checks = value
        .get("contexts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|context| RequiredCheck {
            context: context.to_owned(),
            app_id: None,
        })
        .collect::<Vec<_>>();
    if let Some(required) = value.get("checks").and_then(Value::as_array) {
        for check in required {
            let Some(context) = check.get("context").and_then(Value::as_str) else {
                continue;
            };
            checks.push(RequiredCheck {
                context: context.to_owned(),
                app_id: optional_app_id(check, "app_id")?,
            });
        }
    }
    checks.extend(evaluated_required_checks(evaluated_rules)?);
    Ok(normalize_required_checks(checks))
}

pub(super) fn evaluated_branch_rules(
    actions: &GitHubActions,
    repo: &str,
    base: &str,
) -> Result<Value, String> {
    let encoded_base = encode_path_segment(base);
    gh_json(
        actions,
        &[
            "api".to_owned(),
            format!("repos/{repo}/rules/branches/{encoded_base}"),
            "--paginate".to_owned(),
            "--slurp".to_owned(),
        ],
        "evaluated branch rules",
    )
}

pub(super) fn encode_path_segment(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    encoded
}

pub(super) fn evaluated_required_checks(value: &Value) -> Result<Vec<RequiredCheck>, String> {
    let rules = evaluated_rules(value)?;
    let checks = rules
        .iter()
        .filter(|rule| rule.get("type").and_then(Value::as_str) == Some("required_status_checks"))
        .flat_map(|rule| {
            rule.pointer("/parameters/required_status_checks")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .map(|check| {
            let context = check
                .get("context")
                .and_then(Value::as_str)
                .ok_or_else(|| "required status check missing context".to_owned())?;
            Ok(RequiredCheck {
                context: context.to_owned(),
                app_id: optional_app_id(check, "integration_id")?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(normalize_required_checks(checks))
}

pub(super) fn effective_merge_method(
    settings: &Value,
    evaluated: &Value,
) -> Result<Option<String>, String> {
    let mut allowed = BTreeSet::new();
    for (field, method) in [
        ("allow_merge_commit", "merge"),
        ("allow_squash_merge", "squash"),
        ("allow_rebase_merge", "rebase"),
    ] {
        if settings
            .get(field)
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            allowed.insert(method);
        }
    }

    let rules = evaluated_rules(evaluated)?;
    let mut constrained = false;
    let mut linear_history = false;
    for rule in rules {
        match rule.get("type").and_then(Value::as_str) {
            Some("required_linear_history") => linear_history = true,
            Some("pull_request") => {
                let methods = rule
                    .pointer("/parameters/allowed_merge_methods")
                    .and_then(Value::as_array)
                    .ok_or_else(|| "pull-request rule missing allowed_merge_methods".to_owned())?;
                let mut rule_allowed = BTreeSet::new();
                for method in methods {
                    let method = method
                        .as_str()
                        .ok_or_else(|| "allowed merge method was not a string".to_owned())?
                        .to_ascii_lowercase();
                    match method.as_str() {
                        "merge" => {
                            rule_allowed.insert("merge");
                        }
                        "squash" => {
                            rule_allowed.insert("squash");
                        }
                        "rebase" => {
                            rule_allowed.insert("rebase");
                        }
                        _ => return Err(format!("unknown allowed merge method `{method}`")),
                    }
                }
                allowed.retain(|method| rule_allowed.contains(method));
                constrained = true;
            }
            _ => {}
        }
    }
    if linear_history {
        allowed.remove("merge");
    }
    let preference = if constrained || linear_history {
        ["squash", "rebase", "merge"]
    } else {
        ["merge", "squash", "rebase"]
    };
    Ok(preference
        .into_iter()
        .find(|method| allowed.contains(method))
        .map(str::to_owned))
}

fn evaluated_rules(value: &Value) -> Result<Vec<&Value>, String> {
    let pages = value
        .as_array()
        .ok_or_else(|| "evaluated branch rules was not an array".to_owned())?;
    Ok(if pages.iter().all(Value::is_array) {
        pages
            .iter()
            .flat_map(|page| page.as_array().into_iter().flatten())
            .collect()
    } else {
        pages.iter().collect()
    })
}

fn normalize_required_checks(mut checks: Vec<RequiredCheck>) -> Vec<RequiredCheck> {
    checks.sort();
    checks.dedup();
    let pinned_contexts = checks
        .iter()
        .filter(|check| check.app_id.is_some())
        .map(|check| check.context.clone())
        .collect::<Vec<_>>();
    checks.retain(|check| {
        check.app_id.is_some()
            || !pinned_contexts
                .iter()
                .any(|context| context.eq_ignore_ascii_case(&check.context))
    });
    checks
}

fn optional_app_id(value: &Value, field: &str) -> Result<Option<u64>, String> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => match value.as_i64() {
            Some(-1) => Ok(None),
            Some(app_id) if app_id >= 0 => u64::try_from(app_id)
                .map(Some)
                .map_err(|_| format!("required status check has invalid {field}")),
            _ => Err(format!("required status check has invalid {field}")),
        },
    }
}

pub(super) fn merge_queue_snapshot(
    actions: &GitHubActions,
    repo: &str,
    base: &str,
) -> Result<MergeQueueSnapshot, String> {
    let (owner, name) = repo
        .split_once('/')
        .ok_or_else(|| format!("invalid repository slug `{repo}`"))?;
    let query = "query($owner:String!,$name:String!,$branch:String!){repository(owner:$owner,name:$name){mergeQueue(branch:$branch){entries(first:100){nodes{position enqueuedAt headCommit{oid} pullRequest{number}} pageInfo{hasNextPage}}}}}";
    let args = vec![
        "api".to_owned(),
        "graphql".to_owned(),
        "-f".to_owned(),
        format!("query={query}"),
        "-F".to_owned(),
        format!("owner={owner}"),
        "-F".to_owned(),
        format!("name={name}"),
        "-F".to_owned(),
        format!("branch={base}"),
    ];
    let value = match gh_json(actions, &args, "merge-queue policy") {
        Ok(value) => value,
        Err(error) if is_private_free_entitlement(&error) => {
            return Ok((false, BTreeMap::new(), BTreeMap::new(), BTreeMap::new()));
        }
        Err(error) => return Err(error),
    };
    if value
        .get("errors")
        .and_then(Value::as_array)
        .is_some_and(|errors| !errors.is_empty())
    {
        let text = value.to_string();
        if is_private_free_entitlement(&text) {
            return Ok((false, BTreeMap::new(), BTreeMap::new(), BTreeMap::new()));
        }
        return Err(format!("merge-queue GraphQL errors: {text}"));
    }
    let Some(queue) = value.pointer("/data/repository/mergeQueue") else {
        return Err("merge-queue response missing repository.mergeQueue".to_owned());
    };
    if queue.is_null() {
        return Ok((false, BTreeMap::new(), BTreeMap::new(), BTreeMap::new()));
    }
    if queue
        .pointer("/entries/pageInfo/hasNextPage")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Err("merge queue exceeds 100 entries; refusing a partial snapshot".to_owned());
    }
    let nodes = queue
        .pointer("/entries/nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| "merge-queue response missing entries.nodes".to_owned())?;
    let mut positions = BTreeMap::new();
    let mut heads = BTreeMap::new();
    let mut enqueued = BTreeMap::new();
    for node in nodes {
        let Some(number) = node.pointer("/pullRequest/number").and_then(Value::as_u64) else {
            return Err("merge-queue entry missing PR number".to_owned());
        };
        let Some(position) = node.get("position").and_then(Value::as_u64) else {
            return Err(format!("merge-queue PR #{number} missing position"));
        };
        positions.insert(number, position);
        let Some(enqueued_at) = node.get("enqueuedAt").and_then(Value::as_str) else {
            return Err(format!("merge-queue PR #{number} missing enqueuedAt"));
        };
        enqueued.insert(number, enqueued_at.to_owned());
        if let Some(head) = node
            .pointer("/headCommit/oid")
            .and_then(Value::as_str)
            .filter(|head| is_full_sha(head))
        {
            heads.insert(number, head.to_owned());
        }
    }
    Ok((true, positions, heads, enqueued))
}

pub(super) fn pull_requests(
    actions: &GitHubActions,
    repo: &str,
    base: &str,
    positions: &BTreeMap<u64, u64>,
) -> Result<Vec<ObservedPr>, String> {
    let args = vec![
        "pr".to_owned(),
        "list".to_owned(),
        "--repo".to_owned(),
        repo.to_owned(),
        "--state".to_owned(),
        "open".to_owned(),
        "--base".to_owned(),
        base.to_owned(),
        "--limit".to_owned(),
        "100".to_owned(),
        "--json".to_owned(),
        "id,number,isDraft,baseRefName,headRefOid,headRefName,mergeStateStatus,autoMergeRequest,labels,statusCheckRollup".to_owned(),
    ];
    let value = gh_json(actions, &args, "open PR list")?;
    let rows = value
        .as_array()
        .ok_or_else(|| "open PR list was not an array".to_owned())?;
    if rows.len() == 100 {
        return Err("open PR list reached 100; refusing a possibly partial snapshot".to_owned());
    }
    rows.iter().map(|row| parse_pr(row, positions)).collect()
}

pub(super) fn parse_pr(row: &Value, positions: &BTreeMap<u64, u64>) -> Result<ObservedPr, String> {
    let number = row
        .get("number")
        .and_then(Value::as_u64)
        .ok_or_else(|| "PR row missing number".to_owned())?;
    let node_id = row
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| format!("PR #{number} missing node ID"))?
        .to_owned();
    let rollup = row.get("statusCheckRollup").and_then(Value::as_array);
    let check_rollup_maybe_truncated = rollup.is_some_and(|checks| checks.len() == 100);
    let checks = rollup
        .into_iter()
        .flatten()
        .filter_map(parse_check)
        .collect();
    Ok(ObservedPr {
        node_id,
        fact: StewardPullRequest {
            number,
            head_sha: row
                .get("headRefOid")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            head_branch: row
                .get("headRefName")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            draft: row.get("isDraft").and_then(Value::as_bool).unwrap_or(true),
            merge_state: row
                .get("mergeStateStatus")
                .and_then(Value::as_str)
                .unwrap_or("UNKNOWN")
                .to_owned(),
            auto_merge_active: row
                .get("autoMergeRequest")
                .is_some_and(|request| !request.is_null()),
            queue_position: positions.get(&number).copied(),
            labels: row
                .get("labels")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|label| label.get("name").and_then(Value::as_str))
                .map(str::to_owned)
                .collect(),
            checks,
        },
        check_rollup_maybe_truncated,
    })
}

pub(super) fn parse_check(value: &Value) -> Option<StewardCheck> {
    match value.get("__typename").and_then(Value::as_str)? {
        "CheckRun" => Some(StewardCheck {
            name: value.get("name")?.as_str()?.to_owned(),
            app_id: value
                .pointer("/checkSuite/app/databaseId")
                .or_else(|| value.pointer("/app/id"))
                .and_then(Value::as_u64),
            status: value.get("status")?.as_str()?.to_owned(),
            conclusion: value
                .get("conclusion")
                .and_then(Value::as_str)
                .filter(|conclusion| !conclusion.is_empty())
                .map(str::to_owned),
            run_id: value
                .get("detailsUrl")
                .and_then(Value::as_str)
                .and_then(run_id_from_url),
            observed_at: value
                .get("completedAt")
                .and_then(Value::as_str)
                .or_else(|| value.get("startedAt").and_then(Value::as_str))
                .or_else(|| value.get("createdAt").and_then(Value::as_str))
                .map(str::to_owned),
        }),
        "StatusContext" => {
            let state = value.get("state")?.as_str()?;
            Some(StewardCheck {
                name: value.get("context")?.as_str()?.to_owned(),
                app_id: None,
                status: if matches!(state, "PENDING" | "EXPECTED") {
                    "IN_PROGRESS"
                } else {
                    "COMPLETED"
                }
                .to_owned(),
                conclusion: match state {
                    "SUCCESS" => Some("SUCCESS".to_owned()),
                    "ERROR" | "FAILURE" => Some("FAILURE".to_owned()),
                    _ => None,
                },
                run_id: value
                    .get("targetUrl")
                    .and_then(Value::as_str)
                    .and_then(run_id_from_url),
                observed_at: value
                    .get("createdAt")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            })
        }
        _ => None,
    }
}

pub(super) fn hydrate_required_check_identities(
    actions: &GitHubActions,
    repo: &str,
    required_checks: &[RequiredCheck],
    prs: &mut [ObservedPr],
) -> Result<(), String> {
    if !required_checks.iter().any(|check| check.app_id.is_some()) {
        for pr in prs {
            if pr.check_rollup_maybe_truncated {
                hydrate_complete_head_checks(actions, repo, pr)?;
            }
        }
        return Ok(());
    }
    for pr in prs {
        if !is_full_sha(&pr.fact.head_sha) {
            continue;
        }
        if pr.check_rollup_maybe_truncated {
            hydrate_complete_head_checks(actions, repo, pr)?;
        } else {
            pr.fact
                .checks
                .extend(check_runs_for_head(actions, repo, &pr.fact.head_sha)?);
        }
    }
    Ok(())
}

fn hydrate_complete_head_checks(
    actions: &GitHubActions,
    repo: &str,
    pr: &mut ObservedPr,
) -> Result<(), String> {
    if !is_full_sha(&pr.fact.head_sha) {
        return Ok(());
    }
    let mut checks = check_runs_for_head(actions, repo, &pr.fact.head_sha)?;
    checks.extend(commit_statuses_for_head(actions, repo, &pr.fact.head_sha)?);
    pr.fact.checks = checks;
    pr.check_rollup_maybe_truncated = false;
    Ok(())
}

pub(super) fn check_runs_for_head(
    actions: &GitHubActions,
    repo: &str,
    head_sha: &str,
) -> Result<Vec<StewardCheck>, String> {
    let mut checks = Vec::new();
    for page in 1..=10 {
        let value = gh_json(
            actions,
            &[
                "api".to_owned(),
                format!("repos/{repo}/commits/{head_sha}/check-runs?per_page=100&page={page}"),
            ],
            "current-head check identities",
        )?;
        let rows = value
            .get("check_runs")
            .and_then(Value::as_array)
            .ok_or_else(|| "current-head check identities missing check_runs".to_owned())?;
        let count = rows.len();
        checks.extend(rows.iter().filter_map(parse_rest_check));
        if count < 100 {
            return Ok(checks);
        }
    }
    Err("current-head check runs exceed 1000; refusing partial identity scan".to_owned())
}

pub(super) fn commit_statuses_for_head(
    actions: &GitHubActions,
    repo: &str,
    head_sha: &str,
) -> Result<Vec<StewardCheck>, String> {
    let mut checks = Vec::new();
    for page in 1..=10 {
        let value = gh_json(
            actions,
            &[
                "api".to_owned(),
                format!("repos/{repo}/commits/{head_sha}/statuses?per_page=100&page={page}"),
            ],
            "current-head commit statuses",
        )?;
        let rows = value
            .as_array()
            .ok_or_else(|| "current-head commit statuses were not an array".to_owned())?;
        let count = rows.len();
        checks.extend(rows.iter().filter_map(parse_rest_status));
        if count < 100 {
            return Ok(checks);
        }
    }
    Err("current-head commit statuses exceed 1000; refusing partial status scan".to_owned())
}

pub(super) fn parse_rest_check(value: &Value) -> Option<StewardCheck> {
    Some(StewardCheck {
        name: value.get("name")?.as_str()?.to_owned(),
        app_id: value.pointer("/app/id").and_then(Value::as_u64),
        status: value.get("status")?.as_str()?.to_owned(),
        conclusion: value
            .get("conclusion")
            .and_then(Value::as_str)
            .filter(|conclusion| !conclusion.is_empty())
            .map(str::to_owned),
        run_id: value
            .get("details_url")
            .and_then(Value::as_str)
            .and_then(run_id_from_url),
        observed_at: value
            .get("completed_at")
            .and_then(Value::as_str)
            .or_else(|| value.get("started_at").and_then(Value::as_str))
            .map(str::to_owned),
    })
}

pub(super) fn parse_rest_status(value: &Value) -> Option<StewardCheck> {
    let state = value.get("state")?.as_str()?;
    Some(StewardCheck {
        name: value.get("context")?.as_str()?.to_owned(),
        app_id: None,
        status: if state.eq_ignore_ascii_case("pending") {
            "IN_PROGRESS"
        } else {
            "COMPLETED"
        }
        .to_owned(),
        conclusion: if state.eq_ignore_ascii_case("success") {
            Some("SUCCESS".to_owned())
        } else if matches!(state.to_ascii_lowercase().as_str(), "error" | "failure") {
            Some("FAILURE".to_owned())
        } else {
            None
        },
        run_id: value
            .get("target_url")
            .and_then(Value::as_str)
            .and_then(run_id_from_url),
        observed_at: value
            .get("updated_at")
            .and_then(Value::as_str)
            .or_else(|| value.get("created_at").and_then(Value::as_str))
            .map(str::to_owned),
    })
}

pub(super) fn run_id_from_url(url: &str) -> Option<u64> {
    let tail = url.split("/actions/runs/").nth(1)?;
    tail.split('/').next()?.parse().ok()
}

pub(super) fn active_runs(actions: &GitHubActions, repo: &str) -> Result<Vec<StewardRun>, String> {
    let mut all = Vec::new();
    for status in ["queued", "waiting", "pending", "requested", "in_progress"] {
        for page in 1..=10 {
            let value = gh_json(
                actions,
                &[
                    "api".to_owned(),
                    format!("repos/{repo}/actions/runs?status={status}&per_page=100&page={page}"),
                ],
                "active workflow runs",
            )?;
            let rows = value
                .get("workflow_runs")
                .and_then(Value::as_array)
                .ok_or_else(|| "active workflow response missing workflow_runs".to_owned())?;
            let count = rows.len();
            all.extend(rows.iter().filter_map(parse_run));
            if count < 100 {
                break;
            }
            if page == 10 {
                return Err("active workflow runs exceed 1000; refusing partial scan".to_owned());
            }
        }
    }
    all.sort_unstable_by_key(|run| run.id);
    all.dedup_by_key(|run| run.id);
    Ok(all)
}

pub(super) fn parse_run(value: &Value) -> Option<StewardRun> {
    let pull_request_number = value
        .get("pull_requests")
        .and_then(Value::as_array)
        .filter(|pull_requests| pull_requests.len() == 1)
        .and_then(|pull_requests| pull_requests[0].get("number"))
        .and_then(Value::as_u64);
    Some(StewardRun {
        id: value.get("id")?.as_u64()?,
        workflow_id: value.get("workflow_id")?.as_u64()?,
        run_attempt: value
            .get("run_attempt")
            .and_then(Value::as_u64)
            .unwrap_or(1),
        workflow: value.get("name")?.as_str()?.to_owned(),
        head_sha: value.get("head_sha")?.as_str()?.to_owned(),
        head_branch: value.get("head_branch")?.as_str()?.to_owned(),
        status: value.get("status")?.as_str()?.to_owned(),
        event: value.get("event")?.as_str()?.to_owned(),
        pull_request_number,
        created_at: value.get("created_at")?.as_str()?.to_owned(),
        jobs: Vec::new(),
    })
}

pub(super) fn hydrate_preemption_jobs(
    actions: &GitHubActions,
    repo: &str,
    front_head: Option<&str>,
    policy: &CapacityPreemptionPolicy,
    runs: &mut [StewardRun],
) -> Result<(), String> {
    if !policy.is_enabled() {
        return Ok(());
    }
    let Some(front_head) = front_head else {
        return Ok(());
    };
    for run in runs {
        let is_front_candidate =
            run.event == "merge_group" && front_head.eq_ignore_ascii_case(&run.head_sha);
        let is_preemption_candidate = run.event == "pull_request"
            && run.status.eq_ignore_ascii_case("in_progress")
            && is_capacity_preemption_workflow(&run.workflow, policy);
        if is_front_candidate || is_preemption_candidate {
            run.jobs = fetch_run_jobs(actions, repo, run.id)?;
        }
    }
    Ok(())
}

pub(super) fn fetch_run_jobs(
    actions: &GitHubActions,
    repo: &str,
    run_id: u64,
) -> Result<Vec<StewardJob>, String> {
    let mut all = Vec::new();
    for page in 1..=10 {
        let value = gh_json(
            actions,
            &[
                "api".to_owned(),
                format!(
                    "repos/{repo}/actions/runs/{run_id}/jobs?filter=all&per_page=100&page={page}"
                ),
            ],
            "workflow jobs",
        )?;
        let rows = value
            .get("jobs")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("workflow run {run_id} response missing jobs"))?;
        let count = rows.len();
        for row in rows {
            all.push(parse_job(row)?);
        }
        if count < 100 {
            return Ok(all);
        }
    }
    Err(format!(
        "workflow run {run_id} exceeds 1000 jobs; refusing partial scan"
    ))
}

pub(super) fn fetch_run_jobs_before(
    actions: &GitHubActions,
    repo: &str,
    run_id: u64,
    deadline: Instant,
) -> Result<Vec<StewardJob>, String> {
    let mut all = Vec::new();
    for page in 1..=10 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(format!("workflow run {run_id} job fetch timed out"));
        }
        let value = gh_json_timeout(
            actions,
            &[
                "api".to_owned(),
                format!(
                    "repos/{repo}/actions/runs/{run_id}/jobs?filter=all&per_page=100&page={page}"
                ),
            ],
            "workflow jobs",
            remaining,
        )?;
        let rows = value
            .get("jobs")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("workflow run {run_id} response missing jobs"))?;
        let count = rows.len();
        for row in rows {
            all.push(parse_job(row)?);
        }
        if count < 100 {
            return Ok(all);
        }
    }
    Err(format!(
        "workflow run {run_id} exceeds 1000 jobs; refusing partial scan"
    ))
}

pub(super) fn parse_job(value: &Value) -> Result<StewardJob, String> {
    Ok(StewardJob {
        name: value
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| "workflow job missing name".to_owned())?
            .to_owned(),
        status: value
            .get("status")
            .and_then(Value::as_str)
            .ok_or_else(|| "workflow job missing status".to_owned())?
            .to_owned(),
        conclusion: value
            .get("conclusion")
            .and_then(Value::as_str)
            .map(str::to_owned),
        labels: value
            .get("labels")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        runner_name: value
            .get("runner_name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .map(str::to_owned),
    })
}
