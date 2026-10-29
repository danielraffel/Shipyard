//! Explicit, reversible quarantine for one malformed raw queue row.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;

use chrono::Utc;
use fs2::FileExt;
use serde_json::{Value, json};

use crate::app::CliFailure;

pub(super) fn queue_quarantine_command<W: Write>(
    job_id: &str,
    reason: &str,
    apply: bool,
    state_dir: &Path,
    json_output: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    if job_id.is_empty()
        || !job_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
    {
        return Err(CliFailure::new(
            2,
            "job id must contain only letters, digits, '-', '_' or '.'",
        ));
    }
    let queue_path = state_dir.join("queue.json");
    let raw = fs::read_to_string(&queue_path).map_err(|error| {
        CliFailure::new(
            1,
            format!("cannot read queue state {}: {error}", queue_path.display()),
        )
    })?;
    let root = serde_json::from_str::<Value>(&raw)
        .map_err(|error| CliFailure::new(1, format!("queue state is not valid JSON: {error}")))?;
    let (index, job) = {
        let jobs = root
            .get("jobs")
            .and_then(Value::as_array)
            .ok_or_else(|| CliFailure::new(1, "queue state has no jobs array"))?;
        let Some(index) = jobs
            .iter()
            .position(|job| job.get("id").and_then(Value::as_str) == Some(job_id))
        else {
            return Err(CliFailure::new(
                1,
                format!("queue job {job_id} was not found"),
            ));
        };
        (index, jobs[index].clone())
    };
    let quarantine_dir = state_dir.join("queue-quarantine");
    let stamp = Utc::now().format("%Y%m%dT%H%M%SZ");
    let quarantine_path = quarantine_dir.join(format!("{job_id}-{stamp}.json"));
    let report = json!({
        "schema": "shipyard-queue-quarantine/v1",
        "job_id": job_id,
        "quarantined_at": Utc::now().to_rfc3339(),
        "reason": reason,
        "source": queue_path,
        "job": job,
    });
    if json_output && !apply {
        let mut output = json!({
            "schema_version": 1,
            "command": "queue-quarantine",
            "job_id": job_id,
            "apply": apply,
            "quarantine_path": quarantine_path,
        });
        if !apply {
            output["action"] = Value::String("dry-run".to_owned());
        }
        serde_json::to_writer_pretty(&mut *stdout, &output)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        writeln!(stdout).map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else if !apply {
        writeln!(stdout, "queue job {job_id} is eligible for quarantine")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        writeln!(stdout, "  quarantine={}", quarantine_path.display())
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        if !apply {
            writeln!(
                stdout,
                "  action=dry-run (pass --apply to move the raw row)"
            )
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
    }
    if !apply {
        return Ok(ExitCode::SUCCESS);
    }

    let lock_path = state_dir.join("queue.state.lock");
    fs::create_dir_all(state_dir).map_err(|error| CliFailure::new(1, error.to_string()))?;
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|error| CliFailure::new(1, format!("cannot open queue state lock: {error}")))?;
    lock.try_lock_exclusive().map_err(|error| {
        CliFailure::new(
            1,
            format!("queue state is busy; quarantine was not applied: {error}"),
        )
    })?;
    let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(state_dir)
        .map_err(|error| {
            CliFailure::new(1, format!("cannot acquire queue writer lease: {error}"))
        })?;
    let current_raw = fs::read_to_string(&queue_path).map_err(|error| {
        CliFailure::new(
            1,
            format!("cannot re-read queue state before quarantine: {error}"),
        )
    })?;
    let mut root = serde_json::from_str::<Value>(&current_raw).map_err(|error| {
        CliFailure::new(
            1,
            format!("queue state changed to invalid JSON before quarantine: {error}"),
        )
    })?;
    let jobs = root
        .get_mut("jobs")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| CliFailure::new(1, "queue state changed and has no jobs array"))?;
    let Some(current_index) = jobs
        .iter()
        .position(|candidate| candidate.get("id").and_then(Value::as_str) == Some(job_id))
    else {
        return Err(CliFailure::new(
            1,
            format!("queue job {job_id} changed before quarantine"),
        ));
    };
    if current_index != index || jobs[current_index] != job {
        return Err(CliFailure::new(
            1,
            format!("queue job {job_id} changed before quarantine; inspect and retry"),
        ));
    }
    fs::create_dir_all(&quarantine_dir).map_err(|error| CliFailure::new(1, error.to_string()))?;
    let mut quarantine_file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&quarantine_path)
        .map_err(|error| CliFailure::new(1, format!("cannot create quarantine record: {error}")))?;
    serde_json::to_writer_pretty(&mut quarantine_file, &report)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    quarantine_file
        .write_all(b"\n")
        .and_then(|()| quarantine_file.sync_all())
        .map_err(|error| {
            CliFailure::new(1, format!("cannot finalize quarantine record: {error}"))
        })?;
    jobs.remove(current_index);
    let payload =
        serde_json::to_vec_pretty(&root).map_err(|error| CliFailure::new(1, error.to_string()))?;
    let temp_path = state_dir.join("queue.json.quarantine.tmp");
    let write_result = (|| -> io::Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temp_path)?;
        file.write_all(&payload)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temp_path, &queue_path)?;
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temp_path);
        return Err(CliFailure::new(
            1,
            format!("cannot rewrite queue after quarantine: {error}"),
        ));
    }
    if json_output {
        let output = json!({
            "schema_version": 1,
            "command": "queue-quarantine",
            "job_id": job_id,
            "apply": true,
            "action": "quarantined",
            "quarantine_path": quarantine_path,
        });
        serde_json::to_writer_pretty(&mut *stdout, &output)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        writeln!(stdout).map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        writeln!(stdout, "  action=quarantined")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(ExitCode::SUCCESS)
}
