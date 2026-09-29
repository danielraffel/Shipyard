use super::*;
use crate::identity::RuntimeMode;

#[derive(Default)]
struct FakeLaunchctl {
    calls: Vec<String>,
    fail_bootstrap: bool,
}

impl Launchctl for FakeLaunchctl {
    fn bootout(&mut self, label: &str) -> Result<(), String> {
        self.calls.push(format!("bootout {label}"));
        Ok(())
    }

    fn bootstrap(&mut self, plist: &Path) -> Result<(), String> {
        self.calls.push(format!("bootstrap {}", plist.display()));
        if self.fail_bootstrap {
            return Err("launchctl bootstrap exited 5: Input/output error".to_owned());
        }
        Ok(())
    }

    fn kickstart(&mut self, label: &str) -> Result<(), String> {
        self.calls.push(format!("kickstart {label}"));
        Ok(())
    }
}

fn request(state_dir: &Path) -> SpawnRequest {
    SpawnRequest {
        binary: PathBuf::from("/Users/ci/.local/bin/shipyard"),
        mode: RuntimeMode::Shipyard,
        global_dir_override: Some(PathBuf::from(
            "/Users/ci/Library/Application Support/shipyard",
        )),
        state_dir_override: Some(state_dir.to_path_buf()),
        state_dir: state_dir.to_path_buf(),
        repos: vec!["o/b".to_owned(), "o/a".to_owned(), "o/a".to_owned()],
    }
}

fn strings(arguments: &[OsString]) -> Vec<String> {
    arguments
        .iter()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect()
}

fn write_record(state_dir: &Path, launcher: &Path) -> LauncherRecord {
    let record = LauncherRecord {
        schema: RECORD_SCHEMA,
        launcher_path: launcher.to_path_buf(),
        launcher_sha256: sha256_file(launcher).expect("hash launcher"),
        label: launchd_label(state_dir),
        probed_paths: vec![PathBuf::from("/Volumes/Workshop")],
        installed_by_version: "0.0.0".to_owned(),
        installed_at: "2026-09-29T00:00:00Z".to_owned(),
    };
    fs::create_dir_all(state_dir.join("daemon")).expect("daemon dir");
    fs::write(
        record_path(state_dir),
        serde_json::to_vec(&record).expect("encode record"),
    )
    .expect("write record");
    record
}

#[test]
fn launcher_arguments_hand_the_daemon_the_same_argv_as_a_direct_spawn() {
    let state = Path::new("/Users/ci/Library/Application Support/shipyard");
    let request = request(state);
    let launcher = Path::new("/Users/ci/.local/libexec/shipyard/shipyard-daemon-launcher");
    let arguments = strings(&launcher_arguments(launcher, &request));

    let supervise = arguments
        .iter()
        .position(|argument| argument == "supervise")
        .expect("supervise subcommand");
    assert_eq!(arguments[0], launcher.to_string_lossy());
    assert_eq!(&arguments[supervise - 1], "daemon");
    assert_eq!(
        &arguments[supervise + 1..supervise + 3],
        ["--exec", "/Users/ci/.local/bin/shipyard"]
    );
    // The launcher is invoked with the same global flags the daemon child gets,
    // and the child argv is exactly what a direct spawn would build.
    let child = strings(&crate::daemon_runtime::daemon_run_arguments(&request));
    assert_eq!(arguments[1..supervise - 1], child[..child.len() - 6]);
    assert_eq!(
        child[child.len() - 6..],
        ["daemon", "run", "--repo", "o/a", "--repo", "o/b"]
    );
    assert_eq!(
        arguments[supervise + 3..],
        ["--repo", "o/a", "--repo", "o/b"]
    );
}

#[test]
fn label_is_stable_per_state_root_and_distinct_across_roots() {
    let production = Path::new("/Users/ci/Library/Application Support/shipyard");
    let dev = Path::new("/Users/ci/Library/Application Support/shipyard-dev");
    assert_eq!(launchd_label(production), launchd_label(production));
    assert_ne!(launchd_label(production), launchd_label(dev));
    assert!(launchd_label(production).starts_with("com.danielraffel.shipyard.daemon."));
}

#[test]
fn plist_escapes_values_and_never_keeps_the_daemon_alive() {
    let rendered = render_plist(
        "com.example.label",
        &["/a b/launcher".into(), "--repo".into(), "o/<&'\">".into()],
        Path::new("/tmp/launcher.log"),
        Path::new("/Users/ci"),
        OsStr::new("/usr/bin:/bin"),
        false,
    );
    assert!(rendered.contains("<string>o/&lt;&amp;&apos;&quot;&gt;</string>"));
    assert!(rendered.contains("<key>KeepAlive</key>\n\t<false/>"));
    assert!(rendered.contains("<key>RunAtLoad</key>\n\t<false/>"));
    assert!(rendered.contains("<key>AbandonProcessGroup</key>\n\t<true/>"));
    assert!(!rendered.contains("o/<&"));
}

#[cfg(target_os = "macos")]
#[test]
fn rendered_plist_is_valid_for_launchd() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("agent.plist");
    let rendered = render_plist(
        "com.example.label",
        &launcher_arguments(Path::new("/x/launcher"), &request(temp.path())),
        Path::new("/tmp/launcher.log"),
        Path::new("/Users/ci"),
        OsStr::new("/usr/bin:/bin"),
        true,
    );
    fs::write(&path, rendered).expect("write plist");
    let status = Command::new("/usr/bin/plutil")
        .arg("-lint")
        .arg(&path)
        .status()
        .expect("plutil");
    assert!(status.success(), "plutil rejected the rendered plist");
}

#[test]
fn active_launcher_requires_the_exact_probed_file() {
    let temp = tempfile::tempdir().expect("tempdir");
    let state = temp.path().join("state");
    let launcher = temp.path().join("launcher");
    fs::write(&launcher, b"signed launcher bytes").expect("launcher");

    assert_eq!(active_launcher(&state), None, "no record, no launcher");

    let record = write_record(&state, &launcher);
    assert_eq!(
        active_launcher(&state),
        Some(ActiveLauncher {
            path: launcher.clone(),
            label: record.label,
        })
    );

    // A replaced binary never passed the consent probe; fall back.
    fs::write(&launcher, b"different bytes").expect("replace launcher");
    assert_eq!(active_launcher(&state), None);

    // A symlink is not a stable privacy subject either.
    fs::write(temp.path().join("real"), b"signed launcher bytes").expect("real");
    fs::remove_file(&launcher).expect("remove");
    std::os::unix::fs::symlink(temp.path().join("real"), &launcher).expect("symlink");
    assert_eq!(active_launcher(&state), None);
}

#[test]
fn start_via_launchd_replaces_the_agent_then_kickstarts_it() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let state = temp.path().join("state");
    let launcher = ActiveLauncher {
        path: temp.path().join("launcher"),
        label: "com.example.daemon".to_owned(),
    };
    let mut launchctl = FakeLaunchctl::default();

    start_via_launchd_with(&request(&state), &launcher, &home, &mut launchctl).expect("start");

    let plist = plist_path(&home, "com.example.daemon");
    assert_eq!(
        launchctl.calls,
        [
            "bootout com.example.daemon".to_owned(),
            format!("bootstrap {}", plist.display()),
            "kickstart com.example.daemon".to_owned(),
        ]
    );
    let written = fs::read_to_string(&plist).expect("plist written");
    assert!(written.contains("<string>supervise</string>"));
    assert!(written.contains(&format!("<string>{}</string>", launcher.path.display())));
}

#[test]
fn start_via_launchd_reports_a_bootstrap_failure_without_kickstarting() {
    let temp = tempfile::tempdir().expect("tempdir");
    let launcher = ActiveLauncher {
        path: temp.path().join("launcher"),
        label: "com.example.daemon".to_owned(),
    };
    let mut launchctl = FakeLaunchctl {
        fail_bootstrap: true,
        ..FakeLaunchctl::default()
    };
    let error = start_via_launchd_with(
        &request(&temp.path().join("state")),
        &launcher,
        &temp.path().join("home"),
        &mut launchctl,
    )
    .expect_err("bootstrap failure");
    assert!(error.contains("bootstrap"));
    assert!(
        !launchctl
            .calls
            .iter()
            .any(|call| call.starts_with("kickstart"))
    );
}

#[test]
fn probe_records_readable_and_unreadable_paths() {
    let temp = tempfile::tempdir().expect("tempdir");
    let readable = temp.path().join("volume");
    fs::create_dir(&readable).expect("volume");
    fs::write(readable.join("file"), b"x").expect("file");
    let result = temp.path().join("result.json");

    assert!(probe(std::slice::from_ref(&readable), &result).expect("probe"));
    let missing = temp.path().join("missing");
    assert!(!probe(&[readable.clone(), missing.clone()], &result).expect("probe"));
    let entries = read_probe_result(&result).expect("result");
    assert!(entries[0].ok);
    assert!(!entries[1].ok);
    assert_eq!(entries[1].path, missing);
    assert!(
        !probe(&[], &result).expect("probe"),
        "nothing probed is not consent"
    );
}

#[test]
fn default_probe_paths_only_names_an_external_volume_root() {
    assert!(default_probe_paths(Path::new("/Users/ci/Code/pulp")).is_empty());
    assert!(default_probe_paths(Path::new("/Volumes")).is_empty());
    assert!(default_probe_paths(Path::new("/Volumes/definitely-not-mounted-xyz/a")).is_empty());
}

fn install_plan<'a>(
    temp: &Path,
    launchctl: &'a mut FakeLaunchctl,
    probe_paths: Vec<PathBuf>,
    wait: Duration,
) -> InstallPlan<'a> {
    let source = temp.join("source-shipyard");
    fs::write(&source, b"release binary").expect("source");
    InstallPlan {
        source,
        home: temp.join("home"),
        state_dir: temp.join("state"),
        mode_arguments: vec!["--mode".into(), "shipyard".into()],
        probe_paths,
        wait,
        launchctl,
    }
}

#[test]
fn install_activates_only_after_the_launchd_probe_reads_the_volume() {
    let temp = tempfile::tempdir().expect("tempdir");
    let volume = temp.path().join("volume");
    fs::create_dir(&volume).expect("volume");
    let mut launchctl = ProbeRunningLaunchctl::default();
    let plan = InstallPlan {
        source: {
            let source = temp.path().join("source-shipyard");
            fs::write(&source, b"release binary").expect("source");
            source
        },
        home: temp.path().join("home"),
        state_dir: temp.path().join("state"),
        mode_arguments: vec!["--mode".into(), "shipyard".into()],
        probe_paths: vec![volume.clone()],
        wait: Duration::from_secs(5),
        launchctl: &mut launchctl,
    };
    let mut progress = Vec::new();
    let mut plan = plan;
    let record = install(&mut plan, &mut progress).expect("install");

    let launcher = stable_launcher_path(&temp.path().join("home"));
    assert_eq!(record.launcher_path, launcher);
    assert_eq!(fs::read(&launcher).expect("copied"), b"release binary");
    assert_eq!(read_record(&temp.path().join("state")), Some(record));
    assert!(active_launcher(&temp.path().join("state")).is_some());
    assert!(String::from_utf8_lossy(&progress).contains("approve it"));
    assert!(
        launchctl
            .arguments
            .iter()
            .any(|argument| argument == "launcher-probe"),
        "the probe runs the launcher copy's hidden probe subcommand"
    );
    assert_eq!(launchctl.arguments[0], launcher.to_string_lossy());
}

#[test]
fn install_times_out_without_activating_when_consent_is_pending() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut launchctl = FakeLaunchctl::default();
    let plan = install_plan(
        temp.path(),
        &mut launchctl,
        vec![temp.path().to_path_buf()],
        Duration::from_millis(300),
    );
    let mut plan = plan;
    let error = install(&mut plan, &mut Vec::new()).expect_err("no probe result");
    assert!(error.contains("privacy prompt"), "{error}");
    assert_eq!(read_record(&temp.path().join("state")), None);
    assert_eq!(active_launcher(&temp.path().join("state")), None);
    assert!(
        launchctl
            .calls
            .last()
            .is_some_and(|call| call.starts_with("bootout")),
        "the probe job is unloaded"
    );
}

#[test]
fn install_refuses_without_a_volume_to_probe() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut launchctl = FakeLaunchctl::default();
    let plan = install_plan(temp.path(), &mut launchctl, Vec::new(), Duration::ZERO);
    let mut plan = plan;
    assert!(install(&mut plan, &mut Vec::new()).is_err());
    assert!(launchctl.calls.is_empty());
}

#[test]
fn uninstall_removes_the_record_and_agent_but_keeps_the_launcher() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let state = temp.path().join("state");
    let launcher = stable_launcher_path(&home);
    fs::create_dir_all(launcher.parent().expect("parent")).expect("dir");
    fs::write(&launcher, b"bytes").expect("launcher");
    let record = write_record(&state, &launcher);
    let plist = plist_path(&home, &record.label);
    fs::create_dir_all(plist.parent().expect("parent")).expect("agents");
    fs::write(&plist, b"plist").expect("plist");

    assert!(uninstall(&state, &home).expect("uninstall"));
    assert_eq!(read_record(&state), None);
    assert!(!plist.exists());
    assert!(
        launcher.exists(),
        "keeping the file keeps its recorded consent"
    );
    assert!(!uninstall(&state, &home).expect("second uninstall"));
}

/// A launchctl double that behaves like launchd for the probe: it parses the
/// bootstrapped plist's `ProgramArguments` and runs the probe they describe.
#[derive(Default)]
struct ProbeRunningLaunchctl {
    arguments: Vec<String>,
}

impl Launchctl for ProbeRunningLaunchctl {
    fn bootout(&mut self, _label: &str) -> Result<(), String> {
        Ok(())
    }

    fn bootstrap(&mut self, plist: &Path) -> Result<(), String> {
        let text = fs::read_to_string(plist).map_err(|error| error.to_string())?;
        let array = text
            .split("<key>ProgramArguments</key>")
            .nth(1)
            .and_then(|rest| rest.split("</array>").next())
            .ok_or("no ProgramArguments")?;
        self.arguments = array
            .split("<string>")
            .skip(1)
            .filter_map(|chunk| chunk.split("</string>").next())
            .map(str::to_owned)
            .collect();
        let mut paths = Vec::new();
        let mut result = None;
        let mut iter = self.arguments.iter();
        while let Some(argument) = iter.next() {
            match argument.as_str() {
                "--path" => paths.push(PathBuf::from(iter.next().ok_or("path value")?)),
                "--result" => result = Some(PathBuf::from(iter.next().ok_or("result value")?)),
                _ => {}
            }
        }
        probe(&paths, &result.ok_or("no --result")?).map_err(|error| error.to_string())?;
        Ok(())
    }

    fn kickstart(&mut self, _label: &str) -> Result<(), String> {
        Ok(())
    }
}

#[test]
fn supervise_passes_signals_through_to_the_daemon_child() {
    let temp = tempfile::tempdir().expect("tempdir");
    let state = temp.path().join("state");
    let pid_file = temp.path().join("child.pid");
    let marker = temp.path().join("terminated");
    let script = temp.path().join("fake-daemon");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\ntrap 'echo term > \"{marker}\"; exit 7' TERM\necho $$ > \"{pid}\"\nwhile :; do sleep 0.05; done\n",
            marker = marker.display(),
            pid = pid_file.display()
        ),
    )
    .expect("script");
    set_mode(&script, 0o755).expect("chmod");
    let request = SpawnRequest {
        binary: script,
        mode: RuntimeMode::Shipyard,
        global_dir_override: Some(temp.path().join("global")),
        state_dir_override: Some(state.clone()),
        state_dir: state,
        repos: Vec::new(),
    };

    let (sender, receiver) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let _ = sender.send(supervise(&request));
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    let child_pid = loop {
        if let Some(pid) = fs::read_to_string(&pid_file)
            .ok()
            .and_then(|text| text.trim().parse::<i32>().ok())
        {
            break pid;
        }
        assert!(Instant::now() < deadline, "fake daemon never started");
        thread::sleep(Duration::from_millis(20));
    };
    // The supervisor blocks these signals for itself; the daemon child must
    // not inherit that mask, or it could never be stopped.
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(child_pid),
        nix::sys::signal::Signal::SIGTERM,
    )
    .expect("signal child");
    let Ok(outcome) = receiver.recv_timeout(Duration::from_secs(20)) else {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(child_pid),
            nix::sys::signal::Signal::SIGKILL,
        );
        panic!("the daemon child ignored SIGTERM: it inherited a blocked signal mask");
    };
    let code = outcome.expect("supervise");
    assert_eq!(code, 7, "the daemon's exit code is the supervisor's");
    assert_eq!(
        fs::read_to_string(&marker).expect("trap ran").trim(),
        "term"
    );
}
