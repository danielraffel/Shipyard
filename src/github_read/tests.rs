//! In-process reads against a local mock GitHub.
//!
//! The mock speaks HTTP/1.1 with keep-alive on a loopback port, verifies every
//! App JWT against the public half of a throwaway test key, and records each
//! request, so the tests can assert what was sent as well as what came back.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{Value, json};

use super::argv::{self, ReadRequest};
use super::batch::{self, BatchSizer};
use super::credential::AppCredential;
use super::pr_view::{self, PrField};
use super::{FailureKind, InprocReads, ReadOutcome};

/// A throwaway RSA key generated for these tests only. It authorizes nothing.
const TEST_KEY_DER_HEX: &str = include_str!("test_key.der.hex");
const APP_ID: u64 = 4242;
const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

fn test_key_der() -> Vec<u8> {
    let hex = TEST_KEY_DER_HEX
        .chars()
        .filter(char::is_ascii_hexdigit)
        .collect::<String>();
    hex::decode(hex).expect("test key hex")
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_owned()).collect()
}

/// A private directory holding the test key as a mode-0600 PKCS#1 PEM.
struct KeyDir {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

fn write_key() -> KeyDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("app.pem");
    let body = base64::engine::general_purpose::STANDARD.encode(test_key_der());
    let mut pem = String::from("-----BEGIN RSA PRIVATE KEY-----\n");
    for chunk in body.as_bytes().chunks(64) {
        pem.push_str(std::str::from_utf8(chunk).expect("ascii"));
        pem.push('\n');
    }
    pem.push_str("-----END RSA PRIVATE KEY-----\n");
    std::fs::write(&path, pem).expect("write key");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
            .expect("dir mode");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("key mode");
    }
    KeyDir { _dir: dir, path }
}

fn credential(key: &Path, scope_to_repository: bool) -> AppCredential {
    AppCredential {
        app_id: APP_ID,
        key_path: key.to_path_buf(),
        scope_to_repository,
    }
}

#[derive(Clone, Debug)]
struct Recorded {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Recorded {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    delay: Duration,
}

impl Reply {
    fn json(status: u16, body: &Value) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: body.to_string().into_bytes(),
            delay: Duration::ZERO,
        }
    }

    fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    fn delayed(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
}

type Handler = dyn Fn(&Recorded) -> Reply + Send + Sync;

struct Mock {
    url: String,
    requests: Arc<Mutex<Vec<Recorded>>>,
    connections: Arc<AtomicUsize>,
}

impl Mock {
    fn start(handler: impl Fn(&Recorded) -> Reply + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(AtomicUsize::new(0));
        let handler: Arc<Handler> = Arc::new(handler);
        let (requests_for_thread, connections_for_thread) =
            (Arc::clone(&requests), Arc::clone(&connections));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                connections_for_thread.fetch_add(1, Ordering::SeqCst);
                let handler = Arc::clone(&handler);
                let requests = Arc::clone(&requests_for_thread);
                std::thread::spawn(move || serve(stream, &*handler, &requests));
            }
        });
        Self {
            url,
            requests,
            connections,
        }
    }

    fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().expect("requests").clone()
    }

    fn count(&self, method: &str, path_prefix: &str) -> usize {
        self.requests()
            .iter()
            .filter(|request| request.method == method && request.path.starts_with(path_prefix))
            .count()
    }
}

fn serve(stream: TcpStream, handler: &Handler, requests: &Mutex<Vec<Recorded>>) {
    let mut writer = stream.try_clone().expect("clone");
    let mut reader = BufReader::new(stream);
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let mut parts = line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_owned();
        let path = parts
            .next()
            .unwrap_or_default()
            .trim_start_matches('/')
            .to_owned();
        let mut headers = Vec::new();
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).unwrap_or(0) == 0 {
                return;
            }
            let header = header.trim_end();
            if header.is_empty() {
                break;
            }
            if let Some((name, value)) = header.split_once(':') {
                headers.push((name.trim().to_owned(), value.trim().to_owned()));
            }
        }
        let length = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.parse::<usize>().ok())
            .unwrap_or(0);
        let mut body = vec![0; length];
        if reader.read_exact(&mut body).is_err() {
            return;
        }
        let recorded = Recorded {
            method,
            path,
            headers,
            body,
        };
        requests.lock().expect("requests").push(recorded.clone());
        let reply = handler(&recorded);
        std::thread::sleep(reply.delay);
        let mut head = format!(
            "HTTP/1.1 {} X\r\nContent-Length: {}\r\nContent-Type: application/json\r\n",
            reply.status,
            reply.body.len()
        );
        for (name, value) in &reply.headers {
            let _ = write!(head, "{name}: {value}\r\n");
        }
        head.push_str("\r\n");
        if writer.write_all(head.as_bytes()).is_err() || writer.write_all(&reply.body).is_err() {
            return;
        }
    }
}

/// Check an App JWT the way GitHub would: RS256 over the test key, issued
/// for [`APP_ID`], valid for at most ten minutes around `now`.
fn assert_valid_app_jwt(authorization: &str, now: i64) {
    let jwt = authorization
        .strip_prefix("Bearer ")
        .expect("App requests carry a bearer JWT");
    let mut parts = jwt.split('.');
    let (header, claims, signature) = (
        parts.next().expect("header"),
        parts.next().expect("claims"),
        parts.next().expect("signature"),
    );
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let key = ring::signature::RsaKeyPair::from_der(&test_key_der()).expect("test key");
    let public = ring::signature::UnparsedPublicKey::new(
        &ring::signature::RSA_PKCS1_2048_8192_SHA256,
        ring::signature::KeyPair::public_key(&key).as_ref().to_vec(),
    );
    public
        .verify(
            format!("{header}.{claims}").as_bytes(),
            &engine.decode(signature).expect("signature b64"),
        )
        .expect("JWT signature verifies against the App key");
    let header: Value = serde_json::from_slice(&engine.decode(header).expect("b64")).unwrap();
    assert_eq!(header, json!({"alg": "RS256", "typ": "JWT"}));
    let claims: Value = serde_json::from_slice(&engine.decode(claims).expect("b64")).unwrap();
    assert_eq!(claims["iss"], json!(APP_ID.to_string()));
    let (iat, exp) = (
        claims["iat"].as_i64().expect("iat"),
        claims["exp"].as_i64().expect("exp"),
    );
    assert_eq!(iat, now - 60);
    assert!(exp - iat <= 600, "{claims}");
}

/// A GitHub that answers installation lookups and mints sequential tokens
/// (`ghs_token1`, `ghs_token2`, ...) valid for an hour from `clock`, then
/// hands every other request to `reads`.
fn github(
    clock: Arc<AtomicI64>,
    reads: impl Fn(&Recorded) -> Reply + Send + Sync + 'static,
) -> Mock {
    let minted = Arc::new(AtomicUsize::new(0));
    Mock::start(move |request| {
        let now = clock.load(Ordering::SeqCst);
        if request.path.ends_with("/installation") {
            assert_valid_app_jwt(request.header("authorization").unwrap_or_default(), now);
            return Reply::json(200, &json!({"id": 77}));
        }
        if request.path == "app/installations/77/access_tokens" {
            assert_eq!(request.method, "POST");
            assert_valid_app_jwt(request.header("authorization").unwrap_or_default(), now);
            let number = minted.fetch_add(1, Ordering::SeqCst) + 1;
            let expires = chrono::DateTime::from_timestamp(now + 3600, 0)
                .expect("time")
                .to_rfc3339();
            return Reply::json(
                201,
                &json!({"token": format!("ghs_token{number}"), "expires_at": expires}),
            );
        }
        reads(request)
    })
}

fn reader(mock: &Mock, key: &Path, clock: &Arc<AtomicI64>) -> InprocReads {
    let clock = Arc::clone(clock);
    InprocReads::for_tests(
        credential(key, false),
        &mock.url,
        Arc::new(move || clock.load(Ordering::SeqCst)),
        None,
    )
}

fn now_clock() -> Arc<AtomicI64> {
    Arc::new(AtomicI64::new(chrono::Utc::now().timestamp()))
}

fn answered(outcome: ReadOutcome) -> (bool, String, String, super::CallMeta) {
    match outcome {
        ReadOutcome::Answered {
            success,
            stdout,
            stderr,
            meta,
        } => (
            success,
            String::from_utf8(stdout).expect("utf8"),
            stderr,
            meta,
        ),
        other => panic!("expected an answer, got {other:?}"),
    }
}

const STATUSES: &str =
    "repos/o/r/commits/0123456789abcdef0123456789abcdef01234567/statuses?per_page=100&page=1";

// ---------------------------------------------------------------- classify

#[test]
fn classifier_accepts_the_steward_read_shapes() {
    let rest = argv::classify(&argv(&["api", STATUSES]), Some("o/r")).expect("REST read");
    assert_eq!(rest.repo, "o/r");
    assert_eq!(
        rest.request,
        ReadRequest::Rest {
            path: STATUSES.to_owned()
        }
    );

    let graphql = argv::classify(
        &argv(&[
            "api",
            "graphql",
            "-f",
            "query=query($owner:String!,$n:Int){repository(owner:$owner){id}}",
            "-F",
            "owner=o",
            "-F",
            "n=7",
            "-F",
            "flag=true",
            "-F",
            "plus=+5",
            "-f",
            "sha=0123",
            "-F",
            "cursor=Y3Vyc29yOnYyOpK5MjAyNi0xMS0wOVQxMDo1OTowNSswMDowMM4XZw3m",
        ]),
        Some("o/r"),
    )
    .expect("GraphQL read");
    let ReadRequest::Graphql { variables, .. } = graphql.request else {
        panic!("graphql");
    };
    assert_eq!(variables["owner"], json!("o"));
    assert_eq!(variables["n"], json!(7), "-F types integers as gh does");
    assert_eq!(variables["flag"], json!(true));
    assert_eq!(variables["plus"], json!(5), "Go's Atoi accepts a leading +");
    assert_eq!(variables["sha"], json!("0123"), "-f never types");
    assert!(variables["cursor"].is_string());

    let view = argv::classify(
        &argv(&[
            "pr",
            "view",
            "12",
            "--repo",
            "o/r",
            "--json",
            "id,number,state,statusCheckRollup",
        ]),
        Some("o/r"),
    )
    .expect("pr view");
    assert_eq!(
        view.request,
        ReadRequest::PrView {
            number: 12,
            fields: vec![
                PrField::Id,
                PrField::Number,
                PrField::State,
                PrField::StatusCheckRollup
            ],
        }
    );
}

#[test]
fn classifier_declines_every_write_and_every_unknown_shape() {
    let query = "query=query{viewer{login}}";
    for args in [
        // REST with any flag: method, fields, body, headers, paging, filters.
        argv(&["api", "-X", "POST", "repos/o/r/issues"]),
        argv(&["api", "repos/o/r/issues", "-X", "POST"]),
        argv(&["api", "--method", "PUT", "repos/o/r/pulls/1/merge"]),
        argv(&["api", "--method=DELETE", "repos/o/r/git/refs/heads/x"]),
        argv(&["api", "repos/o/r/issues/1/comments", "-f", "body=hi"]),
        argv(&["api", "repos/o/r/issues/1/comments", "-F", "body=hi"]),
        argv(&["api", "repos/o/r/pulls", "--input", "file.json"]),
        argv(&["api", "--paginate", "repos/o/r/pulls"]),
        argv(&["api", "repos/o/r/pulls", "--jq", ".[]"]),
        argv(&["api", "-H", "Accept: x", "repos/o/r/pulls"]),
        // REST outside one repository, or with placeholders and traversal.
        argv(&["api", "rate_limit"]),
        argv(&["api", "user"]),
        argv(&["api", "repos/{owner}/{repo}/pulls"]),
        argv(&["api", "repos/o/r/../x/pulls"]),
        argv(&["api", "repos/o/r/pulls?x=1#frag"]),
        // GraphQL that writes, subscribes, reads files or pages.
        argv(&["api", "graphql", "-f", "query=mutation{x}"]),
        argv(&["api", "graphql", "-f", "query=  mutation M{x}"]),
        argv(&["api", "graphql", "-f", "query=query{a} mutation{b}"]),
        argv(&["api", "graphql", "-f", "query=subscription{x}"]),
        argv(&["api", "graphql", "-f", "query=queryx{a}"]),
        argv(&["api", "graphql", "-F", "query=@q.graphql"]),
        argv(&["api", "graphql", "-f", query, "-F", "x=@file"]),
        argv(&["api", "graphql", "-f", query, "--paginate"]),
        argv(&["api", "graphql", "-f", query, "-f", "operationName=x"]),
        argv(&["api", "graphql", "-f", query, "-f", query]),
        argv(&["api", "graphql", "-f", query, "-f", "a[]=1"]),
        argv(&["api", "graphql", "-f", query, "-F", "owner={owner}"]),
        // pr view with an unknown field, an extra flag, or no repository.
        argv(&["pr", "view", "1", "--repo", "o/r", "--json", "body"]),
        argv(&["pr", "view", "1", "--repo", "o/r", "--json", "id,id"]),
        argv(&["pr", "view", "1", "--repo", "o/r", "--json", "id", "--web"]),
        argv(&["pr", "view", "1", "--json", "id"]),
        argv(&["pr", "view", "x", "--repo", "o/r", "--json", "id"]),
        argv(&["pr", "merge", "1", "--repo", "o/r"]),
        argv(&["pr", "edit", "1", "--repo", "o/r"]),
        argv(&["run", "cancel", "1"]),
    ] {
        assert!(
            argv::classify(&args, Some("o/r")).is_none(),
            "must decline {args:?}"
        );
    }
}

#[test]
fn classifier_declines_a_read_outside_its_scope() {
    assert!(argv::classify(&argv(&["api", "repos/o/other/pulls/1"]), Some("o/r")).is_none());
    assert!(
        argv::classify(
            &argv(&["pr", "view", "1", "--repo", "o/other", "--json", "id"]),
            Some("o/r")
        )
        .is_none()
    );
    // GraphQL names no repository, so it needs the scope to pick the App
    // installation; without one it is left to gh.
    assert!(
        argv::classify(
            &argv(&["api", "graphql", "-f", "query=query{viewer{login}}"]),
            None
        )
        .is_none()
    );
    // Case differences are the same repository.
    assert!(argv::classify(&argv(&["api", "repos/O/R/pulls/1"]), Some("o/r")).is_some());
}

// ---------------------------------------------------------------- pr view

/// A rollup answer in GitHub's GraphQL shape, and the JSON `gh pr view`
/// printed for it (captured from gh 2.102.0 against a live pull request).
#[test]
fn pr_view_renders_exactly_what_gh_prints() {
    let answer = json!({
        "id": "PR_1", "number": 296, "state": "OPEN", "isDraft": false,
        "baseRefName": "main", "headRefOid": SHA, "headRefName": "feature/x",
        "mergeStateStatus": "BLOCKED", "changedFiles": 6, "mergedAt": null,
        "autoMergeRequest": null,
        "author": {"login": "shipyard-local"},
        "labels": {"nodes": [{"id": "LA_1", "name": "1·claude", "description": null, "color": "1f6feb"}], "totalCount": 1},
        "statusCheckRollup": {"nodes": [{"commit": {"statusCheckRollup": {"contexts": {
            "nodes": [
                {"__typename": "CheckRun", "name": "review-verdict", "checkSuite": {"workflowRun": {"workflow": {"name": "Review verdict"}}},
                 "status": "COMPLETED", "conclusion": "FAILURE", "startedAt": "2026-11-09T19:54:42Z", "completedAt": "2026-11-09T19:54:59Z",
                 "detailsUrl": "https://github.com/o/r/actions/runs/1/job/2"},
                {"__typename": "CheckRun", "name": "gpu", "checkSuite": {"workflowRun": null},
                 "status": "QUEUED", "conclusion": null, "startedAt": "2026-11-09T21:00:00Z", "completedAt": null,
                 "detailsUrl": "https://github.com/o/r/actions/runs/3/job/4"},
                {"__typename": "StatusContext", "context": "review-verdict", "state": "FAILURE",
                 "targetUrl": "https://github.com/o/r/actions/runs/1", "createdAt": "2026-11-09T19:54:57Z", "description": "x"}
            ],
            "pageInfo": {"hasNextPage": false, "endCursor": "c"}}}}}]}
    });
    let fields = "id,number,state,isDraft,baseRefName,headRefOid,headRefName,mergeStateStatus,autoMergeRequest,labels,author,changedFiles,mergedAt,statusCheckRollup"
        .split(',')
        .map(|name| PrField::parse(name).expect("field"))
        .collect::<Vec<_>>();
    let rendered = pr_view::render(&answer, &fields).expect("render");
    assert_eq!(
        rendered,
        json!({
            "author": {"is_bot": true, "login": "app/shipyard-local"},
            "autoMergeRequest": null, "baseRefName": "main", "changedFiles": 6,
            "headRefName": "feature/x", "headRefOid": SHA, "id": "PR_1", "isDraft": false,
            "labels": [{"id": "LA_1", "name": "1·claude", "description": "", "color": "1f6feb"}],
            "mergeStateStatus": "BLOCKED", "mergedAt": null, "number": 296, "state": "OPEN",
            "statusCheckRollup": [
                {"__typename": "CheckRun", "completedAt": "2026-11-09T19:54:59Z", "conclusion": "FAILURE",
                 "detailsUrl": "https://github.com/o/r/actions/runs/1/job/2", "name": "review-verdict",
                 "startedAt": "2026-11-09T19:54:42Z", "status": "COMPLETED", "workflowName": "Review verdict"},
                {"__typename": "CheckRun", "completedAt": "0001-01-01T00:00:00Z", "conclusion": "",
                 "detailsUrl": "https://github.com/o/r/actions/runs/3/job/4", "name": "gpu",
                 "startedAt": "2026-11-09T21:00:00Z", "status": "QUEUED", "workflowName": ""},
                {"__typename": "StatusContext", "context": "review-verdict", "startedAt": "2026-11-09T19:54:57Z",
                 "state": "FAILURE", "targetUrl": "https://github.com/o/r/actions/runs/1"}
            ]
        })
    );
}

#[test]
fn pr_view_encodes_users_auto_merge_and_empty_rollups_like_gh() {
    let fields = [
        PrField::Author,
        PrField::AutoMergeRequest,
        PrField::StatusCheckRollup,
    ];
    let rendered = pr_view::render(
        &json!({
            "author": {"login": "octo", "id": "U_1", "name": null},
            "autoMergeRequest": {"authorEmail": null, "commitBody": null, "commitHeadline": null,
                "mergeMethod": "MERGE", "enabledAt": "2026-11-09T00:00:00Z",
                "enabledBy": {"login": "shipyard-local"}},
            "statusCheckRollup": {"nodes": [{"commit": {"statusCheckRollup": null}}]}
        }),
        &fields,
    )
    .expect("render");
    assert_eq!(
        rendered["author"],
        json!({"id": "U_1", "is_bot": false, "login": "octo", "name": ""})
    );
    assert_eq!(
        rendered["autoMergeRequest"]["enabledBy"],
        json!({"is_bot": true, "login": "app/shipyard-local"})
    );
    assert_eq!(rendered["autoMergeRequest"]["mergeMethod"], json!("MERGE"));
    // A head with no rollup prints `[]` (gh 2.102.0 on a pull request with
    // no checks), and so does an empty rollup.
    assert_eq!(rendered["statusCheckRollup"], json!([]));
    let empty = pr_view::render(
        &json!({"statusCheckRollup": {"nodes": [{"commit": {"statusCheckRollup": {"contexts": {
            "nodes": [], "pageInfo": {"hasNextPage": false, "endCursor": null}}}}}]}}),
        &[PrField::StatusCheckRollup],
    )
    .expect("render");
    assert_eq!(empty["statusCheckRollup"], json!([]));
    let no_commit = pr_view::render(
        &json!({"author": null, "autoMergeRequest": null, "statusCheckRollup": {"nodes": []}}),
        &fields,
    )
    .expect("render");
    assert_eq!(no_commit["statusCheckRollup"], Value::Null);
    assert_eq!(
        no_commit["author"],
        json!({"is_bot": true, "login": "app/"})
    );
}

#[test]
fn pr_view_declines_answers_gh_would_page() {
    let rollup = json!({"statusCheckRollup": {"nodes": [{"commit": {"statusCheckRollup": {"contexts": {
        "nodes": [], "pageInfo": {"hasNextPage": true, "endCursor": "c"}}}}}]}});
    assert_eq!(
        pr_view::render(&rollup, &[PrField::StatusCheckRollup]),
        Err(pr_view::ShapeError::NeedsPaging("statusCheckRollup"))
    );
    let labels = json!({"labels": {"nodes": [], "totalCount": 101}});
    assert_eq!(
        pr_view::render(&labels, &[PrField::Labels]),
        Err(pr_view::ShapeError::NeedsPaging("labels"))
    );
}

// ------------------------------------------------- mint, cache and refresh

#[test]
fn token_is_minted_once_cached_and_refreshed_before_expiry() {
    let key = write_key();
    let clock = now_clock();
    let mock = github(Arc::clone(&clock), |request| {
        Reply::json(200, &json!({"auth": request.header("authorization")}))
    });
    let reads = reader(&mock, &key.path, &clock);
    let args = argv(&["api", STATUSES]);

    for expected in ["token ghs_token1", "token ghs_token1", "token ghs_token1"] {
        let (success, stdout, _, _) =
            answered(reads.try_read(&args, Some("o/r"), Duration::from_secs(5), None));
        assert!(success);
        assert_eq!(
            serde_json::from_str::<Value>(&stdout).unwrap()["auth"],
            json!(expected)
        );
    }
    assert_eq!(mock.count("GET", "repos/o/r/installation"), 1);
    assert_eq!(mock.count("POST", "app/installations"), 1);
    assert_eq!(reads.counters().token_mints, 1);
    assert_eq!(reads.counters().token_cache_hits, 2);

    // 56 minutes on, the hour-long token is inside the five-minute refresh
    // lead: the next read mints a fresh one; the installation is remembered.
    clock.fetch_add(56 * 60, Ordering::SeqCst);
    let (_, stdout, _, meta) =
        answered(reads.try_read(&args, Some("o/r"), Duration::from_secs(5), None));
    assert!(meta.minted);
    assert_eq!(
        serde_json::from_str::<Value>(&stdout).unwrap()["auth"],
        json!("token ghs_token2")
    );
    assert_eq!(mock.count("GET", "repos/o/r/installation"), 1);
    assert_eq!(mock.count("POST", "app/installations"), 2);
}

#[test]
fn concurrent_first_reads_share_one_mint() {
    let key = write_key();
    let clock = now_clock();
    let mock = github(Arc::clone(&clock), |_| Reply::json(200, &json!([])));
    let reads = reader(&mock, &key.path, &clock);
    std::thread::scope(|scope| {
        for _ in 0..6 {
            let reads = reads.clone();
            scope.spawn(move || {
                let (success, ..) = answered(reads.try_read(
                    &argv(&["api", STATUSES]),
                    Some("o/r"),
                    Duration::from_secs(5),
                    None,
                ));
                assert!(success);
            });
        }
    });
    assert_eq!(mock.count("POST", "app/installations"), 1);
}

#[test]
fn connections_are_kept_alive_across_reads() {
    let key = write_key();
    let clock = now_clock();
    let mock = github(Arc::clone(&clock), |_| Reply::json(200, &json!([])));
    let reads = reader(&mock, &key.path, &clock);
    for _ in 0..5 {
        answered(reads.try_read(
            &argv(&["api", STATUSES]),
            Some("o/r"),
            Duration::from_secs(5),
            None,
        ));
    }
    assert_eq!(mock.requests().len(), 7, "2 credential + 5 reads");
    assert_eq!(
        mock.connections.load(Ordering::SeqCst),
        1,
        "every request reused one connection"
    );
}

#[test]
fn scoped_tokens_request_one_repository_and_refuse_a_broader_grant() {
    let key = write_key();
    let clock = now_clock();
    let broad = Arc::new(Mutex::new(false));
    let broad_for_mock = Arc::clone(&broad);
    let mock = Mock::start(move |request| {
        if request.path.ends_with("/installation") {
            return Reply::json(200, &json!({"id": 77}));
        }
        if request.path.ends_with("/access_tokens") {
            let body: Value = serde_json::from_slice(&request.body).expect("json body");
            assert_eq!(body, json!({"repositories": ["r"]}));
            let repositories = if *broad_for_mock.lock().unwrap() {
                json!([{"name": "r"}, {"name": "other"}])
            } else {
                json!([{"name": "r"}])
            };
            return Reply::json(
                201,
                &json!({"token": "ghs_scoped", "expires_at": "2999-01-01T00:00:00Z", "repositories": repositories}),
            );
        }
        Reply::json(200, &json!([]))
    });
    let clock_for_reads = Arc::clone(&clock);
    let reads = InprocReads::for_tests(
        credential(&key.path, true),
        &mock.url,
        Arc::new(move || clock_for_reads.load(Ordering::SeqCst)),
        None,
    );
    *broad.lock().unwrap() = true;
    let outcome = reads.try_read(
        &argv(&["api", STATUSES]),
        Some("o/r"),
        Duration::from_secs(5),
        None,
    );
    let ReadOutcome::Declined {
        reason: Some(reason),
    } = outcome
    else {
        panic!("a broader grant must hand the read to gh, got {outcome:?}");
    };
    assert!(
        reason.contains("more than the requested repository"),
        "{reason}"
    );
    assert!(!reason.contains("ghs_scoped"));
    assert_eq!(
        mock.count("GET", "repos/o/r/commits"),
        0,
        "token never used"
    );

    *broad.lock().unwrap() = false;
    // The refusal is held for a minute, then the credential is asked again.
    clock.fetch_add(61, Ordering::SeqCst);
    let (success, ..) = answered(reads.try_read(
        &argv(&["api", STATUSES]),
        Some("o/r"),
        Duration::from_secs(5),
        None,
    ));
    assert!(success);
}

#[test]
fn a_rejected_token_is_replaced_once() {
    let key = write_key();
    let clock = now_clock();
    let mock = github(Arc::clone(&clock), |request| {
        if request.header("authorization") == Some("token ghs_token1") {
            return Reply::json(401, &json!({"message": "Bad credentials"}));
        }
        Reply::json(200, &json!([]))
    });
    let reads = reader(&mock, &key.path, &clock);
    let (success, _, _, meta) = answered(reads.try_read(
        &argv(&["api", STATUSES]),
        Some("o/r"),
        Duration::from_secs(5),
        None,
    ));
    assert!(success);
    assert!(meta.minted);
    assert_eq!(mock.count("POST", "app/installations"), 2);
}

#[test]
fn credential_failures_hand_the_read_to_gh_without_leaking_material() {
    let key = write_key();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&key.path, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    let clock = now_clock();
    let mock = github(Arc::clone(&clock), |_| Reply::json(200, &json!([])));
    let reads = reader(&mock, &key.path, &clock);
    let outcome = reads.try_read(
        &argv(&["api", STATUSES]),
        Some("o/r"),
        Duration::from_secs(5),
        None,
    );
    let ReadOutcome::Declined {
        reason: Some(reason),
    } = outcome
    else {
        panic!("expected a counted decline, got {outcome:?}");
    };
    #[cfg(unix)]
    assert!(reason.contains("mode 0600"), "{reason}");
    assert!(!reason.contains("PRIVATE KEY"), "{reason}");
    assert!(mock.requests().is_empty(), "nothing reached GitHub");

    // A key path that is not a key declines too, naming no content.
    std::fs::write(&key.path, "not a key at all").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&key.path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let reads = reader(&mock, &key.path, &clock);
    let ReadOutcome::Declined {
        reason: Some(reason),
    } = reads.try_read(
        &argv(&["api", STATUSES]),
        Some("o/r"),
        Duration::from_secs(5),
        None,
    )
    else {
        panic!("expected a decline");
    };
    assert!(!reason.contains("not a key at all"), "{reason}");
}

#[test]
fn tokens_never_appear_in_debug_output() {
    let key = write_key();
    let clock = now_clock();
    let mock = github(Arc::clone(&clock), |_| Reply::json(200, &json!([])));
    let reads = reader(&mock, &key.path, &clock);
    answered(reads.try_read(
        &argv(&["api", STATUSES]),
        Some("o/r"),
        Duration::from_secs(5),
        None,
    ));
    let tokens = reads.inner.tokens.lock().unwrap();
    let debug = format!("{reads:?} {:?}", *tokens);
    assert!(!tokens.is_empty());
    assert!(!debug.contains("ghs_token1"), "{debug}");
    let signer = reads.inner.signer.lock().unwrap();
    assert!(!format!("{:?}", *signer).contains("modulus"));
}

#[test]
fn parse_token_reply_refuses_unusable_tokens() {
    for body in [
        json!({}),
        json!({"token": ""}),
        json!({"token": "ghs a"}),
        json!({"token": "ghs\na"}),
        json!({"token": 7}),
    ] {
        assert!(super::parse_token_reply(body.to_string().as_bytes(), None, 0).is_err());
    }
    let token = super::parse_token_reply(
        br#"{"token":"ghs_x","expires_at":"2026-11-09T21:00:00Z"}"#,
        None,
        0,
    )
    .expect("token");
    assert_eq!(
        token.expires_at,
        chrono::DateTime::parse_from_rfc3339("2026-11-09T21:00:00Z")
            .unwrap()
            .timestamp()
    );
    assert_eq!(
        super::parse_token_reply(br#"{"token":"ghs_x"}"#, None, 100)
            .expect("token")
            .expires_at,
        100 + super::ASSUMED_TOKEN_LIFETIME_SECONDS
    );
}

#[test]
fn a_refused_installation_is_not_asked_again_for_a_while() {
    let key = write_key();
    let clock = now_clock();
    let mock = Mock::start(|request| {
        if request.path.ends_with("/installation") {
            return Reply::json(404, &json!({"message": "Not Found"}));
        }
        Reply::json(200, &json!([]))
    });
    let reads = reader(&mock, &key.path, &clock);
    let read = || {
        reads.try_read(
            &argv(&["api", STATUSES]),
            Some("o/r"),
            Duration::from_secs(5),
            None,
        )
    };
    for _ in 0..3 {
        assert!(matches!(read(), ReadOutcome::Declined { reason: Some(_) }));
    }
    assert_eq!(mock.count("GET", "repos/o/r/installation"), 1);
    clock.fetch_add(61, Ordering::SeqCst);
    assert!(matches!(read(), ReadOutcome::Declined { reason: Some(_) }));
    assert_eq!(mock.count("GET", "repos/o/r/installation"), 2);
}

#[test]
fn a_reader_waiting_on_a_slow_mint_keeps_its_own_deadline() {
    let key = write_key();
    let clock = now_clock();
    let mock = Mock::start(|request| {
        if request.path.ends_with("/installation") {
            return Reply::json(200, &json!({"id": 77}));
        }
        if request.path.ends_with("/access_tokens") {
            return Reply::json(
                201,
                &json!({"token": "ghs_slow", "expires_at": "2999-01-01T00:00:00Z"}),
            )
            .delayed(Duration::from_secs(3));
        }
        Reply::json(200, &json!([]))
    });
    let reads = reader(&mock, &key.path, &clock);
    std::thread::scope(|scope| {
        let slow = scope.spawn(|| {
            reads.try_read(
                &argv(&["api", STATUSES]),
                Some("o/r"),
                Duration::from_secs(10),
                None,
            )
        });
        std::thread::sleep(Duration::from_millis(300));
        let started = Instant::now();
        let waiter = reads.try_read(
            &argv(&["api", STATUSES]),
            Some("o/r"),
            Duration::from_secs(1),
            None,
        );
        let elapsed_waiting = started.elapsed();
        assert!(
            matches!(
                waiter,
                ReadOutcome::Failed {
                    kind: FailureKind::TimedOut,
                    ..
                }
            ),
            "{waiter:?}"
        );
        assert!(
            elapsed_waiting < Duration::from_millis(1_800),
            "{elapsed_waiting:?}"
        );
        assert!(matches!(
            slow.join().unwrap(),
            ReadOutcome::Answered { success: true, .. }
        ));
    });
}

#[test]
fn a_bounded_pr_view_is_judged_on_what_it_prints() {
    let key = write_key();
    let clock = now_clock();
    let padding = "x".repeat(4096);
    let mock = github(Arc::clone(&clock), move |_| {
        Reply::json(
            200,
            &json!({"data": {"repository": {"pullRequest": {"state": "OPEN", "unrequested": padding}}}}),
        )
    });
    let reads = reader(&mock, &key.path, &clock);
    let (success, stdout, ..) = answered(reads.try_read(
        &argv(&["pr", "view", "9", "--repo", "o/r", "--json", "state"]),
        Some("o/r"),
        Duration::from_secs(5),
        Some(1024),
    ));
    assert!(success, "a raw answer over the bound still prints under it");
    assert_eq!(stdout, "{\"state\":\"OPEN\"}\n");
}

// ------------------------------------------------------ timeout and retry

#[test]
fn a_stalled_read_times_out_within_its_budget() {
    let key = write_key();
    let clock = now_clock();
    let mock = github(Arc::clone(&clock), |request| {
        if request.path.starts_with("repos/o/r/commits") {
            return Reply::json(200, &json!([])).delayed(Duration::from_secs(4));
        }
        Reply::json(200, &json!([]))
    });
    let reads = reader(&mock, &key.path, &clock);
    let started = Instant::now();
    let outcome = reads.try_read(
        &argv(&["api", STATUSES]),
        Some("o/r"),
        Duration::from_millis(1_500),
        None,
    );
    let elapsed = started.elapsed();
    assert!(
        matches!(
            outcome,
            ReadOutcome::Failed {
                kind: FailureKind::TimedOut,
                ..
            }
        ),
        "{outcome:?}"
    );
    assert!(elapsed < Duration::from_millis(2_500), "{elapsed:?}");
}

#[test]
fn transient_failures_are_retried_and_others_are_not() {
    let key = write_key();
    let clock = now_clock();
    let script = Arc::new(Mutex::new(VecDeque::from([503_u16, 502, 200, 404, 403])));
    let script_for_mock = Arc::clone(&script);
    let mock = github(Arc::clone(&clock), move |_| {
        match script_for_mock.lock().unwrap().pop_front().unwrap_or(200) {
            200 => Reply::json(200, &json!([{"state": "success"}])),
            404 => Reply::json(404, &json!({"message": "Not Found"})),
            403 => Reply::json(
                403,
                &json!({"message": "API rate limit exceeded for installation ID 77."}),
            ),
            status => Reply::json(status, &json!({"message": "Server Error"})),
        }
    });
    let reads = reader(&mock, &key.path, &clock);
    let args = argv(&["api", STATUSES]);

    let (success, stdout, _, meta) =
        answered(reads.try_read(&args, Some("o/r"), Duration::from_secs(10), None));
    assert!(success, "503 and 502 were retried");
    assert_eq!(meta.attempts, 3);
    assert_eq!(stdout, r#"[{"state":"success"}]"#);

    let (success, stdout, stderr, meta) =
        answered(reads.try_read(&args, Some("o/r"), Duration::from_secs(10), None));
    assert!(!success);
    assert_eq!(meta.attempts, 1, "a 404 is not retried");
    assert_eq!(stderr, "gh: Not Found (HTTP 404)");
    assert_eq!(stdout, r#"{"message":"Not Found"}"#);

    let (success, _, stderr, meta) =
        answered(reads.try_read(&args, Some("o/r"), Duration::from_secs(10), None));
    assert!(!success);
    assert_eq!(meta.attempts, 1, "a rate limit is not retried");
    assert_eq!(
        stderr,
        "gh: API rate limit exceeded for installation ID 77. (HTTP 403)"
    );
}

#[test]
fn retries_stop_at_the_attempt_limit() {
    let key = write_key();
    let clock = now_clock();
    let mock = github(Arc::clone(&clock), |_| {
        Reply::json(502, &json!({"message": "Bad Gateway"}))
    });
    let reads = reader(&mock, &key.path, &clock);
    let (success, _, stderr, meta) = answered(reads.try_read(
        &argv(&["api", STATUSES]),
        Some("o/r"),
        Duration::from_secs(10),
        None,
    ));
    assert!(!success);
    assert_eq!(meta.attempts, super::transport::MAX_ATTEMPTS);
    assert_eq!(stderr, "gh: Bad Gateway (HTTP 502)");
}

#[test]
fn backoff_is_jittered_and_grows() {
    let samples = (0..32)
        .map(|_| super::transport::backoff(1))
        .collect::<Vec<_>>();
    assert!(
        samples
            .iter()
            .all(|wait| *wait <= Duration::from_millis(200))
    );
    assert!(
        samples
            .iter()
            .all(|wait| *wait >= Duration::from_millis(20))
    );
    assert!(
        samples.iter().any(|wait| *wait != samples[0]),
        "waits are randomized"
    );
    assert!(super::transport::backoff(3) <= Duration::from_millis(800));
}

// ------------------------------------------------------ conditional reads

#[test]
fn a_repeated_rest_read_is_revalidated_with_its_etag() {
    let key = write_key();
    let clock = now_clock();
    let mock = github(Arc::clone(&clock), |request| {
        if request.header("if-none-match") == Some("\"v1\"") {
            return Reply {
                status: 304,
                headers: vec![("ETag".to_owned(), "\"v1\"".to_owned())],
                body: Vec::new(),
                delay: Duration::ZERO,
            };
        }
        Reply::json(200, &json!([{"state": "pending"}]))
            .header("ETag", "\"v1\"")
            .header("x-ratelimit-resource", "core")
            .header("x-ratelimit-remaining", "4999")
            .header("x-ratelimit-used", "1")
    });
    let reads = reader(&mock, &key.path, &clock);
    let args = argv(&["api", STATUSES]);
    let (_, first, _, meta) =
        answered(reads.try_read(&args, Some("o/r"), Duration::from_secs(5), None));
    assert!(!meta.conditional_hit);
    assert_eq!(
        meta.rate_limit.as_ref().map(|limit| limit.remaining),
        Some(4999)
    );
    let (success, second, _, meta) =
        answered(reads.try_read(&args, Some("o/r"), Duration::from_secs(5), None));
    assert!(success);
    assert!(meta.conditional_hit);
    assert_eq!(first, second, "a 304 answers with the cached body");
}

#[test]
fn a_redirect_is_handed_to_gh_not_followed() {
    let key = write_key();
    let clock = now_clock();
    let mock = github(Arc::clone(&clock), |request| {
        if request.path.starts_with("repos/o/r/commits") {
            return Reply::json(301, &json!({"message": "Moved Permanently"}))
                .header("Location", "http://127.0.0.1:9/elsewhere");
        }
        Reply::json(200, &json!([]))
    });
    let reads = reader(&mock, &key.path, &clock);
    let outcome = reads.try_read(
        &argv(&["api", STATUSES]),
        Some("o/r"),
        Duration::from_secs(5),
        None,
    );
    assert!(
        matches!(outcome, ReadOutcome::Declined { reason: Some(_) }),
        "{outcome:?}"
    );
    assert_eq!(mock.count("GET", "elsewhere"), 0);
}

#[test]
fn the_production_transport_refuses_plain_http() {
    let mock = Mock::start(|_| Reply::json(200, &json!([])));
    let transport = super::transport::Transport::https_only_for_tests(&mock.url);
    let sent = transport.send(
        &super::transport::HttpRequest {
            method: super::transport::Method::Get,
            path: "repos/o/r".to_owned(),
            headers: Vec::new(),
            body: None,
        },
        "token ghs_never_sent",
        Instant::now() + Duration::from_secs(5),
        1024,
    );
    assert!(sent.result.is_err());
    assert!(
        mock.requests().is_empty(),
        "no credential crossed plain HTTP"
    );
}

// ---------------------------------------------------------------- GraphQL

#[test]
fn graphql_reads_send_typed_variables_and_report_errors_like_gh() {
    let key = write_key();
    let clock = now_clock();
    let mock = github(Arc::clone(&clock), |request| {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        if body["variables"]["owner"] == json!("broken") {
            return Reply::json(
                200,
                &json!({"errors": [{"message": "first"}, {"message": "second"}]}),
            );
        }
        Reply::json(200, &json!({"data": {"echo": body["variables"]}}))
    });
    let reads = reader(&mock, &key.path, &clock);
    let query = "query=query($owner:String!,$n:Int){repository(owner:$owner,name:\"r\"){id}}";
    let (success, stdout, ..) = answered(reads.try_read(
        &argv(&["api", "graphql", "-f", query, "-F", "owner=o", "-F", "n=5"]),
        Some("o/r"),
        Duration::from_secs(5),
        None,
    ));
    assert!(success);
    assert_eq!(
        serde_json::from_str::<Value>(&stdout).unwrap()["data"]["echo"],
        json!({"owner": "o", "n": 5})
    );
    let graphql = mock
        .requests()
        .into_iter()
        .rfind(|request| request.path == "graphql")
        .expect("graphql request");
    assert_eq!(graphql.method, "POST");
    assert_eq!(graphql.header("graphql-features"), Some("merge_queue"));

    let (success, stdout, stderr, _) = answered(reads.try_read(
        &argv(&["api", "graphql", "-f", query, "-F", "owner=broken"]),
        Some("o/r"),
        Duration::from_secs(5),
        None,
    ));
    assert!(!success);
    assert!(stdout.contains("\"errors\""), "gh prints the body too");
    assert_eq!(stderr, "gh: first\nsecond");
}

#[test]
fn pr_view_round_trips_through_the_client() {
    let key = write_key();
    let clock = now_clock();
    let mock = github(Arc::clone(&clock), |request| {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(
            body["variables"],
            json!({"owner": "o", "repo": "r", "pr_number": 9})
        );
        if body["query"].as_str().unwrap().contains("labels") {
            return Reply::json(
                200,
                &json!({"errors": [{"message": "Could not resolve to a PullRequest with the number of 9.", "path": ["repository", "pullRequest"]}]}),
            );
        }
        Reply::json(
            200,
            &json!({"data": {"repository": {"pullRequest": {"state": "OPEN", "headRefOid": SHA}}}}),
        )
    });
    let reads = reader(&mock, &key.path, &clock);
    let (success, stdout, ..) = answered(reads.try_read(
        &argv(&[
            "pr",
            "view",
            "9",
            "--repo",
            "o/r",
            "--json",
            "state,headRefOid",
        ]),
        Some("o/r"),
        Duration::from_secs(5),
        None,
    ));
    assert!(success);
    assert_eq!(
        stdout,
        format!("{{\"headRefOid\":\"{SHA}\",\"state\":\"OPEN\"}}\n")
    );
    let (success, stdout, stderr, _) = answered(reads.try_read(
        &argv(&["pr", "view", "9", "--repo", "o/r", "--json", "labels"]),
        Some("o/r"),
        Duration::from_secs(5),
        None,
    ));
    assert!(!success);
    assert!(stdout.is_empty());
    assert_eq!(
        stderr,
        "GraphQL: Could not resolve to a PullRequest with the number of 9. (repository.pullRequest)"
    );
}

#[test]
fn bounded_reads_refuse_oversized_answers() {
    let key = write_key();
    let clock = now_clock();
    let mock = github(Arc::clone(&clock), |_| {
        Reply::json(200, &json!(["x".repeat(4096)]))
    });
    let reads = reader(&mock, &key.path, &clock);
    let outcome = reads.try_read(
        &argv(&["api", STATUSES]),
        Some("o/r"),
        Duration::from_secs(5),
        Some(1024),
    );
    assert!(
        matches!(
            outcome,
            ReadOutcome::Failed {
                kind: FailureKind::TooLarge,
                ..
            }
        ),
        "{outcome:?}"
    );
}

// ---------------------------------------------------------------- batching

#[test]
fn batch_size_grows_while_fast_and_halves_when_slow_or_failing() {
    let fast = [Duration::from_millis(400); 5];
    assert_eq!(batch::next_size(6, &fast, false), 8);
    assert_eq!(batch::next_size(12, &fast, false), batch::MAX_BATCH);
    assert_eq!(batch::next_size(8, &fast, true), 4, "a failure halves");
    let mut one_slow = fast.to_vec();
    one_slow.push(Duration::from_secs(6));
    assert_eq!(
        batch::next_size(8, &one_slow, false),
        4,
        "a slow batch halves"
    );
    let middling = [Duration::from_secs(3); 4];
    assert_eq!(
        batch::next_size(8, &middling, false),
        8,
        "steady between bounds"
    );
    assert_eq!(batch::next_size(2, &one_slow, true), batch::MIN_BATCH);
    assert_eq!(batch::next_size(7, &[], false), 7, "nothing observed");
    assert_eq!(batch::percentile(&fast, 90), Duration::from_millis(400));
}

#[test]
fn batch_size_persists_between_passes() {
    let dir = tempfile::tempdir().unwrap();
    let first = BatchSizer::persistent(dir.path());
    assert_eq!(first.size(), batch::DEFAULT_BATCH);
    first.record(Duration::from_millis(300), true);
    assert_eq!(first.save().unwrap(), batch::DEFAULT_BATCH + 2);
    let second = BatchSizer::persistent(dir.path());
    assert_eq!(second.size(), batch::DEFAULT_BATCH + 2);
    // Nothing observed: the file is left as it was.
    assert_eq!(second.save().unwrap(), batch::DEFAULT_BATCH + 2);
    let failing = BatchSizer::in_memory(10);
    failing.record(Duration::from_secs(1), false);
    assert_eq!(failing.next_size(), 5);
    std::fs::write(
        dir.path().join("github-read-batch.json"),
        b"{\"heads_per_query\":500}",
    )
    .unwrap();
    assert_eq!(BatchSizer::persistent(dir.path()).size(), batch::MAX_BATCH);
}

// ----------------------------------------------------------- configuration

#[test]
fn credentials_come_only_from_the_canonical_token_command() {
    let canonical = argv(&[
        "/home/u/.local/bin/ghapp",
        "token",
        "--app-id",
        "3878000",
        "--private-key",
        "/home/u/.config/shipyard/github-apps/app.pem",
        "--repo",
        "{repo_slug}",
    ]);
    let credential = AppCredential::from_token_command(&canonical).expect("canonical");
    assert_eq!(credential.app_id, 3_878_000);
    assert!(!credential.scope_to_repository);
    let mut scoped = canonical.clone();
    scoped.push("--scope-repository".to_owned());
    assert!(
        AppCredential::from_token_command(&scoped)
            .expect("scoped")
            .scope_to_repository
    );
    for (index, replacement) in [
        (0, "ghapp"),
        (1, "auth"),
        (3, "0"),
        (3, "12a"),
        (5, "relative/key.pem"),
        (5, "/a/../b.pem"),
        (7, "o/r"),
    ] {
        let mut changed = canonical.clone();
        changed[index] = replacement.to_owned();
        assert!(
            AppCredential::from_token_command(&changed).is_none(),
            "{changed:?}"
        );
    }
    assert!(AppCredential::from_token_command(&canonical[..7]).is_none());
}

#[test]
fn the_default_transport_is_gh_until_a_host_opts_in() {
    if std::env::var_os(super::TRANSPORT_ENV).is_some() {
        return;
    }
    let config = |table: Option<&str>| {
        let data = table.map_or_else(toml::Table::new, |text| text.parse().expect("toml"));
        crate::config::LoadedConfig {
            data,
            global_dir: PathBuf::from("/nonexistent-global"),
            project_dir: None,
            local_dir: None,
            local_overlay_source: crate::config::LocalOverlaySource::None,
        }
    };
    assert_eq!(
        super::configured_transport(&config(None)),
        Ok(super::ReadTransport::Gh)
    );
    assert_eq!(
        super::configured_transport(&config(Some("[github.reads]\ntransport = \"inproc\"\n"))),
        Ok(super::ReadTransport::Inproc)
    );
    assert!(
        super::configured_transport(&config(Some("[github.reads]\ntransport = \"fast\"\n")))
            .is_err()
    );
}

#[test]
fn transport_setting_selects_gh_or_inproc() {
    assert_eq!(super::parse_transport("gh"), Some(super::ReadTransport::Gh));
    assert_eq!(
        super::parse_transport(" inproc "),
        Some(super::ReadTransport::Inproc)
    );
    assert_eq!(super::parse_transport("fast"), None);
}

// ------------------------------------------- GitHubActions integration

#[cfg(unix)]
mod actions {
    use super::*;
    use crate::cloud::GitHubActions;
    use crate::gh_pass::GhPass;

    /// A fake `gh` that logs its argv and prints `answer`.
    fn fake_gh(dir: &Path, answer: &str) -> (PathBuf, PathBuf) {
        let gh = dir.join("gh");
        let log = dir.join("gh.log");
        crate::test_support::write_executable_script(
            &gh,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nprintf '%s' '{answer}'\n",
                log.display()
            ),
        );
        (gh, log)
    }

    fn gh_calls(log: &Path) -> Vec<String> {
        std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn steward_reads_skip_gh_and_report_their_latency() {
        let key = write_key();
        let clock = now_clock();
        let mock = github(Arc::clone(&clock), |_| Reply::json(200, &json!([])));
        let dir = tempfile::tempdir().unwrap();
        let (gh, log) = fake_gh(dir.path(), "[\"from-gh\"]");
        let pass = Arc::new(GhPass::caching());
        let actions = GitHubActions::hermetic_for_tests(dir.path(), &gh)
            .with_repo_override("o/r")
            .with_pass(Arc::clone(&pass))
            .with_inproc_reads(Some(reader(&mock, &key.path, &clock)));

        let read = actions
            .run_gh_with_timeout(&argv(&["api", STATUSES]), Duration::from_secs(5))
            .expect("in-process read");
        assert_eq!(read, "[]");
        let bounded = actions
            .run_gh_with_timeout_bounded(
                &argv(&["api", STATUSES]),
                Duration::from_secs(5),
                4096,
                4096,
            )
            .expect("bounded read");
        assert_eq!(bounded, "[]");
        assert!(gh_calls(&log).is_empty(), "no gh child ran");

        let report = pass.report();
        let inproc = &report.latency["inproc"];
        assert_eq!(inproc.calls, 2);
        assert_eq!(inproc.timeouts, 0);
        let cost = report.api_cost.expect("api cost");
        assert_eq!(cost.token_mints, 1);
        assert_eq!(cost.requests, 2);
        assert_eq!(report.rest, 2, "in-process reads still count at their site");
    }

    #[test]
    fn writes_and_unknown_shapes_still_run_gh() {
        let key = write_key();
        let clock = now_clock();
        let mock = github(Arc::clone(&clock), |_| Reply::json(200, &json!([])));
        let dir = tempfile::tempdir().unwrap();
        let (gh, log) = fake_gh(dir.path(), "{}");
        let actions = GitHubActions::hermetic_for_tests(dir.path(), &gh)
            .with_repo_override("o/r")
            .with_inproc_reads(Some(reader(&mock, &key.path, &clock)));
        for args in [
            argv(&[
                "api",
                "-X",
                "POST",
                "repos/o/r/issues/1/comments",
                "-f",
                "body=x",
            ]),
            argv(&["api", "graphql", "-f", "query=mutation{enqueue}"]),
            argv(&["pr", "merge", "1", "--repo", "o/r"]),
            argv(&["api", "--paginate", "repos/o/r/pulls"]),
        ] {
            actions
                .run_gh_with_timeout(&args, Duration::from_secs(5))
                .expect("gh answered");
        }
        assert_eq!(gh_calls(&log).len(), 4);
        assert!(
            mock.requests().is_empty(),
            "nothing went to GitHub in process"
        );
    }

    #[test]
    fn internal_queue_calls_and_calls_with_extra_environment_always_run_gh() {
        let key = write_key();
        let clock = now_clock();
        let mock = github(Arc::clone(&clock), |_| Reply::json(200, &json!([])));
        let dir = tempfile::tempdir().unwrap();
        let (gh, log) = fake_gh(dir.path(), "[]");
        let actions = GitHubActions::hermetic_for_tests(dir.path(), &gh)
            .with_repo_override("o/r")
            .with_inproc_reads(Some(reader(&mock, &key.path, &clock)));
        // Read-shaped on purpose: only the call path keeps these on gh.
        let args = argv(&["api", STATUSES]);
        actions
            .run_gh_internal_queue_mutation(&args)
            .expect("gh answered");
        actions
            .run_gh_with_timeout_env(&args, Duration::from_secs(5), &[("GH_REPO", "o/r")])
            .expect("gh answered");
        assert_eq!(gh_calls(&log).len(), 2);
        assert!(mock.requests().is_empty());
    }

    #[test]
    fn a_landing_request_always_takes_the_guarded_gh_path() {
        let key = write_key();
        let clock = now_clock();
        let mock = github(Arc::clone(&clock), |_| Reply::json(200, &json!({})));
        let dir = tempfile::tempdir().unwrap();
        let (gh, log) = fake_gh(dir.path(), "{}");
        let actions = GitHubActions::hermetic_for_tests(dir.path(), &gh)
            .with_repo_override("o/r")
            .with_inproc_reads(Some(reader(&mock, &key.path, &clock)));
        // A bare `gh api .../pulls/N/merge` is a GET to the classifier, but
        // the queue guard treats it as a landing; it must reach gh.
        let args = argv(&["api", "repos/o/r/pulls/1/merge"]);
        assert!(argv::classify(&args, Some("o/r")).is_some());
        assert!(crate::queue_arm_gate::is_landing_request(&args));
        let _ = actions.run_gh_with_timeout(&args, Duration::from_secs(5));
        let _ = actions.run_gh(&args);
        assert!(
            mock.requests()
                .iter()
                .all(|request| !request.path.ends_with("/merge")),
            "no landing request was answered in process"
        );
        assert!(
            gh_calls(&log)
                .iter()
                .any(|call| call.contains("pulls/1/merge")),
            "the landing request ran through gh"
        );
    }

    #[test]
    fn a_credential_failure_falls_back_to_gh_and_is_counted() {
        let dir = tempfile::tempdir().unwrap();
        let (gh, log) = fake_gh(dir.path(), "[\"from-gh\"]");
        let pass = Arc::new(GhPass::caching());
        let clock = now_clock();
        let mock = github(Arc::clone(&clock), |_| Reply::json(200, &json!([])));
        let actions = GitHubActions::hermetic_for_tests(dir.path(), &gh)
            .with_repo_override("o/r")
            .with_pass(Arc::clone(&pass))
            .with_inproc_reads(Some(reader(&mock, &dir.path().join("missing.pem"), &clock)));
        let unbounded = actions
            .run_gh(&argv(&["api", STATUSES]))
            .expect("gh answered");
        assert_eq!(unbounded, "[\"from-gh\"]");
        let timed = actions
            .run_gh_with_timeout(&argv(&["api", STATUSES]), Duration::from_secs(5))
            .expect("gh answered");
        assert_eq!(timed, "[\"from-gh\"]");
        assert_eq!(gh_calls(&log).len(), 2);
        let report = pass.report();
        assert_eq!(report.fallbacks[0].reason, "in-process read -> gh");
        assert_eq!(report.fallbacks[0].count, 2);
        assert_eq!(report.latency["gh"].calls, 2);
        assert!(!report.latency.contains_key("inproc"));
    }

    #[test]
    fn a_github_error_keeps_gh_error_text() {
        let key = write_key();
        let clock = now_clock();
        let mock = github(Arc::clone(&clock), |_| {
            Reply::json(404, &json!({"message": "Not Found"}))
        });
        let dir = tempfile::tempdir().unwrap();
        let (gh, log) = fake_gh(dir.path(), "{}");
        let actions = GitHubActions::hermetic_for_tests(dir.path(), &gh)
            .with_repo_override("o/r")
            .with_inproc_reads(Some(reader(&mock, &key.path, &clock)));
        let args = argv(&["api", STATUSES]);
        let error = actions
            .run_gh_with_timeout(&args, Duration::from_secs(5))
            .expect_err("404");
        assert_eq!(
            error.message(),
            format!(
                "gh {} failed with status 1: gh: Not Found (HTTP 404)",
                args.join(" ")
            )
        );
        assert!(gh_calls(&log).is_empty());
    }

    #[test]
    fn an_inproc_timeout_reads_like_a_gh_timeout() {
        let key = write_key();
        let clock = now_clock();
        let mock = github(Arc::clone(&clock), |request| {
            if request.path.starts_with("repos/o/r/commits") {
                return Reply::json(200, &json!([])).delayed(Duration::from_secs(4));
            }
            Reply::json(200, &json!([]))
        });
        let dir = tempfile::tempdir().unwrap();
        let (gh, log) = fake_gh(dir.path(), "{}");
        let pass = Arc::new(GhPass::caching());
        let actions = GitHubActions::hermetic_for_tests(dir.path(), &gh)
            .with_repo_override("o/r")
            .with_pass(Arc::clone(&pass))
            .with_inproc_reads(Some(reader(&mock, &key.path, &clock)));
        let error = actions
            .run_gh_with_timeout(&argv(&["api", STATUSES]), Duration::from_millis(1_200))
            .expect_err("timeout");
        assert!(
            error.message().contains("timed out after 1200ms"),
            "{error}"
        );
        // The steward's bounded-read retry keys on exactly this phrase.
        assert!(error.message().contains("timed out after"));
        assert!(gh_calls(&log).is_empty(), "a timeout is not re-spent on gh");
        assert_eq!(pass.report().latency["inproc"].timeouts, 1);
    }
}
