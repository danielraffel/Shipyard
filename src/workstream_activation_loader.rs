//! Trusted daemon activation loader for workstream continuation.
//!
//! The production constructor accepts no path or mode overrides. Each call to
//! [`WorkstreamActivationLoader::revalidate_for_tick`] re-derives the canonical
//! Shipyard roots and reloads every activation input before a future daemon
//! tick may use the returned policy. This module performs no database, queue,
//! provider, process, or daemon-loop action.
#![allow(dead_code)] // Consumed by the later daemon-loop integration slice.

use std::fmt::{Display, Formatter};
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use crate::config::{LoadedConfig, LocalOverlaySource};
use crate::identity::RuntimeMode;
use crate::paths::RuntimePaths;
use crate::provider_wrapper::provider_wrapper_execution_supported;
use crate::workstream_continuation_config::{
    WorkstreamContinuationConfig, trusted_workstream_continuation_config,
};

const MAX_MACHINE_TAG_BYTES: u64 = 128;
const MAX_MACHINE_CONFIG_BYTES: u64 = 1024 * 1024;

/// Redacted activation refusal suitable for status surfaces and logs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WorkstreamActivationRefusal {
    NonProductionRuntime,
    UnsafeProductionRoots,
    UnsafeMachineIdentity,
    InvalidMachinePolicy,
    UnsupportedProviderPlatform,
    ActivationDrift,
}

impl WorkstreamActivationRefusal {
    /// Stable, non-sensitive reason code.
    pub(crate) const fn code(self) -> &'static str {
        match self {
            Self::NonProductionRuntime => "non_production_runtime",
            Self::UnsafeProductionRoots => "unsafe_production_roots",
            Self::UnsafeMachineIdentity => "unsafe_machine_identity",
            Self::InvalidMachinePolicy => "invalid_machine_policy",
            Self::UnsupportedProviderPlatform => "unsupported_provider_platform",
            Self::ActivationDrift => "activation_drift",
        }
    }
}

impl Display for WorkstreamActivationRefusal {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.code())
    }
}

/// Fully revalidated policy for exactly one future daemon tick.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReadyWorkstreamActivation {
    pub(crate) machine_tag: String,
    pub(crate) config: WorkstreamContinuationConfig,
}

/// Activation state returned by the single daemon-facing loader API.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum WorkstreamActivationState {
    Disabled,
    Ready(ReadyWorkstreamActivation),
    Refused(WorkstreamActivationRefusal),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ActivationBaseline {
    Disabled,
    Ready(ReadyWorkstreamActivation),
}

#[derive(Clone, Debug)]
enum RootAuthority {
    Production,
    #[cfg(test)]
    SimulatedProduction {
        platform: crate::platform::Platform,
        home: PathBuf,
    },
    #[cfg(test)]
    Inspection {
        mode: RuntimeMode,
        paths: RuntimePaths,
    },
}

/// Sticky, fail-closed activation authority owned by the future daemon.
#[derive(Clone, Debug)]
pub(crate) struct WorkstreamActivationLoader {
    root_authority: RootAuthority,
    initial_roots: RuntimePaths,
    baseline: Option<ActivationBaseline>,
    refused: Option<WorkstreamActivationRefusal>,
    #[cfg(test)]
    platform_support_override: Option<bool>,
}

impl WorkstreamActivationLoader {
    /// Construct the only production-capable loader.
    ///
    /// CLI, project, sandbox, and test path overrides cannot enter this API.
    #[must_use]
    pub(crate) fn production() -> Self {
        let initial_roots = RuntimePaths::current(RuntimeMode::Shipyard);
        Self {
            root_authority: RootAuthority::Production,
            initial_roots,
            baseline: None,
            refused: None,
            #[cfg(test)]
            platform_support_override: None,
        }
    }

    /// Reload and fence every activation input immediately before one tick.
    ///
    /// The first result establishes a daemon-lifetime baseline. Any later
    /// policy, machine, root, wrapper, or repository drift becomes a sticky
    /// refusal and requires a daemon restart after operator review.
    pub(crate) fn revalidate_for_tick(&mut self) -> WorkstreamActivationState {
        if let Some(reason) = self.refused {
            return WorkstreamActivationState::Refused(reason);
        }
        let current = match self.load_current() {
            Ok(current) => current,
            Err(reason) => return self.latch_refusal(reason),
        };
        let current_baseline = match &current {
            WorkstreamActivationState::Disabled => ActivationBaseline::Disabled,
            WorkstreamActivationState::Ready(ready) => ActivationBaseline::Ready(ready.clone()),
            WorkstreamActivationState::Refused(reason) => return self.latch_refusal(*reason),
        };
        match &self.baseline {
            None => {
                self.baseline = Some(current_baseline);
                current
            }
            Some(baseline) if baseline == &current_baseline => current,
            Some(_) => self.latch_refusal(WorkstreamActivationRefusal::ActivationDrift),
        }
    }

    fn load_current(&self) -> Result<WorkstreamActivationState, WorkstreamActivationRefusal> {
        let roots = self.derive_roots();
        if roots != self.initial_roots {
            return Err(WorkstreamActivationRefusal::ActivationDrift);
        }
        validate_root_contract(&roots)?;

        let config_before = read_optional_pinned_regular(
            &roots.global_dir.join("config.toml"),
            MAX_MACHINE_CONFIG_BYTES,
        )?;
        let loaded = LoadedConfig::load(
            Some(roots.global_dir.clone()),
            None,
            None,
            LocalOverlaySource::None,
        )
        .map_err(|_| WorkstreamActivationRefusal::InvalidMachinePolicy)?;
        if loaded.project_dir.is_some()
            || loaded.local_dir.is_some()
            || loaded.global_dir != roots.global_dir
        {
            return Err(WorkstreamActivationRefusal::InvalidMachinePolicy);
        }

        // An absent policy is the normal default-off state and does not require
        // a machine tag. A concurrently added policy is observed next tick and
        // then refuses as drift from Disabled.
        if loaded.get("workstream_continuation").is_none() {
            let config_after = read_optional_pinned_regular(
                &roots.global_dir.join("config.toml"),
                MAX_MACHINE_CONFIG_BYTES,
            )?;
            if config_before != config_after || roots != self.derive_roots() {
                return Err(WorkstreamActivationRefusal::ActivationDrift);
            }
            return Ok(WorkstreamActivationState::Disabled);
        }

        // Explicit default-off policy also needs no machine identity. The
        // parser returns None before consulting the syntactically valid probe
        // tag; enabled and malformed policies continue to the real pinned tag.
        if trusted_workstream_continuation_config(&loaded, "activation-probe")
            .is_ok_and(|policy| policy.is_none())
        {
            let config_after = read_optional_pinned_regular(
                &roots.global_dir.join("config.toml"),
                MAX_MACHINE_CONFIG_BYTES,
            )?;
            if config_before != config_after || roots != self.derive_roots() {
                return Err(WorkstreamActivationRefusal::ActivationDrift);
            }
            return Ok(WorkstreamActivationState::Disabled);
        }

        let machine_tag = read_machine_tag(&roots.state_dir)?;
        let policy = trusted_workstream_continuation_config(&loaded, &machine_tag)
            .map_err(|_| WorkstreamActivationRefusal::InvalidMachinePolicy)?;
        let machine_tag_after = read_machine_tag(&roots.state_dir)?;
        let config_after = read_optional_pinned_regular(
            &roots.global_dir.join("config.toml"),
            MAX_MACHINE_CONFIG_BYTES,
        )?;
        if config_before != config_after
            || machine_tag != machine_tag_after
            || roots != self.derive_roots()
        {
            return Err(WorkstreamActivationRefusal::ActivationDrift);
        }
        let Some(config) = policy else {
            return Ok(WorkstreamActivationState::Disabled);
        };
        if !self.is_production_authority() {
            return Err(WorkstreamActivationRefusal::NonProductionRuntime);
        }
        if !self.provider_platform_supported() {
            return Err(WorkstreamActivationRefusal::UnsupportedProviderPlatform);
        }
        Ok(WorkstreamActivationState::Ready(
            ReadyWorkstreamActivation {
                machine_tag,
                config,
            },
        ))
    }

    fn derive_roots(&self) -> RuntimePaths {
        match &self.root_authority {
            RootAuthority::Production => RuntimePaths::current(RuntimeMode::Shipyard),
            #[cfg(test)]
            RootAuthority::SimulatedProduction { platform, home } => {
                RuntimePaths::for_platform(*platform, home, RuntimeMode::Shipyard)
            }
            #[cfg(test)]
            RootAuthority::Inspection { paths, .. } => paths.clone(),
        }
    }

    fn is_production_authority(&self) -> bool {
        match &self.root_authority {
            RootAuthority::Production => true,
            #[cfg(test)]
            RootAuthority::SimulatedProduction { .. } => true,
            #[cfg(test)]
            RootAuthority::Inspection { mode, .. } => {
                debug_assert!(matches!(
                    mode,
                    RuntimeMode::Isolated | RuntimeMode::Shipyard
                ));
                false
            }
        }
    }

    #[allow(clippy::unused_self)]
    fn provider_platform_supported(&self) -> bool {
        #[cfg(test)]
        if let Some(supported) = self.platform_support_override {
            return supported;
        }
        provider_wrapper_execution_supported()
    }

    fn latch_refusal(&mut self, reason: WorkstreamActivationRefusal) -> WorkstreamActivationState {
        let reason = if self.baseline.is_some() {
            WorkstreamActivationRefusal::ActivationDrift
        } else {
            reason
        };
        self.refused = Some(reason);
        WorkstreamActivationState::Refused(reason)
    }

    #[cfg(test)]
    fn simulated_production(platform: crate::platform::Platform, home: PathBuf) -> Self {
        let initial_roots = RuntimePaths::for_platform(platform, &home, RuntimeMode::Shipyard);
        Self {
            root_authority: RootAuthority::SimulatedProduction { platform, home },
            initial_roots,
            baseline: None,
            refused: None,
            platform_support_override: Some(true),
        }
    }

    #[cfg(test)]
    fn inspection(mode: RuntimeMode, paths: RuntimePaths) -> Self {
        Self {
            root_authority: RootAuthority::Inspection {
                mode,
                paths: paths.clone(),
            },
            initial_roots: paths,
            baseline: None,
            refused: None,
            platform_support_override: Some(true),
        }
    }
}

fn validate_root_contract(roots: &RuntimePaths) -> Result<(), WorkstreamActivationRefusal> {
    if roots.mode != RuntimeMode::Shipyard.as_str()
        || !roots.global_dir.is_absolute()
        || !roots.state_dir.is_absolute()
        || has_dot_component(&roots.global_dir)
        || has_dot_component(&roots.state_dir)
        || has_symlink_ancestor(&roots.global_dir)
        || has_symlink_ancestor(&roots.state_dir)
    {
        return Err(WorkstreamActivationRefusal::UnsafeProductionRoots);
    }
    Ok(())
}

fn has_dot_component(path: &Path) -> bool {
    path.components()
        .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
}

fn has_symlink_ancestor(path: &Path) -> bool {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => return true,
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(_) => return true,
        }
    }
    false
}

fn read_machine_tag(state_dir: &Path) -> Result<String, WorkstreamActivationRefusal> {
    let bytes = read_pinned_regular(
        &state_dir.join("machine-tag"),
        MAX_MACHINE_TAG_BYTES,
        WorkstreamActivationRefusal::UnsafeMachineIdentity,
    )?;
    let raw = std::str::from_utf8(&bytes)
        .map_err(|_| WorkstreamActivationRefusal::UnsafeMachineIdentity)?;
    let tag = raw.trim();
    if tag.is_empty()
        || raw.trim_matches(['\r', '\n']) != tag
        || raw.matches('\n').count() > 1
        || raw.contains('\r')
        || crate::runner_provision::validate_machine_tag(tag).is_err()
    {
        return Err(WorkstreamActivationRefusal::UnsafeMachineIdentity);
    }
    Ok(tag.to_owned())
}

fn read_optional_pinned_regular(
    path: &Path,
    limit: u64,
) -> Result<Option<Vec<u8>>, WorkstreamActivationRefusal> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(WorkstreamActivationRefusal::InvalidMachinePolicy)
        }
        Ok(_) => read_pinned_regular(
            path,
            limit,
            WorkstreamActivationRefusal::InvalidMachinePolicy,
        )
        .map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(WorkstreamActivationRefusal::InvalidMachinePolicy),
    }
}

fn read_pinned_regular(
    path: &Path,
    limit: u64,
    refusal: WorkstreamActivationRefusal,
) -> Result<Vec<u8>, WorkstreamActivationRefusal> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        options.custom_flags(0x0020_0000);
    }
    let file = options.open(path).map_err(|_| refusal)?;
    read_bounded_regular_file(file, limit, refusal)
}

fn read_bounded_regular_file(
    file: File,
    limit: u64,
    refusal: WorkstreamActivationRefusal,
) -> Result<Vec<u8>, WorkstreamActivationRefusal> {
    let before = file.metadata().map_err(|_| refusal)?;
    if !before.is_file() || before.len() > limit {
        return Err(refusal);
    }
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| refusal)?;
    if bytes.len() as u64 > limit || bytes.len() as u64 != before.len() {
        return Err(refusal);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::Platform;

    const DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn policy(machine: &str, repositories: &str, wrapper_digest: &str) -> String {
        format!(
            r#"[workstream_continuation]
activation_enabled = true
dispatch_enabled = true
origin_machine = "{machine}"
repositories = {repositories}

[workstream_continuation.provider_wrapper]
executable_path = "/opt/shipyard/bin/workstream-provider"
executable_sha256 = "{wrapper_digest}"
provider_id = "codex"
adapter_id = "codex-wrapper-v1"
deadline_seconds = 30
max_stdout_bytes = 65536
max_stderr_bytes = 65536
"#
        )
    }

    struct Fixture {
        _temp: tempfile::TempDir,
        home: PathBuf,
        paths: RuntimePaths,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().expect("temp");
            let home = fs::canonicalize(temp.path())
                .expect("canonical temp")
                .join("home");
            fs::create_dir_all(&home).expect("home");
            let paths = RuntimePaths::for_platform(Platform::MacOs, &home, RuntimeMode::Shipyard);
            fs::create_dir_all(&paths.global_dir).expect("global");
            fs::create_dir_all(&paths.state_dir).expect("state");
            Self {
                _temp: temp,
                home,
                paths,
            }
        }

        fn write_machine(&self, machine: &str) {
            fs::write(
                self.paths.state_dir.join("machine-tag"),
                format!("{machine}\n"),
            )
            .expect("machine tag");
        }

        fn write_policy(&self, contents: &str) {
            fs::write(self.paths.global_dir.join("config.toml"), contents).expect("policy");
        }

        fn production_loader(&self) -> WorkstreamActivationLoader {
            WorkstreamActivationLoader::simulated_production(Platform::MacOs, self.home.clone())
        }
    }

    #[allow(clippy::needless_pass_by_value)]
    fn assert_refused(state: WorkstreamActivationState, expected: WorkstreamActivationRefusal) {
        assert_eq!(state, WorkstreamActivationState::Refused(expected));
        let rendered = expected.to_string();
        assert_eq!(rendered, expected.code());
        assert!(!rendered.contains('/') && !rendered.contains("m5"));
    }

    #[test]
    fn production_shape_loads_ready_and_revalidates_without_drift() {
        let fixture = Fixture::new();
        fixture.write_machine("m5");
        fixture.write_policy(&policy("m5", r#"["generous-corp/shipyard"]"#, DIGEST));
        let mut loader = fixture.production_loader();
        let first = loader.revalidate_for_tick();
        let second = loader.revalidate_for_tick();
        assert_eq!(first, second);
        let WorkstreamActivationState::Ready(ready) = first else {
            panic!("expected ready")
        };
        assert_eq!(ready.machine_tag, "m5");
        assert_eq!(ready.config.repositories, ["generous-corp/shipyard"]);
        assert_eq!(ready.config.provider_wrapper.executable_sha256, DIGEST);
    }

    #[test]
    fn absent_global_policy_is_disabled_and_project_overlay_is_ignored() {
        let fixture = Fixture::new();
        let project = fixture.home.join("repo/.shipyard");
        fs::create_dir_all(&project).expect("project");
        fs::write(
            project.join("config.toml"),
            policy("m5", r#"["generous-corp/shipyard"]"#, DIGEST),
        )
        .expect("project policy");
        let mut loader = fixture.production_loader();
        assert_eq!(
            loader.revalidate_for_tick(),
            WorkstreamActivationState::Disabled
        );
        fixture.write_machine("m5");
        fixture.write_policy(&policy("m5", r#"["generous-corp/shipyard"]"#, DIGEST));
        assert_refused(
            loader.revalidate_for_tick(),
            WorkstreamActivationRefusal::ActivationDrift,
        );
    }

    #[test]
    fn explicit_default_off_policy_does_not_require_machine_identity() {
        let fixture = Fixture::new();
        fixture.write_policy(
            "[workstream_continuation]\nactivation_enabled = false\ndispatch_enabled = false\n",
        );
        let mut loader = fixture.production_loader();
        assert_eq!(
            loader.revalidate_for_tick(),
            WorkstreamActivationState::Disabled
        );
    }

    #[test]
    fn machine_symlink_and_machine_drift_refuse_with_redacted_reasons() {
        let fixture = Fixture::new();
        fixture.write_policy(&policy("m5", r#"["generous-corp/shipyard"]"#, DIGEST));
        let external = fixture.home.join("external-tag");
        fs::write(&external, "m5\n").expect("external");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&external, fixture.paths.state_dir.join("machine-tag"))
            .expect("symlink");
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&external, fixture.paths.state_dir.join("machine-tag"))
            .expect("symlink");
        let mut loader = fixture.production_loader();
        assert_refused(
            loader.revalidate_for_tick(),
            WorkstreamActivationRefusal::UnsafeMachineIdentity,
        );

        fs::remove_file(fixture.paths.state_dir.join("machine-tag")).expect("remove link");
        fixture.write_machine("m5");
        let mut loader = fixture.production_loader();
        assert!(matches!(
            loader.revalidate_for_tick(),
            WorkstreamActivationState::Ready(_)
        ));
        fixture.write_machine("m3");
        assert_refused(
            loader.revalidate_for_tick(),
            WorkstreamActivationRefusal::ActivationDrift,
        );
    }

    #[test]
    fn oversized_machine_identity_refuses_before_policy_activation() {
        let fixture = Fixture::new();
        fixture.write_policy(&policy("m5", r#"["generous-corp/shipyard"]"#, DIGEST));
        fs::write(
            fixture.paths.state_dir.join("machine-tag"),
            "m".repeat(
                usize::try_from(MAX_MACHINE_TAG_BYTES + 1).expect("machine tag bound fits usize"),
            ),
        )
        .expect("oversized tag");
        let mut loader = fixture.production_loader();
        assert_refused(
            loader.revalidate_for_tick(),
            WorkstreamActivationRefusal::UnsafeMachineIdentity,
        );
    }

    #[test]
    fn wrapper_identity_drift_refuses_and_stays_sticky() {
        let fixture = Fixture::new();
        fixture.write_machine("m5");
        fixture.write_policy(&policy("m5", r#"["generous-corp/shipyard"]"#, DIGEST));
        let mut loader = fixture.production_loader();
        assert!(matches!(
            loader.revalidate_for_tick(),
            WorkstreamActivationState::Ready(_)
        ));
        fixture.write_policy(&policy(
            "m5",
            r#"["generous-corp/shipyard"]"#,
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        ));
        assert_refused(
            loader.revalidate_for_tick(),
            WorkstreamActivationRefusal::ActivationDrift,
        );
        assert_refused(
            loader.revalidate_for_tick(),
            WorkstreamActivationRefusal::ActivationDrift,
        );
    }

    #[test]
    fn invalid_repository_refuses_before_ready() {
        let fixture = Fixture::new();
        fixture.write_machine("m5");
        fixture.write_policy(&policy("m5", r#"["Generous-Corp/Shipyard"]"#, DIGEST));
        let mut loader = fixture.production_loader();
        assert_refused(
            loader.revalidate_for_tick(),
            WorkstreamActivationRefusal::InvalidMachinePolicy,
        );
    }

    #[test]
    fn unsupported_provider_platform_refuses() {
        let fixture = Fixture::new();
        fixture.write_machine("m5");
        fixture.write_policy(&policy("m5", r#"["generous-corp/shipyard"]"#, DIGEST));
        let mut loader = fixture.production_loader();
        loader.platform_support_override = Some(false);
        assert_refused(
            loader.revalidate_for_tick(),
            WorkstreamActivationRefusal::UnsupportedProviderPlatform,
        );
    }

    #[test]
    fn sandbox_and_path_override_can_parse_but_never_become_ready() {
        let fixture = Fixture::new();
        fixture.write_machine("m5");
        fixture.write_policy(&policy("m5", r#"["generous-corp/shipyard"]"#, DIGEST));
        for mode in [RuntimeMode::Isolated, RuntimeMode::Shipyard] {
            let mut loader = WorkstreamActivationLoader::inspection(mode, fixture.paths.clone());
            assert_refused(
                loader.revalidate_for_tick(),
                WorkstreamActivationRefusal::NonProductionRuntime,
            );
        }
    }

    #[test]
    fn symlinked_root_and_noncanonical_path_refuse() {
        let fixture = Fixture::new();
        let real = fixture.home.join("real-global");
        fs::create_dir_all(&real).expect("real");
        let linked = fixture.home.join("linked-global");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &linked).expect("root symlink");
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&real, &linked).expect("root symlink");
        let mut paths = fixture.paths.clone();
        paths.global_dir = linked;
        let mut loader = WorkstreamActivationLoader::inspection(RuntimeMode::Shipyard, paths);
        assert_refused(
            loader.revalidate_for_tick(),
            WorkstreamActivationRefusal::UnsafeProductionRoots,
        );

        let mut paths = fixture.paths.clone();
        paths.state_dir = PathBuf::from("relative/state");
        let mut loader = WorkstreamActivationLoader::inspection(RuntimeMode::Shipyard, paths);
        assert_refused(
            loader.revalidate_for_tick(),
            WorkstreamActivationRefusal::UnsafeProductionRoots,
        );
    }

    #[test]
    fn config_symlink_refuses_instead_of_following() {
        let fixture = Fixture::new();
        fixture.write_machine("m5");
        let external = fixture.home.join("external-config.toml");
        fs::write(
            &external,
            policy("m5", r#"["generous-corp/shipyard"]"#, DIGEST),
        )
        .expect("external policy");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&external, fixture.paths.global_dir.join("config.toml"))
            .expect("config symlink");
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&external, fixture.paths.global_dir.join("config.toml"))
            .expect("config symlink");
        let mut loader = fixture.production_loader();
        assert_refused(
            loader.revalidate_for_tick(),
            WorkstreamActivationRefusal::InvalidMachinePolicy,
        );
    }

    #[test]
    fn production_constructor_ignores_cli_path_concepts() {
        let loader = WorkstreamActivationLoader::production();
        assert_eq!(
            loader.initial_roots,
            RuntimePaths::current(RuntimeMode::Shipyard)
        );
        assert!(matches!(loader.root_authority, RootAuthority::Production));
    }
}
