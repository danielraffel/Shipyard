//! Keep two heads that change the same generated path out of one batch.
//!
//! A generated path is a file regenerated from the rest of the tree (a ctest
//! inputs manifest, a capability catalogue), named per repository in the
//! base's `[auto_merge] generated_paths` ([`EjectionGuards`]). Two pull
//! requests that both change one either conflict textually when the queue
//! stacks them, or each regenerate it against a tree without the other, so
//! the merge group's drift check fails. Neither is fixed by a same-head
//! retry: each needs a regenerate commit on top of the other. On Pulp over one
//! week that was 19 of 59 ejections.
//!
//! So before an enqueue the steward reads the queue live and, when another
//! entry is queued, the candidate's changed files; when it changes a
//! generated path and another queued entry changes that same path, the
//! candidate waits (`generated_path_batch:#<n>`) and is admitted on the first
//! pass after `#<n>` leaves the queue. The steward re-reads the
//! queue live for this, so a pull request it enqueued earlier in the same
//! pass counts.
//!
//! Cost: one GraphQL `files(first:100)` page per pull request, cached for the
//! pass, and only on a base that lists generated paths while another entry is
//! queued. The queued entries' files are read only when the candidate changes
//! a generated path. A list longer
//! than one page is paged to GitHub's 3000-file ceiling; one that is still
//! incomplete is treated as possibly changing any generated path, which can
//! only defer.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use super::{GitHubActions, MutationApplyContext, ObservedPr, RepoObservation};
use crate::merge_steward::EjectionGuards;

/// `blocked_on` / mutation prefix of a head deferred behind a queued entry.
pub(super) const BATCH_PREFIX: &str = "generated_path_batch:";

/// Pages read before a file list is called incomplete (GitHub stops listing a
/// pull request's files at 3000).
const MAX_FILE_PAGES: usize = 30;

const FILES_QUERY: &str = "query($owner:String!,$name:String!,$number:Int!,$cursor:String){repository(owner:$owner,name:$name){pullRequest(number:$number){files(first:100,after:$cursor){nodes{path} pageInfo{hasNextPage endCursor}}}}}";

/// One pull request's changed paths as read this pass.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct ChangedFiles {
    /// Changed paths.
    pub paths: BTreeSet<String>,
    /// Whether every page was read.
    pub complete: bool,
}

/// Changed-file reads made during one pass, by pull-request number.
pub(super) type PassCache = BTreeMap<u64, ChangedFiles>;

/// The base's ejection guards, read once per repository pass.
pub(super) fn read_ejection_guards(
    actions: &GitHubActions,
    observation: &RepoObservation,
) -> EjectionGuards {
    EjectionGuards::from_config(
        &|argv: &[String]| actions.run_gh(argv).map_err(|error| error.to_string()),
        &observation.repo,
        &observation.base,
        observation.merge_queue,
    )
}

/// The mutation token deferring a head behind queued `number`, with the
/// shared generated paths after `@` (`|`-joined; mutation parts are
/// comma-joined).
#[must_use]
pub(super) fn token(number: u64, shared: &[String]) -> String {
    format!("{BATCH_PREFIX}#{number}@{}", shared.join("|"))
}

/// Split a mutation part into its `blocked_on` token and the shared paths.
#[must_use]
pub(super) fn parse_token(part: &str) -> Option<(String, Option<String>)> {
    part.strip_prefix(BATCH_PREFIX)?;
    Some(match part.split_once('@') {
        Some((token, paths)) => (token.to_owned(), Some(paths.replace('|', ", "))),
        None => (part.to_owned(), None),
    })
}

/// Read `number`'s changed files, paged, through `run_gh`.
pub(super) fn read_changed_files(
    actions: &GitHubActions,
    repo: &str,
    number: u64,
) -> Result<ChangedFiles, String> {
    let (owner, name) = repo
        .split_once('/')
        .ok_or_else(|| format!("invalid repository slug `{repo}`"))?;
    let mut files = ChangedFiles::default();
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_FILE_PAGES {
        let mut args = vec![
            "api".to_owned(),
            "graphql".to_owned(),
            "-f".to_owned(),
            format!("query={FILES_QUERY}"),
            "-F".to_owned(),
            format!("owner={owner}"),
            "-F".to_owned(),
            format!("name={name}"),
            "-F".to_owned(),
            format!("number={number}"),
        ];
        if let Some(cursor) = cursor.as_deref() {
            args.extend(["-f".to_owned(), format!("cursor={cursor}")]);
        }
        let raw = actions
            .run_gh(&args)
            .map_err(|error| format!("changed files of #{number} unreadable: {error}"))?;
        let value: Value = serde_json::from_str(&raw)
            .map_err(|error| format!("changed files of #{number} were not JSON: {error}"))?;
        if let Some(errors) = value.get("errors").filter(|errors| !errors.is_null()) {
            return Err(format!("changed files of #{number} unreadable: {errors}"));
        }
        let pull = value
            .pointer("/data/repository/pullRequest")
            .filter(|pull| pull.is_object())
            .ok_or_else(|| format!("changed files of #{number}: no pull request in the answer"))?;
        let connection = pull
            .get("files")
            .filter(|files| files.is_object())
            .ok_or_else(|| format!("changed files of #{number}: no file list in the answer"))?;
        for node in connection
            .get("nodes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(path) = node.get("path").and_then(Value::as_str) {
                files.paths.insert(path.to_owned());
            }
        }
        let has_next = connection
            .pointer("/pageInfo/hasNextPage")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !has_next {
            files.complete = true;
            return Ok(files);
        }
        cursor = connection
            .pointer("/pageInfo/endCursor")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if cursor.is_none() {
            break;
        }
    }
    Ok(files)
}

fn cached_files(context: &MutationApplyContext<'_>, number: u64) -> Result<ChangedFiles, String> {
    if let Some(files) = context.changed_files.borrow().get(&number) {
        return Ok(files.clone());
    }
    let files = read_changed_files(context.actions, &context.observation.repo, number)?;
    context
        .changed_files
        .borrow_mut()
        .insert(number, files.clone());
    Ok(files)
}

/// What one candidate shares with one queued entry, judged from the two file
/// lists: the shared generated paths, or `None` when they share none.
///
/// An incomplete list may hide any generated path, so it is compared by the
/// other side's generated paths alone: that can only defer, never admit a
/// collision.
#[must_use]
pub(super) fn shared_generated_paths(
    guards: &EjectionGuards,
    candidate: &ChangedFiles,
    queued: &ChangedFiles,
) -> Option<Vec<String>> {
    let mine = guards.generated_in(candidate.paths.iter().map(String::as_str));
    let theirs = guards.generated_in(queued.paths.iter().map(String::as_str));
    let shared = match (candidate.complete, queued.complete) {
        (true, true) => mine
            .iter()
            .filter(|path| theirs.contains(path))
            .cloned()
            .collect::<Vec<_>>(),
        (false, true) => theirs,
        (true, false) => mine,
        (false, false) => mine.into_iter().chain(theirs).collect(),
    };
    let mut shared = shared;
    shared.sort();
    shared.dedup();
    if shared.is_empty() {
        // Two incomplete lists with no generated path visible on either side
        // still cannot be told apart from a collision.
        (!candidate.complete && !queued.complete)
            .then(|| vec!["(file lists incomplete on both sides)".to_owned()])
    } else {
        Some(shared)
    }
}

/// The batch guard's verdict on enqueueing `pr` now.
///
/// `None` admits. `Some(Ok(token))` defers behind the first queued entry (in
/// queue order) that shares a generated path; `Some(Err(_))` is a read that
/// failed, which never admits.
pub(super) fn batch_refusal(
    context: &MutationApplyContext<'_>,
    guards: &EjectionGuards,
    pr: &ObservedPr,
) -> Option<Result<String, String>> {
    // The pass-start config read failed: the generated paths are unknown, and
    // a failed read is never cached, so a later approval read can succeed.
    // Re-read live; a second failure refuses rather than admitting unguarded.
    let reread;
    let guards = if guards.read_error.is_some() && guards.batch_guard {
        reread = read_ejection_guards(context.actions, context.observation);
        if let Some(error) = &reread.read_error {
            return Some(Err(format!(
                "generated-path batch guard: base config unreadable, so a collision cannot be \
                 ruled out: {error}"
            )));
        }
        &reread
    } else {
        guards
    };
    if !guards.batch_guard_active() {
        return None;
    }
    // The queue first: with no other entry there is nothing to collide with,
    // and no file list is read at all.
    let positions = match super::merge_queue_snapshot(
        context.actions,
        &context.observation.repo,
        &context.observation.base,
    ) {
        Ok((_, positions, _, _)) => positions,
        Err(error) => {
            return Some(Err(format!(
                "merge queue unreadable for the generated-path batch guard: {error}"
            )));
        }
    };
    if positions.keys().all(|number| *number == pr.fact.number) {
        return None;
    }
    let candidate = match cached_files(context, pr.fact.number) {
        Ok(files) => files,
        Err(error) => return Some(Err(error)),
    };
    if candidate.complete
        && guards
            .generated_in(candidate.paths.iter().map(String::as_str))
            .is_empty()
    {
        return None;
    }
    let mut queued = positions
        .into_iter()
        .filter(|(number, _)| *number != pr.fact.number)
        .collect::<Vec<_>>();
    queued.sort_by_key(|(number, position)| (*position, *number));
    for (number, _) in queued {
        let files = match cached_files(context, number) {
            Ok(files) => files,
            Err(error) => return Some(Err(error)),
        };
        if let Some(shared) = shared_generated_paths(guards, &candidate, &files) {
            return Some(Ok(token(number, &shared)));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guards() -> EjectionGuards {
        EjectionGuards {
            generated_paths: vec![
                "test/ctest_script_inputs.json".to_owned(),
                "test/ctest_script_inputs/**".to_owned(),
                "docs/status/agent-capability-surface.json".to_owned(),
            ],
            ..EjectionGuards::for_base(true)
        }
    }

    fn files(paths: &[&str], complete: bool) -> ChangedFiles {
        ChangedFiles {
            paths: paths.iter().map(|path| (*path).to_owned()).collect(),
            complete,
        }
    }

    #[test]
    fn only_a_shared_generated_path_defers() {
        let guards = guards();
        let candidate = files(&["test/ctest_script_inputs.json", "core/a.cpp"], true);
        assert_eq!(
            shared_generated_paths(
                &guards,
                &candidate,
                &files(&["test/ctest_script_inputs.json", "core/b.cpp"], true)
            ),
            Some(vec!["test/ctest_script_inputs.json".to_owned()])
        );
        // Same source file, different generated files: no generated overlap.
        assert_eq!(
            shared_generated_paths(
                &guards,
                &candidate,
                &files(
                    &["core/a.cpp", "docs/status/agent-capability-surface.json"],
                    true
                )
            ),
            None
        );
        // Two shards of a sharded generated directory do not collide.
        assert_eq!(
            shared_generated_paths(
                &guards,
                &files(&["test/ctest_script_inputs/a.json"], true),
                &files(&["test/ctest_script_inputs/b.json"], true)
            ),
            None
        );
    }

    #[test]
    fn an_incomplete_list_can_only_defer() {
        let guards = guards();
        let big = files(&["core/a.cpp"], false);
        assert_eq!(
            shared_generated_paths(
                &guards,
                &big,
                &files(&["docs/status/agent-capability-surface.json"], true)
            ),
            Some(vec!["docs/status/agent-capability-surface.json".to_owned()])
        );
        assert_eq!(
            shared_generated_paths(&guards, &big, &files(&["core/b.cpp"], true)),
            None
        );
        assert!(shared_generated_paths(&guards, &big, &files(&[], false)).is_some());
    }

    #[test]
    fn the_token_round_trips_to_its_blocked_on_form() {
        let token = token(
            11719,
            &[
                "docs/status/agent-capability-surface.json".to_owned(),
                "test/ctest_script_inputs.json".to_owned(),
            ],
        );
        assert_eq!(
            parse_token(&token),
            Some((
                "generated_path_batch:#11719".to_owned(),
                Some(
                    "docs/status/agent-capability-surface.json, test/ctest_script_inputs.json"
                        .to_owned()
                )
            ))
        );
        assert_eq!(parse_token("waiting_head_approval"), None);
    }
}
