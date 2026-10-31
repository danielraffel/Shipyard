//! The "what happens next" block `shipyard pr` prints once a pull request is
//! open or updated.
//!
//! The text is derived from the same repository facts the merge steward
//! reads (merge queue, `allow_auto_merge`, required contexts, approval
//! context), through [`StewardPolicy`], so the block cannot claim an
//! admission path the steward would refuse. It is output only: nothing here
//! changes what `shipyard pr` does.

use serde_json::{Value, json};

use crate::merge_steward::StewardPolicy;

const VERDICT_LINE: &str =
    "Next: a reviewer posts an exact-head verdict once required checks are green.";
const QUEUE_LINE: &str =
    "The fleet steward enqueues the approved head into the merge queue; do not arm or enqueue.";
const EXACT_HEAD_LINE: &str = "The fleet steward merges the approved head exactly; do not merge.";
const NO_ADMISSION_LINE: &str = "No automatic admission exists for this repository (no merge queue and no enforced approval context); a maintainer merges.";
const UNKNOWN_LINE: &str = "Repository merge settings could not be read, so no admission path is claimed; do not arm or merge on your own.";
const HOLD_LINE: &str = "On a HOLD: merge main (no rebase), fix, push once.";

/// How the steward admits an approved head on this repository.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Admission {
    /// Native merge queue: the steward enqueues the approved head.
    NativeQueue,
    /// No queue, but GitHub enforces an app-bound approval context: the
    /// steward merges the approved head by exact head.
    ExactHeadMerge,
    /// Neither; nothing admits a head automatically.
    None,
    /// The repository settings could not be read.
    Unknown,
}

impl Admission {
    /// Stable label for the JSON `next_steps.admission` field.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::NativeQueue => "native_queue_exact_head",
            Self::ExactHeadMerge => "server_enforced_exact_head_merge",
            Self::None => "none",
            Self::Unknown => "unknown",
        }
    }
}

/// The block, plus the facts it was derived from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NextSteps {
    /// Admission path.
    pub admission: Admission,
    /// Observed `allow_auto_merge`, when read.
    pub allow_auto_merge: Option<bool>,
    /// Observed merge-queue presence, when read.
    pub merge_queue: Option<bool>,
    /// Required contexts observed on the base.
    pub required_contexts: Vec<String>,
    /// Approval context the steward looks for.
    pub approval_context: String,
    /// Why the settings were unreadable, for [`Admission::Unknown`].
    pub unavailable_reason: Option<String>,
}

impl NextSteps {
    /// Derive the block from observed repository policy.
    #[must_use]
    pub fn from_policy(policy: &StewardPolicy) -> Self {
        let admission = if policy.merge_queue {
            Admission::NativeQueue
        } else if policy.server_enforced_exact_head_merge() {
            Admission::ExactHeadMerge
        } else {
            Admission::None
        };
        Self {
            admission,
            allow_auto_merge: Some(policy.native_auto_merge),
            merge_queue: Some(policy.merge_queue),
            required_contexts: policy
                .required_checks
                .iter()
                .map(|check| check.context.clone())
                .collect(),
            approval_context: policy.approval_context.clone(),
            unavailable_reason: None,
        }
    }

    /// The block for a repository whose settings could not be observed.
    #[must_use]
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            admission: Admission::Unknown,
            allow_auto_merge: None,
            merge_queue: None,
            required_contexts: Vec::new(),
            approval_context: crate::merge_steward::DEFAULT_APPROVAL_CONTEXT.to_owned(),
            unavailable_reason: Some(reason.into()),
        }
    }

    /// Whether a steward acts on this repository, so arming or merging by
    /// hand would contend with it.
    #[must_use]
    pub const fn steward_managed(&self) -> bool {
        matches!(
            self.admission,
            Admission::NativeQueue | Admission::ExactHeadMerge
        )
    }

    /// The human-readable lines, without trailing newlines.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        let admission = match self.admission {
            Admission::NativeQueue => QUEUE_LINE,
            Admission::ExactHeadMerge => EXACT_HEAD_LINE,
            Admission::None => NO_ADMISSION_LINE,
            Admission::Unknown => UNKNOWN_LINE,
        };
        vec![
            VERDICT_LINE.to_owned(),
            admission.to_owned(),
            HOLD_LINE.to_owned(),
        ]
    }

    /// The `next_steps` value of `--json` output.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "admission": self.admission.label(),
            "steward_managed": self.steward_managed(),
            "lines": self.lines(),
            "merge_queue": self.merge_queue,
            "allow_auto_merge": self.allow_auto_merge,
            "approval_context": self.approval_context,
            "required_contexts": self.required_contexts,
            "unavailable_reason": self.unavailable_reason,
        })
    }

    /// Write the block as human output.
    ///
    /// # Errors
    /// Returns the writer's error.
    pub fn write_human<W: std::io::Write>(&self, out: &mut W) -> std::io::Result<()> {
        for line in self.lines() {
            writeln!(out, "  {line}")?;
        }
        Ok(())
    }

    /// Warning for an explicit `--arm` on a steward-managed repository.
    #[must_use]
    pub fn arm_warning(&self) -> Option<String> {
        self.steward_managed().then(|| {
            "warning: --arm on a steward-managed repository: the fleet steward admits the approved head; arming it yourself contends with the steward.".to_owned()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::merge_steward::{DEFAULT_APPROVAL_CONTEXT, RequiredCheck};

    fn policy(merge_queue: bool, checks: Vec<RequiredCheck>) -> StewardPolicy {
        StewardPolicy {
            merge_queue,
            native_auto_merge: merge_queue,
            approval_context: DEFAULT_APPROVAL_CONTEXT.to_owned(),
            required_checks: checks,
            opt_out_label: "shipyard:no-auto-merge".to_owned(),
            provenance_blocking_labels: Vec::new(),
            managed_label: None,
            handoff_context: String::new(),
            max_transient_reruns: 1,
        }
    }

    fn check(context: &str, app_id: Option<u64>) -> RequiredCheck {
        RequiredCheck {
            context: context.to_owned(),
            app_id,
        }
    }

    #[test]
    fn native_queue_repository_says_the_steward_enqueues() {
        let steps = NextSteps::from_policy(&policy(true, vec![check("macos", None)]));
        assert_eq!(steps.admission, Admission::NativeQueue);
        let lines = steps.lines();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("Next: a reviewer posts an exact-head verdict"));
        assert!(lines[1].contains("enqueues the approved head into the merge queue"));
        assert!(lines[1].contains("do not arm or enqueue"));
        assert_eq!(
            lines[2],
            "On a HOLD: merge main (no rebase), fix, push once."
        );
        assert!(steps.arm_warning().is_some());
    }

    #[test]
    fn enforced_approval_context_without_a_queue_says_the_steward_merges() {
        let steps = NextSteps::from_policy(&policy(
            false,
            vec![check("macos", None), check("review-verdict", Some(42))],
        ));
        assert_eq!(steps.admission, Admission::ExactHeadMerge);
        assert!(steps.lines()[1].contains("merges the approved head exactly; do not merge"));
        assert!(steps.arm_warning().is_some());
    }

    #[test]
    fn unbound_approval_context_is_not_an_admission_path() {
        // An unbound rule is satisfiable by any actor, so the steward refuses.
        let steps = NextSteps::from_policy(&policy(false, vec![check("review-verdict", None)]));
        assert_eq!(steps.admission, Admission::None);
    }

    #[test]
    fn repository_without_a_queue_or_approval_says_no_automatic_admission() {
        let steps = NextSteps::from_policy(&policy(false, vec![check("macos", None)]));
        assert_eq!(steps.admission, Admission::None);
        assert!(steps.lines()[1].starts_with("No automatic admission exists"));
        assert!(steps.arm_warning().is_none());
    }

    #[test]
    fn unreadable_settings_claim_no_path() {
        let steps = NextSteps::unavailable("boom");
        assert_eq!(steps.admission, Admission::Unknown);
        assert!(steps.lines()[1].contains("could not be read"));
        assert!(steps.arm_warning().is_none());
        assert_eq!(steps.to_json()["unavailable_reason"], "boom");
    }

    #[test]
    fn json_carries_the_observed_facts() {
        let steps = NextSteps::from_policy(&policy(true, vec![check("macos", None)]));
        let value = steps.to_json();
        assert_eq!(value["admission"], "native_queue_exact_head");
        assert_eq!(value["steward_managed"], true);
        assert_eq!(value["merge_queue"], true);
        assert_eq!(value["allow_auto_merge"], true);
        assert_eq!(value["required_contexts"][0], "macos");
        assert_eq!(value["lines"].as_array().expect("lines").len(), 3);
    }

    #[test]
    fn human_output_is_indented_lines() {
        let mut out = Vec::new();
        NextSteps::from_policy(&policy(true, Vec::new()))
            .write_human(&mut out)
            .expect("write");
        let text = String::from_utf8(out).expect("utf8");
        assert_eq!(text.lines().count(), 3);
        assert!(text.lines().all(|line| line.starts_with("  ")));
    }
}
