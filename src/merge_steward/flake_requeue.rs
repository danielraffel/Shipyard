//! The flake-class policy: what the steward does when the same failing test
//! ejects approved heads of different pull requests from the merge queue.
//!
//! A flaky test that fails one merge group in several ejects a different,
//! innocent pull request each time. Under the retry discipline
//! ([`super::EjectionGuards::infra_only_retry`]) none of those heads earns
//! the classifier's free same-head retry (no infrastructure signature, and
//! the test is not yet in `known_flaky_tests`), so each approved head waits
//! for a new push that changes nothing. When the same signature ejects two
//! or more different pull requests within the window, the steward reads it
//! as a flake and re-enqueues each such head once (see the steward's
//! `flake_requeue` module for the rules).
//!
//! Read from the base branch's `.shipyard/config.toml`, never a head:
//!
//! ```toml
//! [auto_merge]
//! flake_requeue = "shadow"     # off | shadow | apply
//! flake_window_hours = 24      # 1..=168
//! flake_max_requeues = 3       # 1..=50 flake re-enqueues per signature per window
//! ```
//!
//! Without `flake_requeue`, a merge-queue base runs `shadow` (report
//! `would_requeue`, file nothing), upgraded to `apply` when the base already
//! lets the steward act on dwell limits (`[auto_merge.dwell] act = "apply"`).
//! A base without a merge queue has nothing to eject and defaults to `off`.

use serde::Serialize;

use super::dwell::{ActMode, DwellPolicy};

/// `[auto_merge]` key selecting [`FlakePolicy::mode`].
pub const MODE_KEY: &str = "flake_requeue";
/// `[auto_merge]` key selecting [`FlakePolicy::window_hours`].
pub const WINDOW_KEY: &str = "flake_window_hours";
/// The default window between two ejections of the same signature.
pub const DEFAULT_WINDOW_HOURS: u32 = 24;
/// The longest window a config may set.
pub const MAX_WINDOW_HOURS: u32 = 168;
/// `[auto_merge]` key selecting [`FlakePolicy::max_requeues`].
pub const MAX_REQUEUES_KEY: &str = "flake_max_requeues";
/// The default cap on flake re-enqueues per signature per window.
pub const DEFAULT_MAX_REQUEUES: u32 = 3;

/// Whether the flake class re-enqueues and files issues.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FlakeMode {
    /// Nothing is recorded or reported.
    #[default]
    Off,
    /// Ejections are classified and reported (`would_requeue`), and the root
    /// cause is reported in the tick; nothing is re-enqueued or filed.
    Shadow,
    /// As shadow, and each flake-ejected head is re-enqueued once and the
    /// root-cause issue is filed.
    Apply,
}

impl FlakeMode {
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

/// The flake class one base runs under.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FlakePolicy {
    /// Off, shadow or apply.
    pub mode: FlakeMode,
    /// Two ejections of the same signature count together only when they
    /// happened within this many hours of each other.
    pub window_hours: u32,
    /// The most flake re-enqueues one signature may spend within the
    /// window, across all pull requests.
    pub max_requeues: u32,
}

impl Default for FlakePolicy {
    fn default() -> Self {
        Self {
            mode: FlakeMode::Off,
            window_hours: DEFAULT_WINDOW_HOURS,
            max_requeues: DEFAULT_MAX_REQUEUES,
        }
    }
}

impl FlakePolicy {
    /// The policy of a base with no config: shadow exactly when it has a
    /// merge queue.
    #[must_use]
    pub fn for_base(merge_queue: bool) -> Self {
        Self {
            mode: if merge_queue {
                FlakeMode::Shadow
            } else {
                FlakeMode::Off
            },
            ..Self::default()
        }
    }

    /// The policy a parsed base config sets. An explicit `flake_requeue`
    /// wins; otherwise a merge-queue base whose dwell acts apply runs
    /// `apply`. An unknown value keeps the default.
    #[must_use]
    pub fn from_table(table: &toml::Table, merge_queue: bool) -> Self {
        let mut policy = Self::for_base(merge_queue);
        let section = table
            .get(super::ejection_guards::CONFIG_TABLE)
            .and_then(toml::Value::as_table);
        let explicit = section
            .and_then(|section| section.get(MODE_KEY))
            .and_then(toml::Value::as_str)
            .and_then(FlakeMode::parse);
        match explicit {
            Some(mode) => policy.mode = mode,
            None if merge_queue && DwellPolicy::from_table(table).act == ActMode::Apply => {
                policy.mode = FlakeMode::Apply;
            }
            None => {}
        }
        if let Some(hours) = section
            .and_then(|section| section.get(WINDOW_KEY))
            .and_then(toml::Value::as_integer)
            .and_then(|hours| u32::try_from(hours).ok())
            .filter(|hours| (1..=MAX_WINDOW_HOURS).contains(hours))
        {
            policy.window_hours = hours;
        }
        if let Some(cap) = section
            .and_then(|section| section.get(MAX_REQUEUES_KEY))
            .and_then(toml::Value::as_integer)
            .and_then(|cap| u32::try_from(cap).ok())
            .filter(|cap| (1..=50).contains(cap))
        {
            policy.max_requeues = cap;
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
    fn a_merge_queue_base_defaults_to_shadow_and_follows_dwell_apply() {
        assert_eq!(
            FlakePolicy::from_table(&toml::Table::new(), true).mode,
            FlakeMode::Shadow
        );
        assert_eq!(
            FlakePolicy::from_table(&toml::Table::new(), false).mode,
            FlakeMode::Off
        );
        let dwell_apply = table("[auto_merge.dwell]\nact = \"apply\"\n");
        assert_eq!(
            FlakePolicy::from_table(&dwell_apply, true).mode,
            FlakeMode::Apply
        );
        // No queue, nothing to eject: dwell apply does not switch it on.
        assert_eq!(
            FlakePolicy::from_table(&dwell_apply, false).mode,
            FlakeMode::Off
        );
    }

    #[test]
    fn an_explicit_mode_and_window_win() {
        let policy = FlakePolicy::from_table(
            &table(
                "[auto_merge]\nflake_requeue = \"shadow\"\nflake_window_hours = 6\n\
                 flake_max_requeues = 5\n\
                 [auto_merge.dwell]\nact = \"apply\"\n",
            ),
            true,
        );
        assert_eq!(policy.mode, FlakeMode::Shadow);
        assert_eq!(policy.window_hours, 6);
        assert_eq!(policy.max_requeues, 5);
        let off = FlakePolicy::from_table(&table("[auto_merge]\nflake_requeue = \"off\"\n"), true);
        assert_eq!(off.mode, FlakeMode::Off);
        // Out of range or unknown values keep the defaults.
        let odd = FlakePolicy::from_table(
            &table("[auto_merge]\nflake_requeue = \"yes\"\nflake_window_hours = 0\n"),
            true,
        );
        assert_eq!(odd, FlakePolicy::for_base(true));
    }
}
