//! Make a provisioned runner's service survive an ordinary exit.
//!
//! The GitHub Actions runner's own `svc.sh install` writes a `LaunchAgent` with
//! `RunAtLoad` and **no** `KeepAlive`. That starts the job once and says
//! nothing about what happens when it stops, which is not supervision — it
//! only looks like it, because a freshly installed runner is running.
//!
//! On 2026-09-05 that gap took out this repository's only macOS runner. A
//! routine force-push tripped `cancel-in-progress`; the cancel arrived as a
//! signal (`Runner will be shutdown for UserCancelled`), the runner exited, and
//! nothing brought it back. A read-only sweep of the fleet afterwards found
//! **four** runners in the same shape across two hosts, one of them already
//! offline on a repository whose only runner it was — quietly unserved, with no
//! queued work to make it visible.
//!
//! So this is not one misconfigured host. Every runner `shipyard runner
//! register` has ever created carries it, because the installer it delegates to
//! writes the same template. Patching the definition at provision time is the
//! fix that retires the class rather than the instance.

/// Key that makes launchd restart a job when it exits.
const KEEP_ALIVE_KEY: &str = "<key>KeepAlive</key>";

/// The key the runner's own template always emits, used as the insertion
/// anchor. It is a top-level entry in the same dict `KeepAlive` belongs to.
const ANCHOR: &str = "<key>RunAtLoad</key>";

/// Add a restart-on-exit policy to a service definition.
///
/// Returns `Ok(None)` when nothing needs doing, so callers can treat a
/// re-provision as a no-op:
///
/// - the definition already declares `KeepAlive`; or
/// - the runner is **ephemeral**, where `KeepAlive` would be a new bug rather
///   than a fix. An ephemeral runner is *supposed* to exit after one job, and
///   restarting it in place would respawn it forever and defeat the isolation
///   it exists to provide. `shipyard runner register` never registers one
///   today, but encoding the rule keeps that true by construction.
///
/// # Errors
///
/// Returns `Err` when the anchor is absent. The alternative — guessing where
/// the top-level dict begins — risks writing a malformed plist, and a runner
/// whose service definition will not parse is a worse outcome than one that
/// merely fails to restart. Refusing loudly leaves the working definition
/// intact and names what changed upstream.
pub(super) fn ensure_restart_on_exit(
    definition: &str,
    ephemeral: bool,
) -> Result<Option<String>, String> {
    if ephemeral {
        return Ok(None);
    }
    if definition.contains(KEEP_ALIVE_KEY) {
        return Ok(None);
    }
    let Some(at) = definition.find(ANCHOR) else {
        return Err(format!(
            "the service definition has no {ANCHOR} entry to anchor against, so its shape is not the one this fix was written for; leaving it untouched"
        ));
    };

    // Reuse the anchor's own indentation so the emitted plist stays readable
    // and diffable against the installer's output.
    let line_start = definition[..at].rfind('\n').map_or(0, |index| index + 1);
    let indent = &definition[line_start..at];

    let insertion = format!("{indent}{KEEP_ALIVE_KEY}\n{indent}<true/>\n");
    let mut patched = String::with_capacity(definition.len() + insertion.len());
    patched.push_str(&definition[..line_start]);
    patched.push_str(&insertion);
    patched.push_str(&definition[line_start..]);
    Ok(Some(patched))
}

/// Patch the service definition the runner's installer just wrote.
///
/// Runs between `svc.sh install` and `svc.sh start`, so the job is loaded with
/// the policy already in place rather than needing a reload.
///
/// A failure here does **not** roll back the installation. The runner is
/// registered and will serve; it simply will not come back on its own, which
/// is strictly better than an operator left with no runner at all. It is
/// reported rather than swallowed, because a supervision gap that nobody is
/// told about is precisely the failure this module exists to end.
pub(super) fn supervise_installed_service(runner_dir: &std::path::Path) {
    let runner_name = runner_dir.file_name().map_or_else(
        || runner_dir.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    let marker = runner_dir.join(".service");
    let Ok(raw) = std::fs::read_to_string(&marker) else {
        eprintln!(
            "warning: runner `{runner_name}` installed, but its .service marker could not be read, so it was not given a restart policy; it will not survive a cancel"
        );
        return;
    };
    let label = raw.trim();
    if label.is_empty() {
        eprintln!(
            "warning: runner `{runner_name}` installed with an empty .service marker; no restart policy applied"
        );
        return;
    }

    // The marker holds the plist path on macOS. Treat a bare label as relative
    // to the user's LaunchAgents directory, which is where svc.sh puts it.
    let path = std::path::Path::new(label);
    let plist = if path.is_absolute() {
        path.to_path_buf()
    } else {
        let Some(home) = std::env::var_os("HOME") else {
            eprintln!(
                "warning: runner `{runner_name}`: HOME is unset, so its service definition could not be located"
            );
            return;
        };
        std::path::Path::new(&home)
            .join("Library/LaunchAgents")
            .join(label)
    };

    let Ok(definition) = std::fs::read_to_string(&plist) else {
        eprintln!(
            "warning: runner `{runner_name}`: could not read {}, so no restart policy was applied",
            plist.display()
        );
        return;
    };

    match ensure_restart_on_exit(&definition, false) {
        Ok(None) => {}
        Ok(Some(patched)) => {
            if let Err(error) = std::fs::write(&plist, patched) {
                eprintln!(
                    "warning: runner `{runner_name}`: could not write {}: {error}; it will not survive a cancel",
                    plist.display()
                );
            }
        }
        Err(reason) => eprintln!("warning: runner `{runner_name}`: {reason}"),
    }
}

#[cfg(test)]
mod tests;
