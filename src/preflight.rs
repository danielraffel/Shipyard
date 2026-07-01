//! Submission preflight checks for `ship --pr`.

use serde_json::{Value, json};
use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::path::{Path, PathBuf};

use crate::config::LoadedConfig;
use crate::daemon_version::{DaemonVersionRelation, read_daemon_version_relation};
use crate::executor::dispatch::{ExecutorDispatcher, ResolvedTarget};

/// Exit code used when a backend is unreachable before submission.
pub const EXIT_BACKEND_UNREACHABLE: u8 = 3;

/// Exit code used when the opt-in host-health gate hard-stops a saturated host.
pub const EXIT_HOST_UNHEALTHY: u8 = 4;

/// One unreachable target discovered during preflight.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TargetPreflightFailure {
    /// Target name.
    pub target_name: String,
    /// Backend label.
    pub backend: String,
    /// Human-readable backend diagnosis.
    pub message: String,
    /// Stable backend failure category.
    pub failure_category: Option<String>,
}

/// Preflight result for one target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TargetPreflightReport {
    /// Target name.
    pub target_name: String,
    /// Primary backend label.
    pub backend: String,
    /// Whether the selected backend is reachable.
    pub reachable: bool,
    /// Backend selected for execution.
    pub selected_backend: String,
    /// Optional diagnostic message.
    pub message: Option<String>,
    /// Stable failure category.
    pub failure_category: Option<String>,
}

impl TargetPreflightReport {
    /// Convert to Python-compatible JSON.
    #[must_use]
    pub fn to_json_value(&self) -> Value {
        let mut data = serde_json::Map::new();
        data.insert("target".to_owned(), Value::from(self.target_name.clone()));
        data.insert("backend".to_owned(), Value::from(self.backend.clone()));
        data.insert("reachable".to_owned(), Value::from(self.reachable));
        data.insert(
            "selected_backend".to_owned(),
            Value::from(self.selected_backend.clone()),
        );
        if let Some(message) = &self.message {
            data.insert("message".to_owned(), Value::from(message.clone()));
        }
        if let Some(category) = &self.failure_category {
            data.insert("failure_category".to_owned(), Value::from(category.clone()));
        }
        Value::Object(data)
    }
}

/// Aggregate submission preflight result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShipPreflightReport {
    /// Git root detected for the working directory.
    pub git_root: Option<PathBuf>,
    /// Expected project root.
    pub expected_root: Option<PathBuf>,
    /// Per-target preflight reports.
    pub targets: Vec<TargetPreflightReport>,
    /// Human-readable warnings.
    pub warnings: Vec<String>,
    /// Deliberately skipped target names.
    pub skipped_targets: Vec<String>,
}

impl ShipPreflightReport {
    /// Convert to Python-compatible JSON.
    #[must_use]
    pub fn to_json_value(&self) -> Value {
        let targets = self
            .targets
            .iter()
            .map(|target| (target.target_name.clone(), target.to_json_value()))
            .collect::<serde_json::Map<_, _>>();
        json!({
            "git_root": self.git_root.as_ref().map(|path| path.display().to_string()),
            "expected_root": self.expected_root.as_ref().map(|path| path.display().to_string()),
            "targets": targets,
            "warnings": self.warnings,
            "skipped_targets": self.skipped_targets,
        })
    }
}

/// Errors returned by `ship --pr` preflight.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShipPreflightError {
    /// The command is being run from a different checkout root than the config.
    RootMismatch {
        /// Root reported by git.
        git_root: PathBuf,
        /// Expected root inferred from `.shipyard`.
        expected_root: PathBuf,
    },
    /// One or more targets cannot be reached.
    BackendUnreachable {
        /// Per-target failures.
        failures: Vec<TargetPreflightFailure>,
        /// Optional daemon-version-skew hypothesis.
        skew_note: Option<String>,
    },
    /// The opt-in host-health gate hard-stopped a saturated self-hosted host.
    HostUnhealthy {
        /// Level label (currently always `critical`).
        level: String,
        /// Human reason from the `host_vitals` signal.
        reason: String,
    },
}

/// Optional bypasses for explicit operator-controlled validation runs.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ShipPreflightOptions {
    /// Skip checkout-root mismatch enforcement.
    pub allow_root_mismatch: bool,
    /// Skip backend reachability failures.
    pub allow_unreachable_targets: bool,
}

impl Display for ShipPreflightError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::RootMismatch {
                git_root,
                expected_root,
            } => write!(
                formatter,
                "Shipyard config belongs to {}, but this command is running in {}",
                expected_root.display(),
                git_root.display()
            ),
            Self::BackendUnreachable {
                failures,
                skew_note,
            } => {
                for (index, failure) in failures.iter().enumerate() {
                    if index > 0 {
                        writeln!(formatter)?;
                    }
                    writeln!(
                        formatter,
                        "Target '{}' ({}) is unreachable.",
                        failure.target_name, failure.backend
                    )?;
                    for line in failure.message.lines() {
                        writeln!(formatter, "  {line}")?;
                    }
                }
                if let Some(note) = skew_note {
                    writeln!(formatter)?;
                    writeln!(formatter, "{note}")?;
                }
                writeln!(formatter)?;
                // Issue #301 (3/3): cite the `--skip-target` escape hatch in
                // the error itself so an agent following CLAUDE.md doesn't
                // need to re-derive it. Names every unreachable target so the
                // exact flag invocation is copy-pasteable.
                let skip_flags = failures
                    .iter()
                    .map(|f| format!("--skip-target {}", f.target_name))
                    .collect::<Vec<_>>()
                    .join(" ");
                writeln!(formatter, "Options:")?;
                writeln!(
                    formatter,
                    "  - Fix the backend (check network, SSH key, hostname), OR"
                )?;
                write!(
                    formatter,
                    "  - Skip the target(s) that don't apply to this change: {skip_flags}"
                )
            }
            Self::HostUnhealthy { level, reason } => write!(
                formatter,
                "Host-health {level} before dispatch: {reason}\n\n\
                 The self-hosted runner is saturated; validating here risks an infra failure \
                 (memory pressure / jetsam / reboot), not a code failure. Options:\n\
                 \x20 - Wait for the host to recover, or ship via GitHub-native auto-merge, OR\n\
                 \x20 - Set host_health.block_on_critical=false to warn instead of block."
            ),
        }
    }
}

impl Error for ShipPreflightError {}

/// Run the pre-execution checks that must pass before durable ship state mutates.
pub fn run_ship_preflight(
    config: &LoadedConfig,
    cwd: &Path,
    state_dir: &Path,
    targets: &[ResolvedTarget],
    dispatcher: &ExecutorDispatcher,
) -> Result<(), ShipPreflightError> {
    run_ship_preflight_with_options(
        config,
        cwd,
        state_dir,
        targets,
        dispatcher,
        ShipPreflightOptions::default(),
    )
}

/// Run preflight checks with explicit operator-controlled bypasses.
pub fn run_ship_preflight_with_options(
    config: &LoadedConfig,
    cwd: &Path,
    state_dir: &Path,
    targets: &[ResolvedTarget],
    dispatcher: &ExecutorDispatcher,
    options: ShipPreflightOptions,
) -> Result<(), ShipPreflightError> {
    collect_ship_preflight_with_options(config, cwd, state_dir, targets, dispatcher, options)
        .map(|_| ())
}

/// Run preflight and return the Python-compatible report on success.
pub fn collect_ship_preflight_with_options(
    config: &LoadedConfig,
    cwd: &Path,
    state_dir: &Path,
    targets: &[ResolvedTarget],
    dispatcher: &ExecutorDispatcher,
    options: ShipPreflightOptions,
) -> Result<ShipPreflightReport, ShipPreflightError> {
    let expected_root = Some(normalize_path(
        &expected_root(config).unwrap_or_else(|| cwd.to_path_buf()),
    ));
    let git_root = git_root_for(cwd);
    let mut warnings = Vec::new();
    if let (Some(git_root), Some(expected_root)) = (&git_root, &expected_root)
        && normalize_path(git_root) != normalize_path(expected_root)
    {
        let message = format!(
            "Git root {} does not match Shipyard project root {}",
            git_root.display(),
            expected_root.display()
        );
        if options.allow_root_mismatch {
            warnings.push(message);
        } else {
            return Err(ShipPreflightError::RootMismatch {
                git_root: git_root.clone(),
                expected_root: expected_root.clone(),
            });
        }
    }

    // Opt-in host-health gate (off by default, fails open). Surfaces a saturated
    // self-hosted host before we spend target probes / mutate ship state.
    match crate::host_health::evaluate(config) {
        crate::host_health::HostHealthOutcome::Ok => {}
        crate::host_health::HostHealthOutcome::Warn(message) => warnings.push(message),
        crate::host_health::HostHealthOutcome::Block { level, reason } => {
            return Err(ShipPreflightError::HostUnhealthy { level, reason });
        }
    }

    let target_reports = targets
        .iter()
        .map(|target| target_report(target, dispatcher))
        .collect::<Vec<_>>();
    let failures = target_reports
        .iter()
        .filter(|target| !target.reachable)
        .map(|target| TargetPreflightFailure {
            target_name: target.target_name.clone(),
            backend: target.backend.clone(),
            message: target
                .message
                .clone()
                .unwrap_or_else(|| "backend did not accept the preflight probe".to_owned()),
            failure_category: target.failure_category.clone(),
        })
        .collect::<Vec<_>>();
    if failures.is_empty() {
        return Ok(ShipPreflightReport {
            git_root,
            expected_root,
            targets: target_reports,
            warnings,
            skipped_targets: Vec::new(),
        });
    }
    if options.allow_unreachable_targets {
        let names = failures
            .iter()
            .map(|failure| failure.target_name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        warnings.push(format!(
            "VALIDATION GAP - the following targets are unreachable and will be SKIPPED, NOT validated: {names}."
        ));
        for failure in &failures {
            warnings.push(format!("  - {}", failure.message));
        }
        if let Some(note) = daemon_skew_note(state_dir) {
            warnings.push(note);
        }
        return Ok(ShipPreflightReport {
            git_root,
            expected_root,
            targets: target_reports,
            warnings,
            skipped_targets: Vec::new(),
        });
    }
    Err(ShipPreflightError::BackendUnreachable {
        failures,
        skew_note: daemon_skew_note(state_dir),
    })
}

fn target_report(
    target: &ResolvedTarget,
    dispatcher: &ExecutorDispatcher,
) -> TargetPreflightReport {
    let diagnostic = dispatcher.diagnose(target);
    TargetPreflightReport {
        target_name: target.name.clone(),
        backend: target.backend_name.clone(),
        reachable: diagnostic.reachable,
        selected_backend: target.backend_name.clone(),
        message: diagnostic.message,
        failure_category: diagnostic.category,
    }
}

fn expected_root(config: &LoadedConfig) -> Option<PathBuf> {
    config.project_dir.as_ref()?.parent().map(Path::to_path_buf)
}

fn git_root_for(cwd: &Path) -> Option<PathBuf> {
    let output = crate::supervised::git_supervised()
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()
        .ok()?;
    output.status.success().then(|| {
        normalize_path(&PathBuf::from(
            String::from_utf8_lossy(&output.stdout).trim(),
        ))
    })
}

fn normalize_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn daemon_skew_note(state_dir: &Path) -> Option<String> {
    daemon_skew_note_from_relation(read_daemon_version_relation(
        state_dir,
        &format!("v{}", env!("CARGO_PKG_VERSION")),
    )?)
}

fn daemon_skew_note_from_relation(relation: DaemonVersionRelation) -> Option<String> {
    match relation {
        DaemonVersionRelation::Mismatch {
            daemon_version,
            cli_version,
        } => Some(format!(
            "Note: the running daemon is v{daemon_version} but this CLI is {cli_version}. If this failure looks surprising, run `shipyard daemon refresh` or `shipyard daemon stop` so the next launch uses the current binary."
        )),
        DaemonVersionRelation::UnknownDaemonVersion { cli_version } => Some(format!(
            "Note: the running daemon predates version reporting, while this CLI is {cli_version}. If this failure looks surprising, run `shipyard daemon refresh` or `shipyard daemon stop` so the next launch uses the current binary."
        )),
        DaemonVersionRelation::Match { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::io::{BufRead, BufReader, Write};
    #[cfg(unix)]
    use std::os::unix::net::UnixListener;
    #[cfg(unix)]
    use std::thread;

    #[cfg(unix)]
    use serde_json::json;
    use toml::Table;

    use super::{
        ShipPreflightError, ShipPreflightOptions, TargetPreflightFailure,
        collect_ship_preflight_with_options, daemon_skew_note_from_relation, run_ship_preflight,
    };
    use crate::config::{LoadedConfig, LocalOverlaySource};
    use crate::daemon_version::DaemonVersionRelation;
    use crate::executor::dispatch::{ExecutorDispatcher, resolve_targets_from_table};
    use crate::job::ValidationMode;

    /// Issue #301 (3/3): the unreachable-target Display surface must cite the
    /// `--skip-target` escape hatch with the exact target names so an agent
    /// can copy-paste the recovery flag.
    #[test]
    fn backend_unreachable_display_names_skip_target_flag() {
        let err = ShipPreflightError::BackendUnreachable {
            failures: vec![
                TargetPreflightFailure {
                    target_name: "ubuntu".to_owned(),
                    backend: "ssh".to_owned(),
                    message: "ssh: connect to host 192.168.64.10 port 22: Operation timed out"
                        .to_owned(),
                    failure_category: Some("ssh_unreachable".to_owned()),
                },
                TargetPreflightFailure {
                    target_name: "windows".to_owned(),
                    backend: "ssh".to_owned(),
                    message: "ssh: connect to host 192.168.64.20 port 22: Operation timed out"
                        .to_owned(),
                    failure_category: Some("ssh_unreachable".to_owned()),
                },
            ],
            skew_note: None,
        };
        let rendered = format!("{err}");
        assert!(
            rendered.contains("--skip-target ubuntu"),
            "must name the ubuntu skip flag verbatim; got:\n{rendered}"
        );
        assert!(
            rendered.contains("--skip-target windows"),
            "must name the windows skip flag verbatim; got:\n{rendered}"
        );
        assert!(
            rendered.contains("Skip the target(s) that don't apply"),
            "must explain the choice; got:\n{rendered}"
        );
        assert!(
            rendered.contains("Fix the backend"),
            "must still offer the original 'fix the backend' option; got:\n{rendered}"
        );
    }

    fn config(data: &str, project_dir: Option<std::path::PathBuf>) -> LoadedConfig {
        LoadedConfig {
            data: data.parse::<Table>().expect("toml"),
            global_dir: std::path::PathBuf::from("/tmp/global"),
            project_dir,
            local_dir: None,
            local_overlay_source: LocalOverlaySource::None,
        }
    }

    #[test]
    fn local_targets_pass_preflight_without_git_root() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config = config(
            r#"
            [validation.default]
            command = "true"

            [targets.mac]
            backend = "local"
            platform = "macos-arm64"
            "#,
            None,
        );
        let targets =
            resolve_targets_from_table(&config.data, ValidationMode::Full).expect("targets");

        run_ship_preflight(
            &config,
            temp.path(),
            temp.path(),
            &targets,
            &ExecutorDispatcher::new(None),
        )
        .expect("preflight");
    }

    /// Write a synthetic `host_vitals` file and return a TOML-safe path string
    /// (forward slashes so a Windows tempdir path stays a valid TOML string and
    /// a readable path). Proves the gate is wired through real preflight.
    fn write_vitals(dir: &std::path::Path, code: i64) -> String {
        let path = dir.join("host_vitals.json");
        std::fs::write(
            &path,
            format!(r#"{{"code":{code},"reason":"synthetic {code}"}}"#),
        )
        .expect("write vitals");
        path.to_string_lossy().replace('\\', "/")
    }

    fn host_health_config(vitals: &str, gate: bool, block_on_critical: bool) -> LoadedConfig {
        config(
            &format!(
                r#"
                [validation.default]
                command = "true"

                [targets.mac]
                backend = "local"
                platform = "macos-arm64"

                [host_health]
                gate = {gate}
                block_on_critical = {block_on_critical}
                file = "{vitals}"
                "#
            ),
            None,
        )
    }

    #[test]
    fn host_health_gate_blocks_on_critical_when_opted_in() {
        let temp = tempfile::tempdir().expect("tempdir");
        let vitals = write_vitals(temp.path(), 20);
        let config = host_health_config(&vitals, true, true);
        let targets =
            resolve_targets_from_table(&config.data, ValidationMode::Full).expect("targets");

        let error = run_ship_preflight(
            &config,
            temp.path(),
            temp.path(),
            &targets,
            &ExecutorDispatcher::new(None),
        )
        .expect_err("critical host must block when block_on_critical is set");

        assert!(matches!(
            error,
            ShipPreflightError::HostUnhealthy { ref level, .. } if level == "critical"
        ));
        assert!(error.to_string().contains("synthetic 20"));
    }

    #[test]
    fn host_health_gate_warns_on_critical_by_default() {
        let temp = tempfile::tempdir().expect("tempdir");
        let vitals = write_vitals(temp.path(), 20);
        // gate on, block_on_critical OFF → surface a warning, do not block.
        let config = host_health_config(&vitals, true, false);
        let targets =
            resolve_targets_from_table(&config.data, ValidationMode::Full).expect("targets");

        let report = collect_ship_preflight_with_options(
            &config,
            temp.path(),
            temp.path(),
            &targets,
            &ExecutorDispatcher::new(None),
            ShipPreflightOptions::default(),
        )
        .expect("default critical must warn, not block");

        assert!(
            report.warnings.iter().any(|w| w.contains("CRITICAL")),
            "expected a host-health CRITICAL warning; got {:?}",
            report.warnings
        );
    }

    #[test]
    fn host_health_gate_off_ignores_a_critical_signal() {
        let temp = tempfile::tempdir().expect("tempdir");
        let vitals = write_vitals(temp.path(), 20);
        // gate OFF (default) → the critical signal is never read.
        let config = host_health_config(&vitals, false, true);
        let targets =
            resolve_targets_from_table(&config.data, ValidationMode::Full).expect("targets");

        let report = collect_ship_preflight_with_options(
            &config,
            temp.path(),
            temp.path(),
            &targets,
            &ExecutorDispatcher::new(None),
            ShipPreflightOptions::default(),
        )
        .expect("gate off must pass");

        assert!(
            !report.warnings.iter().any(|w| w.contains("Host-health")),
            "gate off must not surface any host-health warning; got {:?}",
            report.warnings
        );
    }

    #[test]
    fn missing_ssh_host_returns_backend_unreachable() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config = config(
            r#"
            [validation.default]
            command = "make test"

            [targets.linux]
            backend = "ssh"
            platform = "linux-x64"
            repo_path = "~/repo"
            "#,
            None,
        );
        let targets =
            resolve_targets_from_table(&config.data, ValidationMode::Full).expect("targets");

        let error = run_ship_preflight(
            &config,
            temp.path(),
            temp.path(),
            &targets,
            &ExecutorDispatcher::new(None),
        )
        .expect_err("unreachable");

        assert!(matches!(
            error,
            ShipPreflightError::BackendUnreachable { ref failures, .. }
                if failures[0].target_name == "linux"
                    && failures[0].failure_category.as_deref() == Some("configuration")
        ));
        assert!(error.to_string().contains("target has no host configured"));
    }

    #[test]
    fn daemon_skew_note_only_renders_for_mismatch_or_unknown() {
        assert!(
            daemon_skew_note_from_relation(DaemonVersionRelation::Match {
                daemon_version: "0.1.0".to_owned(),
                cli_version: "v0.1.0".to_owned(),
            })
            .is_none()
        );
        assert!(
            daemon_skew_note_from_relation(DaemonVersionRelation::Mismatch {
                daemon_version: "0.0.9".to_owned(),
                cli_version: "v0.1.0".to_owned(),
            })
            .expect("note")
            .contains("running daemon is v0.0.9 but this CLI is v0.1.0")
        );
        assert!(
            daemon_skew_note_from_relation(DaemonVersionRelation::UnknownDaemonVersion {
                cli_version: "v0.1.0".to_owned(),
            })
            .expect("note")
            .contains("predates version reporting")
        );
    }

    #[cfg(unix)]
    #[test]
    fn backend_unreachable_reads_running_daemon_skew_note() {
        let temp = tempfile::tempdir().expect("tempdir");
        let state_dir = temp.path().join("state");
        let daemon_dir = state_dir.join("daemon");
        std::fs::create_dir_all(&daemon_dir).expect("daemon dir");
        let socket_path = daemon_dir.join("daemon.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind daemon socket");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut request = String::new();
            reader.read_line(&mut request).expect("request");
            serde_json::to_writer(
                &mut stream,
                &json!({
                    "type": "hello",
                    "shipyard_version": "0.0.9",
                }),
            )
            .expect("hello");
            stream.write_all(b"\n").expect("hello newline");
            serde_json::to_writer(
                &mut stream,
                &json!({
                    "type": "status",
                    "shipyard_version": "0.0.9",
                }),
            )
            .expect("status");
            stream.write_all(b"\n").expect("status newline");
        });
        let config = config(
            r#"
            [validation.default]
            command = "make test"

            [targets.linux]
            backend = "ssh"
            platform = "linux-x64"
            repo_path = "~/repo"
            "#,
            None,
        );
        let targets =
            resolve_targets_from_table(&config.data, ValidationMode::Full).expect("targets");

        let error = run_ship_preflight(
            &config,
            temp.path(),
            &state_dir,
            &targets,
            &ExecutorDispatcher::new(None),
        )
        .expect_err("unreachable");

        let message = error.to_string();
        assert!(message.contains("running daemon is v0.0.9"));
        assert!(message.contains(&format!("this CLI is v{}", env!("CARGO_PKG_VERSION"))));
        assert!(message.contains("shipyard daemon refresh"));
        server.join().expect("daemon socket thread");
    }
}
