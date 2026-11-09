//! The GitHub App credential behind in-process reads, and its JWT.
//!
//! The App and key are the ones `shipyard auth helper-argv` hands the `ghapp`
//! wrapper: the machine-global `[github.auth] token_command`, which must have
//! the canonical `[WRAPPER, "token", "--app-id", ID, "--private-key", PATH,
//! "--repo", "{repo_slug}"]` shape. Any other shape is not understood here, so
//! reads keep using `gh`.
//!
//! Key material never leaves this module: the PEM is read, checked and parsed
//! into a signing key, then wiped. No error message carries its contents, and
//! the signing key's `Debug` prints nothing.

use std::fmt;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use zeroize::Zeroize as _;

/// Largest private-key file this reader opens.
const MAX_KEY_BYTES: u64 = 64 * 1024;

/// Which App, which key, and whether tokens are scoped to one repository.
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct AppCredential {
    pub(crate) app_id: u64,
    pub(crate) key_path: PathBuf,
    pub(crate) scope_to_repository: bool,
}

impl fmt::Debug for AppCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AppCredential")
            .field("app_id", &self.app_id)
            .field("key_path", &self.key_path)
            .field("scope_to_repository", &self.scope_to_repository)
            .finish()
    }
}

impl AppCredential {
    /// The credential named by a canonical `token_command` (with the
    /// `--scope-repository` flag Shipyard appends when configured), or `None`.
    pub(crate) fn from_token_command(command: &[String]) -> Option<Self> {
        let (base, scope_to_repository) = match command {
            [base @ .., last] if last == "--scope-repository" => (base, true),
            all => (all, false),
        };
        let [
            wrapper,
            token,
            app_flag,
            app_id,
            key_flag,
            key_path,
            repo_flag,
            repo,
        ] = base
        else {
            return None;
        };
        if !Path::new(wrapper).is_absolute()
            || token != "token"
            || app_flag != "--app-id"
            || key_flag != "--private-key"
            || repo_flag != "--repo"
            || repo != "{repo_slug}"
            || app_id.is_empty()
            || app_id.len() > 20
            || !app_id.bytes().all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        let app_id = app_id.parse::<u64>().ok().filter(|id| *id != 0)?;
        let key_path = PathBuf::from(key_path);
        if !key_path.is_absolute()
            || key_path
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return None;
        }
        Some(Self {
            app_id,
            key_path,
            scope_to_repository,
        })
    }
}

/// A loaded App signing key.
pub(crate) struct AppSigner {
    app_id: u64,
    key: RsaKeyPair,
    rng: SystemRandom,
}

impl fmt::Debug for AppSigner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AppSigner")
            .field("app_id", &self.app_id)
            .finish_non_exhaustive()
    }
}

impl AppSigner {
    /// Load and check the key the same way the `ghapp` token helper does:
    /// a regular file (not a symlink), mode 0600, owned by this user, in a
    /// directory no other user can read.
    pub(crate) fn load(credential: &AppCredential) -> Result<Self, String> {
        check_key_file(&credential.key_path)?;
        let mut pem = read_bounded(&credential.key_path)?;
        let parsed = parse_pem(&pem);
        pem.zeroize();
        let mut der = parsed?;
        let key = RsaKeyPair::from_der(&der)
            .or_else(|_| RsaKeyPair::from_pkcs8(&der))
            .map_err(|_| "GitHub App private key is not a usable RSA key".to_owned());
        der.zeroize();
        Ok(Self {
            app_id: credential.app_id,
            key: key?,
            rng: SystemRandom::new(),
        })
    }

    /// An App JWT valid from a minute ago for nine minutes, the window the
    /// token helper uses.
    pub(crate) fn jwt(&self, now_epoch: i64) -> Result<String, String> {
        let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let header = engine.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let claims = engine.encode(
            serde_json::json!({
                "iat": now_epoch - 60,
                "exp": now_epoch + 9 * 60,
                "iss": self.app_id.to_string(),
            })
            .to_string(),
        );
        let signing_input = format!("{header}.{claims}");
        let mut signature = vec![0; self.key.public().modulus_len()];
        self.key
            .sign(
                &RSA_PKCS1_SHA256,
                &self.rng,
                signing_input.as_bytes(),
                &mut signature,
            )
            .map_err(|_| "could not sign the GitHub App JWT".to_owned())?;
        Ok(format!("{signing_input}.{}", engine.encode(signature)))
    }
}

#[cfg(unix)]
fn check_key_file(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt as _;
    let uid = nix::unistd::getuid().as_raw();
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|_| "GitHub App private key is not readable".to_owned())?;
    if !metadata.file_type().is_file() {
        return Err("GitHub App private key must be a regular file, not a symlink".to_owned());
    }
    if metadata.mode() & 0o777 != 0o600 {
        return Err("GitHub App private key must have mode 0600".to_owned());
    }
    if metadata.uid() != uid {
        return Err("GitHub App private key must be owned by the current user".to_owned());
    }
    let parent = path
        .parent()
        .ok_or_else(|| "GitHub App private key has no directory".to_owned())?;
    let parent_metadata = std::fs::metadata(parent)
        .map_err(|_| "GitHub App private key directory is not readable".to_owned())?;
    if parent_metadata.mode() & 0o077 != 0 {
        return Err("GitHub App private key directory must have mode 0700 or stricter".to_owned());
    }
    if parent_metadata.uid() != uid {
        return Err(
            "GitHub App private key directory must be owned by the current user".to_owned(),
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_key_file(path: &Path) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|_| "GitHub App private key is not readable".to_owned())?;
    if !metadata.file_type().is_file() {
        return Err("GitHub App private key must be a regular file, not a symlink".to_owned());
    }
    Ok(())
}

fn read_bounded(path: &Path) -> Result<String, String> {
    use std::io::Read as _;
    let file = std::fs::File::open(path)
        .map_err(|_| "GitHub App private key is not readable".to_owned())?;
    let mut text = String::new();
    file.take(MAX_KEY_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|_| "GitHub App private key is not valid text".to_owned())?;
    if text.len() as u64 > MAX_KEY_BYTES {
        text.zeroize();
        return Err("GitHub App private key file is too large".to_owned());
    }
    Ok(text)
}

/// The DER body of a PKCS#1 (`RSA PRIVATE KEY`) or PKCS#8 (`PRIVATE KEY`) PEM.
fn parse_pem(pem: &str) -> Result<Vec<u8>, String> {
    let malformed = || "GitHub App private key is not a PEM RSA key".to_owned();
    let mut body = String::new();
    let mut inside = false;
    let mut seen = false;
    for line in pem.lines().map(str::trim) {
        if line == "-----BEGIN RSA PRIVATE KEY-----" || line == "-----BEGIN PRIVATE KEY-----" {
            if seen {
                body.zeroize();
                return Err(malformed());
            }
            inside = true;
            seen = true;
        } else if line == "-----END RSA PRIVATE KEY-----" || line == "-----END PRIVATE KEY-----" {
            if !inside {
                body.zeroize();
                return Err(malformed());
            }
            inside = false;
        } else if inside {
            body.push_str(line);
        }
    }
    if !seen || inside || body.is_empty() {
        body.zeroize();
        return Err(malformed());
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(body.as_bytes())
        .map_err(|_| malformed());
    body.zeroize();
    decoded
}
