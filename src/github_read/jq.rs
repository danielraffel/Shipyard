//! The small `jq` subset in-process reads can evaluate faithfully.
//!
//! `gh api --jq FILTER` runs `FILTER` with gojq and prints each result the way
//! `gh` does: a string raw, `null` as an empty line, a number or boolean as
//! its text, and anything else as compact JSON with sorted keys and Go's
//! HTML-safe string escaping. This module parses only the filter forms the
//! steward builds and evaluates them with the same results. A filter it does
//! not fully understand fails to parse, and the read runs through `gh`
//! unchanged. So does any input whose output it could not print identically
//! (a non-integral number, an integer beyond 2^53, a control character gojq
//! and Go escape differently, an iteration over an object).
//!
//! Grammar, lowest precedence first (jq's own order):
//!
//! ```text
//! pipe  := alt ('|' alt)*
//! alt   := cmp ('//' alt)?
//! cmp   := term (('==' | '!=') term)?
//! term  := primary suffix*          suffix only after a path or parentheses
//! suffix:= '.' ident | '[' ']' | '[' digits ']'
//! primary := '.' ident? | '(' pipe ')' | '[' pipe? ']' | '{' entries '}'
//!          | string | digits | null | true | false | length | select(pipe)
//!          | 'if' pipe 'then' pipe 'else' pipe 'end'
//! entries := (key (':' objval)?) (',' ...)*     key := ident | string
//! objval  := term ('|' term)*
//! ```

use serde_json::{Map, Value};

/// Deepest nesting a filter may have; deeper filters are not parsed.
const MAX_DEPTH: usize = 24;
/// Largest integer printed as `gh` prints it: gh decodes numbers as doubles.
const MAX_SAFE_INTEGER: u64 = 1 << 53;

/// A parsed filter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Program(Expr);

#[derive(Clone, Debug, Eq, PartialEq)]
enum Expr {
    Identity,
    Literal(Value),
    Field(Box<Expr>, String),
    Element(Box<Expr>, usize),
    Iterate(Box<Expr>),
    Pipe(Box<Expr>, Box<Expr>),
    Alternative(Box<Expr>, Box<Expr>),
    Equals {
        left: Box<Expr>,
        right: Box<Expr>,
        negate: bool,
    },
    If {
        condition: Box<Expr>,
        then: Box<Expr>,
        otherwise: Box<Expr>,
    },
    Object(Vec<(String, Expr)>),
    Array(Option<Box<Expr>>),
    Length,
    Select(Box<Expr>),
}

/// Why a filter could not be evaluated or printed the way `gh` does.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Unsupported;

/// Why one step of evaluation stopped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Fail {
    /// jq itself would raise an error here (and `//` would absorb it).
    Jq,
    /// This module cannot say what gojq would do.
    Unsupported,
}

impl From<Unsupported> for Fail {
    fn from(_: Unsupported) -> Self {
        Self::Unsupported
    }
}

impl Program {
    /// Parse `source`, or `None` when it is outside the supported subset.
    pub(crate) fn parse(source: &str) -> Option<Self> {
        let mut parser = Parser {
            chars: source.chars().collect(),
            at: 0,
            depth: 0,
        };
        let expr = parser.pipe()?;
        parser.skip_space();
        (parser.at == parser.chars.len()).then_some(Self(expr))
    }

    /// Run the filter over one page and print its results as `gh` would.
    pub(crate) fn run(&self, page: &Value, out: &mut Vec<u8>) -> Result<(), Unsupported> {
        for value in eval(&self.0, page).map_err(|_| Unsupported)? {
            print(&value, out)?;
        }
        Ok(())
    }
}

struct Parser {
    chars: Vec<char>,
    at: usize,
    depth: usize,
}

impl Parser {
    fn skip_space(&mut self) {
        while self.chars.get(self.at).is_some_and(|ch| ch.is_whitespace()) {
            self.at += 1;
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.at).copied()
    }

    fn starts_with(&self, text: &str) -> bool {
        text.chars()
            .enumerate()
            .all(|(offset, ch)| self.chars.get(self.at + offset) == Some(&ch))
    }

    /// Consume `text` after optional space.
    fn eat(&mut self, text: &str) -> bool {
        self.skip_space();
        if self.starts_with(text) {
            self.at += text.chars().count();
            true
        } else {
            false
        }
    }

    /// Consume a keyword (not the start of a longer identifier).
    fn eat_word(&mut self, word: &str) -> bool {
        self.skip_space();
        let end = self.at + word.chars().count();
        if self.starts_with(word)
            && !self
                .chars
                .get(end)
                .is_some_and(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
        {
            self.at = end;
            true
        } else {
            false
        }
    }

    fn ident(&mut self) -> Option<String> {
        let start = self.at;
        let first = self.peek()?;
        if !(first.is_ascii_alphabetic() || first == '_') {
            return None;
        }
        while self
            .peek()
            .is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        {
            self.at += 1;
        }
        Some(self.chars[start..self.at].iter().collect())
    }

    fn enter(&mut self) -> Option<()> {
        self.depth += 1;
        (self.depth <= MAX_DEPTH).then_some(())
    }

    fn pipe(&mut self) -> Option<Expr> {
        self.enter()?;
        let mut left = self.alternative()?;
        loop {
            self.skip_space();
            if self.starts_with("|") && !self.starts_with("||") {
                self.at += 1;
                let right = self.alternative()?;
                left = Expr::Pipe(Box::new(left), Box::new(right));
            } else {
                break;
            }
        }
        self.depth -= 1;
        Some(left)
    }

    fn alternative(&mut self) -> Option<Expr> {
        let left = self.comparison()?;
        if self.eat("//") {
            if self.starts_with("=") {
                return None;
            }
            self.enter()?;
            let right = self.alternative()?;
            self.depth -= 1;
            return Some(Expr::Alternative(Box::new(left), Box::new(right)));
        }
        Some(left)
    }

    fn comparison(&mut self) -> Option<Expr> {
        let left = self.term()?;
        for (operator, negate) in [("==", false), ("!=", true)] {
            if self.eat(operator) {
                let right = self.term()?;
                return Some(Expr::Equals {
                    left: Box::new(left),
                    right: Box::new(right),
                    negate,
                });
            }
        }
        Some(left)
    }

    fn term(&mut self) -> Option<Expr> {
        self.skip_space();
        let (mut expr, suffixes) = self.primary()?;
        while suffixes && self.at < self.chars.len() {
            if self.starts_with(".") && !self.starts_with("..") {
                self.at += 1;
                expr = Expr::Field(Box::new(expr), self.ident()?);
            } else if self.starts_with("[") {
                self.at += 1;
                self.skip_space();
                if self.peek() == Some(']') {
                    self.at += 1;
                    expr = Expr::Iterate(Box::new(expr));
                } else {
                    let index = self.digits()?;
                    self.skip_space();
                    if self.peek() != Some(']') {
                        return None;
                    }
                    self.at += 1;
                    expr = Expr::Element(Box::new(expr), usize::try_from(index).ok()?);
                }
            } else {
                break;
            }
        }
        Some(expr)
    }

    fn digits(&mut self) -> Option<u64> {
        let start = self.at;
        while self.peek().is_some_and(|ch| ch.is_ascii_digit()) {
            self.at += 1;
        }
        if start == self.at {
            return None;
        }
        // A fraction or exponent is a float; floats are not supported.
        if matches!(self.peek(), Some('.' | 'e' | 'E')) {
            return None;
        }
        let text: String = self.chars[start..self.at].iter().collect();
        text.parse::<u64>().ok().filter(|n| *n < MAX_SAFE_INTEGER)
    }

    /// A primary expression, and whether `.x` / `[..]` may follow it.
    fn primary(&mut self) -> Option<(Expr, bool)> {
        match self.peek()? {
            '.' => {
                if self.starts_with("..") {
                    return None;
                }
                self.at += 1;
                match self.peek() {
                    Some(ch) if ch.is_ascii_alphabetic() || ch == '_' => {
                        Some((Expr::Field(Box::new(Expr::Identity), self.ident()?), true))
                    }
                    Some('"') => None,
                    _ => Some((Expr::Identity, true)),
                }
            }
            '(' => {
                self.at += 1;
                let inner = self.pipe()?;
                self.eat(")").then_some((inner, true))
            }
            '[' => {
                self.at += 1;
                self.skip_space();
                if self.peek() == Some(']') {
                    self.at += 1;
                    return Some((Expr::Array(None), false));
                }
                let inner = self.pipe()?;
                self.eat("]")
                    .then(|| (Expr::Array(Some(Box::new(inner))), false))
            }
            '{' => {
                self.at += 1;
                self.object().map(|object| (object, false))
            }
            '"' => self
                .string()
                .map(|text| (Expr::Literal(Value::String(text)), false)),
            ch if ch.is_ascii_digit() => Some((Expr::Literal(Value::from(self.digits()?)), false)),
            ch if ch.is_ascii_alphabetic() => self.word(),
            _ => None,
        }
    }

    fn word(&mut self) -> Option<(Expr, bool)> {
        if self.eat_word("null") {
            return Some((Expr::Literal(Value::Null), false));
        }
        if self.eat_word("true") {
            return Some((Expr::Literal(Value::Bool(true)), false));
        }
        if self.eat_word("false") {
            return Some((Expr::Literal(Value::Bool(false)), false));
        }
        if self.eat_word("length") {
            return Some((Expr::Length, false));
        }
        if self.eat_word("select") {
            if !self.eat("(") {
                return None;
            }
            let inner = self.pipe()?;
            return self
                .eat(")")
                .then(|| (Expr::Select(Box::new(inner)), false));
        }
        if self.eat_word("if") {
            let condition = self.pipe()?;
            if !self.eat_word("then") {
                return None;
            }
            let then = self.pipe()?;
            if !self.eat_word("else") {
                return None;
            }
            let otherwise = self.pipe()?;
            if !self.eat_word("end") {
                return None;
            }
            return Some((
                Expr::If {
                    condition: Box::new(condition),
                    then: Box::new(then),
                    otherwise: Box::new(otherwise),
                },
                false,
            ));
        }
        None
    }

    /// A string literal with no interpolation; only `\"` and `\\` escape.
    fn string(&mut self) -> Option<String> {
        self.at += 1;
        let mut text = String::new();
        loop {
            let ch = self.peek()?;
            self.at += 1;
            match ch {
                '"' => return Some(text),
                '\\' => {
                    let escaped = self.peek()?;
                    self.at += 1;
                    if !matches!(escaped, '"' | '\\') {
                        return None;
                    }
                    text.push(escaped);
                }
                ch if ch.is_control() => return None,
                ch => text.push(ch),
            }
        }
    }

    fn object(&mut self) -> Option<Expr> {
        let mut entries = Vec::new();
        self.skip_space();
        if self.peek() == Some('}') {
            self.at += 1;
            return Some(Expr::Object(entries));
        }
        loop {
            self.skip_space();
            let key = if self.peek() == Some('"') {
                self.string()?
            } else {
                self.ident()?
            };
            let value = if self.eat(":") {
                self.object_value()?
            } else {
                Expr::Field(Box::new(Expr::Identity), key.clone())
            };
            entries.push((key, value));
            if self.eat(",") {
                continue;
            }
            return self.eat("}").then_some(Expr::Object(entries));
        }
    }

    /// jq allows only terms joined by pipes as an unparenthesised value.
    fn object_value(&mut self) -> Option<Expr> {
        self.enter()?;
        let mut left = self.term()?;
        loop {
            self.skip_space();
            if self.starts_with("|") && !self.starts_with("||") {
                self.at += 1;
                let right = self.term()?;
                left = Expr::Pipe(Box::new(left), Box::new(right));
            } else {
                break;
            }
        }
        self.depth -= 1;
        Some(left)
    }
}

fn truthy(value: &Value) -> bool {
    !matches!(value, Value::Null | Value::Bool(false))
}

/// Deep equality, refusing floats so `1` and `1.0` never need a ruling.
fn same(left: &Value, right: &Value) -> Result<bool, Unsupported> {
    Ok(match (left, right) {
        (Value::Number(a), Value::Number(b)) => {
            if a.is_f64() || b.is_f64() {
                return Err(Unsupported);
            }
            a == b
        }
        (Value::Array(a), Value::Array(b)) => {
            if a.len() != b.len() {
                return Ok(false);
            }
            for (x, y) in a.iter().zip(b) {
                if !same(x, y)? {
                    return Ok(false);
                }
            }
            true
        }
        (Value::Object(a), Value::Object(b)) => {
            if a.len() != b.len() {
                return Ok(false);
            }
            for (key, x) in a {
                match b.get(key) {
                    Some(y) if same(x, y)? => {}
                    _ => return Ok(false),
                }
            }
            true
        }
        _ => left == right,
    })
}

fn single(mut outputs: Vec<Value>) -> Result<Value, Fail> {
    if outputs.len() == 1 {
        Ok(outputs.remove(0))
    } else {
        Err(Fail::Unsupported)
    }
}

fn eval(expr: &Expr, input: &Value) -> Result<Vec<Value>, Fail> {
    Ok(match expr {
        Expr::Identity => vec![input.clone()],
        Expr::Literal(value) => vec![value.clone()],
        Expr::Field(base, name) => {
            let mut out = Vec::new();
            for value in eval(base, input)? {
                out.push(match value {
                    Value::Object(map) => map.get(name).cloned().unwrap_or(Value::Null),
                    Value::Null => Value::Null,
                    _ => return Err(Fail::Jq),
                });
            }
            out
        }
        Expr::Element(base, index) => {
            let mut out = Vec::new();
            for value in eval(base, input)? {
                out.push(match value {
                    Value::Array(items) => items.get(*index).cloned().unwrap_or(Value::Null),
                    Value::Null => Value::Null,
                    _ => return Err(Fail::Jq),
                });
            }
            out
        }
        Expr::Iterate(base) => {
            let mut out = Vec::new();
            for value in eval(base, input)? {
                match value {
                    Value::Array(items) => out.extend(items),
                    Value::Object(_) => return Err(Fail::Unsupported),
                    _ => return Err(Fail::Jq),
                }
            }
            out
        }
        Expr::Pipe(left, right) => {
            let mut out = Vec::new();
            for value in eval(left, input)? {
                out.extend(eval(right, &value)?);
            }
            out
        }
        Expr::Alternative(left, right) => {
            // An error on the left is not an answer: jq tries the right side.
            let found = match eval(left, input) {
                Ok(found) => found,
                Err(Fail::Jq) => Vec::new(),
                Err(Fail::Unsupported) => return Err(Fail::Unsupported),
            }
            .into_iter()
            .filter(truthy)
            .collect::<Vec<_>>();
            if found.is_empty() {
                eval(right, input)?
            } else {
                found
            }
        }
        other => eval_rest(other, input)?,
    })
}

/// The expression forms that build or test values rather than walk paths.
fn eval_rest(expr: &Expr, input: &Value) -> Result<Vec<Value>, Fail> {
    Ok(match expr {
        Expr::Equals {
            left,
            right,
            negate,
        } => {
            let left = single(eval(left, input)?)?;
            let right = single(eval(right, input)?)?;
            vec![Value::Bool(same(&left, &right)? != *negate)]
        }
        Expr::If {
            condition,
            then,
            otherwise,
        } => {
            let mut out = Vec::new();
            for value in eval(condition, input)? {
                out.extend(eval(if truthy(&value) { then } else { otherwise }, input)?);
            }
            out
        }
        Expr::Object(entries) => {
            let mut map = Map::new();
            for (key, value) in entries {
                map.insert(key.clone(), single(eval(value, input)?)?);
            }
            vec![Value::Object(map)]
        }
        Expr::Array(inner) => {
            let items = match inner {
                Some(inner) => eval(inner, input)?,
                None => Vec::new(),
            };
            vec![Value::Array(items)]
        }
        Expr::Length => vec![match input {
            Value::Null => Value::from(0_u64),
            Value::String(text) => Value::from(text.chars().count() as u64),
            Value::Array(items) => Value::from(items.len() as u64),
            Value::Object(map) => Value::from(map.len() as u64),
            Value::Bool(_) | Value::Number(_) => return Err(Fail::Jq),
        }],
        Expr::Select(condition) => {
            let mut out = Vec::new();
            for value in eval(condition, input)? {
                if truthy(&value) {
                    out.push(input.clone());
                }
            }
            out
        }
        Expr::Identity
        | Expr::Literal(_)
        | Expr::Field(..)
        | Expr::Element(..)
        | Expr::Iterate(_)
        | Expr::Pipe(..)
        | Expr::Alternative(..) => eval(expr, input)?,
    })
}

/// Print one result the way `gh api --jq` does, newline included.
fn print(value: &Value, out: &mut Vec<u8>) -> Result<(), Unsupported> {
    match value {
        Value::String(text) => out.extend_from_slice(text.as_bytes()),
        Value::Null => {}
        Value::Bool(flag) => out.extend_from_slice(if *flag { b"true" } else { b"false" }),
        Value::Number(_) | Value::Array(_) | Value::Object(_) => encode(value, out)?,
    }
    out.push(b'\n');
    Ok(())
}

/// Go's `encoding/json` as `gh` uses it: compact, keys sorted, `<`, `>`, `&`
/// and U+2028/9 escaped. A float, a number gh would round, and a control
/// character other than tab, newline and return are refused.
fn encode(value: &Value, out: &mut Vec<u8>) -> Result<(), Unsupported> {
    match value {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(flag) => out.extend_from_slice(if *flag { b"true" } else { b"false" }),
        Value::Number(number) => {
            if let Some(n) = number.as_u64() {
                if n > MAX_SAFE_INTEGER {
                    return Err(Unsupported);
                }
                out.extend_from_slice(n.to_string().as_bytes());
            } else if let Some(n) = number.as_i64() {
                if n.unsigned_abs() > MAX_SAFE_INTEGER {
                    return Err(Unsupported);
                }
                out.extend_from_slice(n.to_string().as_bytes());
            } else {
                return Err(Unsupported);
            }
        }
        Value::String(text) => encode_string(text, out)?,
        Value::Array(items) => {
            out.push(b'[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                encode(item, out)?;
            }
            out.push(b']');
        }
        Value::Object(map) => {
            let mut keys = map.keys().collect::<Vec<_>>();
            keys.sort();
            out.push(b'{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                encode_string(key, out)?;
                out.push(b':');
                encode(&map[key], out)?;
            }
            out.push(b'}');
        }
    }
    Ok(())
}

fn encode_string(text: &str, out: &mut Vec<u8>) -> Result<(), Unsupported> {
    out.push(b'"');
    for ch in text.chars() {
        match ch {
            '"' => out.extend_from_slice(b"\\\""),
            '\\' => out.extend_from_slice(b"\\\\"),
            '\n' => out.extend_from_slice(b"\\n"),
            '\r' => out.extend_from_slice(b"\\r"),
            '\t' => out.extend_from_slice(b"\\t"),
            '<' => out.extend_from_slice(b"\\u003c"),
            '>' => out.extend_from_slice(b"\\u003e"),
            '&' => out.extend_from_slice(b"\\u0026"),
            '\u{2028}' => out.extend_from_slice(b"\\u2028"),
            '\u{2029}' => out.extend_from_slice(b"\\u2029"),
            ch if (ch as u32) < 0x20 => return Err(Unsupported),
            ch => {
                let mut buffer = [0_u8; 4];
                out.extend_from_slice(ch.encode_utf8(&mut buffer).as_bytes());
            }
        }
    }
    out.push(b'"');
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{Program, Unsupported};

    fn run(filter: &str, page: &serde_json::Value) -> Result<String, Unsupported> {
        let program = Program::parse(filter).expect("filter parses");
        let mut out = Vec::new();
        program.run(page, &mut out)?;
        Ok(String::from_utf8(out).expect("utf8"))
    }

    /// Every filter the steward's read paths build today.
    const STEWARD_FILTERS: &[&str] = &[
        ".[].number",
        ".[].ref",
        ".[].sha",
        ".sha",
        ".draft",
        ".behind_by",
        ".status",
        ".head.repo.full_name",
        r#"if .merged then "merged" else .state end"#,
        ".[] | {parents: (.parents | length), message: .commit.message, email: \
         .commit.committer.email, date: .commit.committer.date}",
        "{parents: [.parents[].sha], committer_email: .commit.committer.email, message: \
         .commit.message}",
        "{sha: .object.sha, kind: .object.type}",
        "{truncated: .truncated, tree: [.tree[] | {name: .path, mode: .mode, kind: .type, sha: \
         .sha}]}",
        "{total: .total_commits, merge_base: .merge_base_commit.sha, shas: [.commits[].sha]}",
        "{total: .total_commits, merge_base: .merge_base_commit.sha, commits: [.commits[] | \
         {sha, parents: [.parents[].sha]}]}",
        ".[] | {login: .user.login, state: .state, commit_id: .commit_id, submitted_at: \
         .submitted_at}",
        ".[] | {login: .user.login, body: .body, created_at: .created_at}",
        ".[] | {event: .event, label: .label.name, login: .actor.login, at: .created_at}",
        ".[] | {id: .id, node_id: .node_id, login: .user.login, type: .user.type, association: \
         .author_association, at: .created_at, updated_at: .updated_at, body: .body, \
         performed_via_github_app_id: .performed_via_github_app.id}",
        "[.workflow_runs[] | {id, pr: (.pull_requests[0].number // null), branch: .head_branch}] \
         | .[]",
        r#".[] | select(.event == "labeled") | .created_at"#,
    ];

    #[test]
    fn every_steward_filter_parses() {
        for filter in STEWARD_FILTERS {
            assert!(Program::parse(filter).is_some(), "must parse: {filter}");
        }
    }

    #[test]
    fn filters_outside_the_subset_do_not_parse() {
        for filter in [
            ".[] | [.name, .sha] | @tsv",
            r#".[] | select((.body // "") | ascii_downcase | startswith("x"))"#,
            r#"[.jobs[] | select(.name | test("macos"; "i")) | .labels] | flatten | join(",")"#,
            ".[] | tojson",
            r#""x \(.a)""#,
            ".a // .b // ",
            ".. | .a",
            ".a?",
            ".[1:2]",
            ".[-1]",
            ".5",
            "1.5",
            ".a and .b",
            "{a: .x // 1}",
            "{(.k): 1}",
            "$x",
            "",
            "|",
            ".a |",
            ".\"a\"",
            "def f: .; f",
        ] {
            assert!(Program::parse(filter).is_none(), "must decline: {filter}");
        }
        let deep = format!("{}1{}", "(".repeat(200), ")".repeat(200));
        assert!(Program::parse(&deep).is_none(), "deep nesting is declined");
    }

    #[test]
    fn results_print_the_way_gh_prints_them() {
        let page = json!([
            {"sha": "a", "n": 1, "ok": true, "none": null, "text": "x<y>&z\u{2028}é\"\\\n\t", "o": {"b": 1, "a": 2}},
            {"sha": "b", "n": 0, "ok": false, "none": null, "text": "", "o": {}}
        ]);
        // Scalars: a string raw, null empty, others as text.
        assert_eq!(run(".[].sha", &page).unwrap(), "a\nb\n");
        assert_eq!(run(".[].n", &page).unwrap(), "1\n0\n");
        assert_eq!(run(".[].ok", &page).unwrap(), "true\nfalse\n");
        assert_eq!(run(".[].none", &page).unwrap(), "\n\n");
        assert_eq!(run(".[0].nosuch", &page).unwrap(), "\n");
        assert_eq!(run(".[5]", &page).unwrap(), "\n");
        assert_eq!(
            run(".[0].text", &page).unwrap(),
            "x<y>&z\u{2028}é\"\\\n\t\n"
        );
        // Compound values: sorted keys, Go's HTML-safe escaping.
        assert_eq!(
            run(".[0] | {text, o, none, n}", &page).unwrap(),
            "{\"n\":1,\"none\":null,\"o\":{\"a\":2,\"b\":1},\"text\":\"x\\u003cy\\u003e\\u0026z\\u2028é\\\"\\\\\\n\\t\"}\n"
        );
        assert_eq!(run("[.[].none]", &page).unwrap(), "[null,null]\n");
        assert_eq!(run("[.[].n]", &page).unwrap(), "[1,0]\n");
        assert_eq!(run("{}", &page).unwrap(), "{}\n");
        assert_eq!(run("[]", &page).unwrap(), "[]\n");
    }

    #[test]
    fn jq_semantics_match_for_the_supported_forms() {
        let page = json!({"merged": false, "state": "open", "list": [3, 4], "name": "héé"});
        let eval = |filter: &str| run(filter, &page).unwrap();
        assert_eq!(
            eval(r#"if .merged then "merged" else .state end"#),
            "open\n"
        );
        assert_eq!(eval(".nope // \"d\""), "d\n");
        assert_eq!(eval(".nope.deeper // null"), "\n");
        assert_eq!(eval(".name | length"), "3\n");
        assert_eq!(eval(".list | length"), "2\n");
        assert_eq!(eval(".nope | length"), "0\n");
        assert_eq!(eval(".list[] | select(. == 4)"), "4\n");
        assert_eq!(eval(".list[] | select(. != 4)"), "3\n");
        assert_eq!(eval(".list[1]"), "4\n");
        // `//` takes the right side for null and false, and for an error.
        assert_eq!(eval(".merged // \"f\""), "f\n");
        assert_eq!(eval(".name.x // \"err\""), "err\n");
        // A value that yields nothing or several results is not an object.
        assert!(run("{a: .list[]}", &page).is_err());
        assert!(run("{a: select(.merged)}", &page).is_err());
    }

    #[test]
    fn inputs_gh_could_print_differently_are_refused() {
        let page = json!({"f": 1.5, "g": 1.0, "big": 9_007_199_254_740_993_u64,
            "ctl": "a\u{8}b", "obj": {"x": 1}, "s": "t"});
        for filter in [
            ".f",
            "{a: .f}",
            "[.g]",
            "{a: .big}",
            "{a: .ctl}",
            ".s.x",
            ".s[0]",
            ".obj[]",
            ".s | length | length",
        ] {
            assert!(run(filter, &page).is_err(), "must refuse: {filter}");
        }
        // Comparing floats is not claimed either, and is not swallowed by `//`.
        assert!(Program::parse(".f == 1.5").is_none());
        assert!(run(".f == .g // 1", &page).is_err());
        // A raw control character is fine as a scalar: gh prints the bytes.
        assert_eq!(run(".ctl", &page).unwrap(), "a\u{8}b\n");
    }
}
