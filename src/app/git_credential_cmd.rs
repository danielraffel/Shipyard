//! `shipyard auth git-credential`: a git credential helper that answers with a
//! short-lived GitHub App installation token restricted to the one repository
//! git is talking to.
//!
//! Configure it for github.com only, with the path git sends:
//!
//! ```text
//! git config credential.https://github.com.helper ''
//! git config --add credential.https://github.com.helper '!shipyard auth git-credential'
//! git config credential.https://github.com.useHttpPath true
//! ```
//!
//! Which repositories it serves, and with which App permissions, come only from
//! the machine-global `[git_credential]` table, never from a project layer.

use std::io::{Read, Write};
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use chrono::{DateTime, Utc};
use toml::Value as TomlValue;

use crate::config::LoadedConfig;
use crate::gh::{GhClient, validate_repo_slug};

use super::CliFailure;

const DEFAULT_PERMISSIONS: &[&str] = &["contents=write"];
const MINT_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_INPUT_BYTES: u64 = 64 * 1024;

/// What `get` hands back to git. The token never reaches a `{:?}`.
pub(super) struct Credential {
    token: String,
    expires_at: Option<DateTime<Utc>>,
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credential")
            .field("token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

pub(super) fn git_credential<R: Read, W: Write>(
    action: &str,
    global_dir: &Path,
    cwd: &Path,
    stdin: R,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let mut input = String::new();
    stdin
        .take(MAX_INPUT_BYTES)
        .read_to_string(&mut input)
        .map_err(|error| CliFailure::new(1, format!("git-credential: read stdin: {error}")))?;
    // git also calls `store` and `erase`; a minted token is never stored.
    if action != "get" {
        return Ok(ExitCode::SUCCESS);
    }
    let answer = LoadedConfig::load_machine_global_from_dir(global_dir.to_path_buf())
        .map_err(|error| error.to_string())
        .and_then(|config| {
            answer_get(&input, &config, |slug, args| mint(&config, cwd, slug, args))
        });
    match answer {
        Ok(credential) => {
            write_credential(stdout, &credential)
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
            Ok(ExitCode::SUCCESS)
        }
        Err(reason) => {
            // `quit=1` stops git from asking any later helper (for example
            // a personal `gh auth git-credential`) or prompting.
            writeln!(stdout, "quit=1").map_err(|error| CliFailure::new(1, error.to_string()))?;
            Err(CliFailure::new(1, format!("git-credential: {reason}")))
        }
    }
}

fn mint(
    config: &LoadedConfig,
    cwd: &Path,
    slug: &str,
    args: &[String],
) -> Result<Option<Credential>, String> {
    let client = GhClient::from_loaded_config(config)
        .map_err(|error| error.to_string())?
        .with_repo_override(slug)
        .map_err(|error| error.to_string())?;
    client
        .mint_uncached_command_token(cwd, args, MINT_TIMEOUT)
        .map(|minted| minted.map(|(token, expires_at)| Credential { token, expires_at }))
        .map_err(|error| error.to_string())
}

/// Decide the answer to one `get` request. `mint` receives the exact
/// `OWNER/REPO` and the scope arguments for the token helper.
pub(super) fn answer_get<F>(
    input: &str,
    config: &LoadedConfig,
    mint: F,
) -> Result<Credential, String>
where
    F: FnOnce(&str, &[String]) -> Result<Option<Credential>, String>,
{
    let request = parse_request(input);
    if request.protocol.as_deref() != Some("https") || request.host.as_deref() != Some("github.com")
    {
        return Err(format!(
            "only https://github.com is served, not {}://{}",
            request.protocol.as_deref().unwrap_or("?"),
            request.host.as_deref().unwrap_or("?")
        ));
    }
    let slug = request
        .path
        .as_deref()
        .and_then(repo_from_path)
        .ok_or_else(|| {
            "git sent no OWNER/REPO path; set credential.https://github.com.useHttpPath true"
                .to_owned()
        })?;
    let allowed = configured_repositories(config)?;
    let Some(slug) = allowed
        .into_iter()
        .find(|allowed| allowed.eq_ignore_ascii_case(&slug))
    else {
        return Err(format!(
            "{slug} is not in the machine-global git_credential.repositories"
        ));
    };
    let mut args = vec!["--scope-repository".to_owned()];
    for permission in configured_permissions(config)? {
        args.push("--permission".to_owned());
        args.push(permission);
    }
    let credential = mint(&slug, &args)?.ok_or_else(|| {
        "machine-global github.auth.source must be \"command\" (the ghapp token helper)".to_owned()
    })?;
    if credential
        .expires_at
        .is_some_and(|expires_at| expires_at <= Utc::now())
    {
        return Err("the token helper returned an expired token".to_owned());
    }
    Ok(credential)
}

#[derive(Debug, Default, Eq, PartialEq)]
struct Request {
    protocol: Option<String>,
    host: Option<String>,
    path: Option<String>,
}

fn parse_request(input: &str) -> Request {
    let mut request = Request::default();
    for line in input.lines() {
        if line.is_empty() {
            break;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let slot = match key {
            "protocol" => &mut request.protocol,
            "host" => &mut request.host,
            "path" => &mut request.path,
            _ => continue,
        };
        *slot = Some(value.to_owned());
    }
    request
}

fn repo_from_path(path: &str) -> Option<String> {
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    validate_repo_slug(path).ok()?;
    Some(path.to_owned())
}

fn configured_repositories(config: &LoadedConfig) -> Result<Vec<String>, String> {
    let repositories = string_list(config, "git_credential.repositories")?.unwrap_or_default();
    if repositories.is_empty() {
        return Err(
            "machine-global git_credential.repositories is empty; nothing is served".to_owned(),
        );
    }
    for slug in &repositories {
        validate_repo_slug(slug).map_err(|_| {
            format!("git_credential.repositories entry {slug:?} is not an exact OWNER/REPO")
        })?;
    }
    Ok(repositories)
}

fn configured_permissions(config: &LoadedConfig) -> Result<Vec<String>, String> {
    let permissions = string_list(config, "git_credential.permissions")?
        .unwrap_or_else(|| DEFAULT_PERMISSIONS.iter().map(|&p| p.to_owned()).collect());
    if permissions.is_empty() {
        return Err("git_credential.permissions must name at least one permission".to_owned());
    }
    for permission in &permissions {
        let valid = permission.split_once('=').is_some_and(|(name, level)| {
            !name.is_empty()
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
                && matches!(level, "read" | "write")
        });
        if !valid {
            return Err(format!(
                "git_credential.permissions entry {permission:?} must be NAME=read|write"
            ));
        }
    }
    Ok(permissions)
}

fn string_list(config: &LoadedConfig, key: &str) -> Result<Option<Vec<String>>, String> {
    let Some(value) = config.get(key) else {
        return Ok(None);
    };
    value
        .as_array()
        .and_then(|items| {
            items
                .iter()
                .map(|item| TomlValue::as_str(item).map(str::to_owned))
                .collect::<Option<Vec<_>>>()
        })
        .map(Some)
        .ok_or_else(|| format!("machine-global {key} must be an array of strings"))
}

fn write_credential<W: Write>(stdout: &mut W, credential: &Credential) -> std::io::Result<()> {
    writeln!(stdout, "username=x-access-token")?;
    writeln!(stdout, "password={}", credential.token)?;
    // git (2.41+) drops a credential past this instant instead of retrying it.
    if let Some(expires_at) = credential.expires_at {
        writeln!(stdout, "password_expiry_utc={}", expires_at.timestamp())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::path::PathBuf;

    use super::*;
    use crate::config::LocalOverlaySource;

    fn config(contents: &str) -> LoadedConfig {
        LoadedConfig {
            data: contents.parse::<toml::Table>().expect("config TOML"),
            global_dir: PathBuf::from("/tmp/global"),
            project_dir: None,
            local_dir: None,
            local_overlay_source: LocalOverlaySource::None,
        }
    }

    const ALLOWED: &str = r#"
[git_credential]
repositories = ["danielraffel/Shipyard", "Generous-Corp/pulp"]
"#;

    fn request(path: &str) -> String {
        format!("capability[]=authtype\nprotocol=https\nhost=github.com\npath={path}\n\n")
    }

    fn token(expires_at: Option<DateTime<Utc>>) -> Credential {
        Credential {
            token: "ghs_test".to_owned(),
            expires_at,
        }
    }

    #[test]
    fn get_mints_a_token_scoped_to_the_requested_repository() {
        let seen = RefCell::new(None);
        let expires_at = Utc::now() + chrono::Duration::minutes(50);
        let credential = answer_get(
            &request("generous-corp/pulp.git"),
            &config(ALLOWED),
            |slug, args| {
                seen.replace(Some((slug.to_owned(), args.to_vec())));
                Ok(Some(token(Some(expires_at))))
            },
        )
        .expect("credential");

        let (slug, args) = seen.into_inner().expect("minted");
        // The configured spelling is what reaches the helper.
        assert_eq!(slug, "Generous-Corp/pulp");
        assert_eq!(
            args,
            ["--scope-repository", "--permission", "contents=write"]
        );
        let mut output = Vec::new();
        write_credential(&mut output, &credential).expect("write");
        assert_eq!(
            String::from_utf8(output).expect("utf8"),
            format!(
                "username=x-access-token\npassword=ghs_test\npassword_expiry_utc={}\n",
                expires_at.timestamp()
            )
        );
        assert!(!format!("{credential:?}").contains("ghs_test"));
    }

    #[test]
    fn get_passes_configured_permissions() {
        let config = config(
            r#"
[git_credential]
repositories = ["danielraffel/Shipyard"]
permissions = ["contents=write", "workflows=write"]
"#,
        );
        let seen = RefCell::new(Vec::new());
        answer_get(&request("danielraffel/Shipyard"), &config, |_, args| {
            seen.replace(args.to_vec());
            Ok(Some(token(None)))
        })
        .expect("credential");
        assert_eq!(
            seen.into_inner(),
            [
                "--scope-repository",
                "--permission",
                "contents=write",
                "--permission",
                "workflows=write"
            ]
        );
    }

    #[test]
    fn get_refuses_without_minting() {
        let cases = [
            (
                request("danielraffel/tartci"),
                ALLOWED,
                "not in the machine-global",
            ),
            (
                "protocol=https\nhost=github.com\n\n".to_owned(),
                ALLOWED,
                "useHttpPath",
            ),
            (request("danielraffel"), ALLOWED, "useHttpPath"),
            (request("a/b/c"), ALLOWED, "useHttpPath"),
            (
                "protocol=http\nhost=github.com\npath=danielraffel/Shipyard\n\n".to_owned(),
                ALLOWED,
                "only https://github.com",
            ),
            (
                "protocol=https\nhost=github.com.evil.example\npath=danielraffel/Shipyard\n\n"
                    .to_owned(),
                ALLOWED,
                "only https://github.com",
            ),
            (
                "protocol=https\nhost=gist.github.com\npath=danielraffel/Shipyard\n\n".to_owned(),
                ALLOWED,
                "only https://github.com",
            ),
            (request("danielraffel/Shipyard"), "", "is empty"),
            (
                request("danielraffel/Shipyard"),
                "[git_credential]\nrepositories = \"danielraffel/Shipyard\"\n",
                "array of strings",
            ),
            (
                request("danielraffel/Shipyard"),
                "[git_credential]\nrepositories = [\"danielraffel/Shipyard\"]\npermissions = [\"contents=admin\"]\n",
                "NAME=read|write",
            ),
            (
                request("danielraffel/Shipyard"),
                "[git_credential]\nrepositories = [\"danielraffel/Shipyard\"]\npermissions = []\n",
                "at least one",
            ),
        ];
        for (input, contents, expected) in cases {
            let error = answer_get(&input, &config(contents), |_, _| {
                panic!("minted for {input:?}");
            })
            .expect_err("refused");
            assert!(error.contains(expected), "{input:?}: {error}");
        }
    }

    #[test]
    fn get_refuses_a_non_command_auth_source_and_an_expired_token() {
        let none = answer_get(
            &request("danielraffel/Shipyard"),
            &config(ALLOWED),
            |_, _| Ok(None),
        )
        .expect_err("refused");
        assert!(none.contains("github.auth.source"));

        let expired = answer_get(
            &request("danielraffel/Shipyard"),
            &config(ALLOWED),
            |_, _| Ok(Some(token(Some(Utc::now() - chrono::Duration::seconds(1))))),
        )
        .expect_err("refused");
        assert!(expired.contains("expired"));
    }

    #[test]
    fn request_parsing_stops_at_the_blank_line() {
        assert_eq!(
            parse_request("protocol=https\nhost=github.com\n\npath=x/y\n"),
            Request {
                protocol: Some("https".to_owned()),
                host: Some("github.com".to_owned()),
                path: None,
            }
        );
        assert_eq!(repo_from_path("o/r.git/").as_deref(), Some("o/r"));
    }

    #[test]
    fn store_and_erase_are_silent_no_ops() {
        for action in ["store", "erase", "unknown"] {
            let mut output = Vec::new();
            let exit = git_credential(
                action,
                Path::new("/nonexistent/global"),
                Path::new("/"),
                request("danielraffel/Shipyard").as_bytes(),
                &mut output,
            )
            .expect("no-op");
            assert_eq!(exit, ExitCode::SUCCESS);
            assert!(output.is_empty());
        }
    }

    #[test]
    fn a_refusal_tells_git_to_stop_asking() {
        let global = tempfile::tempdir().expect("tempdir");
        let mut output = Vec::new();
        let error = git_credential(
            "get",
            global.path(),
            Path::new("/"),
            request("danielraffel/Shipyard").as_bytes(),
            &mut output,
        )
        .expect_err("refused");
        assert_eq!(String::from_utf8(output).expect("utf8"), "quit=1\n");
        assert!(error.message.contains("git_credential.repositories"));
    }
}
