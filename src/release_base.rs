//! Release bases: long-lived branches a pull request targets on purpose.
//!
//! A pull request based on a branch other than the default is either a
//! stack (its base is another open pull request's head branch, and it lands
//! on the default branch once that parent merges) or a release refresh (its
//! base is a release branch it is meant to land on). Telling an agent to
//! retarget a release refresh to the default branch is wrong and harmful, so
//! a base is a release base when it matches `[merge_steward] release_bases`
//! (default `["release/*"]`), or when a repository ruleset that names it
//! (not one that covers every branch) protects it as a landing target.
//!
//! The steward admits only the default branch. A release base therefore has
//! a review path only when its rules require the approval context
//! (`review-verdict`) and `[merge_steward] reviewed_release_bases` says the
//! standing reviewer covers it; that list is empty by default, because the
//! standing reviewer schedules only default-branch pull requests today.

use serde_json::Value;

use crate::config::LoadedConfig;

/// `[merge_steward] release_bases`: the release-base patterns.
pub const RELEASE_BASES_KEY: &str = "merge_steward.release_bases";
/// `[merge_steward] reviewed_release_bases`: the release bases the standing
/// reviewer covers.
pub const REVIEWED_RELEASE_BASES_KEY: &str = "merge_steward.reviewed_release_bases";
/// Release-base patterns when the key is absent.
pub const DEFAULT_RELEASE_BASES: [&str; 1] = ["release/*"];

/// Which bases are release bases, and which of those the reviewer covers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReleaseBasePolicy {
    /// Patterns (`*` within one path segment, `**` across segments).
    pub patterns: Vec<String>,
    /// Patterns of release bases the standing reviewer covers.
    pub reviewed: Vec<String>,
}

impl Default for ReleaseBasePolicy {
    fn default() -> Self {
        Self {
            patterns: DEFAULT_RELEASE_BASES
                .iter()
                .map(|pattern| (*pattern).to_owned())
                .collect(),
            reviewed: Vec::new(),
        }
    }
}

impl ReleaseBasePolicy {
    /// The policy `config` sets; the defaults for absent keys.
    ///
    /// # Errors
    ///
    /// When a key is not a list of strings: a malformed policy is refused,
    /// never read as "no release bases".
    pub fn from_config(config: &LoadedConfig) -> Result<Self, String> {
        let mut policy = Self::default();
        if let Some(value) = config.get(RELEASE_BASES_KEY) {
            policy.patterns = string_list(RELEASE_BASES_KEY, value)?;
        }
        if let Some(value) = config.get(REVIEWED_RELEASE_BASES_KEY) {
            policy.reviewed = string_list(REVIEWED_RELEASE_BASES_KEY, value)?;
        }
        Ok(policy)
    }

    /// The machine-global config's policy.
    ///
    /// # Errors
    ///
    /// When the config cannot be loaded or a key is malformed.
    pub fn from_machine_global(global_dir: &std::path::Path) -> Result<Self, String> {
        let config = LoadedConfig::load_machine_global_from_dir(global_dir.to_path_buf())
            .map_err(|error| format!("failed to load {RELEASE_BASES_KEY}: {error}"))?;
        Self::from_config(&config)
    }

    /// The configured pattern `base` matches, if any.
    #[must_use]
    pub fn matching_pattern(&self, base: &str) -> Option<&str> {
        self.patterns
            .iter()
            .find(|pattern| crate::head_approval::glob_match(pattern, base))
            .map(String::as_str)
    }

    /// Whether the standing reviewer covers `base`.
    #[must_use]
    pub fn reviewer_covers(&self, base: &str) -> bool {
        self.reviewed
            .iter()
            .any(|pattern| crate::head_approval::glob_match(pattern, base))
    }
}

fn string_list(key: &str, value: &toml::Value) -> Result<Vec<String>, String> {
    value
        .as_array()
        .ok_or_else(|| format!("{key} must be a list of branch patterns"))?
        .iter()
        .map(|entry| {
            entry
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("{key} must list strings, not {entry}"))
        })
        .collect()
}

/// Why a base is a release base.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReleaseSource {
    /// It matches this configured pattern.
    Pattern(String),
    /// A repository ruleset that names it protects it as a target.
    Ruleset,
}

impl ReleaseSource {
    /// How the classification was made, for a message.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Pattern(pattern) => {
                format!("`{pattern}` in [merge_steward] release_bases")
            }
            Self::Ruleset => "a repository ruleset names it as a protected target".to_owned(),
        }
    }
}

/// Rule types that make a base a landing target: something lands there
/// through a pull request, behind required checks.
const TARGET_RULE_TYPES: [&str; 2] = ["pull_request", "required_status_checks"];

/// The ruleset ids behind the target rules in an evaluated
/// `rules/branches/<base>` answer (paged with `--slurp`, or one page).
#[must_use]
pub fn target_ruleset_ids(evaluated_rules: &Value) -> Vec<u64> {
    let mut ids: Vec<u64> = rule_rows(evaluated_rules)
        .filter(|rule| {
            rule.get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| TARGET_RULE_TYPES.contains(&kind))
        })
        .filter_map(|rule| rule.get("ruleset_id").and_then(Value::as_u64))
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

fn rule_rows(evaluated_rules: &Value) -> impl Iterator<Item = &Value> {
    evaluated_rules
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|entry| match entry.as_array() {
            Some(page) => page.iter().collect::<Vec<_>>(),
            None => vec![entry],
        })
}

/// Whether one ruleset (`repos/{repo}/rulesets/{id}`) names a branch
/// rather than covering every branch: an include other than `~ALL`.
#[must_use]
pub fn ruleset_names_a_branch(ruleset: &Value) -> bool {
    ruleset
        .pointer("/conditions/ref_name/include")
        .and_then(Value::as_array)
        .is_some_and(|include| {
            include
                .iter()
                .filter_map(Value::as_str)
                .any(|pattern| pattern != "~ALL")
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn release_star_is_the_default_and_matches_one_segment() {
        let policy = ReleaseBasePolicy::default();
        assert_eq!(
            policy.matching_pattern("release/v1.0.10"),
            Some("release/*")
        );
        assert_eq!(policy.matching_pattern("feat/parent"), None);
        assert_eq!(policy.matching_pattern("release/v1/hotfix"), None);
        assert!(!policy.reviewer_covers("release/v1.0.10"));
    }

    #[test]
    fn the_keys_are_read_and_a_malformed_list_is_refused() {
        let dir = tempfile::tempdir().expect("temp");
        std::fs::write(
            dir.path().join("config.toml"),
            "[merge_steward]\nrelease_bases = [\"rel-*\"]\nreviewed_release_bases = [\"rel-*\"]\n",
        )
        .expect("config");
        let policy = ReleaseBasePolicy::from_machine_global(dir.path()).expect("policy");
        assert_eq!(policy.matching_pattern("rel-2"), Some("rel-*"));
        assert_eq!(policy.matching_pattern("release/v1"), None);
        assert!(policy.reviewer_covers("rel-2"));

        std::fs::write(
            dir.path().join("config.toml"),
            "[merge_steward]\nrelease_bases = \"release/*\"\n",
        )
        .expect("config");
        assert!(ReleaseBasePolicy::from_machine_global(dir.path()).is_err());
    }

    #[test]
    fn target_rules_name_their_rulesets_and_a_repo_wide_ruleset_names_no_branch() {
        let rules = json!([[
            {"type": "deletion", "ruleset_id": 1},
            {"type": "pull_request", "ruleset_id": 7},
            {"type": "required_status_checks", "ruleset_id": 7},
            {"type": "required_status_checks", "ruleset_id": 9}
        ]]);
        assert_eq!(target_ruleset_ids(&rules), vec![7, 9]);
        assert!(!ruleset_names_a_branch(
            &json!({"conditions": {"ref_name": {"include": ["~ALL"]}}})
        ));
        assert!(ruleset_names_a_branch(
            &json!({"conditions": {"ref_name": {"include": ["refs/heads/release/*"]}}})
        ));
        assert!(!ruleset_names_a_branch(&json!({})));
    }
}
