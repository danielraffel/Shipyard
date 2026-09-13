//! Gathering the four facts the landability preflight needs, and the cache
//! that keeps the cost at two calls on a warm host.
//!
//! ## The budget is the design
//!
//! A check that runs on **every** ship has to be cheap enough that nobody is
//! ever tempted to turn it off, and quiet enough that it cannot contribute to
//! a secondary rate limit. GitHub's anti-burst throttle is separate from the
//! core quota and trips with tens of thousands of core calls still available,
//! taking the whole machine's API access with it — so "we have quota" is not a
//! licence to poll.
//!
//! Hence: **four calls cold, two warm, none in a loop.**
//!
//! | call | why it cannot be avoided | cached |
//! |---|---|---|
//! | branch protection | the required contexts are the *question*; the config copy is a fallback, not a source of truth | 300 s |
//! | repo variables | `runs-on` is an indirection through them | 300 s |
//! | repo runner census | the labels have to be matched against something | 300 s |
//! | org runner census | `repos/…/actions/runners` omits org-registered runners entirely, and returns the same empty list whether the lane is org-served or dead | 300 s |
//!
//! The workflow files are read from the local checkout with `git show`, which
//! costs nothing and is the correct source anyway: the workflow that runs on
//! `pull_request` is the one on the base ref.
//!
//! ## Both scopes, always
//!
//! Dropping the org census is the single most tempting saving here and it is
//! the one that produces confidently wrong refusals. Three of the six declared
//! self-hosted lanes on the fleet this was written against are served only by
//! org-scope runners. E2 in the negative-control plan exists to keep that
//! call honest.

use std::path::{Path, PathBuf};
use std::process::Command;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::cloud::GitHubActions;
use crate::fleet_service::{Boundary, RegisteredRunner, RunnerScope};

/// How long a cached census/variable read stays usable.
pub const CACHE_TTL_SECS: i64 = 300;

/// The four facts, plus what could not be read.
#[derive(Clone, Debug, Default)]
pub struct FleetFacts {
    /// Routing variables by name.
    pub variables: Vec<(String, String)>,
    /// Runner census spanning both scopes.
    pub census: Vec<RegisteredRunner>,
    /// Boundary that stopped the census, if any.
    pub census_boundary: Option<Boundary>,
    /// Boundary that stopped the routing-variable read, if any.
    ///
    /// Load-bearing, and discovered the hard way: when the variables call
    /// fails, every `fromJSON(vars.X || '"ubuntu-latest"')` job looks like an
    /// *unset* variable and falls back to the workflow's own hosted literal —
    /// which parses as `Hosted` and reports **Served**. On a host whose App
    /// token was returning 404 this produced five confident `served` verdicts
    /// for lanes it had not measured at all. An unread variable is
    /// `Unknown`, never a fallback.
    pub variables_boundary: Option<Boundary>,
    /// Required contexts from branch protection, when readable.
    pub required_contexts: Option<Vec<String>>,
    /// Which source supplied the contexts.
    pub contexts_source: String,
    /// Instrument problems worth printing.
    pub warnings: Vec<String>,
    /// How many API calls this gather actually made. Reported rather than
    /// estimated: a budget nobody measures is a wish.
    pub api_calls: u32,
}

impl FleetFacts {
    /// Look up one routing variable.
    #[must_use]
    pub fn variable(&self, name: &str) -> Option<&str> {
        self.variables
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CachedFacts {
    fetched_at: DateTime<Utc>,
    repo: String,
    base: String,
    variables: Vec<(String, String)>,
    census: Vec<CachedRunner>,
    census_boundary: Option<String>,
    #[serde(default)]
    variables_boundary: Option<String>,
    required_contexts: Option<Vec<String>>,
    contexts_source: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CachedRunner {
    name: String,
    scope: String,
    online: bool,
    busy: bool,
    labels: Vec<String>,
}

/// Path of the landability fact cache for a repo.
#[must_use]
pub fn cache_path(state_dir: &Path, repo: &str) -> PathBuf {
    let slug = repo.replace('/', "_");
    state_dir.join(format!("landability-facts-{slug}.json"))
}

/// Gather the four facts, using the cache when it is warm.
///
/// `now` is injected so the cache decision is testable without sleeping.
#[must_use]
pub fn gather(
    actions: &GitHubActions,
    state_dir: &Path,
    repo: &str,
    base: &str,
    now: DateTime<Utc>,
    ignore_cache: bool,
) -> FleetFacts {
    let path = cache_path(state_dir, repo);
    if !ignore_cache
        && let Some(cached) = read_cache(&path)
        && cached.repo == repo
        && cached.base == base
        && now.signed_duration_since(cached.fetched_at) <= Duration::seconds(CACHE_TTL_SECS)
        && now.signed_duration_since(cached.fetched_at) >= Duration::zero()
    {
        return from_cache(cached);
    }

    let mut facts = FleetFacts {
        contexts_source: "config".to_owned(),
        ..FleetFacts::default()
    };

    fetch_required_contexts(actions, repo, base, &mut facts);
    fetch_variables(actions, repo, &mut facts);
    fetch_census(actions, repo, &mut facts);

    write_cache(
        &path,
        &CachedFacts {
            fetched_at: now,
            repo: repo.to_owned(),
            base: base.to_owned(),
            variables: facts.variables.clone(),
            census: facts
                .census
                .iter()
                .map(|runner| CachedRunner {
                    name: runner.name.clone(),
                    scope: runner.scope.as_str().to_owned(),
                    online: runner.online,
                    busy: runner.busy,
                    labels: runner.labels.clone(),
                })
                .collect(),
            census_boundary: facts.census_boundary.map(|b| b.as_str().to_owned()),
            variables_boundary: facts.variables_boundary.map(|b| b.as_str().to_owned()),
            required_contexts: facts.required_contexts.clone(),
            contexts_source: facts.contexts_source.clone(),
        },
    );

    facts
}

/// Required contexts from branch protection. One call.
///
/// The config copy in `[governance] required_status_checks` is a *fallback*,
/// never the source of truth: it is edited by hand and branch protection is
/// what actually blocks a merge, so a divergence between them is itself worth
/// reporting rather than resolving silently in favour of whichever is closer.
fn fetch_required_contexts(
    actions: &GitHubActions,
    repo: &str,
    base: &str,
    facts: &mut FleetFacts,
) {
    facts.api_calls += 1;
    match run_api(
        actions,
        &format!("repos/{repo}/branches/{base}/protection/required_status_checks"),
    ) {
        Ok(raw) => match serde_json::from_str::<serde_json::Value>(&raw) {
            Ok(value) => {
                let contexts: Vec<String> = value
                    .get("contexts")
                    .and_then(serde_json::Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(serde_json::Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                if contexts.is_empty() {
                    facts.warnings.push(
                        "branch protection returned no required contexts; falling back to \
                         [governance] required_status_checks"
                            .to_owned(),
                    );
                } else {
                    facts.required_contexts = Some(contexts);
                    "branch_protection".clone_into(&mut facts.contexts_source);
                }
            }
            Err(error) => facts
                .warnings
                .push(format!("branch protection JSON malformed: {error}")),
        },
        Err(error) => facts.warnings.push(format!(
            "branch protection unreadable ({error}); using [governance] required_status_checks \
             from config, which may be a narrower set"
        )),
    }
}

/// Routing variables. One call.
fn fetch_variables(actions: &GitHubActions, repo: &str, facts: &mut FleetFacts) {
    facts.api_calls += 1;
    match run_api(
        actions,
        &format!("repos/{repo}/actions/variables?per_page=100"),
    ) {
        Ok(raw) => facts.variables = parse_variables(&raw),
        Err(error) => {
            facts.variables_boundary = Some(Boundary::Transport);
            facts.warnings.push(format!(
                "actions variables unreadable: {error} - every lane routed through a \
                 `vars.*_RUNS_ON_JSON` is reported Unknown rather than falling back to the \
                 workflow's literal, because an unread variable and an unset one are \
                 indistinguishable from here and only one of them is safe to assume"
            ));
        }
    }
}

/// Runner census in **both** scopes. Two calls, and neither is optional.
///
/// `repos/{owner}/{repo}/actions/runners` omits org-registered runners
/// entirely and returns the same empty list whether a lane is org-served or
/// genuinely dead. Dropping the second call is the most tempting saving here
/// and the one that produces confidently wrong refusals.
fn fetch_census(actions: &GitHubActions, repo: &str, facts: &mut FleetFacts) {
    let mut census = Vec::new();
    let mut repo_ok = false;
    let mut org_ok = false;

    facts.api_calls += 1;
    match run_api(
        actions,
        &format!("repos/{repo}/actions/runners?per_page=100"),
    ) {
        Ok(raw) => {
            repo_ok = true;
            census.extend(parse_runners(&raw, RunnerScope::Repo));
        }
        Err(error) => facts
            .warnings
            .push(format!("repo runner census unreadable: {error}")),
    }

    if let Some(org) = repo.split('/').next() {
        facts.api_calls += 1;
        match run_api(actions, &format!("orgs/{org}/actions/runners?per_page=100")) {
            Ok(raw) => {
                org_ok = true;
                census.extend(parse_runners(&raw, RunnerScope::Org));
            }
            Err(error) => facts.warnings.push(format!(
                "org runner census unreadable: {error} - a repo-scope-only census reports \
                 org-registered lanes as unserved while they are online, so no lane verdict is \
                 made from this half alone"
            )),
        }
    }

    facts.census = census;
    // A partial census is an unreadable census. Folding half a measurement into
    // a pass is the exact failure this module exists to end.
    if !repo_ok || !org_ok {
        facts.census_boundary = Some(if repo_ok || org_ok {
            Boundary::Scope
        } else {
            Boundary::Transport
        });
    }
}

fn from_cache(cached: CachedFacts) -> FleetFacts {
    FleetFacts {
        variables: cached.variables,
        census: cached
            .census
            .into_iter()
            .map(|runner| RegisteredRunner {
                name: runner.name,
                scope: if runner.scope == "org" {
                    RunnerScope::Org
                } else {
                    RunnerScope::Repo
                },
                online: runner.online,
                busy: runner.busy,
                labels: runner.labels,
            })
            .collect(),
        variables_boundary: cached.variables_boundary.as_deref().map(str_to_boundary),
        census_boundary: cached.census_boundary.as_deref().map(str_to_boundary),
        required_contexts: cached.required_contexts,
        contexts_source: cached.contexts_source,
        warnings: Vec::new(),
        api_calls: 0,
    }
}

fn str_to_boundary(value: &str) -> Boundary {
    match value {
        "scope" => Boundary::Scope,
        "permission" => Boundary::Permission,
        "identity" => Boundary::Identity,
        "grammar" => Boundary::Grammar,
        "parse" => Boundary::Parse,
        _ => Boundary::Transport,
    }
}

fn run_api(actions: &GitHubActions, path: &str) -> Result<String, String> {
    actions
        .run_gh(&["api".to_owned(), path.to_owned()])
        .map_err(|error| error.to_string())
}

/// Parse `GET /repos/{repo}/actions/variables`.
#[must_use]
pub fn parse_variables(raw: &str) -> Vec<(String, String)> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return Vec::new();
    };
    value
        .get("variables")
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let name = item.get("name")?.as_str()?.to_owned();
                    let value = item.get("value")?.as_str()?.to_owned();
                    Some((name, value))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parse a runner census page into typed runners.
#[must_use]
pub fn parse_runners(raw: &str, scope: RunnerScope) -> Vec<RegisteredRunner> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return Vec::new();
    };
    value
        .get("runners")
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    Some(RegisteredRunner {
                        name: item.get("name")?.as_str()?.to_owned(),
                        scope,
                        online: item.get("status").and_then(serde_json::Value::as_str)
                            == Some("online"),
                        busy: item
                            .get("busy")
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(false),
                        labels: item
                            .get("labels")
                            .and_then(serde_json::Value::as_array)
                            .map(|labels| {
                                labels
                                    .iter()
                                    .filter_map(|label| {
                                        label.get("name")?.as_str().map(str::to_owned)
                                    })
                                    .collect()
                            })
                            .unwrap_or_default(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Read a workflow file from a git ref without touching the network.
pub fn read_workflow_at_ref(repo_root: &Path, git_ref: &str, path: &str) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("show")
        .arg(format!("{git_ref}:{path}"))
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    String::from_utf8(output.stdout).map_err(|error| error.to_string())
}

fn read_cache(path: &Path) -> Option<CachedFacts> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn write_cache(path: &Path, cached: &CachedFacts) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(raw) = serde_json::to_string_pretty(cached) {
        let _ = std::fs::write(path, raw);
    }
}
