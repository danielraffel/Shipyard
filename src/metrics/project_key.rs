//! Project keys for the metrics store.
//!
//! A project is named either by its GitHub slug (`Generous-Corp/pulp`) or by
//! the short repository name (`pulp`). The store has always written the short
//! name — `metrics import github` defaults `--project` to the repository name
//! and records the slug in `runs.repo` — so a lookup by slug used to find
//! nothing. Both spellings now resolve to the same rows:
//!
//! * **Write:** a slug passed as the project is split into the short key plus
//!   `repo` ([`normalize_for_write`]). Existing rows are already in that shape,
//!   so nothing is rewritten.
//! * **Read:** a slug matches rows stored under the slug itself, or under its
//!   short name whose `repo` is the same slug or unrecorded. A short name
//!   matches rows stored under that name or under any `<owner>/<name>`. Keys
//!   compare case-insensitively, as GitHub slugs do.
//!
//! A slug never matches a short-keyed row that records a *different* owner, so
//! two forks sharing a repository name stay apart when asked for by slug.

/// A parsed `--project` value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectKey {
    short: String,
    full: Option<String>,
}

impl ProjectKey {
    /// Parse a project argument. Returns `None` for an empty value.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let trimmed = text.trim().trim_matches('/');
        if trimmed.is_empty() {
            return None;
        }
        let lowered = trimmed.to_ascii_lowercase();
        match lowered.rsplit_once('/') {
            Some((_, name)) if !name.is_empty() => Some(Self {
                short: name.to_owned(),
                full: Some(lowered.clone()),
            }),
            _ => Some(Self {
                short: lowered,
                full: None,
            }),
        }
    }

    /// The short (repository-name) form, lower-cased.
    #[must_use]
    pub fn short(&self) -> &str {
        &self.short
    }

    /// The `owner/name` form, lower-cased, when the argument carried one.
    #[must_use]
    pub fn full(&self) -> Option<&str> {
        self.full.as_deref()
    }

    /// Whether a stored `(project, repo)` pair belongs to this key. Mirrors
    /// [`sql_filter`] exactly; the store filters in SQL, this is for callers
    /// that already hold rows.
    #[must_use]
    pub fn matches(&self, project: &str, repo: Option<&str>) -> bool {
        let project = project.to_ascii_lowercase();
        match &self.full {
            None => {
                project == self.short
                    || project
                        .strip_suffix(&self.short)
                        .is_some_and(|prefix| prefix.ends_with('/'))
            }
            Some(full) => {
                project == *full
                    || (project == self.short
                        && repo.is_none_or(|repo| repo.to_ascii_lowercase() == *full))
            }
        }
    }
}

/// SQL predicate selecting rows for an optional project key, over the given
/// project and repo column expressions. Binds `?1` (non-NULL when a key is
/// given), `?2` (short name) and `?3` (slug or NULL); pass [`sql_params`].
#[must_use]
pub fn sql_filter(project_column: &str, repo_column: &str) -> String {
    format!(
        "(?1 IS NULL \
          OR (?3 IS NULL AND (lower({project_column}) = ?2 \
               OR substr(lower({project_column}), -length(?2) - 1) = '/' || ?2)) \
          OR (?3 IS NOT NULL AND (lower({project_column}) = ?3 \
               OR (lower({project_column}) = ?2 \
                   AND ({repo_column} IS NULL OR lower({repo_column}) = ?3)))))"
    )
}

/// Parameters for [`sql_filter`], in order `?1`, `?2`, `?3`.
#[must_use]
pub fn sql_params(key: Option<&ProjectKey>) -> (Option<i64>, Option<String>, Option<String>) {
    key.map_or((None, None, None), |key| {
        (Some(1), Some(key.short.clone()), key.full.clone())
    })
}

/// Split a slug passed as a project into the short key plus `repo`, keeping an
/// explicit `repo`. A short key is returned unchanged.
#[must_use]
pub fn normalize_for_write(project: &str, repo: Option<&str>) -> (String, Option<String>) {
    let trimmed = project.trim().trim_matches('/');
    match trimmed.rsplit_once('/') {
        Some((owner, name)) if !owner.is_empty() && !name.is_empty() => (
            name.to_owned(),
            Some(repo.map_or_else(|| trimmed.to_owned(), str::to_owned)),
        ),
        _ => (trimmed.to_owned(), repo.map(str::to_owned)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_and_short_name_parse_to_the_same_short_key() {
        let full = ProjectKey::parse("Generous-Corp/pulp").unwrap();
        let short = ProjectKey::parse("pulp").unwrap();
        assert_eq!(full.short(), "pulp");
        assert_eq!(full.full(), Some("generous-corp/pulp"));
        assert_eq!(short.short(), "pulp");
        assert_eq!(short.full(), None);
        assert!(ProjectKey::parse("  ").is_none());
    }

    #[test]
    fn slug_finds_legacy_short_keyed_rows() {
        let key = ProjectKey::parse("Generous-Corp/pulp").unwrap();
        assert!(key.matches("pulp", Some("Generous-Corp/pulp")));
        assert!(key.matches("pulp", None));
        assert!(key.matches("generous-corp/pulp", None));
        // A different owner's repository of the same name stays apart.
        assert!(!key.matches("pulp", Some("someone-else/pulp")));
        assert!(!key.matches("pulp-ci", Some("Generous-Corp/pulp")));
    }

    #[test]
    fn short_name_finds_slug_keyed_rows() {
        let key = ProjectKey::parse("pulp").unwrap();
        assert!(key.matches("pulp", Some("Generous-Corp/pulp")));
        assert!(key.matches("Generous-Corp/pulp", None));
        assert!(!key.matches("notpulp", None));
        assert!(!key.matches("Generous-Corp/notpulp", None));
    }

    #[test]
    fn writes_split_a_slug_into_short_key_and_repo() {
        assert_eq!(
            normalize_for_write("Generous-Corp/pulp", None),
            ("pulp".to_owned(), Some("Generous-Corp/pulp".to_owned()))
        );
        assert_eq!(
            normalize_for_write("Generous-Corp/pulp", Some("Generous-Corp/pulp")),
            ("pulp".to_owned(), Some("Generous-Corp/pulp".to_owned()))
        );
        assert_eq!(normalize_for_write("pulp", None), ("pulp".to_owned(), None));
    }
}
