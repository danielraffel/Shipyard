//! The merge-queue dispatch-stall policy: what the steward does when GitHub
//! never hands the queue head's merge-group job to a runner.
//!
//! Under an `ALLGREEN` merge queue every entry behind the head is built on
//! the head's group, so one job that GitHub leaves `queued` with no runner
//! holds the whole queue until the ruleset's check timeout (360 minutes on
//! Generous-Corp/vellum). On 2026-11-28 Vellum run 46104476461 (#825, job
//! `gpu-macos-arm64` 140854026289) sat queued for 69 minutes while two newer
//! jobs with the same labels started on runners; it was cancelled by hand.
//!
//! Read from the base branch's `.shipyard/config.toml`, never a head:
//!
//! ```toml
//! [auto_merge]
//! mq_stall_recover = "off"          # off (default) | shadow | apply: the queue head
//! pr_stall_recover = "off"          # off (default) | shadow: approved PR heads, report only
//! mq_stall_minutes = 20             # 5..=360: how long the job waits before it counts
//! mq_stall_max_cancels_per_hour = 3 # 1..=20 per repository
//! ```
//!
//! `pr_stall_recover` reports the same shape on the required-check jobs of
//! open pull requests whose head the approval context reports green. It is
//! report only: the seven Vellum `gpu-macos-arm64` PR jobs queued from 05:12Z
//! on 2026-11-28 (runs 46111057074 to 46129273537) were a backlog on their
//! runner label served out of order, not jobs GitHub refused a runner, and a
//! cancel and re-run only resets a job's place. `apply` is read as `shadow`
//! and reported in [`MqStallPolicy::error`].
//!
//! The queue-head detection runs on every merge-queue base; the pull request
//! targets are read only when `pr_stall_recover` is not `off`. `shadow` adds
//! `would_cancel`; `mq_stall_recover = "apply"` cancels the stalled run, once
//! per run id and at most once per pull request in six hours, and proves on a
//! later pass that the entry moved. A value that does not parse keeps `off`
//! and is reported in [`MqStallPolicy::error`].

use serde::Serialize;

/// `[auto_merge]` key selecting [`MqStallPolicy::mode`].
pub const MODE_KEY: &str = "mq_stall_recover";
/// `[auto_merge]` key selecting [`MqStallPolicy::pr_mode`].
pub const PR_MODE_KEY: &str = "pr_stall_recover";
/// `[auto_merge]` key selecting [`MqStallPolicy::minutes`].
pub const MINUTES_KEY: &str = "mq_stall_minutes";
/// `[auto_merge]` key selecting [`MqStallPolicy::max_cancels_per_hour`].
pub const MAX_PER_HOUR_KEY: &str = "mq_stall_max_cancels_per_hour";
/// The default wait before a queued, runnerless job counts as stalled.
pub const DEFAULT_MINUTES: u32 = 20;
/// The default cap on stall cancels per repository per hour.
pub const DEFAULT_MAX_PER_HOUR: u32 = 3;

/// Whether the steward cancels a stalled queue-head run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MqStallMode {
    /// Report the stall; never act. The default.
    #[default]
    Off,
    /// As off, and report `would_cancel`.
    Shadow,
    /// Cancel the stalled run once, then prove progress on the next pass.
    Apply,
}

impl MqStallMode {
    /// Parse `off`, `shadow` or `apply`; anything else is `None`.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "off" => Some(Self::Off),
            "shadow" => Some(Self::Shadow),
            "apply" => Some(Self::Apply),
            _ => None,
        }
    }

    /// The config value.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Shadow => "shadow",
            Self::Apply => "apply",
        }
    }
}

/// The dispatch-stall policy one base runs under.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MqStallPolicy {
    /// Off, shadow or apply, for the queue head's merge-group jobs.
    pub mode: MqStallMode,
    /// Off, shadow or apply, for the required-check jobs of approved pull
    /// request heads.
    pub pr_mode: MqStallMode,
    /// Minutes a queued job with no runner waits before it counts.
    pub minutes: u32,
    /// The most stall cancels per repository in any hour.
    pub max_cancels_per_hour: u32,
    /// Why a key did not parse, when one did not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Default for MqStallPolicy {
    fn default() -> Self {
        Self {
            mode: MqStallMode::Off,
            pr_mode: MqStallMode::Off,
            minutes: DEFAULT_MINUTES,
            max_cancels_per_hour: DEFAULT_MAX_PER_HOUR,
            error: None,
        }
    }
}

impl MqStallPolicy {
    /// The policy a parsed base config sets. Every key is optional; a value
    /// of the wrong type or out of range keeps its default and is named in
    /// [`Self::error`], so a typo never turns the act on.
    #[must_use]
    pub fn from_table(table: &toml::Table) -> Self {
        let mut policy = Self::default();
        let Some(section) = table
            .get(super::ejection_guards::CONFIG_TABLE)
            .and_then(toml::Value::as_table)
        else {
            return policy;
        };
        let mut errors = Vec::new();
        for (key, slot) in [
            (MODE_KEY, &mut policy.mode),
            (PR_MODE_KEY, &mut policy.pr_mode),
        ] {
            if let Some(value) = section.get(key) {
                match value.as_str().and_then(MqStallMode::parse) {
                    // Pull request stalls are report only.
                    Some(MqStallMode::Apply) if key == PR_MODE_KEY => {
                        *slot = MqStallMode::Shadow;
                        errors.push(format!(
                            "auto_merge.{key} = \"apply\" is not supported: pull request stalls are report only; read as shadow"
                        ));
                    }
                    Some(mode) => *slot = mode,
                    None => errors.push(format!(
                        "auto_merge.{key} must be \"off\", \"shadow\" or \"apply\"; kept off"
                    )),
                }
            }
        }
        let mut bounded = |key: &str, range: std::ops::RangeInclusive<u32>, slot: &mut u32| {
            if let Some(value) = section.get(key) {
                match value
                    .as_integer()
                    .and_then(|number| u32::try_from(number).ok())
                    .filter(|number| range.contains(number))
                {
                    Some(number) => *slot = number,
                    None => errors.push(format!(
                        "auto_merge.{key} must be an integer in {}..={}; kept {slot}",
                        range.start(),
                        range.end()
                    )),
                }
            }
        };
        bounded(MINUTES_KEY, 5..=360, &mut policy.minutes);
        bounded(MAX_PER_HOUR_KEY, 1..=20, &mut policy.max_cancels_per_hour);
        if !errors.is_empty() {
            policy.error = Some(errors.join("; "));
        }
        policy
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(raw: &str) -> toml::Table {
        raw.parse().expect("toml")
    }

    #[test]
    fn the_default_is_off_with_twenty_minutes_and_three_an_hour() {
        let policy = MqStallPolicy::from_table(&toml::Table::new());
        assert_eq!(policy, MqStallPolicy::default());
        assert_eq!(policy.mode, MqStallMode::Off);
        assert_eq!(policy.pr_mode, MqStallMode::Off);
        assert_eq!((policy.minutes, policy.max_cancels_per_hour), (20, 3));
    }

    #[test]
    fn the_two_modes_are_separate_knobs() {
        let policy = MqStallPolicy::from_table(&table(
            "[auto_merge]\nmq_stall_recover = \"shadow\"\npr_stall_recover = \"apply\"\n",
        ));
        assert_eq!(policy.mode, MqStallMode::Shadow);
        // Pull request stalls never act: apply reads as shadow and says so.
        assert_eq!(policy.pr_mode, MqStallMode::Shadow);
        assert!(
            policy
                .error
                .as_deref()
                .unwrap_or_default()
                .contains(PR_MODE_KEY)
        );
        let typo = MqStallPolicy::from_table(&table("[auto_merge]\npr_stall_recover = \"yes\"\n"));
        assert_eq!(typo.pr_mode, MqStallMode::Off);
        assert!(typo.error.expect("error").contains(PR_MODE_KEY));
    }

    #[test]
    fn explicit_values_win() {
        let policy = MqStallPolicy::from_table(&table(
            "[auto_merge]\nmq_stall_recover = \"apply\"\nmq_stall_minutes = 30\n\
             mq_stall_max_cancels_per_hour = 2\n",
        ));
        assert_eq!(policy.mode, MqStallMode::Apply);
        assert_eq!((policy.minutes, policy.max_cancels_per_hour), (30, 2));
        assert!(policy.error.is_none());
        let shadow =
            MqStallPolicy::from_table(&table("[auto_merge]\nmq_stall_recover = \"shadow\"\n"));
        assert_eq!(shadow.mode, MqStallMode::Shadow);
    }

    #[test]
    fn a_typo_or_out_of_range_value_keeps_the_default_and_says_so() {
        let policy = MqStallPolicy::from_table(&table(
            "[auto_merge]\nmq_stall_recover = \"on\"\nmq_stall_minutes = 1\n\
             mq_stall_max_cancels_per_hour = \"3\"\n",
        ));
        assert_eq!(policy.mode, MqStallMode::Off);
        assert_eq!((policy.minutes, policy.max_cancels_per_hour), (20, 3));
        let error = policy.error.expect("error");
        assert!(error.contains(MODE_KEY), "{error}");
        assert!(error.contains(MINUTES_KEY), "{error}");
        assert!(error.contains(MAX_PER_HOUR_KEY), "{error}");
    }
}
