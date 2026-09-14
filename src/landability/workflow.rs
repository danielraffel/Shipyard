//! A deliberately small reader for the part of a GitHub workflow file that
//! decides *where a job runs*.
//!
//! This is not a YAML parser and must not grow into one. It extracts four
//! facts per job — id, `name`, `needs`, `runs-on` — plus every `vars.*`
//! reference in the job body, and it reports anything it does not understand
//! as [`RunsOnResolution::Unparsable`] so the lane becomes an explicit
//! `Unknown` rather than a silent pass.
//!
//! ## Why not a real YAML dependency
//!
//! Pulling a YAML crate in to answer "which runner labels does this job ask
//! for" would add a parser, its version policy and its failure modes to a
//! binary whose whole job is to be trustworthy about CI. The subset here is
//! the subset workflow files actually use for routing, and every form it does
//! not recognize fails *loudly* rather than being guessed at. That trade is
//! only defensible because of the second half: an unrecognized expression is
//! listed by job id in the output, so a workflow author can see that their
//! expression escaped the analysis instead of quietly not being checked.
//!
//! ## Over-approximation, and which direction it errs
//!
//! A job's `name` is frequently an expression (`${{ … && 'macos' || 'macos-pr-unused' }}`).
//! Rendering it would need the whole expression language and the run context.
//! Instead every string literal in the expression is treated as a name the job
//! *may* render to. That over-approximates producers, which pulls **more**
//! lanes into a context's closure, never fewer — the safe direction for a
//! check whose failure mode is missing a lane. The cost is that a job could be
//! attributed to a context it never produces; the refusal names the job, so
//! that is visible rather than mysterious.

use serde::Serialize;

/// One job as read out of a workflow file.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WorkflowJob {
    /// The job's key under `jobs:`.
    pub id: String,
    /// The raw `name:` value, if the job declares one.
    pub name_expr: Option<String>,
    /// Job ids this job declares in `needs:`.
    pub needs: Vec<String>,
    /// The raw `runs-on:` value, joined onto one line if it was a block.
    pub runs_on_expr: Option<String>,
    /// Every `vars.NAME` referenced anywhere in this job's body, in order of
    /// first appearance. Used to resolve a `runs-on` that comes from a job
    /// output into a candidate set.
    pub var_refs: Vec<String>,
}

impl WorkflowJob {
    /// Whether this job may render to `context`.
    ///
    /// With no `name:`, GitHub displays the job id. With a literal `name:`,
    /// that literal. With an expression, any string literal inside it is a
    /// candidate — see the module note on over-approximation.
    #[must_use]
    pub fn produces(&self, context: &str) -> bool {
        match &self.name_expr {
            None => self.id == context,
            Some(expr) => {
                if expr.contains("${{") {
                    string_literals(expr).iter().any(|lit| lit == context)
                } else {
                    expr.trim().trim_matches(['"', '\'']) == context
                }
            }
        }
    }
}

/// What a `runs-on:` expression resolves to, statically.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RunsOnResolution {
    /// A bare label or JSON literal with no expression: `windows-latest`.
    Literal {
        /// The literal text, ready for `parse_runs_on`.
        value: String,
    },
    /// `fromJSON(vars.X)` or `fromJSON(vars.X || '<literal>')`.
    Variable {
        /// The routing variable name.
        name: String,
        /// The literal used when the variable is unset, if the expression
        /// supplies one.
        fallback: Option<String>,
    },
    /// `fromJSON(needs.J.outputs.K)` or `fromJSON(matrix.K)` — the label set is
    /// computed at run time.
    ///
    /// Resolved to the set of routing variables the producing jobs read, each
    /// assessed separately; the lane is schedulable if **any** candidate is.
    Dynamic {
        /// A human description of where the value comes from.
        origin: String,
        /// Job ids whose `vars.*` references are the candidate set.
        from_jobs: Vec<String>,
    },
    /// Not one of the above. Never folded into a pass.
    Unparsable {
        /// The raw expression, for the operator to read.
        raw: String,
    },
}

/// Resolve one `runs-on:` expression.
///
/// Recognizes exactly the four forms above. Anything else is `Unparsable`.
#[must_use]
pub fn resolve_runs_on_expr(expr: &str) -> RunsOnResolution {
    let trimmed = expr.trim();
    if trimmed.is_empty() {
        return RunsOnResolution::Unparsable {
            raw: expr.to_owned(),
        };
    }
    let Some(inner) = strip_expression(trimmed) else {
        // No `${{ }}` at all: a literal label or a literal JSON array.
        return RunsOnResolution::Literal {
            value: trimmed.to_owned(),
        };
    };

    let inner = inner.trim();
    let Some(arg) = strip_call(inner, "fromJSON") else {
        return RunsOnResolution::Unparsable {
            raw: expr.to_owned(),
        };
    };
    let arg = arg.trim();

    // `vars.X || '<literal>'`
    if let Some((left, right)) = split_once_top_level(arg, "||") {
        let left = left.trim();
        let right = right.trim();
        return match strip_prefix_ident(left, "vars.") {
            Some(name) => RunsOnResolution::Variable {
                name: name.to_owned(),
                fallback: Some(right.trim_matches('\'').to_owned()),
            },
            None => RunsOnResolution::Unparsable {
                raw: expr.to_owned(),
            },
        };
    }

    if let Some(name) = strip_prefix_ident(arg, "vars.") {
        return RunsOnResolution::Variable {
            name: name.to_owned(),
            fallback: None,
        };
    }

    if let Some(rest) = arg.strip_prefix("needs.") {
        let job = rest.split('.').next().unwrap_or_default().to_owned();
        if job.is_empty() {
            return RunsOnResolution::Unparsable {
                raw: expr.to_owned(),
            };
        }
        return RunsOnResolution::Dynamic {
            origin: format!("needs.{job} output"),
            from_jobs: vec![job],
        };
    }

    if arg.starts_with("matrix.") {
        // The matrix itself is built by a `needs` job; the caller supplies
        // which, because that is a fact about the job, not the expression.
        return RunsOnResolution::Dynamic {
            origin: arg.to_owned(),
            from_jobs: Vec::new(),
        };
    }

    RunsOnResolution::Unparsable {
        raw: expr.to_owned(),
    }
}

/// Parse the `jobs:` block of a workflow file.
///
/// Indentation-driven: the `jobs:` key at column 0, job ids at the first
/// indent below it, job properties one level deeper. Comments and blank lines
/// are skipped. Block scalars (`>-`, `|`) on `name:`/`runs-on:` are joined
/// onto one line, because that is how the expression reads anyway.
#[must_use]
pub fn parse_workflow_jobs(source: &str) -> Vec<WorkflowJob> {
    let lines: Vec<&str> = source.lines().collect();
    let Some(jobs_at) = lines.iter().position(|line| line.trim_end() == "jobs:") else {
        return Vec::new();
    };

    let body = &lines[jobs_at + 1..];
    let Some(job_indent) = body
        .iter()
        .find(|line| is_content(line))
        .map(|line| indent_of(line))
    else {
        return Vec::new();
    };
    if job_indent == 0 {
        return Vec::new();
    }

    let mut jobs = Vec::new();
    let mut index = 0usize;
    while index < body.len() {
        let line = body[index];
        if !is_content(line) {
            index += 1;
            continue;
        }
        let this_indent = indent_of(line);
        if this_indent < job_indent {
            // Left the `jobs:` block entirely.
            break;
        }
        if this_indent > job_indent {
            index += 1;
            continue;
        }
        let Some(id) = line.trim().strip_suffix(':').map(str::to_owned) else {
            index += 1;
            continue;
        };

        // Collect this job's body: every following line more indented than the
        // job id (blank lines included, so a block scalar keeps its shape).
        let start = index + 1;
        let mut end = start;
        while end < body.len() {
            let candidate = body[end];
            if is_content(candidate) && indent_of(candidate) <= job_indent {
                break;
            }
            end += 1;
        }
        jobs.push(parse_job(id, &body[start..end]));
        index = end;
    }
    jobs
}

fn parse_job(id: String, body: &[&str]) -> WorkflowJob {
    let prop_indent = body
        .iter()
        .find(|line| is_content(line))
        .map_or(usize::MAX, |line| indent_of(line));

    let mut job = WorkflowJob {
        id,
        name_expr: None,
        needs: Vec::new(),
        runs_on_expr: None,
        var_refs: Vec::new(),
    };

    let mut index = 0usize;
    while index < body.len() {
        let line = body[index];
        if !is_content(line) || indent_of(line) != prop_indent {
            index += 1;
            continue;
        }
        let trimmed = line.trim();
        let Some((key, rest)) = trimmed.split_once(':') else {
            index += 1;
            continue;
        };
        let rest = rest.trim();
        match key {
            "name" => {
                let (value, next) = scalar_value(body, index, prop_indent, rest);
                job.name_expr = Some(value);
                index = next;
                continue;
            }
            "runs-on" => {
                let (value, next) = scalar_value(body, index, prop_indent, rest);
                job.runs_on_expr = Some(value);
                index = next;
                continue;
            }
            "needs" => {
                let (value, next) = scalar_value(body, index, prop_indent, rest);
                job.needs = parse_needs(&value);
                index = next;
                continue;
            }
            _ => {}
        }
        index += 1;
    }

    for line in body {
        for name in var_references(line) {
            if !job.var_refs.contains(&name) {
                job.var_refs.push(name);
            }
        }
    }
    job
}

/// Read a scalar that may be inline, a block scalar, or a nested block.
///
/// Returns the joined value and the index of the first line after it.
fn scalar_value(body: &[&str], at: usize, prop_indent: usize, inline: &str) -> (String, usize) {
    if !inline.is_empty() && inline != ">-" && inline != ">" && inline != "|" && inline != "|-" {
        return (inline.to_owned(), at + 1);
    }
    let mut parts: Vec<String> = Vec::new();
    let mut index = at + 1;
    while index < body.len() {
        let line = body[index];
        if is_content(line) && indent_of(line) <= prop_indent {
            break;
        }
        if is_content(line) {
            parts.push(line.trim().to_owned());
        }
        index += 1;
    }
    (parts.join(" "), index)
}

/// Read a `needs:` value in either of its two spellings.
///
/// `needs: [resolve-provider, classify]` and the block-sequence form (joined
/// by [`scalar_value`] into `- resolve-provider - classify`) both reduce to
/// the same set. Splitting on `-` would shred hyphenated job ids, so the list
/// marker is dropped as a whole token instead.
fn parse_needs(raw: &str) -> Vec<String> {
    let trimmed = raw.trim();
    let inner = trimmed
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(trimmed);
    inner
        .replace(',', " ")
        .split_whitespace()
        .filter(|token| *token != "-")
        .map(|token| token.trim_matches(['"', '\'']).to_owned())
        .filter(|token| !token.is_empty())
        .collect()
}

fn is_content(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty() && !trimmed.starts_with('#')
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

fn strip_expression(text: &str) -> Option<&str> {
    let start = text.find("${{")?;
    let end = text.rfind("}}")?;
    if end <= start + 3 {
        return None;
    }
    // Only treat it as a pure expression when the whole scalar is one.
    if text[..start].trim().is_empty() && text[end + 2..].trim().is_empty() {
        Some(&text[start + 3..end])
    } else {
        None
    }
}

fn strip_call<'a>(text: &'a str, function: &str) -> Option<&'a str> {
    let rest = text.strip_prefix(function)?;
    let rest = rest.trim_start().strip_prefix('(')?;
    let rest = rest.strip_suffix(')')?;
    Some(rest)
}

fn strip_prefix_ident<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let rest = text.strip_prefix(prefix)?;
    if rest.is_empty()
        || !rest
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    {
        return None;
    }
    Some(rest)
}

/// Split on `separator` outside of quotes.
fn split_once_top_level<'a>(text: &'a str, separator: &str) -> Option<(&'a str, &'a str)> {
    let bytes = text.as_bytes();
    let mut quote: Option<u8> = None;
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        match quote {
            Some(open) if byte == open => quote = None,
            None if byte == b'\'' || byte == b'"' => quote = Some(byte),
            None if text[index..].starts_with(separator) => {
                return Some((&text[..index], &text[index + separator.len()..]));
            }
            Some(_) | None => {}
        }
        index += 1;
    }
    None
}

/// Every string literal inside an expression, single- or double-quoted.
#[must_use]
pub fn string_literals(expr: &str) -> Vec<String> {
    let mut found = Vec::new();
    let bytes = expr.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        if (byte == b'\'' || byte == b'"')
            && let Some(close) = expr[index + 1..].find(byte as char)
        {
            found.push(expr[index + 1..index + 1 + close].to_owned());
            index += close + 2;
            continue;
        }
        index += 1;
    }
    found
}

/// Every `vars.NAME` reference in a line.
#[must_use]
pub fn var_references(line: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = line;
    while let Some(at) = rest.find("vars.") {
        let tail = &rest[at + 5..];
        let end = tail
            .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
            .unwrap_or(tail.len());
        if end > 0 {
            found.push(tail[..end].to_owned());
        }
        rest = &tail[end..];
    }
    found
}

/// Transitive `needs` closure of `roots`, plus the roots themselves.
///
/// Cycles are impossible in a valid workflow but are bounded here anyway: a
/// malformed file must not hang a preflight.
#[must_use]
pub fn transitive_needs(jobs: &[WorkflowJob], roots: &[String]) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    let mut queue: Vec<String> = roots.to_vec();
    while let Some(id) = queue.pop() {
        if seen.contains(&id) {
            continue;
        }
        if let Some(job) = jobs.iter().find(|job| job.id == id) {
            queue.extend(job.needs.iter().cloned());
        }
        seen.push(id);
    }
    seen.sort();
    seen
}
