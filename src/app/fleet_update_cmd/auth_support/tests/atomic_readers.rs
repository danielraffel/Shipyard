use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

use super::*;

const REAL_GHAPP: &[u8] = include_bytes!("../../../../../scripts/ghapp");
const REAL_PR_CLOSE_GUARD: &[u8] = include_bytes!("../../../../../scripts/ghapp_pr_close_guard.py");
const GENERATION_CHECKING_GUARD: &[u8] = b"#!/bin/bash\nset -euo pipefail\nif [[ -n \"${SHIPYARD_GHAPP_GENERATION_ID:-}\" ]]; then [[ \"$(basename \"$(dirname \"$0\")\")\" = \"$SHIPYARD_GHAPP_GENERATION_ID\" ]] || exit 93; fi\nexit 0\n";

struct ReaderFixture {
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
    fake_gh: PathBuf,
    authority: ReleaseAuthority,
}

impl ReaderFixture {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("reader root");
        let bin = root.path().join(".local/bin");
        let helper_dir = root.path().join(".config/shipyard/bin");
        let guards_dir = root.path().join(".config/shipyard/guards");
        let state = root.path().join("Library/Application Support/shipyard");
        std::fs::create_dir_all(&bin).expect("bin");
        std::fs::create_dir_all(&helper_dir).expect("helper dir");
        std::fs::create_dir_all(&guards_dir).expect("guards dir");
        std::fs::create_dir_all(&state).expect("state");

        let helper = helper_dir.join("shipyard-github-app-token");
        let wrapper = bin.join("ghapp");
        let binary = bin.join("shipyard");
        let companion = bin.join("shipyard-workstream-provider");
        let context = bin.join("ghapp.shipyard-context.json");
        let helper_source = root.path().join("release-helper.py");
        let wrapper_source = root.path().join("release-ghapp");
        let close_guard_source = root.path().join("release-pr-close-guard");
        let binary_source = root.path().join("release-shipyard.py");
        let fake_gh = root.path().join("fake-gh");
        let private_key = root.path().join("private-key.pem");

        Self::write_executable(&wrapper_source, REAL_GHAPP);
        Self::write_executable(&close_guard_source, GENERATION_CHECKING_GUARD);
        Self::write_executable(&guards_dir.join("pr-close-guard"), REAL_PR_CLOSE_GUARD);
        Self::write_helper_source(&helper_source, "release-one");
        Self::write_binary_source(&binary_source, &private_key);
        Self::write_executable(
            &fake_gh,
            b"#!/bin/bash\nset -euo pipefail\n/usr/bin/printf '%s\\n' \"$GH_TOKEN\"\n",
        );
        std::fs::write(&private_key, b"offline-test-key\n").expect("private key");
        std::fs::set_permissions(&private_key, std::fs::Permissions::from_mode(0o600))
            .expect("private key mode");

        let mut authority = test_release_authority("v0.137.0");
        authority.auth_helper.sha256 = digest(&std::fs::read(&helper_source).expect("helper"));
        authority.auth_wrapper.sha256 = digest(REAL_GHAPP);
        authority.pr_close_guard.sha256 = digest(GENERATION_CHECKING_GUARD);
        Self {
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
            fake_gh,
            authority,
        }
    }

    fn write_executable(path: &Path, contents: &[u8]) {
        std::fs::write(path, contents).expect("write executable");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .expect("executable mode");
    }

    fn legacy_direct_wrapper() -> Vec<u8> {
        let wrapper = String::from_utf8(REAL_GHAPP.to_vec()).expect("UTF-8 wrapper");
        let begin = wrapper
            .find("# Shipyard-Stable-Public-Trampoline-BEGIN\n")
            .expect("trampoline begin");
        let end_marker = "# Shipyard-Stable-Public-Trampoline-END\n";
        let end = wrapper[begin..]
            .find(end_marker)
            .map(|offset| begin + offset + end_marker.len())
            .expect("trampoline end");
        let mut legacy = format!("{}{}", &wrapper[..begin], &wrapper[end..]);
        legacy = legacy
            .replace(
                "# Shipyard-Auth-Generation-Contract: auth-selector-v2\n",
                "# Shipyard-Auth-Generation-Contract: auth-selector-v1\n",
            )
            .replace(
                "# Shipyard-Sibling-Close-Guard-Contract: sibling-close-guard-v1\n",
                "",
            )
            .replace(
                "# Shipyard-Stable-Public-Trampoline-Contract: stable-selector-v1\n",
                "",
            );
        legacy.into_bytes()
    }

    fn write_helper_source(path: &Path, release: &str) {
        Self::write_executable(
            path,
            format!(
                "#!/usr/bin/python3\n# {release}\nimport json, os\ngeneration = os.path.basename(os.path.dirname(__file__))\nexpected = os.environ.get('SHIPYARD_GHAPP_GENERATION_ID')\nif expected is not None and expected != generation:\n    raise SystemExit('mixed helper generation')\nprint(json.dumps({{\"token\": \"reader-\" + generation}}))\n"
            )
            .as_bytes(),
        );
    }

    fn write_binary_source(path: &Path, private_key: &Path) {
        Self::write_executable(
            path,
            format!(
                "#!/usr/bin/python3\nimport json, os, sys, time\ntime.sleep(0.02)\npath_generation = os.path.basename(os.path.dirname(__file__))\nexpected = os.environ.get('SHIPYARD_GHAPP_GENERATION_ID')\ngeneration = expected or path_generation\nif expected is not None and expected != path_generation:\n    raise SystemExit('mixed binary generation')\ncontext_path = os.path.join(os.path.dirname(__file__), 'ghapp.shipyard-context.json')\nif os.path.exists(context_path):\n    with open(context_path, encoding='utf-8') as stream:\n        context = json.load(stream)\n    if expected is not None and context.get('schema_version') == 2 and context.get('generation_id') != expected:\n        raise SystemExit('mixed context generation')\nif 'auth' in sys.argv:\n    wrapper = sys.argv[sys.argv.index('--wrapper') + 1]\n    repo = sys.argv[sys.argv.index('--repo') + 1]\n    print(json.dumps({{\"schema_version\": 1, \"command\": \"auth.helper-argv\", \"wrapper\": wrapper, \"repo\": repo, \"credential_argv\": [\"--app-id\", \"123456\", \"--private-key\", {:?}]}}))\nelse:\n    print(json.dumps({{\"schema_version\": 1, \"command\": \"daemon:refresh\", \"new_pid\": 4242}}))\n",
                private_key.display().to_string()
            )
            .as_bytes(),
        );
    }

    fn state(&self) -> PathBuf {
        self.root
            .path()
            .join("Library/Application Support/shipyard")
    }

    fn install_direct_legacy_reader(&self) {
        let legacy_wrapper = Self::legacy_direct_wrapper();
        Self::write_executable(&self.wrapper, &legacy_wrapper);
        for (source, target, mode) in [
            (&self.helper_source, &self.helper, 0o700),
            (&self.binary_source, &self.binary, 0o700),
            (&self.binary_source, &self.companion, 0o700),
        ] {
            std::fs::copy(source, target).expect("install direct reader member");
            std::fs::set_permissions(target, std::fs::Permissions::from_mode(mode))
                .expect("direct member mode");
        }
        let context = serde_json::json!({
            "schema_version": 1,
            "mode": "shipyard",
            "global_dir": self.state(),
        });
        std::fs::write(
            &self.context,
            serde_json::to_vec(&context).expect("context json"),
        )
        .expect("direct context");
        std::fs::set_permissions(&self.context, std::fs::Permissions::from_mode(0o600))
            .expect("context mode");
    }

    fn update_release(&mut self, release: &str) {
        Self::write_helper_source(&self.helper_source, release);
        self.authority.auth_helper.sha256 =
            digest(&std::fs::read(&self.helper_source).expect("updated helper"));
        self.authority.identity_sha256 = digest(format!("authority-{release}").as_bytes());
    }

    fn update_wrapper_body(&mut self, release: &str) {
        let mut wrapper = std::fs::read_to_string(&self.wrapper_source).expect("wrapper source");
        wrapper.push('\n');
        wrapper.push_str("# release body marker: ");
        wrapper.push_str(release);
        wrapper.push('\n');
        Self::write_executable(&self.wrapper_source, wrapper.as_bytes());
        self.authority.auth_wrapper.sha256 = digest(wrapper.as_bytes());
        self.authority.identity_sha256 = digest(format!("authority-{release}").as_bytes());
    }

    fn downgrade_selected_generation_to_guardless_v1(&self) {
        let selected = std::fs::read_link(self.wrapper.with_extension("shipyard-generation"))
            .expect("selected generation");
        let generation = selected.parent().expect("generation directory");
        let mut wrapper = std::fs::read_to_string(generation.join("ghapp")).expect("wrapper");
        wrapper = wrapper
            .replace(
                "# Shipyard-Auth-Generation-Contract: auth-selector-v2\n",
                "# Shipyard-Auth-Generation-Contract: auth-selector-v1\n",
            )
            .replace(
                "# Shipyard-Sibling-Close-Guard-Contract: sibling-close-guard-v1\n",
                "",
            )
            .replace(
                "    close_guard=\"$generation_dir/pr-close-guard\"\n",
                "    close_guard=\"$guards/pr-close-guard\"\n",
            );
        Self::write_executable(&generation.join("ghapp"), wrapper.as_bytes());
        std::fs::remove_file(generation.join("pr-close-guard")).expect("remove sibling guard");
        let wrapper_sha = digest(wrapper.as_bytes());

        let seed = std::fs::read_to_string(generation.join("generation.seed"))
            .expect("generation seed")
            .lines()
            .filter(|line| !line.starts_with("close_guard_sha256="))
            .map(|line| match line.split_once('=') {
                Some(("generation_contract", _)) => {
                    "generation_contract=auth-selector-v1".to_owned()
                }
                Some(("wrapper_sha256", _)) => format!("wrapper_sha256={wrapper_sha}"),
                _ => line.to_owned(),
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(generation.join("generation.seed"), &seed).expect("legacy seed");
        let generation_id = digest(seed.as_bytes());

        let context_path = generation.join("ghapp.shipyard-context.json");
        let mut context: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&context_path).expect("generation context"))
                .expect("context json");
        context["generation_id"] = serde_json::Value::String(generation_id.clone());
        std::fs::write(
            &context_path,
            serde_json::to_vec(&context).expect("legacy context json"),
        )
        .expect("legacy context");
        let context_sha = digest(&std::fs::read(&context_path).expect("legacy context bytes"));

        let manifest = std::fs::read_to_string(generation.join("generation.manifest"))
            .expect("generation manifest")
            .lines()
            .filter(|line| {
                !line.starts_with("close_guard_sha256=") && !line.starts_with("close_guard_mode=")
            })
            .map(|line| match line.split_once('=') {
                Some(("generation_contract", _)) => {
                    "generation_contract=auth-selector-v1".to_owned()
                }
                Some(("generation_id", _)) => format!("generation_id={generation_id}"),
                Some(("wrapper_sha256", _)) => format!("wrapper_sha256={wrapper_sha}"),
                Some(("context_sha256", _)) => format!("context_sha256={context_sha}"),
                _ => line.to_owned(),
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(generation.join("generation.manifest"), manifest).expect("legacy manifest");

        let legacy_generation = generation
            .parent()
            .expect("generation root")
            .join(generation_id);
        std::fs::rename(generation, &legacy_generation).expect("rename legacy generation");
        let public_guard = self
            .root
            .path()
            .join(".config/shipyard/guards/pr-close-guard");
        std::fs::remove_file(&public_guard).expect("remove generated guard projection");
        Self::write_executable(&public_guard, REAL_PR_CLOSE_GUARD);
        for (projection, member) in [
            (&self.helper, "shipyard-github-app-token"),
            (&self.binary, "shipyard"),
            (&self.companion, "shipyard-workstream-provider"),
            (&self.context, "ghapp.shipyard-context.json"),
        ] {
            std::fs::remove_file(projection).expect("remove generated projection");
            std::os::unix::fs::symlink(legacy_generation.join(member), projection)
                .expect("select legacy member");
        }
        let selector = self.wrapper.with_extension("shipyard-generation");
        std::fs::remove_file(&selector).expect("remove prior selector");
        std::os::unix::fs::symlink(legacy_generation.join("ghapp"), &selector)
            .expect("select legacy generation");
    }

    fn transaction_script(&self) -> String {
        let install_binary = format!(
            "/bin/cp {} \"$auth_generation_stage/shipyard\"; /bin/chmod 700 \"$auth_generation_stage/shipyard\"; /bin/cp \"$auth_generation_stage/shipyard\" \"$auth_generation_stage/shipyard-workstream-provider\"",
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

    fn run_script(&self, script: &str) -> ExitStatus {
        Command::new("/bin/bash")
            .args(["-c", &format!("set -Eeuo pipefail\n{script}")])
            .env_clear()
            .env("HOME", self.root.path())
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .status()
            .expect("auth transaction")
    }

    fn run_script_traced(&self, script: &str) -> std::process::Output {
        Command::new("/bin/bash")
            .args(["-c", &format!("set -Eeuox pipefail\n{script}")])
            .env_clear()
            .env("HOME", self.root.path())
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .output()
            .expect("traced auth transaction")
    }

    fn read_once(&self) -> Result<String, String> {
        let output = Command::new(&self.wrapper)
            .args(["auth", "status", "--repo", "danielraffel/Shipyard"])
            .env_clear()
            .env("HOME", self.root.path())
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("SHIPYARD_GHAPP_PYTHON_BINARY", "/usr/bin/python3")
            .env("SHIPYARD_GHAPP_GH_BINARY", &self.fake_gh)
            .output()
            .map_err(|error| error.to_string())?;
        if !output.status.success() {
            return Err(format!(
                "status={:?} stderr={}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    fn assert_valid_reader_value(value: &str) {
        if value == "reader-bin" {
            return;
        }
        let generation = value.strip_prefix("reader-").expect("reader token prefix");
        assert_eq!(generation.len(), 64, "generation token: {value}");
        assert!(
            generation
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "generation token: {value}"
        );
    }
}

fn exercise_continuous_readers(
    fixture: &Arc<ReaderFixture>,
    mutation: impl FnOnce(&ReaderFixture),
) -> Vec<Result<String, String>> {
    let stop = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(AtomicUsize::new(0));
    let results = Arc::new(Mutex::new(Vec::new()));
    let reader_fixture = Arc::clone(fixture);
    let reader_stop = Arc::clone(&stop);
    let reader_reads = Arc::clone(&reads);
    let reader_results = Arc::clone(&results);
    let reader = thread::spawn(move || {
        while !reader_stop.load(Ordering::Acquire) {
            reader_results
                .lock()
                .expect("reader results")
                .push(reader_fixture.read_once());
            reader_reads.fetch_add(1, Ordering::Release);
        }
    });

    let start_deadline = Instant::now() + Duration::from_secs(30);
    while reads.load(Ordering::Acquire) < 2 {
        assert!(
            Instant::now() < start_deadline,
            "continuous reader did not start"
        );
        thread::sleep(Duration::from_millis(10));
    }
    mutation(fixture);
    let post_mutation_target = reads.load(Ordering::Acquire) + 2;
    let resume_deadline = Instant::now() + Duration::from_secs(30);
    while reads.load(Ordering::Acquire) < post_mutation_target {
        assert!(
            Instant::now() < resume_deadline,
            "continuous reader did not resume"
        );
        thread::sleep(Duration::from_millis(10));
    }
    stop.store(true, Ordering::Release);
    reader.join().expect("continuous reader");
    Arc::try_unwrap(results)
        .expect("reader result owner")
        .into_inner()
        .expect("reader results")
}

fn assert_all_readers_valid(results: &[Result<String, String>]) {
    assert!(results.len() >= 4, "reader count: {}", results.len());
    for result in results {
        let value = result.as_ref().unwrap_or_else(|error| {
            panic!("reader observed unavailable or mixed generation: {error}")
        });
        ReaderFixture::assert_valid_reader_value(value);
    }
}

fn wait_for_path(path: &Path, description: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {description}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn first_migration_selects_anchor_before_enumerating_direct_readers() {
    let fixture = ReaderFixture::new();
    fixture.install_direct_legacy_reader();

    let old_entered = fixture.root.path().join("old-reader-entered");
    let old_release = fixture.root.path().join("old-reader-entered.release");
    let mut legacy_wrapper = std::fs::read_to_string(&fixture.wrapper).expect("legacy wrapper");
    let latch = "fi\nif [[ -n \"${SHIPYARD_TEST_OLD_READER_ENTERED:-}\" ]]; then /usr/bin/touch \"$SHIPYARD_TEST_OLD_READER_ENTERED\"; while [[ ! -e \"$SHIPYARD_TEST_OLD_READER_ENTERED.release\" ]]; do /bin/sleep 0.02; done; fi\ncache_dir=";
    assert_eq!(legacy_wrapper.matches("fi\ncache_dir=").count(), 1);
    legacy_wrapper = legacy_wrapper.replacen("fi\ncache_dir=", latch, 1);
    ReaderFixture::write_executable(&fixture.wrapper, legacy_wrapper.as_bytes());

    let old_reader = Command::new(&fixture.wrapper)
        .args(["auth", "status", "--repo", "danielraffel/Shipyard"])
        .env_clear()
        .env("HOME", fixture.root.path())
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("SHIPYARD_GHAPP_PYTHON_BINARY", "/usr/bin/python3")
        .env("SHIPYARD_GHAPP_GH_BINARY", &fixture.fake_gh)
        .env("SHIPYARD_TEST_OLD_READER_ENTERED", &old_entered)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("old reader");
    wait_for_path(&old_entered, "old reader latch");

    let anchor_selected = fixture.root.path().join("anchor-selected");
    let transaction_release = fixture.root.path().join("transaction.release");
    let script = fixture.transaction_script();
    let anchor_publish = "auth_publish_file \"$auth_wrapper\" \"$auth_generation/ghapp.public-trampoline\"\n  auth_public_trampoline_active=1\n";
    assert_eq!(script.matches(anchor_publish).count(), 1);
    let injected = format!(
        "{anchor_publish}/usr/bin/touch {}\nwhile [ ! -e {} ]; do /bin/sleep 0.02; done\n",
        shlex_quote(&anchor_selected.display().to_string()),
        shlex_quote(&transaction_release.display().to_string()),
    );
    let transaction_script = script.replacen(anchor_publish, &injected, 1);
    let transaction_home = fixture.root.path().to_path_buf();
    let transaction = thread::spawn(move || {
        Command::new("/bin/bash")
            .args(["-c", &format!("set -Eeuo pipefail\n{transaction_script}")])
            .env_clear()
            .env("HOME", transaction_home)
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .status()
            .expect("latched transaction")
    });
    wait_for_path(&anchor_selected, "anchor selector publication");

    assert!(
        !fixture.wrapper.is_symlink(),
        "public wrapper became a symlink"
    );
    let anchor_target = std::fs::read_link(fixture.wrapper.with_extension("shipyard-generation"))
        .expect("anchor selector");
    let anchor_generation = anchor_target
        .parent()
        .and_then(Path::file_name)
        .and_then(|value| value.to_str())
        .expect("anchor generation");
    let anchor_read = fixture.read_once().expect("anchor reader");
    assert_eq!(anchor_read, format!("reader-{anchor_generation}"));
    assert!(
        !fixture.helper.is_symlink(),
        "projection moved before drain"
    );

    std::fs::write(&transaction_release, b"release\n").expect("release transaction latch");
    thread::sleep(Duration::from_millis(200));
    assert!(
        !fixture.helper.is_symlink(),
        "projection moved while old reader remained"
    );

    std::fs::write(&old_release, b"release\n").expect("release old reader");
    let old_output = old_reader.wait_with_output().expect("old reader output");
    assert!(
        old_output.status.success(),
        "old reader failed: {}",
        String::from_utf8_lossy(&old_output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&old_output.stdout).trim(),
        "reader-bin"
    );
    assert!(transaction.join().expect("transaction thread").success());

    let target_read = fixture.read_once().expect("target reader");
    ReaderFixture::assert_valid_reader_value(&target_read);
    assert_ne!(target_read, "reader-bin");
    assert_ne!(target_read, anchor_read);
}

#[test]
fn first_migration_fences_a_reader_that_appears_after_the_initial_observation() {
    let fixture = ReaderFixture::new();
    fixture.install_direct_legacy_reader();
    let observed = fixture.root.path().join("late-reader-observed");
    let release = fixture.root.path().join("late-reader.release");
    let late_reader = Command::new("/bin/bash")
        .args([
            "-c",
            &format!(
                "while [ ! -e {} ]; do /bin/sleep 0.02; done",
                shlex_quote(&release.display().to_string())
            ),
        ])
        .spawn()
        .expect("late reader identity");
    let late_pid = late_reader.id();
    let release_observed = observed.clone();
    let late_reader_wait = thread::spawn(move || {
        wait_for_path(&release_observed, "late reader observation");
        thread::sleep(Duration::from_millis(250));
        std::fs::write(&release, b"release\n").expect("release late reader");
        late_reader.wait_with_output().expect("late reader").status
    });
    let script = fixture.transaction_script();
    let observation = "auth_old_reader_pids=\"$(/bin/ps -axo pid=,command= | /usr/bin/awk -v wrapper=\"$auth_wrapper\" '$2 == \"/bin/bash\" && $3 == wrapper {print $1}')\"\n";
    assert_eq!(script.matches(observation).count(), 1);
    let delayed_observation = format!(
        "if [ \"$auth_observation_round\" = 2 ]; then /usr/bin/touch {}; auth_old_reader_pids={late_pid}; else {observation}fi\n",
        shlex_quote(&observed.display().to_string())
    );
    let script = script.replacen(observation, &delayed_observation, 1);
    let started = Instant::now();
    assert!(fixture.run_script(&script).success());
    assert!(
        started.elapsed() >= Duration::from_secs(1),
        "late reader was not retained in the finite cohort"
    );
    assert!(late_reader_wait.join().expect("late reader wait").success());
}

#[test]
fn first_migration_tolerates_a_reader_that_exits_before_identity_capture() {
    let fixture = ReaderFixture::new();
    fixture.install_direct_legacy_reader();
    let script = fixture.transaction_script();
    let observation = "auth_old_reader_pids=\"$(/bin/ps -axo pid=,command= | /usr/bin/awk -v wrapper=\"$auth_wrapper\" '$2 == \"/bin/bash\" && $3 == wrapper {print $1}')\"\n";
    assert_eq!(script.matches(observation).count(), 1);
    let vanished_observation = "auth_old_reader_pids=999999999\n";
    let script = script.replacen(observation, vanished_observation, 1);

    assert!(fixture.run_script(&script).success());
    ReaderFixture::assert_valid_reader_value(&fixture.read_once().expect("generation read"));
}

#[test]
fn real_wrapper_readers_remain_valid_during_first_generation_migration() {
    let fixture = Arc::new(ReaderFixture::new());
    fixture.install_direct_legacy_reader();
    assert_eq!(fixture.read_once().expect("legacy read"), "reader-bin");

    let results = exercise_continuous_readers(&fixture, |fixture| {
        let output = fixture.run_script_traced(&fixture.transaction_script());
        assert!(
            output.status.success(),
            "first migration failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    });

    assert_all_readers_valid(&results);
    let after = fixture.read_once().expect("generation read");
    ReaderFixture::assert_valid_reader_value(&after);
    assert_ne!(after, "reader-bin");
    assert!(
        results
            .iter()
            .any(|result| result.as_deref() == Ok("reader-bin")),
        "continuous reader did not observe the legacy generation"
    );
    assert!(
        results.iter().any(|result| result.as_deref() == Ok(&after)),
        "continuous reader did not observe the published generation"
    );
}

#[test]
fn real_wrapper_readers_remain_valid_during_generation_upgrade() {
    let mut fixture = ReaderFixture::new();
    fixture.install_direct_legacy_reader();
    assert!(fixture.run_script(&fixture.transaction_script()).success());
    let public_wrapper_inode = std::fs::metadata(&fixture.wrapper)
        .expect("public wrapper metadata")
        .ino();
    let public_wrapper_bytes = std::fs::read(&fixture.wrapper).expect("public wrapper bytes");
    assert!(!fixture.wrapper.is_symlink());
    let first = fixture.read_once().expect("first generation");
    ReaderFixture::assert_valid_reader_value(&first);
    fixture.update_release("release-two");
    let fixture = Arc::new(fixture);

    let results = exercise_continuous_readers(&fixture, |fixture| {
        let output = fixture.run_script_traced(&fixture.transaction_script());
        assert!(
            output.status.success(),
            "guardless generation upgrade failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    });

    assert_all_readers_valid(&results);
    assert_eq!(
        std::fs::metadata(&fixture.wrapper)
            .expect("upgraded public wrapper metadata")
            .ino(),
        public_wrapper_inode,
        "generation upgrade replaced the stable public wrapper"
    );
    assert_eq!(
        std::fs::read(&fixture.wrapper).expect("upgraded public wrapper bytes"),
        public_wrapper_bytes
    );
    let second = fixture.read_once().expect("second generation");
    ReaderFixture::assert_valid_reader_value(&second);
    assert_ne!(first, second);
    assert!(
        results.iter().any(|result| result.as_deref() == Ok(&first)),
        "continuous reader did not observe the prior generation"
    );
    assert!(
        results
            .iter()
            .any(|result| result.as_deref() == Ok(&second)),
        "continuous reader did not observe the successor generation"
    );
}

#[test]
fn changed_generation_wrapper_preserves_stable_public_trampoline_and_probe_receipt() {
    let mut fixture = ReaderFixture::new();
    fixture.install_direct_legacy_reader();
    assert!(fixture.run_script(&fixture.transaction_script()).success());
    let public_inode = std::fs::metadata(&fixture.wrapper)
        .expect("public wrapper metadata")
        .ino();
    let public_digest = digest(&std::fs::read(&fixture.wrapper).expect("public trampoline"));
    let first_target = std::fs::read_link(fixture.wrapper.with_extension("shipyard-generation"))
        .expect("first selected generation");

    fixture.update_wrapper_body("release-wrapper-body-two");
    let expected_wrapper_digest = fixture.authority.auth_wrapper.sha256.clone();
    assert!(fixture.run_script(&fixture.transaction_script()).success());

    assert_eq!(
        std::fs::metadata(&fixture.wrapper)
            .expect("upgraded public wrapper metadata")
            .ino(),
        public_inode,
        "release wrapper body change replaced the stable public trampoline"
    );
    assert_eq!(
        digest(&std::fs::read(&fixture.wrapper).expect("stable public trampoline")),
        public_digest
    );
    let selected = std::fs::read_link(fixture.wrapper.with_extension("shipyard-generation"))
        .expect("upgraded selected generation");
    assert_ne!(selected, first_target);
    assert_eq!(
        digest(&std::fs::read(&selected).expect("selected release wrapper")),
        expected_wrapper_digest
    );

    let probe_script = format!(
        "{}\n/usr/bin/printf '%s|%s\\n' \"$after_auth_wrapper_sha256\" \"$after_auth_wrapper_target\"",
        probe(&fixture.helper, &fixture.wrapper, "after")
    );
    let output = Command::new("/bin/bash")
        .args(["-c", &format!("set -Eeuo pipefail\n{probe_script}")])
        .env_clear()
        .env("HOME", fixture.root.path())
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .output()
        .expect("generation receipt probe");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        format!("{expected_wrapper_digest}|{}", selected.display())
    );
}

#[test]
fn guardless_v1_generation_is_anchored_before_public_guard_upgrade() {
    let mut fixture = ReaderFixture::new();
    fixture.install_direct_legacy_reader();
    assert!(fixture.run_script(&fixture.transaction_script()).success());
    fixture.downgrade_selected_generation_to_guardless_v1();
    let legacy = fixture.read_once().expect("legacy generation read");
    fixture.update_release("guardless-v1-successor");
    let fixture = Arc::new(fixture);

    let results = exercise_continuous_readers(&fixture, |fixture| {
        let output = fixture.run_script_traced(&fixture.transaction_script());
        assert!(
            output.status.success(),
            "guardless generation upgrade failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    });

    assert_all_readers_valid(&results);
    let successor = fixture.read_once().expect("successor generation read");
    assert_ne!(legacy, successor);
}

#[test]
fn guardless_v1_generation_rollback_restores_guard_before_selector() {
    let mut fixture = ReaderFixture::new();
    fixture.install_direct_legacy_reader();
    assert!(fixture.run_script(&fixture.transaction_script()).success());
    fixture.downgrade_selected_generation_to_guardless_v1();
    let legacy = fixture.read_once().expect("legacy generation read");
    let public_wrapper_inode = std::fs::metadata(&fixture.wrapper)
        .expect("public wrapper metadata")
        .ino();
    fixture.update_release("guardless-v1-rollback");
    let fixture = Arc::new(fixture);

    let results = exercise_continuous_readers(&fixture, |fixture| {
        let script = fixture.transaction_script();
        let publish = "auth_write_phase projections-publish-intent\nauth_publish_link \"$auth_close_guard\" \"$auth_generation/pr-close-guard\"\n";
        assert_eq!(script.matches(publish).count(), 1);
        let failed = script.replacen(publish, &format!("{publish}/usr/bin/false\n"), 1);
        assert!(!fixture.run_script(&failed).success());
    });

    assert_all_readers_valid(&results);
    assert_eq!(
        std::fs::metadata(&fixture.wrapper)
            .expect("rolled-back public wrapper metadata")
            .ino(),
        public_wrapper_inode,
        "rollback replaced the stable public wrapper"
    );
    assert!(!fixture.wrapper.is_symlink());
    assert_eq!(fixture.read_once().expect("restored legacy read"), legacy);
}

#[test]
fn sigkill_after_generation_publish_preserves_reader_and_recovers_atomically() {
    let mut fixture = ReaderFixture::new();
    fixture.install_direct_legacy_reader();
    assert!(fixture.run_script(&fixture.transaction_script()).success());
    let first = fixture.read_once().expect("first generation");
    fixture.update_release("release-after-sigkill");

    let interrupted = fixture.transaction_script().replacen(
        "auth_write_phase generation-installed\n",
        "auth_write_phase generation-installed\n/bin/kill -9 $$\n",
        1,
    );
    let status = fixture.run_script(&interrupted);
    assert!(
        !status.success(),
        "SIGKILL transaction unexpectedly succeeded"
    );
    assert_eq!(
        fixture.read_once().expect("reader after SIGKILL"),
        first,
        "unpublished generation must not disturb the live selector"
    );

    let fixture = Arc::new(fixture);
    let results = exercise_continuous_readers(&fixture, |fixture| {
        assert!(
            fixture.run_script(&fixture.transaction_script()).success(),
            "successor transaction must recover the interrupted journal"
        );
    });
    assert_all_readers_valid(&results);
    let recovered = fixture.read_once().expect("recovered generation");
    ReaderFixture::assert_valid_reader_value(&recovered);
    assert_ne!(first, recovered);
    assert!(
        results.iter().any(|result| result.as_deref() == Ok(&first)),
        "recovery readers did not observe the preserved generation"
    );
    assert!(
        results
            .iter()
            .any(|result| result.as_deref() == Ok(&recovered)),
        "recovery readers did not observe the recovered generation"
    );
}

#[test]
fn release_without_authenticated_selector_capability_refuses_before_publication() {
    let mut fixture = ReaderFixture::new();
    let old_wrapper = String::from_utf8(REAL_GHAPP.to_vec())
        .expect("wrapper utf8")
        .replace(
            "# Shipyard-Auth-Generation-Contract: auth-selector-v2\n",
            "",
        );
    ReaderFixture::write_executable(&fixture.wrapper_source, old_wrapper.as_bytes());
    fixture.authority.auth_wrapper.sha256 = digest(old_wrapper.as_bytes());
    fixture.install_direct_legacy_reader();
    let before = std::fs::read(&fixture.wrapper).expect("legacy wrapper");

    assert!(
        !fixture.run_script(&fixture.transaction_script()).success(),
        "a signed but selector-unaware wrapper must refuse"
    );
    assert_eq!(
        std::fs::read(&fixture.wrapper).expect("public wrapper"),
        before
    );
    assert!(!fixture.wrapper.is_symlink());
}

#[test]
fn rollback_intent_survives_sigkill_before_first_restore() {
    let mut fixture = ReaderFixture::new();
    fixture.install_direct_legacy_reader();
    assert!(fixture.run_script(&fixture.transaction_script()).success());
    let original_selector =
        std::fs::read_link(fixture.wrapper.with_extension("shipyard-generation"))
            .expect("original selector");
    fixture.update_release("rollback-intent");
    let script = fixture.transaction_script();
    let intent_boundary = "if [ \"$auth_recovery_phase\" != rollback-intent ]; then auth_write_recovery_phase rollback-intent; fi\n    auth_validate_recovery_prior\n    if [ \"$auth_recovery_anchor_id\" != absent ]; then\n";
    assert_eq!(script.matches(intent_boundary).count(), 1);
    let interrupted = script.replacen(
        intent_boundary,
        "if [ \"$auth_recovery_phase\" != rollback-intent ]; then auth_write_recovery_phase rollback-intent; fi\n    /bin/kill -9 $$\n    auth_validate_recovery_prior\n    if [ \"$auth_recovery_anchor_id\" != absent ]; then\n",
        1,
    );
    let probe = "\"$auth_binary\" --mode \"$auth_mode\" --global-dir \"$auth_global_dir\" auth helper-argv --wrapper \"$auth_wrapper\" --repo \"$auth_probe_repo\" >/dev/null\n";
    assert_eq!(interrupted.matches(probe).count(), 1);
    let interrupted = interrupted.replacen(probe, "/usr/bin/false\n", 1);
    assert!(!fixture.run_script(&interrupted).success());
    let journal = fixture.state().join("fleet-auth-support.transaction");
    assert_eq!(
        std::fs::read_to_string(&journal)
            .expect("rollback journal")
            .lines()
            .nth(1),
        Some("rollback-intent")
    );

    let recovery_boundary = "case \"$auth_recovery_needed\" in 1) auth_recover ;; esac\n";
    assert_eq!(script.matches(recovery_boundary).count(), 1);
    let recovery_only = script.replacen(
        recovery_boundary,
        &format!("{recovery_boundary}/bin/kill -9 $$\n"),
        1,
    );
    assert!(!fixture.run_script(&recovery_only).success());
    assert_eq!(
        std::fs::read_link(fixture.wrapper.with_extension("shipyard-generation"))
            .expect("rolled-back selector"),
        original_selector,
        "durable rollback intent must win over the visible failed target"
    );
    assert!(fixture.run_script(&script).success());
}

#[test]
fn sigkill_validation_boundaries_choose_the_recorded_recovery_direction() {
    for (phase, expected_legacy) in [("validation-intent", true), ("validated", false)] {
        let fixture = ReaderFixture::new();
        fixture.install_direct_legacy_reader();
        let script = fixture.transaction_script();
        let phase_write = format!("auth_write_phase {phase}\n");
        assert_eq!(script.matches(&phase_write).count(), 1);
        let interrupted =
            script.replacen(&phase_write, &format!("{phase_write}/bin/kill -9 $$\n"), 1);
        assert!(!fixture.run_script(&interrupted).success());

        let recovery_boundary = "case \"$auth_recovery_needed\" in 1) auth_recover ;; esac\n";
        assert_eq!(script.matches(recovery_boundary).count(), 1);
        let recovery_only = script.replacen(
            recovery_boundary,
            &format!("{recovery_boundary}/bin/kill -9 $$\n"),
            1,
        );
        assert!(!fixture.run_script(&recovery_only).success());
        let observed = fixture.read_once().expect("reader after recovery decision");
        assert_eq!(
            observed == "reader-bin",
            expected_legacy,
            "phase {phase} chose the wrong durable recovery direction: {observed}"
        );
        ReaderFixture::assert_valid_reader_value(&observed);
        assert!(fixture.run_script(&script).success());
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn sigkill_checkpoint_matrix_never_exposes_an_unreadable_generation() {
    let requested_checkpoint = std::env::var("SHIPYARD_TEST_AUTH_CHECKPOINT").ok();
    let checkpoints = [
        (
            "before-generation-rename",
            "/bin/mv \"$auth_generation_stage\" \"$auth_generation\"\n",
            true,
        ),
        (
            "after-generation-rename",
            "/bin/mv \"$auth_generation_stage\" \"$auth_generation\"\n",
            false,
        ),
        (
            "before-anchor-selector",
            "auth_publish_link \"$auth_selector\" \"$auth_anchor/ghapp\"\n",
            true,
        ),
        (
            "after-anchor-selector",
            "auth_publish_link \"$auth_selector\" \"$auth_anchor/ghapp\"\n",
            false,
        ),
        (
            "after-anchor-trampoline",
            "auth_publish_file \"$auth_wrapper\" \"$auth_generation/ghapp.public-trampoline\"\n  auth_public_trampoline_active=1\n",
            false,
        ),
        (
            "after-close-guard-projection",
            "auth_publish_link \"$auth_close_guard\" \"$auth_generation/pr-close-guard\"\n",
            false,
        ),
        (
            "after-helper-projection",
            "auth_publish_link \"$auth_helper\" \"$auth_generation/shipyard-github-app-token\"\n",
            false,
        ),
        (
            "after-binary-projection",
            "auth_publish_link \"$auth_binary\" \"$auth_generation/shipyard\"\n",
            false,
        ),
        (
            "after-provider-projection",
            "auth_publish_link \"$auth_companion\" \"$auth_generation/shipyard-workstream-provider\"; fi\n",
            false,
        ),
        (
            "after-context-projection",
            "auth_publish_link \"$auth_context\" \"$auth_generation/ghapp.shipyard-context.json\"\n",
            false,
        ),
        (
            "after-projections-journal",
            "auth_write_phase projections-published\n",
            false,
        ),
        (
            "before-target-selector",
            "auth_publish_link \"$auth_selector\" \"$auth_generation/ghapp\"\n",
            true,
        ),
        (
            "after-target-selector",
            "auth_publish_link \"$auth_selector\" \"$auth_generation/ghapp\"\n",
            false,
        ),
        (
            "after-public-trampoline",
            "if [ \"$auth_public_trampoline_active\" = 0 ]; then auth_publish_file \"$auth_wrapper\" \"$auth_generation/ghapp.public-trampoline\"; fi\n",
            false,
        ),
        (
            "after-target-journal",
            "auth_write_phase target-selected\n",
            false,
        ),
        (
            "after-validation-intent",
            "auth_write_phase validation-intent\n",
            false,
        ),
        ("after-validated", "auth_write_phase validated\n", false),
        (
            "after-commit-journal",
            "auth_write_phase committed\n",
            false,
        ),
        (
            "after-commit-cleanup",
            "auth_generation_created=0\ntrap - ERR INT TERM\nauth_cleanup_markers \"$auth_helper\" \"$auth_wrapper\" \"$auth_binary\" \"$auth_companion\" \"$auth_context\" \"$auth_close_guard\"\n",
            false,
        ),
    ];

    let checkpoints = checkpoints
        .into_iter()
        .filter(|(name, _, _)| {
            requested_checkpoint
                .as_deref()
                .is_none_or(|requested| requested == *name)
        })
        .collect::<Vec<_>>();
    assert!(
        requested_checkpoint.is_none() || !checkpoints.is_empty(),
        "requested auth checkpoint must name a matrix entry"
    );

    for chunk in checkpoints.chunks(4) {
        thread::scope(|scope| {
            for &(name, needle, kill_before) in chunk {
                scope.spawn(move || run_sigkill_checkpoint(name, needle, kill_before));
            }
        });
    }
}

fn run_sigkill_checkpoint(name: &str, needle: &str, kill_before: bool) {
    let fixture = ReaderFixture::new();
    fixture.install_direct_legacy_reader();
    assert_eq!(fixture.read_once().expect("legacy read"), "reader-bin");
    let script = fixture.transaction_script();
    assert_eq!(
        script.matches(needle).count(),
        1,
        "checkpoint {name} must identify one exact transition"
    );
    let replacement = if kill_before {
        format!("/bin/kill -9 $$\n{needle}")
    } else {
        format!("{needle}/bin/kill -9 $$\n")
    };
    let interrupted = script.replacen(needle, &replacement, 1);
    assert!(
        !fixture.run_script(&interrupted).success(),
        "checkpoint {name} unexpectedly completed"
    );
    ReaderFixture::assert_valid_reader_value(
        &fixture
            .read_once()
            .unwrap_or_else(|error| panic!("checkpoint {name} reader failed: {error}")),
    );
    let successor = fixture.run_script_traced(&script);
    assert!(
        successor.status.success(),
        "checkpoint {name} successor recovery failed; lock_exists={} pid={:?}:\n{}",
        fixture.state().join("fleet-auth-support.lock").exists(),
        std::fs::read_to_string(fixture.state().join("fleet-auth-support.lock/pid")),
        String::from_utf8_lossy(&successor.stderr),
    );
    ReaderFixture::assert_valid_reader_value(
        &fixture
            .read_once()
            .unwrap_or_else(|error| panic!("checkpoint {name} recovered reader failed: {error}")),
    );
}

#[test]
fn rollback_sigkill_checkpoint_matrix_never_exposes_mixed_generation() {
    let targets = [
        ("selector", "$auth_selector"),
        ("context", "$auth_context"),
        ("companion", "$auth_companion"),
        ("binary", "$auth_binary"),
        ("helper", "$auth_helper"),
        ("close-guard", "$auth_close_guard"),
    ];

    for (name, target) in targets {
        run_rollback_sigkill_checkpoint(name, target);
    }
}

#[test]
fn rollback_cleanup_is_restart_safe_after_backup_removal_begins() {
    let mut fixture = ReaderFixture::new();
    fixture.install_direct_legacy_reader();
    assert!(fixture.run_script(&fixture.transaction_script()).success());
    fixture.update_release("rollback-cleanup");
    let script = fixture.transaction_script();
    let probe = "\"$auth_binary\" --mode \"$auth_mode\" --global-dir \"$auth_global_dir\" auth helper-argv --wrapper \"$auth_wrapper\" --repo \"$auth_probe_repo\" >/dev/null\n";
    assert_eq!(script.matches(probe).count(), 1);
    let rollback = script.replacen(probe, "/usr/bin/false\n", 1);
    let cleanup_phase = "auth_write_recovery_phase rollback-complete\n";
    assert_eq!(rollback.matches(cleanup_phase).count(), 2);
    let interrupted = rollback.replacen(
        cleanup_phase,
        &format!(
            "{cleanup_phase}/bin/rm -f \"$auth_close_guard.shipyard-rollback\"\n/bin/kill -9 $$\n"
        ),
        1,
    );

    assert!(!fixture.run_script(&interrupted).success());
    ReaderFixture::assert_valid_reader_value(&fixture.read_once().expect("restored reader"));
    assert!(fixture.run_script(&script).success());
    ReaderFixture::assert_valid_reader_value(&fixture.read_once().expect("successor reader"));
}

fn run_rollback_sigkill_checkpoint(name: &str, target: &str) {
    let mut fixture = ReaderFixture::new();
    fixture.install_direct_legacy_reader();
    assert!(fixture.run_script(&fixture.transaction_script()).success());
    let original = fixture.read_once().expect("original generation");
    fixture.update_release(&format!("rollback-{name}"));
    let script = fixture.transaction_script();

    let restore = "/bin/mv -f \"$auth_restore_tmp\" \"$auth_target\"\n";
    assert_eq!(script.matches(restore).count(), 1);
    let kill =
        format!("{restore}if [ \"$auth_target\" = \"{target}\" ]; then /bin/kill -9 $$; fi\n");
    let interrupted = script.replacen(restore, &kill, 1);
    let probe = "\"$auth_binary\" --mode \"$auth_mode\" --global-dir \"$auth_global_dir\" auth helper-argv --wrapper \"$auth_wrapper\" --repo \"$auth_probe_repo\" >/dev/null\n";
    assert_eq!(interrupted.matches(probe).count(), 1);
    let interrupted = interrupted.replacen(probe, "/usr/bin/false\n", 1);

    let fixture = Arc::new(fixture);
    let results = exercise_continuous_readers(&fixture, |fixture| {
        assert!(
            !fixture.run_script(&interrupted).success(),
            "rollback checkpoint {name} unexpectedly completed"
        );
        ReaderFixture::assert_valid_reader_value(
            &fixture
                .read_once()
                .unwrap_or_else(|error| panic!("rollback checkpoint {name}: {error}")),
        );
        let successor = fixture.run_script_traced(&script);
        assert!(
            successor.status.success(),
            "rollback checkpoint {name} successor recovery failed: {}",
            String::from_utf8_lossy(&successor.stderr)
        );
    });
    assert_all_readers_valid(&results);
    let successor = fixture.read_once().expect("successor generation");
    ReaderFixture::assert_valid_reader_value(&successor);
    assert_ne!(successor, original);
}
