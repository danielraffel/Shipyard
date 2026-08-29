fn observation_command(
    spec: &ReadOnlyCanaryHostSpec,
) -> Result<ObservationCommand, CanaryObserverError> {
    let script = macos_observer_script(&spec.staging_root);
    match &spec.target {
        ReadOnlyCanaryTarget::Local => {
            let mut command = Command::new("/bin/sh");
            command.args(["-c", &script]);
            Ok(ObservationCommand {
                command,
                transport: CanaryObservationTransport::Local,
                known_hosts: None,
            })
        }
        ReadOnlyCanaryTarget::StrictSsh(target) => {
            validate_executable(&target.ssh_program)?;
            validate_regular_authority(&target.identity_file, "SSH identity")?;
            let authority = KnownHostsAuthority::open(&target.known_hosts_file)?;
            let known_hosts_sha256 = authority.digest().clone();
            let mut command = Command::new(&target.ssh_program);
            command.args([
                "-F",
                "/dev/null",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=8",
                "-o",
                "StrictHostKeyChecking=yes",
                "-o",
                "CheckHostIP=yes",
                "-o",
                "UserKnownHostsFile=/dev/null",
                "-o",
                "KnownHostsCommand=/usr/bin/printenv SHIPYARD_CANARY_KNOWN_HOSTS",
                "-o",
                "IdentitiesOnly=yes",
                "-o",
                "ControlMaster=no",
                "-o",
                "ControlPath=none",
                "-o",
                "ProxyCommand=none",
                "-o",
                "ProxyJump=none",
                "-o",
                "PermitLocalCommand=no",
                "-o",
                "ClearAllForwardings=yes",
                "-i",
            ]);
            command.arg(&target.identity_file);
            command.args(["-p", &target.port.to_string(), "-o"]);
            command.args([
                "GlobalKnownHostsFile=/dev/null",
                "-o",
                "UpdateHostKeys=no",
                "--",
                &target.destination,
                &script,
            ]);
            command.env(KNOWN_HOSTS_ENV, authority.contents());
            Ok(ObservationCommand {
                command,
                transport: CanaryObservationTransport::StrictSsh {
                    destination: target.destination.clone(),
                    known_hosts_sha256,
                },
                known_hosts: Some(authority),
            })
        }
    }
}

fn validate_executable(path: &Path) -> Result<(), CanaryObserverError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        CanaryObserverError::AuthorityUnreadable(format!("{}: {error}", path.display()))
    })?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(CanaryObserverError::AuthorityUnreadable(format!(
            "{} is not a regular non-symlink executable",
            path.display()
        )));
    }
    Ok(())
}

fn validate_regular_authority(path: &Path, label: &str) -> Result<(), CanaryObserverError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        CanaryObserverError::AuthorityUnreadable(format!("{}: {error}", path.display()))
    })?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(CanaryObserverError::AuthorityUnreadable(format!(
            "{label} {} is not a regular non-symlink file",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn validate_private_identity_authority(path: &Path) -> Result<(), CanaryObserverError> {
    use std::os::unix::fs::PermissionsExt;

    validate_regular_authority(path, "SSH identity")?;
    let metadata = fs::metadata(path).map_err(|error| {
        CanaryObserverError::AuthorityUnreadable(format!("{}: {error}", path.display()))
    })?;
    if metadata.uid() != nix::unistd::Uid::effective().as_raw()
        || metadata.permissions().mode() & 0o777 != 0o600
    {
        return Err(CanaryObserverError::AuthorityUnreadable(
            "SSH identity must be current-user-owned mode 0600".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_private_identity_authority(_path: &Path) -> Result<(), CanaryObserverError> {
    Err(CanaryObserverError::InvalidConfiguration(
        "strict SSH identity authority requires a Unix controller".to_owned(),
    ))
}

fn safe_remote_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 1024
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-'))
}

#[cfg(unix)]
struct KnownHostsAuthority {
    file: File,
    contents: String,
    digest: Sha256Digest,
}

#[cfg(unix)]
impl KnownHostsAuthority {
    fn open(path: &Path) -> Result<Self, CanaryObserverError> {
        let before = known_hosts_path_metadata(path)?;
        let mut file = File::open(path).map_err(|error| {
            CanaryObserverError::AuthorityUnreadable(format!("{}: {error}", path.display()))
        })?;
        file.lock_shared().map_err(|error| {
            CanaryObserverError::AuthorityUnreadable(format!(
                "{} shared-lock failed: {error}",
                path.display()
            ))
        })?;
        let opened = file.metadata().map_err(|error| {
            CanaryObserverError::AuthorityUnreadable(format!("{}: {error}", path.display()))
        })?;
        let after = known_hosts_path_metadata(path)?;
        if !same_file_identity(&before, &opened) || !same_file_identity(&opened, &after) {
            return Err(CanaryObserverError::AuthorityUnreadable(format!(
                "{} changed while being opened",
                path.display()
            )));
        }
        let (digest, bytes) = read_known_hosts(&mut file, opened.len(), path)?;
        let contents = String::from_utf8(bytes).map_err(|_| {
            CanaryObserverError::AuthorityUnreadable(format!("{} is not UTF-8", path.display()))
        })?;
        Ok(Self {
            file,
            contents,
            digest,
        })
    }

    fn contents(&self) -> &str {
        &self.contents
    }

    fn digest(&self) -> &Sha256Digest {
        &self.digest
    }

    fn verify_unchanged(&mut self) -> Result<(), CanaryObserverError> {
        let length = self
            .file
            .metadata()
            .map_err(|error| CanaryObserverError::AuthorityUnreadable(error.to_string()))?
            .len();
        let (observed, _) =
            read_known_hosts(&mut self.file, length, Path::new("known-host authority"))?;
        if observed != self.digest {
            return Err(CanaryObserverError::AuthorityUnreadable(
                "known-host authority changed during observation".to_owned(),
            ));
        }
        Ok(())
    }
}

#[cfg(not(unix))]
struct KnownHostsAuthority;

#[cfg(not(unix))]
impl KnownHostsAuthority {
    fn open(_path: &Path) -> Result<Self, CanaryObserverError> {
        Err(CanaryObserverError::InvalidConfiguration(
            "strict macOS observer requires a Unix controller".to_owned(),
        ))
    }

    fn contents(&self) -> &str {
        unreachable!("unsupported controller")
    }

    fn digest(&self) -> &Sha256Digest {
        unreachable!("unsupported controller")
    }

    fn verify_unchanged(&mut self) -> Result<(), CanaryObserverError> {
        unreachable!("unsupported controller")
    }
}

#[cfg(unix)]
fn known_hosts_path_metadata(path: &Path) -> Result<fs::Metadata, CanaryObserverError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        CanaryObserverError::AuthorityUnreadable(format!("{}: {error}", path.display()))
    })?;
    if metadata.file_type().is_symlink()
        || !metadata.file_type().is_file()
        || metadata.len() == 0
        || metadata.len() > MAX_KNOWN_HOSTS_BYTES
    {
        return Err(CanaryObserverError::AuthorityUnreadable(format!(
            "{} is not a bounded regular non-symlink file",
            path.display()
        )));
    }
    Ok(metadata)
}

#[cfg(unix)]
fn same_file_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino() && left.len() == right.len()
}

#[cfg(unix)]
fn read_known_hosts(
    file: &mut File,
    expected_length: u64,
    path: &Path,
) -> Result<(Sha256Digest, Vec<u8>), CanaryObserverError> {
    let capacity = usize::try_from(expected_length).map_err(|_| {
        CanaryObserverError::AuthorityUnreadable(format!(
            "{} is too large for this controller",
            path.display()
        ))
    })?;
    file.seek(SeekFrom::Start(0)).map_err(|error| {
        CanaryObserverError::AuthorityUnreadable(format!("{}: {error}", path.display()))
    })?;
    let mut bytes = Vec::with_capacity(capacity);
    file.take(MAX_KNOWN_HOSTS_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            CanaryObserverError::AuthorityUnreadable(format!("{}: {error}", path.display()))
        })?;
    if bytes.len() as u64 != expected_length {
        return Err(CanaryObserverError::AuthorityUnreadable(format!(
            "{} changed while being read",
            path.display()
        )));
    }
    file.seek(SeekFrom::Start(0)).map_err(|error| {
        CanaryObserverError::AuthorityUnreadable(format!("{}: {error}", path.display()))
    })?;
    Ok((Sha256Digest::of_bytes(&bytes), bytes))
}

fn macos_observer_script(staging_root: &Path) -> String {
    let root = shell_quote(&staging_root.to_string_lossy());
    format!(
        "set -eu\nroot={root}\nplatform_uuid=$(/usr/sbin/ioreg -rd1 -c IOPlatformExpertDevice | /usr/bin/awk -F '\"' '/IOPlatformUUID/ {{print $(NF-1); exit}}')\nboot_seconds=$(/usr/sbin/sysctl -n kern.boottime | /usr/bin/awk -F '[=,]' '{{gsub(/ /, \"\", $2); print $2}}')\nprintf 'schema\\t1\\nplatform_uuid\\t%s\\nboot_seconds\\t%s\\n' \"$platform_uuid\" \"$boot_seconds\"\nif test -d \"$root\" && test ! -L \"$root\"; then\n  identity_before=$(/usr/bin/stat -f '%d:%i' \"$root\")\n  canonical=$(cd \"$root\" && /bin/pwd -P)\n  identity_canonical=$(/usr/bin/stat -f '%d:%i' \"$canonical\")\n  free_kib=$(/bin/df -Pk \"$canonical\" | /usr/bin/awk 'END {{print $4}}')\n  identity_after=$(/usr/bin/stat -f '%d:%i' \"$root\")\n  test ! -L \"$root\"\n  test \"$identity_before\" = \"$identity_canonical\"\n  test \"$identity_before\" = \"$identity_after\"\n  printf 'staging\\tpresent\\ncanonical_root\\t%s\\nfree_kib\\t%s\\n' \"$canonical\" \"$free_kib\"\nelse\n  printf 'staging\\tmissing\\n'\nfi"
    )
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

struct ParsedProbe {
    platform_uuid: String,
    boot_seconds: u64,
    staging: ParsedStaging,
}

enum ParsedStaging {
    Missing,
    Present { canonical: String, free_bytes: u64 },
}

fn parse_probe_output(bytes: &[u8]) -> Result<ParsedProbe, CanaryObserverError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| CanaryObserverError::MalformedOutput("output is not UTF-8".to_owned()))?;
    let mut fields = std::collections::BTreeMap::new();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('\t') else {
            return Err(CanaryObserverError::MalformedOutput(
                "output contains a non-field line".to_owned(),
            ));
        };
        if fields.insert(key, value).is_some() {
            return Err(CanaryObserverError::MalformedOutput(format!(
                "duplicate {key} field"
            )));
        }
    }
    if fields.remove("schema") != Some(OBSERVER_SCHEMA) {
        return Err(CanaryObserverError::MalformedOutput(
            "unsupported observer schema".to_owned(),
        ));
    }
    let platform_uuid = fields
        .remove("platform_uuid")
        .filter(|value| valid_platform_uuid(value))
        .ok_or_else(|| CanaryObserverError::MalformedOutput("invalid platform UUID".to_owned()))?
        .to_owned();
    let boot_seconds = fields
        .remove("boot_seconds")
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value != 0)
        .ok_or_else(|| CanaryObserverError::MalformedOutput("invalid boot session".to_owned()))?;
    let staging = match fields.remove("staging") {
        Some("missing") if fields.is_empty() => ParsedStaging::Missing,
        Some("present") => {
            let canonical = fields
                .remove("canonical_root")
                .filter(|value| safe_absolute_macos_path(Path::new(value)))
                .ok_or_else(|| {
                    CanaryObserverError::MalformedOutput(
                        "invalid canonical staging root".to_owned(),
                    )
                })?
                .to_owned();
            let free_kib = fields
                .remove("free_kib")
                .and_then(|value| value.parse::<u64>().ok())
                .ok_or_else(|| {
                    CanaryObserverError::MalformedOutput(
                        "invalid staging free-space value".to_owned(),
                    )
                })?;
            if !fields.is_empty() {
                return Err(CanaryObserverError::MalformedOutput(
                    "unknown observer fields".to_owned(),
                ));
            }
            ParsedStaging::Present {
                canonical,
                free_bytes: free_kib.checked_mul(1024).ok_or_else(|| {
                    CanaryObserverError::MalformedOutput(
                        "staging free-space value overflows bytes".to_owned(),
                    )
                })?,
            }
        }
        _ => {
            return Err(CanaryObserverError::MalformedOutput(
                "invalid staging observation".to_owned(),
            ));
        }
    };
    Ok(ParsedProbe {
        platform_uuid,
        boot_seconds,
        staging,
    })
}

fn valid_platform_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

fn valid_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b':'))
}

fn safe_absolute_macos_path(path: &Path) -> bool {
    let Some(value) = path.to_str() else {
        return false;
    };
    path.is_absolute()
        && value != "/"
        && !value.ends_with('/')
        && !value.chars().any(char::is_control)
        && value
            .split('/')
            .skip(1)
            .all(|component| !component.is_empty() && component != "." && component != "..")
        && [
            "/tmp",
            "/private/tmp",
            "/var/tmp",
            "/private/var/tmp",
            "/var/folders",
            "/private/var/folders",
        ]
        .iter()
        .all(|temporary| value != *temporary && !value.starts_with(&format!("{temporary}/")))
}

fn milliseconds_ceil(duration: Duration) -> Result<u64, CanaryObserverError> {
    let millis = duration.as_millis();
    let millis = if duration.subsec_nanos().is_multiple_of(1_000_000) {
        millis
    } else {
        millis
            .checked_add(1)
            .ok_or_else(|| CanaryObserverError::Clock("monotonic duration overflow".to_owned()))?
    };
    u64::try_from(millis)
        .map_err(|_| CanaryObserverError::Clock("monotonic duration overflow".to_owned()))
}

fn controller_now_ms() -> Result<u64, CanaryObserverError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| CanaryObserverError::Clock(error.to_string()))?
        .as_millis();
    u64::try_from(millis)
        .map_err(|_| CanaryObserverError::Clock("controller epoch overflow".to_owned()))
}

fn bounded_stderr(bytes: &[u8]) -> String {
    String::from_utf8_lossy(&bytes[..bytes.len().min(512)])
        .trim()
        .to_owned()
}
