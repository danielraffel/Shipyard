use std::cell::RefCell;
use std::collections::BTreeMap;

use chrono::TimeZone;

use super::*;

fn at(minute: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 9, 12, minute, 0).unwrap()
}

fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: BTreeMap<String, String> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    move |name: &str| map.get(name).cloned()
}

fn record() -> OwnerRecord {
    SessionIdentity::from_env(
        "m5".to_owned(),
        &env_of(&[
            ("CODEX_THREAD_ID", "thread-1"),
            ("CMUX_SURFACE_ID", "654D7FD3-5762-47E1-9E06-3E6ED569FB76"),
            ("CMUX_SOCKET_PATH", "/tmp/cmux-nightly.sock"),
            ("SHIPYARD_AGENT_ID", "audio-tools"),
        ]),
    )
    .record("Generous-Corp/pulp", 11796, Source::Ship, at(5))
}

#[test]
fn the_session_is_read_from_the_client_environment() {
    let claude = SessionIdentity::from_env(
        "m3".to_owned(),
        &env_of(&[
            ("CLAUDE_CODE_SESSION_ID", "claude-1"),
            // A Claude session can also carry the Codex companion's id.
            ("CODEX_COMPANION_SESSION_ID", "companion-1"),
            ("CMUX_SURFACE_ID", "S1"),
        ]),
    );
    assert_eq!(claude.client, Some(Client::ClaudeCode));
    assert_eq!(claude.session.as_deref(), Some("claude-1"));
    assert!(claude.addressable());

    let codex = SessionIdentity::from_env("m5".to_owned(), &env_of(&[("CODEX_THREAD_ID", "t-9")]));
    assert_eq!(codex.client, Some(Client::Codex));
    assert!(
        !codex.addressable(),
        "no cmux surface, nothing to deliver to"
    );

    let scheduled = SessionIdentity::from_env(
        "m3".to_owned(),
        &env_of(&[
            ("CLAUDE_CODE_SESSION_ID", "shared"),
            ("SHIPYARD_STAMP_SESSION_ID", "run-42"),
        ]),
    );
    assert_eq!(scheduled.session.as_deref(), Some("run-42"));
    assert_eq!(
        SessionIdentity::from_env("m1".to_owned(), &env_of(&[]))
            .record("o/r", 1, Source::Manual, at(0))
            .client,
        Client::Other
    );
}

#[test]
fn the_host_name_prefers_the_environment_then_the_config() {
    assert_eq!(
        resolve_host_name(Some("m3"), &env_of(&[("SHIPYARD_HOST_NAME", "m5s")])),
        "m5s"
    );
    assert_eq!(resolve_host_name(Some(" m3 "), &env_of(&[])), "m3");
    assert!(!resolve_host_name(None, &env_of(&[])).is_empty());
}

#[test]
fn a_marker_round_trips_through_a_pull_request_body() {
    let record = record();
    let marker = render_marker(&record);
    assert!(marker.starts_with(MARKER_PREFIX) && marker.ends_with("-->"));
    assert!(!marker.contains('\n'));
    let body = format!("## Summary\n\nText.\n\n{marker}\n");
    let parsed = parse_marker("Generous-Corp/pulp", 11796, &body).expect("parses");
    assert_eq!(parsed.host, "m5");
    assert_eq!(parsed.client, Client::Codex);
    assert_eq!(parsed.surface, record.surface);
    assert_eq!(parsed.cmux_socket, record.cmux_socket);
    assert_eq!(parsed.session.as_deref(), Some("thread-1"));
    assert_eq!(parsed.agent.as_deref(), Some("audio-tools"));
    assert_eq!(parsed.registered_at, record.registered_at);
    assert_eq!(parsed.source, Source::BodyMarker);
    assert!(!parsed.released);
}

#[test]
fn an_unsafe_value_never_reaches_the_marker() {
    let mut record = record();
    record.surface = Some("x --> <script>".to_owned());
    record.session = Some("a b".to_owned());
    let marker = render_marker(&record);
    assert!(!marker.contains("script"));
    assert!(!marker.contains("session="));
    assert_eq!(marker.matches("-->").count(), 1, "only the closing one");
    let parsed = parse_marker("o/r", 1, &marker).expect("still parses");
    assert_eq!(parsed.surface, None);
    // A forged field with shell syntax in a body is dropped on read too.
    let forged = "<!-- shipyard-owner: host=m3 surface=$(rm) -->";
    assert_eq!(
        parse_marker("o/r", 1, forged).expect("host parses").surface,
        None
    );
    assert!(!safe_value("a;b") && !safe_value("") && safe_value("surface:1000130019"));
}

#[test]
fn the_last_marker_in_a_body_wins() {
    let body = "<!-- shipyard-owner: host=m1 client=codex -->\nquoted text\n<!-- shipyard-owner: host=m5 client=claude-code -->";
    assert_eq!(parse_marker("o/r", 1, body).expect("parses").host, "m5");
    assert_eq!(parse_marker("o/r", 1, "no marker here"), None);
}

#[test]
fn upserting_a_marker_appends_replaces_or_leaves_the_body_alone() {
    let marker = render_marker(&record());
    let appended = upsert_marker("Body text\n", &marker).expect("appended");
    assert_eq!(appended, format!("Body text\n\n{marker}"));
    assert_eq!(upsert_marker(&appended, &marker), None, "already current");
    assert_eq!(upsert_marker("", &marker).expect("empty body"), marker);

    let old = "<!-- shipyard-owner: host=m1 client=codex -->";
    let replaced = upsert_marker(&format!("A\n{old}\nB\n{old}"), &marker).expect("replaced");
    assert_eq!(replaced, format!("A\n{marker}\nB"), "one marker, in place");
}

#[test]
fn record_paths_refuse_anything_but_owner_slash_repo() {
    let dir = Path::new("/state");
    assert_eq!(
        record_path(dir, "Generous-Corp/pulp", 7),
        Some(PathBuf::from("/state/owners/Generous-Corp/pulp/7.json"))
    );
    assert_eq!(record_path(dir, "../etc", 7), None);
    assert_eq!(record_path(dir, "a/../b", 7), None);
    assert_eq!(record_path(dir, "a/b/c", 7), None);
    assert_eq!(record_path(dir, "noslash", 7), None);
}

#[test]
fn records_are_stored_and_read_back_per_repository() {
    let temp = tempfile::tempdir().expect("temp");
    let first = record();
    let mut second = first.clone();
    second.pr = 11830;
    let mut other = first.clone();
    other.repo = "Generous-Corp/vellum".to_owned();
    other.pr = 255;
    for record in [&first, &second, &other] {
        save(temp.path(), record).expect("save");
    }
    assert_eq!(
        load(temp.path(), "Generous-Corp/pulp", 11796),
        Some(first.clone())
    );
    assert_eq!(load(temp.path(), "Generous-Corp/pulp", 1), None);
    let pulp = load_repo(temp.path(), "Generous-Corp/pulp");
    assert_eq!(
        pulp.iter().map(|r| r.pr).collect::<Vec<_>>(),
        [11796, 11830]
    );
    assert_eq!(load_all(temp.path()).len(), 3);
}

#[test]
fn the_newest_record_wins_and_a_tie_keeps_the_current_one() {
    let older = record();
    let mut newer = older.clone();
    newer.registered_at = at(30);
    newer.host = "m3".to_owned();
    assert_eq!(newest(Some(older.clone()), newer.clone()).host, "m3");
    assert_eq!(newest(Some(newer.clone()), older.clone()).host, "m3");
    let mut tie = older.clone();
    tie.host = "m1".to_owned();
    assert_eq!(newest(Some(older.clone()), tie).host, "m5");
    assert_eq!(newest(None, older).host, "m5");
}

/// A scripted `gh`: answers in order, recording every argv.
struct FakeGh {
    answers: RefCell<Vec<Result<String, String>>>,
    calls: RefCell<Vec<Vec<String>>>,
}

impl FakeGh {
    fn new(answers: Vec<Result<String, String>>) -> Self {
        Self {
            answers: RefCell::new(answers),
            calls: RefCell::new(Vec::new()),
        }
    }

    fn run(&self, args: &[String]) -> Result<String, String> {
        self.calls.borrow_mut().push(args.to_vec());
        let mut answers = self.answers.borrow_mut();
        if answers.is_empty() {
            return Err("no scripted answer".to_owned());
        }
        answers.remove(0)
    }
}

fn pr_answer(body: &str) -> String {
    serde_json::json!({"data":{"repository":{"pullRequest":{
        "id":"PR_node","number":11796,"state":"OPEN","body":body}}}})
    .to_string()
}

#[test]
fn registering_stores_the_record_and_writes_the_marker_over_graphql() {
    let temp = tempfile::tempdir().expect("temp");
    let gh = FakeGh::new(vec![Ok(pr_answer("Summary")), Ok("{}".to_owned())]);
    let run = |args: &[String]| gh.run(args);
    let pr = read_pr_body(&run, "Generous-Corp/pulp", 11796).expect("read");
    let registration = register(&run, temp.path(), &pr, record());
    assert!(registration.stored_at.is_ok());
    assert_eq!(registration.marker, Ok(MarkerWrite::Written));
    assert_eq!(
        load(temp.path(), "Generous-Corp/pulp", 11796),
        Some(record())
    );
    let calls = gh.calls.borrow();
    assert_eq!(calls.len(), 2);
    // Both are GraphQL: the registry spends no REST budget.
    assert!(calls.iter().all(|c| c[0] == "api" && c[1] == "graphql"));
    assert!(calls[0].iter().any(|a| a == "number=11796"));
    assert!(calls[1][3].contains("updatePullRequest"));
    let body = calls[1]
        .iter()
        .find(|a| a.starts_with("body="))
        .expect("body");
    assert!(body.starts_with("body=Summary\n\n<!-- shipyard-owner: host=m5 "));
    assert!(registration.line().contains("body marker written"));
}

#[test]
fn a_current_marker_is_not_rewritten_and_a_failed_write_still_stores() {
    let temp = tempfile::tempdir().expect("temp");
    let marker = render_marker(&record());
    let gh = FakeGh::new(vec![Ok(pr_answer(&marker))]);
    let run = |args: &[String]| gh.run(args);
    let pr = read_pr_body(&run, "Generous-Corp/pulp", 11796).expect("read");
    assert_eq!(
        register(&run, temp.path(), &pr, record()).marker,
        Ok(MarkerWrite::Unchanged)
    );
    assert_eq!(gh.calls.borrow().len(), 1, "no write for an unchanged body");

    let failing = FakeGh::new(vec![Err("HTTP 502".to_owned())]);
    let run = |args: &[String]| failing.run(args);
    let pr = PrBody {
        id: "PR_node".to_owned(),
        number: 11796,
        state: "OPEN".to_owned(),
        body: String::new(),
    };
    let registration = register(&run, temp.path(), &pr, record());
    assert!(
        registration.stored_at.is_ok(),
        "the durable half still happens"
    );
    assert!(registration.marker.is_err());
    assert!(
        registration
            .line()
            .contains("body marker NOT written (HTTP 502)")
    );
}

#[test]
fn the_pull_request_for_a_branch_must_be_unique() {
    let none = serde_json::json!({"data":{"repository":{"pullRequests":{"nodes":[]}}}}).to_string();
    let two = serde_json::json!({"data":{"repository":{"pullRequests":{"nodes":[
        {"id":"a","number":1,"state":"OPEN","body":""},
        {"id":"b","number":2,"state":"OPEN","body":""}]}}}})
    .to_string();
    let one = serde_json::json!({"data":{"repository":{"pullRequests":{"nodes":[
        {"id":"a","number":1,"state":"OPEN","body":"x"}]}}}})
    .to_string();
    let gh = FakeGh::new(vec![Ok(none), Ok(two), Ok(one)]);
    let run = |args: &[String]| gh.run(args);
    assert_eq!(find_pr_for_branch(&run, "o/r", "feat/x"), Ok(None));
    assert!(find_pr_for_branch(&run, "o/r", "feat/x").is_err());
    assert_eq!(
        find_pr_for_branch(&run, "o/r", "feat/x")
            .expect("one")
            .map(|p| p.number),
        Some(1)
    );
    assert!(gh.calls.borrow()[0].iter().any(|a| a == "branch=feat/x"));
}
