//! Read-only discovery and fail-closed policy for GitHub stacked pull requests.

use std::path::Path;

use serde_json::Value;

use crate::gh::{GhAuthPolicy, GhClient, GhSupervision};

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

/// Query formal stack membership using GitHub's public-preview GraphQL fields.
pub fn fetch(
    client: &GhClient,
    cwd: &Path,
    repo: &str,
    pr: u64,
) -> Result<Option<StackInfo>, String> {
    let (owner, name) = repo
        .split_once('/')
        .ok_or_else(|| format!("invalid repository slug {repo:?}"))?;
    let query = "query($owner:String!,$name:String!,$pr:Int!){repository(owner:$owner,name:$name){pullRequest(number:$pr){stack{number size baseRefName} stackEntry{position}}}}";
    let output = client
        .prepare_command(cwd, None, GhSupervision::Supervised, GhAuthPolicy::Default)
        .map_err(|error| format!("stack inspection command preparation failed: {error}"))?
        .args([
            "api",
            "graphql",
            "-f",
            &format!("query={query}"),
            "-F",
            &format!("owner={owner}"),
            "-F",
            &format!("name={name}"),
            "-F",
            &format!("pr={pr}"),
        ])
        .output()
        .map_err(|error| format!("failed to inspect pull request stack: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "failed to inspect pull request stack: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let body: Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("pull request stack query returned invalid JSON: {error}"))?;
    parse_response(&body)
}

/// Explain why Shipyard refuses mutation while stack merge lifecycle support is absent.
#[must_use]
pub fn unsupported_message(pr: u64, stack: &StackInfo) -> String {
    format!(
        "PR #{pr} is position {}/{} in GitHub stack #{} targeting {}; Shipyard refuses to use its unstacked merge path because GitHub requires the asynchronous merge API for stacks. Validate each layer, then use `gh stack merge {pr} --merge` for this observe-only pilot.",
        stack.position, stack.size, stack.number, stack.base_branch
    )
}

fn parse_response(body: &Value) -> Result<Option<StackInfo>, String> {
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
    let pr = body
        .pointer("/data/repository/pullRequest")
        .filter(|value| !value.is_null())
        .ok_or_else(|| "pull request stack query returned no pull request".to_owned())?;
    match (pr.get("stack"), pr.get("stackEntry")) {
        (Some(stack), Some(entry)) if stack.is_null() && entry.is_null() => Ok(None),
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
            Ok(Some(StackInfo {
                number,
                size,
                position,
                base_branch,
            }))
        }
        _ => Err("pull request stack query returned inconsistent metadata".to_owned()),
    }
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
    use super::{StackInfo, parse_response, unsupported_message};

    #[test]
    fn parses_formal_stack_and_unstacked_shapes() {
        let stacked = serde_json::json!({"data":{"repository":{"pullRequest":{
            "stack":{"number":7,"size":3,"baseRefName":"main"},
            "stackEntry":{"position":2}
        }}}});
        assert_eq!(
            parse_response(&stacked).expect("stacked response"),
            Some(StackInfo {
                number: 7,
                size: 3,
                position: 2,
                base_branch: "main".to_owned(),
            })
        );

        let unstacked = serde_json::json!({"data":{"repository":{"pullRequest":{
            "stack":null,"stackEntry":null
        }}}});
        assert_eq!(parse_response(&unstacked), Ok(None));
    }

    #[test]
    fn malformed_or_partial_metadata_fails_closed() {
        for body in [
            serde_json::json!({"data":{"repository":{"pullRequest":{}}}}),
            serde_json::json!({"data":{"repository":{"pullRequest":{
                "stack":{"number":7,"size":3,"baseRefName":"main"},
                "stackEntry":null
            }}}}),
            serde_json::json!({"data":{"repository":{"pullRequest":{
                "stack":{"number":7,"size":3,"baseRefName":"main"},
                "stackEntry":{"position":4}
            }}}}),
        ] {
            assert!(parse_response(&body).is_err());
        }
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
