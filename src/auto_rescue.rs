//! Classify a "Shipyard validated green but GitHub refused the merge" wedge.
//!
//! When `shipyard ship` validates every target green but the follow-up
//! `gh pr merge` is rejected, the cause is usually a GitHub branch-protection
//! required check that is RED on the very SHA Shipyard just validated — a
//! *flaky* required leg (e.g. a timing-sensitive test that flaked under runner
//! load), not a real regression. Recovering it is a one-liner
//! (`shipyard rescue <PR> --rerun-failed`), but the operator has to first
//! recognise the wedge for what it is. This module makes that call.
//!
//! ## Safety contract
//! This is a *diagnostic* classifier: it never mutates the merge path. It only
//! decides whether `shipyard ship`'s hand-back message should point the
//! operator at the one-liner recovery. Even so it is deliberately conservative,
//! because the same signal is intended to gate a future automated rescue:
//!
//! - **Fail closed.** Anything ambiguous (requiredness unavailable, an
//!   unmapped red required check, no red *or* pending required checks) returns
//!   [`WedgeClass::NotRecoverable`] and the caller renders the generic
//!   hand-back — never the flake one-liner.
//! - **Exact / configured mapping only.** A red required GitHub check is only
//!   treated as a flaky Shipyard leg when its context name maps *exactly* (or
//!   through an explicit `required_check_context` config entry) to a target
//!   Shipyard itself validated green. Fuzzy name matching is intentionally not
//!   used here: a wrong mapping would recommend "just rerun it" for a
//!   genuinely-failing required check.
//! - **GraphQL requiredness only.** The rollup must carry `isRequired` (as
//!   `gh pr view --json statusCheckRollup` does). Checks without it are treated
//!   as advisory, so ruleset / merge-queue governance (which does not populate
//!   `isRequired` on the rollup) fails closed.

use std::collections::BTreeSet;

use serde_json::Value;

use crate::config::LoadedConfig;
use crate::ship_state::ShipState;
use crate::wait::{PASSING_CONCLUSIONS, STILL_WAITING_STATES};

/// Inputs to [`classify_wedge`]. Kept pure (no IO) so the decision is trivially
/// unit-testable: callers fetch the rollup and build the validated-green set.
pub struct WedgeInputs<'a> {
    /// GitHub `statusCheckRollup` entries, each an object carrying at least
    /// `name`/`context`, `state`, `conclusion`, and `isRequired`.
    pub rollup: &'a [Value],
    /// The set of GitHub check contexts Shipyard validated green for this SHA —
    /// each validated-green required target's name plus any configured
    /// `required_check_context`, all lowercased. Built by
    /// [`validated_green_contexts`].
    pub validated_green_contexts: &'a BTreeSet<String>,
}

/// Classification of a validated-green-but-merge-rejected wedge.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WedgeClass {
    /// At least one required check is RED, and *every* red required check maps
    /// to a context Shipyard validated green — a flaky required leg the
    /// operator can safely recover with `shipyard rescue --rerun-failed`. The
    /// wrapped names are the red required contexts, for the recovery message.
    FlakyRequired {
        /// Names of the red required checks (all validated green by Shipyard).
        red_contexts: Vec<String>,
    },
    /// No red required checks — one or more required checks are still
    /// pending/in-flight. GitHub is waiting, not flaking; the merge will land
    /// on its own once they finish. Not a rescue.
    RequiredStillPending,
    /// Fail-closed: a red required check does not map to a validated-green
    /// Shipyard target, or requiredness could not be determined, or the merge
    /// was rejected for a reason not visible as a red/pending required check.
    /// The caller renders the generic hand-back, never the flake one-liner.
    NotRecoverable {
        /// Why the wedge is not a recognised flaky-required-leg case.
        reason: String,
    },
}

struct ObservedEntry {
    name: String,
    /// `Some(true)`/`Some(false)` when GitHub reported `isRequired` explicitly;
    /// `None` when the field is absent or non-boolean (ambiguous requiredness).
    required: Option<bool>,
    red: bool,
    pending: bool,
}

/// Parse one rollup entry into its name, requiredness, and terminal
/// disposition. Returns `None` only for a malformed (non-object) entry.
fn observe_entry(entry: &Value) -> Option<ObservedEntry> {
    let object = entry.as_object()?;
    let required = object.get("isRequired").and_then(Value::as_bool);
    let name = object
        .get("name")
        .or_else(|| object.get("context"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let state = object
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_uppercase();
    let conclusion = object
        .get("conclusion")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_uppercase();
    let passing = PASSING_CONCLUSIONS.contains(&conclusion.as_str());
    let pending = STILL_WAITING_STATES.contains(&state.as_str()) && !passing;
    // Terminal and not passing → red. (Pending is terminal-pending, not red.)
    let red = !passing && !pending;
    Some(ObservedEntry {
        name,
        required,
        red,
        pending,
    })
}

/// Decide whether a validated-green-but-merge-rejected wedge is a recoverable
/// flaky required leg. Pure; see the module-level safety contract.
#[must_use]
pub fn classify_wedge(inputs: &WedgeInputs) -> WedgeClass {
    let mut red_contexts: Vec<String> = Vec::new();
    let mut any_pending = false;
    for entry in inputs.rollup {
        let Some(obs) = observe_entry(entry) else {
            continue;
        };
        match obs.required {
            Some(true) => {
                if obs.red {
                    red_contexts.push(obs.name);
                } else if obs.pending {
                    any_pending = true;
                }
            }
            // Explicitly advisory: never blocks the merge, never a rescue target.
            Some(false) => {}
            // Ambiguous requiredness (absent `isRequired` — ruleset / merge-queue
            // governance, an older `gh`, or REST-synthesized data). A green such
            // check is harmless, but a RED *or still-PENDING* one might be a
            // genuinely-failing or genuinely-blocking required check we would
            // otherwise ignore, making the flaky verdict overconfident. Fail
            // closed on either.
            None => {
                if obs.red || obs.pending {
                    return WedgeClass::NotRecoverable {
                        reason: format!(
                            "check '{}' has no explicit isRequired; branch-protection \
                             requiredness is ambiguous, so refusing to classify the wedge as flaky",
                            obs.name
                        ),
                    };
                }
            }
        }
    }

    if red_contexts.is_empty() {
        return if any_pending {
            WedgeClass::RequiredStillPending
        } else {
            WedgeClass::NotRecoverable {
                reason: "no red or pending required checks in the rollup; the merge \
                         was rejected for a reason this classifier does not recognise \
                         (or branch-protection requiredness was unavailable)"
                    .to_owned(),
            }
        };
    }

    let unmapped: Vec<&str> = red_contexts
        .iter()
        .filter(|name| {
            !inputs
                .validated_green_contexts
                .contains(&name.to_ascii_lowercase())
        })
        .map(String::as_str)
        .collect();
    if unmapped.is_empty() {
        WedgeClass::FlakyRequired { red_contexts }
    } else {
        WedgeClass::NotRecoverable {
            reason: format!(
                "red required check(s) [{}] do not map to any Shipyard-validated-green \
                 target; refusing to treat a possibly-real failure as flaky",
                unmapped.join(", ")
            ),
        }
    }
}

/// Whether two commit SHAs identify the same commit: trimmed, non-empty,
/// case-insensitive *full* equality (never a prefix test — a short SHA must
/// never satisfy a full one). Used to prove a freshly-fetched rollup describes
/// the exact SHA Shipyard validated before trusting it. Mirrors the merge
/// preflight's own head comparison.
#[must_use]
pub fn sha_matches(a: &str, b: &str) -> bool {
    let a = a.trim();
    let b = b.trim();
    !a.is_empty() && !b.is_empty() && a.eq_ignore_ascii_case(b)
}

/// Read `[targets.<target>].required_check_context` from the merged config, if
/// declared. This is the explicit, non-fuzzy bridge from a Shipyard target name
/// (e.g. `mac`) to the GitHub branch-protection check context it produces
/// (e.g. `macos`).
#[must_use]
pub fn configured_required_check_context(config: &LoadedConfig, target: &str) -> Option<String> {
    config
        .data
        .get("targets")?
        .as_table()?
        .get(target)?
        .as_table()?
        .get("required_check_context")?
        .as_str()
        .map(str::to_owned)
}

/// Build the set of GitHub check contexts attributable to a Shipyard target it
/// validated green: for each *required* target whose evidence is `pass`, the
/// target name itself plus any configured `required_check_context`, lowercased.
///
/// Called on the `ship_terminal_verdict == Some(true)` branch, where every
/// required target passed — but this intersects with `evidence_snapshot` so a
/// non-`pass` entry never contributes a context.
#[must_use]
pub fn validated_green_contexts(state: &ShipState, config: &LoadedConfig) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for run in &state.dispatched_runs {
        if !run.required {
            continue;
        }
        if state.evidence_snapshot.get(&run.target).map(String::as_str) != Some("pass") {
            continue;
        }
        out.insert(run.target.to_ascii_lowercase());
        if let Some(context) = configured_required_check_context(config, &run.target) {
            out.insert(context.to_ascii_lowercase());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use serde_json::json;

    use crate::config::{LoadedConfig, LocalOverlaySource};
    use crate::ship_state::{DispatchedRun, ShipState};

    fn contexts(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|n| n.to_ascii_lowercase()).collect()
    }

    fn entry(name: &str, state: &str, conclusion: &str, required: bool) -> Value {
        json!({
            "name": name,
            "state": state,
            "conclusion": conclusion,
            "isRequired": required,
        })
    }

    #[test]
    fn red_required_that_maps_is_a_flaky_leg() {
        let rollup = vec![
            entry("macos", "COMPLETED", "FAILURE", true),
            entry("Enforce version & skill sync", "COMPLETED", "SUCCESS", true),
            entry("Windows (x64) [github-hosted]", "IN_PROGRESS", "", false),
        ];
        let green = contexts(&["macos", "enforce version & skill sync"]);
        let out = classify_wedge(&WedgeInputs {
            rollup: &rollup,
            validated_green_contexts: &green,
        });
        assert_eq!(
            out,
            WedgeClass::FlakyRequired {
                red_contexts: vec!["macos".to_owned()],
            }
        );
    }

    #[test]
    fn red_required_not_validated_green_fails_closed() {
        // The version/skill gate is a GHA-only required check Shipyard does not
        // run as a target: it must never be auto-treated as flaky.
        let rollup = vec![
            entry("macos", "COMPLETED", "SUCCESS", true),
            entry("Enforce version & skill sync", "COMPLETED", "FAILURE", true),
        ];
        let green = contexts(&["macos"]);
        let out = classify_wedge(&WedgeInputs {
            rollup: &rollup,
            validated_green_contexts: &green,
        });
        assert!(matches!(out, WedgeClass::NotRecoverable { .. }));
    }

    #[test]
    fn only_pending_required_is_still_waiting() {
        let rollup = vec![
            entry("macos", "COMPLETED", "SUCCESS", true),
            entry("linux", "IN_PROGRESS", "", true),
        ];
        let green = contexts(&["macos", "linux"]);
        let out = classify_wedge(&WedgeInputs {
            rollup: &rollup,
            validated_green_contexts: &green,
        });
        assert_eq!(out, WedgeClass::RequiredStillPending);
    }

    #[test]
    fn no_red_or_pending_required_fails_closed() {
        // All required checks green (merge rejected for some other reason) — not
        // a recognised flaky-required wedge.
        let rollup = vec![entry("macos", "COMPLETED", "SUCCESS", true)];
        let green = contexts(&["macos"]);
        let out = classify_wedge(&WedgeInputs {
            rollup: &rollup,
            validated_green_contexts: &green,
        });
        assert!(matches!(out, WedgeClass::NotRecoverable { .. }));
    }

    #[test]
    fn red_check_missing_is_required_fails_closed() {
        // Ruleset / merge-queue governance (or an older gh) does not populate
        // isRequired: a RED check with ambiguous requiredness must fail closed,
        // never be silently dropped as advisory.
        let rollup = vec![json!({
            "name": "macos",
            "state": "COMPLETED",
            "conclusion": "FAILURE",
        })];
        let green = contexts(&["macos"]);
        let out = classify_wedge(&WedgeInputs {
            rollup: &rollup,
            validated_green_contexts: &green,
        });
        assert!(matches!(out, WedgeClass::NotRecoverable { .. }));
    }

    #[test]
    fn mapped_red_plus_ambiguous_red_fails_closed() {
        // A mapped red required check would look flaky in isolation, but a
        // second RED check with absent isRequired might itself be a genuinely
        // failing required check. The presence of ANY ambiguous red entry must
        // sink the whole wedge to NotRecoverable — never rescue past it.
        let rollup = vec![
            entry("macos", "COMPLETED", "FAILURE", true),
            json!({"name": "mystery-gate", "state": "COMPLETED", "conclusion": "FAILURE"}),
        ];
        let green = contexts(&["macos"]);
        let out = classify_wedge(&WedgeInputs {
            rollup: &rollup,
            validated_green_contexts: &green,
        });
        assert!(matches!(out, WedgeClass::NotRecoverable { .. }));
    }

    #[test]
    fn mapped_red_plus_ambiguous_pending_fails_closed() {
        // A mapped red required check plus a PENDING check with absent
        // isRequired: the pending one might itself be a required blocker still
        // running, so the flaky verdict would be overconfident — fail closed.
        let rollup = vec![
            entry("macos", "COMPLETED", "FAILURE", true),
            json!({"name": "mystery-gate", "state": "IN_PROGRESS", "conclusion": ""}),
        ];
        let green = contexts(&["macos"]);
        let out = classify_wedge(&WedgeInputs {
            rollup: &rollup,
            validated_green_contexts: &green,
        });
        assert!(matches!(out, WedgeClass::NotRecoverable { .. }));
    }

    #[test]
    fn green_advisory_missing_is_required_is_harmless() {
        // A GREEN check with absent isRequired is not a failure and must not
        // block classifying an otherwise-clean flaky-required wedge.
        let rollup = vec![
            entry("macos", "COMPLETED", "FAILURE", true),
            json!({"name": "some-advisory", "state": "COMPLETED", "conclusion": "SUCCESS"}),
        ];
        let green = contexts(&["macos"]);
        let out = classify_wedge(&WedgeInputs {
            rollup: &rollup,
            validated_green_contexts: &green,
        });
        assert_eq!(
            out,
            WedgeClass::FlakyRequired {
                red_contexts: vec!["macos".to_owned()],
            }
        );
    }

    #[test]
    fn mixed_red_required_one_unmapped_fails_closed() {
        // Two red required checks; one maps, one doesn't → the whole wedge is
        // not recoverable (we never partially rescue).
        let rollup = vec![
            entry("macos", "COMPLETED", "FAILURE", true),
            entry("secret-scan", "COMPLETED", "FAILURE", true),
        ];
        let green = contexts(&["macos"]);
        let out = classify_wedge(&WedgeInputs {
            rollup: &rollup,
            validated_green_contexts: &green,
        });
        assert!(matches!(out, WedgeClass::NotRecoverable { .. }));
    }

    fn config_with_context(target: &str, context: &str) -> LoadedConfig {
        let toml = format!("[targets.{target}]\nrequired_check_context = \"{context}\"\n");
        let data = toml.parse::<toml::Table>().expect("valid toml");
        LoadedConfig {
            data,
            global_dir: std::path::PathBuf::from("/tmp"),
            project_dir: None,
            local_dir: None,
            local_overlay_source: LocalOverlaySource::None,
        }
    }

    fn passing_state(target: &str) -> ShipState {
        let now = Utc::now();
        let mut state = ShipState {
            schema_version: 1,
            pr: 1,
            repo: "owner/repo".to_owned(),
            branch: "feat/x".to_owned(),
            base_branch: "main".to_owned(),
            head_sha: "abc123".to_owned(),
            policy_signature: String::new(),
            pr_url: String::new(),
            pr_title: String::new(),
            commit_subject: String::new(),
            dispatched_runs: vec![DispatchedRun {
                target: target.to_owned(),
                provider: "local".to_owned(),
                run_id: "1".to_owned(),
                status: "completed".to_owned(),
                started_at: now,
                updated_at: now,
                attempt: 1,
                last_heartbeat_at: None,
                phase: None,
                required: true,
            }],
            evidence_snapshot: BTreeSet::new()
                .into_iter()
                .collect::<std::collections::BTreeMap<String, String>>(),
            attempt: 1,
            created_at: now,
            updated_at: now,
            merge_queue_observed_at: None,
            merge_queue_attempt_started_at: None,
            merge_queue_enqueue_succeeded_at: None,
            merge_queue_enqueue_started_at: None,
            abandoned: None,
        };
        state
            .evidence_snapshot
            .insert(target.to_owned(), "pass".to_owned());
        state
    }

    #[test]
    fn configured_context_bridges_target_name_to_check_name() {
        // Shipyard target `mac` produces GitHub required check `macos`.
        let config = config_with_context("mac", "macos");
        let state = passing_state("mac");
        let green = validated_green_contexts(&state, &config);
        assert!(green.contains("mac"));
        assert!(green.contains("macos"));

        // End to end: the red `macos` check now maps to the green `mac` target.
        let rollup = vec![entry("macos", "COMPLETED", "FAILURE", true)];
        let out = classify_wedge(&WedgeInputs {
            rollup: &rollup,
            validated_green_contexts: &green,
        });
        assert_eq!(
            out,
            WedgeClass::FlakyRequired {
                red_contexts: vec!["macos".to_owned()],
            }
        );
    }

    #[test]
    fn sha_matches_is_full_case_insensitive_equality() {
        let sha = "deadbeefcafef00d1234567890abcdef12345678";
        assert!(sha_matches(sha, &sha.to_uppercase()));
        assert!(sha_matches(&format!("  {sha}\n"), sha));
        assert!(!sha_matches(sha, "deadbee")); // prefix never satisfies full
        assert!(!sha_matches(sha, "")); // empty never matches
        assert!(!sha_matches(
            sha,
            "0000beefcafef00d1234567890abcdef12345678"
        ));
    }

    #[test]
    fn failed_target_contributes_no_context() {
        // A target present in evidence but not `pass` must not seed a context.
        let config = config_with_context("mac", "macos");
        let mut state = passing_state("mac");
        state
            .evidence_snapshot
            .insert("mac".to_owned(), "fail".to_owned());
        let green = validated_green_contexts(&state, &config);
        assert!(green.is_empty());
    }
}
