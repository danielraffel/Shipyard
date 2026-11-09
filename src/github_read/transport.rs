//! One kept-alive HTTPS connection pool to GitHub, with bounded attempts.
//!
//! Every attempt has its own timeout inside the caller's deadline, so a stalled
//! connection is abandoned and retried on a fresh one instead of spending the
//! whole budget. Only failures a fresh attempt can plausibly cure are retried:
//! transport errors and `500`/`502`/`503`/`504`. A rate limit, an
//! authorization failure or a not-found answer is returned at once, exactly as
//! GitHub sent it. Retries wait an exponential backoff with full jitter.

use std::time::{Duration, Instant};

use ring::rand::SecureRandom as _;

/// Longest one attempt may run. A request that has not answered by then is
/// abandoned and retried on a new connection while the deadline allows.
pub(crate) const ATTEMPT_TIMEOUT_CAP: Duration = Duration::from_secs(10);
/// A retry with less than this left before the deadline is not started.
pub(crate) const RETRY_FLOOR: Duration = Duration::from_millis(750);
/// Attempts per request, the first included.
pub(crate) const MAX_ATTEMPTS: u32 = 3;
/// First backoff step; each retry doubles it, and the wait is jittered.
const BACKOFF_BASE: Duration = Duration::from_millis(200);

/// A request, before credentials are attached.
#[derive(Clone, Debug)]
pub(crate) struct HttpRequest {
    pub(crate) method: Method,
    /// Path and query below the API root, without a leading slash.
    pub(crate) path: String,
    /// `(name, value)` headers. Never includes `Authorization`.
    pub(crate) headers: Vec<(&'static str, String)>,
    pub(crate) body: Option<Vec<u8>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Method {
    Get,
    Post,
}

/// What GitHub answered.
#[derive(Clone, Debug)]
pub(crate) struct HttpReply {
    pub(crate) status: u16,
    pub(crate) body: Vec<u8>,
    pub(crate) etag: Option<String>,
    pub(crate) rate_limit: Option<RateLimitHeaders>,
}

/// `x-ratelimit-*` on one reply.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RateLimitHeaders {
    pub(crate) resource: String,
    pub(crate) remaining: u64,
    pub(crate) used: u64,
}

/// A request that produced no usable reply.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TransportError {
    /// The deadline passed. Carries the total time allowed.
    TimedOut(Duration),
    /// Something other than a timeout went wrong talking to GitHub.
    Failed(String),
    /// The reply was larger than the caller allows.
    TooLarge(u64),
}

/// The outcome of [`Transport::send`], with how many attempts it took.
pub(crate) struct Sent {
    pub(crate) result: Result<HttpReply, TransportError>,
    pub(crate) attempts: u32,
}

#[derive(Clone, Debug)]
pub(crate) struct Transport {
    agent: ureq::Agent,
    api_url: String,
}

impl Transport {
    /// A pool to `https://api.github.com`.
    pub(crate) fn github() -> Self {
        Self::build("https://api.github.com", true)
    }

    /// A pool to a local test server. Plain HTTP is allowed only here.
    #[cfg(test)]
    pub(crate) fn for_tests(api_url: &str) -> Self {
        Self::build(api_url, false)
    }

    /// The production transport's settings against `api_url`, so a test can
    /// show it refuses plain HTTP.
    #[cfg(test)]
    pub(crate) fn https_only_for_tests(api_url: &str) -> Self {
        Self::build(api_url, true)
    }

    fn build(api_url: &str, https_only: bool) -> Self {
        let agent = ureq::Agent::config_builder()
            .https_only(https_only)
            .http_status_as_error(false)
            // A redirect would leave the endpoint the request was authorized
            // for; it is surfaced instead of followed.
            .max_redirects(0)
            .max_idle_connections(16)
            .max_idle_connections_per_host(8)
            .max_idle_age(Duration::from_secs(50))
            .user_agent(format!(
                "shipyard/{} (in-process reads)",
                env!("CARGO_PKG_VERSION")
            ))
            .build()
            .new_agent();
        Self {
            agent,
            api_url: api_url.trim_end_matches('/').to_owned(),
        }
    }

    /// Send `request` with `authorization`, retrying transient failures until
    /// `deadline`. `max_body` bounds the reply body.
    pub(crate) fn send(
        &self,
        request: &HttpRequest,
        authorization: &str,
        deadline: Instant,
        max_body: u64,
    ) -> Sent {
        let started = Instant::now();
        let total = deadline.saturating_duration_since(started);
        let mut attempts = 0;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Sent {
                    result: Err(TransportError::TimedOut(total)),
                    attempts,
                };
            }
            attempts += 1;
            let result = self.attempt(
                request,
                authorization,
                remaining.min(ATTEMPT_TIMEOUT_CAP),
                max_body,
            );
            let retryable = match &result {
                Ok(reply) => matches!(reply.status, 500 | 502 | 503 | 504),
                Err(Attempt::Transient(_)) => true,
                Err(Attempt::Fatal(_) | Attempt::TooLarge(_)) => false,
            };
            let left = deadline.saturating_duration_since(Instant::now());
            if !retryable || attempts >= MAX_ATTEMPTS || left < RETRY_FLOOR {
                let result = result.map_err(|error| match error {
                    Attempt::Transient(Failure::TimedOut) => TransportError::TimedOut(total),
                    Attempt::Transient(Failure::Other(detail)) | Attempt::Fatal(detail) => {
                        TransportError::Failed(detail)
                    }
                    Attempt::TooLarge(limit) => TransportError::TooLarge(limit),
                });
                return Sent { result, attempts };
            }
            let wait = backoff(attempts).min(left.saturating_sub(RETRY_FLOOR / 2));
            std::thread::sleep(wait);
        }
    }

    fn attempt(
        &self,
        request: &HttpRequest,
        authorization: &str,
        timeout: Duration,
        max_body: u64,
    ) -> Result<HttpReply, Attempt> {
        let url = format!("{}/{}", self.api_url, request.path);
        let outcome = match request.method {
            Method::Get => {
                let mut builder = self.agent.get(&url).header("Authorization", authorization);
                for (name, value) in &request.headers {
                    builder = builder.header(*name, value);
                }
                builder
                    .config()
                    .timeout_global(Some(timeout))
                    .build()
                    .call()
            }
            Method::Post => {
                let mut builder = self.agent.post(&url).header("Authorization", authorization);
                for (name, value) in &request.headers {
                    builder = builder.header(*name, value);
                }
                builder
                    .config()
                    .timeout_global(Some(timeout))
                    .build()
                    .send(request.body.clone().unwrap_or_default())
            }
        };
        let mut response = outcome.map_err(classify_error)?;
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        };
        let etag = header("etag");
        let rate_limit = match (
            header("x-ratelimit-resource"),
            header("x-ratelimit-remaining").and_then(|value| value.parse().ok()),
            header("x-ratelimit-used").and_then(|value| value.parse().ok()),
        ) {
            (Some(resource), Some(remaining), Some(used)) => Some(RateLimitHeaders {
                resource,
                remaining,
                used,
            }),
            _ => None,
        };
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .with_config()
            .limit(max_body)
            .read_to_vec()
            .map_err(|error| match error {
                ureq::Error::BodyExceedsLimit(limit) => Attempt::TooLarge(limit),
                other => classify_error(other),
            })?;
        Ok(HttpReply {
            status,
            body,
            etag,
            rate_limit,
        })
    }
}

enum Failure {
    TimedOut,
    Other(String),
}

enum Attempt {
    Transient(Failure),
    Fatal(String),
    TooLarge(u64),
}

/// Map a `ureq` error to a retry class. The message names the failure kind
/// only; it never echoes a header, so no credential can reach it.
fn classify_error(error: ureq::Error) -> Attempt {
    match error {
        ureq::Error::Timeout(_) => Attempt::Transient(Failure::TimedOut),
        ureq::Error::Io(io) => Attempt::Transient(Failure::Other(format!(
            "connection failed ({:?})",
            io.kind()
        ))),
        ureq::Error::HostNotFound => {
            Attempt::Transient(Failure::Other("host not found".to_owned()))
        }
        ureq::Error::ConnectionFailed => {
            Attempt::Transient(Failure::Other("connection failed".to_owned()))
        }
        ureq::Error::Protocol(_) => {
            Attempt::Transient(Failure::Other("HTTP protocol error".to_owned()))
        }
        ureq::Error::BodyExceedsLimit(limit) => Attempt::TooLarge(limit),
        ureq::Error::Tls(_) => Attempt::Fatal("TLS failure".to_owned()),
        ureq::Error::RequireHttpsOnly(_) => Attempt::Fatal("plain HTTP refused".to_owned()),
        _ => Attempt::Fatal("request failed".to_owned()),
    }
}

/// Full jitter: a uniformly random wait in `[0, base * 2^(attempt-1))`,
/// never less than a tenth of the step so retries do not stampede.
pub(crate) fn backoff(attempt: u32) -> Duration {
    let step = BACKOFF_BASE.saturating_mul(1 << attempt.saturating_sub(1).min(6));
    let mut bytes = [0_u8; 4];
    let fraction = if SystemRandom::new().fill(&mut bytes).is_ok() {
        f64::from(u32::from_le_bytes(bytes)) / f64::from(u32::MAX)
    } else {
        0.5
    };
    step.mul_f64(fraction.max(0.1))
}

use ring::rand::SystemRandom;
