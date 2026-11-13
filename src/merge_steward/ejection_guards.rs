//! The steward's merge-queue ejection guards, and the base config that
//! switches them.
//!
//! A merge-queue ejection is expensive twice: the batch it breaks rebuilds,
//! and the ejected pull request waits hours for a new head. Measured on
//! Generous-Corp/pulp over one week, an ejected pull request took a median of
//! 231 minutes to merge against 31 for one that merged first time. The three
//! guards here stop the steward admitting what it can already tell will be
//! ejected:
//!
//! * **Generated-path batch guard** ([`EjectionGuards::batch_guard`]): a pull
//!   request that changes a *generated path* (a file regenerated from the
//!   tree, such as a ctest inputs manifest) waits while another queued entry
//!   changes the same path. Two such heads in one batch conflict textually or
//!   drift the generated output, and neither is fixed by a same-head retry.
//! * **A red required check outranks a pending one**
//!   ([`EjectionGuards::red_outranks_pending`]): a head with a failed
//!   required check reports `blocked_on=required_failed` even while another
//!   required check is still running, instead of `required_pending`. The
//!   admission decision is unchanged (it waits, and never admits a red head);
//!   recovery still waits for a settled head before it asks an agent.
//! * **Retry discipline** ([`EjectionGuards::infra_only_retry`]): the one free
//!   same-head re-enqueue after an ejection is spent only on a known
//!   infrastructure signature (runner lost, cancelled, timed out, HTTP 5xx,
//!   rate limit, download failure), never on a failure that merely passed on
//!   the head; and `invalid_merge_commit` is never re-enqueued on the same
//!   head (it is refused like `merge_conflict`).
//!
//! All three are read from the base branch's tracked `.shipyard/config.toml`
//! `[auto_merge]` table, never from a head, and each defaults **on** for a
//! base that has a merge queue and off otherwise:
//!
//! ```toml
//! [auto_merge]
//! generated_paths = ["test/ctest_script_inputs.json", "test/ctest_script_inputs/**"]
//! generated_path_batch_guard = true   # default: on when the base has a merge queue
//! red_check_outranks_pending = true   # default: on when the base has a merge queue
//! infra_only_same_head_retry = true   # default: on when the base has a merge queue
//! known_flaky_tests = ["ui-script-plugin-editor-au"]  # names or `*` globs
//! red_everywhere = "shadow"           # off | shadow (default) | enforce
//! ```
//!
//! `red_everywhere` is not an ejection guard but lives in the same table: it
//! reports (and under `enforce` holds) pull requests that fail a required
//! check the same way as two or more others, a base-level cause.
//!
//! The batch guard needs `generated_paths`; with none listed it never defers.

use serde::Serialize;

/// The base config table the keys live in.
pub const CONFIG_TABLE: &str = "auto_merge";
/// Globs (`**` whole segments, `*` within a segment) naming generated paths.
pub const GENERATED_PATHS_KEY: &str = "generated_paths";
/// Switch for [`EjectionGuards::batch_guard`].
pub const BATCH_GUARD_KEY: &str = "generated_path_batch_guard";
/// Switch for [`EjectionGuards::red_outranks_pending`].
pub const RED_OUTRANKS_PENDING_KEY: &str = "red_check_outranks_pending";
/// Switch for [`EjectionGuards::infra_only_retry`].
pub const INFRA_ONLY_RETRY_KEY: &str = "infra_only_same_head_retry";
/// Test or check names (or `*` globs) whose failure earns the retry under
/// the discipline ([`EjectionGuards::known_flaky_tests`]).
pub const KNOWN_FLAKY_TESTS_KEY: &str = "known_flaky_tests";
/// The red-everywhere mode: `off`, `shadow` (default) or `enforce`.
pub const RED_EVERYWHERE_KEY: &str = "red_everywhere";

/// What the steward does when the same required check fails the same way on
/// many open pull requests (`[auto_merge] red_everywhere`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RedEverywhereMode {
    /// Nothing.
    Off,
    /// Report episodes and mark member rows. The default.
    #[default]
    Shadow,
    /// As shadow, and hold a member's unchanged head from the queue.
    Enforce,
}

impl RedEverywhereMode {
    /// Parse `off`, `shadow` or `enforce`; anything else is `None`.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "off" => Some(Self::Off),
            "shadow" => Some(Self::Shadow),
            "enforce" => Some(Self::Enforce),
            _ => None,
        }
    }

    /// The config value.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Shadow => "shadow",
            Self::Enforce => "enforce",
        }
    }
}

/// The ejection guards one base runs under.
///
/// [`Default`] is every guard off: what a library caller or a test that does
/// not ask for them gets. The steward builds its value with
/// [`Self::from_config`], whose defaults follow the merge queue.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct EjectionGuards {
    /// Generated-path globs from the base config.
    pub generated_paths: Vec<String>,
    /// Defer an admission that shares a generated path with a queued entry.
    pub batch_guard: bool,
    /// A failed required check outranks a pending one in `blocked_on`.
    pub red_outranks_pending: bool,
    /// Spend the free same-head retry only on infrastructure signatures, and
    /// never re-enqueue an `invalid_merge_commit` head.
    pub infra_only_retry: bool,
    /// Known flaky tests or checks: under the discipline, an ejection whose
    /// failing steps name only these tests (or whose check is one), and whose
    /// check passed on the head, still earns the one same-head retry.
    pub known_flaky_tests: Vec<String>,
    /// What the steward does when the same required check fails the same way
    /// on many open pull requests (`[auto_merge] red_everywhere`). Shadow
    /// unless the base config says otherwise; an unknown value keeps shadow.
    pub red_everywhere: RedEverywhereMode,
    /// The base-poison admission hold and fix fast-track, from the base
    /// config's `[base_health]` table ([`crate::base_health::BasePoisonPolicy`]).
    pub base_poison: crate::base_health::BasePoisonPolicy,
    /// Why the base config could not be read, when it could not. The guards
    /// then keep their merge-queue defaults (on) with no generated paths.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_error: Option<String>,
}

impl EjectionGuards {
    /// The guards of a base with no config: on exactly when it has a merge
    /// queue.
    #[must_use]
    pub fn for_base(merge_queue: bool) -> Self {
        Self {
            generated_paths: Vec::new(),
            batch_guard: merge_queue,
            red_outranks_pending: merge_queue,
            infra_only_retry: merge_queue,
            known_flaky_tests: Vec::new(),
            red_everywhere: RedEverywhereMode::Shadow,
            base_poison: crate::base_health::BasePoisonPolicy::for_base(merge_queue),
            read_error: None,
        }
    }

    /// The guards a parsed base config sets. An absent key keeps the
    /// merge-queue default; a key of the wrong type is ignored the same way.
    #[must_use]
    pub fn from_table(table: &toml::Table, merge_queue: bool) -> Self {
        let mut guards = Self::for_base(merge_queue);
        guards.base_poison = crate::base_health::BasePoisonPolicy::from_table(table, merge_queue);
        let Some(section) = table.get(CONFIG_TABLE).and_then(toml::Value::as_table) else {
            return guards;
        };
        let flag = |key: &str, default: bool| {
            section
                .get(key)
                .and_then(toml::Value::as_bool)
                .unwrap_or(default)
        };
        guards.batch_guard = flag(BATCH_GUARD_KEY, guards.batch_guard);
        guards.red_outranks_pending = flag(RED_OUTRANKS_PENDING_KEY, guards.red_outranks_pending);
        guards.infra_only_retry = flag(INFRA_ONLY_RETRY_KEY, guards.infra_only_retry);
        if let Some(mode) = section
            .get(RED_EVERYWHERE_KEY)
            .and_then(toml::Value::as_str)
            .and_then(RedEverywhereMode::parse)
        {
            guards.red_everywhere = mode;
        }
        guards.known_flaky_tests = section
            .get(KNOWN_FLAKY_TESTS_KEY)
            .and_then(toml::Value::as_array)
            .map(|names| {
                names
                    .iter()
                    .filter_map(toml::Value::as_str)
                    .map(|name| name.trim().to_owned())
                    .filter(|name| !name.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        guards.generated_paths = section
            .get(GENERATED_PATHS_KEY)
            .and_then(toml::Value::as_array)
            .map(|paths| {
                paths
                    .iter()
                    .filter_map(toml::Value::as_str)
                    .map(|path| path.trim().trim_start_matches("./").to_owned())
                    .filter(|path| !path.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        guards
    }

    /// Read the guards from `base`'s tracked `.shipyard/config.toml`.
    ///
    /// Never fails: a missing file is [`Self::for_base`], and an unreadable
    /// or unparsable one is [`Self::for_base`] with [`Self::read_error`] set.
    /// That leaves every guard on for a merge-queue base, but with no
    /// generated paths; the admission path's own approval read refuses an
    /// unreadable config before any enqueue
    /// ([`crate::head_approval::read_policy_required`]).
    ///
    /// The argv is the one every other base-config read uses, so a steward
    /// pass serves all of them from one GitHub call ([`crate::gh_pass`]).
    #[must_use]
    pub fn from_config(
        run_gh: &dyn Fn(&[String]) -> Result<String, String>,
        repo: &str,
        base: &str,
        merge_queue: bool,
    ) -> Self {
        let raw = match run_gh(&[
            "api".to_owned(),
            "-H".to_owned(),
            "Accept: application/vnd.github.raw+json".to_owned(),
            format!("repos/{repo}/contents/.shipyard/config.toml?ref={base}"),
        ]) {
            Ok(raw) => raw,
            Err(detail) if detail.contains("404") || detail.contains("Not Found") => {
                return Self::for_base(merge_queue);
            }
            Err(detail) => {
                return Self {
                    read_error: Some(format!(".shipyard/config.toml on {base}: {detail}")),
                    ..Self::for_base(merge_queue)
                };
            }
        };
        match raw.parse::<toml::Table>() {
            Ok(table) => Self::from_table(&table, merge_queue),
            Err(error) => Self {
                read_error: Some(format!(
                    ".shipyard/config.toml on {base} does not parse: {error}"
                )),
                ..Self::for_base(merge_queue)
            },
        }
    }

    /// The paths in `changed` that match a configured generated path.
    #[must_use]
    pub fn generated_in<'a>(&self, changed: impl IntoIterator<Item = &'a str>) -> Vec<String> {
        let mut hits = changed
            .into_iter()
            .filter(|path| {
                self.generated_paths
                    .iter()
                    .any(|glob| crate::head_approval::glob_match(glob, path))
            })
            .map(str::to_owned)
            .collect::<Vec<_>>();
        hits.sort();
        hits.dedup();
        hits
    }

    /// Whether the batch guard can defer anything on this base.
    #[must_use]
    pub fn batch_guard_active(&self) -> bool {
        self.batch_guard && !self.generated_paths.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PULP: &str = r#"
[auto_merge]
generated_paths = [
  "test/ctest_script_inputs.json",
  "test/ctest_script_inputs/**",
  "docs/status/agent-capability-surface.json",
  "tools/scripts/agent_capability_manifest.py",
  "tools/scripts/agent_capability_catalog_*.py",
  "tools/agent-capabilities/contract-history.json",
  "docs/status/consumption-profiles.json",
]
"#;

    fn table(raw: &str) -> toml::Table {
        raw.parse().expect("toml")
    }

    #[test]
    fn guards_default_on_only_for_a_merge_queue_base() {
        let queued = EjectionGuards::from_table(&toml::Table::new(), true);
        assert!(queued.batch_guard && queued.red_outranks_pending && queued.infra_only_retry);
        assert!(
            !queued.batch_guard_active(),
            "no generated paths, nothing to guard"
        );
        let direct = EjectionGuards::from_table(&toml::Table::new(), false);
        assert!(!direct.batch_guard && !direct.red_outranks_pending && !direct.infra_only_retry);
        assert_eq!(EjectionGuards::default(), EjectionGuards::for_base(false));
    }

    #[test]
    fn each_guard_has_its_own_switch() {
        let guards = EjectionGuards::from_table(
            &table(
                "[auto_merge]\ngenerated_path_batch_guard = false\n\
                 red_check_outranks_pending = false\ninfra_only_same_head_retry = false\n",
            ),
            true,
        );
        assert!(!guards.batch_guard && !guards.red_outranks_pending && !guards.infra_only_retry);
        // A repository without a queue can opt in explicitly.
        let opted = EjectionGuards::from_table(
            &table("[auto_merge]\ninfra_only_same_head_retry = true\n"),
            false,
        );
        assert!(opted.infra_only_retry && !opted.batch_guard);
    }

    #[test]
    fn pulp_generated_paths_match_the_hot_files_from_the_report() {
        let guards = EjectionGuards::from_table(&table(PULP), true);
        assert!(guards.batch_guard_active());
        let hits = guards.generated_in([
            "test/ctest_script_inputs.json",
            "test/ctest_script_inputs/shard_07.json",
            "tools/scripts/agent_capability_catalog_dsp.py",
            "docs/status/consumption-profiles.json",
            "core/view/src/widgets.cpp",
            "tools/scripts/agent_capability_manifest_test.py",
            "test/ctest_script_inputs.jsonl",
        ]);
        assert_eq!(
            hits,
            vec![
                "docs/status/consumption-profiles.json",
                "test/ctest_script_inputs.json",
                "test/ctest_script_inputs/shard_07.json",
                "tools/scripts/agent_capability_catalog_dsp.py",
            ]
        );
    }

    #[test]
    fn known_flaky_tests_are_read_from_the_base_config() {
        let guards = EjectionGuards::from_table(
            &table(
                "[auto_merge]\nknown_flaky_tests = [\"ui-script-plugin-editor-au\", \" \", \"a public WaveNet node runs its GPU path*\"]\n",
            ),
            true,
        );
        assert_eq!(
            guards.known_flaky_tests,
            vec![
                "ui-script-plugin-editor-au".to_owned(),
                "a public WaveNet node runs its GPU path*".to_owned()
            ]
        );
        assert!(EjectionGuards::for_base(true).known_flaky_tests.is_empty());
    }

    #[test]
    fn an_unreadable_config_keeps_the_queue_defaults_and_says_why() {
        let failing = |_: &[String]| Err("HTTP 502: Bad Gateway".to_owned());
        let guards = EjectionGuards::from_config(&failing, "o/r", "main", true);
        assert!(guards.infra_only_retry && guards.red_outranks_pending);
        assert!(
            guards
                .read_error
                .as_deref()
                .is_some_and(|e| e.contains("502"))
        );
        let missing = |_: &[String]| Err("HTTP 404: Not Found".to_owned());
        let guards = EjectionGuards::from_config(&missing, "o/r", "main", true);
        assert_eq!(guards, EjectionGuards::for_base(true));
        let served = |args: &[String]| {
            assert!(args[3].ends_with("contents/.shipyard/config.toml?ref=main"));
            Ok(PULP.to_owned())
        };
        let guards = EjectionGuards::from_config(&served, "o/r", "main", true);
        assert_eq!(guards.generated_paths.len(), 7);
    }
}
