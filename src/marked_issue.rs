//! Find the open GitHub issues a Shipyard subsystem owns, by an HTML-comment
//! marker in the body.
//!
//! Matching by marker rather than title keeps an issue a human retitled from
//! being duplicated, and an issue without the marker is somebody else's and is
//! never touched.

use serde_json::Value;

/// One open issue whose body carries the caller's marker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MarkedIssue {
    /// Issue number.
    pub number: u64,
    /// The key the marker carries (`<prefix>KEY -->`).
    pub key: String,
    /// The full body, for an in-place edit.
    pub body: String,
}

/// The `gh api` argv that lists every open issue, one JSON object per line.
#[must_use]
pub fn list_open_args(repo: &str) -> Vec<String> {
    vec![
        "api".to_owned(),
        "--paginate".to_owned(),
        format!("repos/{repo}/issues?state=open&per_page=100"),
        "--jq".to_owned(),
        ".[]".to_owned(),
    ]
}

/// The issues in `raw` (the output of [`list_open_args`]) whose body carries
/// `prefix`. Pull requests, which the issues endpoint also returns, are
/// skipped.
#[must_use]
pub fn parse(raw: &str, prefix: &str) -> Vec<MarkedIssue> {
    let mut issues = Vec::new();
    for line in raw.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("pull_request").is_some() {
            continue;
        }
        let body = value
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let Some(key) = marker_key(body, prefix) else {
            continue;
        };
        let Some(number) = value.get("number").and_then(Value::as_u64) else {
            continue;
        };
        issues.push(MarkedIssue {
            number,
            key,
            body: body.to_owned(),
        });
    }
    issues
}

/// The key a body's marker carries, if it carries one.
#[must_use]
pub fn marker_key(body: &str, prefix: &str) -> Option<String> {
    let start = body.find(prefix)? + prefix.len();
    let rest = &body[start..];
    let end = rest.find("-->")?;
    let key = rest[..end].trim();
    (!key.is_empty()).then(|| key.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PREFIX: &str = "<!-- test-subject: ";

    #[test]
    fn keeps_only_marked_issues_and_skips_pull_requests() {
        let raw = [
            r#"{"number":1,"body":"<!-- test-subject: a -->\nopen"}"#,
            r#"{"number":2,"body":"no marker"}"#,
            r#"{"number":3,"body":"<!-- test-subject: b -->","pull_request":{}}"#,
            r#"{"number":4,"body":"<!-- other-subject: c -->"}"#,
            "not json",
        ]
        .join("\n");
        let issues = parse(&raw, PREFIX);
        assert_eq!(issues.len(), 1);
        assert_eq!((issues[0].number, issues[0].key.as_str()), (1, "a"));
    }

    #[test]
    fn an_empty_or_unterminated_marker_is_no_marker() {
        assert_eq!(marker_key("<!-- test-subject:  -->", PREFIX), None);
        assert_eq!(marker_key("<!-- test-subject: a", PREFIX), None);
        assert_eq!(
            marker_key("x <!-- test-subject: k --> y", PREFIX).as_deref(),
            Some("k")
        );
    }
}
