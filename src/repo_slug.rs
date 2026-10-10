//! Canonical identity of a repository the daemon watches.
//!
//! GitHub keeps serving a renamed or transferred repository at its old
//! `OWNER/REPO` for ordinary API reads (a 301 to `repositories/{id}`), but
//! not everywhere: a GitHub App installation lookup for the old name is a
//! plain 404. A daemon started with the old slug therefore never registers
//! its webhook and retries forever, while every read that follows redirects
//! looks healthy. Resolving the slug to the name GitHub now reports, before
//! the daemon starts watching, is what closes that gap.
//!
//! Resolution asks one or more probes in order. The first probe that finds
//! the repository decides; a repository no probe can see is `NotFound`; any
//! probe that could not answer at all makes the result `Unknown` rather than
//! a confident verdict.

use serde_json::Value;

/// What one probe learned about a slug.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SlugProbe {
    /// GitHub answered with this `full_name`.
    Found(String),
    /// GitHub answered 404.
    NotFound,
    /// The probe could not get an answer (network, credentials, parse).
    Unreadable(String),
}

/// The verdict for one watched slug.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SlugResolution {
    /// GitHub reports the same name (case-insensitively).
    Canonical,
    /// GitHub now reports the repository under another name.
    Renamed {
        /// The name GitHub reports now.
        to: String,
    },
    /// Every probe that answered said 404.
    NotFound,
    /// No probe found the repository and at least one could not answer.
    Unknown {
        /// Why the answer is missing.
        reason: String,
    },
}

/// Resolve `requested` by asking each probe in order.
pub fn resolve(
    requested: &str,
    probes: &mut [&mut dyn FnMut(&str) -> SlugProbe],
) -> SlugResolution {
    let mut unreadable = Vec::new();
    for probe in probes.iter_mut() {
        match probe(requested) {
            SlugProbe::Found(full_name) => {
                return if full_name.eq_ignore_ascii_case(requested) {
                    SlugResolution::Canonical
                } else {
                    SlugResolution::Renamed { to: full_name }
                };
            }
            SlugProbe::NotFound => {}
            SlugProbe::Unreadable(reason) => unreadable.push(reason),
        }
    }
    if unreadable.is_empty() {
        SlugResolution::NotFound
    } else {
        SlugResolution::Unknown {
            reason: unreadable.join("; "),
        }
    }
}

/// Arguments for an anonymous `curl` read of `repos/{slug}` that follows
/// GitHub's redirect and appends the final HTTP status on its own line.
#[must_use]
pub fn anonymous_probe_args(slug: &str) -> Vec<String> {
    [
        "-sS",
        "-L",
        "--max-time",
        "10",
        "-H",
        "Accept: application/vnd.github+json",
        "-w",
        "\n%{http_code}",
    ]
    .iter()
    .map(|arg| (*arg).to_owned())
    .chain(std::iter::once(format!(
        "https://api.github.com/repos/{slug}"
    )))
    .collect()
}

/// Interpret the output of [`anonymous_probe_args`]. A private repository
/// also reads as `NotFound` anonymously, which is why callers follow this
/// probe with an authenticated one.
#[must_use]
pub fn parse_anonymous_probe(stdout: &str) -> SlugProbe {
    let trimmed = stdout.trim_end();
    let Some((body, status)) = trimmed.rsplit_once('\n') else {
        return SlugProbe::Unreadable(format!("no HTTP status in curl output: {trimmed:?}"));
    };
    match status.trim() {
        "200" => full_name(body).map_or_else(
            || SlugProbe::Unreadable("HTTP 200 without a full_name".to_owned()),
            SlugProbe::Found,
        ),
        "404" => SlugProbe::NotFound,
        // GitHub's own message is what tells an exhausted anonymous rate limit
        // apart from any other refusal, so it travels with the status.
        other => SlugProbe::Unreadable(match message(body) {
            Some(message) => format!("HTTP {other}: {message}"),
            None => format!("HTTP {other}"),
        }),
    }
}

/// Ask GitHub anonymously which name `slug` has now. Bounded by curl's own
/// `--max-time`; a slug that is not a plain `OWNER/REPO` is never sent.
#[must_use]
pub fn anonymous_probe(slug: &str) -> SlugProbe {
    if crate::gh::validate_repo_slug(slug).is_err() {
        return SlugProbe::Unreadable(format!("`{slug}` is not an OWNER/REPO slug"));
    }
    // Unit tests never reach the network; resolution is proven through
    // injected probes instead.
    if cfg!(test) {
        return SlugProbe::Unreadable("network probes are disabled in unit tests".to_owned());
    }
    match std::process::Command::new("curl")
        .args(anonymous_probe_args(slug))
        .stdin(std::process::Stdio::null())
        .output()
    {
        Ok(output) => parse_anonymous_probe(&String::from_utf8_lossy(&output.stdout)),
        Err(error) => SlugProbe::Unreadable(format!("curl could not run: {error}")),
    }
}

/// `message` from a GitHub error body.
fn message(body: &str) -> Option<String> {
    serde_json::from_str::<Value>(body)
        .ok()?
        .get("message")?
        .as_str()
        .map(str::to_owned)
}

/// `full_name` from a `repos/{slug}` JSON body.
#[must_use]
pub fn full_name(body: &str) -> Option<String> {
    serde_json::from_str::<Value>(body)
        .ok()?
        .get("full_name")?
        .as_str()
        .map(str::to_owned)
}

/// Replace every renamed slug with the name GitHub reports, keeping the
/// original for any other verdict so an unreadable probe never drops a
/// repository. Returns the lowercased, deduplicated list and the renames made.
pub fn canonicalize(
    repos: Vec<String>,
    mut resolve_one: impl FnMut(&str) -> SlugResolution,
) -> (Vec<String>, Vec<(String, String)>) {
    let mut renames = Vec::new();
    let mut resolved: Vec<String> = repos
        .into_iter()
        .map(|repo| match resolve_one(&repo) {
            SlugResolution::Renamed { to } => {
                let to = to.to_ascii_lowercase();
                renames.push((repo, to.clone()));
                to
            }
            _ => repo.to_ascii_lowercase(),
        })
        .collect();
    resolved.sort();
    resolved.dedup();
    (resolved, renames)
}

/// The daemon's watch list after resolving every requested slug.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WatchList {
    /// Lowercased, deduplicated slugs the daemon watches.
    pub watched: Vec<String>,
    /// `(requested, now)` for each slug GitHub reports under another name.
    pub renames: Vec<(String, String)>,
    /// `(requested, why)` for each slug the daemon refuses to watch.
    pub refused: Vec<(String, String)>,
}

/// Resolve the slugs a daemon is asked to watch.
///
/// The anonymous probe runs first and is the only way to learn a public
/// repository's new name. The authenticated probe uses the credential the
/// daemon itself registers webhooks with, so its verdict decides what the
/// daemon can do: a name it reports is watched under that name, and an HTTP
/// 404 from it refuses the slug. GitHub serves a renamed or transferred
/// repository at its old name only to reads that follow redirects; an App
/// installation lookup for the old name is a plain 404, so a daemon watching
/// it retries registration forever. That holds whatever the anonymous probe
/// said, including a rate-limited non-answer. A slug the authenticated probe
/// could not answer for is kept: an offline probe must never drop a
/// repository from the watch list.
pub fn watch_list(
    repos: Vec<String>,
    anonymous: &mut dyn FnMut(&str) -> SlugProbe,
    authenticated: &mut dyn FnMut(&str) -> SlugProbe,
) -> WatchList {
    let mut list = WatchList::default();
    for repo in repos {
        let renamed = |list: &mut WatchList, to: String| {
            let to = to.to_ascii_lowercase();
            list.renames.push((repo.clone(), to.clone()));
            list.watched.push(to);
        };
        if let SlugProbe::Found(name) = anonymous(&repo) {
            if name.eq_ignore_ascii_case(&repo) {
                list.watched.push(repo.to_ascii_lowercase());
            } else {
                renamed(&mut list, name);
            }
            continue;
        }
        match authenticated(&repo) {
            SlugProbe::Found(name) if !name.eq_ignore_ascii_case(&repo) => renamed(&mut list, name),
            SlugProbe::NotFound => list.refused.push((
                repo.clone(),
                format!(
                    "GitHub answers HTTP 404 for {repo} to this host's credential, so the daemon \
                     can never register its webhook; if the repository was renamed or \
                     transferred, configure its current name"
                ),
            )),
            _ => list.watched.push(repo.to_ascii_lowercase()),
        }
    }
    list.watched.sort();
    list.watched.dedup();
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(name: &str) -> impl FnMut(&str) -> SlugProbe + '_ {
        move |_| SlugProbe::Found(name.to_owned())
    }

    #[test]
    fn a_renamed_repository_resolves_to_the_name_github_reports() {
        let mut anon = found("Generous-Corp/pulp");
        assert_eq!(
            resolve("danielraffel/pulp", &mut [&mut anon]),
            SlugResolution::Renamed {
                to: "Generous-Corp/pulp".to_owned()
            }
        );
    }

    #[test]
    fn the_same_name_in_another_case_is_canonical() {
        let mut anon = found("Generous-Corp/pulp");
        assert_eq!(
            resolve("generous-corp/pulp", &mut [&mut anon]),
            SlugResolution::Canonical
        );
    }

    #[test]
    fn a_later_probe_answers_when_an_earlier_one_cannot_see_the_repository() {
        let mut anon = |_: &str| SlugProbe::NotFound;
        let mut auth = found("owner/private");
        assert_eq!(
            resolve("owner/private", &mut [&mut anon, &mut auth]),
            SlugResolution::Canonical
        );
    }

    #[test]
    fn not_found_needs_every_probe_to_say_404() {
        let mut anon = |_: &str| SlugProbe::NotFound;
        let mut auth = |_: &str| SlugProbe::NotFound;
        assert_eq!(
            resolve("owner/gone", &mut [&mut anon, &mut auth]),
            SlugResolution::NotFound
        );
        let mut anon = |_: &str| SlugProbe::NotFound;
        let mut offline = |_: &str| SlugProbe::Unreadable("timed out".to_owned());
        assert_eq!(
            resolve("owner/gone", &mut [&mut anon, &mut offline]),
            SlugResolution::Unknown {
                reason: "timed out".to_owned()
            }
        );
    }

    #[test]
    fn anonymous_output_is_parsed_by_its_final_status_line() {
        assert_eq!(
            parse_anonymous_probe("{\"full_name\":\"Generous-Corp/pulp\"}\n200"),
            SlugProbe::Found("Generous-Corp/pulp".to_owned())
        );
        assert_eq!(
            parse_anonymous_probe("{\"message\":\"Not Found\"}\n404\n"),
            SlugProbe::NotFound
        );
        assert!(matches!(
            parse_anonymous_probe("{\"message\":\"rate limited\"}\n403"),
            SlugProbe::Unreadable(reason) if reason == "HTTP 403: rate limited"
        ));
        assert!(matches!(
            parse_anonymous_probe("<html>bad gateway</html>\n502"),
            SlugProbe::Unreadable(reason) if reason == "HTTP 502"
        ));
        assert!(matches!(
            parse_anonymous_probe(""),
            SlugProbe::Unreadable(_)
        ));
        let args = anonymous_probe_args("o/r");
        assert!(
            args.contains(&"-L".to_owned()),
            "redirects must be followed"
        );
        assert_eq!(
            args.last().map(String::as_str),
            Some("https://api.github.com/repos/o/r")
        );
    }

    #[test]
    fn canonicalize_replaces_only_renamed_slugs_and_dedups() {
        let (repos, renames) = canonicalize(
            vec![
                "danielraffel/pulp".to_owned(),
                "generous-corp/pulp".to_owned(),
                "owner/offline".to_owned(),
            ],
            |repo| match repo {
                "danielraffel/pulp" => SlugResolution::Renamed {
                    to: "Generous-Corp/pulp".to_owned(),
                },
                "owner/offline" => SlugResolution::Unknown {
                    reason: "timed out".to_owned(),
                },
                _ => SlugResolution::Canonical,
            },
        );
        assert_eq!(repos, ["generous-corp/pulp", "owner/offline"]);
        assert_eq!(
            renames,
            [(
                "danielraffel/pulp".to_owned(),
                "generous-corp/pulp".to_owned()
            )]
        );
    }

    #[test]
    fn the_watch_list_refuses_a_slug_the_daemon_credential_cannot_see() {
        // m1, 10-09: an anonymous read was rate-limited and the App token
        // helper answered HTTP 404 for the pre-transfer name.
        let list = watch_list(
            vec![
                "danielraffel/pulp".to_owned(),
                "owner/public-renamed".to_owned(),
                "owner/private".to_owned(),
                "owner/offline".to_owned(),
                "Owner/Same".to_owned(),
            ],
            &mut |repo| match repo {
                "owner/public-renamed" => SlugProbe::Found("owner/public-new".to_owned()),
                "Owner/Same" => SlugProbe::Found("owner/same".to_owned()),
                "owner/private" => SlugProbe::NotFound,
                _ => SlugProbe::Unreadable("HTTP 403: API rate limit exceeded".to_owned()),
            },
            &mut |repo| match repo {
                "danielraffel/pulp" => SlugProbe::NotFound,
                "owner/private" => SlugProbe::Found("Owner/Private-New".to_owned()),
                "owner/offline" => SlugProbe::Unreadable("timed out".to_owned()),
                other => panic!("authenticated probe asked about {other}"),
            },
        );
        assert_eq!(
            list.watched,
            [
                "owner/offline",
                "owner/private-new",
                "owner/public-new",
                "owner/same"
            ]
        );
        assert_eq!(
            list.renames,
            [
                (
                    "owner/public-renamed".to_owned(),
                    "owner/public-new".to_owned()
                ),
                ("owner/private".to_owned(), "owner/private-new".to_owned()),
            ]
        );
        assert_eq!(list.refused.len(), 1);
        assert_eq!(list.refused[0].0, "danielraffel/pulp");
        assert!(list.refused[0].1.contains("HTTP 404"), "{:?}", list.refused);
    }

    #[test]
    fn an_unanswered_authenticated_probe_keeps_the_slug() {
        // Negative control: nothing answered, so nothing is refused.
        let list = watch_list(
            vec!["owner/repo".to_owned()],
            &mut |_| SlugProbe::Unreadable("offline".to_owned()),
            &mut |_| SlugProbe::Unreadable("offline".to_owned()),
        );
        assert_eq!(list.watched, ["owner/repo"]);
        assert!(list.refused.is_empty() && list.renames.is_empty());
    }
}
