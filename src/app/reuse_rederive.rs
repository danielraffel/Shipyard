//! Host re-derivation of a keyed shadow run: re-run the protected base's key
//! code over the run's copied inputs and compare with what the runner
//! reported. The result is one `rederivation-<result sha256>.json` receipt in
//! the exact trial directory, which the trial reader requires before it
//! records a keyed run. A refusal is counted per host, once per (head, result),
//! and the second one turns live reuse off.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use base64::Engine as _;
use chrono::Utc;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use wait_timeout::ChildExt as _;

use crate::changed_surface::executable_reuse::{
    BaseCandidate, ExecutableReuseBinding, KEYED_CONTEXT_RECEIPT, KeyedRunContext, Rederivation,
    compare_rederivation, derivation_copy, materialize, record_digest,
};
use crate::changed_surface::trial::{REDERIVATION_RECEIPT_PREFIX, TrialIdentity, result_directory};
use crate::changed_surface::{KEYED_BOUNDED_SHADOW, KEYED_FULL_SHADOW, policy_from_base};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(600);
const MAX_COMMAND_OUTPUT: u64 = 1024 * 1024;
const ACTIVATION_RECEIPT: &str = "activation-shadow_compare.json";
/// The key code's marker for a build-dir string that matched no registration.
const INVENTORY_UNMATCHED: &str = "inventory_unmatched";
/// Refusals on one host that turn live reuse off.
const TRIP_AFTER_REFUSALS: usize = 2;
/// The runner's copied inputs, each with the `derived` field holding its
/// sha256 in the result receipt.
const RUNNER_INPUTS: [(&str, &str); 5] = [
    ("ctest-listing.json", "ctest_listing_sha256"),
    ("toolchain.json", "toolchain_sha256"),
    ("codemodel-digest.json", "codemodel_digest_sha256"),
    ("executable-keys.json", "key_manifest_sha256"),
    ("selection.json", "selection_sha256"),
];

/// Who asked for a re-derivation, recorded in its receipt.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProducedBy {
    /// A ship completion path, just before merge readiness.
    Completion,
    /// The bounded startup sweep.
    Sweep,
    /// An operator running `shipyard reuse rederive`.
    Operator,
}

/// What one call did.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub(crate) enum Outcome {
    /// The trial is not a keyed run; nothing to check.
    NotKeyed,
    /// The keyed run has not written its result yet.
    NoResult,
    /// This result already has its receipt; nothing was counted again.
    AlreadyRecorded { receipt: String },
    /// A receipt was written.
    Recorded {
        receipt: String,
        verdict: String,
        reason: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        tripped: Option<String>,
    },
}

#[derive(Debug, Deserialize)]
struct Activation {
    plan: ActivationPlan,
}

#[derive(Debug, Deserialize)]
struct ActivationPlan {
    target: String,
    base_sha: String,
    head_sha: String,
    policy_digest: String,
    execution_payload_digest: String,
    #[serde(default)]
    disposition: Option<String>,
}

#[derive(Debug, Serialize)]
struct RederivationRecord<'a> {
    schema_version: u32,
    result_receipt: &'a str,
    result_receipt_sha256: &'a str,
    head_sha: &'a str,
    verdict: &'a str,
    reason: &'a str,
    diagnostics: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    keyed_record_run_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    keyed_record_sha256: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    toolchain_matched: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    host_output_dir: Option<&'a str>,
    produced_by: ProducedBy,
    #[serde(skip_serializing_if = "Option::is_none")]
    refusal_count: Option<usize>,
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    trip_reasons: &'a [String],
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    key_blind_candidates: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    tripped: Option<&'a str>,
}

/// A verdict before it is written.
struct Verdict {
    kind: &'static str,
    reason: String,
    diagnostics: Vec<String>,
    pick: Option<BaseCandidate>,
    host_output_dir: Option<PathBuf>,
    /// The base policy's switch variable, once the policy was read.
    switch_variable: Option<String>,
    /// Whether the record keyed against was the lane's toolchain match
    /// (`false`: the first candidate, keyed for its refusal reason).
    toolchain_matched: Option<bool>,
}

impl Verdict {
    fn refuse(reason: impl Into<String>) -> Self {
        Self {
            kind: "refuse",
            reason: reason.into(),
            diagnostics: Vec::new(),
            pick: None,
            host_output_dir: None,
            switch_variable: None,
            toolchain_matched: None,
        }
    }

    fn not_derived(reason: impl Into<String>) -> Self {
        Self {
            kind: "not_derived",
            reason: reason.into(),
            diagnostics: Vec::new(),
            pick: None,
            host_output_dir: None,
            switch_variable: None,
            toolchain_matched: None,
        }
    }

    fn after(mut self, pick: BaseCandidate, out: PathBuf) -> Self {
        self.pick = Some(pick);
        self.host_output_dir = Some(out);
        self
    }
}

/// One evidence directory's re-derivation outcome within a head.
#[derive(Debug, Eq, PartialEq, Serialize)]
pub(crate) struct PayloadOutcome {
    /// The keyed payload directory's digest; `None` for the top-level
    /// evidence of the head.
    pub(crate) payload_sha256: Option<String>,
    /// What happened, when it could be decided.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) outcome: Option<Outcome>,
    /// Why nothing was recorded for it; a later call can retry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
}

/// Re-derive every keyed run of one head: its top-level evidence and each
/// keyed payload directory, at most once per result. Each directory is judged
/// on its own, so one that cannot be read never hides another's outcome, and
/// refusals stay counted per (head, result).
///
/// `gh` runs a `gh` command for the run's checkout (its first argument); it
/// is used only to trip the switch after the second refusal.
pub(crate) fn rederive_trial<F>(
    state_dir: &Path,
    identity: &TrialIdentity,
    produced_by: ProducedBy,
    gh: &F,
) -> Vec<PayloadOutcome>
where
    F: Fn(&Path, &[String]) -> Result<String, String> + ?Sized,
{
    let trial_dir = result_directory(state_dir, identity);
    std::iter::once((None, trial_dir.clone()))
        .chain(
            crate::changed_surface::trial::payload_dirs(&trial_dir)
                .into_iter()
                .map(|(digest, dir)| (Some(digest), dir)),
        )
        .map(|(payload_sha256, dir)| {
            let (outcome, error) = match rederive_dir(state_dir, &dir, produced_by, gh) {
                Ok(outcome) => (Some(outcome), None),
                Err(error) => (None, Some(error)),
            };
            PayloadOutcome {
                payload_sha256,
                outcome,
                error,
            }
        })
        .collect()
}

/// Re-derive the keyed run whose evidence is `trial_dir` (a head's top-level
/// evidence or one keyed payload directory), at most once per result.
///
/// # Errors
///
/// The directory or a receipt could not be read or written, or the checkout
/// the run used is unreadable. Nothing is recorded and a later call (the
/// sweep, an operator) can retry.
pub(crate) fn rederive_dir<F>(
    state_dir: &Path,
    trial_dir: &Path,
    produced_by: ProducedBy,
    gh: &F,
) -> Result<Outcome, String>
where
    F: Fn(&Path, &[String]) -> Result<String, String> + ?Sized,
{
    let trial_dir = trial_dir.to_path_buf();
    let Some(activation_bytes) = read_optional(&trial_dir.join(ACTIVATION_RECEIPT))? else {
        return Ok(Outcome::NotKeyed);
    };
    let activation: Activation = serde_json::from_slice(&activation_bytes)
        .map_err(|error| format!("decode the shadow activation: {error}"))?;
    let plan = activation.plan;
    if !matches!(
        plan.disposition.as_deref(),
        Some(KEYED_FULL_SHADOW | KEYED_BOUNDED_SHADOW)
    ) {
        return Ok(Outcome::NotKeyed);
    }
    let results = named_files(&trial_dir, "result-")?;
    let [(result_name, result_bytes)] = results.as_slice() else {
        return if results.is_empty() {
            Ok(Outcome::NoResult)
        } else {
            Err("more than one result for one keyed run".to_owned())
        };
    };
    let result_sha = sha256_hex(result_bytes);
    let receipt_name = format!("{REDERIVATION_RECEIPT_PREFIX}{result_sha}.json");
    if trial_dir.join(&receipt_name).exists() {
        return Ok(Outcome::AlreadyRecorded {
            receipt: receipt_name,
        });
    }
    let context_bytes = read_optional(&trial_dir.join(KEYED_CONTEXT_RECEIPT))?
        .ok_or_else(|| "the keyed run left no context receipt".to_owned())?;
    let context: KeyedRunContext = serde_json::from_slice(&context_bytes)
        .map_err(|error| format!("decode the keyed run context: {error}"))?;
    let work = state_dir
        .join("executable-reuse")
        .join("rederive")
        .join(&result_sha);
    let verdict = judge(state_dir, &trial_dir, &plan, &context, result_bytes, &work)?;
    let TripDecision {
        signals,
        candidates,
        refusal_count,
        tripped,
    } = decide_trip(
        state_dir,
        &JudgedRun {
            plan: &plan,
            context: &context,
            result_bytes,
            result_sha: &result_sha,
            work: &work,
        },
        &verdict,
        gh,
    )?;
    let diagnostics = receipt_diagnostics(&verdict, result_bytes);
    let host_output_dir = verdict
        .host_output_dir
        .as_ref()
        .map(|dir| dir.to_string_lossy().into_owned());
    let record = RederivationRecord {
        schema_version: 1,
        result_receipt: result_name,
        result_receipt_sha256: &result_sha,
        head_sha: &plan.head_sha,
        verdict: verdict.kind,
        reason: &verdict.reason,
        diagnostics: &diagnostics,
        keyed_record_run_id: verdict.pick.as_ref().map(|pick| pick.run_id.as_str()),
        keyed_record_sha256: verdict
            .pick
            .as_ref()
            .map(|pick| pick.record_sha256.as_str()),
        toolchain_matched: verdict.toolchain_matched,
        host_output_dir: host_output_dir.as_deref(),
        produced_by,
        refusal_count,
        trip_reasons: &signals,
        key_blind_candidates: &candidates,
        tripped: tripped.as_deref(),
    };
    write_new(&trial_dir.join(&receipt_name), &record)?;
    Ok(Outcome::Recorded {
        receipt: receipt_name,
        verdict: verdict.kind.to_owned(),
        reason: verdict.reason,
        tripped,
    })
}

/// The verdict's diagnostics plus the closure modules the run could not
/// compare, which are reported and never tripped on.
fn receipt_diagnostics(verdict: &Verdict, result_bytes: &[u8]) -> Vec<String> {
    let mut diagnostics = verdict.diagnostics.clone();
    if let Some(unchecked) = serde_json::from_slice::<Value>(result_bytes)
        .ok()
        .as_ref()
        .and_then(|result| result.pointer("/executable_reuse/derived/unreached_unchecked_modules"))
        .and_then(Value::as_array)
        .filter(|modules| !modules.is_empty())
    {
        let names: Vec<String> = unchecked
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        diagnostics.push(format!(
            "unreached_unchecked_modules ({}): closure modules with no recorded hash, so their bytes were not compared: {}",
            names.len(),
            listed(&names)
        ));
    }
    diagnostics
}

/// One keyed run's verified identity and evidence.
struct JudgedRun<'a> {
    plan: &'a ActivationPlan,
    context: &'a KeyedRunContext,
    result_bytes: &'a [u8],
    result_sha: &'a str,
    work: &'a Path,
}

/// What a re-derivation did to the switch.
struct TripDecision {
    /// What the verified result reported that live reuse would have got wrong.
    signals: Vec<String>,
    /// Executables whose key was equal while live reuse would have got them
    /// wrong: candidates for the key-blind list.
    candidates: Vec<String>,
    /// Refusals counted on this host, when this one was a refusal.
    refusal_count: Option<usize>,
    /// What tripping the switch did, when it was tripped.
    tripped: Option<String>,
}

/// Read the trip signals from evidence the host did not refuse, count a
/// refusal, and trip the switch on the second refusal or on any signal.
fn decide_trip<F>(
    state_dir: &Path,
    run: &JudgedRun<'_>,
    verdict: &Verdict,
    gh: &F,
) -> Result<TripDecision, String>
where
    F: Fn(&Path, &[String]) -> Result<String, String> + ?Sized,
{
    let JudgedRun {
        plan,
        context,
        result_bytes,
        result_sha,
        work,
    } = *run;
    let checkout = Path::new(&context.checkout);
    let gh_here = |args: &[String]| gh(checkout, args);
    // Only evidence the host did not refuse is read for trip reasons.
    let result = (verdict.kind != "refuse")
        .then(|| serde_json::from_slice::<Value>(result_bytes).ok())
        .flatten();
    let signals = result.as_ref().map(trip_reasons).unwrap_or_default();
    let manifest = read_optional(&work.join("inputs").join("executable-keys.json"))
        .ok()
        .flatten()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    let candidates = result
        .as_ref()
        .map(|result| key_blind_candidates(result, manifest.as_ref()))
        .unwrap_or_default();
    let (refusal_count, tripped) = if verdict.kind == "refuse" {
        let count = record_refusal(
            state_dir,
            &context.repository,
            &plan.head_sha,
            result_sha,
            &verdict.reason,
        )?;
        let tripped = (count >= TRIP_AFTER_REFUSALS).then(|| {
            trip(
                &gh_here,
                &context.repository,
                verdict.switch_variable.as_deref(),
                &format!(
                    "host re-derivation refused {count} keyed runs on this host; latest at {}: {}",
                    plan.head_sha, verdict.reason
                ),
            )
        });
        (Some(count), tripped)
    } else if signals.is_empty() {
        (None, None)
    } else {
        let tripped = trip(
            &gh_here,
            &context.repository,
            verdict.switch_variable.as_deref(),
            &format!(
                "keyed run at {} reported what live reuse would have got wrong: {}",
                plan.head_sha,
                signals.join("; ")
            ),
        );
        (None, Some(tripped))
    };
    Ok(TripDecision {
        signals,
        candidates,
        refusal_count,
        tripped,
    })
}

/// Decide the verdict. Anything that shows the evidence disagrees with what
/// was bound is a refusal; an `Err` is only for a host that cannot look.
fn judge(
    state_dir: &Path,
    trial_dir: &Path,
    plan: &ActivationPlan,
    context: &KeyedRunContext,
    result_bytes: &[u8],
    work: &Path,
) -> Result<Verdict, String> {
    let Ok(payload) =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(&context.execution_payload_b64)
    else {
        return Ok(Verdict::refuse("the context's payload is not base64"));
    };
    if sha256_hex(&payload) != plan.execution_payload_digest {
        return Ok(Verdict::refuse(
            "the context's payload is not the one the activation bound",
        ));
    }
    let payload: Value = serde_json::from_slice(&payload).unwrap_or(Value::Null);
    // The runner refused a binding file that did not match the payload's
    // digest and ran unkeyed: nothing was keyed, so there is nothing to judge.
    if let Some(status) = serde_json::from_slice::<Value>(result_bytes)
        .ok()
        .and_then(|result| {
            result
                .pointer("/executable_reuse_binding/status")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .filter(|status| status != "verified")
    {
        return Ok(Verdict::not_derived(format!(
            "the runner refused the binding: {status}"
        )));
    }
    let binding = match payload
        .get("executable_reuse_binding_digest")
        .and_then(Value::as_str)
    {
        Some(digest) => read_binding_file(trial_dir, digest),
        None => payload
            .get("executable_reuse")
            .cloned()
            .ok_or_else(|| "the bound payload carries no reuse binding".to_owned())
            .and_then(|value| {
                serde_json::from_value::<ExecutableReuseBinding>(value)
                    .map_err(|_| "the bound payload carries no reuse binding".to_owned())
            }),
    };
    let binding = match binding {
        Ok(binding) => binding,
        Err(reason) => return Ok(Verdict::refuse(reason)),
    };
    let checkout = Path::new(&context.checkout);
    let config = git_bytes(
        checkout,
        &["show", &format!("{}:.shipyard/config.toml", plan.base_sha)],
    )?;
    let config =
        String::from_utf8(config).map_err(|_| "the base configuration is not UTF-8".to_owned())?;
    let policy = policy_from_base(&config, &plan.target, |path| {
        git_bytes(checkout, &["show", &format!("{}:{path}", plan.base_sha)]).and_then(|bytes| {
            String::from_utf8(bytes)
                .map(|text| text.trim().to_owned())
                .map_err(|_| format!("{path} is not UTF-8"))
        })
    })?;
    if crate::changed_surface::policy_digest(&policy) != plan.policy_digest {
        return Ok(Verdict::refuse(
            "the base policy no longer has the digest the plan bound",
        ));
    }
    let Some(reuse) = policy.executable_reuse.as_ref() else {
        return Ok(Verdict::refuse(
            "the base policy declares no executable reuse",
        ));
    };
    let run = KeyedRun {
        plan,
        context,
        binding: &binding,
        reuse,
        checkout,
    };
    judge_run(state_dir, trial_dir, &run, result_bytes, work).map(|mut verdict| {
        verdict.switch_variable = Some(reuse.switch_variable.clone());
        verdict
    })
}

/// A schema-3 plan's binding: the file beside the activation, accepted only
/// when its bytes hash to the digest the payload names. The runner verified
/// the same file before it ran, so a difference now means the evidence moved.
fn read_binding_file(trial_dir: &Path, digest: &str) -> Result<ExecutableReuseBinding, String> {
    let name = crate::changed_surface::EXECUTABLE_REUSE_BINDING_FILE;
    let bytes = fs::read(trial_dir.join(name))
        .map_err(|_| format!("the binding file {name} is missing"))?;
    if sha256_hex(&bytes) != digest {
        return Err(format!(
            "the binding file {name} is not the one the payload named"
        ));
    }
    serde_json::from_slice(&bytes).map_err(|_| format!("the binding file {name} is not a binding"))
}

/// What a keyed run bound, read back and verified.
struct KeyedRun<'a> {
    plan: &'a ActivationPlan,
    context: &'a KeyedRunContext,
    binding: &'a ExecutableReuseBinding,
    reuse: &'a crate::changed_surface::executable_reuse::ExecutableReusePolicy,
    checkout: &'a Path,
}

/// Re-run the base key code over the run's copied inputs with its pick.
fn judge_run(
    state_dir: &Path,
    trial_dir: &Path,
    run: &KeyedRun<'_>,
    result_bytes: &[u8],
    work: &Path,
) -> Result<Verdict, String> {
    let KeyedRun {
        plan,
        context,
        binding,
        reuse,
        checkout,
    } = *run;
    let result: Value = serde_json::from_slice(result_bytes)
        .map_err(|error| format!("decode the result: {error}"))?;
    let derived = result
        .pointer("/executable_reuse/derived")
        .filter(|value| value.is_object());
    let Some(derived) = derived else {
        return Ok(Verdict::not_derived("the run derived no keys"));
    };
    let (pick, toolchain_matched) = match resolve_pick(derived, binding) {
        Ok(pick) => pick,
        Err(verdict) => return Ok(*verdict),
    };
    match record_digest(Path::new(&pick.record_path)) {
        Ok(digest) if digest == pick.record_sha256 => {}
        Ok(_) => {
            return Ok(Verdict::refuse(format!(
                "the picked record {} changed after it was bound",
                pick.run_id
            )));
        }
        Err(error) => {
            return Err(format!(
                "cannot read the picked record {}: {error}",
                pick.run_id
            ));
        }
    }
    let code = derivation_copy(reuse, |path| {
        git_bytes(checkout, &["show", &format!("{}:{path}", plan.base_sha)])
    })?;
    if code.digest != binding.derivation_code_sha256 {
        return Ok(Verdict::refuse(
            "the base's key code is not the code the plan bound",
        ));
    }
    let code_dir = materialize(
        &code,
        &state_dir.join("executable-reuse").join("derivation"),
    )
    .map_err(|error| format!("materialize the derivation code: {error}"))?;
    let _ = fs::remove_dir_all(work);
    let inputs = work.join("inputs");
    let out = work.join("out");
    fs::create_dir_all(&inputs).map_err(|error| format!("create {}: {error}", inputs.display()))?;
    fs::create_dir_all(&out).map_err(|error| format!("create {}: {error}", out.display()))?;
    if let Some(refusal) = copy_runner_inputs(trial_dir, derived, &inputs)? {
        return Ok(Verdict::refuse(refusal));
    }
    let substitutions = [
        ("{source_root}", context.checkout.clone()),
        ("{base_sha}", pick.commit.clone()),
        ("{head_sha}", plan.head_sha.clone()),
        ("{base_record_dir}", pick.record_path.clone()),
        ("{base_record_run_id}", pick.run_id.clone()),
        ("{result_dir}", inputs.to_string_lossy().into_owned()),
        ("{build_dir}", binding.build_dir.clone()),
        ("{out_dir}", out.to_string_lossy().into_owned()),
        ("{sample_seed}", binding.sample_seed.clone()),
        ("{sample_percent}", binding.sample_percent.to_string()),
    ];
    for command in &reuse.rederive {
        let command = command
            .iter()
            .map(|arg| {
                substitutions
                    .iter()
                    .fold(arg.clone(), |arg, (key, value)| arg.replace(key, value))
            })
            .collect::<Vec<_>>();
        if let Err(error) = run_base_command(&command, &code_dir, "the base key code") {
            return Ok(
                Verdict::refuse(format!("the host re-derivation failed: {error}")).after(pick, out),
            );
        }
    }
    let mut verdict = compare_outputs(&inputs, &out)?;
    verdict.toolchain_matched = Some(toolchain_matched);
    Ok(verdict.after(pick, out))
}

/// The record the runner keyed against and whether it was the lane's
/// toolchain match. The runner keys against its pick; with no toolchain match
/// it keys against the first candidate (every executable then reads as
/// another toolchain); with no candidate it keys against nothing at all.
fn resolve_pick(
    derived: &Value,
    binding: &ExecutableReuseBinding,
) -> Result<(BaseCandidate, bool), Box<Verdict>> {
    let text = |key: &str| derived.get(key).and_then(Value::as_str);
    match (text("base_record_run_id"), text("base_record_sha256")) {
        (None, None) => binding
            .candidates
            .first()
            .map(|first| (first.clone(), false))
            .ok_or_else(|| {
                Box::new(Verdict::not_derived(
                    "no candidate was bound, so every executable is unrecorded",
                ))
            }),
        (Some(run_id), Some(record_sha)) => binding
            .candidates
            .iter()
            .find(|candidate| candidate.run_id == run_id && candidate.record_sha256 == record_sha)
            .map(|pick| (pick.clone(), true))
            .ok_or_else(|| {
                Box::new(Verdict::refuse(format!(
                    "the run keyed against {run_id}, which is not in the bound candidate set"
                )))
            }),
        _ => Err(Box::new(Verdict::refuse(
            "the result names a pick's run id or digest but not both",
        ))),
    }
}

/// Copy the runner's inputs read-only for the host run, each checked against
/// the hash the result states; `Some` names why the evidence is refused.
fn copy_runner_inputs(
    trial_dir: &Path,
    derived: &Value,
    inputs: &Path,
) -> Result<Option<String>, String> {
    for (name, key) in RUNNER_INPUTS {
        let Some(want) = derived.get(key).and_then(Value::as_str) else {
            return Ok(Some(format!("the result states no {key}")));
        };
        let Some(bytes) = read_optional(&trial_dir.join(name))? else {
            return Ok(Some(format!("the run left no {name}")));
        };
        if sha256_hex(&bytes) != want {
            return Ok(Some(format!("{name} is not the file the run hashed")));
        }
        write_read_only(&inputs.join(name), &bytes)?;
    }
    Ok(None)
}

/// Compare the host's outputs with the runner's copied ones.
fn compare_outputs(inputs: &Path, out: &Path) -> Result<Verdict, String> {
    let read = |dir: &Path, name: &str| read_optional(&dir.join(name));
    let (Some(host_manifest), Some(host_selection)) = (
        read(out, "executable-keys.json")?,
        read(out, "selection.json")?,
    ) else {
        return Ok(Verdict::refuse(
            "the host re-derivation wrote no manifest or selection",
        ));
    };
    let runner_manifest = read(inputs, "executable-keys.json")?.unwrap_or_default();
    let runner_selection = read(inputs, "selection.json")?.unwrap_or_default();
    let mut verdict = match compare_rederivation(
        &runner_manifest,
        &host_manifest,
        &runner_selection,
        &host_selection,
    ) {
        Rederivation::Match => Verdict {
            kind: "match",
            reason: "the host re-derived the same manifest and selection".to_owned(),
            ..Verdict::refuse("")
        },
        Rederivation::MatchWithDiagnostics(fields) => Verdict {
            kind: "match_with_diagnostics",
            reason: "the selections match; the manifests differ only in run-specific fields"
                .to_owned(),
            diagnostics: fields,
            ..Verdict::refuse("")
        },
        Rederivation::Refuse(reason) => Verdict::refuse(reason),
    };
    // A build-dir spelling that matched no test registration keys every
    // executable as always-run on both sides alike: a configuration error to
    // report, never a refusal to count.
    if String::from_utf8_lossy(&host_manifest).contains(INVENTORY_UNMATCHED) {
        verdict.diagnostics.push(format!(
            "{INVENTORY_UNMATCHED}: the build_dir string matched no test registration; \
             check the policy's build_dir"
        ));
    }
    Ok(verdict)
}

/// Count a refusal for this host, once per (head, result), and return the
/// number of distinct refusals recorded for the repository.
fn record_refusal(
    state_dir: &Path,
    repository: &str,
    head_sha: &str,
    result_sha: &str,
    reason: &str,
) -> Result<usize, String> {
    let dir = state_dir.join("executable-reuse");
    fs::create_dir_all(&dir).map_err(|error| format!("create {}: {error}", dir.display()))?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join(".refusals.lock"))
        .map_err(|error| format!("open the refusal lock: {error}"))?;
    FileExt::lock_exclusive(&lock).map_err(|error| format!("lock the refusals: {error}"))?;
    let path = dir.join("refusals.json");
    let mut ledger: BTreeMap<String, BTreeMap<String, String>> = match read_optional(&path)? {
        Some(bytes) => serde_json::from_slice(&bytes)
            .map_err(|error| format!("decode {}: {error}", path.display()))?,
        None => BTreeMap::new(),
    };
    let entries = ledger.entry(repository.to_owned()).or_default();
    entries
        .entry(format!("{head_sha}:{result_sha}"))
        .or_insert_with(|| reason.to_owned());
    let count = entries.len();
    let temporary = dir.join(format!(".refusals.{}.tmp", std::process::id()));
    let bytes = serde_json::to_vec_pretty(&ledger)
        .map_err(|error| format!("encode the refusals: {error}"))?;
    fs::write(&temporary, bytes)
        .and_then(|()| fs::rename(&temporary, &path))
        .map_err(|error| format!("write {}: {error}", path.display()))?;
    Ok(count)
}

/// Turn live reuse off (the switch first, then the issue) and say what
/// happened; a failed trip is recorded, not raised, so the refusal still
/// lands.
fn trip<F>(gh: &F, repository: &str, variable: Option<&str>, why: &str) -> String
where
    F: Fn(&[String]) -> Result<String, String> + ?Sized,
{
    let Some(variable) = variable else {
        return "not tripped: the refusal came before the base policy was read".to_owned();
    };
    match crate::changed_surface::live_switch::trip(gh, repository, variable, why, Utc::now(), true)
    {
        Ok(outcome) => format!("{variable}: {}; {}", outcome.variable, outcome.issue),
        Err(error) => format!("trip of {variable} failed: {error}"),
    }
}

/// How many names a trip reason lists before it says how many more.
const SIGNAL_NAMES: usize = 5;

/// The first [`SIGNAL_NAMES`] names and how many more there are.
fn listed(names: &[String]) -> String {
    let shown = names.iter().take(SIGNAL_NAMES).cloned().collect::<Vec<_>>();
    match names.len().saturating_sub(SIGNAL_NAMES) {
        0 => shown.join(", "),
        more => format!("{} and {more} more", shown.join(", ")),
    }
}

/// The executables whose key was equal while live reuse would have got
/// them wrong: those registering a sampled failure or a false skip (read
/// from the verified manifest), and those rebuilt to different bytes. Each is
/// a candidate for the project's key-blind list.
fn key_blind_candidates(result: &Value, manifest: Option<&Value>) -> Vec<String> {
    let strings = |value: Option<&Value>| -> std::collections::BTreeSet<String> {
        value
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut failed = strings(result.pointer("/executable_reuse/sampled_failures"));
    failed.extend(strings(result.pointer("/executable_reuse/false_skips")));
    let mut candidates = strings(result.pointer("/executable_reuse/derived/unreached_changed"));
    if let Some(executables) = manifest
        .and_then(|manifest| manifest.get("executables"))
        .and_then(Value::as_object)
    {
        for (artifact, entry) in executables {
            if strings(entry.get("registrations"))
                .iter()
                .any(|test| failed.contains(test))
            {
                candidates.insert(artifact.clone());
            }
        }
    }
    candidates.into_iter().collect()
}

/// What a keyed result reports that live reuse would have got wrong, each
/// of which turns live reuse off at once: a sampled would-skip executable's
/// test failed, a would-skip test failed in the full run, or an executable
/// the key called unchanged was rebuilt to different bytes.
fn trip_reasons(result: &Value) -> Vec<String> {
    let names = |pointer: &str| -> Vec<String> {
        result
            .pointer(pointer)
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut signals = Vec::new();
    for (pointer, what) in [
        (
            "/executable_reuse/sampled_failures",
            "sampled would-skip tests failed",
        ),
        (
            "/executable_reuse/false_skips",
            "would-skip tests failed in the full run",
        ),
        (
            "/executable_reuse/derived/unreached_changed",
            "executables keyed unchanged were rebuilt to different bytes",
        ),
    ] {
        let found = names(pointer);
        if !found.is_empty() {
            signals.push(format!("{what} ({}): {}", found.len(), listed(&found)));
        }
    }
    signals
}

/// Re-derive, newest first and at most `cap`, the keyed runs on this host
/// whose result has no receipt yet: the hand that picks up a completion path
/// that crashed before it ran.
pub(crate) fn sweep<F>(
    state_dir: &Path,
    cap: usize,
    gh: &F,
) -> Vec<(TrialIdentity, Result<Outcome, String>)>
where
    F: Fn(&Path, &[String]) -> Result<String, String> + ?Sized,
{
    let mut pending = Vec::new();
    let root = state_dir.join("changed-surface-results");
    let evidence_dirs = trial_dirs(&root).into_iter().flat_map(|trial_dir| {
        let payloads = crate::changed_surface::trial::payload_dirs(&trial_dir);
        std::iter::once((trial_dir.clone(), trial_dir.clone())).chain(
            payloads
                .into_iter()
                .map(move |(_, dir)| (dir, trial_dir.clone())),
        )
    });
    for (evidence_dir, trial_dir) in evidence_dirs {
        let Ok(Some(bytes)) = read_optional(&evidence_dir.join(ACTIVATION_RECEIPT)) else {
            continue;
        };
        let Ok(activation) = serde_json::from_slice::<SweepActivation>(&bytes) else {
            continue;
        };
        let plan = activation.plan;
        if !matches!(
            plan.disposition.as_deref(),
            Some(KEYED_FULL_SHADOW | KEYED_BOUNDED_SHADOW)
        ) {
            continue;
        }
        let Ok(results) = named_files(&evidence_dir, "result-") else {
            continue;
        };
        let [(_, bytes)] = results.as_slice() else {
            continue;
        };
        let receipt = format!("{REDERIVATION_RECEIPT_PREFIX}{}.json", sha256_hex(bytes));
        if evidence_dir.join(receipt).exists() {
            continue;
        }
        // The adapter's own record time, not a filesystem timestamp: two
        // results filed within one mtime tick must still order newest first.
        let recorded = serde_json::from_slice::<Value>(bytes)
            .ok()
            .and_then(|result| result.get("recorded_at_unix_ns").and_then(Value::as_u64))
            .unwrap_or(0);
        let identity = TrialIdentity {
            repository: plan.repository,
            pull_request: plan.pull_request,
            target: plan.target,
            head_sha: plan.head_sha,
        };
        if result_directory(state_dir, &identity) == trial_dir {
            pending.push((recorded, evidence_dir, identity));
        }
    }
    pending.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    pending
        .into_iter()
        .take(cap)
        .map(|(_, evidence_dir, identity)| {
            let outcome = rederive_dir(state_dir, &evidence_dir, ProducedBy::Sweep, gh);
            (identity, outcome)
        })
        .collect()
}

#[derive(Debug, Deserialize)]
struct SweepActivation {
    plan: SweepPlan,
}

#[derive(Debug, Deserialize)]
struct SweepPlan {
    repository: String,
    pull_request: u64,
    target: String,
    head_sha: String,
    #[serde(default)]
    disposition: Option<String>,
}

/// Every `<repo>/<pr>/<head>/<target>` directory under the results root.
fn trial_dirs(root: &Path) -> Vec<PathBuf> {
    let mut level = vec![root.to_path_buf()];
    for _ in 0..4 {
        level = level
            .iter()
            .filter_map(|dir| fs::read_dir(dir).ok())
            .flat_map(Iterator::flatten)
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .map(|entry| entry.path())
            .collect();
    }
    level
}

/// The only environment a base-declared command inherits.
const COMMAND_ENV: [&str; 4] = ["PATH", "HOME", "TMPDIR", "LANG"];
/// How much of a failing command's stderr its error keeps.
const STDERR_TAIL: usize = 300;

/// Run one base-declared command from the materialized derivation code with a
/// bounded wait, bounded output and only [`COMMAND_ENV`] from this process's
/// environment, returning its stdout.
///
/// # Errors
///
/// Why it could not run, timed out, failed (with the end of its stderr), or
/// printed too much.
pub(crate) fn run_base_command(
    command: &[String],
    code_dir: &Path,
    what: &str,
) -> Result<Vec<u8>, String> {
    let (program, args) = command
        .split_first()
        .ok_or_else(|| format!("{what} names no command"))?;
    let mut child = Command::new(program);
    child
        .args(args)
        .current_dir(code_dir)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for name in COMMAND_ENV {
        if let Some(value) = std::env::var_os(name) {
            child.env(name, value);
        }
    }
    let mut child = child
        .spawn()
        .map_err(|error| format!("cannot start {what}: {error}"))?;
    let drain = |stream: Option<Box<dyn std::io::Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(stream) = stream {
                let _ = stream.take(MAX_COMMAND_OUTPUT + 1).read_to_end(&mut bytes);
            }
            bytes
        })
    };
    let stdout = drain(
        child
            .stdout
            .take()
            .map(|out| Box::new(out) as Box<dyn std::io::Read + Send>),
    );
    let stderr = drain(
        child
            .stderr
            .take()
            .map(|err| Box::new(err) as Box<dyn std::io::Read + Send>),
    );
    let status = match child.wait_timeout(COMMAND_TIMEOUT) {
        Ok(Some(status)) => status,
        Ok(None) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "{what} did not finish within {}s",
                COMMAND_TIMEOUT.as_secs()
            ));
        }
        Err(error) => {
            let _ = child.kill();
            return Err(format!("cannot wait for {what}: {error}"));
        }
    };
    let bytes = stdout
        .join()
        .map_err(|_| format!("{what}'s reader panicked"))?;
    let errors = stderr.join().unwrap_or_default();
    if !status.success() {
        let tail = &errors[errors.len().saturating_sub(STDERR_TAIL)..];
        return Err(format!(
            "{what} failed ({status}): {}",
            String::from_utf8_lossy(tail).trim()
        ));
    }
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_COMMAND_OUTPUT {
        return Err(format!("{what} printed too much"));
    }
    Ok(bytes)
}

/// A tracked file's exact bytes at a commit.
pub(crate) fn git_bytes(cwd: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|error| format!("git {}: {error}", args.join(" ")))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output.stdout)
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => fs::read(path)
            .map(Some)
            .map_err(|error| format!("read {}: {error}", path.display())),
        Ok(_) => Err(format!("{} is not a regular file", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("inspect {}: {error}", path.display())),
    }
}

/// Regular files in `dir` whose names start with `prefix` and end `.json`,
/// sorted by name.
fn named_files(dir: &Path, prefix: &str) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut found = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(found),
        Err(error) => return Err(format!("read {}: {error}", dir.display())),
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(prefix)
            && Path::new(&name)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
            && let Some(bytes) = read_optional(&entry.path())?
        {
            found.push((name, bytes));
        }
    }
    found.sort();
    Ok(found)
}

fn write_read_only(path: &Path, bytes: &[u8]) -> Result<(), String> {
    fs::write(path, bytes).map_err(|error| format!("write {}: {error}", path.display()))?;
    let mut permissions = fs::metadata(path)
        .map_err(|error| format!("inspect {}: {error}", path.display()))?
        .permissions();
    permissions.set_readonly(true);
    fs::set_permissions(path, permissions)
        .map_err(|error| format!("protect {}: {error}", path.display()))
}

fn write_new<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let mut bytes =
        serde_json::to_vec_pretty(value).map_err(|error| format!("encode receipt: {error}"))?;
    bytes.push(b'\n');
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| format!("create {}: {error}", path.display()))?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("write {}: {error}", path.display()))
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::changed_surface::executable_reuse::DerivationCode;
    use crate::reuse_record_store::{create_pending, file, store_dir};
    use serde_json::json;
    use std::cell::RefCell;

    const CONFIG: &str = r#"
[targets.mac]
validation_build_type = "debug"

[targets.mac.changed_surface_selection]
schema_version = 1
full_test_count = 100
build_type = "debug"
baseline_tests = ["smoke boots"]
test_topology_paths = ["tests/**"]

[targets.mac.changed_surface_selection.executable_reuse]
switch_variable = "PULP_REUSE_LIVE"
derivation_paths = ["rederive.py"]
build_dir = "build"
platform_probe = ["python3", "-I", "probe.py"]
rederive = [["python3", "-I", "rederive.py", "{result_dir}", "{out_dir}", "{base_record_run_id}"]]

[targets.mac.changed_surface_selection.executable_reuse.base_record]
platform = "/platform"
toolchain = "/toolchain/digest"

[[targets.mac.changed_surface_selection.families]]
name = "keys"
paths = ["tools/**"]
tests = ["keys selftest"]
supported_build_types = ["debug"]
"#;

    /// A faithful re-derivation: the host writes what the runner wrote.
    const FAITHFUL: &str = "import shutil, sys\n\
        for name in ('executable-keys.json', 'selection.json'):\n\
        \x20   shutil.copy(sys.argv[1] + '/' + name, sys.argv[2])\n";

    struct Fixture {
        _root: tempfile::TempDir,
        repo: PathBuf,
        state: PathBuf,
        base: String,
        policy_digest: String,
        code: DerivationCode,
        candidate: BaseCandidate,
    }

    fn git(repo: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .current_dir(repo)
            .output()
            .expect("git");
        assert!(output.status.success(), "{args:?}");
        String::from_utf8(output.stdout)
            .expect("utf8")
            .trim()
            .to_owned()
    }

    fn fixture(script: &str) -> Fixture {
        let root = tempfile::tempdir().expect("root");
        let repo = root.path().join("repo");
        let state = root.path().join("state");
        fs::create_dir_all(repo.join(".shipyard")).expect("repo");
        fs::write(repo.join(".shipyard/config.toml"), CONFIG).expect("config");
        fs::write(repo.join("rederive.py"), script).expect("script");
        git(&repo, &["init", "-q"]);
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "base"]);
        let base = git(&repo, &["rev-parse", "HEAD"]);
        let policy = policy_from_base(CONFIG, "mac", |_| Err("none".to_owned())).expect("policy");
        let reuse = policy.executable_reuse.clone().expect("reuse");
        let code = derivation_copy(&reuse, |path| {
            git_bytes(&repo, &["show", &format!("{base}:{path}")])
        })
        .expect("code");
        let store = store_dir(&state, "owner/repo");
        let pending = create_pending(&store, &base, Utc::now()).expect("pending");
        fs::write(
            pending.join("job.json"),
            json!({"platform": "p", "toolchain": {"digest": "t"}}).to_string(),
        )
        .expect("job");
        let crate::reuse_record_store::Filed::Kept(path) =
            file(&store, &pending, &base).expect("file")
        else {
            panic!("record kept");
        };
        let candidate = BaseCandidate {
            run_id: path
                .file_name()
                .expect("name")
                .to_string_lossy()
                .into_owned(),
            record_sha256: record_digest(&path).expect("digest"),
            record_path: path.to_string_lossy().into_owned(),
            commit: base.clone(),
        };
        Fixture {
            policy_digest: crate::changed_surface::policy_digest(&policy),
            _root: root,
            repo,
            state,
            base,
            code,
            candidate,
        }
    }

    /// Lay out one keyed run for `head` and return its identity; `edit`
    /// changes the result before it is written.
    fn keyed_run(fixture: &Fixture, head: &str, edit: impl Fn(&mut Value)) -> TrialIdentity {
        keyed_run_with(
            fixture,
            head,
            std::slice::from_ref(&fixture.candidate),
            edit,
        )
    }

    fn keyed_run_with(
        fixture: &Fixture,
        head: &str,
        candidates: &[BaseCandidate],
        edit: impl Fn(&mut Value),
    ) -> TrialIdentity {
        keyed_run_into(fixture, head, candidates, None, edit).0
    }

    /// Lay out one keyed run; with `payload_tag` it goes into its own keyed
    /// payload directory (the tag varies the sample seed, so each tag is a
    /// distinct payload, and the runner artifacts, so each run's differ).
    /// Returns the identity and the run's evidence directory.
    fn keyed_run_into(
        fixture: &Fixture,
        head: &str,
        candidates: &[BaseCandidate],
        payload_tag: Option<&str>,
        edit: impl Fn(&mut Value),
    ) -> (TrialIdentity, PathBuf) {
        let identity = TrialIdentity {
            repository: "owner/repo".to_owned(),
            pull_request: 7,
            target: "mac".to_owned(),
            head_sha: head.to_owned(),
        };
        let tag = payload_tag.unwrap_or("");
        let binding = ExecutableReuseBinding {
            candidates: candidates.to_vec(),
            rules_digest: "r".repeat(64),
            derivation_code_dir: "/unused".to_owned(),
            derivation_code_sha256: fixture.code.digest.clone(),
            sample_seed: format!("{tag}{}", "s".repeat(64 - tag.len())),
            sample_percent: 5,
            build_dir: "build".to_owned(),
        };
        let payload = serde_json::to_vec(&json!({"executable_reuse": binding})).expect("payload");
        let trial_dir = result_directory(&fixture.state, &identity);
        let dir = if payload_tag.is_some() {
            crate::changed_surface::trial::payload_dir(&trial_dir, &sha256_hex(&payload))
        } else {
            trial_dir
        };
        fs::create_dir_all(&dir).expect("trial dir");
        fs::write(
            dir.join(ACTIVATION_RECEIPT),
            json!({"plan": {
                "repository": "owner/repo", "pull_request": 7, "target": "mac",
                "base_sha": fixture.base, "head_sha": head,
                "policy_digest": fixture.policy_digest,
                "execution_payload_digest": sha256_hex(&payload),
                "disposition": "keyed_full_shadow"}})
            .to_string(),
        )
        .expect("activation");
        fs::write(
            dir.join(KEYED_CONTEXT_RECEIPT),
            serde_json::to_vec(&KeyedRunContext {
                schema_version: 1,
                repository: "owner/repo".to_owned(),
                checkout: fixture.repo.to_string_lossy().into_owned(),
                execution_payload_b64: base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .encode(&payload),
            })
            .expect("context"),
        )
        .expect("context");
        let mut derived = serde_json::Map::new();
        derived.insert("base_record_run_id".into(), json!(fixture.candidate.run_id));
        derived.insert(
            "base_record_sha256".into(),
            json!(fixture.candidate.record_sha256),
        );
        for (name, key) in RUNNER_INPUTS {
            let bytes = format!("{{\"{name}\": \"{head}{tag}\"}}\n");
            fs::write(dir.join(name), &bytes).expect("input");
            derived.insert(key.into(), json!(sha256_hex(bytes.as_bytes())));
        }
        let mut result = json!({
            "recorded_at_unix_ns": recorded_at(head),
            "executable_reuse": {"derived": derived},
        });
        edit(&mut result);
        fs::write(dir.join("result-1.json"), result.to_string()).expect("result");
        (identity, dir)
    }

    /// A distinct adapter record time per fixture head (`h1` < `h2` < ...).
    fn recorded_at(head: &str) -> u64 {
        1_000 + head.trim_start_matches('h').parse::<u64>().unwrap_or(0)
    }

    type Calls = RefCell<Vec<Vec<String>>>;

    fn rederive(fixture: &Fixture, identity: &TrialIdentity, calls: &Calls) -> Outcome {
        let gh = |_: &Path, args: &[String]| {
            calls.borrow_mut().push(args.to_vec());
            Err("offline".to_owned())
        };
        top_level(rederive_trial(
            &fixture.state,
            identity,
            ProducedBy::Completion,
            &gh,
        ))
    }

    /// The head's top-level evidence outcome, which these fixtures write.
    fn top_level(outcomes: Vec<PayloadOutcome>) -> Outcome {
        let mut outcomes = outcomes.into_iter();
        let top = outcomes.next().expect("the top level is always judged");
        assert_eq!(top.payload_sha256, None);
        top.outcome
            .unwrap_or_else(|| panic!("rederive: {:?}", top.error))
    }

    fn verdict(outcome: &Outcome) -> &str {
        match outcome {
            Outcome::Recorded { verdict, .. } => verdict,
            other => panic!("not recorded: {other:?}"),
        }
    }

    #[test]
    fn a_faithful_rederivation_matches_once_and_is_never_counted_again() {
        let fixture = fixture(FAITHFUL);
        let calls = Calls::default();
        let identity = keyed_run(&fixture, "h1", |_| {});
        let first = rederive(&fixture, &identity, &calls);
        assert_eq!(verdict(&first), "match", "{first:?}");
        let dir = result_directory(&fixture.state, &identity);
        let Outcome::Recorded { receipt, .. } = &first else {
            unreachable!()
        };
        let written: Value =
            serde_json::from_slice(&fs::read(dir.join(receipt)).expect("receipt")).expect("json");
        assert_eq!(written["schema_version"], 1);
        assert_eq!(written["produced_by"], "completion");
        assert_eq!(
            written["keyed_record_run_id"],
            json!(fixture.candidate.run_id)
        );
        assert_eq!(written["toolchain_matched"], json!(true));
        assert_eq!(
            written["result_receipt_sha256"],
            json!(sha256_hex(
                &fs::read(dir.join("result-1.json")).expect("result")
            ))
        );
        assert!(matches!(
            rederive(&fixture, &identity, &calls),
            Outcome::AlreadyRecorded { .. }
        ));
        assert!(calls.borrow().is_empty(), "nothing tripped");
    }

    #[test]
    fn evidence_that_disagrees_with_the_binding_is_refused() {
        let fixture = fixture(FAITHFUL);
        let calls = Calls::default();
        let outside = keyed_run(&fixture, "h1", |result| {
            result["executable_reuse"]["derived"]["base_record_run_id"] = json!("elsewhere");
        });
        assert_eq!(verdict(&rederive(&fixture, &outside, &calls)), "refuse");

        let swapped = keyed_run(&fixture, "h2", |_| {});
        let dir = result_directory(&fixture.state, &swapped);
        fs::write(dir.join("selection.json"), "{}\n").expect("swap");
        let outcome = rederive(&fixture, &swapped, &calls);
        assert_eq!(verdict(&outcome), "refuse");
        let Outcome::Recorded {
            reason, tripped, ..
        } = outcome
        else {
            unreachable!()
        };
        assert!(reason.contains("selection.json"), "{reason}");
        assert!(
            tripped.is_some_and(|trip| trip.contains("PULP_REUSE_LIVE")),
            "the second refusal on this host trips the switch"
        );
        assert!(!calls.borrow().is_empty(), "the trip reached gh");
    }

    #[test]
    fn a_host_rederivation_that_disagrees_is_refused_and_counted_once_per_result() {
        let fixture = fixture(
            "import shutil, sys\n\
             open(sys.argv[2] + '/selection.json', 'w').write('{\"other\": 1}\\n')\n\
             shutil.copy(sys.argv[1] + '/executable-keys.json', sys.argv[2])\n",
        );
        let calls = Calls::default();
        let first = keyed_run(&fixture, "h1", |_| {});
        let outcome = rederive(&fixture, &first, &calls);
        assert_eq!(verdict(&outcome), "refuse");
        let Outcome::Recorded { tripped, .. } = outcome else {
            unreachable!()
        };
        assert!(tripped.is_none(), "one refusal does not trip");
        assert!(matches!(
            rederive(&fixture, &first, &calls),
            Outcome::AlreadyRecorded { .. }
        ));
        let ledger: Value = serde_json::from_slice(
            &fs::read(fixture.state.join("executable-reuse/refusals.json")).expect("ledger"),
        )
        .expect("json");
        assert_eq!(
            ledger["owner/repo"].as_object().map(serde_json::Map::len),
            Some(1)
        );
    }

    #[test]
    fn a_run_that_derived_nothing_is_not_derived() {
        let fixture = fixture(FAITHFUL);
        let calls = Calls::default();
        let nothing = keyed_run(&fixture, "h1", |result| {
            result["executable_reuse"]["derived"] = Value::Null;
        });
        assert_eq!(
            verdict(&rederive(&fixture, &nothing, &calls)),
            "not_derived"
        );
    }

    #[test]
    fn with_no_toolchain_match_the_host_keys_against_the_first_candidate() {
        let fixture = fixture(FAITHFUL);
        let calls = Calls::default();
        let unpicked = keyed_run(&fixture, "h1", |result| {
            result["executable_reuse"]["derived"]["base_record_run_id"] = Value::Null;
            result["executable_reuse"]["derived"]["base_record_sha256"] = Value::Null;
        });
        let outcome = rederive(&fixture, &unpicked, &calls);
        assert_eq!(verdict(&outcome), "match", "{outcome:?}");
        let dir = result_directory(&fixture.state, &unpicked);
        let receipt = named_files(&dir, REDERIVATION_RECEIPT_PREFIX).expect("receipts");
        let written: Value = serde_json::from_slice(&receipt[0].1).expect("json");
        assert_eq!(
            written["keyed_record_run_id"],
            json!(fixture.candidate.run_id)
        );
        assert_eq!(written["toolchain_matched"], json!(false));

        let half = keyed_run(&fixture, "h2", |result| {
            result["executable_reuse"]["derived"]["base_record_sha256"] = Value::Null;
        });
        assert_eq!(verdict(&rederive(&fixture, &half, &calls)), "refuse");
    }

    #[test]
    fn with_no_candidate_bound_there_is_nothing_to_rederive() {
        let fixture = fixture(FAITHFUL);
        let calls = Calls::default();
        let identity = keyed_run_with(&fixture, "h1", &[], |result| {
            result["executable_reuse"]["derived"]["base_record_run_id"] = Value::Null;
            result["executable_reuse"]["derived"]["base_record_sha256"] = Value::Null;
        });
        let outcome = rederive(&fixture, &identity, &calls);
        assert_eq!(verdict(&outcome), "not_derived", "{outcome:?}");
        assert!(
            !fixture
                .state
                .join("executable-reuse/refusals.json")
                .exists()
        );
    }

    #[test]
    fn an_unmatched_build_dir_is_reported_not_refused() {
        let fixture = fixture(FAITHFUL);
        let calls = Calls::default();
        let identity = keyed_run(&fixture, "h1", |_| {});
        let dir = result_directory(&fixture.state, &identity);
        // The runner's manifest says the build_dir matched no registration;
        // the host derives the same from the same inputs.
        let manifest = "{\"inventory\": \"inventory_unmatched\"}\n";
        fs::write(dir.join("executable-keys.json"), manifest).expect("manifest");
        let mut result: Value =
            serde_json::from_slice(&fs::read(dir.join("result-1.json")).expect("result"))
                .expect("json");
        result["executable_reuse"]["derived"]["key_manifest_sha256"] =
            json!(sha256_hex(manifest.as_bytes()));
        fs::write(dir.join("result-1.json"), result.to_string()).expect("result");
        let outcome = rederive(&fixture, &identity, &calls);
        assert_eq!(verdict(&outcome), "match", "{outcome:?}");
        let receipt = named_files(&dir, REDERIVATION_RECEIPT_PREFIX).expect("receipts");
        let written: Value = serde_json::from_slice(&receipt[0].1).expect("json");
        assert!(
            written["diagnostics"][0]
                .as_str()
                .is_some_and(|line| line.starts_with("inventory_unmatched")),
            "{written}"
        );
        assert!(
            !fixture
                .state
                .join("executable-reuse/refusals.json")
                .exists()
        );
    }

    #[test]
    fn the_sweep_picks_up_runs_without_a_verdict_and_names_itself() {
        let fixture = fixture(FAITHFUL);
        // Filed newest first, so the filesystem's order contradicts the
        // adapter's record times: only the record time can order these.
        let newer = keyed_run(&fixture, "h2", |_| {});
        let older = keyed_run(&fixture, "h1", |_| {});
        let gh = |_: &Path, _: &[String]| Err("offline".to_owned());
        let swept = sweep(&fixture.state, 1, &gh);
        assert_eq!(swept.len(), 1, "capped");
        assert_eq!(swept[0].0, newer, "newest first");
        let swept = sweep(&fixture.state, 8, &gh);
        assert_eq!(swept.len(), 1, "the newer one already has its verdict");
        assert_eq!(swept[0].0, older);
        let dir = result_directory(&fixture.state, &older);
        let receipt = named_files(&dir, REDERIVATION_RECEIPT_PREFIX).expect("receipts");
        let written: Value = serde_json::from_slice(&receipt[0].1).expect("json");
        assert_eq!(written["produced_by"], "sweep");
        assert!(sweep(&fixture.state, 8, &gh).is_empty());
    }

    #[test]
    fn the_sweep_breaks_a_record_time_tie_by_path() {
        let fixture = fixture(FAITHFUL);
        let a = keyed_run(&fixture, "h1", |_| {});
        let b = keyed_run(&fixture, "h1x", |result| {
            result["recorded_at_unix_ns"] = json!(recorded_at("h1"));
        });
        let gh = |_: &Path, _: &[String]| Err("offline".to_owned());
        let first = sweep(&fixture.state, 1, &gh).remove(0).0;
        let want = if result_directory(&fixture.state, &a) < result_directory(&fixture.state, &b) {
            a
        } else {
            b
        };
        assert_eq!(first, want, "equal record times order by trial directory");
    }

    #[test]
    fn a_retried_refusal_of_one_result_is_counted_once() {
        // A crash between counting and writing the receipt makes the next
        // call count the same result again; the ledger key absorbs it.
        let state = tempfile::tempdir().expect("state");
        let count = |head: &str, result: &str| {
            record_refusal(state.path(), "owner/repo", head, result, "why").expect("count")
        };
        assert_eq!(count("h1", "r1"), 1);
        assert_eq!(count("h1", "r1"), 1, "the same result again");
        assert_eq!(count("h1", "r2"), 2, "another result of the same head");
        assert_eq!(
            record_refusal(state.path(), "other/repo", "h1", "r1", "why").expect("count"),
            1,
            "counted per repository"
        );
    }

    #[test]
    fn a_base_command_sees_only_the_allowed_environment_and_reports_its_stderr() {
        let dir = tempfile::tempdir().expect("dir");
        let run = |code: &str| {
            run_base_command(
                &[
                    "python3".to_owned(),
                    "-I".to_owned(),
                    "-c".to_owned(),
                    code.to_owned(),
                ],
                dir.path(),
                "the probe",
            )
        };
        let outside: Vec<String> = std::env::vars_os()
            .filter_map(|(name, _)| name.into_string().ok())
            .filter(|name| !COMMAND_ENV.contains(&name.as_str()))
            .collect();
        assert!(
            !outside.is_empty(),
            "control: this process has other variables"
        );
        let seen = run("import os, json; print(json.dumps(sorted(os.environ)))").expect("run");
        let names: Vec<String> = serde_json::from_slice(&seen).expect("json");
        for name in &outside {
            // The interpreter may set a locale variable itself; nothing else
            // from this process may reach the command.
            assert!(
                !names.contains(name) || name.starts_with("LC_") || name.starts_with("__CF"),
                "{name} leaked"
            );
        }
        assert!(
            names.contains(&"PATH".to_owned()),
            "control: PATH is passed"
        );

        let long = "x".repeat(500);
        let error = run(&format!(
            "import sys; sys.stderr.write('{long}TAIL'); sys.exit(3)"
        ))
        .expect_err("fails");
        assert!(error.ends_with("TAIL"), "{error}");
        assert!(
            error.len() < 400,
            "only the stderr tail is kept: {}",
            error.len()
        );
    }

    fn written_receipt(fixture: &Fixture, identity: &TrialIdentity) -> Value {
        let dir = result_directory(&fixture.state, identity);
        let receipt = named_files(&dir, REDERIVATION_RECEIPT_PREFIX).expect("receipts");
        serde_json::from_slice(&receipt[0].1).expect("json")
    }

    #[test]
    fn what_live_reuse_would_have_got_wrong_trips_at_once() {
        type Edit = fn(&mut Value);
        let cases: [(&str, Edit, &str); 3] = [
            (
                "h1",
                |r| r["executable_reuse"]["sampled_failures"] = json!(["s1"]),
                "sampled would-skip tests failed (1): s1",
            ),
            (
                "h2",
                |r| r["executable_reuse"]["false_skips"] = json!(["f1", "f2"]),
                "would-skip tests failed in the full run (2): f1, f2",
            ),
            (
                "h3",
                |r| r["executable_reuse"]["derived"]["unreached_changed"] = json!(["test/x"]),
                "executables keyed unchanged were rebuilt to different bytes (1): test/x",
            ),
        ];
        let fixture = fixture(FAITHFUL);
        for (head, edit, signal) in cases {
            let calls = Calls::default();
            let identity = keyed_run(&fixture, head, edit);
            let outcome = rederive(&fixture, &identity, &calls);
            assert_eq!(verdict(&outcome), "match", "{head}: {outcome:?}");
            let Outcome::Recorded { tripped, .. } = outcome else {
                unreachable!()
            };
            assert!(
                tripped.is_some_and(|trip| trip.contains("PULP_REUSE_LIVE")),
                "{head}: one signal trips without waiting for a second"
            );
            assert!(!calls.borrow().is_empty(), "{head}: the trip reached gh");
            assert_eq!(
                written_receipt(&fixture, &identity)["trip_reasons"],
                json!([signal])
            );
        }
        assert!(
            !fixture
                .state
                .join("executable-reuse/refusals.json")
                .exists(),
            "a trip signal is not a refusal"
        );
    }

    #[test]
    fn a_clean_keyed_run_trips_nothing() {
        let fixture = fixture(FAITHFUL);
        let calls = Calls::default();
        let identity = keyed_run(&fixture, "h1", |result| {
            result["executable_reuse"]["sampled_failures"] = json!([]);
            result["executable_reuse"]["false_skips"] = json!([]);
            result["executable_reuse"]["derived"]["unreached_changed"] = json!([]);
        });
        let Outcome::Recorded { tripped, .. } = rederive(&fixture, &identity, &calls) else {
            panic!("recorded");
        };
        assert!(tripped.is_none());
        assert!(calls.borrow().is_empty());
        assert!(
            written_receipt(&fixture, &identity)
                .get("trip_reasons")
                .is_none()
        );
    }

    #[test]
    fn signals_in_refused_evidence_are_not_read() {
        let fixture = fixture(FAITHFUL);
        let calls = Calls::default();
        let identity = keyed_run(&fixture, "h1", |result| {
            result["executable_reuse"]["sampled_failures"] = json!(["s1"]);
            result["executable_reuse"]["derived"]["base_record_run_id"] = json!("elsewhere");
        });
        let Outcome::Recorded {
            verdict, tripped, ..
        } = rederive(&fixture, &identity, &calls)
        else {
            panic!("recorded");
        };
        assert_eq!(verdict, "refuse");
        assert!(tripped.is_none(), "a first refusal does not trip");
        assert!(
            written_receipt(&fixture, &identity)
                .get("trip_reasons")
                .is_none()
        );
    }

    #[test]
    fn a_trip_reason_lists_five_names_and_counts_the_rest() {
        let result = json!({"executable_reuse": {
            "sampled_failures": ["a", "b", "c", "d", "e", "f", "g"]}});
        assert_eq!(
            trip_reasons(&result),
            ["sampled would-skip tests failed (7): a, b, c, d, e and 2 more"]
        );
        assert!(trip_reasons(&json!({"executable_reuse": {"sampled_failures": null}})).is_empty());
    }

    /// Replace a keyed run's manifest and restate its hash in the result.
    fn with_manifest(fixture: &Fixture, identity: &TrialIdentity, manifest: &Value) {
        let dir = result_directory(&fixture.state, identity);
        let bytes = serde_json::to_vec(manifest).expect("manifest");
        fs::write(dir.join("executable-keys.json"), &bytes).expect("write");
        let mut result: Value =
            serde_json::from_slice(&fs::read(dir.join("result-1.json")).expect("result"))
                .expect("json");
        result["executable_reuse"]["derived"]["key_manifest_sha256"] = json!(sha256_hex(&bytes));
        fs::write(dir.join("result-1.json"), result.to_string()).expect("result");
    }

    #[test]
    fn the_executables_live_reuse_got_wrong_are_named_for_the_key_blind_list() {
        let fixture = fixture(FAITHFUL);
        let calls = Calls::default();
        let identity = keyed_run(&fixture, "h1", |result| {
            result["executable_reuse"]["false_skips"] = json!(["t1"]);
            result["executable_reuse"]["sampled_failures"] = json!(["t3"]);
            result["executable_reuse"]["derived"]["unreached_changed"] = json!(["test/d"]);
        });
        with_manifest(
            &fixture,
            &identity,
            &json!({"executables": {
                "test/a": {"registrations": ["t1"]},
                "test/b": {"registrations": ["t2"]},
                "test/c": {"registrations": ["t3", "t4"]}}}),
        );
        let outcome = rederive(&fixture, &identity, &calls);
        assert_eq!(verdict(&outcome), "match", "{outcome:?}");
        assert_eq!(
            written_receipt(&fixture, &identity)["key_blind_candidates"],
            json!(["test/a", "test/c", "test/d"])
        );
    }

    #[test]
    fn closure_modules_without_a_hash_are_reported_not_tripped() {
        let fixture = fixture(FAITHFUL);
        let calls = Calls::default();
        let identity = keyed_run(&fixture, "h1", |result| {
            result["executable_reuse"]["derived"]["unreached_changed"] = json!([]);
            result["executable_reuse"]["derived"]["unreached_unchecked_modules"] =
                json!(["lib/m1.dylib", "lib/m2.dylib"]);
        });
        let Outcome::Recorded { tripped, .. } = rederive(&fixture, &identity, &calls) else {
            panic!("recorded");
        };
        assert!(tripped.is_none());
        let receipt = written_receipt(&fixture, &identity);
        assert!(
            receipt["diagnostics"]
                .as_array()
                .expect("diagnostics")
                .iter()
                .any(|line| line
                    .as_str()
                    .is_some_and(|line| line.starts_with("unreached_unchecked_modules (2)"))),
            "{receipt}"
        );
    }

    #[test]
    fn without_a_manifest_only_rebuilt_executables_are_candidates() {
        let result = json!({"executable_reuse": {
            "false_skips": ["t1"],
            "derived": {"unreached_changed": ["test/x"]}}});
        assert_eq!(key_blind_candidates(&result, None), ["test/x"]);
        assert!(key_blind_candidates(&json!({}), None).is_empty());
    }

    #[test]
    fn an_activation_conflict_is_never_a_refusal() {
        // A conflicting re-ship runs its configured stages unkeyed and writes
        // no result, so the host has nothing to re-derive for it: the head
        // reads as not keyed (an earlier unkeyed activation) or as no result
        // yet (an earlier keyed one), and nothing is counted or tripped.
        let fixture = fixture(FAITHFUL);
        let calls = Calls::default();
        let gh = |_: &Path, args: &[String]| {
            calls.borrow_mut().push(args.to_vec());
            Err("offline".to_owned())
        };
        for (head, disposition, expected) in [
            ("h1", "bounded", Outcome::NotKeyed),
            ("h2", "keyed_full_shadow", Outcome::NoResult),
        ] {
            let identity = keyed_run(&fixture, head, |_| {});
            let dir = result_directory(&fixture.state, &identity);
            fs::remove_file(dir.join("result-1.json")).expect("no result from the unkeyed re-ship");
            let mut activation: Value = serde_json::from_slice(
                &fs::read(dir.join(ACTIVATION_RECEIPT)).expect("activation"),
            )
            .expect("json");
            activation["plan"]["disposition"] = json!(disposition);
            fs::write(dir.join(ACTIVATION_RECEIPT), activation.to_string()).expect("write");
            fs::write(
                dir.join("fallback-1-2-0.json"),
                json!({"category": "activation_conflict"}).to_string(),
            )
            .expect("diagnostic");
            assert_eq!(
                top_level(rederive_trial(
                    &fixture.state,
                    &identity,
                    ProducedBy::Completion,
                    &gh
                )),
                expected,
                "{head}"
            );
        }
        assert!(calls.borrow().is_empty());
        assert!(
            !fixture
                .state
                .join("executable-reuse/refusals.json")
                .exists()
        );
    }

    const DISAGREEING: &str = "import shutil, sys\n\
         open(sys.argv[2] + '/selection.json', 'w').write('{\"other\": 1}\\n')\n\
         shutil.copy(sys.argv[1] + '/executable-keys.json', sys.argv[2])\n";

    fn gh_recording(calls: &Calls) -> impl Fn(&Path, &[String]) -> Result<String, String> + '_ {
        |_: &Path, args: &[String]| {
            calls.borrow_mut().push(args.to_vec());
            Err("offline".to_owned())
        }
    }

    #[test]
    fn a_second_payload_run_never_disturbs_the_first_runs_rederivation() {
        let fixture = fixture(FAITHFUL);
        let calls = Calls::default();
        let candidates = std::slice::from_ref(&fixture.candidate);
        let (identity, first) = keyed_run_into(&fixture, "h1", candidates, Some("a"), |_| {});
        let (_, second) = keyed_run_into(&fixture, "h1", candidates, Some("b"), |_| {});
        assert_ne!(
            first, second,
            "two candidate sets, two evidence directories"
        );
        let outcomes = rederive_trial(
            &fixture.state,
            &identity,
            ProducedBy::Completion,
            &gh_recording(&calls),
        );
        let verdicts: Vec<_> = outcomes
            .iter()
            .filter(|payload| payload.payload_sha256.is_some())
            .map(|payload| verdict(payload.outcome.as_ref().expect("decided")).to_owned())
            .collect();
        assert_eq!(verdicts, ["match", "match"], "{outcomes:?}");
        assert_eq!(
            outcomes[0].outcome,
            Some(Outcome::NotKeyed),
            "the empty top level is not a run"
        );
        assert!(calls.borrow().is_empty(), "nothing tripped");
    }

    #[test]
    fn two_refused_payloads_on_one_head_count_two_and_the_second_trips() {
        let fixture = fixture(DISAGREEING);
        let calls = Calls::default();
        let candidates = std::slice::from_ref(&fixture.candidate);
        let (identity, _) = keyed_run_into(&fixture, "h1", candidates, Some("a"), |_| {});
        keyed_run_into(&fixture, "h1", candidates, Some("b"), |_| {});
        let outcomes = rederive_trial(
            &fixture.state,
            &identity,
            ProducedBy::Completion,
            &gh_recording(&calls),
        );
        let refusals: Vec<_> = outcomes
            .iter()
            .filter_map(|payload| match &payload.outcome {
                Some(Outcome::Recorded {
                    verdict, tripped, ..
                }) => Some((verdict.as_str(), tripped.is_some())),
                _ => None,
            })
            .collect();
        assert_eq!(
            refusals,
            [("refuse", false), ("refuse", true)],
            "{outcomes:?}"
        );
        let ledger: Value = serde_json::from_slice(
            &fs::read(fixture.state.join("executable-reuse/refusals.json")).expect("ledger"),
        )
        .expect("json");
        assert_eq!(
            ledger["owner/repo"].as_object().map(serde_json::Map::len),
            Some(2),
            "one entry per (head, result)"
        );
        assert!(!calls.borrow().is_empty(), "the trip reached gh");
    }

    #[test]
    fn the_sweep_reaches_keyed_payload_directories() {
        let fixture = fixture(FAITHFUL);
        let calls = Calls::default();
        let candidates = std::slice::from_ref(&fixture.candidate);
        keyed_run_into(&fixture, "h1", candidates, Some("a"), |_| {});
        keyed_run_into(&fixture, "h1", candidates, Some("b"), |_| {});
        let swept = sweep(&fixture.state, 8, &gh_recording(&calls));
        let verdicts: Vec<_> = swept
            .iter()
            .map(|(_, outcome)| verdict(outcome.as_ref().expect("decided")).to_owned())
            .collect();
        assert_eq!(verdicts, ["match", "match"], "{swept:?}");
        assert!(
            sweep(&fixture.state, 8, &gh_recording(&calls)).is_empty(),
            "each once"
        );
    }

    /// Rewrite a laid-out keyed run into schema 3: the binding moves from the
    /// payload into its file, and the payload names it by digest.
    fn to_schema_3(dir: &Path) -> Vec<u8> {
        let context_path = dir.join(KEYED_CONTEXT_RECEIPT);
        let mut context: KeyedRunContext =
            serde_json::from_slice(&fs::read(&context_path).expect("context")).expect("json");
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&context.execution_payload_b64)
            .expect("payload");
        let mut payload: Value = serde_json::from_slice(&payload).expect("json");
        let binding = payload
            .as_object_mut()
            .expect("payload")
            .remove("executable_reuse")
            .expect("inline binding");
        let bytes = serde_json::to_vec(&binding).expect("binding");
        fs::write(
            dir.join(crate::changed_surface::EXECUTABLE_REUSE_BINDING_FILE),
            &bytes,
        )
        .expect("binding file");
        payload["executable_reuse_binding_digest"] = json!(sha256_hex(&bytes));
        let payload = serde_json::to_vec(&payload).expect("payload");
        context.execution_payload_b64 =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&payload);
        fs::write(&context_path, serde_json::to_vec(&context).expect("json")).expect("context");
        let mut activation: Value =
            serde_json::from_slice(&fs::read(dir.join(ACTIVATION_RECEIPT)).expect("activation"))
                .expect("json");
        activation["plan"]["execution_payload_digest"] = json!(sha256_hex(&payload));
        fs::write(dir.join(ACTIVATION_RECEIPT), activation.to_string()).expect("activation");
        bytes
    }

    #[test]
    fn a_schema_3_run_is_judged_against_its_binding_file() {
        let fixture = fixture(FAITHFUL);
        let calls = Calls::default();
        let identity = keyed_run(&fixture, "h1", |_| {});
        to_schema_3(&result_directory(&fixture.state, &identity));
        assert_eq!(verdict(&rederive(&fixture, &identity, &calls)), "match");
    }

    #[test]
    fn a_binding_file_that_moved_after_the_run_is_refused() {
        let fixture = fixture(FAITHFUL);
        let calls = Calls::default();
        let identity = keyed_run(&fixture, "h1", |_| {});
        let dir = result_directory(&fixture.state, &identity);
        let mut bytes = to_schema_3(&dir);
        bytes.push(b' ');
        fs::write(
            dir.join(crate::changed_surface::EXECUTABLE_REUSE_BINDING_FILE),
            &bytes,
        )
        .expect("tamper");
        let outcome = rederive(&fixture, &identity, &calls);
        assert_eq!(verdict(&outcome), "refuse");
        let Outcome::Recorded { reason, .. } = outcome else {
            unreachable!()
        };
        assert!(reason.contains("not the one the payload named"), "{reason}");
    }

    #[test]
    fn a_run_whose_runner_refused_the_binding_is_not_derived() {
        let fixture = fixture(FAITHFUL);
        let calls = Calls::default();
        let identity = keyed_run(&fixture, "h1", |result| {
            result["executable_reuse_binding"] =
                json!({"digest": "d", "status": "keyed_binding_mismatch"});
        });
        to_schema_3(&result_directory(&fixture.state, &identity));
        assert_eq!(
            verdict(&rederive(&fixture, &identity, &calls)),
            "not_derived"
        );
        assert!(
            !fixture
                .state
                .join("executable-reuse/refusals.json")
                .exists(),
            "an unkeyed run is never a refusal"
        );
    }
}
