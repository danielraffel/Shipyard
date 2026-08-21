use super::*;
use std::time::{Duration, Instant};

#[test]
fn contended_store_lock_respects_the_caller_deadline() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = RecoveryStore::new(temp.path()).expect("store");
    let held = store.lock().expect("held lock");
    let bounded = store
        .clone()
        .with_lock_deadline(Instant::now() + Duration::from_millis(50));

    let started = Instant::now();
    let error = bounded
        .pending(1)
        .expect_err("contended store lock must time out");
    assert!(error.to_string().contains("timed out acquiring exclusive"));
    assert!(started.elapsed() < Duration::from_secs(1));
    drop(held);
}
