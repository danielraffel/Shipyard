use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

use tempfile::TempDir;

use super::*;
use crate::config::LocalOverlaySource;

#[cfg(unix)]
fn write_executable(path: &Path, contents: &str) {
    use std::os::unix::fs::PermissionsExt;

    std::fs::write(path, contents).expect("write script");
    let mut permissions = std::fs::metadata(path).expect("metadata").permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).expect("chmod script");
}

fn config_from_toml(input: &str) -> LoadedConfig {
    LoadedConfig {
        data: input.parse::<toml::Table>().expect("parse toml"),
        global_dir: PathBuf::from("/tmp/shipyard-global"),
        project_dir: None,
        local_dir: None,
        local_overlay_source: LocalOverlaySource::None,
    }
}

fn env_value(command: &Command, key: &str) -> Option<OsString> {
    command
        .get_envs()
        .find_map(|(name, value)| (name == key).then(|| value.map(std::ffi::OsStr::to_owned))?)
}

fn env_is_removed(command: &Command, key: &str) -> bool {
    command
        .get_envs()
        .any(|(name, value)| name == key && value.is_none())
}

#[test]
fn missing_config_uses_ambient_auth() {
    let config = config_from_toml("");
    let client = GhClient::from_loaded_config(&config).expect("client");
    assert_eq!(client.auth.source, GhAuthSource::GhCli);
    assert_eq!(client.auth.ambient_gh_binary, None);
    assert_eq!(client.auth.privileged_gh_binary, None);
    assert_eq!(client.auth.privileged_git_binary, None);
}

#[test]
fn parses_env_auth_config() {
    let config = config_from_toml(
        r#"
            [github.auth]
            source = "env"
            token_env = "SHIPYARD_GITHUB_TOKEN"
            "#,
    );
    let client = GhClient::from_loaded_config(&config).expect("client");
    assert_eq!(
        client.auth.source,
        GhAuthSource::Env {
            token_env: "SHIPYARD_GITHUB_TOKEN".to_owned()
        }
    );
}

#[test]
fn rejects_token_settings_without_source() {
    let config = config_from_toml(
        r#"
            [github.auth]
            token_env = "SHIPYARD_GITHUB_TOKEN"
            "#,
    );
    let error = GhClient::from_loaded_config(&config).expect_err("invalid config");
    assert!(error.to_string().contains("source"));
}

#[test]
fn rejects_empty_command_auth_config() {
    let config = config_from_toml(
        r#"
            [github.auth]
            source = "command"
            token_command = []
            "#,
    );
    let error = GhClient::from_loaded_config(&config).expect_err("invalid config");
    assert!(error.to_string().contains("token_command"));
}

#[cfg(unix)]
#[test]
fn bounded_command_preparation_times_out_token_helper() {
    let temp = TempDir::new().expect("temp");
    let helper = temp.path().join("token-helper");
    write_executable(&helper, "#!/bin/sh\nsleep 5\nprintf token\n");
    let config = config_from_toml(&format!(
        r#"
            [github.auth]
            source = "command"
            token_command = ["{}"]
            "#,
        helper.display()
    ));
    let client = GhClient::from_loaded_config(&config).expect("client");
    let started = std::time::Instant::now();
    let error = client
        .prepare_command_with_auth_timeout(
            temp.path(),
            Some(Path::new("/tmp/fake-gh")),
            GhSupervision::Unsupervised,
            GhAuthPolicy::Default,
            Duration::from_millis(20),
        )
        .expect_err("helper timeout");
    assert!(matches!(error, GhPrepareError::HelperTimedOut { .. }));
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[cfg(unix)]
#[test]
fn merged_pr_observation_uses_configured_auth_and_explicit_repository() {
    let temp = TempDir::new().expect("temp");
    let fake_gh = temp.path().join("gh");
    let invocation = temp.path().join("invocation");
    write_executable(
        &fake_gh,
        &format!(
            r#"#!/bin/sh
printf '%s\n%s' "$GH_TOKEN" "$*" > '{}'
printf '%s' '{{"state":"MERGED","headRefOid":"abc123"}}'
"#,
            invocation.display()
        ),
    );
    let config = config_from_toml(
        r#"
            [github.auth]
            source = "env"
            token_env = "PATH"
            "#,
    );
    let client = GhClient::from_loaded_config(&config).expect("client");

    let merged = pr_merged_head_sha_with_options(
        Some(&client),
        "owner/repo",
        42,
        temp.path(),
        None,
        Some(&fake_gh),
        Duration::from_secs(30),
    );

    assert_eq!(merged.as_deref(), Some("abc123"));
    let invocation = std::fs::read_to_string(invocation).expect("invocation");
    let (token, args) = invocation.split_once('\n').expect("token and args");
    assert!(!token.is_empty());
    assert_eq!(args, "pr view 42 --repo owner/repo --json state,headRefOid");
}

#[cfg(unix)]
#[test]
fn merged_pr_observation_kills_a_hung_github_command() {
    let temp = TempDir::new().expect("temp");
    let fake_gh = temp.path().join("gh");
    write_executable(&fake_gh, "#!/bin/sh\nsleep 5\n");
    let client = GhClient::ambient();
    let started = std::time::Instant::now();

    let merged = pr_merged_head_sha_with_options(
        Some(&client),
        "owner/repo",
        42,
        temp.path(),
        None,
        Some(&fake_gh),
        Duration::from_millis(20),
    );

    assert_eq!(merged, None);
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn merged_pr_snapshot_never_invokes_external_github_command() {
    let temp = TempDir::new().expect("temp");
    let snapshot = temp.path().join("merged.json");
    std::fs::write(&snapshot, r#"{"state":"MERGED","headRefOid":"abc123"}"#).expect("snapshot");

    let merged = pr_merged_head_sha_with_options(
        Some(&GhClient::ambient()),
        "owner/repo",
        42,
        temp.path(),
        Some(&snapshot),
        Some(Path::new("/definitely/missing/gh-must-not-run")),
        Duration::from_millis(1),
    );

    assert_eq!(merged.as_deref(), Some("abc123"));
}

#[cfg(unix)]
#[test]
fn bounded_helper_receives_eof_instead_of_inheriting_caller_stdin() {
    let temp = TempDir::new().expect("temp");
    let helper = temp.path().join("token-helper");
    write_executable(&helper, "#!/bin/sh\nread ignored || true\nprintf token\n");
    // Execute the just-written fixture through the immutable system shell.
    // Directly exec'ing a mutable temp script can race macOS/coverage file
    // instrumentation and fail with ETXTBSY before this stdin invariant is
    // exercised.
    let mut command = Command::new("/bin/sh");
    command.arg(&helper);
    // A caller-provided pipe would remain open in the parent and make the
    // helper block on `read` unless the bounded boundary replaces stdin.
    command.stdin(Stdio::piped());

    // Keep the assertion about EOF, not scheduler latency in the highly
    // parallel full suite. The helper returns immediately on the good path.
    let output = run_helper_with_timeout(&mut command, "token-helper", Duration::from_secs(30))
        .expect("helper should see EOF");

    assert!(output.status.success());
    assert_eq!(output.stdout, b"token");
}

#[cfg(unix)]
#[test]
fn successful_bounded_helper_does_not_leave_detached_descendants() {
    let temp = TempDir::new().expect("temp");
    let helper = temp.path().join("token-helper");
    let descendant_pid = temp.path().join("descendant.pid");
    write_executable(
        &helper,
        &format!(
            "#!/bin/sh\nsleep 120 </dev/null >/dev/null 2>&1 &\nprintf '%s\\n' \"$!\" > '{}'\nprintf token\n",
            descendant_pid.display()
        ),
    );
    let mut command = Command::new("/bin/sh");
    command.arg(&helper);

    // Full macOS CI runs many process-heavy tests concurrently. Keep this
    // boundary comfortably above scheduler latency while the descendant's
    // 120-second lifetime still proves cleanup rather than natural exit.
    let output = run_helper_with_timeout(&mut command, "token-helper", Duration::from_secs(30))
        .expect("helper succeeds");
    let pid = std::fs::read_to_string(&descendant_pid).expect("descendant pid");
    let pid = pid.trim();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut running = true;
    while running && std::time::Instant::now() < deadline {
        running = Command::new("kill")
            .args(["-0", pid])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if running {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    if running {
        let _ = Command::new("kill")
            .args(["-KILL", pid])
            .stderr(Stdio::null())
            .status();
    }

    assert!(output.status.success());
    assert_eq!(output.stdout, b"token");
    assert!(!running, "helper descendant {pid} survived success");
}

#[test]
fn prepare_command_injects_env_token_and_supervised_marker() {
    let config = config_from_toml(
        r#"
            [github.auth]
            source = "env"
            token_env = "PATH"
            "#,
    );
    let expected_path = env::var("PATH").expect("PATH");
    let client = GhClient::from_loaded_config(&config).expect("client");
    let native = std::env::current_exe().expect("native test executable");
    let command = client
        .prepare_command(
            Path::new("/tmp"),
            Some(&native),
            GhSupervision::Supervised,
            GhAuthPolicy::Default,
        )
        .expect("command");

    assert_eq!(
        command.get_program(),
        native.canonicalize().expect("canonical native executable")
    );
    assert_eq!(
        env_value(&command, GH_TOKEN_ENV).as_deref(),
        Some(OsString::from(expected_path).as_os_str())
    );
    assert_eq!(
        env_value(&command, crate::supervised::SUPERVISED_ENV_VAR).as_deref(),
        Some(OsString::from(crate::supervised::SUPERVISED_ENV_VALUE).as_os_str())
    );
}

#[test]
fn ambient_only_does_not_inject_configured_token() {
    let config = config_from_toml(
        r#"
            [github.auth]
            source = "env"
            token_env = "PATH"
            "#,
    );
    let client = GhClient::from_loaded_config(&config).expect("client");
    let native = std::env::current_exe().expect("native test executable");
    let command = client
        .prepare_command(
            Path::new("/tmp"),
            Some(&native),
            GhSupervision::Unsupervised,
            GhAuthPolicy::AmbientOnly,
        )
        .expect("command");
    assert_eq!(
        command.get_program(),
        native.canonicalize().expect("canonical native executable")
    );
    assert!(env_is_removed(&command, GH_TOKEN_ENV));
    assert!(env_is_removed(&command, "GITHUB_TOKEN"));
}

#[test]
fn ambient_only_rejects_a_script_binary_override() {
    let temp = TempDir::new().expect("tempdir");
    let wrapper = temp
        .path()
        .join(if cfg!(windows) { "gh.exe" } else { "gh" });
    std::fs::write(&wrapper, b"#!/bin/sh\nexit 91\n").expect("wrapper fixture");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&wrapper)
            .expect("wrapper metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&wrapper, permissions).expect("executable wrapper");
    }

    let error = GhClient::ambient()
        .prepare_command(
            temp.path(),
            Some(&wrapper),
            GhSupervision::Unsupervised,
            GhAuthPolicy::AmbientOnly,
        )
        .expect_err("ambient override must be native");

    assert!(matches!(
        error,
        GhPrepareError::InvalidAmbientGhBinary { .. }
    ));
}

#[cfg(unix)]
#[test]
fn token_bearing_command_rejects_a_path_shim_binary() {
    let temp = TempDir::new().expect("tempdir");
    let wrapper = temp.path().join("gh");
    write_executable(&wrapper, "#!/bin/sh\nexit 91\n");
    let config = config_from_toml(&format!(
        r#"
            [github.auth]
            source = "command"
            token_command = ["/bin/echo", "ghs_app_token"]
            privileged_gh_binary = "{}"
            "#,
        wrapper.display()
    ));
    let client = GhClient::from_loaded_config(&config).expect("client");

    let error = client
        .prepare_privileged_command(temp.path(), GhSupervision::Unsupervised)
        .expect_err("token-bearing command must reject a script shim");

    assert!(matches!(
        error,
        GhPrepareError::InvalidPrivilegedGhBinary { .. }
    ));
}

#[cfg(unix)]
#[test]
fn privileged_git_uses_the_exact_configured_native_binary() {
    let temp = TempDir::new().expect("tempdir");
    let native_dir = temp.path().join("native");
    std::fs::create_dir_all(&native_dir).expect("native dir");
    let native_git = native_dir.join("git");
    std::os::unix::fs::symlink("/bin/echo", &native_git).expect("native git fixture");
    let config = config_from_toml(&format!(
        r#"
            [github.auth]
            source = "command"
            token_command = ["/bin/echo", "ghs_app_token"]
            privileged_git_binary = "{}"
            "#,
        native_git.display()
    ));
    let client = GhClient::from_loaded_config(&config).expect("client");

    let command = client
        .prepare_git_command(temp.path())
        .expect("privileged git command");

    assert_eq!(
        command.get_program(),
        native_git.canonicalize().expect("canonical native git")
    );
}

#[cfg(unix)]
#[test]
fn authenticated_git_can_take_its_binary_only_from_a_separate_authority() {
    let temp = TempDir::new().expect("tempdir");
    let native_env = Path::new("/usr/bin/env");
    let primary = GhClient::from_loaded_config(&config_from_toml(
        r#"
            [github.auth]
            source = "command"
            token_command = ["/bin/echo", "ghs_primary_token"]
            "#,
    ))
    .expect("primary auth client");
    let binary_authority = GhClient::from_loaded_config(&config_from_toml(&format!(
        r#"
            [github.auth]
            source = "command"
            token_command = ["/bin/echo", "ghs_unrelated_token"]
            privileged_git_binary = "{}"
            "#,
        native_env.display()
    )))
    .expect("binary authority client");

    let output = primary
        .prepare_git_command_with_binary_authority(temp.path(), &binary_authority)
        .expect("separated Git command")
        .output()
        .expect("Git environment");
    assert!(output.status.success());
    let environment = String::from_utf8(output.stdout).expect("environment UTF-8");
    assert!(environment.contains("GH_TOKEN=ghs_primary_token"));
    assert!(!environment.contains("ghs_unrelated_token"));
}

#[cfg(unix)]
#[test]
fn privileged_token_children_receive_only_allowlisted_environment() {
    let temp = TempDir::new().expect("tempdir");
    let native_env = Path::new("/usr/bin/env");
    let config = config_from_toml(&format!(
        r#"
            [github.auth]
            source = "command"
            token_command = ["/bin/echo", "ghs_app_token"]
            privileged_gh_binary = "{}"
            privileged_git_binary = "{}"
            "#,
        native_env.display(),
        native_env.display()
    ));
    let client = GhClient::from_loaded_config(&config).expect("client");

    let gh = client
        .prepare_privileged_command(temp.path(), GhSupervision::Unsupervised)
        .expect("privileged gh")
        .output()
        .expect("gh environment");
    assert!(gh.status.success());
    let gh = String::from_utf8(gh.stdout).expect("gh environment UTF-8");
    assert!(gh.contains("GH_TOKEN=ghs_app_token"));
    assert!(gh.contains("GH_HOST=github.com"));
    assert!(!gh.contains("HOME="));
    assert!(!gh.contains("PATH="));
    assert!(!gh.contains("LD_AUDIT="));
    assert!(!gh.contains("DYLD_FRAMEWORK_PATH="));
    assert!(!gh.contains("SSL_CERT_FILE="));
    assert!(!gh.contains("HTTPS_PROXY="));

    let git = client
        .prepare_git_command(temp.path())
        .expect("privileged git")
        .output()
        .expect("git environment");
    assert!(git.status.success());
    let git = String::from_utf8(git.stdout).expect("git environment UTF-8");
    assert!(git.contains("GH_TOKEN=ghs_app_token"));
    assert!(git.contains("GIT_CONFIG_NOSYSTEM=1"));
    assert!(!git.contains("HOME="));
    assert!(!git.contains("PATH="));
    assert!(!git.contains("LD_AUDIT="));
    assert!(!git.contains("DYLD_FRAMEWORK_PATH="));
    assert!(!git.contains("SSL_CERT_FILE="));
    assert!(!git.contains("HTTPS_PROXY="));
}

#[cfg(unix)]
#[test]
fn ambient_resolution_skips_ghapp_script_shim_for_native_gh() {
    let temp = TempDir::new().expect("tempdir");
    let shim_dir = temp.path().join("shim");
    let native_dir = temp.path().join("native");
    std::fs::create_dir_all(&shim_dir).expect("shim dir");
    std::fs::create_dir_all(&native_dir).expect("native dir");
    write_executable(
        &shim_dir.join("gh"),
        "#!/bin/sh\nprintf 'ghapp wrapper must not run' >&2\nexit 91\n",
    );
    let native_gh = native_dir.join("gh");
    std::os::unix::fs::symlink("/bin/echo", &native_gh).expect("native gh fixture");
    let path = env::join_paths([shim_dir, native_dir]).expect("PATH fixture");

    let resolved = resolve_ambient_gh_from_path(Some(&path)).expect("native gh");

    assert_eq!(
        resolved,
        native_gh.canonicalize().expect("canonical native gh")
    );
}

#[cfg(unix)]
#[test]
fn configured_ambient_binary_outranks_hostile_path_and_removes_tokens() {
    let temp = TempDir::new().expect("tempdir");
    let shim_dir = temp.path().join("shim");
    let native_dir = temp.path().join("native");
    std::fs::create_dir_all(&shim_dir).expect("shim dir");
    std::fs::create_dir_all(&native_dir).expect("native dir");
    let marker = temp.path().join("shim-ran");
    write_executable(
        &shim_dir.join("gh"),
        &format!("#!/bin/sh\ntouch '{}'\nexit 91\n", marker.display()),
    );
    let native_gh = native_dir.join("gh");
    std::os::unix::fs::symlink("/bin/echo", &native_gh).expect("native gh fixture");
    let config = config_from_toml(&format!(
        r#"
            [github.auth]
            source = "env"
            token_env = "PATH"
            ambient_gh_binary = "{}"
            "#,
        native_gh.display()
    ));
    let client = GhClient::from_loaded_config(&config).expect("client");
    let hostile_path = env::join_paths([shim_dir, native_dir]).expect("PATH fixture");
    let mut command = client
        .prepare_command(
            temp.path(),
            None,
            GhSupervision::Unsupervised,
            GhAuthPolicy::AmbientOnly,
        )
        .expect("ambient command");
    command.env("PATH", hostile_path).arg("api-proof");

    let output = command.output().expect("run native fixture");

    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "api-proof");
    assert!(!marker.exists(), "ghapp-compatible shim ran");
}

#[cfg(unix)]
#[test]
fn configured_ambient_binary_rejects_script_wrappers() {
    let temp = TempDir::new().expect("tempdir");
    let wrapper = temp.path().join("gh");
    write_executable(&wrapper, "#!/bin/sh\nexit 0\n");
    let config = config_from_toml(&format!(
        r#"
            [github.auth]
            source = "command"
            token_command = ["/bin/echo", "token"]
            ambient_gh_binary = "{}"
            "#,
        wrapper.display()
    ));
    let client = GhClient::from_loaded_config(&config).expect("client");

    let error = client
        .prepare_command(
            temp.path(),
            None,
            GhSupervision::Unsupervised,
            GhAuthPolicy::AmbientOnly,
        )
        .expect_err("script wrapper rejected");

    assert!(matches!(
        error,
        GhPrepareError::InvalidAmbientGhBinary { .. }
    ));
}

#[test]
fn configured_ambient_binary_must_be_absolute() {
    let config = config_from_toml(
        r#"
            [github.auth]
            source = "gh-cli"
            ambient_gh_binary = "relative/gh"
            "#,
    );

    let error = GhClient::from_loaded_config(&config).expect_err("relative path rejected");

    assert!(error.to_string().contains("must be an absolute path"));
}

#[test]
fn privileged_binary_paths_must_be_explicit_and_absolute() {
    let client = GhClient::ambient();
    let error = client
        .prepare_privileged_command(Path::new("/tmp"), GhSupervision::Unsupervised)
        .expect_err("privileged gh path is required");
    assert!(matches!(
        error,
        GhPrepareError::PrivilegedGhBinaryNotConfigured
    ));

    let config = config_from_toml(
        r#"
            [github.auth]
            privileged_gh_binary = "relative/gh"
            privileged_git_binary = "relative/git"
            "#,
    );
    let error = GhClient::from_loaded_config(&config).expect_err("relative path rejected");
    assert!(error.to_string().contains("must be an absolute path"));
}

#[test]
fn parses_plain_helper_stdout_with_ttl() {
    let now = Utc::now();
    let token = parse_helper_stdout("ghp_plain\n", now, Some(300), DEFAULT_REFRESH_SKEW_SECONDS)
        .expect("token");
    assert_eq!(token.token, "ghp_plain");
    assert!(
        token
            .valid_until
            .is_some_and(|valid_until| valid_until > now)
    );
}

#[test]
fn infers_installation_kind_from_plain_github_app_token() {
    let now = Utc::now();
    let token = parse_helper_stdout(
        "ghs_installation-token\n",
        now,
        None,
        DEFAULT_REFRESH_SKEW_SECONDS,
    )
    .expect("token");
    assert_eq!(token.kind.as_deref(), Some("github-app-installation"));
}

#[test]
fn parses_json_helper_stdout_with_expiry() {
    let now = Utc::now();
    let expires_at = now + chrono::Duration::seconds(600);
    let stdout = serde_json::json!({
        "token": "ghs_json",
        "expires_at": expires_at.to_rfc3339(),
        "kind": "github-app-installation"
    })
    .to_string();
    let token =
        parse_helper_stdout(&stdout, now, None, DEFAULT_REFRESH_SKEW_SECONDS).expect("token");
    assert_eq!(token.token, "ghs_json");
    assert_eq!(token.kind.as_deref(), Some("github-app-installation"));
    assert_eq!(token.expires_at, Some(expires_at));
    assert!(
        token
            .valid_until
            .is_some_and(|valid_until| valid_until < expires_at)
    );
}

#[test]
fn rejects_expired_json_helper_token() {
    let now = Utc::now();
    let expires_at = now + chrono::Duration::seconds(30);
    let stdout = serde_json::json!({
        "token": "ghs_json",
        "expires_at": expires_at.to_rfc3339()
    })
    .to_string();
    let error =
        parse_helper_stdout(&stdout, now, None, DEFAULT_REFRESH_SKEW_SECONDS).expect_err("expired");
    assert!(matches!(error, GhPrepareError::TokenExpired));
}

#[test]
fn expands_repo_placeholders() {
    let repo = TempDir::new().expect("tempdir");
    git(repo.path(), &["init", "--quiet", "--initial-branch=main"]);
    git(
        repo.path(),
        &["remote", "add", "origin", "git@github.com:owner/repo.git"],
    );
    let command = vec![
        "helper".to_owned(),
        "--repo".to_owned(),
        "{repo_slug}".to_owned(),
        "--owner".to_owned(),
        "{repo_owner}".to_owned(),
        "--name".to_owned(),
        "{repo_name}".to_owned(),
        "--cwd".to_owned(),
        "{cwd}".to_owned(),
    ];
    // The CWD's GitHub remote wins even when a hint is present.
    let hint = RepoIdentity::from_slug("hintowner/hintrepo");
    let expanded =
        expand_token_command(&command, repo.path(), hint.as_ref(), None).expect("expanded");
    assert_eq!(expanded[2], "owner/repo");
    assert_eq!(expanded[4], "owner");
    assert_eq!(expanded[6], "repo");
    assert_eq!(expanded[8], repo.path().display().to_string());

    let repo_override = RepoIdentity::from_slug("target/other").expect("override");
    let overridden =
        expand_token_command(&command, repo.path(), hint.as_ref(), Some(&repo_override))
            .expect("overridden");
    assert_eq!(overridden[2], "target/other");
    assert_eq!(overridden[4], "target");
    assert_eq!(overridden[6], "other");
}

#[test]
fn repo_placeholder_falls_back_to_hint_when_cwd_is_not_a_repo() {
    // The daemon case: CWD has no GitHub remote, so `{repo_slug}` must come
    // from the explicit hint (the served `--repo`) instead of erroring.
    let not_a_repo = TempDir::new().expect("tempdir");
    let command = vec![
        "helper".to_owned(),
        "--repo".to_owned(),
        "{repo_slug}".to_owned(),
    ];
    let hint = RepoIdentity::from_slug("danielraffel/pulp").expect("valid slug");
    let expanded =
        expand_token_command(&command, not_a_repo.path(), Some(&hint), None).expect("expanded");
    assert_eq!(expanded[2], "danielraffel/pulp");

    // Without a hint, it still errors (unchanged behavior).
    let err = expand_token_command(&command, not_a_repo.path(), None, None);
    assert!(matches!(err, Err(GhPrepareError::RepoSlugRequired)));
}

#[test]
fn repo_identity_from_slug_rejects_malformed() {
    assert!(RepoIdentity::from_slug("owner/name").is_some());
    assert!(RepoIdentity::from_slug("nope").is_none());
    assert!(RepoIdentity::from_slug("owner/").is_none());
    assert!(RepoIdentity::from_slug("/name").is_none());
    assert!(RepoIdentity::from_slug("a/b/c").is_none());
}

#[test]
fn explicit_repo_override_rejects_malformed_instead_of_falling_back() {
    let error = GhClient::ambient()
        .with_repo_override("owner/repo/extra")
        .expect_err("invalid explicit slug");

    assert!(matches!(
        error,
        GhPrepareError::InvalidRepoSlug { slug } if slug == "owner/repo/extra"
    ));
}

#[test]
fn parses_github_remote_urls() {
    assert_eq!(
        parse_github_remote_slug("git@github.com:owner/repo.git").as_deref(),
        Some("owner/repo")
    );
    assert_eq!(
        parse_github_remote_slug("ssh://git@github.com/owner/repo.git").as_deref(),
        Some("owner/repo")
    );
    assert_eq!(
        parse_github_remote_slug("https://github.com/owner/repo").as_deref(),
        Some("owner/repo")
    );
    assert_eq!(
        parse_github_remote_slug("https://example.com/owner/repo"),
        None
    );
}

#[test]
fn detects_graphql_rate_limit_text() {
    assert!(is_graphql_rate_limited("GraphQL: API rate limit exceeded"));
    assert!(!is_graphql_rate_limited("REST API rate limit exceeded"));
}

#[test]
fn authenticated_git_command_uses_environment_credential_helper() {
    let cwd = TempDir::new().expect("tempdir");
    let command = GhClient::ambient()
        .prepare_git_command(cwd.path())
        .expect("git command");
    assert_eq!(
        command.get_program().to_string_lossy(),
        crate::supervised::git_supervised()
            .get_program()
            .to_string_lossy()
    );
    assert_eq!(
        env_value(&command, "GIT_TERMINAL_PROMPT"),
        Some(OsString::from("0"))
    );
    assert_eq!(
        env_value(&command, "GIT_CONFIG_COUNT"),
        Some(OsString::from("7"))
    );
    assert_eq!(
        env_value(&command, "GIT_CONFIG_VALUE_0"),
        Some(OsString::from(""))
    );
    assert_eq!(
        env_value(&command, "GIT_CONFIG_VALUE_1"),
        Some(OsString::from("!gh auth git-credential"))
    );
    assert!(
        command
            .get_args()
            .all(|arg| !arg.to_string_lossy().contains("token"))
    );
}

#[cfg(unix)]
#[test]
fn token_credential_helper_releases_only_to_exact_github_https() {
    use std::io::Write as _;

    fn fill(input: &[u8]) -> Output {
        let mut child = Command::new("/usr/bin/git")
            .args(["-c", "credential.helper="])
            .arg("-c")
            .arg(format!(
                "credential.helper={}",
                token_environment_credential_helper()
            ))
            .args(["credential", "fill"])
            .env(GH_TOKEN_ENV, "ghs_fixture_secret")
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start git credential fill");
        child
            .stdin
            .take()
            .expect("credential stdin")
            .write_all(input)
            .expect("write credential query");
        child.wait_with_output().expect("credential output")
    }

    let github = fill(b"protocol=https\nhost=github.com\n\n");
    assert!(github.status.success());
    let github_output = String::from_utf8(github.stdout).expect("credential UTF-8");
    assert!(github_output.contains("password=ghs_fixture_secret"));

    for denied in [
        b"protocol=http\nhost=github.com\n\n".as_slice(),
        b"protocol=https\nhost=github.example\n\n".as_slice(),
    ] {
        let output = fill(denied);
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stdout).contains("ghs_fixture_secret"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("ghs_fixture_secret"));
    }
}

#[cfg(unix)]
#[test]
fn pinned_command_auth_uses_the_exact_validated_token_for_every_command() {
    let temp = TempDir::new().expect("tempdir");
    let helper = temp.path().join("alternating-token-helper");
    let count = temp.path().join("helper-count");
    write_executable(
        &helper,
        &format!(
            "#!/bin/sh\nif [ -e '{}' ]; then printf 'ghp_wrong_authority\\n'; else printf '1\\n' > '{}' && printf 'ghs_validated_app\\n'; fi\n",
            count.display(),
            count.display()
        ),
    );
    let native = std::env::current_exe().expect("native test executable");
    let config = config_from_toml(&format!(
        r#"
            [github.auth]
            source = "command"
            token_command = ["{}"]
            privileged_git_binary = "{}"
            "#,
        helper.display(),
        native.display()
    ));
    let mut client = GhClient::from_loaded_config(&config).expect("client");

    let summary = client.pin_command_auth(temp.path()).expect("pin App auth");
    assert_eq!(
        summary.token_kind.as_deref(),
        Some("github-app-installation")
    );
    let api = client
        .prepare_command(
            temp.path(),
            Some(&native),
            GhSupervision::Unsupervised,
            GhAuthPolicy::Default,
        )
        .expect("API command");
    let git = client
        .prepare_git_command(temp.path())
        .expect("Git command");
    assert_eq!(
        env_value(&api, GH_TOKEN_ENV),
        Some(OsString::from("ghs_validated_app"))
    );
    assert_eq!(
        env_value(&git, GH_TOKEN_ENV),
        Some(OsString::from("ghs_validated_app"))
    );
    assert!(Path::new(git.get_program()).is_absolute());
    assert_eq!(
        env_value(&git, "GIT_CONFIG_NOSYSTEM"),
        Some(OsString::from("1"))
    );
    assert_eq!(
        env_value(&git, "GIT_CONFIG_GLOBAL"),
        Some(OsString::from(null_device()))
    );
    assert_eq!(
        env_value(&git, "GIT_CONFIG_COUNT"),
        Some(OsString::from("7"))
    );
    assert_eq!(
        env_value(&git, "GIT_CONFIG_VALUE_1"),
        Some(OsString::from(token_environment_credential_helper()))
    );
    assert!(
        !env_value(&git, "GIT_CONFIG_VALUE_1")
            .expect("credential helper")
            .to_string_lossy()
            .contains("gh auth")
    );
    let credential_helper = env_value(&git, "GIT_CONFIG_VALUE_1")
        .expect("credential helper")
        .to_string_lossy()
        .into_owned();
    assert!(credential_helper.contains("$protocol\" = https"));
    assert!(credential_helper.contains("$host\" = github.com"));
    assert_eq!(
        env_value(&git, "GIT_CONFIG_VALUE_3"),
        Some(OsString::from(null_device()))
    );
    assert_eq!(
        env_value(&git, "GIT_CONFIG_VALUE_4"),
        Some(OsString::from("never"))
    );
    assert_eq!(
        env_value(&git, "GIT_CONFIG_VALUE_5"),
        Some(OsString::from("always"))
    );
    assert_eq!(std::fs::read_to_string(count).expect("helper count"), "1\n");
}

#[test]
fn helper_failure_display_redacts_token_like_stderr() {
    let error = GhPrepareError::HelperFailed {
        program: "helper".to_owned(),
        status: Some(1),
        stderr: "failed after minting ghs_secret123 and github_pat_abcDEF".to_owned(),
    };

    let rendered = error.to_string();

    assert!(rendered.contains("<redacted-token>"));
    assert!(!rendered.contains("ghs_secret123"));
    assert!(!rendered.contains("github_pat_abcDEF"));
}

#[test]
fn classifies_only_recoverable_token_helper_failures_as_transient() {
    let reset = GhPrepareError::HelperFailed {
        program: "helper".to_owned(),
        status: Some(1),
        stderr: "GitHub API request failed: [Errno 54] Connection reset by peer".to_owned(),
    };
    let unauthorized = GhPrepareError::HelperFailed {
        program: "helper".to_owned(),
        status: Some(1),
        stderr: "HTTP 401: bad credentials".to_owned(),
    };
    let start_reset = GhPrepareError::HelperStart {
        program: "helper".to_owned(),
        source: std::io::Error::from(std::io::ErrorKind::ConnectionReset),
    };
    let missing = GhPrepareError::HelperStart {
        program: "helper".to_owned(),
        source: std::io::Error::from(std::io::ErrorKind::NotFound),
    };

    assert!(reset.is_transient());
    assert!(
        GhPrepareError::HelperTimedOut {
            program: "helper".to_owned(),
            timeout_ms: 1_000,
        }
        .is_transient()
    );
    assert!(start_reset.is_transient());
    assert!(!unauthorized.is_transient());
    assert!(!missing.is_transient());
    assert!(
        !GhPrepareError::MissingTokenEnv {
            name: "TOKEN".to_owned(),
        }
        .is_transient()
    );
}

fn git(cwd: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("git");
    assert!(status.success(), "git command failed: {args:?}");
}
