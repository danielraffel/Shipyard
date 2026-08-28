//! Canonical validation for identities persisted in durable records.

/// Whether a GitHub repository slug has the historical durable-record shape.
///
/// Existing queue and recovery records accepted ASCII case differences and
/// surrounding whitespace because repository ownership keys are canonicalized
/// before comparison. Keep that compatibility here while rejecting ambiguous
/// separators, whitespace inside either component, and unsupported bytes.
pub(crate) fn is_valid_repository_slug(value: &str) -> bool {
    let canonical = canonical_repository_slug(value);
    let Some((owner, repository)) = canonical.split_once('/') else {
        return false;
    };
    !repository.contains('/')
        && !owner.is_empty()
        && !repository.is_empty()
        && owner
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && repository.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        })
}

/// Canonical durable ownership key for a repository slug.
pub(crate) fn canonical_repository_slug(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

/// Whether `value` is an exact lowercase SHA-1 object identity.
pub(crate) fn is_exact_lower_hex_sha1(value: &str) -> bool {
    is_lower_hex_of_length(value, 40)
}

/// Whether `value` is an exact lowercase SHA-1 or SHA-256 object identity.
#[cfg(any(unix, test))]
pub(crate) fn is_exact_lower_hex_git_sha(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && is_lower_hex(value)
}

fn is_lower_hex_of_length(value: &str, length: usize) -> bool {
    value.len() == length && is_lower_hex(value)
}

fn is_lower_hex(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_slug_preserves_historical_case_and_outer_space_acceptance() {
        assert!(is_valid_repository_slug("DanielRaffel/Shipyard"));
        assert!(is_valid_repository_slug(" owner/repository "));
        assert!(is_valid_repository_slug("owner/repo_name.rs"));
        for invalid in [
            "owner",
            "owner/repo/extra",
            "owner name/repo",
            "owner/repo name",
            "owner_/repo",
            "/repo",
            "owner/",
        ] {
            assert!(!is_valid_repository_slug(invalid), "accepted {invalid:?}");
        }
        assert_eq!(
            canonical_repository_slug(" DanielRaffel/Shipyard "),
            "danielraffel/shipyard"
        );
    }

    #[test]
    fn exact_sha_helpers_reject_case_and_length_ambiguity() {
        assert!(is_exact_lower_hex_sha1(&"a".repeat(40)));
        assert!(is_exact_lower_hex_git_sha(&"b".repeat(40)));
        assert!(is_exact_lower_hex_git_sha(&"c".repeat(64)));
        assert!(!is_exact_lower_hex_sha1(&"a".repeat(64)));
        assert!(!is_exact_lower_hex_git_sha(&"A".repeat(40)));
        assert!(!is_exact_lower_hex_git_sha(&"g".repeat(40)));
        assert!(!is_exact_lower_hex_git_sha(&"a".repeat(39)));
    }
}
