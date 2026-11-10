//! `gh pr view --json FIELDS`, answered in process.
//!
//! The query asks GitHub for exactly the selection `gh` asks for, and the
//! answer is reshaped exactly as `gh` reshapes it, so a caller parsing the JSON
//! cannot tell which path produced it. The reshaping rules are `gh`'s, not
//! GitHub's, and several are surprising; each is pinned by a test:
//!
//! * `author` becomes `{"is_bot", "login", "id", "name"}`. An author without a
//!   `User` id is reported as a bot, with `app/` prefixed to its login.
//! * `statusCheckRollup` flattens the last commit's rollup contexts. A check
//!   run's missing timestamps become `0001-01-01T00:00:00Z` and its missing
//!   conclusion becomes `""`; a status context reports its `createdAt` as
//!   `startedAt`. A head with no checks is `[]`; only a pull request with no
//!   commit is `null`.
//! * `labels` is the label node list; a missing description is `""`.
//!
//! When the answer is something `gh` would have paged further (more than 100
//! labels or rollup contexts), the read is declined and `gh` answers instead.

use serde_json::{Map, Value, json};

const GO_ZERO_TIME: &str = "0001-01-01T00:00:00Z";

/// A `--json` field this client can answer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PrField {
    Id,
    Number,
    State,
    IsDraft,
    BaseRefName,
    HeadRefOid,
    HeadRefName,
    MergeStateStatus,
    AutoMergeRequest,
    Labels,
    Author,
    ChangedFiles,
    MergedAt,
    StatusCheckRollup,
}

impl PrField {
    pub(crate) fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "id" => Self::Id,
            "number" => Self::Number,
            "state" => Self::State,
            "isDraft" => Self::IsDraft,
            "baseRefName" => Self::BaseRefName,
            "headRefOid" => Self::HeadRefOid,
            "headRefName" => Self::HeadRefName,
            "mergeStateStatus" => Self::MergeStateStatus,
            "autoMergeRequest" => Self::AutoMergeRequest,
            "labels" => Self::Labels,
            "author" => Self::Author,
            "changedFiles" => Self::ChangedFiles,
            "mergedAt" => Self::MergedAt,
            "statusCheckRollup" => Self::StatusCheckRollup,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Self::Id => "id",
            Self::Number => "number",
            Self::State => "state",
            Self::IsDraft => "isDraft",
            Self::BaseRefName => "baseRefName",
            Self::HeadRefOid => "headRefOid",
            Self::HeadRefName => "headRefName",
            Self::MergeStateStatus => "mergeStateStatus",
            Self::AutoMergeRequest => "autoMergeRequest",
            Self::Labels => "labels",
            Self::Author => "author",
            Self::ChangedFiles => "changedFiles",
            Self::MergedAt => "mergedAt",
            Self::StatusCheckRollup => "statusCheckRollup",
        }
    }

    /// The selection `gh` sends for this field.
    fn selection(self) -> &'static str {
        match self {
            Self::AutoMergeRequest => {
                "autoMergeRequest {authorEmail,commitBody,commitHeadline,mergeMethod,enabledAt,enabledBy{login,...on User{id,name}}}"
            }
            Self::Labels => "labels(first:100){nodes{id,name,description,color},totalCount}",
            Self::Author => "author{login,...on User{id,name}}",
            Self::StatusCheckRollup => {
                "statusCheckRollup: commits(last: 1) {nodes {commit {statusCheckRollup {contexts(first:100) {nodes {__typename...on StatusContext {context,state,targetUrl,createdAt,description},...on CheckRun {name,checkSuite{workflowRun{workflow{name}}},status,conclusion,startedAt,completedAt,detailsUrl}},pageInfo{hasNextPage,endCursor}}}}}}"
            }
            other => other.name(),
        }
    }
}

/// The GraphQL document and variables for one `pr view`.
pub(crate) fn query(repo: &str, number: u64, fields: &[PrField]) -> (String, Map<String, Value>) {
    let selection = fields
        .iter()
        .map(|field| field.selection())
        .collect::<Vec<_>>()
        .join(",");
    let document = format!(
        "query PullRequestByNumber($owner: String!, $repo: String!, $pr_number: Int!) {{\n\trepository(owner: $owner, name: $repo) {{\n\t\tpullRequest(number: $pr_number) {{{selection}}}\n\t}}\n}}"
    );
    let (owner, name) = repo.split_once('/').unwrap_or((repo, ""));
    let mut variables = Map::new();
    variables.insert("owner".to_owned(), json!(owner));
    variables.insert("repo".to_owned(), json!(name));
    variables.insert("pr_number".to_owned(), json!(number));
    (document, variables)
}

/// Why a successful GraphQL answer could not be reshaped.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ShapeError {
    /// `gh` would have read more pages; let it.
    NeedsPaging(&'static str),
    /// The answer is not the shape the query asked for.
    Malformed(&'static str),
}

/// Reshape `data.repository.pullRequest` the way `gh` prints it.
pub(crate) fn render(pull_request: &Value, fields: &[PrField]) -> Result<Value, ShapeError> {
    let object = pull_request
        .as_object()
        .ok_or(ShapeError::Malformed("pullRequest is not an object"))?;
    let mut output = Map::new();
    for field in fields {
        let raw = object.get(field.name()).unwrap_or(&Value::Null);
        let value = match field {
            PrField::Author => author(raw),
            PrField::AutoMergeRequest => auto_merge_request(raw),
            PrField::Labels => labels(raw)?,
            PrField::StatusCheckRollup => status_check_rollup(raw)?,
            _ => raw.clone(),
        };
        output.insert(field.name().to_owned(), value);
    }
    Ok(Value::Object(output))
}

fn string_or_empty(value: Option<&Value>) -> Value {
    Value::String(value.and_then(Value::as_str).unwrap_or_default().to_owned())
}

fn time_or_zero(value: Option<&Value>) -> Value {
    Value::String(
        value
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .unwrap_or(GO_ZERO_TIME)
            .to_owned(),
    )
}

/// `gh`'s `Author` encoding: no `User` id means a bot.
fn author(raw: &Value) -> Value {
    let login = raw.get("login").and_then(Value::as_str).unwrap_or_default();
    let id = raw.get("id").and_then(Value::as_str).unwrap_or_default();
    if id.is_empty() {
        return json!({"is_bot": true, "login": format!("app/{login}")});
    }
    json!({
        "id": id,
        "is_bot": false,
        "login": login,
        "name": string_or_empty(raw.get("name")),
    })
}

fn auto_merge_request(raw: &Value) -> Value {
    let Some(request) = raw.as_object() else {
        return Value::Null;
    };
    let mut output = Map::new();
    for key in ["authorEmail", "commitBody", "commitHeadline"] {
        output.insert(
            key.to_owned(),
            request.get(key).cloned().unwrap_or(Value::Null),
        );
    }
    output.insert(
        "mergeMethod".to_owned(),
        string_or_empty(request.get("mergeMethod")),
    );
    output.insert(
        "enabledAt".to_owned(),
        time_or_zero(request.get("enabledAt")),
    );
    output.insert(
        "enabledBy".to_owned(),
        author(request.get("enabledBy").unwrap_or(&Value::Null)),
    );
    Value::Object(output)
}

fn labels(raw: &Value) -> Result<Value, ShapeError> {
    let nodes = raw
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or(ShapeError::Malformed("labels.nodes missing"))?;
    let total = raw.get("totalCount").and_then(Value::as_u64).unwrap_or(0);
    if total > nodes.len() as u64 {
        return Err(ShapeError::NeedsPaging("labels"));
    }
    Ok(Value::Array(
        nodes
            .iter()
            .map(|node| {
                json!({
                    "id": string_or_empty(node.get("id")),
                    "name": string_or_empty(node.get("name")),
                    "description": string_or_empty(node.get("description")),
                    "color": string_or_empty(node.get("color")),
                })
            })
            .collect(),
    ))
}

fn status_check_rollup(raw: &Value) -> Result<Value, ShapeError> {
    let commits = raw
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or(ShapeError::Malformed("statusCheckRollup.nodes missing"))?;
    // gh prints `null` only when the pull request has no commit; a head
    // with no rollup, or an empty one, is `[]`.
    let Some(commit) = commits.first() else {
        return Ok(Value::Null);
    };
    let Some(rollup) = commit
        .pointer("/commit/statusCheckRollup")
        .filter(|rollup| !rollup.is_null())
    else {
        return Ok(Value::Array(Vec::new()));
    };
    let contexts = rollup
        .get("contexts")
        .ok_or(ShapeError::Malformed("rollup contexts missing"))?;
    if contexts
        .pointer("/pageInfo/hasNextPage")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Err(ShapeError::NeedsPaging("statusCheckRollup"));
    }
    let nodes = contexts
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or(ShapeError::Malformed("rollup context nodes missing"))?;
    Ok(Value::Array(nodes.iter().map(rollup_context).collect()))
}

fn rollup_context(node: &Value) -> Value {
    let typename = string_or_empty(node.get("__typename"));
    if typename.as_str() == Some("CheckRun") {
        return json!({
            "__typename": typename,
            "name": string_or_empty(node.get("name")),
            "workflowName": string_or_empty(node.pointer("/checkSuite/workflowRun/workflow/name")),
            "status": string_or_empty(node.get("status")),
            "conclusion": string_or_empty(node.get("conclusion")),
            "startedAt": time_or_zero(node.get("startedAt")),
            "completedAt": time_or_zero(node.get("completedAt")),
            "detailsUrl": string_or_empty(node.get("detailsUrl")),
        });
    }
    json!({
        "__typename": typename,
        "context": string_or_empty(node.get("context")),
        "state": string_or_empty(node.get("state")),
        "targetUrl": string_or_empty(node.get("targetUrl")),
        "startedAt": time_or_zero(node.get("createdAt")),
    })
}

/// `gh`'s message for GraphQL errors on a `pr view`:
/// `GraphQL: message (path), message (path)`.
pub(crate) fn graphql_error_message(errors: &[Value]) -> String {
    let messages = errors
        .iter()
        .map(|error| {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let path = error
                .get("path")
                .and_then(Value::as_array)
                .map(|path| {
                    path.iter()
                        .map(|segment| match segment {
                            Value::String(text) => text.clone(),
                            other => other.to_string(),
                        })
                        .collect::<Vec<_>>()
                        .join(".")
                })
                .unwrap_or_default();
            if path.is_empty() {
                message.to_owned()
            } else {
                format!("{message} ({path})")
            }
        })
        .collect::<Vec<_>>();
    format!("GraphQL: {}", messages.join(", "))
}
