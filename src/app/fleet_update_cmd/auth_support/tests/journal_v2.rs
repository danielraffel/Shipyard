use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

use sha2::{Digest, Sha256};

use super::*;
use crate::app::fleet_update_cmd::test_release_authority;

const REAL_GHAPP: &[u8] = include_bytes!("../../../../../scripts/ghapp");
const REAL_PR_CLOSE_GUARD: &[u8] = include_bytes!("../../../../../scripts/ghapp_pr_close_guard.py");

fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn legacy_backup_record(label: &str, target: &Path) -> String {
    let rollback = PathBuf::from(format!("{}.shipyard-rollback", target.display()));
    let absent = PathBuf::from(format!("{}.shipyard-was-absent", target.display()));
    if rollback.is_symlink() {
        return format!(
            "{label}=symlink:{}\n",
            std::fs::read_link(rollback)
                .expect("rollback symlink")
                .display()
        );
    }
    if rollback.is_file() {
        let metadata = std::fs::metadata(&rollback).expect("rollback metadata");
        return format!(
            "{label}=file:{}:{:o}\n",
            digest(&std::fs::read(rollback).expect("rollback file")),
            metadata.permissions().mode() & 0o777
        );
    }
    assert!(absent.is_file(), "missing legacy backup for {label}");
    format!("{label}=absent\n")
}

struct Fixture {
    root: tempfile::TempDir,
    helper: PathBuf,
    wrapper: PathBuf,
    binary: PathBuf,
    companion: PathBuf,
    context: PathBuf,
    helper_source: PathBuf,
    wrapper_source: PathBuf,
    close_guard_source: PathBuf,
    binary_source: PathBuf,
    authority: ReleaseAuthority,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("fixture root");
        let bin = root.path().join(".local/bin");
        let helper_dir = root.path().join(".config/shipyard/bin");
        let state = root.path().join("Library/Application Support/shipyard");
        std::fs::create_dir_all(&bin).expect("bin");
        std::fs::create_dir_all(&helper_dir).expect("helper dir");
        std::fs::create_dir_all(&state).expect("state");
        let helper = helper_dir.join("shipyard-github-app-token");
        let wrapper = bin.join("ghapp");
        let binary = bin.join("shipyard");
        let companion = bin.join("shipyard-workstream-provider");
        let context = bin.join("ghapp.shipyard-context.json");
        let helper_source = root.path().join("release-helper");
        let wrapper_source = root.path().join("release-ghapp");
        let close_guard_source = root.path().join("release-pr-close-guard");
        let binary_source = root.path().join("release-shipyard");
        let private_key = root.path().join("private-key.pem");
        Self::write_executable(
            &helper_source,
            b"#!/bin/bash\n/usr/bin/printf '{\"token\":\"fixture\"}\\n'\n",
        );
        Self::write_executable(&wrapper_source, REAL_GHAPP);
        Self::write_executable(&close_guard_source, REAL_PR_CLOSE_GUARD);
        Self::write_executable(
            &binary_source,
            format!(
                "#!/usr/bin/python3\nimport json,sys\nif 'auth' in sys.argv:\n w=sys.argv[sys.argv.index('--wrapper')+1]; r=sys.argv[sys.argv.index('--repo')+1]; print(json.dumps({{'schema_version':1,'command':'auth.helper-argv','wrapper':w,'repo':r,'credential_argv':['--app-id','123','--private-key',{:?}]}}))\nelse: print(json.dumps({{'schema_version':1,'command':'daemon:refresh'}}))\n",
                private_key.display().to_string()
            )
            .as_bytes(),
        );
        std::fs::write(&private_key, b"key\n").expect("private key");
        std::fs::set_permissions(&private_key, std::fs::Permissions::from_mode(0o600))
            .expect("private key mode");
        let mut authority = test_release_authority("v0.137.0");
        authority.auth_helper.sha256 = digest(&std::fs::read(&helper_source).expect("helper"));
        authority.auth_wrapper.sha256 = digest(REAL_GHAPP);
        authority.pr_close_guard.sha256 = digest(REAL_PR_CLOSE_GUARD);
        let fixture = Self {
            root,
            helper,
            wrapper,
            binary,
            companion,
            context,
            helper_source,
            wrapper_source,
            close_guard_source,
            binary_source,
            authority,
        };
        fixture.install_legacy(&private_key);
        fixture
    }

    fn write_executable(path: &Path, contents: &[u8]) {
        std::fs::write(path, contents).expect("write executable");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .expect("executable mode");
    }

    fn state(&self) -> PathBuf {
        self.root
            .path()
            .join("Library/Application Support/shipyard")
    }

    fn install_legacy(&self, private_key: &Path) {
        for (source, target) in [
            (&self.helper_source, &self.helper),
            (&self.wrapper_source, &self.wrapper),
            (&self.binary_source, &self.binary),
            (&self.binary_source, &self.companion),
        ] {
            std::fs::copy(source, target).expect("legacy member");
            std::fs::set_permissions(target, std::fs::Permissions::from_mode(0o700))
                .expect("legacy mode");
        }
        std::fs::write(
            &self.context,
            serde_json::to_vec(&serde_json::json!({
                "schema_version": 1,
                "mode": "shipyard",
                "global_dir": self.state(),
            }))
            .expect("context"),
        )
        .expect("legacy context");
        std::fs::set_permissions(&self.context, std::fs::Permissions::from_mode(0o600))
            .expect("context mode");
        std::fs::write(
            self.state().join("config.toml"),
            format!(
                "[github.auth]\nsource=\"command\"\ntoken_command=[{:?},\"token\",\"--app-id\",\"123\",\"--private-key\",{:?},\"--repo\",\"{{repo_slug}}\"]\n",
                self.wrapper.display().to_string(),
                private_key.display().to_string()
            ),
        )
        .expect("config");
    }

    fn script(&self) -> String {
        let install_binary = format!(
            "/bin/cp {} \"$auth_generation_stage/shipyard\"; /bin/cp {} \"$auth_generation_stage/shipyard-workstream-provider\"",
            shlex_quote(&self.binary_source.display().to_string()),
            shlex_quote(&self.binary_source.display().to_string())
        );
        install_transaction(
            &self.helper,
            &self.wrapper,
            &self.binary,
            &self.companion,
            true,
            true,
            &shlex_quote(&self.helper_source.display().to_string()),
            &shlex_quote(&self.wrapper_source.display().to_string()),
            &shlex_quote(&self.close_guard_source.display().to_string()),
            &install_binary,
            "shipyard",
            &self.state(),
            &self.state(),
            "danielraffel/Shipyard",
            &self.authority,
            "",
            false,
        )
    }

    fn update_release(&mut self, marker: &str) {
        Self::write_executable(
            &self.helper_source,
            format!("#!/bin/bash\n# {marker}\n/usr/bin/printf '{{\"token\":\"fixture\"}}\\n'\n")
                .as_bytes(),
        );
        self.authority.auth_helper.sha256 =
            digest(&std::fs::read(&self.helper_source).expect("updated helper"));
        self.authority.identity_sha256 = digest(format!("authority-{marker}").as_bytes());
    }

    fn run(&self, script: &str) -> ExitStatus {
        Command::new("/bin/bash")
            .args(["-c", &format!("set -Eeuo pipefail\n{script}")])
            .env_clear()
            .env("HOME", self.root.path())
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .status()
            .expect("run transaction")
    }

    fn run_traced(&self, script: &str) -> std::process::Output {
        Command::new("/bin/bash")
            .args(["-c", &format!("set -Eeuox pipefail\n{script}")])
            .env_clear()
            .env("HOME", self.root.path())
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .output()
            .expect("run traced transaction")
    }

    fn legacy_backup_cohort_digest(&self) -> String {
        let close_guard = self
            .root
            .path()
            .join(".config/shipyard/guards/pr-close-guard");
        let cohort = [
            ("helper", self.helper.as_path()),
            ("wrapper", self.wrapper.as_path()),
            ("binary", self.binary.as_path()),
            ("companion", self.companion.as_path()),
            ("context", self.context.as_path()),
            ("close_guard", close_guard.as_path()),
        ]
        .into_iter()
        .map(|(label, path)| legacy_backup_record(label, path))
        .collect::<String>();
        digest(cohort.as_bytes())
    }
}

#[test]
fn legacy_v2_preparing_journal_recovers_without_schema_upgrade() {
    let fixture = Fixture::new();
    let script = fixture.script();
    let needle = "auth_write_phase preparing\n";
    assert_eq!(script.matches(needle).count(), 1);
    let interrupted = script.replacen(needle, &format!("{needle}/bin/kill -9 $$\n"), 1);
    assert!(!fixture.run(&interrupted).success());

    let journal = fixture.state().join("fleet-auth-support.transaction");
    let mut lines: Vec<String> = std::fs::read_to_string(&journal)
        .expect("v3 journal")
        .lines()
        .map(ToOwned::to_owned)
        .collect();
    assert_eq!(lines.len(), 26);
    lines[0] = "shipyard-fleet-auth-v2".to_owned();
    lines.remove(11);
    lines.remove(8);
    assert_eq!(lines.len(), 24);
    std::fs::write(&journal, format!("{}\n", lines.join("\n"))).expect("v2 journal");

    assert!(fixture.run(&script).success(), "legacy v2 recovery");
    assert!(!journal.exists());
    assert!(std::fs::read_link(fixture.wrapper.with_extension("shipyard-generation")).is_ok());
}

#[test]
fn legacy_v3_prepared_journal_validates_the_pre_selector_backup_cohort() {
    let fixture = Fixture::new();
    let script = fixture.script();
    let needle = "auth_write_phase prepared\n";
    assert_eq!(script.matches(needle).count(), 1);
    let interrupted = script.replacen(needle, &format!("{needle}/bin/kill -9 $$\n"), 1);
    assert!(!fixture.run(&interrupted).success());

    let journal = fixture.state().join("fleet-auth-support.transaction");
    let mut lines: Vec<String> = std::fs::read_to_string(&journal)
        .expect("v4 journal")
        .lines()
        .map(ToOwned::to_owned)
        .collect();
    assert_eq!(lines.len(), 26);
    assert_eq!(lines[0], "shipyard-fleet-auth-v4");
    assert_eq!(lines[1], "prepared");
    lines[0] = "shipyard-fleet-auth-v3".to_owned();
    lines[24] = fixture.legacy_backup_cohort_digest();
    std::fs::write(&journal, format!("{}\n", lines.join("\n"))).expect("v3 journal");
    for suffix in ["shipyard-rollback", "shipyard-was-absent"] {
        let selector_marker = PathBuf::from(format!(
            "{}.{}",
            fixture
                .wrapper
                .with_extension("shipyard-generation")
                .display(),
            suffix
        ));
        if selector_marker.is_symlink() || selector_marker.exists() {
            std::fs::remove_file(selector_marker).expect("remove v4-only selector marker");
        }
    }

    assert!(fixture.run(&script).success(), "legacy v3 recovery");
    assert!(!journal.exists());
    assert!(std::fs::read_link(fixture.wrapper.with_extension("shipyard-generation")).is_ok());
}

#[test]
fn target_selector_rename_before_journal_rolls_forward_on_successor() {
    let fixture = Fixture::new();
    let script = fixture.script();
    let needle = "auth_publish_link \"$auth_selector\" \"$auth_generation/ghapp\"\nif [ \"$auth_public_trampoline_active\" = 0 ]; then auth_publish_file \"$auth_wrapper\" \"$auth_generation/ghapp.public-trampoline\"; fi\nauth_write_phase target-selected\n";
    assert_eq!(script.matches(needle).count(), 1);
    let interrupted = script.replacen(
        needle,
        "auth_publish_link \"$auth_selector\" \"$auth_generation/ghapp\"\nif [ \"$auth_public_trampoline_active\" = 0 ]; then auth_publish_file \"$auth_wrapper\" \"$auth_generation/ghapp.public-trampoline\"; fi\n/bin/kill -9 $$\nauth_write_phase target-selected\n",
        1,
    );
    assert!(!fixture.run(&interrupted).success());
    let selected_before = std::fs::read_link(fixture.wrapper.with_extension("shipyard-generation"))
        .expect("target selector");
    let successor = fixture.run_traced(&script);
    assert!(
        successor.status.success(),
        "successor recovery:\n{}",
        String::from_utf8_lossy(&successor.stderr)
    );
    assert_eq!(
        std::fs::read_link(fixture.wrapper.with_extension("shipyard-generation"))
            .expect("recovered selector"),
        selected_before
    );
    assert!(
        !fixture
            .state()
            .join("fleet-auth-support.transaction")
            .exists()
    );
}

#[test]
fn clean_install_recovers_a_selector_published_before_the_public_trampoline() {
    let fixture = Fixture::new();
    for installed in [
        &fixture.helper,
        &fixture.wrapper,
        &fixture.binary,
        &fixture.companion,
        &fixture.context,
    ] {
        std::fs::remove_file(installed).expect("remove legacy installed member");
    }
    let script = fixture.script();
    let needle = "auth_publish_link \"$auth_selector\" \"$auth_generation/ghapp\"\n";
    assert_eq!(script.matches(needle).count(), 1);
    let interrupted = script.replacen(needle, &format!("{needle}/bin/kill -9 $$\n"), 1);
    assert!(!fixture.run(&interrupted).success());
    assert!(!fixture.wrapper.exists());
    assert!(std::fs::read_link(fixture.wrapper.with_extension("shipyard-generation")).is_ok());

    let successor = fixture.run_traced(&script);
    assert!(
        successor.status.success(),
        "clean-install recovery:\n{}",
        String::from_utf8_lossy(&successor.stderr)
    );
    assert!(fixture.wrapper.is_file());
    assert!(std::fs::read_link(fixture.wrapper.with_extension("shipyard-generation")).is_ok());
    assert!(
        !fixture
            .state()
            .join("fleet-auth-support.transaction")
            .exists()
    );
}

#[test]
fn v3_recovery_refuses_a_target_generation_missing_its_guard() {
    let fixture = Fixture::new();
    let script = fixture.script();
    let needle = "auth_write_phase target-selected\n";
    assert_eq!(script.matches(needle).count(), 1);
    let interrupted = script.replacen(needle, &format!("{needle}/bin/kill -9 $$\n"), 1);
    assert!(!fixture.run(&interrupted).success());

    let selected = std::fs::read_link(fixture.wrapper.with_extension("shipyard-generation"))
        .expect("target selector");
    std::fs::remove_file(
        selected
            .parent()
            .expect("generation directory")
            .join("pr-close-guard"),
    )
    .expect("remove target guard");

    assert!(!fixture.run(&script).success());
    assert!(
        fixture
            .state()
            .join("fleet-auth-support.transaction")
            .exists()
    );
}

#[test]
fn unknown_live_selector_refuses_without_touching_projections_or_journal() {
    let fixture = Fixture::new();
    let script = fixture.script();
    let needle = "auth_write_phase anchor-selected\n";
    assert_eq!(script.matches(needle).count(), 1);
    let interrupted = script.replacen(needle, &format!("{needle}/bin/kill -9 $$\n"), 1);
    assert!(!fixture.run(&interrupted).success());
    let unknown_id = "9".repeat(64);
    let unknown = fixture
        .root
        .path()
        .join(".local/share/shipyard/auth-generations")
        .join(unknown_id);
    std::fs::create_dir(&unknown).expect("unknown generation");
    std::fs::set_permissions(&unknown, std::fs::Permissions::from_mode(0o700))
        .expect("unknown mode");
    SelfContained::write_unknown_wrapper(&unknown.join("ghapp"));
    std::fs::remove_file(&fixture.wrapper).expect("remove selector");
    symlink(unknown.join("ghapp"), &fixture.wrapper).expect("unknown selector");
    let before = [
        std::fs::read(&fixture.helper).expect("helper projection"),
        std::fs::read(&fixture.binary).expect("binary projection"),
        std::fs::read(&fixture.companion).expect("provider projection"),
        std::fs::read(&fixture.context).expect("context projection"),
    ];
    assert!(
        !fixture.run(&script).success(),
        "unknown selector must refuse"
    );
    assert_eq!(std::fs::read(&fixture.helper).expect("helper"), before[0]);
    assert_eq!(std::fs::read(&fixture.binary).expect("binary"), before[1]);
    assert_eq!(
        std::fs::read(&fixture.companion).expect("provider"),
        before[2]
    );
    assert_eq!(std::fs::read(&fixture.context).expect("context"), before[3]);
    assert!(
        fixture
            .state()
            .join("fleet-auth-support.transaction")
            .exists()
    );
}

#[test]
fn tampered_prior_backup_cohort_refuses_recovery_without_publication() {
    let mut fixture = Fixture::new();
    assert!(
        fixture.run(&fixture.script()).success(),
        "initial generation"
    );
    let original_selector =
        std::fs::read_link(fixture.wrapper.with_extension("shipyard-generation"))
            .expect("original selector");
    fixture.update_release("successor");
    let script = fixture.script();
    let needle = "auth_write_phase prepared\n";
    assert_eq!(script.matches(needle).count(), 1);
    let interrupted = script.replacen(needle, &format!("{needle}/bin/kill -9 $$\n"), 1);
    assert!(!fixture.run(&interrupted).success());
    std::fs::write(
        fixture.helper.with_extension("shipyard-rollback"),
        b"tampered backup\n",
    )
    .expect("tamper helper backup");

    assert!(
        !fixture.run(&script).success(),
        "tampered cohort must refuse"
    );
    assert_eq!(
        std::fs::read_link(fixture.wrapper.with_extension("shipyard-generation"))
            .expect("preserved selector"),
        original_selector
    );
    assert!(
        fixture
            .state()
            .join("fleet-auth-support.transaction")
            .exists(),
        "refusal must retain recovery evidence"
    );
}

#[test]
fn tampered_prior_generation_manifest_refuses_recovery_without_publication() {
    let mut fixture = Fixture::new();
    assert!(
        fixture.run(&fixture.script()).success(),
        "initial generation"
    );
    let original_selector =
        std::fs::read_link(fixture.wrapper.with_extension("shipyard-generation"))
            .expect("original selector");
    let original_manifest = original_selector
        .parent()
        .expect("generation dir")
        .join("generation.manifest");
    fixture.update_release("successor");
    let script = fixture.script();
    let needle = "auth_write_phase prepared\n";
    assert_eq!(script.matches(needle).count(), 1);
    let interrupted = script.replacen(needle, &format!("{needle}/bin/kill -9 $$\n"), 1);
    assert!(!fixture.run(&interrupted).success());
    let mut manifest = std::fs::read(&original_manifest).expect("prior manifest");
    manifest.extend_from_slice(b"tampered=true\n");
    std::fs::write(&original_manifest, manifest).expect("tamper prior manifest");

    assert!(
        !fixture.run(&script).success(),
        "tampered prior generation must refuse"
    );
    assert_eq!(
        std::fs::read_link(fixture.wrapper.with_extension("shipyard-generation"))
            .expect("preserved selector"),
        original_selector
    );
    assert!(
        fixture
            .state()
            .join("fleet-auth-support.transaction")
            .exists(),
        "refusal must retain recovery evidence"
    );
}

struct SelfContained;

impl SelfContained {
    fn write_unknown_wrapper(path: &Path) {
        Fixture::write_executable(path, b"#!/bin/bash\nexit 0\n");
    }
}
