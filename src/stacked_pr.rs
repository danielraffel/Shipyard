//! Read-only discovery and fail-closed policy for GitHub stacked pull requests.

use std::path::Path;

use serde::Serialize;
use serde_json::Value;

use crate::config::LoadedConfig;
use crate::gh::{GhAuthPolicy, GhClient, GhSupervision};

/// Per-repository rollout mode for formal GitHub stacked pull requests.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum StackedPrMode {
    /// Detect stacks and retain Shipyard's existing fail-closed behavior.
    #[default]
    Off,
    /// Emit an exact-head immutable plan, but perform no GitHub mutation.
    Observe,
    /// Reserved for a future durable asynchronous stack-merge lifecycle.
    Apply,
}

impl StackedPrMode {
    fn parse(value: &toml::Value, source: &str) -> Result<Self, String> {
        let value = value
            .as_str()
            .ok_or_else(|| format!("{source} stacked_pr_mode must be a string"))?;
        match value {
            "off" => Ok(Self::Off),
            "observe" => Ok(Self::Observe),
            "apply" => Ok(Self::Apply),
            _ => Err(format!(
                "{source} stacked_pr_mode must be one of off, observe, or apply; got {value:?}"
            )),
        }
    }
}

/// Formal GitHub stack membership for one pull request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StackInfo {
    /// Repository-scoped stack number.
    pub number: u64,
    /// Number of pull requests in the stack.
    pub size: u64,
    /// One-based position, where one is closest to the stack base.
    pub position: u64,
    /// Branch targeted by the complete stack.
    pub base_branch: String,
}

/// One bounded stack/config observation returned by GitHub.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StackInspection {
    /// Exact PR head observed in the same GraphQL response as stack metadata.
    pub(crate) head_sha: String,
    /// Effective per-repository mode after the trusted global off override.
    pub(crate) mode: StackedPrMode,
    /// Formal stack membership, when present.
    pub(crate) stack: Option<StackInfo>,
}

/// Classified stack-inspection failure used to constrain classic REST fallback.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StackInspectionError {
    message: String,
    graphql_rate_limited: bool,
}

impl StackInspectionError {
    pub(crate) fn query(message: String) -> Self {
        let graphql_rate_limited = crate::gh::is_graphql_rate_limited(&message);
        Self {
            message,
            graphql_rate_limited,
        }
    }

    fn response(message: String) -> Self {
        let graphql_rate_limited = message
            .starts_with("pull request stack query returned GraphQL errors:")
            && crate::gh::is_graphql_rate_limited(&message);
        Self {
            message,
            graphql_rate_limited,
        }
    }

    fn after_membership(stack: Option<&StackInfo>, message: String, response_error: bool) -> Self {
        if let Some(stack) = stack {
            return Self::validation(format!(
                "formal GitHub stack #{} was discovered before policy inspection; {message}",
                stack.number
            ));
        }
        if response_error {
            Self::response(message)
        } else {
            Self::query(message)
        }
    }

    pub(crate) fn validation(message: String) -> Self {
        Self {
            message,
            graphql_rate_limited: false,
        }
    }

    #[must_use]
    pub(crate) fn is_graphql_rate_limited(&self) -> bool {
        self.graphql_rate_limited
    }

    #[must_use]
    pub(crate) fn into_message(self) -> String {
        self.message
    }
}

/// Deterministic, exact-head telemetry for an observed formal stack.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct StackPlan {
    schema_version: u32,
    kind: &'static str,
    mode: StackedPrMode,
    disposition: &'static str,
    identity: String,
    repository: String,
    pull_request: u64,
    head_sha: String,
    stack_number: u64,
    stack_size: u64,
    stack_position: u64,
    stack_base_branch: String,
    github_mutation: bool,
    required_checks_suppressed: bool,
}

/// Query formal stack membership using GitHub's public-preview GraphQL fields.
///
/// This public compatibility surface intentionally retains its original
/// signature and result. Shipyard's mutation paths use the stricter internal
/// inspection below so adding rollout policy does not break library callers.
pub fn fetch(
    client: &GhClient,
    cwd: &Path,
    repo: &str,
    pr: u64,
) -> Result<Option<StackInfo>, String> {
    let args = membership_query_args(repo, pr)?;
    let raw = run_query(client, cwd, args)?;
    parse_membership_json(&raw)
}

/// Query stack membership and protected-base rollout policy together.
pub(crate) fn fetch_inspection(
    client: &GhClient,
    cwd: &Path,
    repo: &str,
    base_branch: &str,
    pr: u64,
    global_dir: &Path,
) -> Result<StackInspection, StackInspectionError> {
    let membership_args =
        membership_query_args(repo, pr).map_err(StackInspectionError::validation)?;
    let membership_raw =
        run_query(client, cwd, membership_args).map_err(StackInspectionError::query)?;
    let initial_stack =
        parse_membership_json(&membership_raw).map_err(StackInspectionError::response)?;
    let policy_ref = rollout_policy_ref(base_branch, initial_stack.as_ref())
        .map_err(StackInspectionError::validation)?;
    let args =
        inspection_query_args(repo, &policy_ref, pr).map_err(StackInspectionError::validation)?;
    let raw = run_query(client, cwd, args).map_err(|error| {
        StackInspectionError::after_membership(initial_stack.as_ref(), error, false)
    })?;
    let mut inspection = parse_json(&raw).map_err(|error| {
        StackInspectionError::after_membership(initial_stack.as_ref(), error, true)
    })?;
    validate_policy_ref(base_branch, &policy_ref, &inspection)
        .map_err(StackInspectionError::validation)?;
    let global_override =
        trusted_global_override(global_dir).map_err(StackInspectionError::validation)?;
    apply_global_override(&mut inspection, global_override)
        .map_err(StackInspectionError::validation)?;
    Ok(inspection)
}

fn run_query(client: &GhClient, cwd: &Path, args: Vec<String>) -> Result<String, String> {
    let output = client
        .prepare_command(cwd, None, GhSupervision::Supervised, GhAuthPolicy::Default)
        .map_err(|error| format!("stack inspection command preparation failed: {error}"))?
        .args(args)
        .output()
        .map_err(|error| format!("failed to inspect pull request stack: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "failed to inspect pull request stack: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub(crate) fn membership_query_args(repo: &str, pr: u64) -> Result<Vec<String>, String> {
    let (owner, name) = parse_repo_slug(repo)?;
    let query = "query($owner:String!,$name:String!,$pr:Int!){repository(owner:$owner,name:$name){pullRequest(number:$pr){stack{number size baseRefName} stackEntry{position}}}}";
    Ok(vec![
        "api".to_owned(),
        "graphql".to_owned(),
        "-f".to_owned(),
        format!("query={query}"),
        "-F".to_owned(),
        format!("owner={owner}"),
        "-F".to_owned(),
        format!("name={name}"),
        "-F".to_owned(),
        format!("pr={pr}"),
    ])
}

/// Build the shared stack/config query for every merge mutation boundary.
pub(crate) fn inspection_query_args(
    repo: &str,
    policy_ref: &str,
    pr: u64,
) -> Result<Vec<String>, String> {
    let (owner, name) = parse_repo_slug(repo)?;
    if policy_ref.is_empty() {
        return Err("stack inspection policy ref is empty".to_owned());
    }
    let query = "query($owner:String!,$name:String!,$pr:Int!,$config:String!){repository(owner:$owner,name:$name){stackConfig:object(expression:$config){... on Blob{text}} pullRequest(number:$pr){headRefOid stack{number size baseRefName} stackEntry{position}}}}";
    Ok(vec![
        "api".to_owned(),
        "graphql".to_owned(),
        "-f".to_owned(),
        format!("query={query}"),
        "-F".to_owned(),
        format!("owner={owner}"),
        "-F".to_owned(),
        format!("name={name}"),
        "-F".to_owned(),
        format!("pr={pr}"),
        "-f".to_owned(),
        format!("config={policy_ref}:.shipyard/config.toml"),
    ])
}

fn parse_repo_slug(repo: &str) -> Result<(&str, &str), String> {
    repo.split_once('/')
        .filter(|(owner, name)| !owner.is_empty() && !name.is_empty() && !name.contains('/'))
        .ok_or_else(|| format!("invalid repository slug {repo:?}"))
}

/// Parse stack/config JSON returned through either GitHub transport.
pub(crate) fn parse_json(raw: &str) -> Result<StackInspection, String> {
    let body: Value = serde_json::from_str(raw)
        .map_err(|error| format!("pull request stack query returned invalid JSON: {error}"))?;
    parse_response(&body)
}

pub(crate) fn parse_membership_json(raw: &str) -> Result<Option<StackInfo>, String> {
    let body: Value = serde_json::from_str(raw)
        .map_err(|error| format!("pull request stack query returned invalid JSON: {error}"))?;
    reject_graphql_errors(&body)?;
    let pr = body
        .pointer("/data/repository/pullRequest")
        .filter(|value| !value.is_null())
        .ok_or_else(|| "pull request stack query returned no pull request".to_owned())?;
    parse_stack(pr)
}

/// Select the governing policy lookup ref discovered from formal stack identity.
pub(crate) fn rollout_policy_ref(
    direct_base: &str,
    stack: Option<&StackInfo>,
) -> Result<String, String> {
    if direct_base.is_empty() {
        return Err("pull request direct base branch is empty".to_owned());
    }
    Ok(stack.map_or_else(|| direct_base.to_owned(), |stack| stack.base_branch.clone()))
}

/// Reject stack-base drift between policy-ref discovery and the exact inspection.
pub(crate) fn validate_policy_ref(
    direct_base: &str,
    queried_ref: &str,
    inspection: &StackInspection,
) -> Result<(), String> {
    let observed_ref = rollout_policy_ref(direct_base, inspection.stack.as_ref())?;
    if observed_ref != queried_ref {
        return Err(format!(
            "pull request stack policy ref changed from {queried_ref:?} to {observed_ref:?} during inspection; refusing mutation"
        ));
    }
    Ok(())
}

/// Apply the trusted machine-global override to an already parsed observation.
pub(crate) fn apply_trusted_global_override(
    inspection: &mut StackInspection,
    global_dir: &Path,
) -> Result<(), String> {
    apply_global_override(inspection, trusted_global_override(global_dir)?)
}

/// Refuse a formal stack before Shipyard reaches an unstacked mutation path.
pub(crate) fn ensure_unstacked(
    repo: &str,
    pr: u64,
    expected_head: &str,
    inspection: &StackInspection,
) -> Result<(), String> {
    if !inspection.head_sha.eq_ignore_ascii_case(expected_head) {
        return Err(format!(
            "stack inspection observed PR #{pr} head {} instead of expected exact head {expected_head}; refusing mutation",
            inspection.head_sha
        ));
    }
    let Some(stack) = inspection.stack.as_ref() else {
        return Ok(());
    };
    Err(refusal_message(
        repo,
        pr,
        expected_head,
        inspection.mode,
        stack,
    ))
}

/// Explain why Shipyard refuses mutation while stack merge lifecycle support is absent.
#[must_use]
pub fn unsupported_message(pr: u64, stack: &StackInfo) -> String {
    format!(
        "PR #{pr} is position {}/{} in GitHub stack #{} targeting {}; Shipyard refuses to use its unstacked merge path because GitHub requires the asynchronous merge API for stacks. Validate each layer, then use `gh stack merge {pr} --merge` for this observe-only pilot.",
        stack.position, stack.size, stack.number, stack.base_branch
    )
}

fn refusal_message(
    repo: &str,
    pr: u64,
    head_sha: &str,
    mode: StackedPrMode,
    stack: &StackInfo,
) -> String {
    let base = unsupported_message(pr, stack);
    match mode {
        StackedPrMode::Off => format!("{base} stacked_pr_mode=off."),
        StackedPrMode::Observe | StackedPrMode::Apply => {
            let plan = StackPlan::new(repo, pr, head_sha, mode, stack);
            let telemetry = serde_json::to_string(&plan)
                .expect("stack plan contains only serializable deterministic fields");
            if mode == StackedPrMode::Observe {
                format!(
                    "{base} stacked-pr-plan={telemetry}; observe mode never mutates GitHub and does not suppress required checks."
                )
            } else {
                format!(
                    "{base} stacked-pr-plan={telemetry}; stacked_pr_mode=apply is reserved and structurally unavailable (NO-GO)."
                )
            }
        }
    }
}

impl StackPlan {
    fn new(repo: &str, pr: u64, head_sha: &str, mode: StackedPrMode, stack: &StackInfo) -> Self {
        let disposition = match mode {
            StackedPrMode::Off => "blocked",
            StackedPrMode::Observe => "observe_only",
            StackedPrMode::Apply => "apply_unavailable",
        };
        Self {
            schema_version: 1,
            kind: "github_stacked_pr_plan",
            mode,
            disposition,
            identity: format!(
                "{repo}#{pr}@{head_sha}:stack-{}:{}/{}:{}",
                stack.number, stack.position, stack.size, stack.base_branch
            ),
            repository: repo.to_owned(),
            pull_request: pr,
            head_sha: head_sha.to_owned(),
            stack_number: stack.number,
            stack_size: stack.size,
            stack_position: stack.position,
            stack_base_branch: stack.base_branch.clone(),
            github_mutation: false,
            required_checks_suppressed: false,
        }
    }
}

fn parse_response(body: &Value) -> Result<StackInspection, String> {
    reject_graphql_errors(body)?;
    let repository = body
        .pointer("/data/repository")
        .filter(|value| !value.is_null())
        .ok_or_else(|| "pull request stack query returned no repository".to_owned())?;
    let config = repository
        .get("stackConfig")
        .ok_or_else(|| "pull request stack query omitted stackConfig".to_owned())?;
    let mode = if config.is_null() {
        StackedPrMode::Off
    } else {
        let text = config
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| "pull request stack config was not a readable blob".to_owned())?;
        parse_repository_mode(text)?
    };
    let pr = repository
        .get("pullRequest")
        .filter(|value| !value.is_null())
        .ok_or_else(|| "pull request stack query returned no pull request".to_owned())?;
    let head_sha = pr
        .get("headRefOid")
        .and_then(Value::as_str)
        .filter(|value| is_full_sha(value))
        .ok_or_else(|| "pull request stack query returned no exact head SHA".to_owned())?
        .to_owned();
    Ok(StackInspection {
        head_sha,
        mode,
        stack: parse_stack(pr)?,
    })
}

fn reject_graphql_errors(body: &Value) -> Result<(), String> {
    if let Some(errors) = body
        .get("errors")
        .and_then(Value::as_array)
        .filter(|errors| !errors.is_empty())
    {
        let messages = errors
            .iter()
            .filter_map(|error| error.get("message").and_then(Value::as_str))
            .collect::<Vec<_>>();
        let detail = if messages.is_empty() {
            "unknown GraphQL error".to_owned()
        } else {
            messages.join("; ")
        };
        return Err(format!(
            "pull request stack query returned GraphQL errors: {detail}"
        ));
    }
    Ok(())
}

fn parse_stack(pr: &Value) -> Result<Option<StackInfo>, String> {
    let stack = match (pr.get("stack"), pr.get("stackEntry")) {
        (Some(stack), Some(entry)) if stack.is_null() && entry.is_null() => None,
        (Some(stack), Some(entry)) if !stack.is_null() && !entry.is_null() => {
            let number = required_positive_u64(stack, "number")?;
            let size = required_positive_u64(stack, "size")?;
            let position = required_positive_u64(entry, "position")?;
            if position > size {
                return Err(format!(
                    "pull request stack position {position} exceeds size {size}"
                ));
            }
            let base_branch = stack
                .get("baseRefName")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "pull request stack is missing baseRefName".to_owned())?
                .to_owned();
            Some(StackInfo {
                number,
                size,
                position,
                base_branch,
            })
        }
        _ => return Err("pull request stack query returned inconsistent metadata".to_owned()),
    };
    Ok(stack)
}

fn parse_repository_mode(text: &str) -> Result<StackedPrMode, String> {
    let table = text
        .parse::<toml::Table>()
        .map_err(|error| format!("protected-base Shipyard config is invalid TOML: {error}"))?;
    table
        .get("stacked_pr_mode")
        .map_or(Ok(StackedPrMode::Off), |value| {
            StackedPrMode::parse(value, "protected-base")
        })
}

fn trusted_global_override(global_dir: &Path) -> Result<Option<StackedPrMode>, String> {
    let config = LoadedConfig::load_machine_global_from_dir(global_dir.to_path_buf())
        .map_err(|error| format!("failed to load trusted global stacked-PR policy: {error}"))?;
    let Some(value) = config.get("stacked_pr_mode") else {
        return Ok(None);
    };
    let mode = StackedPrMode::parse(value, "machine-global")?;
    if mode != StackedPrMode::Off {
        return Err(
            "machine-global stacked_pr_mode is a conservative override and may only be off"
                .to_owned(),
        );
    }
    Ok(Some(mode))
}

fn apply_global_override(
    inspection: &mut StackInspection,
    override_mode: Option<StackedPrMode>,
) -> Result<(), String> {
    if let Some(mode) = override_mode {
        if mode != StackedPrMode::Off {
            return Err("global stacked-PR override must be off".to_owned());
        }
        inspection.mode = StackedPrMode::Off;
    }
    Ok(())
}

fn is_full_sha(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn required_positive_u64(value: &Value, field: &str) -> Result<u64, String> {
    value
        .get(field)
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
        .ok_or_else(|| format!("pull request stack is missing positive {field}"))
}

#[cfg(test)]
mod tests {
    use super::{
        StackInfo, StackInspection, StackInspectionError, StackedPrMode, apply_global_override,
        ensure_unstacked, parse_membership_json, parse_repository_mode, parse_response,
        rollout_policy_ref, trusted_global_override, unsupported_message, validate_policy_ref,
    };

    const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn response(mode: Option<&str>, stack: &serde_json::Value) -> serde_json::Value {
        serde_json::json!({"data":{"repository":{
            "stackConfig": mode.map_or(serde_json::Value::Null, |mode| serde_json::json!({"text": format!("stacked_pr_mode = {mode:?}\n")})),
            "pullRequest": {
                "headRefOid": HEAD,
                "stack": stack.get("stack").cloned().unwrap_or(serde_json::Value::Null),
                "stackEntry": stack.get("stackEntry").cloned().unwrap_or(serde_json::Value::Null)
            }
        }}})
    }

    #[test]
    fn parses_formal_stack_and_unstacked_shapes_in_every_mode() {
        let stack = serde_json::json!({
            "stack":{"number":7,"size":3,"baseRefName":"main"},
            "stackEntry":{"position":2}
        });
        for (raw, mode) in [
            (None, StackedPrMode::Off),
            (Some("off"), StackedPrMode::Off),
            (Some("observe"), StackedPrMode::Observe),
            (Some("apply"), StackedPrMode::Apply),
        ] {
            assert_eq!(
                parse_response(&response(raw, &stack)).expect("stacked response"),
                StackInspection {
                    head_sha: HEAD.to_owned(),
                    mode,
                    stack: Some(StackInfo {
                        number: 7,
                        size: 3,
                        position: 2,
                        base_branch: "main".to_owned(),
                    }),
                }
            );
        }

        let unstacked = response(Some("observe"), &serde_json::json!({}));
        assert_eq!(
            parse_response(&unstacked),
            Ok(StackInspection {
                head_sha: HEAD.to_owned(),
                mode: StackedPrMode::Observe,
                stack: None,
            })
        );
    }

    #[test]
    fn public_membership_parser_retains_the_original_response_contract() {
        let stacked = serde_json::json!({"data":{"repository":{"pullRequest":{
            "stack":{"number":7,"size":3,"baseRefName":"main"},
            "stackEntry":{"position":2}
        }}}});
        assert_eq!(
            parse_membership_json(&stacked.to_string()),
            Ok(Some(StackInfo {
                number: 7,
                size: 3,
                position: 2,
                base_branch: "main".to_owned(),
            }))
        );
        let unstacked = serde_json::json!({"data":{"repository":{"pullRequest":{
            "stack":null,"stackEntry":null
        }}}});
        assert_eq!(parse_membership_json(&unstacked.to_string()), Ok(None));
    }

    #[test]
    fn inspection_passes_at_prefixed_base_expression_as_a_raw_string() {
        let args = super::inspection_query_args("owner/repo", "@release", 42)
            .expect("valid inspection arguments");
        let config_index = args
            .iter()
            .position(|arg| arg == "config=@release:.shipyard/config.toml")
            .expect("config expression");
        assert_eq!(args.get(config_index - 1).map(String::as_str), Some("-f"));
    }

    #[test]
    fn formal_stack_trunk_controls_policy_and_base_drift_fails_closed() {
        let stack = StackInfo {
            number: 7,
            size: 3,
            position: 2,
            base_branch: "main".to_owned(),
        };
        assert_eq!(
            rollout_policy_ref("layer-one", Some(&stack)),
            Ok("main".to_owned())
        );
        assert_eq!(rollout_policy_ref("main", None), Ok("main".to_owned()));

        let inspection = StackInspection {
            head_sha: HEAD.to_owned(),
            mode: StackedPrMode::Observe,
            stack: Some(StackInfo {
                base_branch: "release".to_owned(),
                ..stack
            }),
        };
        let error = validate_policy_ref("layer-one", "main", &inspection)
            .expect_err("changed stack trunk must fail closed");
        assert!(
            error.contains("changed from \"main\" to \"release\""),
            "{error}"
        );
    }

    #[test]
    fn discovered_stack_disables_later_graphql_exhaustion_fallback() {
        let stack = StackInfo {
            number: 7,
            size: 3,
            position: 2,
            base_branch: "main".to_owned(),
        };
        let error = StackInspectionError::after_membership(
            Some(&stack),
            "GraphQL: API rate limit already exceeded".to_owned(),
            false,
        );
        assert!(!error.is_graphql_rate_limited());
        assert!(
            error
                .into_message()
                .contains("formal GitHub stack #7 was discovered")
        );

        let unstacked_error = StackInspectionError::after_membership(
            None,
            "GraphQL: API rate limit already exceeded".to_owned(),
            false,
        );
        assert!(unstacked_error.is_graphql_rate_limited());
    }

    #[test]
    fn malformed_or_partial_metadata_fails_closed() {
        for body in [
            serde_json::json!({"data":{"repository":{"stackConfig":null,"pullRequest":{}}}}),
            response(
                None,
                &serde_json::json!({
                    "stack":{"number":7,"size":3,"baseRefName":"main"},
                    "stackEntry":null
                }),
            ),
            response(
                None,
                &serde_json::json!({
                    "stack":{"number":7,"size":3,"baseRefName":"main"},
                    "stackEntry":{"position":4}
                }),
            ),
            serde_json::json!({"data":{"repository":{"pullRequest":{
                "headRefOid": HEAD,
                "stack":null,
                "stackEntry":null
            }}}}),
        ] {
            assert!(parse_response(&body).is_err());
        }
    }

    #[test]
    fn invalid_modes_and_global_widening_fail_closed() {
        assert!(parse_repository_mode("stacked_pr_mode = true").is_err());
        assert!(parse_repository_mode("stacked_pr_mode = \"pilot\"").is_err());
        let mut inspection = StackInspection {
            head_sha: HEAD.to_owned(),
            mode: StackedPrMode::Observe,
            stack: None,
        };
        apply_global_override(&mut inspection, Some(StackedPrMode::Off)).expect("force off");
        assert_eq!(inspection.mode, StackedPrMode::Off);
        assert!(apply_global_override(&mut inspection, Some(StackedPrMode::Observe)).is_err());
    }

    #[test]
    fn trusted_global_config_can_only_force_off() {
        let temp = tempfile::tempdir().expect("temp");
        assert_eq!(trusted_global_override(temp.path()), Ok(None));

        std::fs::write(
            temp.path().join("config.toml"),
            "stacked_pr_mode = \"off\"\n",
        )
        .expect("write off override");
        assert_eq!(
            trusted_global_override(temp.path()),
            Ok(Some(StackedPrMode::Off))
        );

        std::fs::write(
            temp.path().join("config.toml"),
            "stacked_pr_mode = \"observe\"\n",
        )
        .expect("write widening override");
        assert!(trusted_global_override(temp.path()).is_err());
    }

    #[test]
    fn graphql_error_messages_survive_for_rate_limit_classification() {
        let error = parse_response(&serde_json::json!({
            "data": null,
            "errors": [{"message": "API rate limit already exceeded for user ID 123"}]
        }))
        .expect_err("GraphQL error must fail closed");
        assert!(error.contains("GraphQL"));
        assert!(error.contains("rate limit already exceeded"));
        assert!(crate::gh::is_graphql_rate_limited(&error));
    }

    #[test]
    fn observe_plan_is_deterministic_exact_head_telemetry() {
        let inspection = StackInspection {
            head_sha: HEAD.to_owned(),
            mode: StackedPrMode::Observe,
            stack: Some(StackInfo {
                number: 7,
                size: 3,
                position: 2,
                base_branch: "main".to_owned(),
            }),
        };
        let first = ensure_unstacked("owner/repo", 42, HEAD, &inspection)
            .expect_err("stack must be blocked");
        let second = ensure_unstacked("owner/repo", 42, HEAD, &inspection)
            .expect_err("stack must be blocked");
        assert_eq!(first, second);
        assert!(first.contains("\"mode\":\"observe\""), "{first}");
        assert!(
            first.contains(&format!("\"head_sha\":\"{HEAD}\"")),
            "{first}"
        );
        assert!(first.contains("\"stack_number\":7"), "{first}");
        assert!(first.contains("\"stack_position\":2"), "{first}");
        assert!(first.contains("\"github_mutation\":false"), "{first}");
        assert!(
            first.contains("\"required_checks_suppressed\":false"),
            "{first}"
        );
    }

    #[test]
    fn apply_mode_is_structurally_unavailable() {
        let inspection = StackInspection {
            head_sha: HEAD.to_owned(),
            mode: StackedPrMode::Apply,
            stack: Some(StackInfo {
                number: 7,
                size: 3,
                position: 2,
                base_branch: "main".to_owned(),
            }),
        };
        let error = ensure_unstacked("owner/repo", 42, HEAD, &inspection)
            .expect_err("apply must remain unavailable");
        assert!(error.contains("\"disposition\":\"apply_unavailable\""));
        assert!(error.contains("NO-GO"));
    }

    #[test]
    fn head_mismatch_fails_before_plan_emission() {
        let inspection = StackInspection {
            head_sha: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned(),
            mode: StackedPrMode::Observe,
            stack: None,
        };
        let error = ensure_unstacked("owner/repo", 42, HEAD, &inspection)
            .expect_err("head mismatch must fail");
        assert!(error.contains("instead of expected exact head"));
        assert!(!error.contains("stacked-pr-plan="));
    }

    #[test]
    fn refusal_names_stack_and_supported_pilot_path() {
        let message = unsupported_message(
            42,
            &StackInfo {
                number: 7,
                size: 3,
                position: 2,
                base_branch: "main".to_owned(),
            },
        );
        assert!(message.contains("position 2/3"));
        assert!(message.contains("stack #7"));
        assert!(message.contains("gh stack merge 42 --merge"));
    }
}
