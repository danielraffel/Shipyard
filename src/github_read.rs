//! GitHub reads answered inside the Shipyard process.
//!
//! A steward read through `gh` runs the `ghapp` wrapper chain: a trampoline,
//! `lsof`, the wrapper itself, `shipyard auth helper-argv`, several `python
//! -I` processes, `openssl` and a token mint before `gh` starts. That is about
//! ten processes and over a second per read when the host is idle, and several
//! seconds when it is not, so a fifteen-second read timeout fails whenever the
//! host stalls.
//!
//! [`InprocReads`] answers the same reads with one kept-alive HTTPS pool and an
//! installation token minted here and reused until shortly before it expires.
//! It uses the App and key the wrapper would have used (see
//! [`credential::AppCredential`]) and the installation GitHub reports for the
//! exact repository, as the wrapper's token helper does.
//!
//! Only reads come here. [`argv::classify`] accepts the exact `gh` shapes the
//! steward's read paths build (`gh api repos/...`, a read-only `gh api graphql`
//! query, and `gh pr view --json` for a known field set) and declines
//! everything else, so every write, and every read it does not fully
//! understand, still runs through `gh` and the wrapper's guards. The answer is
//! shaped as `gh` would have printed it, error text included, so callers and
//! their error classifiers see no difference.
//!
//! Turn it off with `[github.reads] transport = "gh"` in the machine-global
//! config, or `SHIPYARD_GITHUB_READS=gh` for one process.

pub(crate) mod argv;
pub(crate) mod batch;
pub(crate) mod credential;
pub(crate) mod pr_view;
pub(crate) mod transport;

#[cfg(test)]
mod tests;

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Value, json};
use zeroize::Zeroizing;

use crate::config::LoadedConfig;

use argv::{ClassifiedRead, ReadRequest};
use credential::{AppCredential, AppSigner};
use transport::{HttpReply, HttpRequest, Method, RateLimitHeaders, Transport, TransportError};

/// Environment override for one process: `gh` or `inproc`.
pub(crate) const TRANSPORT_ENV: &str = "SHIPYARD_GITHUB_READS";
/// A token this close to expiry is replaced before use.
const TOKEN_REFRESH_LEAD_SECONDS: i64 = 300;
/// Lifetime assumed when GitHub omits `expires_at` (it never has).
const ASSUMED_TOKEN_LIFETIME_SECONDS: i64 = 50 * 60;
/// How long a refused credential sends a repository's reads straight to `gh`.
const REFUSAL_HOLD_SECONDS: i64 = 60;
/// Largest body any read accepts when the caller sets no bound.
const DEFAULT_MAX_BODY: u64 = 32 * 1024 * 1024;
/// Largest body kept for a conditional re-read.
const ETAG_MAX_BODY: usize = 1024 * 1024;
/// Conditional-read entries kept per process.
const ETAG_MAX_ENTRIES: usize = 256;
const API_VERSION: &str = "2022-11-28";
const PR_VIEW_ACCEPT: &str =
    "application/vnd.github.merge-info-preview+json, application/vnd.github.nebula-preview";

/// The read transport the configuration selects.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ReadTransport {
    /// Every read runs `gh`.
    Gh,
    /// Reads the classifier accepts run in process.
    Inproc,
}

/// Which transport `config` (and the process environment) selects.
/// In-process is the default; anything unrecognised selects `gh`.
pub(crate) fn configured_transport(config: &LoadedConfig) -> Result<ReadTransport, String> {
    if let Ok(value) = std::env::var(TRANSPORT_ENV) {
        return parse_transport(&value)
            .ok_or_else(|| format!("{TRANSPORT_ENV}={value:?} is not \"gh\" or \"inproc\""));
    }
    match config.get("github.reads.transport") {
        None => Ok(ReadTransport::Inproc),
        Some(value) => value
            .as_str()
            .and_then(parse_transport)
            .ok_or_else(|| "github.reads.transport must be \"gh\" or \"inproc\"".to_owned()),
    }
}

/// The in-process reader a steward pass uses, or `None` when the
/// configuration selects `gh` or names no canonical App `token_command`. A
/// malformed setting is reported on stderr and selects `gh`.
pub(crate) fn for_steward(config: &LoadedConfig, state_dir: &Path) -> Option<InprocReads> {
    match configured_transport(config) {
        Ok(ReadTransport::Inproc) => {}
        Ok(ReadTransport::Gh) => return None,
        Err(error) => {
            eprintln!("shipyard: {error}; GitHub reads use gh");
            return None;
        }
    }
    let client = crate::gh::GhClient::from_loaded_config(config).ok()?;
    InprocReads::for_token_command(client.command_token_argv()?, Some(state_dir))
}

fn parse_transport(value: &str) -> Option<ReadTransport> {
    match value.trim() {
        "gh" => Some(ReadTransport::Gh),
        "inproc" => Some(ReadTransport::Inproc),
        _ => None,
    }
}

/// What one in-process read produced.
#[derive(Debug)]
pub(crate) enum ReadOutcome {
    /// GitHub answered. `success` and the streams are what `gh` would have
    /// produced for the same answer.
    Answered {
        success: bool,
        stdout: Vec<u8>,
        stderr: String,
        meta: CallMeta,
    },
    /// No answer arrived in time, or the transport failed after retries.
    Failed {
        kind: FailureKind,
        detail: String,
        meta: CallMeta,
    },
    /// Not answered here; the caller runs `gh`. A reason is present when the
    /// read was eligible but could not be served (credential or shape), and
    /// is worth counting.
    Declined { reason: Option<String> },
}

/// Why a read failed without an answer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FailureKind {
    TimedOut,
    TooLarge,
    Transport,
}

/// Per-call facts for the pass report.
#[derive(Clone, Debug, Default)]
pub(crate) struct CallMeta {
    pub(crate) attempts: u32,
    pub(crate) conditional_hit: bool,
    pub(crate) minted: bool,
    pub(crate) rate_limit: Option<RateLimitHeaders>,
}

/// Process-wide counters.
#[cfg(test)]
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct InprocCounters {
    /// Installation tokens minted.
    pub token_mints: u64,
    /// Token reads answered from the in-process cache.
    pub token_cache_hits: u64,
    /// Mints that failed, so the read went to `gh`.
    pub mint_failures: u64,
}

/// An installation token. Its text never appears in `Debug` output and is
/// wiped when dropped.
struct InstallationToken {
    secret: Zeroizing<String>,
    expires_at: i64,
}

impl fmt::Debug for InstallationToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InstallationToken")
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct TokenKey {
    installation_id: u64,
    /// The one repository a scoped token covers.
    scoped_repo: Option<String>,
}

#[derive(Default)]
struct EtagCache {
    entries: HashMap<(u64, String), (String, Vec<u8>)>,
    order: VecDeque<(u64, String)>,
}

impl EtagCache {
    fn get(&self, key: &(u64, String)) -> Option<&(String, Vec<u8>)> {
        self.entries.get(key)
    }

    fn put(&mut self, key: (u64, String), etag: String, body: Vec<u8>) {
        if body.len() > ETAG_MAX_BODY {
            self.entries.remove(&key);
            return;
        }
        if self.entries.insert(key.clone(), (etag, body)).is_none() {
            self.order.push_back(key);
        }
        while self.entries.len() > ETAG_MAX_ENTRIES {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.entries.remove(&oldest);
        }
    }
}

pub(crate) type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

struct Inner {
    credential: AppCredential,
    transport: Transport,
    clock: Clock,
    signer: Mutex<Option<Arc<AppSigner>>>,
    installations: Mutex<HashMap<String, u64>>,
    tokens: Mutex<HashMap<TokenKey, Arc<InstallationToken>>>,
    mint_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// Repositories whose credential was refused recently, until when (by
    /// [`Clock`]), and why: their reads go straight to `gh`.
    refused: Mutex<HashMap<String, (i64, String)>>,
    etags: Mutex<EtagCache>,
    mints: AtomicU64,
    token_hits: AtomicU64,
    mint_failures: AtomicU64,
    batch: Option<batch::BatchSizer>,
}

/// In-process GitHub reads for one credential. Cheap to clone; clones share
/// the connection pool and caches.
#[derive(Clone)]
pub(crate) struct InprocReads {
    inner: Arc<Inner>,
}

impl fmt::Debug for InprocReads {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InprocReads")
            .field("credential", &self.inner.credential)
            .finish_non_exhaustive()
    }
}

impl InprocReads {
    /// The reader for `token_command`, or `None` when it is not the canonical
    /// App command shape. `state_dir` holds the adaptive batch size.
    pub(crate) fn for_token_command(
        token_command: &[String],
        state_dir: Option<&Path>,
    ) -> Option<Self> {
        let credential = AppCredential::from_token_command(token_command)?;
        Some(Self::new(
            credential,
            Transport::github(),
            Arc::new(|| chrono::Utc::now().timestamp()),
            state_dir.map(batch::BatchSizer::persistent),
        ))
    }

    /// A reader against a local test server, with a controllable clock.
    #[cfg(test)]
    pub(crate) fn for_tests(
        credential: AppCredential,
        api_url: &str,
        clock: Clock,
        batch: Option<batch::BatchSizer>,
    ) -> Self {
        Self::new(credential, Transport::for_tests(api_url), clock, batch)
    }

    fn new(
        credential: AppCredential,
        transport: Transport,
        clock: Clock,
        batch: Option<batch::BatchSizer>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                credential,
                transport,
                clock,
                signer: Mutex::new(None),
                installations: Mutex::new(HashMap::new()),
                tokens: Mutex::new(HashMap::new()),
                mint_locks: Mutex::new(HashMap::new()),
                refused: Mutex::new(HashMap::new()),
                etags: Mutex::new(EtagCache::default()),
                mints: AtomicU64::new(0),
                token_hits: AtomicU64::new(0),
                mint_failures: AtomicU64::new(0),
                batch,
            }),
        }
    }

    /// Process-wide counters so far.
    #[cfg(test)]
    pub(crate) fn counters(&self) -> InprocCounters {
        InprocCounters {
            token_mints: self.inner.mints.load(Ordering::Relaxed),
            token_cache_hits: self.inner.token_hits.load(Ordering::Relaxed),
            mint_failures: self.inner.mint_failures.load(Ordering::Relaxed),
        }
    }

    /// The adaptive GraphQL batch sizer, when this reader keeps one.
    pub(crate) fn batch(&self) -> Option<&batch::BatchSizer> {
        self.inner.batch.as_ref()
    }

    /// Answer `args` in process when it is a read this client understands.
    /// `scope_repo` is the repository the caller's client is scoped to;
    /// `max_stdout` bounds the printed answer as `gh`'s capture would.
    pub(crate) fn try_read(
        &self,
        args: &[String],
        scope_repo: Option<&str>,
        timeout: Duration,
        max_stdout: Option<usize>,
    ) -> ReadOutcome {
        let Some(read) = argv::classify(args, scope_repo) else {
            return ReadOutcome::Declined { reason: None };
        };
        let deadline = Instant::now() + timeout;
        // A REST body is printed as is, so the caller's bound applies to it.
        // A GraphQL body is reshaped first; its bound is checked on the output.
        let max_body = match (&read.request, max_stdout) {
            (ReadRequest::Rest { .. }, Some(limit)) => limit as u64,
            _ => DEFAULT_MAX_BODY,
        };
        let mut meta = CallMeta::default();
        let outcome = self.answer(&read, deadline, max_body, &mut meta);
        match outcome {
            Ok((success, stdout, stderr)) => {
                if max_stdout.is_some_and(|limit| stdout.len() > limit) {
                    return ReadOutcome::Failed {
                        kind: FailureKind::TooLarge,
                        detail: "answer exceeds the caller's bound".to_owned(),
                        meta,
                    };
                }
                ReadOutcome::Answered {
                    success,
                    stdout,
                    stderr,
                    meta,
                }
            }
            Err(Unanswered::Decline(reason)) => ReadOutcome::Declined {
                reason: Some(reason),
            },
            Err(Unanswered::Fail(kind, detail)) => ReadOutcome::Failed { kind, detail, meta },
        }
    }

    fn answer(
        &self,
        read: &ClassifiedRead,
        deadline: Instant,
        max_body: u64,
        meta: &mut CallMeta,
    ) -> Result<(bool, Vec<u8>, String), Unanswered> {
        let request = match &read.request {
            ReadRequest::Rest { path } => HttpRequest {
                method: Method::Get,
                path: path.clone(),
                headers: vec![
                    ("Accept", "application/vnd.github+json".to_owned()),
                    ("X-GitHub-Api-Version", API_VERSION.to_owned()),
                ],
                body: None,
            },
            ReadRequest::Graphql { query, variables } => graphql_request(
                &json!({"query": query, "variables": variables}),
                "application/json",
            ),
            ReadRequest::PrView { number, fields } => {
                let (query, variables) = pr_view::query(&read.repo, *number, fields);
                graphql_request(
                    &json!({"query": query, "variables": variables}),
                    PR_VIEW_ACCEPT,
                )
            }
        };
        let reply = self.authorized_send(&read.repo, &request, deadline, max_body, meta)?;
        if let ReadRequest::Rest { .. } = &read.request {
            if (300..400).contains(&reply.status) && reply.status != 304 {
                return Err(Unanswered::Decline(format!(
                    "GitHub redirected a REST read ({})",
                    reply.status
                )));
            }
            return Ok(rest_answer(&reply));
        }
        if !(200..300).contains(&reply.status) {
            if (300..400).contains(&reply.status) {
                return Err(Unanswered::Decline(format!(
                    "GitHub redirected a GraphQL read ({})",
                    reply.status
                )));
            }
            return Ok(match &read.request {
                ReadRequest::PrView { .. } => (
                    false,
                    Vec::new(),
                    match body_message(&reply.body) {
                        Some(message) => format!(
                            "HTTP {}: {message} (https://api.github.com/graphql)",
                            reply.status
                        ),
                        None => format!("HTTP {} (https://api.github.com/graphql)", reply.status),
                    },
                ),
                _ => (false, reply.body.clone(), gh_http_error(&reply)),
            });
        }
        let value = serde_json::from_slice::<Value>(&reply.body).map_err(|_| {
            Unanswered::Decline("GitHub returned a non-JSON GraphQL answer".to_owned())
        })?;
        let errors = value
            .get("errors")
            .and_then(Value::as_array)
            .filter(|errors| !errors.is_empty());
        match &read.request {
            ReadRequest::PrView { fields, .. } => {
                if let Some(errors) = errors {
                    return Ok((false, Vec::new(), pr_view::graphql_error_message(errors)));
                }
                let pull_request = value
                    .pointer("/data/repository/pullRequest")
                    .filter(|pull_request| !pull_request.is_null())
                    .ok_or_else(|| {
                        Unanswered::Decline("pull request missing from answer".to_owned())
                    })?;
                let rendered = pr_view::render(pull_request, fields).map_err(|error| {
                    Unanswered::Decline(match error {
                        pr_view::ShapeError::NeedsPaging(what) => {
                            format!("pr view {what} needs paging")
                        }
                        pr_view::ShapeError::Malformed(what) => format!("pr view answer: {what}"),
                    })
                })?;
                let mut stdout = serde_json::to_vec(&rendered).map_err(|_| {
                    Unanswered::Decline("pr view answer could not be encoded".to_owned())
                })?;
                stdout.push(b'\n');
                Ok((true, stdout, String::new()))
            }
            _ => match errors {
                Some(errors) => Ok((false, reply.body.clone(), gh_graphql_errors(errors))),
                None => Ok((true, reply.body.clone(), String::new())),
            },
        }
    }

    /// Send `request` with the installation token for `repo`. A `401` drops
    /// the cached token and tries once more with a fresh one.
    fn authorized_send(
        &self,
        repo: &str,
        request: &HttpRequest,
        deadline: Instant,
        max_body: u64,
        meta: &mut CallMeta,
    ) -> Result<HttpReply, Unanswered> {
        for fresh_token_retry in [false, true] {
            let (key, token) = self.token_for(repo, deadline, meta)?;
            let mut request = request.clone();
            let etag_key = (request.method == Method::Get)
                .then(|| (key.installation_id, request.path.clone()));
            let cached_etag = etag_key.as_ref().and_then(|etag_key| {
                self.inner
                    .etags
                    .lock()
                    .ok()
                    .and_then(|cache| cache.get(etag_key).map(|(etag, _)| etag.clone()))
            });
            if let Some(etag) = &cached_etag {
                request.headers.push(("If-None-Match", etag.clone()));
            }
            let authorization = Zeroizing::new(format!("token {}", token.secret.as_str()));
            drop(token);
            let sent = self
                .inner
                .transport
                .send(&request, &authorization, deadline, max_body);
            meta.attempts += sent.attempts;
            let mut reply = sent.result.map_err(|error| match error {
                TransportError::TimedOut(total) => Unanswered::Fail(
                    FailureKind::TimedOut,
                    format!("timed out after {}ms", total.as_millis()),
                ),
                TransportError::TooLarge(limit) => Unanswered::Fail(
                    FailureKind::TooLarge,
                    format!("answer exceeds {limit} bytes"),
                ),
                TransportError::Failed(detail) => Unanswered::Fail(FailureKind::Transport, detail),
            })?;
            if reply.rate_limit.is_some() {
                meta.rate_limit.clone_from(&reply.rate_limit);
            }
            if reply.status == 401 && !fresh_token_retry {
                if let Ok(mut tokens) = self.inner.tokens.lock() {
                    tokens.remove(&key);
                }
                continue;
            }
            if let Some(etag_key) = etag_key {
                if reply.status == 304 {
                    let cached = self
                        .inner
                        .etags
                        .lock()
                        .ok()
                        .and_then(|cache| cache.get(&etag_key).map(|(_, body)| body.clone()));
                    let Some(body) = cached else {
                        return Err(Unanswered::Decline(
                            "conditional read lost its cached body".to_owned(),
                        ));
                    };
                    meta.conditional_hit = true;
                    reply.status = 200;
                    reply.body = body;
                } else if reply.status == 200
                    && let Some(etag) = reply.etag.clone()
                    && let Ok(mut cache) = self.inner.etags.lock()
                {
                    cache.put(etag_key, etag, reply.body.clone());
                }
            }
            return Ok(reply);
        }
        unreachable!("the second pass always returns")
    }

    /// The cached installation token for `repo`, minting one when none is
    /// cached or the cached one is within [`TOKEN_REFRESH_LEAD_SECONDS`] of
    /// expiry.
    fn token_for(
        &self,
        repo: &str,
        deadline: Instant,
        meta: &mut CallMeta,
    ) -> Result<(TokenKey, Arc<InstallationToken>), Unanswered> {
        let repo_key = repo.to_ascii_lowercase();
        if let Some(found) = self.cached_token(&repo_key) {
            self.inner.token_hits.fetch_add(1, Ordering::Relaxed);
            return Ok(found);
        }
        if let Some(reason) = self.recently_refused(&repo_key) {
            return Err(Unanswered::Decline(reason));
        }
        // One mint per repository at a time: concurrent readers of the same
        // repository wait for it, then reuse it. A waiter never waits past
        // its own deadline.
        let lock = self
            .inner
            .mint_locks
            .lock()
            .map_err(|_| Unanswered::Decline("token mint lock poisoned".to_owned()))?
            .entry(repo_key.clone())
            .or_default()
            .clone();
        let _mint = loop {
            match lock.try_lock() {
                Ok(guard) => break guard,
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    return Err(Unanswered::Decline("token mint lock poisoned".to_owned()));
                }
                Err(std::sync::TryLockError::WouldBlock) => {
                    if Instant::now() >= deadline {
                        return Err(Unanswered::Fail(
                            FailureKind::TimedOut,
                            "timed out waiting for a token mint".to_owned(),
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        };
        if let Some(found) = self.cached_token(&repo_key) {
            self.inner.token_hits.fetch_add(1, Ordering::Relaxed);
            return Ok(found);
        }
        if let Some(reason) = self.recently_refused(&repo_key) {
            return Err(Unanswered::Decline(reason));
        }
        let minted = self.mint(repo, &repo_key, deadline);
        match minted {
            Ok(found) => {
                self.inner.mints.fetch_add(1, Ordering::Relaxed);
                meta.minted = true;
                Ok(found)
            }
            Err(error) => {
                self.inner.mint_failures.fetch_add(1, Ordering::Relaxed);
                if let Unanswered::Decline(reason) = &error
                    && let Ok(mut refused) = self.inner.refused.lock()
                {
                    // A refused credential stays refused for a while: do not
                    // sign and ask again on every read of the pass.
                    refused.insert(
                        repo_key,
                        ((self.inner.clock)() + REFUSAL_HOLD_SECONDS, reason.clone()),
                    );
                }
                Err(error)
            }
        }
    }

    fn recently_refused(&self, repo_key: &str) -> Option<String> {
        let now = (self.inner.clock)();
        let refused = self.inner.refused.lock().ok()?;
        let (until, reason) = refused.get(repo_key)?;
        (now < *until).then(|| format!("{reason} (held from an earlier attempt)"))
    }

    fn cached_token(&self, repo_key: &str) -> Option<(TokenKey, Arc<InstallationToken>)> {
        let installation_id = *self.inner.installations.lock().ok()?.get(repo_key)?;
        let key = self.token_key(installation_id, repo_key);
        let token = self.inner.tokens.lock().ok()?.get(&key).cloned()?;
        let now = (self.inner.clock)();
        (token.expires_at - now > TOKEN_REFRESH_LEAD_SECONDS).then_some((key, token))
    }

    fn token_key(&self, installation_id: u64, repo_key: &str) -> TokenKey {
        TokenKey {
            installation_id,
            scoped_repo: self
                .inner
                .credential
                .scope_to_repository
                .then(|| repo_key.to_owned()),
        }
    }

    fn signer(&self) -> Result<Arc<AppSigner>, Unanswered> {
        let mut slot = self
            .inner
            .signer
            .lock()
            .map_err(|_| Unanswered::Decline("signer lock poisoned".to_owned()))?;
        if let Some(signer) = slot.as_ref() {
            return Ok(Arc::clone(signer));
        }
        let signer = Arc::new(
            AppSigner::load(&self.inner.credential)
                .map_err(|error| Unanswered::Decline(format!("credential: {error}")))?,
        );
        *slot = Some(Arc::clone(&signer));
        Ok(signer)
    }

    fn mint(
        &self,
        repo: &str,
        repo_key: &str,
        deadline: Instant,
    ) -> Result<(TokenKey, Arc<InstallationToken>), Unanswered> {
        let signer = self.signer()?;
        let now = (self.inner.clock)();
        let jwt = Zeroizing::new(
            signer
                .jwt(now)
                .map_err(|error| Unanswered::Decline(format!("credential: {error}")))?,
        );
        let bearer = Zeroizing::new(format!("Bearer {}", jwt.as_str()));
        let installation_id = self.installation_id(repo, repo_key, &bearer, deadline)?;
        let key = self.token_key(installation_id, repo_key);
        let name = repo.split_once('/').map_or(repo, |(_, name)| name);
        let body = if self.inner.credential.scope_to_repository {
            json!({"repositories": [name]})
        } else {
            json!({})
        };
        let request = HttpRequest {
            method: Method::Post,
            path: format!("app/installations/{installation_id}/access_tokens"),
            headers: vec![
                ("Accept", "application/vnd.github+json".to_owned()),
                ("Content-Type", "application/json".to_owned()),
                ("X-GitHub-Api-Version", API_VERSION.to_owned()),
            ],
            body: Some(body.to_string().into_bytes()),
        };
        let reply = self.credential_send(&request, &bearer, deadline)?;
        if reply.status != 201 && reply.status != 200 {
            return Err(Unanswered::Decline(format!(
                "credential: GitHub refused the installation token mint (HTTP {})",
                reply.status
            )));
        }
        let token = parse_token_reply(
            &reply.body,
            self.inner.credential.scope_to_repository.then_some(name),
            now,
        )
        .map_err(|error| Unanswered::Decline(format!("credential: {error}")))?;
        let token = Arc::new(token);
        if let Ok(mut tokens) = self.inner.tokens.lock() {
            tokens.insert(key.clone(), Arc::clone(&token));
        }
        Ok((key, token))
    }

    fn installation_id(
        &self,
        repo: &str,
        repo_key: &str,
        bearer: &str,
        deadline: Instant,
    ) -> Result<u64, Unanswered> {
        if let Some(id) = self
            .inner
            .installations
            .lock()
            .ok()
            .and_then(|map| map.get(repo_key).copied())
        {
            return Ok(id);
        }
        let request = HttpRequest {
            method: Method::Get,
            path: format!("repos/{repo}/installation"),
            headers: vec![
                ("Accept", "application/vnd.github+json".to_owned()),
                ("X-GitHub-Api-Version", API_VERSION.to_owned()),
            ],
            body: None,
        };
        let reply = self.credential_send(&request, bearer, deadline)?;
        if reply.status != 200 {
            return Err(Unanswered::Decline(format!(
                "credential: no App installation for {repo} (HTTP {})",
                reply.status
            )));
        }
        let id = serde_json::from_slice::<Value>(&reply.body)
            .ok()
            .and_then(|value| value.get("id").and_then(Value::as_u64))
            .filter(|id| *id > 0)
            .ok_or_else(|| {
                Unanswered::Decline(format!(
                    "credential: GitHub did not return an installation id for {repo}"
                ))
            })?;
        if let Ok(mut map) = self.inner.installations.lock() {
            map.insert(repo_key.to_owned(), id);
        }
        Ok(id)
    }

    /// A credential request: a timeout fails the read (`gh` would mint over
    /// the same network), anything else declines it to `gh`.
    fn credential_send(
        &self,
        request: &HttpRequest,
        authorization: &str,
        deadline: Instant,
    ) -> Result<HttpReply, Unanswered> {
        self.inner
            .transport
            .send(request, authorization, deadline, 1024 * 1024)
            .result
            .map_err(|error| match error {
                TransportError::TimedOut(total) => Unanswered::Fail(
                    FailureKind::TimedOut,
                    format!("token mint timed out after {}ms", total.as_millis()),
                ),
                TransportError::TooLarge(_) => {
                    Unanswered::Decline("credential: oversized mint answer".to_owned())
                }
                TransportError::Failed(detail) => {
                    Unanswered::Decline(format!("credential: mint transport failed ({detail})"))
                }
            })
    }
}

enum Unanswered {
    Decline(String),
    Fail(FailureKind, String),
}

fn graphql_request(body: &Value, accept: &str) -> HttpRequest {
    HttpRequest {
        method: Method::Post,
        path: "graphql".to_owned(),
        headers: vec![
            ("Accept", accept.to_owned()),
            ("Content-Type", "application/json; charset=utf-8".to_owned()),
            ("GraphQL-Features", "merge_queue".to_owned()),
            ("X-GitHub-Api-Version", API_VERSION.to_owned()),
        ],
        body: Some(body.to_string().into_bytes()),
    }
}

/// `gh api`'s answer to a REST reply: the body on stdout, and on a non-2xx
/// status `gh: MESSAGE (HTTP STATUS)` on stderr.
fn rest_answer(reply: &HttpReply) -> (bool, Vec<u8>, String) {
    if (200..300).contains(&reply.status) {
        return (true, reply.body.clone(), String::new());
    }
    (false, reply.body.clone(), gh_http_error(reply))
}

fn gh_http_error(reply: &HttpReply) -> String {
    match body_message(&reply.body) {
        Some(message) => format!("gh: {message} (HTTP {})", reply.status),
        None => format!("gh: HTTP {}", reply.status),
    }
}

fn body_message(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<Value>(body)
        .ok()?
        .get("message")?
        .as_str()
        .filter(|message| !message.is_empty())
        .map(str::to_owned)
}

/// `gh api graphql`'s stderr for an answer that carries errors.
fn gh_graphql_errors(errors: &[Value]) -> String {
    let messages = errors
        .iter()
        .filter_map(|error| error.get("message").and_then(Value::as_str))
        .collect::<Vec<_>>();
    format!("gh: {}", messages.join("\n"))
}

/// Parse an access-token reply, refusing a token with whitespace or control
/// characters and, for a scoped request, a token that covers anything other
/// than exactly `scoped_name`.
fn parse_token_reply(
    body: &[u8],
    scoped_name: Option<&str>,
    now: i64,
) -> Result<InstallationToken, String> {
    let value = serde_json::from_slice::<Value>(body)
        .map_err(|_| "installation token answer is not JSON".to_owned())?;
    let secret = value
        .get("token")
        .and_then(Value::as_str)
        .filter(|token| {
            !token.is_empty()
                && token.len() <= 512
                && token.bytes().all(|byte| byte.is_ascii_graphic())
        })
        .ok_or_else(|| "GitHub response did not include a usable installation token".to_owned())?;
    if let Some(name) = scoped_name {
        let names = value
            .get("repositories")
            .and_then(Value::as_array)
            .map(|repositories| {
                repositories
                    .iter()
                    .map(|repository| repository.get("name").and_then(Value::as_str))
                    .collect::<Option<Vec<_>>>()
            });
        let exact = matches!(names, Some(Some(names)) if names.len() == 1 && names[0].eq_ignore_ascii_case(name));
        if !exact {
            return Err("installation token covers more than the requested repository".to_owned());
        }
    }
    let expires_at = value
        .get("expires_at")
        .and_then(Value::as_str)
        .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
        .map_or(now + ASSUMED_TOKEN_LIFETIME_SECONDS, |at| at.timestamp());
    Ok(InstallationToken {
        secret: Zeroizing::new(secret.to_owned()),
        expires_at,
    })
}
