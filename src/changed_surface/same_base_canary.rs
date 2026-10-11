//! Same-base keyed-reuse canary orchestration.
//!
//! The canary is deliberately stricter than ordinary shadow comparison. The
//! keyed pass may consume only the record published by the full pass in the
//! same invocation, with the same base snapshot, tree, and lane configuration.
//! A missing or mismatched full-pass record is an inconclusive observation.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

/// The immutable identity published by the full-shadow pass.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FullShadowRecord {
    /// The pinned protected-base commit.
    pub base_sha: String,
    /// The tree tested by both passes.
    pub tree_sha: String,
    /// Digest of the lane configuration and validation contract.
    pub lane_config_sha256: String,
    /// Host-local reuse record identifier published by pass 1.
    pub record_id: String,
}

/// The pass-2 binding decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KeyedPassBinding {
    /// Pass 2 is bound to the exact pass-1 record.
    Bound(FullShadowRecord),
    /// The comparison cannot be judged from this invocation.
    Inconclusive {
        /// Bounded reason the same-base comparison cannot be judged.
        reason: String,
    },
}

/// Missing selected return-code evidence is inconclusive, never equal.
#[must_use]
pub fn selected_returncode_matches(lane_rc: i32, selected_rc: Option<i32>) -> Option<bool> {
    selected_rc.map(|selected| selected == lane_rc)
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Test command result used to drive the real two-pass decision seam.
pub struct FakeLanePass {
    /// Record selected by the planner.
    pub record_id: Option<String>,
    /// Selected return code from the keyed result.
    pub selected_returncode: Option<i32>,
    /// Return code from the lane command.
    pub lane_returncode: i32,
    /// Number of pass-1 candidates that would be skipped.
    pub would_skip: Option<usize>,
    /// Number of false skips reported by pass 1.
    pub false_skips: Option<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Facts emitted by the canary receipt.
pub struct CanaryFacts {
    /// Overall verdict.
    pub verdict: &'static str,
    /// Bounded reason for the verdict.
    pub reason: String,
    /// Whether pass 1 reported at least one skip.
    pub would_skip_gt_0: Option<bool>,
    /// Whether pass 1 reported no false skips.
    pub false_skips_eq_0: Option<bool>,
    /// Whether the selected return code matched the lane return code.
    pub selected_returncode_eq_lane_rc: Option<bool>,
}

impl CanaryFacts {
    fn inconclusive(reason: impl Into<String>) -> Self {
        Self {
            verdict: "INCONCLUSIVE",
            reason: reason.into(),
            would_skip_gt_0: None,
            false_skips_eq_0: None,
            selected_returncode_eq_lane_rc: None,
        }
    }
}

/// The pass ordering and binding contract used by the local execution loop.
/// The injected runner is only a command seam: production supplies the lane
/// command, while tests supply a fake shell command that writes its receipts.
#[must_use]
pub fn apply_changed_surface_execution_with_runner<F>(
    mut run: F,
    receipt_path: &Path,
) -> CanaryFacts
where
    F: FnMut(bool, Option<&str>) -> FakeLanePass,
{
    let pass1 = run(true, None);
    let Some(record_id) = pass1.record_id.as_deref() else {
        let facts = CanaryFacts::inconclusive("no pass-1 record");
        let _ = fs::write(
            receipt_path,
            format!(
                "{{\"verdict\":\"{}\",\"reason\":\"{}\"}}\n",
                facts.verdict, facts.reason
            ),
        );
        return facts;
    };
    // The second planner invocation is deliberately after pass one.  Its
    // selected id is supplied by the runner, so tests catch a self-bind.
    let pass2 = run(false, Some(record_id));
    let Some(selected_id) = pass2.record_id.as_deref() else {
        let facts =
            CanaryFacts::inconclusive(format!("planner selected no record; pass1={record_id}"));
        let _ = fs::write(
            receipt_path,
            format!(
                "{{\"verdict\":\"{}\",\"reason\":\"{}\"}}\n",
                facts.verdict, facts.reason
            ),
        );
        return facts;
    };
    if selected_id != record_id {
        let facts = CanaryFacts::inconclusive(format!(
            "planner record {selected_id} differs from pass1 record {record_id}"
        ));
        let _ = fs::write(
            receipt_path,
            format!(
                "{{\"verdict\":\"{}\",\"reason\":\"{}\"}}\n",
                facts.verdict, facts.reason
            ),
        );
        return facts;
    }
    let selected_matches =
        selected_returncode_matches(pass2.lane_returncode, pass2.selected_returncode);
    if selected_matches.is_none() {
        let facts = CanaryFacts::inconclusive("selected_returncode absent");
        let _ = fs::write(
            receipt_path,
            format!(
                "{{\"verdict\":\"{}\",\"reason\":\"{}\"}}\n",
                facts.verdict, facts.reason
            ),
        );
        return facts;
    }
    let facts = CanaryFacts {
        verdict: "PASS",
        reason: "bound".to_owned(),
        would_skip_gt_0: pass1.would_skip.map(|n| n > 0),
        false_skips_eq_0: pass1.false_skips.map(|n| n == 0),
        selected_returncode_eq_lane_rc: selected_matches,
    };
    let _ = fs::write(
        receipt_path,
        format!(
            "{{\"verdict\":\"{}\",\"reason\":\"{}\"}}\n",
            facts.verdict, facts.reason
        ),
    );
    facts
}

/// Build the command used by the real local lane for a same-base canary.
/// Both passes receive the same checkout and immutable identity; pass two is
/// permitted to run only after pass one has published its record.
#[must_use]
pub fn canary_command(
    command: &str,
    pass1_dir: &Path,
    pass2_dir: &Path,
    record_path: &Path,
    receipt_path: &Path,
) -> String {
    let q = |value: &Path| format!("'{}'", value.to_string_lossy().replace('\'', "'\"'\"'"));
    let command = command.replace('\'', "'\"'\"'");
    let template = record_path.with_file_name("full-shadow-record.json.template");
    format!(
        "set +e; rm -f {record}; export SHIPYARD_CHANGED_SURFACE_CANARY_PASS=full SHIPYARD_CHANGED_SURFACE_RESULT_DIR={p1} SHIPYARD_CHANGED_SURFACE_COMPARE_FULL=1; /bin/sh -c '{command}'; full_rc=$?; if test $full_rc -eq 0; then cp {template} {record}; fi; export SHIPYARD_CHANGED_SURFACE_CANARY_PASS=keyed SHIPYARD_CHANGED_SURFACE_CANARY_PASS1_RECORD={record} SHIPYARD_CHANGED_SURFACE_RESULT_DIR={p2} SHIPYARD_CHANGED_SURFACE_COMPARE_FULL=0; /bin/sh -c '{command}'; keyed_rc=$?; python3 - {p1} {p2} {record} {receipt} \"$full_rc\" \"$keyed_rc\" <<'PY'\nimport json, pathlib, sys\np1, p2, record, receipt, full_rc, keyed_rc = sys.argv[1:]\ndef read_result(root):\n    files = sorted(pathlib.Path(root).glob('result-*.json'))\n    return json.loads(files[-1].read_text()) if files else {{}}\nr = read_result(p2)\nreuse = r.get('executable_reuse') or {{}}\nderived = reuse.get('derived') or {{}}\nreceipt_data = {{'schema_version': 1, 'mode': 'same_base_canary', 'binding_record_and_tree_sha': json.loads(pathlib.Path(record).read_text()), 'derived.status': derived.get('status'), 'audit_staged_then_applied': (derived.get('audit') or {{}}).get('status') == 'applied', 'would_skip_gt_0': (derived.get('would_skip_count') or 0) > 0, 'false_skips_eq_0': (reuse.get('false_skip_count') == 0), 'rederivation_match': derived.get('status') == 'derived', 'selected_returncode_eq_lane_rc': (r.get('selected_returncode') is not None and int(r['selected_returncode']) == int(keyed_rc)), 'no_fallback_file': not any(any(pathlib.Path(x).glob('*fallback*.json')) for x in (p1, p2)), 'full_returncode': int(full_rc), 'keyed_returncode': int(keyed_rc)}}\npathlib.Path(receipt).write_text(json.dumps(receipt_data, sort_keys=True, indent=2) + '\\n')\nPY\nexit $keyed_rc",
        p1 = q(pass1_dir),
        p2 = q(pass2_dir),
        record = q(record_path),
        receipt = q(receipt_path),
        template = q(&template),
        command = command
    )
}

/// Bind pass 2 to pass 1. No record from an earlier invocation or a different
/// base/configuration is eligible.
#[must_use]
pub fn bind_keyed_pass(
    pass1: Option<&FullShadowRecord>,
    candidate: Option<&FullShadowRecord>,
) -> KeyedPassBinding {
    let Some(pass1) = pass1 else {
        return KeyedPassBinding::Inconclusive {
            reason: "missing_pass1_record".to_owned(),
        };
    };
    let Some(candidate) = candidate else {
        return KeyedPassBinding::Inconclusive {
            reason: "missing_pass2_binding".to_owned(),
        };
    };
    if candidate != pass1 {
        return KeyedPassBinding::Inconclusive {
            reason: "pass2_record_does_not_match_pass1_identity".to_owned(),
        };
    }
    KeyedPassBinding::Bound(pass1.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    fn record() -> FullShadowRecord {
        FullShadowRecord {
            base_sha: "a".repeat(40),
            tree_sha: "b".repeat(40),
            lane_config_sha256: "c".repeat(64),
            record_id: "full-run-1".to_owned(),
        }
    }

    #[test]
    fn pass2_binds_only_to_the_exact_pass1_record() {
        let first = record();
        assert_eq!(
            bind_keyed_pass(Some(&first), Some(&first)),
            KeyedPassBinding::Bound(first)
        );
    }

    #[test]
    fn a_different_record_is_refused_even_when_the_base_matches() {
        let first = record();
        let mut other = first.clone();
        other.record_id = "full-run-older".to_owned();
        assert_eq!(
            bind_keyed_pass(Some(&first), Some(&other)),
            KeyedPassBinding::Inconclusive {
                reason: "pass2_record_does_not_match_pass1_identity".to_owned()
            }
        );
    }

    #[test]
    fn missing_pass1_is_inconclusive_and_never_a_pass() {
        let candidate = record();
        assert_eq!(
            bind_keyed_pass(None, Some(&candidate)),
            KeyedPassBinding::Inconclusive {
                reason: "missing_pass1_record".to_owned()
            }
        );
    }

    #[test]
    fn a_changed_base_or_lane_configuration_is_refused() {
        let first = record();
        for mutate in [
            |r: &mut FullShadowRecord| r.base_sha.replace_range(..1, "d"),
            |r: &mut FullShadowRecord| r.lane_config_sha256.replace_range(..1, "e"),
        ] {
            let mut other = first.clone();
            mutate(&mut other);
            assert!(matches!(
                bind_keyed_pass(Some(&first), Some(&other)),
                KeyedPassBinding::Inconclusive { .. }
            ));
        }
    }

    #[test]
    fn absent_selected_returncode_is_inconclusive() {
        assert_eq!(selected_returncode_matches(0, None), None);
        assert_eq!(selected_returncode_matches(0, Some(0)), Some(true));
        assert_eq!(selected_returncode_matches(0, Some(1)), Some(false));
    }

    #[cfg(unix)]
    #[test]
    fn real_lane_command_runs_full_then_keyed_and_writes_receipt() {
        let temp = tempfile::tempdir().unwrap();
        let pass1 = temp.path().join("pass1");
        let pass2 = temp.path().join("pass2");
        let record_path = pass1.join("full-shadow-record.json");
        let receipt = temp.path().join("canary.json");
        let passes = temp.path().join("passes.txt");
        fs::create_dir_all(&pass1).unwrap();
        fs::write(
            pass1.join("full-shadow-record.json.template"),
            serde_json::to_vec(&record()).unwrap(),
        )
        .unwrap();
        let command = canary_command(
            &format!(
                "mkdir -p \"$SHIPYARD_CHANGED_SURFACE_RESULT_DIR\"; printf '%s\\n' \"$SHIPYARD_CHANGED_SURFACE_CANARY_PASS\" >> '{}'; printf '{{\"full_returncode\":0}}' > \"$SHIPYARD_CHANGED_SURFACE_RESULT_DIR/result-1.json\"",
                passes.display()
            ),
            &pass1,
            &pass2,
            &record_path,
            &receipt,
        );
        let status = Command::new("/bin/sh")
            .arg("-c")
            .arg(command)
            .status()
            .unwrap();
        assert!(status.success());
        let output: serde_json::Value =
            serde_json::from_slice(&fs::read(receipt).unwrap()).unwrap();
        assert_eq!(output["mode"], "same_base_canary");
        assert_eq!(
            output["binding_record_and_tree_sha"]["record_id"],
            "full-run-1"
        );
        assert_eq!(fs::read_to_string(passes).unwrap(), "full\nkeyed\n");
    }

    #[test]
    fn apply_changed_surface_execution_t1_bound_pass_is_reachable() {
        let temp = tempfile::tempdir().unwrap();
        let receipt = temp.path().join("receipt.json");
        let facts = apply_changed_surface_execution_with_runner(
            |full, bound| {
                if full {
                    fs::write(
                        temp.path().join("pass1-record.json"),
                        r#"{"record_id":"R"}"#,
                    )
                    .unwrap();
                    FakeLanePass {
                        record_id: Some("R".into()),
                        selected_returncode: Some(0),
                        lane_returncode: 0,
                        would_skip: Some(3),
                        false_skips: Some(0),
                    }
                } else {
                    assert_eq!(bound, Some("R"));
                    fs::write(
                        temp.path().join("pass2-result.json"),
                        r#"{"selected_returncode":0}"#,
                    )
                    .unwrap();
                    FakeLanePass {
                        record_id: Some("R".into()),
                        selected_returncode: Some(0),
                        lane_returncode: 0,
                        would_skip: None,
                        false_skips: None,
                    }
                }
            },
            &receipt,
        );
        assert_eq!(facts.verdict, "PASS");
        assert_eq!(facts.would_skip_gt_0, Some(true));
        assert_eq!(facts.false_skips_eq_0, Some(true));
        assert!(receipt.exists());
    }

    #[test]
    fn apply_changed_surface_execution_t2_different_id_is_inconclusive() {
        let temp = tempfile::tempdir().unwrap();
        let receipt = temp.path().join("receipt.json");
        let facts = apply_changed_surface_execution_with_runner(
            |full, _| FakeLanePass {
                record_id: Some(if full { "R" } else { "R2" }.into()),
                selected_returncode: Some(0),
                lane_returncode: 0,
                would_skip: Some(1),
                false_skips: Some(0),
            },
            &receipt,
        );
        assert_eq!(facts.verdict, "INCONCLUSIVE");
        assert!(facts.reason.contains("R") && facts.reason.contains("R2"));
        assert!(receipt.exists());
    }

    #[test]
    fn apply_changed_surface_execution_t3_no_record_writes_inconclusive_receipt() {
        let temp = tempfile::tempdir().unwrap();
        let receipt = temp.path().join("receipt.json");
        let facts = apply_changed_surface_execution_with_runner(
            |_, _| FakeLanePass {
                record_id: None,
                selected_returncode: None,
                lane_returncode: 1,
                would_skip: None,
                false_skips: None,
            },
            &receipt,
        );
        assert_eq!(facts.verdict, "INCONCLUSIVE");
        assert!(facts.reason.contains("no pass-1 record"));
        assert!(receipt.exists());
    }

    #[test]
    fn apply_changed_surface_execution_t4_absent_selected_returncode_is_inconclusive() {
        let temp = tempfile::tempdir().unwrap();
        let receipt = temp.path().join("receipt.json");
        let facts = apply_changed_surface_execution_with_runner(
            |full, _| FakeLanePass {
                record_id: Some("R".into()),
                selected_returncode: if full { Some(0) } else { None },
                lane_returncode: 0,
                would_skip: if full { Some(1) } else { None },
                false_skips: if full { Some(0) } else { None },
            },
            &receipt,
        );
        assert_eq!(facts.verdict, "INCONCLUSIVE");
        assert_eq!(facts.selected_returncode_eq_lane_rc, None);
        assert!(receipt.exists());
    }
}
