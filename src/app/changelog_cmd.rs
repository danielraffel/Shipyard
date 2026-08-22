use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use serde_json::Value;

use super::{CliFailure, cli::ChangelogCommand};
use crate::changelog::{
    ChangelogConfig, build_entries, changelog_path, load_changelog_config, render_changelog,
    render_release_notes,
};
use crate::config::LoadedConfig;
use crate::identity::{ProductIdentity, RuntimeMode};
use crate::output::write_json_envelope;

pub(super) fn changelog_command<W: Write>(
    command: ChangelogCommand,
    mode: RuntimeMode,
    cwd: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    match command {
        ChangelogCommand::Regenerate {
            check,
            release_notes_tag,
            stdout: print_stdout,
        } => changelog_regenerate(
            check,
            release_notes_tag.as_deref(),
            print_stdout,
            mode,
            cwd,
            json,
            stdout,
        ),
        ChangelogCommand::Check => changelog_regenerate(true, None, false, mode, cwd, json, stdout),
        ChangelogCommand::Init {
            product,
            repo_url,
            force,
        } => changelog_init(product, repo_url, force, mode, cwd, json, stdout),
    }
}

fn changelog_init<W: Write>(
    product: Option<String>,
    repo_url: Option<String>,
    force: bool,
    mode: RuntimeMode,
    cwd: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let config = LoadedConfig::load_from_cwd(mode, cwd)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let project_dir = cwd.join(ProductIdentity::for_mode(mode).tracked_project_dir_name);
    let config_path = project_dir.join("config.toml");
    {
        let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(&config_path)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        fs::create_dir_all(&project_dir).map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    let resolved_repo = repo_url.unwrap_or_else(|| detect_repo_url_or_empty(cwd));
    let resolved_product = product.unwrap_or_else(|| {
        config
            .get_str("project.name")
            .map_or_else(|| project_name(cwd), ToOwned::to_owned)
    });

    let mut existing_text = fs::read_to_string(&config_path).unwrap_or_default();
    if existing_text.contains("[release.changelog]") && !force {
        render_already_configured(&config_path, json, stdout)?;
        return Ok(ExitCode::SUCCESS);
    }

    let changelog_path = cwd.join("CHANGELOG.md");
    let changelog_exists = changelog_path.exists();
    let backup_path = if changelog_exists {
        let backup = cwd.join("CHANGELOG.md.pre-shipyard.bak");
        if !backup.exists() {
            fs::copy(&changelog_path, &backup)
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
        Some(backup)
    } else {
        None
    };

    if force && existing_text.contains("[release.changelog]") {
        existing_text = strip_release_sections(&existing_text);
    }
    let prefix = if existing_text.trim().is_empty() {
        String::new()
    } else {
        format!("{}\n", existing_text.trim_end())
    };
    let new_text = format!(
        "{prefix}{}",
        changelog_stub(&resolved_repo, &resolved_product)
    );
    let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(&config_path)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    fs::write(&config_path, new_text).map_err(|error| CliFailure::new(1, error.to_string()))?;

    if json {
        render_written_json(
            &config_path,
            changelog_exists,
            backup_path.as_ref(),
            &resolved_repo,
            &resolved_product,
            stdout,
        )?;
    } else {
        render_written_human(&config_path, changelog_exists, backup_path.as_ref(), stdout)?;
    }
    Ok(ExitCode::SUCCESS)
}

fn changelog_regenerate<W: Write>(
    check: bool,
    release_notes_tag: Option<&str>,
    print_stdout: bool,
    mode: RuntimeMode,
    cwd: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let config = LoadedConfig::load_from_cwd(mode, cwd)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let Some(cfg) = load_enabled_config(&config, json, stdout)? else {
        return Ok(ExitCode::from(2));
    };
    let entries =
        build_entries(&cfg, cwd).map_err(|error| CliFailure::new(1, error.to_string()))?;

    if let Some(tag) = release_notes_tag {
        return changelog_release_notes(tag, &entries, &cfg, json, stdout);
    }

    let rendered = render_changelog(&entries, &cfg);
    if print_stdout {
        write!(stdout, "{rendered}").map_err(|error| CliFailure::new(1, error.to_string()))?;
        return Ok(ExitCode::SUCCESS);
    }

    let path = changelog_path(&cfg, cwd);
    let current = fs::read_to_string(&path).unwrap_or_default();
    let drift = current != rendered;
    if check {
        render_changelog_check(&path, drift, entries.len(), json, stdout)?;
        return Ok(if drift {
            ExitCode::from(1)
        } else {
            ExitCode::SUCCESS
        });
    }

    let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(&path)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    fs::write(&path, rendered).map_err(|error| CliFailure::new(1, error.to_string()))?;
    render_changelog_regenerate(&path, drift, entries.len(), json, stdout)?;
    Ok(ExitCode::SUCCESS)
}

fn changelog_release_notes<W: Write>(
    tag: &str,
    entries: &[crate::changelog::ChangelogEntry],
    cfg: &ChangelogConfig,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let Some((index, entry)) = entries
        .iter()
        .enumerate()
        .find(|(_, entry)| entry.tag == tag)
    else {
        if json {
            let mut data = BTreeMap::new();
            data.insert("tag".to_owned(), Value::String(tag.to_owned()));
            data.insert("error".to_owned(), Value::String("not_found".to_owned()));
            write_json_envelope(stdout, "changelog:release-notes", data)
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
        } else {
            writeln!(stdout, "No tag {tag:?} with user-visible merges.")
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
        return Ok(ExitCode::from(2));
    };
    let previous = entries.get(index + 1);
    write!(stdout, "{}", render_release_notes(entry, previous, cfg))
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    Ok(ExitCode::SUCCESS)
}

fn load_enabled_config<W: Write>(
    config: &LoadedConfig,
    json: bool,
    stdout: &mut W,
) -> Result<Option<ChangelogConfig>, CliFailure> {
    let cfg = load_changelog_config(config);
    if !cfg.enabled {
        if json {
            let mut data = BTreeMap::new();
            data.insert("error".to_owned(), Value::String("disabled".to_owned()));
            data.insert(
                "message".to_owned(),
                Value::String(
                    "No [release.changelog] section in .shipyard/config.toml, or `enabled = false`. Run `shipyard changelog init` to opt in."
                        .to_owned(),
                ),
            );
            write_json_envelope(stdout, "changelog:error", data)
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
        } else {
            writeln!(
                stdout,
                "No [release.changelog] section enabled in .shipyard/config.toml."
            )
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
            writeln!(stdout, "Run `shipyard changelog init` to opt in.")
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
        return Ok(None);
    }
    if cfg.repo_url.is_empty() {
        if json {
            let mut data = BTreeMap::new();
            data.insert(
                "error".to_owned(),
                Value::String("missing_repo_url".to_owned()),
            );
            data.insert(
                "message".to_owned(),
                Value::String("release.changelog.repo_url is required".to_owned()),
            );
            write_json_envelope(stdout, "changelog:error", data)
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
        } else {
            writeln!(
                stdout,
                "release.changelog.repo_url is required — set it in .shipyard/config.toml."
            )
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
        return Ok(None);
    }
    Ok(Some(cfg))
}

fn render_changelog_check<W: Write>(
    path: &Path,
    drift: bool,
    versions: usize,
    json: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    if json {
        let mut data = BTreeMap::new();
        data.insert(
            "path".to_owned(),
            Value::String(path.to_string_lossy().into_owned()),
        );
        data.insert("drift".to_owned(), Value::Bool(drift));
        data.insert("versions".to_owned(), Value::from(versions));
        write_json_envelope(stdout, "changelog:check", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        return Ok(());
    }
    if drift {
        writeln!(
            stdout,
            "{} is out of date. Run `shipyard changelog regenerate` to regenerate.",
            path.display()
        )
    } else {
        writeln!(
            stdout,
            "{} is in sync ({versions} versions).",
            path.display()
        )
    }
    .map_err(|error| CliFailure::new(1, error.to_string()))
}

fn render_changelog_regenerate<W: Write>(
    path: &Path,
    drift_before: bool,
    versions: usize,
    json: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    if json {
        let mut data = BTreeMap::new();
        data.insert(
            "path".to_owned(),
            Value::String(path.to_string_lossy().into_owned()),
        );
        data.insert("versions".to_owned(), Value::from(versions));
        data.insert("drift_before".to_owned(), Value::Bool(drift_before));
        write_json_envelope(stdout, "changelog:regenerate", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        return Ok(());
    }
    writeln!(stdout, "Wrote {} ({versions} versions).", path.display())
        .map_err(|error| CliFailure::new(1, error.to_string()))
}

fn render_already_configured<W: Write>(
    config_path: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    if json {
        let mut data = BTreeMap::new();
        data.insert(
            "path".to_owned(),
            Value::String(config_path.to_string_lossy().into_owned()),
        );
        data.insert(
            "status".to_owned(),
            Value::String("already_configured".to_owned()),
        );
        write_json_envelope(stdout, "changelog:init", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        writeln!(
            stdout,
            "[release.changelog] already present in {}. Pass --force to overwrite.",
            config_path.display()
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(())
}

fn render_written_json<W: Write>(
    config_path: &Path,
    changelog_exists: bool,
    backup_path: Option<&PathBuf>,
    repo_url: &str,
    product: &str,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    let mut data = BTreeMap::new();
    data.insert(
        "path".to_owned(),
        Value::String(config_path.to_string_lossy().into_owned()),
    );
    data.insert("status".to_owned(), Value::String("written".to_owned()));
    data.insert(
        "existing_changelog".to_owned(),
        Value::Bool(changelog_exists),
    );
    data.insert(
        "changelog_backup".to_owned(),
        backup_path.map_or(Value::Null, |path| {
            Value::String(path.to_string_lossy().into_owned())
        }),
    );
    data.insert("repo_url".to_owned(), Value::String(repo_url.to_owned()));
    data.insert("product".to_owned(), Value::String(product.to_owned()));
    write_json_envelope(stdout, "changelog:init", data)
        .map_err(|error| CliFailure::new(1, error.to_string()))
}

fn render_written_human<W: Write>(
    config_path: &Path,
    changelog_exists: bool,
    backup_path: Option<&PathBuf>,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    writeln!(stdout, "Wrote {}", config_path.display())
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    if changelog_exists {
        writeln!(stdout).map_err(|error| CliFailure::new(1, error.to_string()))?;
        writeln!(
            stdout,
            "Heads up: CHANGELOG.md already exists. Shipyard will overwrite it on the next `shipyard changelog regenerate` run."
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
        if let Some(backup_path) = backup_path {
            writeln!(
                stdout,
                "Backed up existing file to {} so you can hand-merge anything that matters.",
                backup_path.display()
            )
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
    }
    writeln!(stdout).map_err(|error| CliFailure::new(1, error.to_string()))?;
    writeln!(stdout, "Next steps:").map_err(|error| CliFailure::new(1, error.to_string()))?;
    writeln!(stdout, "  1. shipyard changelog regenerate")
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    writeln!(stdout, "  2. shipyard release-bot hook install")
        .map_err(|error| CliFailure::new(1, error.to_string()))
}

fn changelog_stub(repo_url: &str, product: &str) -> String {
    format!(
        r#"
[release.changelog]
enabled    = true
path       = "CHANGELOG.md"
repo_url   = "{repo_url}"
tag_filter = "v*"
product    = "{product}"
skip_commit_patterns = [
  "^chore: bump",
  "^chore\\(release\\):",
  "^bump .*to v?\\d+\\.\\d+\\.\\d+$",
]

[release.post_tag_hook]
enabled              = true
command              = "shipyard changelog regenerate"
watch                = ["CHANGELOG.md"]
trailers = [
  'Version-Bump: sdk=skip reason="docs-only automated regeneration"',
  'Skill-Update: skip skill=ci reason="no workflow shape change"',
  'Release: skip reason="bot commit; prevent recursive auto-release"',
]
only_for_tag_pattern = "v*"
max_push_attempts    = 5

[release.post_tag_hook.bot_identity]
name  = "shipyard-release-bot"
email = "shipyard-release-bot@users.noreply.github.com"
"#
    )
}

fn strip_release_sections(toml_text: &str) -> String {
    let mut out = String::new();
    let mut skipping = false;
    for line in toml_text.split_inclusive('\n') {
        let stripped = line.trim();
        if stripped.starts_with("[release.") || stripped == "[release]" {
            skipping = true;
            continue;
        }
        if skipping {
            if stripped.starts_with('[')
                && stripped.ends_with(']')
                && !stripped.starts_with("[release")
            {
                skipping = false;
                out.push_str(line);
            }
            continue;
        }
        out.push_str(line);
    }
    out
}

fn detect_repo_url_or_empty(cwd: &Path) -> String {
    let Ok(output) = Command::new("git")
        .args(["config", "--get", "remote.origin.url"])
        .current_dir(cwd)
        .output()
    else {
        return String::new();
    };
    if !output.status.success() {
        return String::new();
    }
    let url = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    normalize_repo_url(&url)
}

fn normalize_repo_url(url: &str) -> String {
    if let Some(slug) = url.strip_prefix("git@github.com:") {
        return format!("https://github.com/{}", slug.trim_end_matches(".git"));
    }
    if url.starts_with("https://github.com/") {
        return url.trim_end_matches(".git").to_owned();
    }
    url.to_owned()
}

fn project_name(path: &Path) -> String {
    path.file_name()
        .and_then(std::ffi::OsStr::to_str)
        .filter(|name| !name.is_empty())
        .unwrap_or("project")
        .to_owned()
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::{Command, ExitCode};

    use clap::Parser;
    use serde_json::Value;

    use super::{normalize_repo_url, strip_release_sections};
    use crate::app::{Cli, run_with};

    #[test]
    fn changelog_init_json_writes_stub_with_project_defaults() {
        let temp = tempfile::tempdir().expect("tempdir");
        let project_dir = temp.path().join(".shipyard");
        std::fs::create_dir_all(&project_dir).expect("project dir");
        std::fs::write(
            project_dir.join("config.toml"),
            "[project]\nname = \"Demo Product\"\n",
        )
        .expect("config");
        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "changelog",
            "init",
            "--repo-url",
            "https://github.com/example/demo.git",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, std::process::ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let payload: Value = serde_json::from_slice(&stdout).expect("json payload");
        assert_eq!(payload["command"], "changelog:init");
        assert_eq!(payload["status"], "written");
        assert_eq!(payload["existing_changelog"], false);
        assert_eq!(payload["changelog_backup"], Value::Null);
        assert_eq!(payload["repo_url"], "https://github.com/example/demo.git");
        assert_eq!(payload["product"], "Demo Product");
        let config = std::fs::read_to_string(project_dir.join("config.toml")).expect("config");
        assert!(config.contains("[release.changelog]"));
        assert!(config.contains("repo_url   = \"https://github.com/example/demo.git\""));
        assert!(config.contains("product    = \"Demo Product\""));
        assert!(config.contains("[release.post_tag_hook.bot_identity]"));
    }

    #[test]
    fn changelog_init_refuses_existing_section_without_force() {
        let temp = tempfile::tempdir().expect("tempdir");
        let project_dir = temp.path().join(".shipyard");
        std::fs::create_dir_all(&project_dir).expect("project dir");
        let config_path = project_dir.join("config.toml");
        std::fs::write(&config_path, "[release.changelog]\nenabled = true\n").expect("config");
        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "changelog",
            "init",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, std::process::ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let payload: Value = serde_json::from_slice(&stdout).expect("json payload");
        assert_eq!(payload["command"], "changelog:init");
        assert_eq!(payload["status"], "already_configured");
        assert_eq!(
            std::fs::read_to_string(config_path).expect("config"),
            "[release.changelog]\nenabled = true\n"
        );
    }

    #[test]
    fn changelog_init_force_strips_release_sections_and_backs_up_changelog() {
        let temp = tempfile::tempdir().expect("tempdir");
        let project_dir = temp.path().join(".shipyard");
        std::fs::create_dir_all(&project_dir).expect("project dir");
        std::fs::write(
            project_dir.join("config.toml"),
            "[project]\nname = \"demo\"\n\n[release.changelog]\nenabled = false\n\n[release.post_tag_hook]\nenabled = false\n\n[tool]\nkeep = true\n",
        )
        .expect("config");
        std::fs::write(temp.path().join("CHANGELOG.md"), "hand maintained\n").expect("changelog");
        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "changelog",
            "init",
            "--force",
            "--product",
            "Forced Product",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, std::process::ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let payload: Value = serde_json::from_slice(&stdout).expect("json payload");
        assert_eq!(payload["status"], "written");
        assert_eq!(payload["existing_changelog"], true);
        let backup = temp.path().join("CHANGELOG.md.pre-shipyard.bak");
        assert_eq!(
            payload["changelog_backup"],
            backup.to_string_lossy().as_ref()
        );
        assert_eq!(
            std::fs::read_to_string(&backup).expect("backup"),
            "hand maintained\n"
        );
        let config = std::fs::read_to_string(project_dir.join("config.toml")).expect("config");
        assert!(config.contains("[tool]"));
        assert!(config.contains("keep = true"));
        assert!(config.contains("product    = \"Forced Product\""));
        assert_eq!(config.matches("[release.changelog]").count(), 1);
    }

    #[test]
    fn changelog_regenerate_json_writes_file_from_tags() {
        let temp = tempfile::tempdir().expect("tempdir");
        seed_changelog_repo(temp.path());
        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "changelog",
            "regenerate",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let payload: Value = serde_json::from_slice(&stdout).expect("json payload");
        assert_eq!(payload["command"], "changelog:regenerate");
        assert_eq!(payload["versions"], 2);
        assert_eq!(payload["drift_before"], true);
        let changelog =
            std::fs::read_to_string(temp.path().join("CHANGELOG.md")).expect("generated changelog");
        assert!(changelog.contains("# Changelog"));
        assert!(changelog.contains(r#"<a id="v020"></a>"#));
        assert!(changelog.contains("- feat: next ([#3](https://github.com/example/demo/pull/3))"));
        assert!(!changelog.contains("chore: bump package version"));
        assert!(changelog.contains("[0.1.0]: https://github.com/example/demo/releases/tag/v0.1.0"));
    }

    #[test]
    fn changelog_check_reports_sync_and_drift() {
        let temp = tempfile::tempdir().expect("tempdir");
        seed_changelog_repo(temp.path());
        let regenerate = Cli::parse_from([
            "shipyard",
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "changelog",
            "regenerate",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        assert_eq!(
            run_with(regenerate, &mut stdout, &mut stderr),
            ExitCode::SUCCESS
        );

        let check = Cli::parse_from([
            "shipyard",
            "--json",
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "changelog",
            "check",
        ]);
        stdout.clear();
        stderr.clear();
        let code = run_with(check, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        let payload: Value = serde_json::from_slice(&stdout).expect("json payload");
        assert_eq!(payload["command"], "changelog:check");
        assert_eq!(payload["drift"], false);

        std::fs::write(temp.path().join("CHANGELOG.md"), "stale\n").expect("stale changelog");
        let check = Cli::parse_from([
            "shipyard",
            "--json",
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "changelog",
            "check",
        ]);
        stdout.clear();
        stderr.clear();
        let code = run_with(check, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::from(1));
        let payload: Value = serde_json::from_slice(&stdout).expect("json payload");
        assert_eq!(payload["command"], "changelog:check");
        assert_eq!(payload["drift"], true);
    }

    #[test]
    fn changelog_release_notes_prints_tag_notes() {
        let temp = tempfile::tempdir().expect("tempdir");
        seed_changelog_repo(temp.path());
        let cli = Cli::parse_from([
            "shipyard",
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "changelog",
            "regenerate",
            "--release-notes",
            "v0.2.0",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let output = String::from_utf8(stdout).expect("stdout");
        assert!(output.contains("## What's new in v0.2.0"));
        assert!(output.contains("- feat: next (#3)"));
        assert!(output.contains("**Previous release:** [v0.1.0]"));
    }

    #[test]
    fn changelog_missing_config_json_exits_two() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "changelog",
            "regenerate",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::from(2));
        assert!(stderr.is_empty());
        let payload: Value = serde_json::from_slice(&stdout).expect("json payload");
        assert_eq!(payload["command"], "changelog:error");
        assert_eq!(payload["error"], "disabled");
    }

    #[test]
    fn repo_url_normalization_matches_python_shape() {
        assert_eq!(
            normalize_repo_url("git@github.com:owner/repo.git"),
            "https://github.com/owner/repo"
        );
        assert_eq!(
            normalize_repo_url("https://github.com/owner/repo.git"),
            "https://github.com/owner/repo"
        );
        assert_eq!(normalize_repo_url("file:///tmp/repo"), "file:///tmp/repo");
    }

    #[test]
    fn strip_release_sections_preserves_unrelated_sections() {
        let stripped = strip_release_sections(
            "[project]\nname = \"demo\"\n\n[release.changelog]\nenabled = true\n[release.post_tag_hook]\nenabled = true\n[tool]\nkeep = true\n",
        );

        assert!(stripped.contains("[project]"));
        assert!(stripped.contains("[tool]"));
        assert!(!stripped.contains("[release.changelog]"));
        assert!(!stripped.contains("[release.post_tag_hook]"));
    }

    fn seed_changelog_repo(root: &Path) {
        git(root, &["init", "--quiet", "--initial-branch=main"]);
        let project_dir = root.join(".shipyard");
        std::fs::create_dir_all(&project_dir).expect("project dir");
        std::fs::write(
            project_dir.join("config.toml"),
            "[release.changelog]\nenabled = true\npath = \"CHANGELOG.md\"\nrepo_url = \"https://github.com/example/demo\"\nproduct = \"Demo\"\ntag_filter = \"v*\"\n",
        )
        .expect("config");
        commit(root, "chore: config");
        std::fs::write(root.join("one.txt"), "one\n").expect("one");
        commit(root, "feat: initial (#1)");
        git(root, &["tag", "v0.1.0"]);
        std::fs::write(root.join("bump.txt"), "bump\n").expect("bump");
        commit(root, "chore: bump package version (#2)");
        git(root, &["tag", "v0.1.1"]);
        std::fs::write(root.join("two.txt"), "two\n").expect("two");
        commit(root, "feat: next (#3)");
        git(root, &["tag", "v0.2.0"]);
    }

    fn commit(root: &Path, message: &str) {
        git(root, &["add", "."]);
        git(
            root,
            &[
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "-m",
                message,
            ],
        );
    }

    fn git(root: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(["-c", "commit.gpgSign=false", "-c", "tag.gpgSign=false"])
            .args(args)
            .current_dir(root)
            .status()
            .expect("git should run");
        assert!(
            status.success(),
            "git failed in {}: {args:?}",
            root.display()
        );
    }
}
