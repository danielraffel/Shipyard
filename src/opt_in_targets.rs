//! Opt-in targets as a reported fact, never a silent absence.
//!
//! A target with `default = false` is left out of `pr`, `ship` and `run`
//! unless something names it. Left out quietly, it reads to a later reader as
//! "the mac lane never ran", which is exactly the shape of a broken lane. So
//! every surface that lists targets or explains a ship's verdict names each
//! opt-in target with the same line, and none of them probes it, counts it as
//! a required context, or reports it as a validation gap.

use serde::Serialize;
use toml::Table;

use crate::config::LoadedConfig;
use crate::executor::dispatch::opt_in_target_names;

/// Status of an opt-in target that nothing requested.
pub const NOT_RUN: &str = "opt-in, not run";
/// Who decides whether a pull request lands when the target does not run.
pub const VERDICT_OWNER: &str = "required-checks";

/// One opt-in target, as every reporting surface renders it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct OptInTarget {
    /// Target name from `[targets.<name>]`.
    pub name: String,
    /// Always [`NOT_RUN`].
    pub status: &'static str,
    /// Always [`VERDICT_OWNER`].
    pub verdict_owner: &'static str,
}

impl OptInTarget {
    /// The one human line every surface prints for this target.
    #[must_use]
    pub fn line(&self) -> String {
        format!("{}: {NOT_RUN} (GitHub required checks decide)", self.name)
    }
}

/// Opt-in targets declared in a config table.
///
/// A malformed `default` yields nothing here: the command that would run the
/// target already refuses that config loudly, and a report is not the place
/// to repeat the refusal.
#[must_use]
pub fn from_table(data: &Table) -> Vec<OptInTarget> {
    opt_in_target_names(data)
        .unwrap_or_default()
        .into_iter()
        .map(|name| OptInTarget {
            name,
            status: NOT_RUN,
            verdict_owner: VERDICT_OWNER,
        })
        .collect()
}

/// Opt-in targets declared in a loaded config.
#[must_use]
pub fn from_config(config: &LoadedConfig) -> Vec<OptInTarget> {
    from_table(&config.data)
}

/// Whether `name` is an opt-in target in `data`.
#[must_use]
pub fn is_opt_in(data: &Table, name: &str) -> bool {
    opt_in_target_names(data).is_ok_and(|names| names.contains(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(text: &str) -> Table {
        text.parse::<Table>().expect("config TOML")
    }

    #[test]
    fn opt_in_target_renders_the_shared_line() {
        let targets = from_table(&table(
            "[targets.mac]\nbackend = \"local\"\ndefault = false\n\n[targets.linux]\nbackend = \"local\"\n",
        ));
        assert_eq!(targets.len(), 1);
        assert_eq!(
            targets[0].line(),
            "mac: opt-in, not run (GitHub required checks decide)"
        );
        assert_eq!(targets[0].verdict_owner, "required-checks");
    }

    #[test]
    fn default_targets_are_not_reported_as_opt_in() {
        let data = table("[targets.mac]\nbackend = \"local\"\n");
        assert!(from_table(&data).is_empty());
        assert!(!is_opt_in(&data, "mac"));
    }
}
