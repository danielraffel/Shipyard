//! Which `gh` argument vectors an in-process read may answer.
//!
//! The classifier is deliberately narrow. It accepts only the exact shapes the
//! steward's read paths build, and only when every argument is understood.
//! Anything else, including every flag that could make `gh` write or send a
//! body to a REST endpoint, is declined and runs through `gh` unchanged.

use serde_json::{Map, Value};

use super::pr_view::PrField;

/// One read an in-process client can answer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ReadRequest {
    /// `gh api repos/OWNER/REPO/...`: a REST `GET`.
    Rest {
        /// Endpoint path and query, without a leading slash.
        path: String,
    },
    /// `gh api graphql -f query=... [-f|-F name=value]...`: a read query.
    Graphql {
        /// Query document.
        query: String,
        /// Variables, typed the way `gh` types them.
        variables: Map<String, Value>,
    },
    /// `gh pr view N --repo OWNER/REPO --json FIELDS`.
    PrView {
        /// Pull-request number.
        number: u64,
        /// Requested fields, in request order.
        fields: Vec<PrField>,
    },
}

/// A classified read and the exact repository whose App installation must
/// answer it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ClassifiedRead {
    pub(crate) request: ReadRequest,
    pub(crate) repo: String,
}

/// Classify `args`. `scope_repo` is the repository the caller scoped its
/// client to (`GH_REPO` for the wrapper); a read whose own repository
/// disagrees with it is declined rather than guessed.
pub(crate) fn classify(args: &[String], scope_repo: Option<&str>) -> Option<ClassifiedRead> {
    let first = args.first()?.as_str();
    let classified = match first {
        "api" if args.get(1).map(String::as_str) == Some("graphql") => {
            let request = classify_graphql(&args[2..])?;
            ClassifiedRead {
                request,
                repo: exact_slug(scope_repo?)?,
            }
        }
        "api" => classify_rest(args)?,
        "pr" => classify_pr_view(args)?,
        _ => return None,
    };
    if let Some(scope) = scope_repo
        && !scope.eq_ignore_ascii_case(&classified.repo)
    {
        return None;
    }
    Some(classified)
}

fn classify_rest(args: &[String]) -> Option<ClassifiedRead> {
    // Exactly `api ENDPOINT`: any flag (`-X`, `-f`, `--input`, `--paginate`,
    // `--jq`, `-H`, ...) changes the method, the body or the output.
    let [_, endpoint] = args else {
        return None;
    };
    let path = endpoint.strip_prefix('/').unwrap_or(endpoint);
    if path.is_empty()
        || path.starts_with('-')
        || path
            .chars()
            .any(|ch| ch.is_whitespace() || ch.is_control() || matches!(ch, '#' | '{' | '}' | '\\'))
    {
        return None;
    }
    let route = path.split('?').next().unwrap_or_default();
    let segments = route.split('/').collect::<Vec<_>>();
    let ["repos", owner, name, ..] = segments.as_slice() else {
        return None;
    };
    if segments
        .iter()
        .any(|segment| matches!(*segment, "" | "." | ".."))
    {
        return None;
    }
    let repo = exact_slug(&format!("{owner}/{name}"))?;
    Some(ClassifiedRead {
        request: ReadRequest::Rest {
            path: path.to_owned(),
        },
        repo,
    })
}

fn classify_graphql(rest: &[String]) -> Option<ReadRequest> {
    let mut query = None;
    let mut variables = Map::new();
    let mut iter = rest.iter();
    while let Some(flag) = iter.next() {
        let raw = match flag.as_str() {
            "-f" | "--raw-field" => true,
            "-F" | "--field" => false,
            _ => return None,
        };
        let (key, value) = iter.next()?.split_once('=')?;
        if !is_variable_name(key) {
            return None;
        }
        let value = if raw {
            Value::String(value.to_owned())
        } else {
            typed_field(value)?
        };
        if key == "query" {
            let Value::String(text) = value else {
                return None;
            };
            if query.replace(text).is_some() {
                return None;
            }
        } else if key == "operationName" || variables.insert(key.to_owned(), value).is_some() {
            return None;
        }
    }
    let query = query?;
    is_read_only_query(&query).then_some(ReadRequest::Graphql { query, variables })
}

/// `gh api -F` value typing: `true`, `false`, `null` and integers become
/// JSON literals; a leading `@` reads a file, which this client never does.
fn typed_field(value: &str) -> Option<Value> {
    if value.starts_with('@') {
        return None;
    }
    Some(match value {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        "null" => Value::Null,
        _ => value
            .parse::<i64>()
            .ok()
            .filter(|_| {
                // Go's strconv.Atoi accepts an optional sign and decimal digits
                // only; Rust's parser agrees except for a leading `+`.
                !value.starts_with('+')
            })
            .map_or_else(|| Value::String(value.to_owned()), Value::from),
    })
}

fn is_variable_name(key: &str) -> bool {
    let mut chars = key.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

/// A query document that cannot write: it opens with `query` or a bare
/// selection set, and names no `mutation` or `subscription` anywhere.
pub(crate) fn is_read_only_query(query: &str) -> bool {
    let trimmed = query.trim_start();
    let opens_as_read = trimmed.starts_with('{')
        || trimmed.strip_prefix("query").is_some_and(|rest| {
            rest.starts_with(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
        });
    let words = query
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
        .collect::<Vec<_>>();
    opens_as_read
        && !words
            .iter()
            .any(|word| matches!(*word, "mutation" | "subscription"))
}

fn classify_pr_view(args: &[String]) -> Option<ClassifiedRead> {
    if args.get(1).map(String::as_str) != Some("view") {
        return None;
    }
    let number = args.get(2)?;
    if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let number = number.parse::<u64>().ok().filter(|number| *number > 0)?;
    let mut repo = None;
    let mut fields = None;
    let mut iter = args[3..].iter();
    while let Some(flag) = iter.next() {
        let value = iter.next()?;
        let slot = match flag.as_str() {
            "--repo" | "-R" => &mut repo,
            "--json" => &mut fields,
            _ => return None,
        };
        if slot.replace(value.clone()).is_some() {
            return None;
        }
    }
    let repo = exact_slug(&repo?)?;
    let mut parsed = Vec::new();
    for name in fields?.split(',') {
        let field = PrField::parse(name)?;
        if parsed.contains(&field) {
            return None;
        }
        parsed.push(field);
    }
    if parsed.is_empty() {
        return None;
    }
    Some(ClassifiedRead {
        request: ReadRequest::PrView {
            number,
            fields: parsed,
        },
        repo,
    })
}

/// `OWNER/REPO` exactly as GitHub spells a repository, or `None`.
pub(crate) fn exact_slug(slug: &str) -> Option<String> {
    let (owner, name) = slug.split_once('/')?;
    let owner_ok = (1..=39).contains(&owner.len())
        && owner
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && owner
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
    let name_ok = !matches!(name, "" | "." | "..")
        && name.len() <= 100
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    (owner_ok && name_ok).then(|| slug.to_owned())
}
