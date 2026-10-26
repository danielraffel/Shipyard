use std::collections::BTreeMap;

use super::{
    Merge, OffBy, PrSample, Sample, Stage, TrainSwitch, enabled_in_config, kill_switch, metrics,
    stage_from_decision,
};
use crate::merge_steward::StewardDecision;

const REPO: &str = "owner/repo";

#[test]
fn the_train_is_off_unless_the_config_says_true() {
    assert_eq!(enabled_in_config(""), Ok(false));
    assert_eq!(enabled_in_config("[queue]\nx = 1\n"), Ok(false));
    assert_eq!(enabled_in_config("[train]\n"), Ok(false));
    assert_eq!(enabled_in_config("[train]\nenabled = false\n"), Ok(false));
    assert_eq!(enabled_in_config("[train]\nenabled = true\n"), Ok(true));
    assert!(enabled_in_config("[train]\nenabled = \"yes\"\n").is_err());
    assert!(enabled_in_config("[train\n").is_err());
}

/// The kill switch wins over everything and is read before GitHub: the env
/// for every repository or exactly one, then the pause file.
#[test]
fn the_kill_switch_names_every_repository_or_one() {
    assert_eq!(kill_switch(None, None, REPO), None);
    for value in ["off", "OFF", " off ", "0", "false", "FALSE"] {
        assert!(matches!(
            kill_switch(Some(value), None, REPO),
            Some(OffBy::KillEnv { .. })
        ));
    }
    assert!(matches!(
        kill_switch(Some("off:Owner/Repo"), None, REPO),
        Some(OffBy::KillEnv { .. })
    ));
    assert_eq!(kill_switch(Some("off:other/repo"), None, REPO), None);
    assert_eq!(kill_switch(Some("on"), None, REPO), None);
    assert_eq!(
        kill_switch(None, Some("deploy freeze".to_owned()), REPO),
        Some(OffBy::KillPause {
            reason: "deploy freeze".to_owned()
        })
    );
    // The env names the cause when both kill.
    assert!(matches!(
        kill_switch(Some("off"), Some("x".to_owned()), REPO),
        Some(OffBy::KillEnv { .. })
    ));
}

#[test]
fn a_pause_file_that_does_not_parse_still_pauses() {
    let dir = tempfile::tempdir().expect("temp");
    let path = dir.path().join(super::PAUSE_FILE);
    assert_eq!(super::read_pause(&path), None);
    std::fs::write(&path, "not json").expect("write");
    assert!(
        super::read_pause(&path).is_some_and(|reason| reason.contains("still pauses")),
        "{:?}",
        super::read_pause(&path)
    );
    std::fs::write(
        &path,
        r#"{"reason":"deploy freeze","at":"2026-10-26T00:00:00Z","by":"agent"}"#,
    )
    .expect("write");
    assert_eq!(super::read_pause(&path), Some("deploy freeze".to_owned()));
}

#[test]
fn the_switch_explains_what_decided_it() {
    assert!(TrainSwitch::On.is_on());
    let off = TrainSwitch::Off {
        by: OffBy::ConfigOff,
    };
    assert!(!off.is_on());
    assert!(
        off.explain()
            .contains("does not set `[train] enabled = true`")
    );
}

#[test]
fn the_stewards_decision_sets_the_stage_until_it_would_arm() {
    assert_eq!(
        stage_from_decision(&StewardDecision::Queued { position: 0 }),
        Some(Stage::Queued)
    );
    assert_eq!(
        stage_from_decision(&StewardDecision::Draft),
        Some(Stage::Draft)
    );
    assert_eq!(
        stage_from_decision(&StewardDecision::NeedsUpdate {
            merge_state: "DIRTY".to_owned()
        }),
        Some(Stage::Conflict)
    );
    assert_eq!(
        stage_from_decision(&StewardDecision::WaitingRequired {
            contexts: vec!["macos".to_owned()]
        }),
        Some(Stage::ChecksRunning)
    );
    assert_eq!(
        stage_from_decision(&StewardDecision::RequiredFailed {
            contexts: vec!["macos".to_owned()]
        }),
        Some(Stage::ChecksFailed)
    );
    assert_eq!(
        stage_from_decision(&StewardDecision::OptedOut),
        Some(Stage::Held)
    );
    assert_eq!(stage_from_decision(&StewardDecision::ArmMergeQueue), None);
}

fn pr(stage: Stage) -> PrSample {
    PrSample {
        stage,
        head: "a".repeat(40),
    }
}

fn sample(at: &str, depth: u64, prs: &[(u64, Stage)], merged: &[(u64, &str)]) -> Sample {
    Sample {
        at: at.to_owned(),
        queue_depth: depth,
        prs: prs.iter().map(|(n, stage)| (*n, pr(*stage))).collect(),
        merged: merged
            .iter()
            .map(|(number, at)| Merge {
                number: *number,
                merged_at: (*at).to_owned(),
            })
            .collect(),
    }
}

/// Three ten-minute samples: PR 1 is ready while the queue is empty for the
/// first interval, then queued, then merged together with PR 2.
#[test]
fn metrics_measure_throughput_latency_and_idle_queue_time() {
    let samples = [
        sample(
            "2026-10-26T10:00:00Z",
            0,
            &[(1, Stage::Ready), (2, Stage::NeedsReview)],
            &[],
        ),
        sample(
            "2026-10-26T10:10:00Z",
            2,
            &[(1, Stage::Queued), (2, Stage::Queued)],
            &[],
        ),
        sample(
            "2026-10-26T11:00:00Z",
            0,
            &[],
            &[(1, "2026-10-26T10:40:00Z"), (2, "2026-10-26T10:40:30Z")],
        ),
    ];
    let measured = metrics(&samples);
    assert_eq!(measured.samples, 3);
    assert_eq!(measured.merges, 2);
    assert!((measured.merges_per_hour - 2.0).abs() < 1e-9);
    // PR 1: approved 10:00, merged 10:40. PR 2: approved 10:10, merged 10:40:30.
    assert_eq!(measured.approved_to_merged_p50_minutes, Some(30));
    assert_eq!(measured.approved_to_merged_p95_minutes, Some(40));
    assert_eq!(measured.latency_population, 2);
    // Only the first interval: empty queue, a ready PR.
    assert_eq!(measured.queue_empty_while_ready_minutes, 10);
    assert_eq!(measured.batch_sizes, BTreeMap::from([(2, 1)]));
}

/// Control: an empty queue with nothing ready is not waste, and a gap longer
/// than the sampling bound never counts as idle time.
#[test]
fn an_empty_queue_with_nothing_ready_or_a_sampling_gap_is_not_waste() {
    let quiet = [
        sample("2026-10-26T10:00:00Z", 0, &[(1, Stage::NeedsReview)], &[]),
        sample("2026-10-26T10:10:00Z", 0, &[(1, Stage::ChecksRunning)], &[]),
    ];
    assert_eq!(metrics(&quiet).queue_empty_while_ready_minutes, 0);
    let gap = [
        sample("2026-10-26T10:00:00Z", 0, &[(1, Stage::Ready)], &[]),
        sample("2026-10-26T13:00:00Z", 0, &[(1, Stage::Ready)], &[]),
    ];
    assert_eq!(metrics(&gap).queue_empty_while_ready_minutes, 0);
    assert!(metrics(&[]).merges_per_hour.abs() < 1e-9);
}

/// A merge whose approval was never sampled has no latency, rather than a
/// latency of zero.
#[test]
fn a_merge_never_seen_approved_has_no_latency() {
    let samples = [
        sample("2026-10-26T10:00:00Z", 0, &[(1, Stage::NeedsReview)], &[]),
        sample(
            "2026-10-26T10:10:00Z",
            0,
            &[],
            &[(1, "2026-10-26T10:05:00Z")],
        ),
    ];
    let measured = metrics(&samples);
    assert_eq!(measured.merges, 1);
    assert_eq!(measured.latency_population, 0);
    assert_eq!(measured.approved_to_merged_p50_minutes, None);
    assert_eq!(measured.batch_sizes, BTreeMap::from([(1, 1)]));
}

/// Each distinct head sampled for a merged pull request is churn it cost.
#[test]
fn heads_per_merged_pr_counts_distinct_sampled_heads() {
    let mut first = sample("2026-10-26T10:00:00Z", 0, &[(1, Stage::ChecksRunning)], &[]);
    first.prs.get_mut(&1).expect("pr").head = "b".repeat(40);
    let samples = [
        first,
        sample("2026-10-26T10:10:00Z", 0, &[(1, Stage::Ready)], &[]),
        sample(
            "2026-10-26T10:20:00Z",
            0,
            &[],
            &[(1, "2026-10-26T10:15:00Z")],
        ),
    ];
    assert_eq!(metrics(&samples).heads_per_merged_pr, Some(2.0));
    assert_eq!(metrics(&samples[..2]).heads_per_merged_pr, None);
}
