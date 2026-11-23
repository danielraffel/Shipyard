use std::cell::RefCell;

use super::*;
use crate::base_health::merge_group::{MergeGroupHealth, PoisonedContext};
use crate::base_health::{BasePoisonPolicy, PoisonHold};

const REPO: &str = "Generous-Corp/vellum";
const TIP: &str = "61bda8ea0000000000000000000000000000beef";

fn poisoned(fixes: &[u64], red_since: &str) -> RepoPoison {
    let health = MergeGroupHealth::Poisoned {
        contexts: vec![PoisonedContext {
            context: "provenance-verify".to_owned(),
            first_error: None,
            groups: Vec::new(),
            latest_failure_at: None,
            check_id: None,
            same_first_error: None,
        }],
    };
    let mut tip = super::super::red_tip(&health);
    for reading in tip.values_mut() {
        reading.sha = Some(TIP.to_owned());
        reading.red_since = Some(
            DateTime::parse_from_rfc3339(red_since)
                .expect("time")
                .with_timezone(&Utc),
        );
    }
    RepoPoison {
        policy: BasePoisonPolicy::for_base(true),
        health,
        fixes: fixes
            .iter()
            .map(|pr| (*pr, vec!["label `shipyard:unblock`".to_owned()]))
            .collect(),
        candidates: std::collections::BTreeSet::new(),
        fix_read_error: None,
        tip,
        red_everywhere: BTreeMap::new(),
        covering: std::collections::BTreeSet::new(),
        fix_ambiguity: None,
    }
}

fn at(text: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(text)
        .expect("time")
        .with_timezone(&Utc)
}

struct Run<'a> {
    dir: &'a Path,
    posts: RefCell<Vec<(u64, String)>>,
}

impl Run<'_> {
    fn pass(&self, mode: PoisonHold, poison: &RepoPoison, red: &[&str], now: &str) -> Vec<String> {
        let red: Vec<String> = red.iter().map(|check| (*check).to_owned()).collect();
        notify(
            mode,
            &|pr, text| {
                self.posts.borrow_mut().push((pr, text.to_owned()));
                Ok(())
            },
            self.dir,
            REPO,
            "main",
            poison,
            3,
            &|_| Some(red.clone()),
            at(now),
        )
    }
}

#[test]
fn one_comment_per_episode_and_none_on_the_next_pass() {
    let temp = tempfile::tempdir().expect("temp");
    let run = Run {
        dir: temp.path(),
        posts: RefCell::new(Vec::new()),
    };
    let poison = poisoned(&[719], "2026-11-23T16:51:00Z");
    let first = run.pass(PoisonHold::On, &poison, &[], "2026-11-23T17:00:00Z");
    assert_eq!(first, vec!["fix_notice #719: posted"]);
    let second = run.pass(PoisonHold::On, &poison, &[], "2026-11-23T17:05:00Z");
    assert_eq!(second, vec!["fix_notice #719: already posted this episode"]);
    let posts = run.posts.borrow();
    assert_eq!(posts.len(), 1, "{posts:?}");
    let text = &posts[0].1;
    assert!(text.contains("`provenance-verify`"), "{text}");
    assert!(text.contains("`61bda8ea0000`"), "{text}");
    assert!(text.contains("holding 3 approved"), "{text}");
}

#[test]
fn a_new_episode_or_a_new_fix_pick_posts_again() {
    let temp = tempfile::tempdir().expect("temp");
    let run = Run {
        dir: temp.path(),
        posts: RefCell::new(Vec::new()),
    };
    run.pass(
        PoisonHold::On,
        &poisoned(&[719], "2026-11-23T16:51:00Z"),
        &[],
        "2026-11-23T17:00:00Z",
    );
    // The base recovered and went red again: a different start.
    run.pass(
        PoisonHold::On,
        &poisoned(&[719], "2026-11-24T09:00:00Z"),
        &[],
        "2026-11-24T09:05:00Z",
    );
    // Same episode, the pick moved to another pull request.
    run.pass(
        PoisonHold::On,
        &poisoned(&[720], "2026-11-24T09:00:00Z"),
        &[],
        "2026-11-24T09:10:00Z",
    );
    let numbers: Vec<u64> = run.posts.borrow().iter().map(|post| post.0).collect();
    assert_eq!(numbers, vec![719, 719, 720]);
}

#[test]
fn a_fix_refused_for_a_red_required_check_names_it_and_asks_for_the_review() {
    let temp = tempfile::tempdir().expect("temp");
    let run = Run {
        dir: temp.path(),
        posts: RefCell::new(Vec::new()),
    };
    run.pass(
        PoisonHold::On,
        &poisoned(&[719], "2026-11-23T16:51:00Z"),
        &["release-path-review"],
        "2026-11-23T17:00:00Z",
    );
    let text = run.posts.borrow()[0].1.clone();
    assert!(
        text.contains("required check `release-path-review` is red"),
        "{text}"
    );
    assert!(text.contains("run or post that review"), "{text}");
}

#[test]
fn a_green_fix_is_not_asked_for_any_review() {
    let text = body(&Facts {
        base: "main",
        contexts: vec!["provenance-verify".to_owned()],
        tip: &BTreeMap::new(),
        held: 1,
        fix: 719,
        red_required: Vec::new(),
    });
    assert!(!text.contains("review for this head"), "{text}");
    assert!(text.contains("admitted ahead of the held"), "{text}");
}

#[test]
fn no_fix_pick_posts_nothing_and_says_so() {
    let temp = tempfile::tempdir().expect("temp");
    let run = Run {
        dir: temp.path(),
        posts: RefCell::new(Vec::new()),
    };
    let lines = run.pass(
        PoisonHold::On,
        &poisoned(&[], "2026-11-23T16:51:00Z"),
        &[],
        "2026-11-23T17:00:00Z",
    );
    assert_eq!(lines, vec!["fix_notice: no fix pick; nothing posted"]);
    assert!(run.posts.borrow().is_empty());
}

#[test]
fn off_and_dry_run_post_nothing_and_a_failed_post_is_retried_later() {
    let temp = tempfile::tempdir().expect("temp");
    let run = Run {
        dir: temp.path(),
        posts: RefCell::new(Vec::new()),
    };
    let poison = poisoned(&[719], "2026-11-23T16:51:00Z");
    assert!(
        run.pass(PoisonHold::Off, &poison, &[], "2026-11-23T17:00:00Z")
            .is_empty()
    );
    let dry = run.pass(PoisonHold::DryRun, &poison, &[], "2026-11-23T17:00:00Z");
    assert!(
        dry[0].starts_with("fix_notice #719: would comment"),
        "{dry:?}"
    );
    assert!(run.posts.borrow().is_empty());
    // A failed post is recorded, left alone for the retry window, then retried.
    let failing = |now: &str| {
        notify(
            PoisonHold::On,
            &|_, _| Err("HTTP 502".to_owned()),
            temp.path(),
            REPO,
            "main",
            &poison,
            3,
            &|_| Some(Vec::new()),
            at(now),
        )
    };
    assert_eq!(
        failing("2026-11-23T17:00:00Z"),
        vec!["fix_notice #719: failed: HTTP 502"]
    );
    assert_eq!(
        failing("2026-11-23T17:10:00Z"),
        vec!["fix_notice #719: already posted this episode"]
    );
    assert_eq!(
        failing("2026-11-23T17:31:00Z"),
        vec!["fix_notice #719: failed: HTTP 502"]
    );
}

#[test]
fn a_streak_the_tip_does_not_confirm_posts_nothing() {
    let temp = tempfile::tempdir().expect("temp");
    let run = Run {
        dir: temp.path(),
        posts: RefCell::new(Vec::new()),
    };
    let mut poison = poisoned(&[719], "2026-11-23T16:51:00Z");
    poison.tip.clear();
    assert!(
        run.pass(PoisonHold::On, &poison, &[], "2026-11-23T17:00:00Z")
            .is_empty()
    );
    assert!(run.posts.borrow().is_empty());
}
