//! Read-only host and SSH contract validation for custody setup.

use std::fs::{self, OpenOptions};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use super::{MAX_SETUP_BYTES, REQUIRED_SUBSYSTEM};
use crate::custody_transport::policy::CustodyPeer;

const MAX_SSHD_BYTES: u64 = 512 * 1024;

pub(super) fn ensure_private_directory(path: &Path) -> Result<(), &'static str> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            validate_private_mode(&metadata, 0o700, "custody-config-directory-untrusted")
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path).map_err(|_| "custody-config-directory-create-failed")?;
            let permissions = fs::metadata(path)
                .map_err(|_| "custody-config-directory-untrusted")?
                .permissions();
            #[cfg(unix)]
            let permissions = {
                use std::os::unix::fs::PermissionsExt;
                let mut permissions = permissions;
                permissions.set_mode(0o700);
                permissions
            };
            fs::set_permissions(path, permissions)
                .map_err(|_| "custody-config-directory-untrusted")?;
            let metadata =
                fs::symlink_metadata(path).map_err(|_| "custody-config-directory-untrusted")?;
            validate_private_mode(&metadata, 0o700, "custody-config-directory-untrusted")
        }
        Err(_) => Err("custody-config-directory-unavailable"),
    }
}

fn validate_private_mode(
    metadata: &fs::Metadata,
    expected_mode: u32,
    reason: &'static str,
) -> Result<(), &'static str> {
    if !metadata.is_dir() {
        return Err(reason);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let uid = nix::unistd::Uid::effective().as_raw();
        if metadata.uid() != uid || metadata.permissions().mode() & 0o777 != expected_mode {
            return Err(reason);
        }
    }
    #[cfg(not(unix))]
    let _ = expected_mode;
    Ok(())
}

pub(in crate::custody_transport) fn read_private_input(
    path: &Path,
    max: u64,
    require_mode: bool,
) -> Result<Vec<u8>, ReadPrivateError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed());
    }
    #[cfg(not(unix))]
    let _ = require_mode;
    let file = options.open(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            ReadPrivateError::Missing
        } else {
            ReadPrivateError::new("custody-private-file-unavailable")
        }
    })?;
    let metadata = file
        .metadata()
        .map_err(|_| ReadPrivateError::new("custody-private-file-untrusted"))?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > max {
        return Err(ReadPrivateError::new("custody-private-file-untrusted"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let uid = nix::unistd::Uid::effective().as_raw();
        if metadata.nlink() != 1
            || (require_mode && metadata.uid() != uid)
            || (require_mode && metadata.permissions().mode() & 0o777 != 0o600)
            || (!require_mode && metadata.permissions().mode() & 0o022 != 0)
        {
            return Err(ReadPrivateError::new("custody-private-file-untrusted"));
        }
    }
    let mut bytes = Vec::new();
    file.take(max + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ReadPrivateError::new("custody-private-file-unavailable"))?;
    if bytes.len() as u64 > max {
        return Err(ReadPrivateError::new("custody-private-file-untrusted"));
    }
    Ok(bytes)
}

pub(super) fn validate_private_file(path: &Path, kind: &str) -> Result<String, &'static str> {
    read_private_input(path, MAX_SETUP_BYTES, true)
        .map(|_| format!("{kind} is owner-only and non-symlink"))
        .map_err(|error| match error {
            ReadPrivateError::Missing => "custody-private-file-unavailable",
            ReadPrivateError::Code(code) => code,
        })
}

pub(super) fn read_public_config(path: &Path, max: u64) -> Result<String, &'static str> {
    let bytes = read_private_input(path, max, false).map_err(|error| error.code())?;
    String::from_utf8(bytes).map_err(|_| "custody-file-not-utf8")
}

pub(super) fn derive_public_key_digest(identity: &Path) -> Result<String, &'static str> {
    let mut command = Command::new("ssh-keygen");
    command
        .args(["-y", "-f"])
        .arg(identity)
        .stdin(Stdio::null())
        .env_clear()
        .env("LANG", "C")
        .env("LC_ALL", "C");
    let output = crate::process::run_output_until(
        &mut command,
        Instant::now() + Duration::from_secs(5),
        "custody ssh-keygen",
    )
    .map_err(|_| "custody-public-key-derivation-unavailable")?;
    if !output.status.success() {
        return Err("custody-public-key-derivation-refused");
    }
    let text =
        String::from_utf8(output.stdout).map_err(|_| "custody-public-key-derivation-invalid")?;
    let key = normalize_public_key(&text).ok_or("custody-public-key-derivation-invalid")?;
    Ok(hex::encode(Sha256::digest(key.as_bytes())))
}

pub(super) fn normalize_public_key(text: &str) -> Option<String> {
    let mut rows = text.lines().filter_map(|line| {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        let key_type = fields.iter().position(|field| {
            field.starts_with("ssh-") || field.starts_with("ecdsa-") || field.starts_with("sk-ssh-")
        })?;
        (fields.len() > key_type + 1)
            .then(|| format!("{} {}", fields[key_type], fields[key_type + 1]))
    });
    let key = rows.next()?;
    rows.next().is_none().then_some(key)
}

pub(super) fn validate_sshd_effective_config(
    config_path: &Path,
    authorized_keys_path: &Path,
    receiver_program: &Path,
) -> Result<(), &'static str> {
    let mut command = Command::new("/usr/sbin/sshd");
    command
        .args(["-T", "-f"])
        .arg(config_path)
        .env_clear()
        .env("LANG", "C")
        .env("LC_ALL", "C");
    let output = crate::process::run_output_until(
        &mut command,
        Instant::now() + Duration::from_secs(5),
        "custody sshd effective config",
    )
    .map_err(|_| "custody-sshd-effective-config-unavailable")?;
    if !output.status.success() {
        return Err("custody-sshd-effective-config-refused");
    }
    if output.stdout.len() as u64 > MAX_SSHD_BYTES {
        return Err("custody-sshd-effective-config-too-large");
    }
    let text =
        String::from_utf8(output.stdout).map_err(|_| "custody-sshd-effective-config-invalid")?;
    let mut expose = 0usize;
    let mut subsystem = 0usize;
    let mut authorized = Vec::new();
    let receiver_program = receiver_program.to_string_lossy();
    for line in text.lines() {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.len() >= 2 && fields[0].eq_ignore_ascii_case("exposeauthinfo") {
            if fields[1].eq_ignore_ascii_case("yes") {
                expose += 1;
            }
            continue;
        }
        if fields.len() >= 3 && fields[0].eq_ignore_ascii_case("subsystem") {
            let command = &fields[2..];
            if fields[1] == REQUIRED_SUBSYSTEM
                && command.len() == 5
                && command[0] == receiver_program.as_ref()
                && command[1] == "--mode"
                && command[2] == "shipyard"
                && command[3] == "work-ledger"
                && command[4] == "custody-receive"
            {
                subsystem += 1;
            }
            continue;
        }
        if fields.len() >= 2 && fields[0].eq_ignore_ascii_case("authorizedkeysfile") {
            authorized.extend(fields[1..].iter().copied());
        }
    }
    if expose != 1 || subsystem != 1 || authorized.is_empty() {
        return Err("custody-sshd-effective-config-incomplete");
    }
    let configured = authorized_keys_path;
    let matches = authorized.iter().filter(|entry| {
        let entry = entry.trim_matches('"');
        effective_authorized_keys_path(entry)
            .as_deref()
            .is_some_and(|path| path == configured)
    });
    if matches.count() != 1 {
        return Err("custody-sshd-authorized-keys-path-mismatch");
    }
    Ok(())
}

/// Resolve only the account-local forms emitted by `sshd -T` and compare them
/// to the exact owner path in policy. `%u`, `~`, and other substitutions are
/// intentionally rejected because setup has no authenticated context for
/// them. Relative paths are anchored to the current owner home, never to the
/// process cwd or an arbitrary suffix.
pub(super) fn effective_authorized_keys_path(value: &str) -> Option<PathBuf> {
    let value = value.trim();
    if value.is_empty() || value.contains('%') && !value.starts_with("%h/") {
        return None;
    }
    let path = if let Some(rest) = value.strip_prefix("%h/") {
        crate::paths::home_dir().join(rest)
    } else if Path::new(value).is_absolute() {
        PathBuf::from(value)
    } else {
        crate::paths::home_dir().join(value)
    };
    normalize_path(&path)
}

fn normalize_path(path: &Path) -> Option<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
            Component::Normal(value) => normalized.push(value),
        }
    }
    Some(normalized)
}

#[cfg(all(test, unix))]
pub(super) fn validate_sshd_config(text: &str) -> Result<(), &'static str> {
    let mut expose = false;
    let mut subsystem = false;
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or_default().trim();
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.len() == 2
            && fields[0].eq_ignore_ascii_case("ExposeAuthInfo")
            && fields[1].eq_ignore_ascii_case("yes")
        {
            expose = true;
        }
        if fields.len() >= 3
            && fields[0].eq_ignore_ascii_case("Subsystem")
            && fields[1] == REQUIRED_SUBSYSTEM
            && fields.len() == 7
            && fields[2] == "/bin/shipyard"
            && fields[3..] == ["--mode", "shipyard", "work-ledger", "custody-receive"]
        {
            subsystem = true;
        }
    }
    if expose && subsystem {
        Ok(())
    } else {
        Err("custody-sshd-subsystem-incomplete")
    }
}

pub(super) fn validate_authorized_keys<'a>(
    text: &str,
    peers: impl Iterator<Item = &'a CustodyPeer>,
) -> Result<(), &'static str> {
    let mut keys = Vec::new();
    for line in text.lines() {
        if let Some(key) = parse_authorized_key_line(line)? {
            keys.push(key);
        }
    }
    for peer in peers {
        let identity_digest = peer.ssh_auth_key_sha256.as_str();
        let count = keys
            .iter()
            .filter(|key| hex::encode(Sha256::digest(key.as_bytes())) == identity_digest)
            .count();
        if count != 1 {
            return Err("custody-authorized-key-mismatch");
        }
    }
    Ok(())
}

pub(super) fn parse_authorized_key_line(line: &str) -> Result<Option<String>, &'static str> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return Ok(None);
    }
    let fields = shell_tokens(line).ok_or("custody-authorized-key-ambiguous")?;
    let candidates = fields
        .iter()
        .enumerate()
        .filter(|(_, field)| {
            field.starts_with("ssh-") || field.starts_with("ecdsa-") || field.starts_with("sk-ssh-")
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return Ok(None);
    }
    if candidates.len() != 1 {
        return Err("custody-authorized-key-ambiguous");
    }
    let index = candidates[0];
    if fields.len() <= index + 1 || fields[index + 1].is_empty() {
        return Err("custody-authorized-key-malformed");
    }
    Ok(Some(format!("{} {}", fields[index], fields[index + 1])))
}

fn shell_tokens(line: &str) -> Option<Vec<String>> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    for byte in line.bytes() {
        let ch = byte as char;
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if let Some(open) = quote {
            if ch == open {
                quote = None;
            } else {
                current.push(ch);
            }
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            ch if ch.is_ascii_whitespace() => {
                if !current.is_empty() {
                    fields.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }
    if escaped || quote.is_some() {
        return None;
    }
    if !current.is_empty() {
        fields.push(current);
    }
    Some(fields)
}

#[derive(Clone, Debug)]
pub(in crate::custody_transport) enum ReadPrivateError {
    Missing,
    Code(&'static str),
}

impl ReadPrivateError {
    fn new(code: &'static str) -> Self {
        Self::Code(code)
    }

    pub(super) fn code(&self) -> &'static str {
        match self {
            Self::Missing => "custody-private-file-unavailable",
            Self::Code(code) => code,
        }
    }
}
