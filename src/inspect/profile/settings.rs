//! Facts read from repository settings and from recent runs.

use serde_json::Value;

use super::{
    Evidence, FactKind, Facts, Look, LookStatus, RunSample, Snapshot, Source, look, unavailable,
};

pub(super) fn runtime_runner_facts(runs: &RunSample, facts: &mut Facts, looks: &mut Vec<Look>) {
    for run in &runs.sampled {
        for job in &run.jobs {
            let Some(runner) = job
                .get("runner_name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
            else {
                continue;
            };
            let labels: Vec<&str> = job
                .get("labels")
                .and_then(Value::as_array)
                .map(|labels| labels.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let group = job
                .get("runner_group_name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let hosted = group == "GitHub Actions";
            let value = format!(
                "{} [{}]",
                if hosted {
                    "github-hosted"
                } else {
                    "self-hosted"
                },
                labels.join(", ")
            );
            facts.add(
                FactKind::RunnerClass,
                &value,
                Source::Runs,
                false,
                Evidence {
                    at: format!("run {} ({})", run.run_id, run.workflow),
                    text: format!(
                        "job {} ran on {runner}{}",
                        job.get("name").and_then(Value::as_str).unwrap_or("?"),
                        if group.is_empty() {
                            String::new()
                        } else {
                            format!(" (group {group})")
                        }
                    ),
                },
            );
        }
    }
    let searched = format!(
        "the runner of each job in the latest completed run of the {} workflow(s) whose runs-on is computed",
        runs.wanted.len()
    );
    let found = facts.count(FactKind::RunnerClass, Source::Runs);
    let mut entry = look(FactKind::RunnerClass, Source::Runs, found, searched);
    let mut notes = Vec::new();
    if !runs.over_budget.is_empty() {
        notes.push(format!(
            "not read (over the run budget): {}",
            runs.over_budget.join(", ")
        ));
    }
    if !runs.errors.is_empty() {
        notes.push(format!(
            "read failed: {}",
            runs.errors
                .iter()
                .map(|(workflow, error)| format!("{workflow}: {error}"))
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }
    if runs.sampled.iter().any(|run| run.partial) {
        notes.push("a run listed more jobs than one page returned".to_owned());
    }
    if !runs.no_runs.is_empty() {
        notes.push(format!("no completed run: {}", runs.no_runs.join(", ")));
    }
    if !runs.over_budget.is_empty()
        || !runs.errors.is_empty()
        || runs.sampled.iter().any(|run| run.partial)
    {
        entry.status = if found == 0 && runs.sampled.is_empty() && !runs.errors.is_empty() {
            LookStatus::Unavailable
        } else {
            LookStatus::Partial
        };
    }
    if !notes.is_empty() {
        entry.note = Some(notes.join("; "));
    }
    looks.push(entry);
}

pub(super) fn settings_facts(
    repo: &str,
    base: &str,
    snapshot: &Snapshot,
    facts: &mut Facts,
    looks: &mut Vec<Look>,
) {
    let rules_at = format!("api:repos/{repo}/rules/branches/{base}");
    match &snapshot.rules {
        Ok(rules) => queue_fact(base, &rules_at, rules, facts, looks),
        Err(error) => looks.push(unavailable(
            FactKind::MergeQueue,
            Source::Settings,
            format!("active rules on {base}"),
            error,
        )),
    }
    required_check_facts(repo, base, &rules_at, snapshot, facts, looks);
}

fn queue_fact(base: &str, at: &str, rules: &Value, facts: &mut Facts, looks: &mut Vec<Look>) {
    let queue = rules
        .as_array()
        .into_iter()
        .flatten()
        .find(|rule| rule.get("type").and_then(Value::as_str) == Some("merge_queue"));
    if let Some(rule) = queue {
        let parameters = rule.get("parameters").cloned().unwrap_or(Value::Null);
        let detail = |key: &str| {
            parameters.get(key).map_or_else(
                || "?".to_owned(),
                |value| value.to_string().trim_matches('"').to_owned(),
            )
        };
        facts.add(
            FactKind::MergeQueue,
            "present",
            Source::Settings,
            false,
            Evidence {
                at: at.to_owned(),
                text: format!(
                    "merge_queue rule: grouping {}, max_entries_to_merge {}, merge_method {}",
                    detail("grouping_strategy"),
                    detail("max_entries_to_merge"),
                    detail("merge_method"),
                ),
            },
        );
    }
    let mut entry = look(
        FactKind::MergeQueue,
        Source::Settings,
        usize::from(queue.is_some()),
        format!("active rules on {base}"),
    );
    if queue.is_none() {
        entry.note = Some(
            "branch protection cannot express a queue, so the rules are the only surface read"
                .to_owned(),
        );
    }
    looks.push(entry);
}

fn required_check_facts(
    repo: &str,
    base: &str,
    rules_at: &str,
    snapshot: &Snapshot,
    facts: &mut Facts,
    looks: &mut Vec<Look>,
) {
    let protection_at =
        format!("api:repos/{repo}/branches/{base}/protection/required_status_checks");
    let from_rules = snapshot
        .rules
        .as_ref()
        .ok()
        .and_then(|rules| crate::required_check_policy::evaluated_required_checks(rules).ok())
        .unwrap_or_default();
    let from_protection = match &snapshot.protection {
        Ok(Some(protection)) => {
            crate::required_check_policy::classic_required_checks(protection).unwrap_or_default()
        }
        _ => Vec::new(),
    };
    for (checks, at, text) in [
        (from_rules, rules_at, "required_status_checks rule"),
        (from_protection, protection_at.as_str(), "branch protection"),
    ] {
        for check in checks {
            facts.add(
                FactKind::RequiredCheck,
                &check.context,
                Source::Settings,
                false,
                Evidence {
                    at: at.to_owned(),
                    text: text.to_owned(),
                },
            );
        }
    }
    let found = facts.count(FactKind::RequiredCheck, Source::Settings);
    let errors: Vec<String> = [
        snapshot
            .rules
            .as_ref()
            .err()
            .map(|error| format!("rules: {error}")),
        snapshot
            .protection
            .as_ref()
            .err()
            .map(|error| format!("protection: {error}")),
    ]
    .into_iter()
    .flatten()
    .collect();
    let mut entry = look(
        FactKind::RequiredCheck,
        Source::Settings,
        found,
        format!("active rules and branch protection on {base}"),
    );
    if !errors.is_empty() {
        entry.status = if errors.len() == 2 {
            LookStatus::Unavailable
        } else {
            LookStatus::Partial
        };
        entry.note = Some(errors.join("; "));
    } else if matches!(snapshot.protection, Ok(None)) && found == 0 {
        entry.note = Some(format!(
            "{base} has no branch protection and no required-check rule"
        ));
    }
    looks.push(entry);
}
