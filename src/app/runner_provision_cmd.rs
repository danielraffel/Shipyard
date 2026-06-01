//! CLI handlers for `shipyard runner register|list|remove|tag`.
//!
//! This is the shell-out side of runner provisioning: it talks to `gh`
//! (registration/removal tokens, the runners API, the runner release asset),
//! the GitHub Actions runner's own `config.sh`/`svc.sh`, and the local
//! `~/actions-runner-*` directories. All naming/index/label/table logic is the
//! pure code in [`crate::runner_provision`]; this module only does I/O.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use serde_json::Value;

use super::CliFailure;
use super::runner_cmd::{parse_github_repo_slug, resolve_repo_slug};
use crate::cloud::GitHubActions;
use crate::identity::RuntimeMode;
use crate::output::write_json_envelope;
use crate::paths::RuntimePaths;
use crate::runner_provision::{
    ApiRunner, AuditFinding, PoolRow, audit_runners, default_labels, format_audit_table,
    format_pool_table, next_index, orphan_local_runners, pool_rows, runner_name, short_repo,
    validate_machine_tag,
};

/// Fetch every self-hosted runner for a repo across **all** pages. GitHub caps
/// `per_page` at 100, so a one-page fetch silently misses runners on a large
/// fleet — `gh api --paginate` follows the `Link` headers and `--jq '.runners[]'`
/// streams each runner object (newline-delimited) across pages.
fn fetch_all_runners(actions: &GitHubActions, slug: &str) -> Result<Vec<ApiRunner>, CliFailure> {
    let raw = actions
        .run_gh(&[
            "api".to_owned(),
            "--paginate".to_owned(),
            format!("repos/{slug}/actions/runners?per_page=100"),
            "--jq".to_owned(),
            ".runners[]".to_owned(),
        ])
        .map_err(|e| CliFailure::new(2, format!("failed to list runners for {slug}: {e}")))?;
    let mut runners = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let runner: ApiRunner = serde_json::from_str(line)
            .map_err(|e| CliFailure::new(2, format!("runner JSON parse failed for {slug}: {e}")))?;
        runners.push(runner);
    }
    Ok(runners)
}

/// jq filter selecting the Apple-silicon macOS runner tarball download URL.
const RUNNER_ASSET_JQ: &str =
    r#".assets[] | select(.name | test("osx-arm64.*tar.gz$")) | .browser_download_url"#;

// ---------- tag ----------

fn machine_tag_path(mode: RuntimeMode) -> PathBuf {
    RuntimePaths::current(mode).state_dir.join("machine-tag")
}

/// Read this box's stored machine tag, if any.
fn read_stored_tag(mode: RuntimeMode) -> Option<String> {
    let raw = fs::read_to_string(machine_tag_path(mode)).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

/// `shipyard runner tag [--set <tag>]`.
pub(super) fn tag_command<W: Write>(
    mode: RuntimeMode,
    set: Option<String>,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    if let Some(tag) = set {
        let tag = tag.trim().to_owned();
        validate_machine_tag(&tag).map_err(|reason| CliFailure::new(2, reason))?;
        let path = machine_tag_path(mode);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| CliFailure::new(1, format!("failed to create state dir: {e}")))?;
        }
        fs::write(&path, format!("{tag}\n"))
            .map_err(|e| CliFailure::new(1, format!("failed to write machine tag: {e}")))?;
        if json {
            let mut data = BTreeMap::new();
            data.insert("machine_tag".to_owned(), Value::from(tag.clone()));
            data.insert("path".to_owned(), Value::from(path.display().to_string()));
            envelope(stdout, "runner.tag", data)?;
        } else {
            writeln!(stdout, "machine tag set to `{tag}` ({})", path.display()).ok();
        }
        return Ok(ExitCode::SUCCESS);
    }

    match read_stored_tag(mode) {
        Some(tag) => {
            if json {
                let mut data = BTreeMap::new();
                data.insert("machine_tag".to_owned(), Value::from(tag.clone()));
                envelope(stdout, "runner.tag", data)?;
            } else {
                writeln!(stdout, "{tag}").ok();
            }
            Ok(ExitCode::SUCCESS)
        }
        None => Err(CliFailure::new(
            1,
            "No machine tag set. Set one with `shipyard runner tag --set <studio|m1|m5>`.",
        )),
    }
}

// ---------- register ----------

/// Inputs for [`register_command`].
pub(super) struct RegisterArgs<'a> {
    pub mode: RuntimeMode,
    pub cwd: &'a Path,
    pub actions: &'a GitHubActions,
    pub repo: Option<String>,
    pub count: u32,
    pub machine_tag: Option<String>,
    pub labels: Vec<String>,
    pub ci_root: Option<PathBuf>,
    pub dry_run: bool,
    pub json: bool,
}

fn default_ci_root() -> PathBuf {
    home_dir().join("actions-ci")
}

fn home_dir() -> PathBuf {
    std::env::var("HOME").map_or_else(|_| PathBuf::from("."), PathBuf::from)
}

fn cpu_count() -> usize {
    Command::new("sysctl")
        .args(["-n", "hw.ncpu"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(4)
}

/// The per-runner `.env` the GitHub Actions service loads: points jobs at the
/// shared caches and isolates each runner's ccache base path so cross-worktree
/// hits work (`CCACHE_BASEDIR` + `CCACHE_NOHASHDIR`). Cache *size* is owned by
/// the host's `ccache.conf`, not set here.
fn runner_env_file(ci_root: &Path, work: &Path, parallel: usize) -> String {
    let cache = ci_root.join("cache");
    format!(
        "CCACHE_DIR={ccache}\n\
         CCACHE_BASEDIR={work}\n\
         CCACHE_NOHASHDIR=true\n\
         CCACHE_DEPEND=true\n\
         CCACHE_SLOPPINESS=time_macros,pch_defines\n\
         CMAKE_BUILD_PARALLEL_LEVEL={parallel}\n\
         CTEST_PARALLEL_LEVEL={parallel}\n\
         FETCHCONTENT_BASE_DIR={fetchcontent}\n",
        ccache = cache.join("ccache").display(),
        work = work.display(),
        fetchcontent = cache.join("fetchcontent-src").display(),
    )
}

/// Existing runner names registered on a repo (any machine), for index
/// continuation.
fn existing_runner_names(actions: &GitHubActions, slug: &str) -> Result<Vec<String>, CliFailure> {
    // Paginated: a fleet with >100 runners must not under-count, or the next
    // index could collide with an existing `<repo>-<tag>-NN`.
    Ok(fetch_all_runners(actions, slug)?
        .into_iter()
        .map(|r| r.name)
        .collect())
}

/// `shipyard runner register`.
#[allow(clippy::needless_pass_by_value, clippy::too_many_lines)]
pub(super) fn register_command<W: Write>(
    args: RegisterArgs,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    if args.count == 0 {
        return Err(CliFailure::new(2, "--count must be at least 1"));
    }
    let slug = resolve_repo_slug(args.repo.clone(), args.cwd)?;
    let repo_short = short_repo(&slug).to_owned();

    let tag = match args.machine_tag.clone() {
        Some(tag) => {
            let tag = tag.trim().to_owned();
            validate_machine_tag(&tag).map_err(|reason| CliFailure::new(2, reason))?;
            tag
        }
        None => read_stored_tag(args.mode).ok_or_else(|| {
            CliFailure::new(
                1,
                "No machine tag set. Pass --machine-tag or run `shipyard runner tag --set <tag>`.",
            )
        })?,
    };
    validate_machine_tag(&tag).map_err(|reason| CliFailure::new(2, reason))?;

    let labels = if args.labels.is_empty() {
        default_labels(&repo_short, &tag)
    } else {
        args.labels.clone()
    };
    let labels_csv = labels.join(",");
    let ci_root = args.ci_root.clone().unwrap_or_else(default_ci_root);

    let existing = existing_runner_names(args.actions, &slug)?;
    let start = next_index(&existing, &repo_short, &tag);
    let parallel = (cpu_count() / args.count as usize).max(1);

    // Build the plan first so dry-run and real runs agree.
    let plan: Vec<RunnerPlan> = (0..args.count)
        .map(|k| {
            let name = runner_name(&repo_short, &tag, start + k);
            RunnerPlan {
                work: ci_root.join("work").join(&name),
                dir: home_dir().join(format!("actions-runner-{name}")),
                name,
            }
        })
        .collect();

    if args.dry_run {
        return report_register(
            stdout,
            args.json,
            &slug,
            &tag,
            &labels_csv,
            &ci_root,
            parallel,
            &plan,
            true,
        );
    }

    // Resolve + cache the runner tarball once, then provision each runner.
    let pkg_url = args
        .actions
        .run_gh(&[
            "api".to_owned(),
            "repos/actions/runner/releases/latest".to_owned(),
            "--jq".to_owned(),
            RUNNER_ASSET_JQ.to_owned(),
        ])
        .map_err(|e| CliFailure::new(2, format!("failed to resolve runner package URL: {e}")))?
        .trim()
        .to_owned();
    if pkg_url.is_empty() {
        return Err(CliFailure::new(
            2,
            "could not resolve the osx-arm64 actions-runner package URL",
        ));
    }
    let pkg_name = pkg_url
        .rsplit('/')
        .next()
        .unwrap_or("actions-runner.tar.gz");
    let pkg_cache = ci_root.join("cache").join("actions-runner-pkg");
    fs::create_dir_all(&pkg_cache)
        .map_err(|e| CliFailure::new(1, format!("failed to create package cache: {e}")))?;
    let pkg_path = pkg_cache.join(pkg_name);
    if !pkg_path.exists() {
        run(
            "curl",
            &["-fsSL", "-o", &pkg_path.to_string_lossy(), &pkg_url],
            "download runner",
        )?;
    }

    for entry in &plan {
        fs::create_dir_all(&entry.dir)
            .map_err(|e| CliFailure::new(1, format!("failed to create runner dir: {e}")))?;
        fs::create_dir_all(&entry.work)
            .map_err(|e| CliFailure::new(1, format!("failed to create work dir: {e}")))?;
        if !entry.dir.join("config.sh").exists() {
            run(
                "tar",
                &[
                    "xzf",
                    &pkg_path.to_string_lossy(),
                    "-C",
                    &entry.dir.to_string_lossy(),
                ],
                "extract runner",
            )?;
        }
        fs::write(
            entry.dir.join(".env"),
            runner_env_file(&ci_root, &entry.work, parallel),
        )
        .map_err(|e| CliFailure::new(1, format!("failed to write .env: {e}")))?;

        let token = args
            .actions
            .run_gh(&[
                "api".to_owned(),
                "-X".to_owned(),
                "POST".to_owned(),
                format!("repos/{slug}/actions/runners/registration-token"),
                "--jq".to_owned(),
                ".token".to_owned(),
            ])
            .map_err(|e| CliFailure::new(2, format!("failed to get registration token: {e}")))?
            .trim()
            .to_owned();

        run_in(
            &entry.dir,
            "./config.sh",
            &[
                "--unattended",
                "--replace",
                "--url",
                &format!("https://github.com/{slug}"),
                "--token",
                &token,
                "--name",
                &entry.name,
                "--labels",
                &labels_csv,
                "--work",
                &entry.work.to_string_lossy(),
            ],
            "configure runner",
        )?;
        run_in(
            &entry.dir,
            "./svc.sh",
            &["install"],
            "install runner service",
        )?;
        run_in(&entry.dir, "./svc.sh", &["start"], "start runner service")?;
    }

    report_register(
        stdout,
        args.json,
        &slug,
        &tag,
        &labels_csv,
        &ci_root,
        parallel,
        &plan,
        false,
    )
}

struct RunnerPlan {
    name: String,
    dir: PathBuf,
    work: PathBuf,
}

#[allow(clippy::too_many_arguments)]
fn report_register<W: Write>(
    stdout: &mut W,
    json: bool,
    slug: &str,
    tag: &str,
    labels_csv: &str,
    ci_root: &Path,
    parallel: usize,
    plan: &[RunnerPlan],
    dry_run: bool,
) -> Result<ExitCode, CliFailure> {
    if json {
        let mut data = BTreeMap::new();
        data.insert("repo".to_owned(), Value::from(slug.to_owned()));
        data.insert("machine_tag".to_owned(), Value::from(tag.to_owned()));
        data.insert("labels".to_owned(), Value::from(labels_csv.to_owned()));
        data.insert("dry_run".to_owned(), Value::from(dry_run));
        data.insert(
            "parallel_per_runner".to_owned(),
            Value::from(parallel as u64),
        );
        let runners: Vec<Value> = plan
            .iter()
            .map(|p| {
                let mut m = serde_json::Map::new();
                m.insert("name".to_owned(), Value::from(p.name.clone()));
                m.insert("dir".to_owned(), Value::from(p.dir.display().to_string()));
                m.insert("work".to_owned(), Value::from(p.work.display().to_string()));
                Value::Object(m)
            })
            .collect();
        data.insert("runners".to_owned(), Value::from(runners));
        envelope(stdout, "runner.register", data)?;
        return Ok(ExitCode::SUCCESS);
    }

    let verb = if dry_run {
        "Would register"
    } else {
        "Registered"
    };
    writeln!(
        stdout,
        "{verb} {} runner(s) for {slug} [tag={tag}, ~{parallel} cores each]",
        plan.len()
    )
    .ok();
    writeln!(stdout, "  labels:  {labels_csv}").ok();
    writeln!(stdout, "  ci-root: {}", ci_root.display()).ok();
    for p in plan {
        writeln!(stdout, "  - {}  (work={})", p.name, p.work.display()).ok();
    }
    if dry_run {
        writeln!(stdout, "\nRe-run without --dry-run to apply.").ok();
    }
    Ok(ExitCode::SUCCESS)
}

// ---------- list ----------

struct LocalRunner {
    name: String,
    repo_slug: String,
}

/// Parse a runner `.runner` config file, returning `(agent_name, repo_slug)`.
fn parse_dot_runner(raw: &str) -> Option<(String, String)> {
    // `.runner` files are written with a UTF-8 BOM; strip it before parsing.
    let cleaned = raw.trim_start_matches('\u{feff}').trim_start();
    let value: Value = serde_json::from_str(cleaned).ok()?;
    let name = value.get("agentName")?.as_str()?.to_owned();
    let url = value.get("gitHubUrl")?.as_str()?;
    let slug = parse_github_repo_slug(url)?;
    Some((name, slug))
}

/// Discover configured runners from this machine's `~/actions-runner*` dirs.
fn scan_local_runners() -> Vec<LocalRunner> {
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir(home_dir()) else {
        return found;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with("actions-runner") {
            continue;
        }
        let dot = entry.path().join(".runner");
        let Ok(raw) = fs::read_to_string(&dot) else {
            continue;
        };
        if let Some((agent, slug)) = parse_dot_runner(&raw) {
            found.push(LocalRunner {
                name: agent,
                repo_slug: slug,
            });
        }
    }
    found
}

/// `shipyard runner list`.
pub(super) fn list_command<W: Write>(
    cwd: &Path,
    actions: &GitHubActions,
    repo: &[String],
    all_repos: bool,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let locals = scan_local_runners();

    let mut slugs: Vec<String> = Vec::new();
    let push = |slug: String, slugs: &mut Vec<String>| {
        if !slug.is_empty() && !slugs.iter().any(|s| s.eq_ignore_ascii_case(&slug)) {
            slugs.push(slug);
        }
    };
    for r in repo {
        push(r.clone(), &mut slugs);
    }
    if repo.is_empty() || all_repos {
        for local in &locals {
            push(local.repo_slug.clone(), &mut slugs);
        }
    }
    if let Ok(current) = resolve_repo_slug(None, cwd) {
        push(current, &mut slugs);
    }
    if slugs.is_empty() {
        return Err(CliFailure::new(
            1,
            "No repos to query. Pass --repo OWNER/REPO, or run where local runner dirs exist.",
        ));
    }

    let mut rows: Vec<PoolRow> = Vec::new();
    let mut github_names: Vec<String> = Vec::new();
    for slug in &slugs {
        let runners = fetch_all_runners(actions, slug)?;
        for r in &runners {
            github_names.push(r.name.clone());
        }
        rows.extend(pool_rows(short_repo(slug), &runners));
    }

    let local_names: Vec<String> = locals.iter().map(|l| l.name.clone()).collect();
    let orphans = orphan_local_runners(&local_names, &github_names);

    if json {
        let mut data = BTreeMap::new();
        data.insert("repos".to_owned(), Value::from(slugs.clone()));
        let row_values: Vec<Value> = rows
            .iter()
            .map(|r| {
                let mut m = serde_json::Map::new();
                m.insert("name".to_owned(), Value::from(r.name.clone()));
                m.insert("repo".to_owned(), Value::from(r.repo.clone()));
                m.insert("machine".to_owned(), Value::from(r.machine.clone()));
                m.insert("status".to_owned(), Value::from(r.status.clone()));
                m.insert("busy".to_owned(), Value::from(r.busy));
                m.insert("labels".to_owned(), Value::from(r.labels.clone()));
                Value::Object(m)
            })
            .collect();
        data.insert("runners".to_owned(), Value::from(row_values));
        data.insert("orphans".to_owned(), Value::from(orphans.clone()));
        envelope(stdout, "runner.list", data)?;
        return Ok(ExitCode::SUCCESS);
    }

    writeln!(stdout, "{}", format_pool_table(&rows)).ok();
    if !orphans.is_empty() {
        writeln!(
            stdout,
            "\n⚠︎ {} local runner dir(s) not registered on GitHub (orphaned — remove with `shipyard runner remove`):",
            orphans.len()
        )
        .ok();
        for name in &orphans {
            writeln!(
                stdout,
                "  - {name}  (~/actions-runner-{name} or ~/actions-runner)"
            )
            .ok();
        }
    }
    Ok(ExitCode::SUCCESS)
}

// ---------- audit ----------

/// Resolve the repo slugs to audit, mirroring `list_command`'s resolution:
/// explicit `--repo`, local runner dirs, then the current checkout.
fn resolve_audit_slugs(cwd: &Path, repo: &[String]) -> Result<Vec<String>, CliFailure> {
    let locals = scan_local_runners();
    let mut slugs: Vec<String> = Vec::new();
    let mut push = |slug: String| {
        if !slug.is_empty() && !slugs.iter().any(|s| s.eq_ignore_ascii_case(&slug)) {
            slugs.push(slug);
        }
    };
    for r in repo {
        push(r.clone());
    }
    if repo.is_empty() {
        for local in &locals {
            push(local.repo_slug.clone());
        }
        if let Ok(current) = resolve_repo_slug(None, cwd) {
            push(current);
        }
    }
    if slugs.is_empty() {
        return Err(CliFailure::new(
            1,
            "No repos to audit. Pass --repo OWNER/REPO, or run where local runner dirs exist.",
        ));
    }
    Ok(slugs)
}

/// `shipyard runner audit` — flag host-class naming/label drift across a repo's
/// runners. Exit 0 when every runner conforms; exit 1 when any drift is found
/// (CI-friendly). Pure logic lives in [`crate::runner_provision::audit_runners`].
pub(super) fn audit_command<W: Write>(
    cwd: &Path,
    actions: &GitHubActions,
    repo: &[String],
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let slugs = resolve_audit_slugs(cwd, repo)?;

    let mut findings: Vec<(String, AuditFinding)> = Vec::new();
    for slug in &slugs {
        let runners = fetch_all_runners(actions, slug)?;
        let repo_short = short_repo(slug);
        for finding in audit_runners(repo_short, &runners) {
            findings.push((repo_short.to_owned(), finding));
        }
    }

    let with_issues = findings.iter().filter(|(_, f)| f.has_issues()).count();
    let drift = findings.iter().any(|(_, f)| f.is_drift());
    let exit = if with_issues == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    };

    if json {
        let mut data = BTreeMap::new();
        data.insert("repos".to_owned(), Value::from(slugs.clone()));
        let finding_values: Vec<Value> = findings
            .iter()
            .map(|(repo_short, f)| {
                let mut m = serde_json::Map::new();
                m.insert("name".to_owned(), Value::from(f.name.clone()));
                m.insert("repo".to_owned(), Value::from(repo_short.clone()));
                m.insert(
                    "name_class".to_owned(),
                    f.name_class.clone().map_or(Value::Null, Value::from),
                );
                m.insert(
                    "label_class".to_owned(),
                    f.label_class.clone().map_or(Value::Null, Value::from),
                );
                m.insert("ok".to_owned(), Value::from(!f.has_issues()));
                m.insert("drift".to_owned(), Value::from(f.is_drift()));
                m.insert(
                    "issues".to_owned(),
                    Value::from(
                        f.issues
                            .iter()
                            .map(|i| Value::from(i.code()))
                            .collect::<Vec<_>>(),
                    ),
                );
                Value::Object(m)
            })
            .collect();
        data.insert("findings".to_owned(), Value::from(finding_values));
        data.insert("with_issues".to_owned(), Value::from(with_issues));
        data.insert("drift".to_owned(), Value::from(drift));
        envelope(stdout, "runner.audit", data)?;
        return Ok(exit);
    }

    let bare: Vec<AuditFinding> = findings.into_iter().map(|(_, f)| f).collect();
    writeln!(stdout, "{}", format_audit_table(&bare)).ok();
    if with_issues == 0 {
        writeln!(stdout, "\n✓ All runners conform to the host-class scheme.").ok();
    } else {
        writeln!(
            stdout,
            "\n⚠︎ {with_issues} runner(s) drift from the host-class scheme \
             (<repo>-<class>-NN + <repo>-build / <repo>-build-<class>).\n  \
             Fix labels with `shipyard runner register --labels …` or re-tag/re-register \
             the host; physical host class is confirmed by `shipyard runner capacity`."
        )
        .ok();
    }
    Ok(exit)
}

// ---------- remove ----------

/// `shipyard runner remove`.
#[allow(clippy::too_many_arguments)]
pub(super) fn remove_command<W: Write>(
    cwd: &Path,
    actions: &GitHubActions,
    name: String,
    repo: Option<String>,
    purge_dir: bool,
    yes: bool,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    if !yes {
        return Err(CliFailure::new(
            2,
            format!("Refusing to remove `{name}` without confirmation. Re-run with --yes."),
        ));
    }
    let slug = resolve_repo_slug(repo, cwd)?;
    let dir = home_dir().join(format!("actions-runner-{name}"));
    if !dir.join("config.sh").exists() {
        return Err(CliFailure::new(
            1,
            format!("no configured runner dir at {}", dir.display()),
        ));
    }

    let token = actions
        .run_gh(&[
            "api".to_owned(),
            "-X".to_owned(),
            "POST".to_owned(),
            format!("repos/{slug}/actions/runners/remove-token"),
            "--jq".to_owned(),
            ".token".to_owned(),
        ])
        .map_err(|e| CliFailure::new(2, format!("failed to get removal token: {e}")))?
        .trim()
        .to_owned();

    // Stop the service first; ignore failure (it may already be stopped).
    let _ = Command::new("./svc.sh")
        .current_dir(&dir)
        .arg("stop")
        .status();
    run_in(
        &dir,
        "./config.sh",
        &["remove", "--token", &token],
        "deregister runner",
    )?;

    if purge_dir {
        fs::remove_dir_all(&dir)
            .map_err(|e| CliFailure::new(1, format!("failed to purge runner dir: {e}")))?;
    }

    if json {
        let mut data = BTreeMap::new();
        data.insert("removed".to_owned(), Value::from(name));
        data.insert("repo".to_owned(), Value::from(slug));
        data.insert("purged_dir".to_owned(), Value::from(purge_dir));
        envelope(stdout, "runner.remove", data)?;
    } else {
        writeln!(stdout, "Removed runner `{name}` from {slug}").ok();
        if purge_dir {
            writeln!(stdout, "  purged {}", dir.display()).ok();
        }
    }
    Ok(ExitCode::SUCCESS)
}

// ---------- shared shell helpers ----------

fn run(program: &str, args: &[&str], what: &str) -> Result<(), CliFailure> {
    let status = Command::new(program)
        .args(args)
        .status()
        .map_err(|e| CliFailure::new(1, format!("failed to {what}: {e}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(CliFailure::new(
            1,
            format!("{what} failed (exit {:?})", status.code()),
        ))
    }
}

fn run_in(dir: &Path, program: &str, args: &[&str], what: &str) -> Result<(), CliFailure> {
    let status = Command::new(program)
        .current_dir(dir)
        .args(args)
        .status()
        .map_err(|e| CliFailure::new(1, format!("failed to {what}: {e}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(CliFailure::new(
            1,
            format!("{what} failed (exit {:?})", status.code()),
        ))
    }
}

fn envelope<W: Write>(
    stdout: &mut W,
    command: &str,
    data: BTreeMap<String, Value>,
) -> Result<(), CliFailure> {
    write_json_envelope(stdout, command, data)
        .map_err(|e| CliFailure::new(1, format!("failed to write JSON: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_dot_runner_handles_bom_and_extracts_slug() {
        let raw = "\u{feff}{\"agentName\":\"pulp-m1-01\",\"gitHubUrl\":\"https://github.com/danielraffel/pulp\"}";
        let (name, slug) = parse_dot_runner(raw).expect("parse");
        assert_eq!(name, "pulp-m1-01");
        assert_eq!(slug, "danielraffel/pulp");
    }

    #[test]
    fn parse_dot_runner_rejects_missing_fields() {
        assert!(parse_dot_runner("{}").is_none());
        assert!(parse_dot_runner("not json").is_none());
    }

    #[test]
    fn runner_env_file_points_at_shared_caches() {
        let env = runner_env_file(
            Path::new("/Volumes/Workshop/ci/pulp"),
            Path::new("/Volumes/Workshop/ci/pulp/work/pulp-studio-01"),
            9,
        );
        assert!(env.contains("CCACHE_DIR=/Volumes/Workshop/ci/pulp/cache/ccache"));
        assert!(env.contains("CCACHE_BASEDIR=/Volumes/Workshop/ci/pulp/work/pulp-studio-01"));
        assert!(
            env.contains("FETCHCONTENT_BASE_DIR=/Volumes/Workshop/ci/pulp/cache/fetchcontent-src")
        );
        assert!(env.contains("CMAKE_BUILD_PARALLEL_LEVEL=9"));
        assert!(env.contains("CTEST_PARALLEL_LEVEL=9"));
        assert!(env.contains("CCACHE_NOHASHDIR=true"));
    }
}
