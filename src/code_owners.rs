//! Whether a pull request still waits on a code-owner review, derived from
//! the base's `CODEOWNERS` and the reviews GitHub recorded on the exact head.
//!
//! A ruleset can require a code-owner review while requiring zero general
//! approvals. GitHub's `reviewDecision` is then `null` whether or not the code
//! owner approved, so it cannot answer the question. This module answers it
//! from the same facts GitHub enforces: the owners `CODEOWNERS` (at the base)
//! assigns to each changed file, and each reviewer's latest review.
//!
//! The answer is only ever a reason *not* to ask GitHub. Anything this module
//! cannot judge exactly (a team or e-mail owner nobody listed by login
//! approved, the PR author owning a file, a diff past GitHub's file-list cap)
//! is [`CodeOwnerStatus::Undetermined`], and the caller asks GitHub, which
//! stays the authority.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::head_approval::RunGh;

/// Where GitHub looks for `CODEOWNERS`, in the order it looks; the first one
/// present wins.
pub const CODEOWNERS_PATHS: [&str; 3] = [".github/CODEOWNERS", "CODEOWNERS", "docs/CODEOWNERS"];

/// GitHub's pull-request file list stops at this many files.
const FILE_LIST_CAP: usize = 3000;

/// The verdict on one exact head.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CodeOwnerStatus {
    /// The base does not require a code-owner review, has no `CODEOWNERS`,
    /// or no changed file has an owner.
    NotRequired,
    /// Every owned file has an approval on the exact head from one of its
    /// owners.
    Satisfied {
        /// The owners whose exact-head approval satisfied the rule.
        approvers: Vec<String>,
    },
    /// At least one owned file has no exact-head approval from any of its
    /// owners. `owners` lists the owners who could satisfy it, as written in
    /// `CODEOWNERS` without the `@`.
    Missing {
        /// Owners who could still approve, sorted and deduplicated.
        owners: Vec<String>,
        /// Up to a few of the files that wait, for the operator.
        files: Vec<String>,
    },
    /// The requirement could not be judged exactly; ask GitHub.
    Undetermined(String),
}

/// One `CODEOWNERS` rule: a pattern and the owners it assigns.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Rule {
    pattern: Pattern,
    owners: Vec<String>,
}

/// A parsed `CODEOWNERS` file. The last rule that matches a path wins.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CodeOwners {
    rules: Vec<Rule>,
}

impl CodeOwners {
    /// Parse `CODEOWNERS` text. Lines GitHub rejects (negation, character
    /// classes, escapes) are skipped, as GitHub skips them.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let rules = text
            .trim_start_matches('\u{feff}')
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    return None;
                }
                let mut tokens = line.split_whitespace();
                let pattern = Pattern::parse(tokens.next()?)?;
                let owners = tokens
                    .take_while(|token| !token.starts_with('#'))
                    .map(|owner| owner.trim_start_matches('@').to_owned())
                    .filter(|owner| !owner.is_empty())
                    .collect();
                Some(Rule { pattern, owners })
            })
            .collect();
        Self { rules }
    }

    /// The owners of `path`: those of the last matching rule. `None` when no
    /// rule matches or the matching rule names nobody.
    #[must_use]
    pub fn owners_of(&self, path: &str) -> Option<&[String]> {
        self.rules
            .iter()
            .rev()
            .find(|rule| rule.pattern.matches(path))
            .map(|rule| rule.owners.as_slice())
            .filter(|owners| !owners.is_empty())
    }
}

/// A `CODEOWNERS` path pattern, split into `/`-separated segments.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Pattern {
    segments: Vec<String>,
    /// A trailing `/`: the pattern names a directory, never a file.
    directory_only: bool,
    /// Whether a match on a leading directory owns everything under it.
    /// `docs/*` owns only the direct children of `docs`, so it does not.
    owns_contents: bool,
}

impl Pattern {
    fn parse(raw: &str) -> Option<Self> {
        if raw.starts_with('!') || raw.contains(['[', ']', '\\']) {
            return None;
        }
        let directory_only = raw.ends_with('/') && raw.len() > 1;
        let anchored_at_root = raw.starts_with('/');
        let trimmed = raw.trim_start_matches('/').trim_end_matches('/');
        if trimmed.is_empty() {
            return None;
        }
        // A slash anywhere but the end anchors the pattern at the root, as
        // in `.gitignore`; otherwise it matches at any depth.
        let anchored = anchored_at_root || trimmed.contains('/');
        let mut segments: Vec<String> = trimmed.split('/').map(str::to_owned).collect();
        if !anchored {
            segments.insert(0, "**".to_owned());
        }
        let last = segments.last().map_or("", String::as_str);
        let owns_contents = last == "**" || !last.contains('*');
        Some(Self {
            segments,
            directory_only,
            owns_contents,
        })
    }

    fn matches(&self, path: &str) -> bool {
        let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
        if !self.directory_only && match_segments(&self.segments, &parts) {
            return true;
        }
        if !(self.owns_contents || self.directory_only) {
            return false;
        }
        // A match on a leading directory owns the files inside it.
        (1..parts.len()).any(|len| match_segments(&self.segments, &parts[..len]))
    }
}

fn match_segments(pattern: &[String], path: &[&str]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((first, rest)) if first == "**" => {
            (0..=path.len()).any(|skip| match_segments(rest, &path[skip..]))
        }
        Some((first, rest)) => path.split_first().is_some_and(|(segment, tail)| {
            glob_segment(first, segment) && match_segments(rest, tail)
        }),
    }
}

/// `*` (any run of characters) and `?` (one character) within one segment.
fn glob_segment(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut p, mut t) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some((p, t));
            p += 1;
        } else if let Some((star_p, star_t)) = star {
            p = star_p + 1;
            t = star_t + 1;
            star = Some((star_p, star_t + 1));
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|c| *c == '*')
}

/// One review as GitHub lists it (`pulls/<n>/reviews`), in list order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Review {
    /// The reviewer's login.
    pub login: String,
    /// `APPROVED`, `CHANGES_REQUESTED`, `COMMENTED`, `DISMISSED` or `PENDING`.
    pub state: String,
    /// The commit the review was submitted on.
    pub commit_id: String,
}

/// The logins whose latest deciding review approves exactly `head`. A comment
/// does not change a reviewer's standing; a later change request or a
/// dismissal does.
#[must_use]
pub fn exact_head_approvers(reviews: &[Review], head: &str) -> BTreeSet<String> {
    approvers_where(reviews, |commit| commit.eq_ignore_ascii_case(head))
}

/// The logins whose latest deciding review approves a commit other than
/// `head`: an approval GitHub still counts when the rule keeps stale reviews.
#[must_use]
pub fn earlier_head_approvers(reviews: &[Review], head: &str) -> BTreeSet<String> {
    approvers_where(reviews, |commit| !commit.eq_ignore_ascii_case(head))
}

fn approvers_where(reviews: &[Review], commit_matches: impl Fn(&str) -> bool) -> BTreeSet<String> {
    let mut latest: BTreeMap<String, &Review> = BTreeMap::new();
    for review in reviews {
        let state = review.state.to_ascii_uppercase();
        if matches!(
            state.as_str(),
            "APPROVED" | "CHANGES_REQUESTED" | "DISMISSED"
        ) {
            latest.insert(review.login.to_ascii_lowercase(), review);
        }
    }
    latest
        .into_iter()
        .filter(|(_, review)| {
            review.state.eq_ignore_ascii_case("APPROVED") && commit_matches(&review.commit_id)
        })
        .map(|(login, _)| login)
        .collect()
}

/// Judge `files` against `codeowners` and the exact-head `approvers`.
///
/// Each owned file needs an approval from one of its owners. An owner set
/// that only a team or an e-mail address could satisfy, or that names the
/// pull request's `author` (who cannot approve their own change), is judged
/// by GitHub instead: [`CodeOwnerStatus::Undetermined`].
#[must_use]
pub fn judge(
    codeowners: &CodeOwners,
    files: &[String],
    approvers: &BTreeSet<String>,
    author: Option<&str>,
) -> CodeOwnerStatus {
    let author = author
        .map(|login| login.trim_start_matches("app/").to_ascii_lowercase())
        .filter(|login| !login.is_empty());
    let mut satisfied_by = BTreeSet::new();
    let mut missing_owners = BTreeSet::new();
    let mut missing_files = Vec::new();
    let mut undetermined = None;
    for file in files {
        let Some(owners) = codeowners.owners_of(file) else {
            continue;
        };
        let lowered: Vec<String> = owners
            .iter()
            .map(|owner| owner.to_ascii_lowercase())
            .collect();
        if let Some(approver) = lowered
            .iter()
            .find(|owner| is_user(owner) && approvers.contains(*owner))
        {
            satisfied_by.insert(approver.clone());
            continue;
        }
        if lowered.iter().any(|owner| !is_user(owner)) {
            undetermined.get_or_insert_with(|| {
                format!(
                    "{file} is owned by a team or e-mail owner ({}); GitHub judges it",
                    owners.join(" ")
                )
            });
            continue;
        }
        if author
            .as_ref()
            .is_some_and(|author| lowered.iter().any(|owner| owner == author))
        {
            undetermined.get_or_insert_with(|| {
                format!("{file} is owned by the pull request's author; GitHub judges it")
            });
            continue;
        }
        missing_owners.extend(owners.iter().cloned());
        if missing_files.len() < 5 {
            missing_files.push(file.clone());
        }
    }
    // A file only a listed user can approve, and none has, is a refusal
    // GitHub would make whatever it decides about the rest.
    if !missing_owners.is_empty() {
        return CodeOwnerStatus::Missing {
            owners: missing_owners.into_iter().collect(),
            files: missing_files,
        };
    }
    if let Some(reason) = undetermined {
        return CodeOwnerStatus::Undetermined(reason);
    }
    if satisfied_by.is_empty() {
        CodeOwnerStatus::NotRequired
    } else {
        CodeOwnerStatus::Satisfied {
            approvers: satisfied_by.into_iter().collect(),
        }
    }
}

/// A user login: not `org/team`, not an e-mail address.
fn is_user(owner: &str) -> bool {
    !owner.contains('/') && !owner.contains('@')
}

/// Whether evaluated branch rules (`rules/branches/<base>`, optionally
/// `--slurp`ed into pages) require a code-owner review.
#[must_use]
pub fn rules_require_code_owner_review(rules: &Value) -> bool {
    !code_owner_rules(rules).is_empty()
}

/// Whether every rule that requires a code-owner review also dismisses stale
/// reviews on push. When one does not, GitHub still counts an owner's
/// approval of an earlier commit.
#[must_use]
pub fn rules_dismiss_stale_reviews(rules: &Value) -> bool {
    let requiring = code_owner_rules(rules);
    !requiring.is_empty()
        && requiring.iter().all(|rule| {
            rule.pointer("/parameters/dismiss_stale_reviews_on_push")
                .and_then(Value::as_bool)
                == Some(true)
        })
}

/// The `pull_request` rules (in `rules/branches/<base>`, optionally
/// `--slurp`ed into pages) that require a code-owner review.
fn code_owner_rules(rules: &Value) -> Vec<&Value> {
    let requires = |rule: &&Value| {
        rule.get("type").and_then(Value::as_str) == Some("pull_request")
            && rule
                .pointer("/parameters/require_code_owner_review")
                .and_then(Value::as_bool)
                == Some(true)
    };
    rules
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|item| match item.as_array() {
            Some(page) => page.iter().collect::<Vec<_>>(),
            None => vec![item],
        })
        .filter(requires)
        .collect()
}

/// Read and judge whether pull request `pr` at `head` waits on a code-owner
/// review on `base`. `author` is the pull request's author, when known.
///
/// # Errors
///
/// When a fact the verdict needs cannot be read. A caller that only uses the
/// verdict to avoid a predictable refusal may treat an error as "ask GitHub".
pub fn read_status(
    run_gh: RunGh<'_>,
    repo: &str,
    base: &str,
    pr: u64,
    head: &str,
    author: Option<&str>,
) -> Result<CodeOwnerStatus, String> {
    let rules = json(
        run_gh,
        &[
            "api".to_owned(),
            format!(
                "repos/{repo}/rules/branches/{}?per_page=100",
                crate::required_check_policy::encode_path_segment(base)
            ),
            "--paginate".to_owned(),
            "--slurp".to_owned(),
        ],
        "evaluated branch rules",
    )?;
    if !rules_require_code_owner_review(&rules) {
        return Ok(CodeOwnerStatus::NotRequired);
    }
    let Some(text) = read_codeowners(run_gh, repo, base)? else {
        return Ok(CodeOwnerStatus::NotRequired);
    };
    let codeowners = CodeOwners::parse(&text);
    let files = read_lines(
        run_gh,
        &format!("repos/{repo}/pulls/{pr}/files?per_page=100"),
        ".[] | .filename",
    )?;
    if files.len() >= FILE_LIST_CAP {
        return Ok(CodeOwnerStatus::Undetermined(format!(
            "the diff lists {} files, GitHub's cap; GitHub judges it",
            files.len()
        )));
    }
    let reviews = read_lines(
        run_gh,
        &format!("repos/{repo}/pulls/{pr}/reviews?per_page=100"),
        r#".[] | [(.user.login // ""), (.state // ""), (.commit_id // "")] | @tsv"#,
    )?
    .into_iter()
    .filter_map(|line| {
        let mut fields = line.split('\t');
        Some(Review {
            login: fields.next()?.to_owned(),
            state: fields.next()?.to_owned(),
            commit_id: fields.next()?.to_owned(),
        })
    })
    .filter(|review| !review.login.is_empty())
    .collect::<Vec<_>>();
    let approvers = exact_head_approvers(&reviews, head);
    let status = judge(&codeowners, &files, &approvers, author);
    if let CodeOwnerStatus::Missing { owners, .. } = &status
        && !rules_dismiss_stale_reviews(&rules)
    {
        // The steward's own reviewer gate is exact-head; GitHub's code-owner
        // rule may not be. An owner who approved an earlier commit, on a rule
        // that keeps stale reviews, may already satisfy GitHub: let it judge.
        let earlier = earlier_head_approvers(&reviews, head);
        if let Some(owner) = owners
            .iter()
            .find(|owner| earlier.contains(&owner.to_ascii_lowercase()))
        {
            return Ok(CodeOwnerStatus::Undetermined(format!(
                "{owner} approved an earlier commit and the rule keeps stale reviews; \
                 GitHub judges it"
            )));
        }
    }
    if matches!(status, CodeOwnerStatus::Missing { .. }) {
        // GitHub skips a line or an owner it rejects (an unknown owner, one
        // without write access), which this parser cannot see. Hold the head
        // only when GitHub reports the file clean.
        if let Some(reason) = codeowners_errors(run_gh, repo, base) {
            return Ok(CodeOwnerStatus::Undetermined(reason));
        }
    }
    Ok(status)
}

/// Why GitHub's own reading of `CODEOWNERS` might differ from this one:
/// the errors it reports for the file on `base`, or the failure to read them.
fn codeowners_errors(run_gh: RunGh<'_>, repo: &str, base: &str) -> Option<String> {
    let errors = match json(
        run_gh,
        &[
            "api".to_owned(),
            format!(
                "repos/{repo}/codeowners/errors?ref={}",
                crate::required_check_policy::encode_path_segment(base)
            ),
        ],
        "CODEOWNERS errors",
    ) {
        Ok(value) => value,
        Err(error) => return Some(error),
    };
    match errors.get("errors").and_then(Value::as_array) {
        Some(list) if list.is_empty() => None,
        Some(list) => Some(format!(
            "GitHub reports {} CODEOWNERS error(s) on {base}; GitHub judges it",
            list.len()
        )),
        None => Some("CODEOWNERS errors malformed".to_owned()),
    }
}

/// The first `CODEOWNERS` present on `base`, as text; `None` when there is
/// none.
fn read_codeowners(run_gh: RunGh<'_>, repo: &str, base: &str) -> Result<Option<String>, String> {
    for path in CODEOWNERS_PATHS {
        match run_gh(&[
            "api".to_owned(),
            "-H".to_owned(),
            "Accept: application/vnd.github.raw+json".to_owned(),
            format!(
                "repos/{repo}/contents/{path}?ref={}",
                crate::required_check_policy::encode_path_segment(base)
            ),
        ]) {
            Ok(text) => return Ok(Some(text)),
            Err(detail) if crate::head_approval::is_clean_not_found(&detail) => {}
            Err(detail) => return Err(format!("{path} on {base} unreadable: {detail}")),
        }
    }
    Ok(None)
}

fn json(run_gh: RunGh<'_>, argv: &[String], purpose: &str) -> Result<Value, String> {
    let raw = run_gh(argv).map_err(|error| format!("{purpose} unreadable: {error}"))?;
    serde_json::from_str(&raw).map_err(|error| format!("{purpose} malformed: {error}"))
}

fn read_lines(run_gh: RunGh<'_>, path: &str, jq: &str) -> Result<Vec<String>, String> {
    let raw = run_gh(&[
        "api".to_owned(),
        "--paginate".to_owned(),
        path.to_owned(),
        "--jq".to_owned(),
        jq.to_owned(),
    ])
    .map_err(|error| format!("{path} unreadable: {error}"))?;
    Ok(raw
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect())
}

#[cfg(test)]
mod tests;
