//! One steward pass's GitHub reads: how many, where, and a narrow cache.
//!
//! A [`GhPass`] is attached to a [`crate::cloud::GitHubActions`] for the
//! length of one pass. Every `gh` child the client spawns is counted by call
//! site (REST or GraphQL, endpoint shape), so a pass can report what it cost.
//! When caching is on, a successful answer to a read in [`pass_cacheable`] is
//! reused for the rest of the pass instead of being asked again.
//!
//! Only two kinds of read are cacheable, and both are safe to reuse within a
//! pass by construction:
//!
//! * content-addressed reads, whose answer cannot change: a commit, tree or
//!   blob by full SHA, or a compare between two full SHAs;
//! * the base branch's policy: a file read at a named ref (the base's
//!   `.shipyard/config.toml`), the base's evaluated rules and its branch
//!   protection. The pass already takes those as one snapshot elsewhere (the
//!   unapproved-admission sweep reads its policy once per pass).
//!
//! Nothing that decides whether one head may land right now is cached:
//! reviews, comments, check runs, statuses, queue state, the pull request
//! itself and every GraphQL read are always asked fresh, so each live
//! revalidation before a mutation still sees the live answer. A failed read is
//! never cached either; the next caller asks again.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;

/// Which GitHub budget a `gh` command spends.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ApiKind {
    /// The REST core budget.
    Rest,
    /// The GraphQL budget.
    Graphql,
}

#[derive(Default)]
struct SiteStats {
    calls: u64,
    millis: u128,
}

#[derive(Default)]
struct Inner {
    sites: BTreeMap<(ApiKind, String), SiteStats>,
    cache: HashMap<Vec<String>, String>,
    cache_hits: BTreeMap<(ApiKind, String), u64>,
    fallbacks: BTreeMap<String, (u64, String)>,
}

/// Counters, and optionally a read cache, for one pass.
#[derive(Default)]
pub struct GhPass {
    cache_reads: bool,
    inner: Mutex<Inner>,
}

impl std::fmt::Debug for GhPass {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GhPass")
            .field("cache_reads", &self.cache_reads)
            .finish_non_exhaustive()
    }
}

/// One call site's line in a [`GhPassReport`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SiteReport {
    /// Budget spent.
    pub kind: ApiKind,
    /// Endpoint shape, with SHAs, numbers and query strings elided.
    pub site: String,
    /// Children spawned.
    pub calls: u64,
    /// Answers served from the pass cache instead.
    pub cache_hits: u64,
    /// Wall time spent in those children, in milliseconds.
    pub millis: u128,
}

/// What one pass asked GitHub.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct GhPassReport {
    /// REST children spawned.
    pub rest: u64,
    /// GraphQL children spawned.
    pub graphql: u64,
    /// Reads answered from the pass cache.
    pub cache_hits: u64,
    /// Summed wall time of every child, in milliseconds. Children run
    /// concurrently during hydration, so this can exceed the pass's own time.
    pub millis: u128,
    /// Per call site, most calls first.
    pub sites: Vec<SiteReport>,
    /// Faster reads that were refused and answered by a slower path instead,
    /// by reason. Empty on a healthy pass; a reason that keeps appearing is a
    /// schema change or a persistent failure, not noise.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fallbacks: Vec<FallbackReport>,
}

/// One reason a pass fell back from a faster read to a slower one.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FallbackReport {
    /// What fell back, and why, in a stable short form.
    pub reason: String,
    /// How many times this pass.
    pub count: u64,
    /// The last refusal's message, bounded.
    pub last_detail: String,
}

impl GhPass {
    /// Counters only.
    #[must_use]
    pub fn counting() -> Self {
        Self::default()
    }

    /// Counters, and reuse of [`pass_cacheable`] reads for the pass.
    #[must_use]
    pub fn caching() -> Self {
        Self {
            cache_reads: true,
            inner: Mutex::default(),
        }
    }

    /// A cached answer to `args`, when caching is on and one is held.
    pub(crate) fn cached(&self, args: &[String]) -> Option<String> {
        if !self.cache_reads || !pass_cacheable(args) {
            return None;
        }
        let mut inner = self.inner.lock().ok()?;
        let hit = inner.cache.get(args).cloned()?;
        *inner.cache_hits.entry(site(args)).or_default() += 1;
        Some(hit)
    }

    /// Keep a successful answer to `args` for the rest of the pass.
    pub(crate) fn remember(&self, args: &[String], stdout: &str) {
        if !self.cache_reads || !pass_cacheable(args) {
            return;
        }
        if let Ok(mut inner) = self.inner.lock() {
            inner.cache.insert(args.to_vec(), stdout.to_owned());
        }
    }

    /// Count one spawned child.
    pub(crate) fn record(&self, args: &[String], elapsed: Duration) {
        if let Ok(mut inner) = self.inner.lock() {
            let stats = inner.sites.entry(site(args)).or_default();
            stats.calls += 1;
            stats.millis += elapsed.as_millis();
        }
    }

    /// Count one fallback from a faster read to a slower one.
    pub(crate) fn record_fallback(&self, reason: &str, detail: &str) {
        if let Ok(mut inner) = self.inner.lock() {
            let entry = inner
                .fallbacks
                .entry(reason.to_owned())
                .or_insert((0, String::new()));
            entry.0 += 1;
            entry.1 = detail.chars().take(300).collect();
        }
    }

    /// Everything counted so far.
    #[must_use]
    pub fn report(&self) -> GhPassReport {
        let Ok(inner) = self.inner.lock() else {
            return GhPassReport::default();
        };
        let mut keys = inner
            .sites
            .keys()
            .chain(inner.cache_hits.keys())
            .cloned()
            .collect::<Vec<_>>();
        keys.sort();
        keys.dedup();
        let mut report = GhPassReport::default();
        for key in keys {
            let (calls, millis) = inner
                .sites
                .get(&key)
                .map_or((0, 0), |stats| (stats.calls, stats.millis));
            let cache_hits = inner.cache_hits.get(&key).copied().unwrap_or(0);
            match key.0 {
                ApiKind::Rest => report.rest += calls,
                ApiKind::Graphql => report.graphql += calls,
            }
            report.cache_hits += cache_hits;
            report.millis += millis;
            report.sites.push(SiteReport {
                kind: key.0,
                site: key.1,
                calls,
                cache_hits,
                millis,
            });
        }
        report.fallbacks = inner
            .fallbacks
            .iter()
            .map(|(reason, (count, detail))| FallbackReport {
                reason: reason.clone(),
                count: *count,
                last_detail: detail.clone(),
            })
            .collect();
        report.sites.sort_by(|left, right| {
            right
                .calls
                .cmp(&left.calls)
                .then(left.site.cmp(&right.site))
        });
        report
    }
}

/// `gh api` flags that take a value.
const API_VALUE_FLAGS: &[&str] = &[
    "-X",
    "--method",
    "-H",
    "--header",
    "-f",
    "--raw-field",
    "-F",
    "--field",
    "-q",
    "--jq",
    "-t",
    "--template",
    "--input",
    "--cache",
    "--hostname",
    "-p",
    "--preview",
];

/// Flags that make `gh api` write, or send a body.
const API_WRITE_FLAGS: &[&str] = &[
    "-X",
    "--method",
    "-f",
    "--raw-field",
    "-F",
    "--field",
    "--input",
];

/// The endpoint a `gh api` command names, and whether it carries a write flag.
fn api_endpoint(args: &[String]) -> Option<(&str, bool)> {
    if args.first().map(String::as_str) != Some("api") {
        return None;
    }
    let mut writes = false;
    let mut endpoint = None;
    let mut rest = args[1..].iter();
    while let Some(arg) = rest.next() {
        let flag = arg.split_once('=').map_or(arg.as_str(), |(flag, _)| flag);
        if API_WRITE_FLAGS.contains(&flag) {
            writes = true;
        }
        if API_VALUE_FLAGS.contains(&arg.as_str()) {
            rest.next();
        } else if arg.starts_with('-') {
            // `--paginate`, `--slurp`, `--flag=value` and the like.
        } else if endpoint.is_none() {
            endpoint = Some(arg.as_str());
        }
    }
    endpoint.map(|endpoint| (endpoint, writes))
}

fn is_full_sha(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Whether one pass may reuse a successful answer to `args`. See the module
/// documentation for what qualifies and why.
#[must_use]
pub fn pass_cacheable(args: &[String]) -> bool {
    let Some((endpoint, writes)) = api_endpoint(args) else {
        return false;
    };
    if writes {
        return false;
    }
    let (path, query) = endpoint
        .trim_start_matches('/')
        .split_once('?')
        .unwrap_or((endpoint.trim_start_matches('/'), ""));
    let segments = path.split('/').collect::<Vec<_>>();
    let ["repos", owner, name, rest @ ..] = segments.as_slice() else {
        return false;
    };
    if owner.is_empty() || name.is_empty() {
        return false;
    }
    match rest {
        ["commits", sha] | ["git", "commits" | "trees" | "blobs", sha] => is_full_sha(sha),
        ["compare", range] => range
            .split_once("...")
            .is_some_and(|(base, head)| is_full_sha(base) && is_full_sha(head)),
        ["contents", path @ ..] => {
            !path.is_empty() && query.split('&').any(|pair| pair.starts_with("ref="))
        }
        ["rules", "branches", branch] | ["branches", branch, "protection", ..] => {
            !branch.is_empty()
        }
        _ => false,
    }
}

/// The budget and endpoint shape `args` spends, for counting.
#[must_use]
pub fn site(args: &[String]) -> (ApiKind, String) {
    let command = args.first().map_or("", String::as_str);
    if let Some((endpoint, _)) = api_endpoint(args) {
        if endpoint.trim_start_matches('/') == "graphql" {
            return (ApiKind::Graphql, graphql_site(args));
        }
        return (ApiKind::Rest, rest_site(endpoint));
    }
    let sub = args.get(1).map_or("", String::as_str);
    let kind = match command {
        // `gh pr`, `gh issue` and `gh repo` read and write through GraphQL.
        "pr" | "issue" | "repo" => ApiKind::Graphql,
        _ => ApiKind::Rest,
    };
    (kind, format!("gh {command} {sub}").trim_end().to_owned())
}

fn rest_site(endpoint: &str) -> String {
    let path = endpoint
        .trim_start_matches('/')
        .split('?')
        .next()
        .unwrap_or_default();
    let shape = path
        .split('/')
        .enumerate()
        .map(|(index, segment)| {
            if is_full_sha(segment) {
                "{sha}".to_owned()
            } else if segment.contains("...") {
                "{range}".to_owned()
            } else if !segment.is_empty() && segment.bytes().all(|byte| byte.is_ascii_digit()) {
                "{n}".to_owned()
            } else if index == 1 || index == 2 {
                // Owner and repository: one pass reads one repository.
                "*".to_owned()
            } else {
                segment.to_owned()
            }
        })
        .collect::<Vec<_>>();
    // Keep the shape short: the branch or file under a policy endpoint is
    // the same site whatever it names.
    let mut shape = shape;
    if let Some(position) = shape
        .iter()
        .position(|segment| segment == "contents" || segment == "branches")
    {
        shape.truncate(position + 1);
    }
    format!("api {}", shape.join("/"))
}

/// The first field a GraphQL query selects under its root (or under
/// `repository`), so different queries count separately.
fn graphql_site(args: &[String]) -> String {
    let query = args
        .iter()
        .find_map(|arg| arg.strip_prefix("query="))
        .unwrap_or_default();
    let operation = if query.trim_start().starts_with("mutation") {
        "mutation"
    } else {
        "query"
    };
    let body = query
        .find("repository(")
        .and_then(|start| query[start..].find('{').map(|open| start + open + 1))
        .or_else(|| query.find('{').map(|open| open + 1))
        .map_or("", |start| &query[start..]);
    let field = body
        .trim_start()
        .split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
        .next()
        .unwrap_or_default();
    format!("graphql {operation} {field}").trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| (*part).to_owned()).collect()
    }

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn content_addressed_and_policy_reads_are_cacheable() {
        for args in [
            argv(&["api", &format!("repos/o/r/commits/{SHA}")]),
            argv(&[
                "api",
                &format!("repos/o/r/commits/{SHA}"),
                "--jq",
                ".commit.committer.date",
            ]),
            argv(&["api", &format!("repos/o/r/git/blobs/{SHA}")]),
            argv(&["api", &format!("repos/o/r/compare/{SHA}...{SHA}")]),
            argv(&[
                "api",
                "-H",
                "Accept: application/vnd.github.raw+json",
                "repos/o/r/contents/.shipyard/config.toml?ref=main",
            ]),
            argv(&["api", "repos/o/r/rules/branches/main", "--paginate"]),
            argv(&[
                "api",
                "repos/o/r/branches/main/protection/required_status_checks",
            ]),
        ] {
            assert!(pass_cacheable(&args), "{args:?}");
        }
    }

    #[test]
    fn live_and_write_reads_are_never_cacheable() {
        for args in [
            // Anything a head's admission reads live.
            argv(&["api", "--paginate", "repos/o/r/pulls/7/reviews"]),
            argv(&["api", "--paginate", "repos/o/r/issues/7/comments"]),
            argv(&["api", &format!("repos/o/r/commits/{SHA}/check-runs?page=1")]),
            argv(&["api", &format!("repos/o/r/commits/{SHA}/statuses?page=1")]),
            argv(&["api", "repos/o/r/pulls/7"]),
            argv(&["api", "repos/o/r"]),
            argv(&["api", "graphql", "-f", "query=query{viewer{login}}"]),
            argv(&["pr", "view", "7", "--repo", "o/r"]),
            // A compare against a moving branch name.
            argv(&["api", &format!("repos/o/r/compare/main...{SHA}")]),
            // A commit by branch name.
            argv(&["api", "repos/o/r/commits/main"]),
            // A file at the default ref, which moves.
            argv(&["api", "repos/o/r/contents/README.md"]),
            // Writes, whatever the endpoint.
            argv(&["api", "-X", "DELETE", "repos/o/r/rules/branches/main"]),
            argv(&[
                "api",
                "--method=PATCH",
                &format!("repos/o/r/git/blobs/{SHA}"),
            ]),
            argv(&["api", "repos/o/r/contents/x?ref=main", "-f", "content=x"]),
        ] {
            assert!(!pass_cacheable(&args), "{args:?}");
        }
    }

    #[test]
    fn sites_elide_identity_and_split_budgets() {
        assert_eq!(
            site(&argv(&[
                "api",
                &format!("repos/o/r/commits/{SHA}/check-runs?page=2")
            ])),
            (
                ApiKind::Rest,
                "api repos/*/*/commits/{sha}/check-runs".to_owned()
            )
        );
        assert_eq!(
            site(&argv(&["api", "--paginate", "repos/o/r/pulls/12/reviews"])),
            (ApiKind::Rest, "api repos/*/*/pulls/{n}/reviews".to_owned())
        );
        assert_eq!(
            site(&argv(&[
                "api",
                "graphql",
                "-f",
                "query=query($o:String!){repository(owner:$o,name:$n){mergeQueue(branch:$b){x}}}"
            ])),
            (ApiKind::Graphql, "graphql query mergeQueue".to_owned())
        );
        assert_eq!(
            site(&argv(&["pr", "list", "--repo", "o/r"])),
            (ApiKind::Graphql, "gh pr list".to_owned())
        );
    }

    #[test]
    fn caching_reuses_only_successful_cacheable_answers() {
        let pass = GhPass::caching();
        let policy = argv(&["api", "repos/o/r/contents/.shipyard/config.toml?ref=main"]);
        let reviews = argv(&["api", "repos/o/r/pulls/7/reviews"]);
        assert_eq!(pass.cached(&policy), None);
        pass.remember(&policy, "[auto_merge]");
        pass.remember(&reviews, "[]");
        assert_eq!(pass.cached(&policy).as_deref(), Some("[auto_merge]"));
        assert_eq!(pass.cached(&reviews), None);
        assert_eq!(pass.report().cache_hits, 1);

        let counting = GhPass::counting();
        counting.remember(&policy, "[auto_merge]");
        assert_eq!(counting.cached(&policy), None);
    }

    #[test]
    fn report_totals_each_budget() {
        let pass = GhPass::counting();
        pass.record(
            &argv(&["api", "repos/o/r/pulls/1"]),
            Duration::from_millis(5),
        );
        pass.record(
            &argv(&["api", "repos/o/r/pulls/2"]),
            Duration::from_millis(5),
        );
        pass.record(&argv(&["pr", "list"]), Duration::from_millis(1));
        let report = pass.report();
        assert_eq!((report.rest, report.graphql, report.millis), (2, 1, 11));
        assert_eq!(report.sites[0].site, "api repos/*/*/pulls/{n}");
        assert_eq!(report.sites[0].calls, 2);
    }
}
