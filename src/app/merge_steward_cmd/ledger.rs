use super::{
    CliFailure, LedgerAudit, OpenOptions, Path, PendingMutationKind, StewardLedger, Utc, Write, fs,
};
use crate::queue::replace_file_with_windows_retry;

pub(super) fn attempt_key(repo: &str, pr: u64, head: &str, run_id: u64) -> String {
    format!("{repo}#{pr}:{head}:{run_id}")
}

pub(super) fn record_audit(ledger: &mut StewardLedger, repo: &str, subject: &str, action: &str) {
    ledger.audit.push(LedgerAudit {
        at: Utc::now().to_rfc3339(),
        repo: repo.to_owned(),
        subject: subject.to_owned(),
        action: action.to_owned(),
    });
    if ledger.audit.len() > 1_000 {
        ledger.audit.drain(..ledger.audit.len() - 1_000);
    }
}

pub(super) fn persist_pending_mutation_correlation(
    ledger: &mut StewardLedger,
    ledger_path: &Path,
    key: &str,
    correlation_id: &str,
    mutation_kind: PendingMutationKind,
    action: &str,
) -> Result<(), String> {
    let pending = ledger
        .pending_cancellations
        .get_mut(key)
        .ok_or_else(|| "pending cancellation record disappeared".to_owned())?;
    correlation_id.clone_into(&mut pending.mutation_correlation_id);
    pending.mutation_kind = mutation_kind;
    let repo = pending.repo.clone();
    let run_id = pending.run_id;
    record_audit(ledger, &repo, &format!("capacity-run:{run_id}"), action);
    save_ledger(ledger_path, ledger).map_err(|error| {
        format!(
            "could not persist pending mutation correlation: {}",
            error.message
        )
    })
}

pub(super) fn load_ledger(path: &Path) -> Result<StewardLedger, CliFailure> {
    Ok(load_existing_ledger(path)?.unwrap_or_default())
}

pub(super) fn load_existing_ledger(path: &Path) -> Result<Option<StewardLedger>, CliFailure> {
    match fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw)
            .map(Some)
            .map_err(|error| CliFailure::new(1, format!("invalid steward ledger: {error}"))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(CliFailure::new(
            1,
            format!("could not read steward ledger {}: {error}", path.display()),
        )),
    }
}

pub(super) fn save_ledger(path: &Path, ledger: &StewardLedger) -> Result<(), CliFailure> {
    let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(path)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|error| {
            CliFailure::new(
                1,
                format!(
                    "could not create steward state {}: {error}",
                    parent.display()
                ),
            )
        })?;
    }
    let payload = serde_json::to_vec_pretty(ledger)
        .map_err(|error| CliFailure::new(1, format!("could not encode steward ledger: {error}")))?;
    let temp = path.with_extension("json.tmp");
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temp)
        .map_err(|error| {
            CliFailure::new(
                1,
                format!("could not open steward ledger {}: {error}", temp.display()),
            )
        })?;
    file.write_all(&payload).map_err(|error| {
        CliFailure::new(
            1,
            format!("could not write steward ledger {}: {error}", temp.display()),
        )
    })?;
    file.sync_all().map_err(|error| {
        CliFailure::new(
            1,
            format!("could not sync steward ledger {}: {error}", temp.display()),
        )
    })?;
    replace_file_with_windows_retry(&temp, path).map_err(|error| {
        CliFailure::new(
            1,
            format!(
                "could not publish steward ledger {}: {error}",
                path.display()
            ),
        )
    })?;
    #[cfg(not(windows))]
    sync_parent_directory(path)?;
    #[cfg(windows)]
    sync_parent_directory(path);
    Ok(())
}

#[cfg(not(windows))]
fn sync_parent_directory(path: &Path) -> Result<(), CliFailure> {
    let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return Ok(());
    };
    fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| {
            CliFailure::new(
                1,
                format!(
                    "could not sync steward state directory {}: {error}",
                    parent.display()
                ),
            )
        })
}

#[cfg(windows)]
fn sync_parent_directory(_path: &Path) {
    // Windows does not support opening a directory with std::fs::File for
    // sync_all(). The temp file itself is flushed before the atomic replace.
}
