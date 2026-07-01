//! Opt-in, default-off same-backend retry policy for local ship/run legs.
//!
//! A LOCAL leg that fails with a transient `INFRA` blip (a momentary network /
//! runner hiccup, not an authoritative test failure) is worth re-running once on
//! the same backend before the failure is recorded. This module resolves that
//! policy from `[ship] transient_local_retries` once at the command layer, where
//! `LoadedConfig` is available, and hands the execution seam a small `Copy`
//! value — mirroring how `host_health::incident_reclassify_path` resolves the
//! reclassification opt-in.
//!
//! The default is zero (off): with no `[ship]` block, or the key absent, the
//! live CI machine behaves exactly as before — a single attempt per leg.

use serde::Deserialize;

use crate::config::LoadedConfig;

/// Upper bound on same-backend local retries. A transient blip clears in one
/// re-run; more than a couple attempts just burns wall-clock on a leg that is
/// probably failing for a real reason. Values above this clamp down to it.
pub const MAX_TRANSIENT_LOCAL_RETRIES: u32 = 2;

/// Resolved same-backend retry budget for local legs. `Copy` so it threads
/// through the execution seam as cheaply as a bool.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TransientRetryPolicy {
    max_retries: u32,
}

impl TransientRetryPolicy {
    /// The off (default) policy: no same-backend retries.
    #[must_use]
    pub fn disabled() -> Self {
        Self { max_retries: 0 }
    }

    /// Construct a policy with an explicit budget, clamped to
    /// `0..=MAX_TRANSIENT_LOCAL_RETRIES`.
    #[must_use]
    pub fn with_max_retries(max_retries: u32) -> Self {
        Self {
            max_retries: max_retries.min(MAX_TRANSIENT_LOCAL_RETRIES),
        }
    }

    /// Number of extra same-backend attempts permitted after the first.
    #[must_use]
    pub fn max_retries(self) -> u32 {
        self.max_retries
    }

    /// Whether any same-backend retry is permitted at all.
    #[must_use]
    pub fn is_enabled(self) -> bool {
        self.max_retries > 0
    }
}

/// Raw `[ship]` config sub-table. `#[serde(default)]` makes the block and every
/// field optional, so absence is the off-by-default state. Deserialized as `i64`
/// to tolerate any integer a hand-edited config might carry (including negatives
/// or absurdly large values), then clamped — a malformed value fails safe to
/// off rather than rejecting the whole config.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
struct ShipRetryConfig {
    transient_local_retries: i64,
}

/// Resolve the opt-in `[ship] transient_local_retries` budget into a policy.
/// Missing/invalid config, or a value `<= 0`, yields the disabled policy;
/// anything above [`MAX_TRANSIENT_LOCAL_RETRIES`] clamps down to it.
#[must_use]
pub fn transient_local_retry_policy(config: &LoadedConfig) -> TransientRetryPolicy {
    let cfg: ShipRetryConfig = config
        .get("ship")
        .and_then(|value| value.clone().try_into().ok())
        .unwrap_or_default();
    let clamped = cfg
        .transient_local_retries
        .clamp(0, i64::from(MAX_TRANSIENT_LOCAL_RETRIES));
    // Clamp guarantees `0..=MAX_TRANSIENT_LOCAL_RETRIES`, so `try_from` always
    // succeeds; the `unwrap_or(0)` is a fail-safe that keeps the policy off.
    TransientRetryPolicy::with_max_retries(u32::try_from(clamped).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::{MAX_TRANSIENT_LOCAL_RETRIES, TransientRetryPolicy, transient_local_retry_policy};
    use crate::config::{LoadedConfig, LocalOverlaySource};
    use toml::Table;

    fn loaded_config(toml: &str) -> LoadedConfig {
        LoadedConfig {
            data: toml.parse::<Table>().expect("toml"),
            global_dir: std::path::PathBuf::from("/tmp/global"),
            project_dir: None,
            local_dir: None,
            local_overlay_source: LocalOverlaySource::None,
        }
    }

    #[test]
    fn absent_config_is_disabled() {
        let policy = transient_local_retry_policy(&loaded_config(""));
        assert_eq!(policy, TransientRetryPolicy::disabled());
        assert!(!policy.is_enabled());
        assert_eq!(policy.max_retries(), 0);
    }

    #[test]
    fn absent_key_in_present_table_is_disabled() {
        let policy = transient_local_retry_policy(&loaded_config("[ship]\nother = true\n"));
        assert_eq!(policy.max_retries(), 0);
    }

    #[test]
    fn explicit_one_enables_a_single_retry() {
        let policy =
            transient_local_retry_policy(&loaded_config("[ship]\ntransient_local_retries = 1\n"));
        assert!(policy.is_enabled());
        assert_eq!(policy.max_retries(), 1);
    }

    #[test]
    fn oversized_value_clamps_to_max() {
        let policy =
            transient_local_retry_policy(&loaded_config("[ship]\ntransient_local_retries = 99\n"));
        assert_eq!(policy.max_retries(), MAX_TRANSIENT_LOCAL_RETRIES);
    }

    #[test]
    fn negative_value_fails_safe_to_disabled() {
        let policy =
            transient_local_retry_policy(&loaded_config("[ship]\ntransient_local_retries = -3\n"));
        assert_eq!(policy.max_retries(), 0);
    }

    #[test]
    fn non_integer_value_fails_safe_to_disabled() {
        // A wrong type must not reject the whole config — it falls back to off.
        let policy = transient_local_retry_policy(&loaded_config(
            "[ship]\ntransient_local_retries = \"lots\"\n",
        ));
        assert_eq!(policy.max_retries(), 0);
    }

    #[test]
    fn constructor_clamps() {
        assert_eq!(TransientRetryPolicy::with_max_retries(0).max_retries(), 0);
        assert_eq!(TransientRetryPolicy::with_max_retries(1).max_retries(), 1);
        assert_eq!(
            TransientRetryPolicy::with_max_retries(50).max_retries(),
            MAX_TRANSIENT_LOCAL_RETRIES
        );
    }
}
