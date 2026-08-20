//! Typed model of a CI routing profile.
//!
//! A routing profile declares, per repository and per context (`pr`,
//! `merge_group`, `release`, `coverage`, `scheduled`), an ordered fallback
//! chain of runner targets for each lane (`macos`, `linux`, `windows`, ...).
//! Shipyard reads it to explain a route, to publish a health lease, and to
//! write the GitHub `runs-on` variables that decide where a job lands.
//!
//! The model is deliberately strict: every table denies unknown fields, so a
//! misspelled key is a load-time error rather than a silently ignored setting
//! that leaves a lane routed somewhere nobody intended.

use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};

use serde::{Deserialize, Serialize};
use toml::Value;

/// A parsed CI routing profile.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CiProfile {
    /// Profile name. Must match the file stem callers ask for.
    pub name: String,
    /// Human-readable summary of the routing policy.
    #[serde(default)]
    pub description: String,
    /// Per-repository routing, keyed by `owner/name`.
    #[serde(default)]
    pub repo: BTreeMap<String, RepoRouting>,
    /// Target definitions referenced by lane fallback chains.
    #[serde(default)]
    pub targets: BTreeMap<String, ProfileTarget>,
}

/// Routing for one repository: context name to lane map.
pub type RepoRouting = BTreeMap<String, ContextRouting>;

/// Routing for one context: lane name to lane declaration.
pub type ContextRouting = BTreeMap<String, Lane>;

/// One lane's ordered fallback chain plus its publication policy.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Lane {
    /// How the chain is consumed. `ordered-fallback` walks it in order;
    /// `github-only` pins the lane to GitHub-hosted runners.
    #[serde(default = "default_strategy")]
    pub strategy: String,
    /// Ordered target ids. The first usable entry wins.
    #[serde(default)]
    pub targets: Vec<String>,
    /// GitHub repository variable that carries this lane's `runs-on` JSON.
    #[serde(default)]
    pub github_variable: Option<String>,
    /// Whether a failure should open or update a deduplicated issue.
    #[serde(default)]
    pub issue_on_failure: bool,
    /// Whether every self-managed target in the chain must be ephemeral.
    #[serde(default)]
    pub ephemeral_required: bool,
    /// Scheduled lanes only: whether the schedule is live.
    #[serde(default)]
    pub enabled: bool,
    /// Scheduled lanes only: branch the schedule runs against.
    #[serde(default)]
    pub branch: Option<String>,
    /// Scheduled lanes only: workflow file the schedule dispatches.
    #[serde(default)]
    pub workflow: Option<String>,
    /// Health-lease declaration for this lane, if it publishes one.
    #[serde(flatten)]
    pub health_lease: HealthLeaseFields,
}

/// Raw `health_lease_*` keys as they appear on a lane.
///
/// Flattened into [`Lane`] so the TOML stays flat, and kept separate here so
/// [`HealthLease::from_fields`] can validate the group as a unit.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HealthLeaseFields {
    /// GitHub variable the lease publishes its expiry into.
    #[serde(default)]
    pub health_lease_variable: Option<String>,
    /// How long a published lease stays valid.
    #[serde(default)]
    pub health_lease_ttl_seconds: Option<u64>,
    /// Workflow events this lease authorizes.
    #[serde(default)]
    pub health_lease_events: Option<Vec<String>>,
    /// Name prefix every eligible runner must carry.
    #[serde(default)]
    pub health_lease_runner_name_prefix: Option<String>,
    /// Branch whose merge-queue concurrency bounds the admission burst.
    #[serde(default)]
    pub health_lease_merge_queue_branch: Option<String>,
    /// How many runners must be simultaneously admissible.
    #[serde(default)]
    pub health_lease_admission_burst: Option<usize>,
    /// Minimum idle runners required before a lease may renew.
    #[serde(default)]
    pub health_lease_min_idle: Option<usize>,
    /// Capability label an eligible runner must advertise.
    #[serde(default)]
    pub health_lease_required_capability: Option<String>,
    /// Capability label that disqualifies a runner, keeping trusted and
    /// PR-safe pools from bleeding into each other.
    #[serde(default)]
    pub health_lease_forbidden_capability: Option<String>,
}

/// A validated health-lease declaration.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct HealthLease {
    /// GitHub variable the lease publishes its expiry into.
    pub variable: String,
    /// How long a published lease stays valid.
    pub ttl_seconds: u64,
    /// Workflow events this lease authorizes.
    pub events: Vec<String>,
    /// Name prefix every eligible runner must carry.
    pub runner_name_prefix: String,
    /// Branch whose merge-queue concurrency bounds the admission burst.
    pub merge_queue_branch: String,
    /// How many runners must be simultaneously admissible.
    pub admission_burst: usize,
    /// Minimum idle runners required before a lease may renew.
    pub min_idle: usize,
    /// Capability label an eligible runner must advertise.
    pub required_capability: Option<String>,
    /// Capability label that disqualifies a runner.
    pub forbidden_capability: Option<String>,
}

/// One runner target a lane can route to.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileTarget {
    /// The `runs-on` selector: a bare label string or an array of labels.
    #[serde(default)]
    pub runs_on_json: Option<Value>,
    /// Who operates the runner (`github`, `namespace`, or a local host).
    #[serde(default)]
    pub provider: Option<String>,
    /// Host machine that backs the runner.
    #[serde(default)]
    pub host: Option<String>,
    /// Operating system family.
    #[serde(default)]
    pub os: Option<String>,
    /// CPU architecture.
    #[serde(default)]
    pub arch: Option<String>,
    /// Free-form role tag used to explain a target's purpose.
    #[serde(default)]
    pub role: Option<String>,
    /// Whether the runner is torn down after each job.
    #[serde(default)]
    pub ephemeral: bool,
    /// Whether a real job has been observed to dispatch here.
    ///
    /// The apply gate refuses to write a variable for an unproven target: a
    /// job routed at a lane with no runners queues forever and GitHub reports
    /// no error, so an unproven route is a silent black hole.
    #[serde(default)]
    pub proven: bool,
    /// Platforms this target is the authoritative validator for.
    #[serde(default)]
    pub authoritative_for: Vec<String>,
    /// Job-name pattern the proof gate looks for in dispatch evidence.
    #[serde(default)]
    pub evidence_job_pattern: Option<String>,
    /// Runner group that must grant this repository workflow access.
    #[serde(default)]
    pub runner_group: Option<String>,
}

fn default_strategy() -> String {
    "ordered-fallback".to_owned()
}

/// Errors raised while loading or validating a routing profile.
#[derive(Debug, PartialEq)]
pub enum ProfileError {
    /// The TOML did not match the schema (including unknown keys).
    Parse(String),
    /// The profile's `name` disagreed with the name the caller asked for.
    NameMismatch {
        /// Name the caller requested.
        expected: String,
        /// Name the file declared.
        found: String,
    },
    /// The profile declares no routing for the requested repository.
    UnknownRepo(String),
    /// The repository declares no such context.
    UnknownContext {
        /// Repository slug.
        repo: String,
        /// Context the caller asked for.
        context: String,
    },
    /// The context declares no such lane.
    UnknownLane {
        /// Repository slug.
        repo: String,
        /// Context that was found.
        context: String,
        /// Lane the caller asked for.
        lane: String,
    },
    /// A health-lease declaration was incomplete or out of range.
    HealthLease(String),
}

impl Display for ProfileError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse(message) | Self::HealthLease(message) => write!(f, "{message}"),
            Self::NameMismatch { expected, found } => write!(
                f,
                "profile declares name = {found:?} but was loaded as {expected:?}"
            ),
            Self::UnknownRepo(repo) => write!(f, "profile has no repo entry for {repo}"),
            Self::UnknownContext { repo, context } => {
                write!(f, "profile has no context repo.{repo}.{context}")
            }
            Self::UnknownLane {
                repo,
                context,
                lane,
            } => write!(f, "profile has no lane repo.{repo}.{context}.{lane}"),
        }
    }
}

impl std::error::Error for ProfileError {}

impl CiProfile {
    /// Parse a routing profile from TOML text, rejecting unknown keys.
    ///
    /// # Errors
    ///
    /// Returns [`ProfileError::Parse`] when the text is not valid TOML or
    /// contains a key the schema does not define.
    pub fn parse(text: &str) -> Result<Self, ProfileError> {
        toml::from_str(text).map_err(|error| ProfileError::Parse(error.to_string()))
    }

    /// Parse a profile and require it to declare the expected name.
    ///
    /// # Errors
    ///
    /// Returns [`ProfileError::Parse`] on a schema violation, or
    /// [`ProfileError::NameMismatch`] when the declared name disagrees.
    pub fn parse_named(text: &str, expected: &str) -> Result<Self, ProfileError> {
        let profile = Self::parse(text)?;
        if profile.name != expected {
            return Err(ProfileError::NameMismatch {
                expected: expected.to_owned(),
                found: profile.name.clone(),
            });
        }
        Ok(profile)
    }

    /// Look up the routing declared for one repository.
    ///
    /// # Errors
    ///
    /// Returns [`ProfileError::UnknownRepo`] when the profile has no entry.
    pub fn routing(&self, repo: &str) -> Result<&RepoRouting, ProfileError> {
        self.repo
            .get(repo)
            .ok_or_else(|| ProfileError::UnknownRepo(repo.to_owned()))
    }

    /// Look up one context's lanes for a repository.
    ///
    /// # Errors
    ///
    /// Returns an error when either the repository or the context is absent.
    pub fn context(&self, repo: &str, context: &str) -> Result<&ContextRouting, ProfileError> {
        self.routing(repo)?
            .get(context)
            .ok_or_else(|| ProfileError::UnknownContext {
                repo: repo.to_owned(),
                context: context.to_owned(),
            })
    }

    /// Look up a single lane.
    ///
    /// # Errors
    ///
    /// Returns an error when the repository, context, or lane is absent.
    pub fn lane(&self, repo: &str, context: &str, lane: &str) -> Result<&Lane, ProfileError> {
        self.context(repo, context)?
            .get(lane)
            .ok_or_else(|| ProfileError::UnknownLane {
                repo: repo.to_owned(),
                context: context.to_owned(),
                lane: lane.to_owned(),
            })
    }

    /// Every `(context, lane)` pair declared for a repository, in a stable order.
    #[must_use]
    pub fn lane_specs(&self, repo: &str) -> Vec<(String, String, &Lane)> {
        self.repo
            .get(repo)
            .map(|routing| {
                routing
                    .iter()
                    .flat_map(|(context, lanes)| {
                        lanes
                            .iter()
                            .map(move |(lane, body)| (context.clone(), lane.clone(), body))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Resolve a target id to its declaration.
    #[must_use]
    pub fn target(&self, id: &str) -> Option<&ProfileTarget> {
        self.targets.get(id)
    }
}

impl HealthLease {
    /// Validate a lane's `health_lease_*` keys into a usable declaration.
    ///
    /// Returns `Ok(None)` when the lane declares no lease at all. A partial
    /// declaration is an error rather than a silent skip, because a lane that
    /// looks leased but publishes nothing is exactly the failure the lease
    /// exists to prevent.
    ///
    /// # Errors
    ///
    /// Returns [`ProfileError::HealthLease`] when the group is incomplete or a
    /// value is out of range.
    pub fn from_fields(fields: &HealthLeaseFields) -> Result<Option<Self>, ProfileError> {
        let declared = [
            fields.health_lease_variable.is_some(),
            fields.health_lease_ttl_seconds.is_some(),
            fields.health_lease_events.is_some(),
            fields.health_lease_runner_name_prefix.is_some(),
            fields.health_lease_merge_queue_branch.is_some(),
            fields.health_lease_admission_burst.is_some(),
        ];
        if declared.iter().all(|present| !present) {
            return Ok(None);
        }
        if !declared.iter().all(|present| *present) {
            return Err(ProfileError::HealthLease(
                "a lane that declares any health_lease_* key must declare health_lease_variable, \
                 health_lease_ttl_seconds, health_lease_events, health_lease_runner_name_prefix, \
                 health_lease_merge_queue_branch, and health_lease_admission_burst"
                    .to_owned(),
            ));
        }

        let ttl_seconds = fields.health_lease_ttl_seconds.expect("checked present");
        if !(60..=900).contains(&ttl_seconds) {
            return Err(ProfileError::HealthLease(
                "health_lease_ttl_seconds must be between 60 and 900".to_owned(),
            ));
        }
        let admission_burst = fields
            .health_lease_admission_burst
            .expect("checked present");
        if admission_burst == 0 {
            return Err(ProfileError::HealthLease(
                "health_lease_admission_burst must be a positive integer".to_owned(),
            ));
        }
        let events = fields.health_lease_events.clone().expect("checked present");
        if events.is_empty() {
            return Err(ProfileError::HealthLease(
                "health_lease_events must name at least one workflow event".to_owned(),
            ));
        }
        let variable = fields
            .health_lease_variable
            .clone()
            .expect("checked present");
        if variable.trim().is_empty() {
            return Err(ProfileError::HealthLease(
                "health_lease_variable must not be empty".to_owned(),
            ));
        }

        Ok(Some(Self {
            variable,
            ttl_seconds,
            events,
            runner_name_prefix: fields
                .health_lease_runner_name_prefix
                .clone()
                .expect("checked present"),
            merge_queue_branch: fields
                .health_lease_merge_queue_branch
                .clone()
                .expect("checked present"),
            admission_burst,
            min_idle: fields.health_lease_min_idle.unwrap_or(admission_burst),
            required_capability: fields.health_lease_required_capability.clone(),
            forbidden_capability: fields.health_lease_forbidden_capability.clone(),
        }))
    }
}

impl ProfileTarget {
    /// Whether this target is served by GitHub-hosted runners.
    #[must_use]
    pub fn is_github(&self, id: &str) -> bool {
        self.provider.as_deref() == Some("github") || id.starts_with("github.")
    }

    /// The `runs-on` selector rendered as compact JSON.
    #[must_use]
    pub fn runs_on_compact_json(&self) -> Option<String> {
        let value = self.runs_on_json.as_ref()?;
        let json = serde_json::to_value(value).ok()?;
        serde_json::to_string(&json).ok()
    }

    /// Labels a runner must carry to satisfy this target's selector.
    ///
    /// A bare string selector (a GitHub-hosted image name) has no label set.
    #[must_use]
    pub fn required_labels(&self) -> Option<Vec<String>> {
        match self.runs_on_json.as_ref()? {
            Value::Array(values) => values
                .iter()
                .map(|value| value.as_str().map(str::to_owned))
                .collect(),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CiProfile, HealthLease, ProfileError};

    const PULP_LIKE: &str = r#"
name = "normal-local-fast"
description = "Fast PRs on local ARM64 VMs."

[repo."Generous-Corp/pulp".pr.linux]
strategy = "ordered-fallback"
targets = ["macstudio.linux-arm64-vm", "github.linux-x64"]
github_variable = "PULP_LOCAL_LINUX_RUNS_ON_JSON"
health_lease_variable = "PULP_PR_SAFE_LINUX_LEASE_UNTIL"
health_lease_ttl_seconds = 300
health_lease_events = ["pull_request"]
health_lease_runner_name_prefix = "pulp-pr-safe-ephemeral-"
health_lease_merge_queue_branch = "main"
health_lease_admission_burst = 2

[targets."macstudio.linux-arm64-vm"]
runs_on_json = ["self-hosted", "Linux", "ARM64", "pulp-pr-safe-linux-x64"]
proven = true
ephemeral = true

[targets."github.linux-x64"]
runs_on_json = "ubuntu-latest"
authoritative_for = ["linux-x64"]
"#;

    #[test]
    fn parses_a_full_profile_and_exposes_typed_lanes() {
        let profile = CiProfile::parse_named(PULP_LIKE, "normal-local-fast").expect("profile");

        assert_eq!(profile.name, "normal-local-fast");
        let lane = profile
            .lane("Generous-Corp/pulp", "pr", "linux")
            .expect("lane");
        assert_eq!(lane.strategy, "ordered-fallback");
        assert_eq!(
            lane.targets,
            vec![
                "macstudio.linux-arm64-vm".to_owned(),
                "github.linux-x64".to_owned()
            ]
        );
        assert_eq!(
            lane.github_variable.as_deref(),
            Some("PULP_LOCAL_LINUX_RUNS_ON_JSON")
        );

        let target = profile.target("macstudio.linux-arm64-vm").expect("target");
        assert!(target.proven);
        assert!(target.ephemeral);
        assert_eq!(
            target.required_labels(),
            Some(vec![
                "self-hosted".to_owned(),
                "Linux".to_owned(),
                "ARM64".to_owned(),
                "pulp-pr-safe-linux-x64".to_owned(),
            ])
        );
    }

    #[test]
    fn a_misspelled_lane_key_is_a_load_error_not_a_silent_no_op() {
        // `github_var` instead of `github_variable`. Untyped loading accepted
        // this and left the lane publishing nothing.
        let text = r#"
name = "typo"

[repo."owner/repo".pr.linux]
targets = ["github.linux-x64"]
github_var = "SOME_VARIABLE"

[targets."github.linux-x64"]
runs_on_json = "ubuntu-latest"
"#;

        let error = CiProfile::parse(text).expect_err("typo must not load");

        let ProfileError::Parse(message) = error else {
            panic!("expected a parse error, got {error:?}");
        };
        assert!(
            message.contains("github_var"),
            "error should name the offending key, got: {message}"
        );
    }

    #[test]
    fn a_misspelled_target_key_is_a_load_error() {
        let text = r#"
name = "typo"

[repo."owner/repo".pr.linux]
targets = ["local.linux"]

[targets."local.linux"]
runs_on_json = ["self-hosted", "Linux"]
prooven = true
"#;

        let error = CiProfile::parse(text).expect_err("typo must not load");

        let ProfileError::Parse(message) = error else {
            panic!("expected a parse error, got {error:?}");
        };
        assert!(
            message.contains("prooven"),
            "error should name the offending key, got: {message}"
        );
    }

    #[test]
    fn control_the_same_profile_without_the_typo_loads_cleanly() {
        // Negative control for the two typo tests: proves the rejection is the
        // unknown key, not something else wrong with the fixture shape.
        let text = r#"
name = "typo"

[repo."owner/repo".pr.linux]
targets = ["local.linux"]

[targets."local.linux"]
runs_on_json = ["self-hosted", "Linux"]
proven = true
"#;

        let profile = CiProfile::parse(text).expect("clean profile loads");

        assert!(profile.target("local.linux").expect("target").proven);
    }

    #[test]
    fn name_mismatch_is_reported_against_the_requested_name() {
        let error = CiProfile::parse_named(PULP_LIKE, "some-other-name")
            .expect_err("name mismatch must fail");

        assert_eq!(
            error,
            ProfileError::NameMismatch {
                expected: "some-other-name".to_owned(),
                found: "normal-local-fast".to_owned(),
            }
        );
    }

    #[test]
    fn health_lease_validates_as_a_group() {
        let profile = CiProfile::parse_named(PULP_LIKE, "normal-local-fast").expect("profile");
        let lane = profile
            .lane("Generous-Corp/pulp", "pr", "linux")
            .expect("lane");

        let lease = HealthLease::from_fields(&lane.health_lease)
            .expect("lease validates")
            .expect("lease present");

        assert_eq!(lease.variable, "PULP_PR_SAFE_LINUX_LEASE_UNTIL");
        assert_eq!(lease.ttl_seconds, 300);
        assert_eq!(lease.admission_burst, 2);
        // min_idle defaults to the admission burst rather than to zero, so an
        // undeclared floor cannot authorize a lease over a busy fleet.
        assert_eq!(lease.min_idle, 2);
    }

    #[test]
    fn a_lane_with_no_lease_keys_simply_has_no_lease() {
        let text = r#"
name = "no-lease"

[repo."owner/repo".pr.windows]
strategy = "github-only"
targets = ["github.windows-x64"]

[targets."github.windows-x64"]
runs_on_json = "windows-latest"
"#;
        let profile = CiProfile::parse(text).expect("profile");
        let lane = profile.lane("owner/repo", "pr", "windows").expect("lane");

        assert_eq!(
            HealthLease::from_fields(&lane.health_lease).expect("no error"),
            None
        );
    }

    #[test]
    fn a_partial_health_lease_is_an_error_not_a_skip() {
        let text = r#"
name = "partial"

[repo."owner/repo".pr.linux]
targets = ["github.linux-x64"]
health_lease_variable = "SOME_LEASE_UNTIL"
health_lease_ttl_seconds = 300

[targets."github.linux-x64"]
runs_on_json = "ubuntu-latest"
"#;
        let profile = CiProfile::parse(text).expect("profile");
        let lane = profile.lane("owner/repo", "pr", "linux").expect("lane");

        let error = HealthLease::from_fields(&lane.health_lease).expect_err("partial must fail");

        assert!(matches!(error, ProfileError::HealthLease(_)));
    }

    #[test]
    fn out_of_range_ttl_is_rejected() {
        let text = r#"
name = "long-ttl"

[repo."owner/repo".pr.linux]
targets = ["github.linux-x64"]
health_lease_variable = "SOME_LEASE_UNTIL"
health_lease_ttl_seconds = 5400
health_lease_events = ["pull_request"]
health_lease_runner_name_prefix = "x-"
health_lease_merge_queue_branch = "main"
health_lease_admission_burst = 1

[targets."github.linux-x64"]
runs_on_json = "ubuntu-latest"
"#;
        let profile = CiProfile::parse(text).expect("profile");
        let lane = profile.lane("owner/repo", "pr", "linux").expect("lane");

        let error = HealthLease::from_fields(&lane.health_lease).expect_err("ttl must be bounded");

        assert_eq!(
            error,
            ProfileError::HealthLease(
                "health_lease_ttl_seconds must be between 60 and 900".to_owned()
            )
        );
    }

    #[test]
    fn lane_lookup_errors_name_the_missing_level() {
        let profile = CiProfile::parse_named(PULP_LIKE, "normal-local-fast").expect("profile");

        assert_eq!(
            profile.routing("nobody/nothing").expect_err("no repo"),
            ProfileError::UnknownRepo("nobody/nothing".to_owned())
        );
        assert_eq!(
            profile
                .context("Generous-Corp/pulp", "merge_group")
                .expect_err("no context"),
            ProfileError::UnknownContext {
                repo: "Generous-Corp/pulp".to_owned(),
                context: "merge_group".to_owned(),
            }
        );
        assert_eq!(
            profile
                .lane("Generous-Corp/pulp", "pr", "macos")
                .expect_err("no lane"),
            ProfileError::UnknownLane {
                repo: "Generous-Corp/pulp".to_owned(),
                context: "pr".to_owned(),
                lane: "macos".to_owned(),
            }
        );
    }

    #[test]
    fn lane_specs_enumerate_every_context_and_lane() {
        let text = r#"
name = "multi"

[repo."owner/repo".pr.linux]
targets = ["github.linux-x64"]

[repo."owner/repo".pr.macos]
targets = ["github.macos-arm64"]

[repo."owner/repo".merge_group.linux]
targets = ["github.linux-x64"]

[targets."github.linux-x64"]
runs_on_json = "ubuntu-latest"

[targets."github.macos-arm64"]
runs_on_json = "macos-15"
"#;
        let profile = CiProfile::parse(text).expect("profile");

        let specs = profile
            .lane_specs("owner/repo")
            .into_iter()
            .map(|(context, lane, _)| format!("{context}.{lane}"))
            .collect::<Vec<_>>();

        assert_eq!(
            specs,
            vec![
                "merge_group.linux".to_owned(),
                "pr.linux".to_owned(),
                "pr.macos".to_owned(),
            ]
        );
    }

    #[test]
    fn a_bare_string_selector_has_no_label_set() {
        let profile = CiProfile::parse_named(PULP_LIKE, "normal-local-fast").expect("profile");
        let hosted = profile.target("github.linux-x64").expect("target");

        assert_eq!(hosted.required_labels(), None);
        assert_eq!(
            hosted.runs_on_compact_json().as_deref(),
            Some("\"ubuntu-latest\"")
        );
        assert!(hosted.is_github("github.linux-x64"));
    }
}
