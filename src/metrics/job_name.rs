//! Matching GitHub Actions job names that GitHub reports unevaluated.
//!
//! GitHub evaluates a job's `name:` expression only when the job runs. A job
//! skipped by its `if:` is reported with the raw expression text instead, with
//! the `${{ }}` wrapper removed, for example
//!
//! ```text
//! github.event_name == 'merge_group' && (...) && 'macos' || 'macos-merge-unused'
//! ```
//!
//! and a matrix job that never expanded keeps `${{ matrix.os }}` in its name.
//! A metric keyed by the exact name files those jobs under a lane of their own
//! that no agent asks for, so the gate's samples look thinner than they are.
//!
//! [`candidates`] returns the names such a job can evaluate to, and
//! [`matches`] answers "is this reported name the job called `wanted`?".

/// Whether `name` still carries expression syntax, so GitHub did not evaluate
/// it.
#[must_use]
pub fn is_unevaluated(name: &str) -> bool {
    name.contains("${{")
        || ((name.contains(" && ") || name.contains(" || "))
            && (name.contains("github.")
                || name.contains("needs.")
                || name.contains("matrix.")
                || name.contains("inputs.")
                || name.contains("vars.")
                || name.contains('\'')))
}

/// Every string literal of an `a && 'x' || 'y'` style expression, in order.
fn value_literals(expression: &str) -> Vec<String> {
    let mut out = Vec::new();
    for operator in ["&&", "||"] {
        let mut rest = expression;
        while let Some(index) = rest.find(operator) {
            rest = &rest[index + operator.len()..];
            let trimmed = rest.trim_start();
            if let Some(body) = trimmed.strip_prefix('\'')
                && let Some(end) = body.find('\'')
            {
                let literal = &body[..end];
                // Only a literal that is a whole operand is a name candidate;
                // `x == 'y'` compares, it does not choose.
                let after = body[end + 1..].trim_start();
                if (after.is_empty()
                    || after.starts_with("&&")
                    || after.starts_with("||")
                    || after.starts_with(')')
                    || after.starts_with("}}"))
                    && !literal.is_empty()
                    && !out.iter().any(|seen: &String| seen == literal)
                {
                    out.push(literal.to_owned());
                }
            }
        }
    }
    // Keep the `&&` operand (the name the job has when it runs) first.
    out
}

/// Names an unevaluated job name can evaluate to. An evaluated name yields
/// itself. A name with no recoverable literal yields nothing.
#[must_use]
pub fn candidates(name: &str) -> Vec<String> {
    if !is_unevaluated(name) {
        return vec![name.to_owned()];
    }
    let stripped = name.replace("${{", " ").replace("}}", " ");
    value_literals(&stripped)
}

/// The name a report should file this job under: the name itself when it was
/// evaluated, otherwise the name the job carries when it runs (the `&&`
/// operand of a `cond && 'name' || 'other'` expression).
#[must_use]
pub fn canonical(name: &str) -> String {
    if !is_unevaluated(name) {
        return name.to_owned();
    }
    candidates(name)
        .into_iter()
        .next()
        .unwrap_or_else(|| name.to_owned())
}

/// Whether `reported` (as GitHub returned it) is the job named `wanted`:
/// exactly, as one of the values an unevaluated `cond && 'a' || 'b'` name
/// chooses between, or through a `${{ matrix.x }}` placeholder that stands for
/// any text.
#[must_use]
pub fn matches(wanted: &str, reported: &str) -> bool {
    if reported == wanted {
        return true;
    }
    if !is_unevaluated(reported) {
        return false;
    }
    if candidates(reported).iter().any(|name| name == wanted) {
        return true;
    }
    placeholder_glob(wanted, reported)
}

/// `build (${{ matrix.os }})` matches `build (macos-15)`: literal segments
/// must appear in order, each `${{ ... }}` stands for any text.
fn placeholder_glob(wanted: &str, pattern: &str) -> bool {
    if !pattern.contains("${{") {
        return false;
    }
    let mut literals = Vec::new();
    let mut rest = pattern;
    while let Some(open) = rest.find("${{") {
        literals.push(&rest[..open]);
        let Some(close) = rest[open..].find("}}") else {
            return false;
        };
        rest = &rest[open + close + 2..];
    }
    literals.push(rest);
    let mut cursor = 0;
    let last = literals.len() - 1;
    for (index, literal) in literals.iter().enumerate() {
        if index == 0 {
            if !wanted.starts_with(literal) {
                return false;
            }
            cursor = literal.len();
        } else if index == last {
            return wanted.len() >= cursor + literal.len() && wanted[cursor..].ends_with(literal);
        } else if let Some(found) = wanted[cursor..].find(literal) {
            cursor += found + literal.len();
        } else {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const PR_ALTERNATE: &str = "(github.event_name == 'pull_request' || github.event_name == 'workflow_dispatch') && (needs.resolve-provider.result != 'success' || needs.classify.result != 'success' || needs.classify.outputs.native_build_required != 'true') && 'macos' || 'macos-pr-unused'";
    const MG_ALTERNATE: &str = "github.event_name == 'merge_group' && (needs.resolve-provider.result != 'success' || needs.protected-receipt-reuse.outputs.macos_reused == 'true') && 'macos' || 'macos-merge-unused'";

    #[test]
    fn a_skipped_ternary_name_resolves_to_the_names_it_chooses_between() {
        assert_eq!(candidates(PR_ALTERNATE), vec!["macos", "macos-pr-unused"]);
        assert_eq!(
            candidates(MG_ALTERNATE),
            vec!["macos", "macos-merge-unused"]
        );
        assert_eq!(canonical(PR_ALTERNATE), "macos");
        assert!(matches("macos", PR_ALTERNATE));
        assert!(matches("macos-merge-unused", MG_ALTERNATE));
        assert!(!matches("linux", MG_ALTERNATE));
    }

    #[test]
    fn comparison_literals_are_not_name_candidates() {
        let names = candidates(PR_ALTERNATE);
        assert!(!names.iter().any(|name| name == "pull_request"));
        assert!(!names.iter().any(|name| name == "success"));
        assert!(!names.iter().any(|name| name == "true"));
    }

    #[test]
    fn evaluated_names_match_exactly_and_only_exactly() {
        assert!(!is_unevaluated("macos"));
        assert!(!is_unevaluated("Linux (x64) [github-hosted]"));
        assert!(!is_unevaluated(
            "android-build (macos-latest, \"macos-latest\")"
        ));
        assert_eq!(canonical("macos"), "macos");
        assert!(matches("macos", "macos"));
        assert!(!matches("macos", "macos-15"));
        assert!(!matches("macos", "macOS local smoke"));
    }

    #[test]
    fn a_matrix_placeholder_stands_for_any_text() {
        assert!(matches("build (macos-15)", "build (${{ matrix.os }})"));
        assert!(matches(
            "test / macos / 2",
            "test / ${{ matrix.os }} / ${{ matrix.shard }}"
        ));
        assert!(!matches("lint (macos-15)", "build (${{ matrix.os }})"));
        assert!(!matches(
            "build (macos-15) extra",
            "build (${{ matrix.os }})"
        ));
    }
}
