use std::path::{Component, Path, PathBuf};

use super::DependencyChannel;

pub(super) fn required<'a>(value: Option<&'a str>, name: &str) -> Result<&'a str, String> {
    value
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("dependencies.pulp.{name} is required for this channel"))
}

pub(super) fn reject_present(
    value: Option<&str>,
    name: &str,
    channel: DependencyChannel,
) -> Result<(), String> {
    if value.is_some() {
        return Err(format!(
            "dependencies.pulp.{name} is not valid for channel={:?}",
            channel.as_str()
        ));
    }
    Ok(())
}

pub(super) fn validate_repo_slug(value: &str) -> Result<(), String> {
    if value != value.trim() {
        return Err(
            "dependencies.pulp.repository must not contain surrounding whitespace".to_owned(),
        );
    }
    let Some((owner, repo)) = value.split_once('/') else {
        return Err("dependencies.pulp.repository must be an owner/repo slug".to_owned());
    };
    if owner.is_empty()
        || repo.is_empty()
        || repo.contains('/')
        || matches!(owner, "." | "..")
        || matches!(repo, "." | "..")
        || owner.contains("..")
        || repo.contains("..")
        || !owner.chars().all(repo_slug_char)
        || !repo.chars().all(repo_slug_char)
    {
        return Err("dependencies.pulp.repository must be an owner/repo slug".to_owned());
    }
    Ok(())
}

pub(super) fn validate_signer_workflow(repository: &str, value: &str) -> Result<(), String> {
    let expected_prefix = format!("github.com/{repository}/.github/workflows/");
    let workflow_path = value.strip_prefix(&expected_prefix);
    if workflow_path.is_none_or(|path| {
        path.is_empty()
            || path.contains('@')
            || path.contains('/')
            || path == "."
            || path == ".."
            || !Path::new(path).extension().is_some_and(|extension| {
                extension.eq_ignore_ascii_case("yml") || extension.eq_ignore_ascii_case("yaml")
            })
            || !path
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    }) {
        return Err(format!(
            "dependencies.pulp.signer_workflow must name an exact workflow path under {expected_prefix} without a ref"
        ));
    }
    Ok(())
}

pub(super) fn validate_actions_invocation(repository: &str, value: &str) -> Result<(), String> {
    let prefix = format!("https://github.com/{repository}/actions/runs/");
    let Some((run, attempt)) = value
        .strip_prefix(&prefix)
        .and_then(|suffix| suffix.split_once("/attempts/"))
    else {
        return Err("build attestation invocation must identify an exact configured-repository Actions run attempt".to_owned());
    };
    if run.is_empty()
        || attempt.is_empty()
        || !run.bytes().all(|byte| byte.is_ascii_digit())
        || !attempt.bytes().all(|byte| byte.is_ascii_digit())
        || run.parse::<u64>().ok().is_none_or(|value| value == 0)
        || attempt.parse::<u64>().ok().is_none_or(|value| value == 0)
    {
        return Err("build attestation invocation must identify an exact configured-repository Actions run attempt".to_owned());
    }
    Ok(())
}

pub(super) fn repo_slug_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.')
}

pub(super) fn validate_release_tag(value: &str) -> Result<(), String> {
    version_tuple(value).map(|_| ())
}

pub(super) fn version_tuple(value: &str) -> Result<(u64, u64, u64), String> {
    let Some(version) = value.strip_prefix('v') else {
        return Err(format!(
            "release identity {value:?} is not an exact vMAJOR.MINOR.PATCH tag (floating refs such as main are forbidden)"
        ));
    };
    let mut parts = version.split('.');
    let major = parts.next().and_then(|part| part.parse().ok());
    let minor = parts.next().and_then(|part| part.parse().ok());
    let patch = parts.next().and_then(|part| part.parse().ok());
    if parts.next().is_some() || major.is_none() || minor.is_none() || patch.is_none() {
        return Err(format!(
            "release identity {value:?} is not an exact vMAJOR.MINOR.PATCH tag (draft/prerelease/floating refs are forbidden)"
        ));
    }
    let tuple = (
        major.expect("checked"),
        minor.expect("checked"),
        patch.expect("checked"),
    );
    if value != format!("v{}.{}.{}", tuple.0, tuple.1, tuple.2) {
        return Err(format!(
            "release identity {value:?} is not a canonical vMAJOR.MINOR.PATCH tag"
        ));
    }
    Ok(tuple)
}

pub(super) fn validate_git_sha(value: &str, label: &str) -> Result<(), String> {
    validate_hex(value, label, 40)
}

pub(super) fn validate_sha256(value: &str, label: &str) -> Result<(), String> {
    validate_hex(value, label, 64)
}

pub(super) fn validate_hex(value: &str, label: &str, length: usize) -> Result<(), String> {
    if value.len() != length {
        return Err(format!(
            "{label} must be a {length}-character hexadecimal digest"
        ));
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(format!("{label} must be lowercase hexadecimal"));
    }
    Ok(())
}

pub(super) fn validate_asset_name(value: &str, label: &str) -> Result<(), String> {
    if value.is_empty()
        || value == "."
        || value == ".."
        || value.contains('/')
        || value.contains('\\')
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        return Err(format!("{label} {value:?} must be a safe basename"));
    }
    Ok(())
}

pub(super) fn validate_relative_lock_path(path: &Path) -> Result<(), String> {
    if path.is_absolute()
        || path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        || !path.starts_with(".shipyard/dependencies")
        || path.components().count() < 3
    {
        return Err(
            "dependencies.pulp.lock_file must be a file below the reserved .shipyard/dependencies directory with no traversal"
                .to_owned(),
        );
    }
    if path.components().any(|component| {
        let Component::Normal(component) = component else {
            return true;
        };
        component.to_str().is_none_or(|component| {
            component.is_empty()
                || !component
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
        })
    }) {
        return Err(
            "dependencies.pulp.lock_file components may contain only ASCII letters, digits, dot, dash, and underscore"
                .to_owned(),
        );
    }
    Ok(())
}

pub(super) fn validate_branch_name(value: &str) -> Result<(), String> {
    let invalid_component = value.split('/').any(|component| {
        component.is_empty()
            || component.starts_with('.')
            || component.to_ascii_lowercase().ends_with(".lock")
    });
    if value.is_empty()
        || value.starts_with('-')
        || value.contains("..")
        || value.contains("@{")
        || value.starts_with('/')
        || value.ends_with('/')
        || value.ends_with('.')
        || value.contains("//")
        || invalid_component
        || value
            .chars()
            .any(|ch| !ch.is_ascii_alphanumeric() && !matches!(ch, '-' | '_' | '.' | '/'))
    {
        return Err("dependencies.pulp.base_branch is not a safe Git branch name".to_owned());
    }
    Ok(())
}

pub(super) fn default_manifest_asset() -> String {
    "SHA256SUMS".to_owned()
}

pub(super) fn default_lock_file() -> PathBuf {
    PathBuf::from(".shipyard/dependencies/pulp.lock.json")
}

pub(super) fn default_base_branch() -> String {
    "main".to_owned()
}
