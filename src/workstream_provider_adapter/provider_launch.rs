#[cfg(unix)]
use std::fs::OpenOptions;
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use super::HashWriter;
use crate::provider_wrapper::{ProviderDeliveryTargetV1, ProviderWrapperRequestV1};

#[cfg(not(test))]
pub(super) const PRIVATE_LAUNCH_ACCEPTANCE_DEADLINE: Duration = Duration::from_secs(5);
#[cfg(test)]
pub(super) const PRIVATE_LAUNCH_ACCEPTANCE_DEADLINE: Duration = Duration::from_millis(50);

pub(super) trait ProviderLaunchAuthority {
    fn verify_route(&mut self, request: &ProviderWrapperRequestV1) -> Result<(), &'static str>;
    fn prepare_launch(
        &mut self,
        request: &ProviderWrapperRequestV1,
    ) -> Result<PrivateLaunch, &'static str>;
}

pub(super) struct ProductionSubrouterLaunchAuthority;

impl ProviderLaunchAuthority for ProductionSubrouterLaunchAuthority {
    fn verify_route(&mut self, request: &ProviderWrapperRequestV1) -> Result<(), &'static str> {
        verify_subrouter_executable(request)
    }

    fn prepare_launch(
        &mut self,
        request: &ProviderWrapperRequestV1,
    ) -> Result<PrivateLaunch, &'static str> {
        prepare_private_launch(request, true)
    }
}

#[cfg(unix)]
pub(super) fn verify_subrouter_executable(
    request: &ProviderWrapperRequestV1,
) -> Result<(), &'static str> {
    const MAX_SUBROUTER_BYTES: u64 = 128 * 1024 * 1024;
    let path = Path::new(&request.protected_route.argv[0]);
    let mut executable = OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
        .open(path)
        .map_err(|_| "subrouter-executable-unavailable")?;
    let before = executable
        .metadata()
        .map_err(|_| "subrouter-executable-untrusted")?;
    let effective_uid = nix::unistd::Uid::effective().as_raw();
    if !before.is_file()
        || (before.uid() != 0 && before.uid() != effective_uid)
        || before.nlink() != 1
        || before.len() == 0
        || before.len() > MAX_SUBROUTER_BYTES
        || before.mode() & 0o111 == 0
        || before.mode() & 0o022 != 0
    {
        return Err("subrouter-executable-untrusted");
    }
    let mut hasher = Sha256::new();
    let copied = std::io::copy(&mut executable, &mut HashWriter(&mut hasher))
        .map_err(|_| "subrouter-executable-unreadable")?;
    let after = executable
        .metadata()
        .map_err(|_| "subrouter-executable-untrusted")?;
    if copied != before.len()
        || before.dev() != after.dev()
        || before.ino() != after.ino()
        || before.len() != after.len()
        || hex::encode(hasher.finalize()) != request.protected_route.executable_sha256
    {
        return Err("subrouter-executable-drift");
    }
    Ok(())
}

#[cfg(not(unix))]
pub(super) fn verify_subrouter_executable(
    _: &ProviderWrapperRequestV1,
) -> Result<(), &'static str> {
    Err("subrouter-executable-verification-unavailable")
}

pub(super) struct PrivateLaunch {
    pub(super) command: String,
    route_path: PathBuf,
    executable_path: PathBuf,
}

impl Drop for PrivateLaunch {
    fn drop(&mut self) {
        match std::fs::remove_file(&self.route_path) {
            Ok(()) => {
                let _ = std::fs::remove_file(&self.executable_path);
                if let Some(parent) = self.route_path.parent() {
                    let _ = std::fs::remove_dir(parent);
                }
            }
            // A consumed production launch opened the snapshot by descriptor and
            // unlinked both capsule files before exec. Never race that descriptor.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {}
        }
    }
}

impl PrivateLaunch {
    pub(super) fn wait_until_consumed(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            match std::fs::symlink_metadata(&self.route_path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return true,
                Err(_) => return false,
                Ok(_) if Instant::now() >= deadline => return false,
                Ok(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }
}

pub(super) fn launch_command(
    request: &ProviderWrapperRequestV1,
    executable_path: &Path,
    wait_for_child_cleanup: bool,
) -> Result<String, &'static str> {
    let prompt = delivery_prompt(request);
    let argv = if request.delivery_target == ProviderDeliveryTargetV1::FreshCheckpoint {
        &request.protected_route.fresh_argv
    } else {
        &request.protected_route.argv
    };
    let mut lines = request
        .protected_route
        .environment
        .iter()
        .map(|(name, value)| {
            shell_word(&format!("{name}={value}")).map(|word| format!("export {word}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut invocation = vec![shell_word(
        executable_path
            .to_str()
            .ok_or("private-launch-path-invalid")?,
    )?];
    invocation.extend(
        argv[1..]
            .iter()
            .map(|value| shell_word(value))
            .collect::<Result<Vec<_>, _>>()?,
    );
    invocation.push(shell_word(&prompt)?);
    if wait_for_child_cleanup {
        // The production snapshot must remain named while the provider is
        // running. Keep this shell as its parent so the installed trap removes
        // the snapshot and capsule directory after every normal/signal exit.
        lines.push("set +e".to_owned());
        lines.push(invocation.join(" "));
        lines.push("provider_status=$?".to_owned());
        lines.push("set -e".to_owned());
        lines.push("exit \"$provider_status\"".to_owned());
    } else {
        lines.push(format!("exec {}", invocation.join(" ")));
    }
    Ok(lines.join("\n"))
}

pub(super) fn delivery_prompt(request: &ProviderWrapperRequestV1) -> String {
    format!(
        "Resume tracked workstream {}. First run `shipyard --json work-ledger context-challenge --wake {}` and reconstruct that exact durable context. Write the matching receipt to a private file, then run `shipyard --json work-ledger acknowledge-context --wake {} --receipt <private-path>`. Complete the remaining work and keep Linear current. Before handoff, run `shipyard --json work-ledger return-challenge --ownership <ownership-id>`, write separate reviewed expectation and receipt files proving a newer checkpoint, evidence, and remote acknowledgement, then run `shipyard --json work-ledger return-ownership --ownership <ownership-id> --expectation <private-path> --receipt <private-path>`. Never put receipt JSON or secrets in argv.",
        request.resume_expectation.workstream_handle,
        request.delivery_fence.wake_id,
        request.delivery_fence.wake_id,
    )
}

pub(super) fn prepare_private_launch(
    request: &ProviderWrapperRequestV1,
    snapshot_executable: bool,
) -> Result<PrivateLaunch, &'static str> {
    let directory = tempfile::Builder::new()
        .prefix(".shipyard-workstream-route-")
        .tempdir()
        .map_err(|_| "private-launch-directory-unavailable")?;
    let directory_path = directory.path().to_path_buf();
    let executable_path = directory_path.join("subrouter");
    let (launch_executable, prologue) = if snapshot_executable {
        snapshot_subrouter(request, &executable_path)?;
        let executable_word = shell_word(
            executable_path
                .to_str()
                .ok_or("private-launch-path-invalid")?,
        )?;
        let directory_word = shell_word(
            directory_path
                .to_str()
                .ok_or("private-launch-path-invalid")?,
        )?;
        (
            executable_path.as_path(),
            format!(
                "#!/bin/sh\nset -eu\nrm -f -- \"$0\"\ncleanup() {{ rm -f -- {executable_word}; rmdir -- {directory_word} 2>/dev/null || :; }}\ntrap cleanup EXIT HUP INT TERM\n"
            ),
        )
    } else {
        (
            Path::new(&request.protected_route.argv[0]),
            "#!/bin/sh\nset -eu\nrm -f -- \"$0\"\n".to_owned(),
        )
    };
    let body = launch_command(request, launch_executable, snapshot_executable)?;
    let route_path = directory_path.join("launch.sh");
    let mut route = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&route_path)
        .map_err(|_| "private-launch-file-unavailable")?;
    route
        .write_all(prologue.as_bytes())
        .and_then(|()| route.write_all(body.as_bytes()))
        .and_then(|()| route.write_all(b"\n"))
        .and_then(|()| route.sync_all())
        .map_err(|_| "private-launch-file-unwritable")?;
    drop(route);
    sync_directory(&directory_path)?;
    let directory_path = directory.keep();
    let route_path = directory_path.join("launch.sh");
    Ok(PrivateLaunch {
        command: format!(
            "'/bin/sh' {}",
            shell_word(route_path.to_str().ok_or("private-launch-path-invalid")?)?
        ),
        route_path,
        executable_path,
    })
}

#[cfg(unix)]
fn snapshot_subrouter(
    request: &ProviderWrapperRequestV1,
    destination: &Path,
) -> Result<(), &'static str> {
    let source_path = Path::new(&request.protected_route.argv[0]);
    let mut source = OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
        .open(source_path)
        .map_err(|_| "subrouter-executable-unavailable")?;
    let before = source
        .metadata()
        .map_err(|_| "subrouter-executable-untrusted")?;
    let mut destination = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o500)
        .open(destination)
        .map_err(|_| "subrouter-snapshot-unavailable")?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    let mut copied = 0_u64;
    loop {
        let read = source
            .read(&mut buffer)
            .map_err(|_| "subrouter-executable-unreadable")?;
        if read == 0 {
            break;
        }
        copied = copied.saturating_add(read as u64);
        hasher.update(&buffer[..read]);
        destination
            .write_all(&buffer[..read])
            .map_err(|_| "subrouter-snapshot-unwritable")?;
    }
    let after = source
        .metadata()
        .map_err(|_| "subrouter-executable-untrusted")?;
    if copied != before.len()
        || before.dev() != after.dev()
        || before.ino() != after.ino()
        || before.len() != after.len()
        || hex::encode(hasher.finalize()) != request.protected_route.executable_sha256
    {
        return Err("subrouter-executable-drift");
    }
    destination
        .sync_all()
        .map_err(|_| "subrouter-snapshot-unwritable")
}

#[cfg(not(unix))]
fn snapshot_subrouter(_: &ProviderWrapperRequestV1, _: &Path) -> Result<(), &'static str> {
    Err("subrouter-executable-verification-unavailable")
}

fn sync_directory(path: &Path) -> Result<(), &'static str> {
    std::fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| "private-launch-directory-unwritable")
}

fn shell_word(value: &str) -> Result<String, &'static str> {
    if value.is_empty()
        || value
            .chars()
            .any(|character| character == '\0' || character.is_control())
    {
        return Err("launch-value-is-not-shell-safe");
    }
    Ok(format!("'{}'", value.replace('\'', "'\"'\"'")))
}
