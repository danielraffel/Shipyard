use std::cell::RefCell;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use super::*;

const REPO: &str = "Generous-Corp/pulp";

/// A fake `gh` that answers every log request with a log naming the job, and
/// records each request's job id.
struct FakeGh {
    requests: RefCell<Vec<u64>>,
    missing: Vec<u64>,
    broken: Vec<u64>,
}

impl FakeGh {
    fn new() -> Self {
        Self {
            requests: RefCell::new(Vec::new()),
            missing: Vec::new(),
            broken: Vec::new(),
        }
    }

    fn answer(&self, argv: &[String]) -> Result<String, String> {
        let path = argv
            .iter()
            .find(|arg| arg.starts_with("repos/"))
            .expect("a REST path");
        let id: u64 = path
            .trim_end_matches("/logs")
            .rsplit('/')
            .next()
            .and_then(|id| id.parse().ok())
            .expect("a job id");
        self.requests.borrow_mut().push(id);
        if self.missing.contains(&id) {
            return Err("gh: Not Found (HTTP 404)".to_owned());
        }
        if self.broken.contains(&id) {
            return Err("gh: Server Error (HTTP 502)".to_owned());
        }
        Ok(format!("log of job {id}\n##[error]boom\n"))
    }

    fn reader(&self) -> impl Fn(&[String]) -> Result<String, String> + '_ {
        |argv: &[String]| self.answer(argv)
    }

    fn requests(&self) -> Vec<u64> {
        self.requests.borrow().clone()
    }
}

#[allow(clippy::unnecessary_wraps)] // must match the lease function type
fn granted(_: &Path) -> io::Result<Option<ProductionWriterDomainLease>> {
    Ok(None)
}

fn policy(per_pass: usize, per_minute: usize) -> JobLogPolicy {
    JobLogPolicy {
        max_fetches_per_pass: per_pass,
        max_fetches_per_minute: per_minute,
        max_wait: Duration::ZERO,
        ..JobLogPolicy::default()
    }
}

fn store(dir: &Path, policy: &JobLogPolicy) -> JobLogStore {
    JobLogStore::open_with_lease(dir, policy, granted).expect("store")
}

fn own_window() -> &'static RateWindow {
    Box::leak(Box::new(RateWindow::default()))
}

fn pass(dir: &Path, policy: JobLogPolicy) -> JobLogPass {
    let store = store(dir, &policy);
    JobLogPass::with_window(Some(store), policy, own_window())
}

#[test]
fn a_cache_hit_sends_no_request() {
    let root = tempfile::tempdir().expect("temp");
    let gh = FakeGh::new();
    let first = pass(root.path(), policy(10, 100));
    let log = first.read(&gh.reader(), REPO, 7).expect("first read");
    assert_eq!(gh.requests(), vec![7]);

    // A later pass (a new process, a restarted daemon) reads the same job.
    let second = pass(root.path(), policy(10, 100));
    assert_eq!(second.read(&gh.reader(), REPO, 7).expect("cached"), log);
    assert_eq!(
        gh.requests(),
        vec![7],
        "a cached completed log was refetched"
    );
    assert_eq!(
        second.stats(),
        JobLogStats {
            fetched: 0,
            cached: 1,
            deferred: 0
        }
    );
}

#[test]
fn the_pass_budget_holds() {
    let root = tempfile::tempdir().expect("temp");
    let gh = FakeGh::new();
    let logs = pass(root.path(), policy(2, 100));
    let results: Vec<_> = (1..=5)
        .map(|id| logs.read(&gh.reader(), REPO, id))
        .collect();
    assert_eq!(
        gh.requests(),
        vec![1, 2],
        "more requests than the pass budget"
    );
    assert!(results[..2].iter().all(Result::is_ok));
    for result in &results[2..] {
        let error = result.as_ref().expect_err("deferred");
        assert!(is_deferred(error), "not reported as deferred: {error}");
    }
    assert_eq!(logs.stats().deferred, 3);
}

#[test]
fn the_per_minute_ceiling_holds_across_passes() {
    let root = tempfile::tempdir().expect("temp");
    let gh = FakeGh::new();
    let window = own_window();
    let make = || {
        JobLogPass::with_window(
            Some(store(root.path(), &policy(100, 3))),
            policy(100, 3),
            window,
        )
    };
    let first = make();
    let second = make();
    for id in 1..=2 {
        first
            .read(&gh.reader(), REPO, id)
            .expect("within the minute");
    }
    second
        .read(&gh.reader(), REPO, 3)
        .expect("within the minute");
    let error = second
        .read(&gh.reader(), REPO, 4)
        .expect_err("over the minute");
    assert!(is_deferred(&error), "{error}");
    assert_eq!(gh.requests(), vec![1, 2, 3]);
}

#[test]
fn the_rate_window_slides() {
    let window = RateWindow::default();
    let start = Instant::now();
    assert!(window.try_take(start, 2));
    assert!(window.try_take(start, 2));
    assert!(!window.try_take(start + Duration::from_secs(59), 2));
    assert!(window.try_take(start + Duration::from_secs(60), 2));
}

#[test]
fn a_backfill_resumes_where_the_last_pass_stopped() {
    let root = tempfile::tempdir().expect("temp");
    let gh = FakeGh::new();
    let backlog: Vec<u64> = (1..=5).collect();
    let mut done = Vec::new();
    for _ in 0..3 {
        let logs = pass(root.path(), policy(2, 100));
        for id in &backlog {
            if logs.read(&gh.reader(), REPO, *id).is_ok() && !done.contains(id) {
                done.push(*id);
            }
        }
    }
    assert_eq!(done, backlog, "the backlog did not drain");
    assert_eq!(
        gh.requests(),
        vec![1, 2, 3, 4, 5],
        "a resumed backfill refetched a log it already had"
    );
}

#[test]
fn a_cold_cache_under_the_cap_reads_every_log() {
    let root = tempfile::tempdir().expect("temp");
    let gh = FakeGh::new();
    let logs = pass(root.path(), policy(10, 100));
    for id in 1..=4 {
        assert_eq!(
            logs.read(&gh.reader(), REPO, id).expect("read"),
            format!("log of job {id}\n##[error]boom\n")
        );
    }
    assert_eq!(gh.requests(), vec![1, 2, 3, 4]);
    assert_eq!(logs.stats().deferred, 0);
}

#[test]
fn without_a_store_every_read_is_a_request() {
    let gh = FakeGh::new();
    let logs = JobLogPass::with_window(None, policy(10, 100), own_window());
    logs.read(&gh.reader(), REPO, 9).expect("read");
    logs.read(&gh.reader(), REPO, 9).expect("read");
    assert_eq!(gh.requests(), vec![9, 9]);
}

#[test]
fn a_permanent_miss_is_remembered_and_a_transient_error_is_not() {
    let root = tempfile::tempdir().expect("temp");
    let mut gh = FakeGh::new();
    gh.missing = vec![1];
    gh.broken = vec![2];
    let first = pass(root.path(), policy(10, 100));
    assert!(first.read(&gh.reader(), REPO, 1).is_err());
    assert!(first.read(&gh.reader(), REPO, 2).is_err());
    let second = pass(root.path(), policy(10, 100));
    let error = second
        .read(&gh.reader(), REPO, 1)
        .expect_err("still missing");
    assert!(error.contains("HTTP 404"), "{error}");
    assert!(second.read(&gh.reader(), REPO, 2).is_err());
    assert_eq!(gh.requests(), vec![1, 2, 2], "a 404 was re-asked");
}

#[test]
fn an_expired_miss_is_asked_again() {
    let root = tempfile::tempdir().expect("temp");
    let mut gh = FakeGh::new();
    gh.missing = vec![1];
    let no_memory = JobLogPolicy {
        missing_ttl: Duration::ZERO,
        ..policy(10, 100)
    };
    let first = pass(root.path(), no_memory.clone());
    assert!(first.read(&gh.reader(), REPO, 1).is_err());
    let second = pass(root.path(), no_memory);
    assert!(second.read(&gh.reader(), REPO, 1).is_err());
    assert_eq!(gh.requests(), vec![1, 1]);
}

#[test]
fn eviction_drops_the_least_recently_used_log() {
    let root = tempfile::tempdir().expect("temp");
    let entry = format!("{LOG_MAGIC} {}:1 100\n", REPO.to_ascii_lowercase()).len() as u64 + 100;
    let small = JobLogPolicy {
        cache_max_bytes: entry * 2 + entry / 2,
        ..JobLogPolicy::default()
    };
    let cache = store(root.path(), &small);
    let log = "x".repeat(100);
    cache.put_log(REPO, 1, &log);
    std::thread::sleep(Duration::from_millis(20));
    cache.put_log(REPO, 2, &log);
    std::thread::sleep(Duration::from_millis(20));
    // Reading job 1 makes job 2 the least recently used.
    assert!(cache.get(REPO, 1).is_some());
    std::thread::sleep(Duration::from_millis(20));
    cache.put_log(REPO, 3, &log);
    assert!(cache.get(REPO, 1).is_some(), "evicted a recently used log");
    assert!(
        cache.get(REPO, 2).is_none(),
        "kept the least recently used log"
    );
    assert!(cache.get(REPO, 3).is_some(), "evicted the newest log");
    assert!(cache.bytes() <= small.cache_max_bytes);
}

#[test]
fn an_oversized_log_is_returned_but_not_cached() {
    let root = tempfile::tempdir().expect("temp");
    let tiny = JobLogPolicy {
        max_entry_bytes: 4,
        ..JobLogPolicy::default()
    };
    let cache = store(root.path(), &tiny);
    cache.put_log(REPO, 1, "longer than four bytes");
    assert_eq!(cache.get(REPO, 1), None);
}

#[test]
fn a_foreign_entry_is_a_miss() {
    let root = tempfile::tempdir().expect("temp");
    let cache = store(root.path(), &JobLogPolicy::default());
    cache.put_log(REPO, 1, "log");
    let key = JobLogStore::key(REPO, 1);
    let path = cache.path(&key);
    std::fs::write(&path, format!("{LOG_MAGIC} other/repo:1 3\nlog")).expect("write");
    assert_eq!(cache.get(REPO, 1), None);
}

#[test]
fn a_torn_entry_is_a_miss() {
    let root = tempfile::tempdir().expect("temp");
    let cache = store(root.path(), &JobLogPolicy::default());
    cache.put_log(REPO, 1, "a full log");
    assert_eq!(
        cache.get(REPO, 1),
        Some(Cached::Log("a full log".to_owned()))
    );
    let path = cache.path(&JobLogStore::key(REPO, 1));
    let text = std::fs::read_to_string(&path).expect("read");
    std::fs::write(&path, &text[..text.len() - 3]).expect("truncate");
    assert_eq!(cache.get(REPO, 1), None, "served a truncated log");
}

static LEASE_CALLS: AtomicUsize = AtomicUsize::new(0);

/// Refuses the first lease, grants every later one.
fn refuses_once(_: &Path) -> io::Result<Option<ProductionWriterDomainLease>> {
    if LEASE_CALLS.fetch_add(1, Ordering::SeqCst) == 0 {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "audit holds the domain",
        ));
    }
    Ok(None)
}

#[test]
fn one_refused_lease_skips_one_write_not_the_rest() {
    let root = tempfile::tempdir().expect("temp");
    LEASE_CALLS.store(0, Ordering::SeqCst);
    let cache = JobLogStore::open_with_lease(root.path(), &JobLogPolicy::default(), refuses_once)
        .expect("store");
    cache.put_log(REPO, 1, "first");
    cache.put_log(REPO, 2, "second");
    assert_eq!(cache.get(REPO, 1), None, "wrote without the lease");
    assert_eq!(
        cache.get(REPO, 2),
        Some(Cached::Log("second".to_owned())),
        "a single refused lease stopped later writes"
    );
}

#[test]
fn a_dedicated_reader_carries_the_request_and_falls_back_when_not_allowed() {
    let gh = FakeGh::new();
    let dedicated = JobLogPass::with_window(None, policy(10, 100), own_window()).with_reader(
        Box::new(|argv: &[String]| {
            if argv.iter().any(|arg| arg.contains("/jobs/2/")) {
                Err("gh: Resource not accessible by integration (HTTP 403)".to_owned())
            } else {
                Ok("from the dedicated credential".to_owned())
            }
        }),
    );
    assert_eq!(
        dedicated.read(&gh.reader(), REPO, 1).expect("read"),
        "from the dedicated credential"
    );
    assert!(
        gh.requests().is_empty(),
        "the shared reader carried the request"
    );
    dedicated.read(&gh.reader(), REPO, 2).expect("fallback");
    assert_eq!(gh.requests(), vec![2]);
}

#[test]
fn the_policy_reads_its_table() {
    let root = tempfile::tempdir().expect("temp");
    std::fs::write(
        root.path().join("config.toml"),
        "[github.job_logs]\nmax_fetches_per_pass = 7\nmax_fetches_per_minute = 5\n\
         cache_max_mb = 3\nmissing_ttl_hours = 2\nadmission_auth = true\n",
    )
    .expect("config");
    let config =
        LoadedConfig::load_machine_global_from_dir(root.path().to_path_buf()).expect("load");
    let (policy, warnings) = JobLogPolicy::from_config(&config);
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(policy.max_fetches_per_pass, 7);
    assert_eq!(policy.max_fetches_per_minute, 5);
    assert_eq!(policy.cache_max_bytes, 3 * 1024 * 1024);
    assert_eq!(policy.missing_ttl, Duration::from_hours(2));
    assert!(policy.admission_auth);

    std::fs::write(
        root.path().join("config.toml"),
        "[github.job_logs]\nmax_fetches_per_pass = -1\nadmission_auth = \"yes\"\n\
         max_wait_seconds = 18446744073709551\n",
    )
    .expect("config");
    let config =
        LoadedConfig::load_machine_global_from_dir(root.path().to_path_buf()).expect("load");
    let (policy, warnings) = JobLogPolicy::from_config(&config);
    assert_eq!(
        policy,
        JobLogPolicy {
            max_wait: MAX_WAIT_CEILING,
            ..JobLogPolicy::default()
        }
    );
    assert_eq!(warnings.len(), 3, "{warnings:?}");
}

#[test]
fn a_full_minute_waits_for_a_slot_instead_of_deferring() {
    let window = own_window();
    let start = Instant::now()
        .checked_sub(Duration::from_millis(59_700))
        .expect("a monotonic clock older than a minute");
    assert!(window.try_take(start, 1));
    // The only slot frees 300 ms from now: a short wait takes it.
    assert!(window.take(1, Duration::from_secs(2)));
    // Now it is taken for a minute: no wait means no slot.
    assert!(!window.take(1, Duration::ZERO));
}

#[test]
fn concurrent_reads_never_exceed_the_pass_budget() {
    use std::sync::Mutex;
    let requests = Mutex::new(0_usize);
    let gh = |_: &[String]| -> Result<String, String> {
        *requests.lock().expect("requests") += 1;
        std::thread::sleep(Duration::from_millis(2));
        Ok("log".to_owned())
    };
    let logs = JobLogPass::with_window(None, policy(5, 1_000), own_window());
    let deferred = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for worker in 0..8_u64 {
            let (logs, gh, deferred) = (&logs, &gh, &deferred);
            scope.spawn(move || {
                for id in 0..10 {
                    if let Err(error) = logs.read(gh, REPO, worker * 100 + id) {
                        assert!(is_deferred(&error), "{error}");
                        deferred.fetch_add(1, Ordering::Relaxed);
                    }
                }
            });
        }
    });
    assert_eq!(*requests.lock().expect("requests"), 5);
    assert_eq!(deferred.load(Ordering::Relaxed), 75);
    assert_eq!(logs.stats().fetched, 5);
}

#[test]
fn a_fallback_is_counted_as_two_requests() {
    let gh = FakeGh::new();
    let logs = JobLogPass::with_window(None, policy(10, 100), own_window()).with_reader(Box::new(
        |_: &[String]| Err("gh: Bad credentials (HTTP 401)".to_owned()),
    ));
    logs.read(&gh.reader(), REPO, 1).expect("fallback answer");
    assert_eq!(gh.requests(), vec![1]);
    assert_eq!(
        logs.stats().fetched,
        2,
        "the fallback request went uncounted"
    );
}

#[test]
fn a_404_is_remembered_for_less_time_than_a_410() {
    let root = tempfile::tempdir().expect("temp");
    let cache = store(root.path(), &JobLogPolicy::default());
    cache.put_missing(REPO, 1, "gh: Not Found (HTTP 404)", Duration::ZERO);
    cache.put_missing(REPO, 2, "gh: Gone (HTTP 410)", Duration::from_hours(24));
    assert_eq!(cache.get(REPO, 1), None, "an expired 404 was still served");
    assert!(matches!(cache.get(REPO, 2), Some(Cached::Missing(_))));
}

#[test]
fn the_default_pass_is_unbounded() {
    let gh = FakeGh::new();
    let logs = JobLogPass::default();
    for id in 0..150 {
        logs.read(&gh.reader(), REPO, id)
            .expect("an unconfigured caller lost a read");
    }
    assert_eq!(logs.stats().deferred, 0);
}
