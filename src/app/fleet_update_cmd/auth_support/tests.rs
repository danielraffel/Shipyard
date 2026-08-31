use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use sha2::{Digest, Sha256};

use super::*;
use crate::app::fleet_update_cmd::test_release_authority;

const NEW_WRAPPER: &[u8] =
    b"#!/bin/bash\n# Shipyard-Auth-Generation-Contract: auth-selector-v1\nexit 0\n";

fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

enum RefreshBehavior {
    Success,
    Fail,
    SignalTerm,
    SpawnDetached,
    ReplaceLegacyPid,
    ObservePublishedLock(PathBuf),
    Touch(PathBuf),
}

enum LockPublishBehavior {
    Success,
    CrashBeforePublish,
    RaceWithLegacy,
    RaceThenCrashAfterMove,
}

impl LockPublishBehavior {
    fn shell_prefix(self, state: &Path) -> String {
        let publish_command = r#"/bin/mv -n "$auth_lock_staging" "$auth_state_dir/""#;
        let trap_condition = format!("[ \"$BASH_COMMAND\" = {} ]", shlex_quote(publish_command));
        match self {
            Self::Success => String::new(),
            Self::CrashBeforePublish => {
                let trap_body = format!("if {trap_condition}; then trap - DEBUG; exit 90; fi");
                format!("trap {} DEBUG\n", shlex_quote(&trap_body))
            }
            behavior @ (Self::RaceWithLegacy | Self::RaceThenCrashAfterMove) => {
                let lock = state.join("fleet-auth-support.lock");
                let crash = matches!(behavior, Self::RaceThenCrashAfterMove);
                let after_race = if crash {
                    format!("{publish_command}; exit 91")
                } else {
                    ":".to_owned()
                };
                let trap_body = format!(
                    "if {trap_condition}; then trap - DEBUG; /bin/mkdir \"$test_auth_lock\"; /usr/bin/printf '%s\\n' \"$test_foreign_pid\" > \"$test_auth_lock/pid\"; /bin/chmod 600 \"$test_auth_lock/pid\"; {after_race}; fi"
                );
                format!(
                    "test_auth_lock={}\ntest_foreign_pid={}\ntrap {} DEBUG\n",
                    shlex_quote(&lock.display().to_string()),
                    std::process::id(),
                    shlex_quote(&trap_body)
                )
            }
        }
    }
}

struct RunOptions {
    fail_after_helper: bool,
    target: &'static str,
    resolver_succeeds: bool,
    refresh_prefix: &'static str,
    refresh: RefreshBehavior,
    lock_publish: LockPublishBehavior,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            fail_after_helper: false,
            target: "v0.134.0",
            resolver_succeeds: true,
            refresh_prefix: "",
            refresh: RefreshBehavior::Success,
            lock_publish: LockPublishBehavior::Success,
        }
    }
}

struct Fixture {
    root: tempfile::TempDir,
    helper: PathBuf,
    wrapper: PathBuf,
    helper_source: PathBuf,
    wrapper_source: PathBuf,
    authority: ReleaseAuthority,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("root");
        let bin = root.path().join(".local/bin");
        let helper_dir = root.path().join(".config/shipyard/bin");
        let state = root.path().join("Library/Application Support/shipyard");
        std::fs::create_dir_all(&bin).expect("bin");
        std::fs::create_dir_all(&helper_dir).expect("helper dir");
        std::fs::create_dir_all(&state).expect("state");
        let helper = helper_dir.join("shipyard-github-app-token");
        let wrapper = bin.join("ghapp");
        let helper_source = root.path().join("new-helper");
        let wrapper_source = root.path().join("new-wrapper");
        std::fs::write(&helper_source, b"new helper\n").expect("helper source");
        std::fs::write(&wrapper_source, NEW_WRAPPER).expect("wrapper source");
        let mut authority = test_release_authority("v0.127.0");
        authority.auth_helper.sha256 = digest(b"new helper\n");
        authority.auth_wrapper.sha256 = digest(NEW_WRAPPER);
        Self {
            root,
            helper,
            wrapper,
            helper_source,
            wrapper_source,
            authority,
        }
    }

    fn state(&self) -> PathBuf {
        self.root
            .path()
            .join("Library/Application Support/shipyard")
    }

    fn binary(&self) -> PathBuf {
        self.root.path().join(".local/bin/shipyard")
    }

    fn companion(&self) -> PathBuf {
        self.root
            .path()
            .join(".local/bin/shipyard-workstream-provider")
    }

    fn installed_binary_command(
        &self,
        options: &RunOptions,
        state: &Path,
        _binary: &Path,
    ) -> String {
        let expected_global_dir = shlex_quote(&state.display().to_string());
        let expected_wrapper = shlex_quote(&self.wrapper.display().to_string());
        let mut lines = vec![
            "#!/bin/sh".to_owned(),
            "set -eu".to_owned(),
            "test \"$1\" = --mode".to_owned(),
            "test \"$2\" = shipyard".to_owned(),
            "test \"$3\" = --global-dir".to_owned(),
            format!("test \"$4\" = {expected_global_dir}"),
            "if [ \"$5\" = auth ]; then".to_owned(),
            "  test \"$#\" = 10".to_owned(),
            "  test \"$6\" = helper-argv".to_owned(),
            "  test \"$7\" = --wrapper".to_owned(),
            format!("  test \"$8\" = {expected_wrapper}"),
            "  test \"$9\" = --repo".to_owned(),
            "  test \"${10}\" = danielraffel/Shipyard".to_owned(),
            format!("  exit {}", if options.resolver_succeeds { 0 } else { 71 }),
            "fi".to_owned(),
            "test \"$#\" = 9".to_owned(),
            "test \"$5\" = --state-dir".to_owned(),
            format!("test \"$6\" = {expected_global_dir}"),
            "test \"$7\" = --json".to_owned(),
            "test \"$8\" = daemon".to_owned(),
            "test \"$9\" = refresh".to_owned(),
        ];
        match &options.refresh {
            RefreshBehavior::Success => {}
            RefreshBehavior::Fail => lines.push("exit 72".to_owned()),
            RefreshBehavior::SignalTerm => {
                lines.push("/bin/kill -TERM \"$PPID\"".to_owned());
            }
            RefreshBehavior::SpawnDetached => {
                lines.push("(/bin/sleep 2 >/dev/null 2>&1 &)".to_owned());
            }
            RefreshBehavior::ReplaceLegacyPid => {
                let pid = state.join("fleet-auth-support.lock/pid");
                lines.extend([
                    format!(
                        "/usr/bin/printf '%s\\n' 99999999 > {}",
                        shlex_quote(&pid.display().to_string())
                    ),
                    format!("/bin/chmod 600 {}", shlex_quote(&pid.display().to_string())),
                ]);
            }
            RefreshBehavior::ObservePublishedLock(path) => {
                let lock = state.join("fleet-auth-support.lock");
                let pid = lock.join("pid");
                lines.extend([
                    format!("test -d {}", shlex_quote(&lock.display().to_string())),
                    format!("test ! -L {}", shlex_quote(&lock.display().to_string())),
                    format!("test -f {}", shlex_quote(&pid.display().to_string())),
                    format!("test ! -L {}", shlex_quote(&pid.display().to_string())),
                    format!(
                        "test \"$(/usr/bin/stat -f '%Lp' {})\" = 600",
                        shlex_quote(&pid.display().to_string())
                    ),
                    format!(
                        "published_pid=\"$(/bin/cat {})\"",
                        shlex_quote(&pid.display().to_string())
                    ),
                    "case \"$published_pid\" in ''|*[!0-9]*) exit 73 ;; esac".to_owned(),
                    "/bin/kill -0 \"$published_pid\"".to_owned(),
                    format!(
                        "/usr/bin/touch {}",
                        shlex_quote(&path.display().to_string())
                    ),
                ]);
            }
            RefreshBehavior::Touch(path) => lines.push(format!(
                "/usr/bin/touch {}",
                shlex_quote(&path.display().to_string())
            )),
        }
        lines.push(
            "/usr/bin/printf '%s\\n' '{\"schema_version\":1,\"command\":\"daemon:refresh\",\"new_pid\":4242}'"
                .to_owned(),
        );
        let quoted_lines = lines
            .iter()
            .map(|line| shlex_quote(line))
            .collect::<Vec<_>>()
            .join(" ");
        format!(
            "/usr/bin/printf '%s\\n' {quoted_lines} > \"$auth_generation_stage/shipyard\"; /bin/chmod 700 \"$auth_generation_stage/shipyard\"; /bin/cp \"$auth_generation_stage/shipyard\" \"$auth_generation_stage/shipyard-workstream-provider\""
        )
    }

    fn run_output(&self, options: RunOptions) -> Output {
        let resolver_required =
            crate::app::fleet_update_cmd::tag_supports_auth_resolver(options.target);
        let state = self.state();
        let binary = self.binary();
        let companion = self.companion();
        for path in [&binary, &companion] {
            if !path.exists() {
                std::fs::write(path, b"#!/bin/sh\nexit 0\n").expect("binary fixture");
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
                    .expect("binary mode");
            }
        }

        let installed_binary = self.installed_binary_command(&options, &state, &binary);
        let script = install_transaction(
            &self.helper,
            &self.wrapper,
            &binary,
            &companion,
            true,
            resolver_required,
            &shlex_quote(&self.helper_source.display().to_string()),
            &shlex_quote(&self.wrapper_source.display().to_string()),
            &installed_binary,
            "shipyard",
            &state,
            &state,
            "danielraffel/Shipyard",
            &self.authority,
            options.refresh_prefix,
            options.fail_after_helper,
        );
        let shell_prefix = options.lock_publish.shell_prefix(&state);
        Command::new("/bin/bash")
            .args(["-c", &format!("set -Eeuo pipefail\n{shell_prefix}{script}")])
            .env("HOME", self.root.path())
            .output()
            .expect("transaction")
    }

    fn run(&self, options: RunOptions) -> std::process::ExitStatus {
        self.run_output(options).status
    }
}

mod atomic_readers;
mod journal_v2;
mod lock;

#[test]
fn committed_transaction_streams_the_typed_refresh_receipt() {
    let fixture = Fixture::new();
    let output = fixture.run_output(RunOptions::default());
    assert!(output.status.success());
    let receipt: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("typed refresh receipt");
    assert_eq!(receipt["schema_version"], 1);
    assert_eq!(receipt["command"], "daemon:refresh");
    assert_eq!(receipt["new_pid"], 4242);
}

#[test]
fn committed_transaction_frames_the_remote_refresh_marker_exactly() {
    let fixture = Fixture::new();
    let output = fixture.run_output(RunOptions {
        refresh_prefix: crate::app::fleet_update_cmd::REMOTE_REFRESH_PREFIX,
        ..RunOptions::default()
    });
    assert!(output.status.success());
    let line = std::str::from_utf8(&output.stdout).expect("remote refresh marker");
    assert!(line.ends_with('\n'));
    assert_eq!(line.matches('\n').count(), 1);
    let payload = line
        .strip_prefix(crate::app::fleet_update_cmd::REMOTE_REFRESH_PREFIX)
        .and_then(|value| value.strip_suffix('\n'))
        .expect("exact remote marker framing");
    let receipt: serde_json::Value =
        serde_json::from_str(payload).expect("typed remote refresh receipt");
    assert_eq!(receipt["schema_version"], 1);
    assert_eq!(receipt["command"], "daemon:refresh");
    assert_eq!(receipt["new_pid"], 4242);
}

#[test]
fn legacy_pair_is_migrated_helper_first_to_exact_private_files() {
    let fixture = Fixture::new();
    std::fs::write(&fixture.helper, b"legacy fixed installation helper\n").expect("old helper");
    std::fs::write(&fixture.wrapper, b"legacy wrapper\n").expect("old wrapper");
    std::fs::set_permissions(&fixture.helper, std::fs::Permissions::from_mode(0o755))
        .expect("mode");
    std::fs::set_permissions(&fixture.wrapper, std::fs::Permissions::from_mode(0o755))
        .expect("mode");

    assert!(fixture.run(RunOptions::default()).success());
    assert_eq!(
        std::fs::read(&fixture.helper).expect("helper"),
        b"new helper\n"
    );
    assert_eq!(
        std::fs::read(&fixture.wrapper).expect("wrapper"),
        NEW_WRAPPER
    );
    let context = fixture
        .wrapper
        .with_file_name("ghapp.shipyard-context.json");
    let context_value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&context).expect("resolver context"))
            .expect("typed resolver context");
    let wrapper_target = std::fs::read_link(&fixture.wrapper).expect("generation wrapper");
    let generation_id = wrapper_target
        .parent()
        .and_then(Path::file_name)
        .and_then(|value| value.to_str())
        .expect("generation id");
    assert_eq!(context_value["schema_version"], 2);
    assert_eq!(context_value["mode"], "shipyard");
    assert_eq!(
        context_value["global_dir"],
        fixture
            .root
            .path()
            .join("Library/Application Support/shipyard")
            .display()
            .to_string()
    );
    assert_eq!(
        context_value["authority_identity"],
        fixture.authority.identity_sha256
    );
    assert_eq!(context_value["generation_id"], generation_id);
    assert_eq!(
        std::fs::metadata(&context)
            .expect("context metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(&fixture.helper)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(&fixture.wrapper)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert!(
        !fixture
            .root
            .path()
            .join("Library/Application Support/shipyard/fleet-auth-support.transaction")
            .exists()
    );
}

#[test]
fn v0_130_target_retains_four_target_transaction_without_resolver_probe() {
    let fixture = Fixture::new();

    assert!(
        fixture
            .run(RunOptions {
                fail_after_helper: false,
                target: "v0.130.1",
                resolver_succeeds: false,
                ..RunOptions::default()
            })
            .success(),
        "legacy target must not execute the installed binary's failing resolver path",
    );
    assert!(
        !fixture
            .wrapper
            .with_file_name("ghapp.shipyard-context.json")
            .exists()
    );
    assert!(
        !fixture
            .root
            .path()
            .join("Library/Application Support/shipyard/fleet-auth-support.transaction")
            .exists()
    );
}
#[test]
fn v0_131_recovers_v0_130_nine_line_journal_and_partial_atomic_backups() {
    let fixture = Fixture::new();
    let state = fixture
        .root
        .path()
        .join("Library/Application Support/shipyard");
    let binary = fixture.root.path().join(".local/bin/shipyard");
    let companion = fixture
        .root
        .path()
        .join(".local/bin/shipyard-workstream-provider");
    for (path, contents) in [
        (&fixture.helper, b"old helper\n".as_slice()),
        (&fixture.wrapper, b"old wrapper\n".as_slice()),
        (&binary, b"old binary\n".as_slice()),
        (&companion, b"old companion\n".as_slice()),
    ] {
        std::fs::write(path, contents).expect("old artifact");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).expect("old mode");
    }
    for path in [&binary, &companion] {
        std::fs::write(
            format!("{}.shipyard-rollback.tmp", path.display()),
            b"partial backup",
        )
        .expect("interrupted backup");
    }
    let journal = state.join("fleet-auth-support.transaction");
    let journal_contents = format!(
        "preparing\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n1\n",
        "f".repeat(64),
        fixture.helper.display(),
        fixture.wrapper.display(),
        binary.display(),
        companion.display(),
        digest(b"old helper\n"),
        digest(b"old wrapper\n"),
    );
    assert_eq!(journal_contents.lines().count(), 9);
    std::fs::write(&journal, journal_contents).expect("legacy journal");

    assert!(
        !fixture
            .run(RunOptions {
                fail_after_helper: true,
                target: "v0.134.0",
                resolver_succeeds: false,
                ..RunOptions::default()
            })
            .success()
    );
    for (path, expected) in [
        (&fixture.helper, b"old helper\n".as_slice()),
        (&fixture.wrapper, b"old wrapper\n".as_slice()),
        (&binary, b"old binary\n".as_slice()),
        (&companion, b"old companion\n".as_slice()),
    ] {
        assert_eq!(std::fs::read(path).expect("restored artifact"), expected);
    }
    assert!(!journal.exists());
    assert!(!state.join("fleet-auth-support.lock").exists());
    let lock = state.join("fleet-auth-support.guard");
    assert!(lock.is_file());
    assert_eq!(
        std::fs::metadata(&lock)
            .expect("lock metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(!std::path::Path::new(&format!("{}.shipyard-rollback.tmp", binary.display())).exists());
    assert!(
        !std::path::Path::new(&format!("{}.shipyard-rollback.tmp", companion.display())).exists()
    );
}

#[test]
fn v0_131_preparing_recovery_discards_v0_130_partial_direct_backups() {
    let fixture = Fixture::new();
    let state = fixture.state();
    let binary = fixture.binary();
    let companion = fixture.companion();
    for (path, contents) in [
        (&binary, b"old binary\n".as_slice()),
        (&companion, b"old companion\n".as_slice()),
    ] {
        std::fs::write(path, contents).expect("intact live binary");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .expect("binary mode");
        std::fs::write(
            format!("{}.shipyard-rollback", path.display()),
            b"partial direct backup",
        )
        .expect("legacy partial rollback");
    }
    for (path, contents) in [
        (&fixture.helper, b"old helper\n".as_slice()),
        (&fixture.wrapper, b"old wrapper\n".as_slice()),
    ] {
        std::fs::write(format!("{}.shipyard-rollback", path.display()), contents)
            .expect("moved legacy auth artifact");
    }
    let journal = state.join("fleet-auth-support.transaction");
    std::fs::write(
        &journal,
        format!(
            "preparing\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n1\n",
            "f".repeat(64),
            fixture.helper.display(),
            fixture.wrapper.display(),
            binary.display(),
            companion.display(),
            digest(b"new helper\n"),
            digest(NEW_WRAPPER),
        ),
    )
    .expect("legacy preparing journal");

    assert!(
        !fixture
            .run(RunOptions {
                fail_after_helper: true,
                target: "v0.134.0",
                ..RunOptions::default()
            })
            .success()
    );
    for (path, expected) in [
        (&fixture.helper, b"old helper\n".as_slice()),
        (&fixture.wrapper, b"old wrapper\n".as_slice()),
        (&binary, b"old binary\n".as_slice()),
        (&companion, b"old companion\n".as_slice()),
    ] {
        assert_eq!(std::fs::read(path).expect("recovered artifact"), expected);
        assert!(!std::path::Path::new(&format!("{}.shipyard-rollback", path.display())).exists());
    }
    assert!(!journal.exists());
}

#[test]
fn partial_install_failure_rolls_back_both_legacy_files() {
    let fixture = Fixture::new();
    std::fs::write(&fixture.helper, b"old helper\n").expect("old helper");
    std::fs::write(&fixture.wrapper, b"old wrapper\n").expect("old wrapper");
    assert!(
        !fixture
            .run(RunOptions {
                fail_after_helper: true,
                ..RunOptions::default()
            })
            .success()
    );
    assert_eq!(
        std::fs::read(&fixture.helper).expect("helper"),
        b"old helper\n"
    );
    assert_eq!(
        std::fs::read(&fixture.wrapper).expect("wrapper"),
        b"old wrapper\n"
    );
}

#[test]
fn post_install_resolver_failure_rolls_back_all_installed_artifacts() {
    let fixture = Fixture::new();
    let binary = fixture.root.path().join(".local/bin/shipyard");
    let companion = fixture
        .root
        .path()
        .join(".local/bin/shipyard-workstream-provider");
    let context = fixture
        .wrapper
        .with_file_name("ghapp.shipyard-context.json");
    for (path, contents) in [
        (&fixture.helper, b"old helper\n".as_slice()),
        (&fixture.wrapper, b"old wrapper\n".as_slice()),
        (&binary, b"old binary\n".as_slice()),
        (&companion, b"old companion\n".as_slice()),
    ] {
        std::fs::write(path, contents).expect("old artifact");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).expect("old mode");
    }
    std::fs::write(&context, b"old context\n").expect("old context");
    std::fs::set_permissions(&context, std::fs::Permissions::from_mode(0o600))
        .expect("context mode");

    assert!(
        !fixture
            .run(RunOptions {
                fail_after_helper: false,
                target: "v0.134.0",
                resolver_succeeds: false,
                ..RunOptions::default()
            })
            .success()
    );
    assert_eq!(
        std::fs::read(&fixture.helper).expect("helper"),
        b"old helper\n"
    );
    assert_eq!(
        std::fs::read(&fixture.wrapper).expect("wrapper"),
        b"old wrapper\n"
    );
    assert_eq!(std::fs::read(&binary).expect("binary"), b"old binary\n");
    assert_eq!(std::fs::read(&context).expect("context"), b"old context\n");
    assert_eq!(
        std::fs::read(&companion).expect("companion"),
        b"old companion\n"
    );
}

#[test]
fn next_release_recovers_an_interrupted_prior_release_before_installing() {
    let fixture = Fixture::new();
    let state = fixture
        .root
        .path()
        .join("Library/Application Support/shipyard");
    let binary = fixture.root.path().join(".local/bin/shipyard");
    let companion = fixture
        .root
        .path()
        .join(".local/bin/shipyard-workstream-provider");
    std::fs::write(
        fixture.helper.with_extension("shipyard-rollback"),
        b"old helper\n",
    )
    .expect("helper rollback");
    std::fs::write(
        fixture.wrapper.with_extension("shipyard-rollback"),
        b"old wrapper\n",
    )
    .expect("wrapper rollback");
    std::fs::write(&fixture.helper, b"interrupted prior-release helper\n")
        .expect("interrupted helper");
    for (path, current, old) in [
        (
            &binary,
            b"interrupted binary\n".as_slice(),
            b"old binary\n".as_slice(),
        ),
        (
            &companion,
            b"interrupted companion\n".as_slice(),
            b"old companion\n".as_slice(),
        ),
    ] {
        std::fs::write(path, current).expect("interrupted binary pair");
        let rollback = path.with_extension("shipyard-rollback");
        std::fs::write(&rollback, old).expect("binary rollback");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .expect("binary mode");
        std::fs::set_permissions(&rollback, std::fs::Permissions::from_mode(0o700))
            .expect("rollback mode");
    }
    let journal = state.join("fleet-auth-support.transaction");
    std::fs::write(
        &journal,
        format!(
            "auth-installed\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n1\n",
            "f".repeat(64),
            fixture.helper.display(),
            fixture.wrapper.display(),
            binary.display(),
            companion.display(),
            digest(b"interrupted prior-release helper\n"),
            digest(b"interrupted prior-release wrapper\n")
        ),
    )
    .expect("prior journal");
    let lock = state.join("fleet-auth-support.lock");
    std::fs::create_dir(&lock).expect("stale legacy lock");
    std::fs::write(lock.join("pid"), b"99999999\n").expect("stale legacy pid");
    std::fs::set_permissions(lock.join("pid"), std::fs::Permissions::from_mode(0o600))
        .expect("stale pid mode");

    assert!(
        !fixture
            .run(RunOptions {
                fail_after_helper: true,
                ..RunOptions::default()
            })
            .success()
    );
    assert_eq!(
        std::fs::read(&fixture.helper).expect("helper"),
        b"old helper\n"
    );
    assert_eq!(
        std::fs::read(&fixture.wrapper).expect("wrapper"),
        b"old wrapper\n"
    );
    assert_eq!(std::fs::read(&binary).expect("binary"), b"old binary\n");
    assert_eq!(
        std::fs::read(&companion).expect("companion"),
        b"old companion\n"
    );
    assert!(!journal.exists());

    assert!(fixture.run(RunOptions::default()).success());
    assert_eq!(
        std::fs::read(&fixture.helper).expect("helper"),
        b"new helper\n"
    );
    assert_eq!(
        std::fs::read(&fixture.wrapper).expect("wrapper"),
        NEW_WRAPPER
    );
}

#[test]
fn next_release_rolls_back_an_interrupted_context_install() {
    let fixture = Fixture::new();
    let state = fixture
        .root
        .path()
        .join("Library/Application Support/shipyard");
    let binary = fixture.root.path().join(".local/bin/shipyard");
    let companion = fixture
        .root
        .path()
        .join(".local/bin/shipyard-workstream-provider");
    let context = fixture
        .wrapper
        .with_file_name("ghapp.shipyard-context.json");
    for (path, current, old, mode) in [
        (
            &fixture.helper,
            b"new helper\n".as_slice(),
            b"old helper\n".as_slice(),
            0o700,
        ),
        (
            &fixture.wrapper,
            NEW_WRAPPER,
            b"old wrapper\n".as_slice(),
            0o700,
        ),
        (
            &binary,
            b"new binary\n".as_slice(),
            b"old binary\n".as_slice(),
            0o700,
        ),
        (
            &companion,
            b"new companion\n".as_slice(),
            b"old companion\n".as_slice(),
            0o700,
        ),
        (
            &context,
            b"new context\n".as_slice(),
            b"old context\n".as_slice(),
            0o600,
        ),
    ] {
        std::fs::write(path, current).expect("interrupted artifact");
        std::fs::write(
            std::path::PathBuf::from(format!("{}.shipyard-rollback", path.display())),
            old,
        )
        .expect("rollback artifact");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .expect("artifact mode");
    }
    std::fs::write(
        state.join("fleet-auth-support.transaction"),
        format!(
            "context-installed\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n1\n",
            "f".repeat(64),
            fixture.helper.display(),
            fixture.wrapper.display(),
            binary.display(),
            companion.display(),
            context.display(),
            digest(b"new helper\n"),
            digest(NEW_WRAPPER),
            digest(b"new context\n"),
        ),
    )
    .expect("current journal");

    assert!(
        !fixture
            .run(RunOptions {
                fail_after_helper: true,
                ..RunOptions::default()
            })
            .success()
    );
    for (path, expected) in [
        (&fixture.helper, b"old helper\n".as_slice()),
        (&fixture.wrapper, b"old wrapper\n".as_slice()),
        (&binary, b"old binary\n".as_slice()),
        (&companion, b"old companion\n".as_slice()),
        (&context, b"old context\n".as_slice()),
    ] {
        assert_eq!(std::fs::read(path).expect("restored artifact"), expected);
    }
}

#[test]
fn tampered_source_and_symlink_target_fail_before_mutation() {
    let fixture = Fixture::new();
    std::fs::write(&fixture.helper_source, b"tampered\n").expect("tamper");
    assert!(!fixture.run(RunOptions::default()).success());
    assert!(!fixture.helper.exists());
    assert!(!fixture.wrapper.exists());

    std::fs::write(&fixture.helper_source, b"new helper\n").expect("restore source");
    let real = fixture.root.path().join("real-helper");
    std::fs::write(&real, b"legacy\n").expect("real");
    symlink(&real, &fixture.helper).expect("symlink");
    assert!(!fixture.run(RunOptions::default()).success());
    assert_eq!(std::fs::read_link(&fixture.helper).expect("link"), real);
}
