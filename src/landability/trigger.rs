//! A reader for the part of a GitHub workflow file that decides *whether a run
//! is requested at all* — the `on:` block.
//!
//! ## Why this is a sibling of [`super::workflow`] and not an extension of it
//!
//! The jobs reader states its safety direction plainly: it over-approximates,
//! pulling **more** lanes into a context's closure than strictly run, because
//! for a schedulability question a missed lane is the dangerous error and a
//! spurious one is merely noisy.
//!
//! This reader has the **opposite** safety direction, and that is the whole
//! reason it is a separate file with its own module doc. A mis-read trigger
//! filter that *admits* a pull request the real filter excludes is a false
//! pass — the tool would say "the gate will be requested" about a gate that is
//! never requested, which is precisely the failure this module exists to end.
//! On 2026-09-14 an operator waited 2 h 48 m on a required context that
//! `on.pull_request.branches: [main]` had already refused, and no instrument
//! said so.
//!
//! So the rule here is: **exact, or [`TriggerUnknown`]. Never a partial filter
//! list.** Every form outside the implemented subset refuses loudly with the
//! offending line, and `Unknown` is printed with its boundary rather than
//! folded into either a pass or a refusal.
//!
//! ## What it deliberately does not read
//!
//! Job-level `if:`. A job skipped by `if:` reports **Success** to branch
//! protection, so it cannot make a required context unreachable; a workflow
//! skipped by a `branches`/`paths` filter leaves the context *Pending forever*.
//! Only the second is an absence risk, and only the second is modelled here.
//! Evaluating `if:` would require the whole expression language and the run
//! context, and would buy nothing.

use std::collections::BTreeSet;
use std::fmt;

use serde::Serialize;

/// GitHub's limit on how many changed files a `paths` filter is evaluated
/// against. Past it the filter's behaviour is not statically decidable, so the
/// reader refuses rather than guessing.
pub const PATHS_DIFF_LIMIT: usize = 300;

/// Activity types GitHub defines for `pull_request` / `pull_request_target`.
///
/// A `types:` list naming anything outside this set is a refusal, not a
/// silently-ignored entry: an unknown type is either a typo (the workflow does
/// not fire the way its author thinks) or a GitHub addition this reader has
/// not learned, and both deserve to be visible.
const PULL_REQUEST_TYPES: &[&str] = &[
    "assigned",
    "auto_merge_disabled",
    "auto_merge_enabled",
    "closed",
    "converted_to_draft",
    "demilestoned",
    "dequeued",
    "edited",
    "enqueued",
    "labeled",
    "locked",
    "milestoned",
    "opened",
    "ready_for_review",
    "reopened",
    "review_request_removed",
    "review_requested",
    "synchronize",
    "unassigned",
    "unlabeled",
    "unlocked",
];

/// Activity types that fire by default when a `pull_request` event declares no
/// `types:` list.
pub const DEFAULT_PULL_REQUEST_TYPES: &[&str] = &["opened", "synchronize", "reopened"];

/// The reader refused to read this `on:` block.
///
/// Carries the boundary and the offending line so the refusal points at
/// something a workflow author can act on. A parser that returns "could not
/// parse" without a line is a parser its operator learns to ignore.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TriggerUnknown {
    /// Short machine-readable boundary: `expression`, `anchor`, `tab`,
    /// `duplicate`, `multi_document`, `pattern`, `activity_type`,
    /// `conflicting_filters`, `negated_ignore`, `shape`.
    pub boundary: String,
    /// Human sentence naming what could not be read.
    pub detail: String,
    /// 1-based line number of the offending line, when there is one.
    pub line: Option<usize>,
}

impl TriggerUnknown {
    fn new(boundary: &str, detail: impl Into<String>, line: Option<usize>) -> Self {
        Self {
            boundary: boundary.to_owned(),
            detail: detail.into(),
            line,
        }
    }
}

impl fmt::Display for TriggerUnknown {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.line {
            Some(line) => write!(
                formatter,
                "{} (line {line}) [{}]",
                self.detail, self.boundary
            ),
            None => write!(formatter, "{} [{}]", self.detail, self.boundary),
        }
    }
}

/// Filters declared under one `pull_request`-shaped event.
///
/// `None` means the key was absent, which is **not** the same as an empty
/// list: an absent `branches` admits every base, an empty one admits none.
/// Collapsing the two is the single easiest way to write a false pass here.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct EventFilter {
    /// `branches:` — base branches admitted.
    pub branches: Option<Vec<String>>,
    /// `branches-ignore:` — base branches excluded.
    pub branches_ignore: Option<Vec<String>>,
    /// `paths:` — changed paths that must match for the run to be requested.
    pub paths: Option<Vec<String>>,
    /// `paths-ignore:` — changed paths that, alone, suppress the run.
    pub paths_ignore: Option<Vec<String>>,
    /// `types:` — activity types. Absent means [`DEFAULT_PULL_REQUEST_TYPES`].
    pub types: Option<Vec<String>>,
}

/// Every trigger the reader understands, for one workflow file.
///
/// The booleans are genuinely independent facts about one file — a workflow
/// may declare any combination — so collapsing them into an enum would model
/// something that is not true.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct Triggers {
    /// `on.pull_request`, when declared.
    pub pull_request: Option<EventFilter>,
    /// `on.pull_request_target`, when declared.
    pub pull_request_target: Option<EventFilter>,
    /// `on.merge_group` declared at all.
    pub merge_group: bool,
    /// `on.push` declared at all.
    pub push: bool,
    /// `on.workflow_dispatch` declared at all.
    pub workflow_dispatch: bool,
    /// `on.schedule` declared at all.
    pub schedule: bool,
    /// Every event key seen, including ones this reader does not model.
    pub events: Vec<String>,
    /// The file used a bare `on` that a YAML 1.1 reader folds to `true:`.
    /// Harmless, but printed, because it means two readers of this file
    /// disagree about its shape.
    pub yaml_true_key: bool,
}

impl Triggers {
    /// Whether this workflow declares any event that can produce a check on a
    /// pull request's merge ref.
    #[must_use]
    pub fn has_pull_request_shaped_event(&self) -> bool {
        self.pull_request.is_some() || self.pull_request_target.is_some() || self.merge_group
    }

    /// Whether the `pull_request` event re-fires on a retarget (`edited`).
    ///
    /// A required producer without this never gets a run when GitHub
    /// auto-retargets a stacked child PR, which is how the 2026-09-14 incident
    /// recurs. Reported as a warning, never a refusal.
    #[must_use]
    pub fn refires_on_retarget(&self) -> bool {
        [&self.pull_request, &self.pull_request_target]
            .into_iter()
            .flatten()
            .any(|filter| filter.has_type("edited"))
    }
}

impl EventFilter {
    /// Whether this event fires for `activity`.
    #[must_use]
    pub fn has_type(&self, activity: &str) -> bool {
        match &self.types {
            None => DEFAULT_PULL_REQUEST_TYPES.contains(&activity),
            Some(types) => types.iter().any(|entry| entry == activity),
        }
    }

    /// Whether this event's branch filters admit `base`.
    #[must_use]
    pub fn admits_base(&self, base: &str) -> Admit {
        if let Some(patterns) = &self.branches {
            return match matches_any(patterns, base, MatchKind::Branch) {
                Ok(true) => Admit::Yes,
                Ok(false) => Admit::No {
                    clause: format!("branches: [{}]", patterns.join(", ")),
                    detail: format!("does not admit this PR's base `{base}`"),
                },
                Err(error) => Admit::Unknown(error),
            };
        }
        if let Some(patterns) = &self.branches_ignore {
            return match matches_any(patterns, base, MatchKind::Branch) {
                Ok(true) => Admit::No {
                    clause: format!("branches-ignore: [{}]", patterns.join(", ")),
                    detail: format!("excludes this PR's base `{base}`"),
                },
                Ok(false) => Admit::Yes,
                Err(error) => Admit::Unknown(error),
            };
        }
        Admit::Yes
    }

    /// Whether this event's path filters admit a diff of `changed` files.
    ///
    /// `changed` must be the three-dot diff against the base, which is how
    /// GitHub evaluates it. A diff longer than [`PATHS_DIFF_LIMIT`] is
    /// [`Admit::Unknown`] whenever a path filter is present: past that limit
    /// GitHub's own evaluation is truncated, so no static answer is honest.
    #[must_use]
    pub fn admits_paths(&self, changed: &[String]) -> Admit {
        let has_filter = self.paths.is_some() || self.paths_ignore.is_some();
        if !has_filter {
            return Admit::Yes;
        }
        if changed.len() > PATHS_DIFF_LIMIT {
            return Admit::Unknown(TriggerUnknown::new(
                "diff_size",
                format!(
                    "{} changed files exceeds GitHub's {PATHS_DIFF_LIMIT}-file path-filter \
                     evaluation limit; the filter's result is not statically decidable",
                    changed.len()
                ),
                None,
            ));
        }
        if changed.is_empty() {
            return Admit::Unknown(TriggerUnknown::new(
                "diff_empty",
                "no changed files: either this checkout has no commits the base does not (there \
                 is no pull request here to classify) or the diff was unreadable. A path filter \
                 cannot be evaluated from nothing, and assuming it admits is how a false pass is \
                 written.",
                None,
            ));
        }
        if let Some(patterns) = &self.paths {
            let mut any = false;
            for file in changed {
                match matches_any(patterns, file, MatchKind::Path) {
                    Ok(true) => {
                        any = true;
                        break;
                    }
                    Ok(false) => {}
                    Err(error) => return Admit::Unknown(error),
                }
            }
            if !any {
                return Admit::No {
                    clause: format!("paths: [{}]", patterns.join(", ")),
                    detail: format!("matches none of the {} changed file(s)", changed.len()),
                };
            }
        }
        if let Some(patterns) = &self.paths_ignore {
            let mut any_kept = false;
            for file in changed {
                match matches_any(patterns, file, MatchKind::Path) {
                    Ok(true) => {}
                    Ok(false) => {
                        any_kept = true;
                        break;
                    }
                    Err(error) => return Admit::Unknown(error),
                }
            }
            if !any_kept {
                return Admit::No {
                    clause: format!("paths-ignore: [{}]", patterns.join(", ")),
                    detail: format!(
                        "excludes every one of the {} changed file(s)",
                        changed.len()
                    ),
                };
            }
        }
        Admit::Yes
    }
}

/// Result of one filter evaluation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "admit", rename_all = "snake_case")]
pub enum Admit {
    /// The filter admits this PR.
    Yes,
    /// The filter excludes it, and names the clause as written in the file.
    No {
        /// The clause verbatim enough for the author to find it.
        clause: String,
        /// What the clause did to this PR.
        detail: String,
    },
    /// The filter could not be evaluated. Never a pass, never alone a refusal.
    Unknown(TriggerUnknown),
}

impl Admit {
    /// Whether this is an outright exclusion.
    #[must_use]
    pub fn excludes(&self) -> bool {
        matches!(self, Self::No { .. })
    }
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Read the `on:` block of a workflow file.
///
/// # Errors
///
/// Returns [`TriggerUnknown`] for every form outside the implemented subset —
/// see the module doc for why that list is long on purpose.
#[allow(clippy::too_many_lines)]
pub fn parse_workflow_triggers(source: &str) -> Result<Triggers, TriggerUnknown> {
    let lines: Vec<&str> = source.lines().collect();
    reject_document_hazards(&lines)?;

    let mut start: Option<usize> = None;
    for (index, raw) in lines.iter().enumerate() {
        if !is_top_level_key(raw) {
            continue;
        }
        let Some((key, _)) = split_key(raw.trim_end()) else {
            continue;
        };
        let normalized = key.trim().trim_matches(['"', '\'']);
        if normalized == "on" || normalized == "true" {
            if start.is_some() {
                return Err(TriggerUnknown::new(
                    "duplicate",
                    "a second top-level `on:` key; which one GitHub honours is not decidable here",
                    Some(index + 1),
                ));
            }
            start = Some(index);
        }
    }

    let Some(start) = start else {
        // Not an Unknown: a workflow with no triggers is a definite fact about
        // the file, and the caller renders it as "no `on:` found".
        return Ok(Triggers::default());
    };

    let mut triggers = Triggers {
        yaml_true_key: lines[start].trim_start().starts_with("true:"),
        ..Triggers::default()
    };

    let (_, inline) = split_key(lines[start].trim_end())
        .ok_or_else(|| TriggerUnknown::new("shape", "unreadable `on:` line", Some(start + 1)))?;
    let inline = strip_comment(inline).trim();

    // Body = every line after `on:` up to the next column-0 key.
    let mut end = lines.len();
    for (offset, raw) in lines.iter().enumerate().skip(start + 1) {
        if is_top_level_key(raw) {
            end = offset;
            break;
        }
    }
    let body = &lines[start + 1..end];

    for (offset, raw) in body.iter().enumerate() {
        reject_line_hazards(raw, start + 2 + offset)?;
    }
    reject_line_hazards(lines[start], start + 1)?;

    if !inline.is_empty() {
        // Scalar (`on: pull_request`) or flow list (`on: [push, pull_request]`).
        let names = if let Some(stripped) = inline.strip_prefix('[') {
            let stripped = stripped.strip_suffix(']').ok_or_else(|| {
                TriggerUnknown::new(
                    "shape",
                    "unterminated flow list after `on:`",
                    Some(start + 1),
                )
            })?;
            stripped
                .split(',')
                .map(|item| item.trim().trim_matches(['"', '\'']).to_owned())
                .filter(|item| !item.is_empty())
                .collect::<Vec<_>>()
        } else {
            vec![inline.trim_matches(['"', '\'']).to_owned()]
        };
        for name in names {
            record_event(&mut triggers, &name, EventFilter::default());
        }
        if body.iter().any(|raw| !is_blank_or_comment(raw)) {
            return Err(TriggerUnknown::new(
                "shape",
                "`on:` carries both an inline value and a nested block",
                Some(start + 1),
            ));
        }
        return Ok(triggers);
    }

    // Mapping form. The first non-blank body line fixes the event indent.
    let Some(event_indent) = body
        .iter()
        .find(|raw| !is_blank_or_comment(raw))
        .map(|raw| indent_of(raw))
    else {
        return Err(TriggerUnknown::new(
            "shape",
            "`on:` has neither an inline value nor any nested event",
            Some(start + 1),
        ));
    };

    let mut index = 0usize;
    while index < body.len() {
        let raw = body[index];
        if is_blank_or_comment(raw) {
            index += 1;
            continue;
        }
        let line_no = start + 2 + index;
        let this_indent = indent_of(raw);
        if this_indent < event_indent {
            return Err(TriggerUnknown::new(
                "shape",
                "an `on:` body line is indented less than the first event key",
                Some(line_no),
            ));
        }
        if this_indent > event_indent {
            return Err(TriggerUnknown::new(
                "shape",
                "an `on:` body line is indented deeper than its event without a key to own it",
                Some(line_no),
            ));
        }
        let Some((key, inline)) = split_key(raw.trim_end()) else {
            return Err(TriggerUnknown::new(
                "shape",
                format!("`on:` body line is not a `key:` mapping: {}", raw.trim()),
                Some(line_no),
            ));
        };
        let name = key.trim().trim_matches(['"', '\'']).to_owned();
        let inline = strip_comment(inline).trim().to_owned();

        // Collect this event's nested block: everything indented deeper.
        let mut next = index + 1;
        while next < body.len()
            && (is_blank_or_comment(body[next]) || indent_of(body[next]) > event_indent)
        {
            next += 1;
        }
        let nested = &body[index + 1..next];

        let filter = if name == "pull_request" || name == "pull_request_target" {
            if !inline.is_empty() {
                return Err(TriggerUnknown::new(
                    "shape",
                    format!("`{name}:` carries an inline value this reader does not model"),
                    Some(line_no),
                ));
            }
            parse_event_filter(nested, line_no + 1, &name)?
        } else {
            EventFilter::default()
        };
        record_event(&mut triggers, &name, filter);
        index = next;
    }

    Ok(triggers)
}

/// Filters under one `pull_request`-shaped event.
fn parse_event_filter(
    nested: &[&str],
    first_line: usize,
    event: &str,
) -> Result<EventFilter, TriggerUnknown> {
    let mut filter = EventFilter::default();
    let Some(key_indent) = nested
        .iter()
        .find(|raw| !is_blank_or_comment(raw))
        .map(|raw| indent_of(raw))
    else {
        return Ok(filter);
    };

    let mut index = 0usize;
    while index < nested.len() {
        let raw = nested[index];
        if is_blank_or_comment(raw) {
            index += 1;
            continue;
        }
        let line_no = first_line + index;
        if indent_of(raw) != key_indent {
            return Err(TriggerUnknown::new(
                "shape",
                format!("`{event}:` filter block has inconsistent indentation"),
                Some(line_no),
            ));
        }
        let Some((key, inline)) = split_key(raw.trim_end()) else {
            return Err(TriggerUnknown::new(
                "shape",
                format!(
                    "`{event}:` filter line is not a `key:` mapping: {}",
                    raw.trim()
                ),
                Some(line_no),
            ));
        };
        let key = key.trim().trim_matches(['"', '\'']).to_owned();
        let inline = strip_comment(inline).trim().to_owned();

        let mut next = index + 1;
        while next < nested.len()
            && (is_blank_or_comment(nested[next]) || indent_of(nested[next]) > key_indent)
        {
            next += 1;
        }
        let items = read_list(&inline, &nested[index + 1..next], line_no)?;

        let slot = match key.as_str() {
            "branches" => &mut filter.branches,
            "branches-ignore" => &mut filter.branches_ignore,
            "paths" => &mut filter.paths,
            "paths-ignore" => &mut filter.paths_ignore,
            "types" => &mut filter.types,
            // `secrets`, `inputs` and the like never appear under
            // pull_request; anything unrecognized here is a shape this reader
            // has not learned and must not silently drop.
            other => {
                return Err(TriggerUnknown::new(
                    "shape",
                    format!("unrecognized `{event}:` filter key `{other}`"),
                    Some(line_no),
                ));
            }
        };
        if slot.is_some() {
            return Err(TriggerUnknown::new(
                "duplicate",
                format!("`{key}` declared twice under `{event}:`"),
                Some(line_no),
            ));
        }
        *slot = Some(items);
        index = next;
    }

    validate_filter(&filter, first_line, event)?;
    Ok(filter)
}

/// Reject filter combinations GitHub forbids or this reader cannot evaluate.
fn validate_filter(filter: &EventFilter, line: usize, event: &str) -> Result<(), TriggerUnknown> {
    if filter.branches.is_some() && filter.branches_ignore.is_some() {
        return Err(TriggerUnknown::new(
            "conflicting_filters",
            format!(
                "`{event}:` declares both `branches` and `branches-ignore`; GitHub rejects \
                     this and the file may be mid-edit"
            ),
            Some(line),
        ));
    }
    if filter.paths.is_some() && filter.paths_ignore.is_some() {
        return Err(TriggerUnknown::new(
            "conflicting_filters",
            format!("`{event}:` declares both `paths` and `paths-ignore`; GitHub rejects this"),
            Some(line),
        ));
    }
    for (name, patterns) in [
        ("branches-ignore", &filter.branches_ignore),
        ("paths-ignore", &filter.paths_ignore),
    ] {
        if let Some(patterns) = patterns
            && let Some(bad) = patterns.iter().find(|pattern| pattern.starts_with('!'))
        {
            return Err(TriggerUnknown::new(
                "negated_ignore",
                format!(
                    "`{name}` contains the negated pattern `{bad}`, which GitHub does not allow"
                ),
                Some(line),
            ));
        }
    }
    if let Some(types) = &filter.types {
        if types.is_empty() {
            return Err(TriggerUnknown::new(
                "activity_type",
                format!("`{event}:` declares an empty `types` list"),
                Some(line),
            ));
        }
        if let Some(bad) = types
            .iter()
            .find(|entry| !PULL_REQUEST_TYPES.contains(&entry.as_str()))
        {
            return Err(TriggerUnknown::new(
                "activity_type",
                format!("`{event}.types` names `{bad}`, which is not a GitHub activity type"),
                Some(line),
            ));
        }
    }
    for (name, patterns) in [
        ("branches", &filter.branches),
        ("branches-ignore", &filter.branches_ignore),
    ] {
        if let Some(patterns) = patterns {
            if patterns.is_empty() {
                return Err(TriggerUnknown::new(
                    "shape",
                    format!("`{event}.{name}` is an empty list"),
                    Some(line),
                ));
            }
            for pattern in patterns {
                validate_pattern(pattern, line)?;
            }
        }
    }
    for (name, patterns) in [
        ("paths", &filter.paths),
        ("paths-ignore", &filter.paths_ignore),
    ] {
        if let Some(patterns) = patterns {
            if patterns.is_empty() {
                return Err(TriggerUnknown::new(
                    "shape",
                    format!("`{event}.{name}` is an empty list"),
                    Some(line),
                ));
            }
            for pattern in patterns {
                validate_pattern(pattern, line)?;
            }
        }
    }
    Ok(())
}

/// A list written either inline (`[a, b]`) or as a block (`- a`).
fn read_list(inline: &str, nested: &[&str], line: usize) -> Result<Vec<String>, TriggerUnknown> {
    if !inline.is_empty() {
        if nested.iter().any(|raw| !is_blank_or_comment(raw)) {
            return Err(TriggerUnknown::new(
                "shape",
                "a filter key carries both an inline list and a nested block",
                Some(line),
            ));
        }
        let stripped = inline
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
            .ok_or_else(|| {
                TriggerUnknown::new(
                    "shape",
                    format!("inline filter value `{inline}` is not a flow list"),
                    Some(line),
                )
            })?;
        return Ok(stripped
            .split(',')
            .map(|item| item.trim().trim_matches(['"', '\'']).to_owned())
            .filter(|item| !item.is_empty())
            .collect());
    }
    let mut items = Vec::new();
    for (offset, raw) in nested.iter().enumerate() {
        if is_blank_or_comment(raw) {
            continue;
        }
        let trimmed = raw.trim();
        let Some(item) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix('-'))
        else {
            return Err(TriggerUnknown::new(
                "shape",
                format!("filter block line is not a list item: {trimmed}"),
                Some(line + 1 + offset),
            ));
        };
        let item = strip_comment(item)
            .trim()
            .trim_matches(['"', '\''])
            .to_owned();
        if !item.is_empty() {
            items.push(item);
        }
    }
    Ok(items)
}

fn record_event(triggers: &mut Triggers, name: &str, filter: EventFilter) {
    if !triggers.events.iter().any(|seen| seen == name) {
        triggers.events.push(name.to_owned());
    }
    match name {
        "pull_request" => triggers.pull_request = Some(filter),
        "pull_request_target" => triggers.pull_request_target = Some(filter),
        "merge_group" => triggers.merge_group = true,
        "push" => triggers.push = true,
        "workflow_dispatch" => triggers.workflow_dispatch = true,
        "schedule" => triggers.schedule = true,
        _ => {}
    }
}

/// Document-level forms this reader refuses outright.
fn reject_document_hazards(lines: &[&str]) -> Result<(), TriggerUnknown> {
    let mut seen_content = false;
    for (index, raw) in lines.iter().enumerate() {
        // A document separator is one only at column 0. An indented `---` is
        // ordinary content — most often a markdown horizontal rule inside a
        // block scalar, which is what Pulp's `release-cli.yml` carries at line
        // 1904 inside a release-body template. Trimming before this test made
        // the reader refuse that file outright, and the whole-directory
        // control is what made the false refusal visible instead of silent.
        if raw.starts_with("---") {
            if seen_content {
                return Err(TriggerUnknown::new(
                    "multi_document",
                    "a second YAML document; which one carries the triggers is not decidable here",
                    Some(index + 1),
                ));
            }
            continue;
        }
        if !is_blank_or_comment(raw) {
            seen_content = true;
        }
    }
    Ok(())
}

/// Line-level forms this reader refuses inside the `on:` block.
fn reject_line_hazards(raw: &str, line: usize) -> Result<(), TriggerUnknown> {
    if is_blank_or_comment(raw) {
        return Ok(());
    }
    let leading = &raw[..raw.len() - raw.trim_start().len()];
    if leading.contains('\t') {
        return Err(TriggerUnknown::new(
            "tab",
            "a tab is used for indentation, which YAML forbids and different readers recover \
             from differently",
            Some(line),
        ));
    }
    let content = strip_comment(raw);
    if content.contains("${{") {
        return Err(TriggerUnknown::new(
            "expression",
            "an expression inside the `on:` block; its value is not known statically",
            Some(line),
        ));
    }
    // An anchor or alias can sit in three places: as a bare value after a key,
    // as a list item, or as a merge key. Checking only the start of the line
    // misses `branches: &b`, which is the form a real workflow would use.
    let trimmed = content.trim();
    if trimmed.starts_with("<<:") {
        return Err(TriggerUnknown::new(
            "anchor",
            "a YAML merge key inside the `on:` block",
            Some(line),
        ));
    }
    let value = match split_key(trimmed) {
        Some((_, after)) => after,
        None => trimmed.trim_start_matches('-'),
    }
    .trim();
    if is_anchor_or_alias(value) {
        return Err(TriggerUnknown::new(
            "anchor",
            "a YAML anchor or alias inside the `on:` block",
            Some(line),
        ));
    }
    Ok(())
}

/// Whether a value is a YAML anchor (`&name`) or alias (`*name`).
///
/// A quoted `'*'` is a legitimate filter pattern and must not be mistaken for
/// an alias, which is why this looks at the character *after* the sigil.
fn is_anchor_or_alias(value: &str) -> bool {
    let Some(rest) = value.strip_prefix('&').or_else(|| value.strip_prefix('*')) else {
        return false;
    };
    if value.starts_with("**") {
        return false;
    }
    rest.chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
}

// ---------------------------------------------------------------------------
// Filter pattern matching
// ---------------------------------------------------------------------------

/// Which flavour of filter pattern is being matched.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MatchKind {
    /// A `branches`/`branches-ignore` pattern against a branch name.
    Branch,
    /// A `paths`/`paths-ignore` pattern against a repository-relative path.
    Path,
}

/// Characters outside GitHub's documented filter-pattern subset.
///
/// `^` and `$` in particular are the tell of a pattern written as a regex.
/// GitHub would treat them literally; a reader that also treated them
/// literally would agree by accident, and a reader that treated them as
/// anchors would disagree silently. Refusing is the only honest answer.
const UNSUPPORTED_PATTERN_CHARS: &[char] = &['^', '$', '(', ')', '{', '}', '|', '\\'];

fn validate_pattern(pattern: &str, line: usize) -> Result<(), TriggerUnknown> {
    if pattern.is_empty() {
        return Err(TriggerUnknown::new(
            "pattern",
            "an empty filter pattern",
            Some(line),
        ));
    }
    if let Some(bad) = pattern
        .chars()
        .find(|c| UNSUPPORTED_PATTERN_CHARS.contains(c))
    {
        return Err(TriggerUnknown::new(
            "pattern",
            format!(
                "filter pattern `{pattern}` uses `{bad}`, which is outside GitHub's documented \
                 filter-pattern subset (*, **, ?, +, [], leading !)"
            ),
            Some(line),
        ));
    }
    if pattern.matches('[').count() != pattern.matches(']').count() {
        return Err(TriggerUnknown::new(
            "pattern",
            format!("filter pattern `{pattern}` has an unbalanced character class"),
            Some(line),
        ));
    }
    Ok(())
}

/// Evaluate a pattern list with GitHub's documented "last match wins" ordering.
///
/// A list of only negated patterns matches nothing, which is GitHub's
/// behaviour and the reason the accumulator starts `false`.
fn matches_any(
    patterns: &[String],
    candidate: &str,
    kind: MatchKind,
) -> Result<bool, TriggerUnknown> {
    let mut included = false;
    for pattern in patterns {
        let (negated, bare) = match pattern.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, pattern.as_str()),
        };
        let normalized = if kind == MatchKind::Branch {
            bare.strip_prefix("refs/heads/").unwrap_or(bare)
        } else {
            bare
        };
        if matches_pattern(normalized, candidate)? {
            included = !negated;
        }
    }
    Ok(included)
}

#[derive(Clone, Debug)]
enum Token {
    /// `*` — any run of characters that does not cross a `/`.
    Star,
    /// `**` — any run of characters, `/` included.
    DoubleStar,
    /// A literal character.
    Literal(char),
    /// `[abc]` / `[a-z]`, optionally negated with a leading `!` or `^`.
    Class {
        set: BTreeSet<char>,
        ranges: Vec<(char, char)>,
        negated: bool,
    },
    /// `?` or `+` applied to the preceding token: (min, max) repetitions.
    Repeat {
        inner: Box<Token>,
        min: usize,
        max: usize,
    },
}

fn tokenize(pattern: &str) -> Result<Vec<Token>, TriggerUnknown> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut tokens: Vec<Token> = Vec::new();
    let mut index = 0usize;
    while index < chars.len() {
        match chars[index] {
            '*' => {
                if index + 1 < chars.len() && chars[index + 1] == '*' {
                    tokens.push(Token::DoubleStar);
                    index += 2;
                } else {
                    tokens.push(Token::Star);
                    index += 1;
                }
            }
            '[' => {
                let close = chars[index + 1..]
                    .iter()
                    .position(|c| *c == ']')
                    .map(|offset| index + 1 + offset)
                    .ok_or_else(|| {
                        TriggerUnknown::new(
                            "pattern",
                            format!("unterminated character class in `{pattern}`"),
                            None,
                        )
                    })?;
                let mut body: Vec<char> = chars[index + 1..close].to_vec();
                let negated = matches!(body.first(), Some('!' | '^'));
                if negated {
                    body.remove(0);
                }
                let mut set = BTreeSet::new();
                let mut ranges = Vec::new();
                let mut cursor = 0usize;
                while cursor < body.len() {
                    if cursor + 2 < body.len() && body[cursor + 1] == '-' {
                        ranges.push((body[cursor], body[cursor + 2]));
                        cursor += 3;
                    } else {
                        set.insert(body[cursor]);
                        cursor += 1;
                    }
                }
                tokens.push(Token::Class {
                    set,
                    ranges,
                    negated,
                });
                index = close + 1;
            }
            '?' | '+' => {
                let (min, max) = if chars[index] == '?' {
                    (0, 1)
                } else {
                    (1, usize::MAX)
                };
                let inner = tokens.pop().ok_or_else(|| {
                    TriggerUnknown::new(
                        "pattern",
                        format!(
                            "`{}` in `{pattern}` has no preceding character to repeat",
                            chars[index]
                        ),
                        None,
                    )
                })?;
                tokens.push(Token::Repeat {
                    inner: Box::new(inner),
                    min,
                    max,
                });
                index += 1;
            }
            other => {
                tokens.push(Token::Literal(other));
                index += 1;
            }
        }
    }
    Ok(tokens)
}

fn matches_pattern(pattern: &str, candidate: &str) -> Result<bool, TriggerUnknown> {
    let tokens = tokenize(pattern)?;
    let chars: Vec<char> = candidate.chars().collect();
    Ok(match_tokens(&tokens, &chars))
}

fn match_tokens(tokens: &[Token], input: &[char]) -> bool {
    let Some((first, rest)) = tokens.split_first() else {
        return input.is_empty();
    };
    match first {
        Token::Literal(expected) => {
            matches!(input.split_first(), Some((head, tail)) if head == expected && match_tokens(rest, tail))
        }
        Token::Class { .. } => {
            matches!(input.split_first(), Some((head, tail)) if class_matches(first, *head) && match_tokens(rest, tail))
        }
        Token::Star => (0..=input.len())
            .take_while(|take| input[..*take].iter().all(|c| *c != '/'))
            .any(|take| match_tokens(rest, &input[take..])),
        Token::DoubleStar => (0..=input.len()).any(|take| match_tokens(rest, &input[take..])),
        Token::Repeat { inner, min, max } => {
            let mut count = 0usize;
            let mut cursor = 0usize;
            loop {
                if count >= *min && match_tokens(rest, &input[cursor..]) {
                    return true;
                }
                if count >= *max || cursor >= input.len() {
                    return false;
                }
                let matched = match inner.as_ref() {
                    Token::Literal(expected) => input[cursor] == *expected,
                    Token::Class { .. } => class_matches(inner, input[cursor]),
                    // `?`/`+` after `*` or `**` is not a form GitHub documents;
                    // treat the quantifier as satisfied by the wildcard itself.
                    _ => return match_tokens(rest, &input[cursor..]),
                };
                if !matched {
                    return false;
                }
                cursor += 1;
                count += 1;
            }
        }
    }
}

fn class_matches(token: &Token, candidate: char) -> bool {
    let Token::Class {
        set,
        ranges,
        negated,
    } = token
    else {
        return false;
    };
    let hit = set.contains(&candidate)
        || ranges
            .iter()
            .any(|(low, high)| candidate >= *low && candidate <= *high);
    hit != *negated
}

// ---------------------------------------------------------------------------
// Small line helpers
// ---------------------------------------------------------------------------

fn is_top_level_key(raw: &str) -> bool {
    !raw.is_empty()
        && !raw.starts_with([' ', '\t', '#', '-'])
        && raw.contains(':')
        && !raw.trim_start().starts_with("---")
}

fn is_blank_or_comment(raw: &str) -> bool {
    let trimmed = raw.trim();
    trimmed.is_empty() || trimmed.starts_with('#')
}

fn indent_of(raw: &str) -> usize {
    raw.len() - raw.trim_start().len()
}

/// Split `key: value` on the first colon that is not inside quotes.
fn split_key(raw: &str) -> Option<(&str, &str)> {
    let bytes = raw.as_bytes();
    let mut quote: Option<u8> = None;
    for (index, byte) in bytes.iter().enumerate() {
        match quote {
            Some(open) if *byte == open => quote = None,
            Some(_) => {}
            None => match byte {
                b'"' | b'\'' => quote = Some(*byte),
                b':' => return Some((&raw[..index], &raw[index + 1..])),
                _ => {}
            },
        }
    }
    None
}

/// Drop a trailing `#` comment that is not inside quotes.
fn strip_comment(raw: &str) -> &str {
    let bytes = raw.as_bytes();
    let mut quote: Option<u8> = None;
    for (index, byte) in bytes.iter().enumerate() {
        match quote {
            Some(open) if *byte == open => quote = None,
            Some(_) => {}
            None => match byte {
                b'"' | b'\'' => quote = Some(*byte),
                // Only a `#` preceded by whitespace or at the start opens a
                // comment; `a#b` is a literal.
                b'#' if index == 0 || bytes[index - 1].is_ascii_whitespace() => {
                    return &raw[..index];
                }
                _ => {}
            },
        }
    }
    raw
}

#[cfg(test)]
mod tests;
