//! Transport for the read-only `shipyard queue-digest` command.

use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use serde_json::Value;

use super::CliFailure;
use crate::output::write_json_envelope;
use crate::queue_digest::{read_digest, render_markdown};

pub(super) fn queue_digest_command<W: Write>(
    state_dir: &Path,
    stale_after_seconds: u64,
    json_mode: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let digest = read_digest(state_dir, stale_after_seconds);
    if json_mode {
        let mut data = serde_json::Map::new();
        data.insert(
            "digest_schema_version".to_owned(),
            Value::from(digest.schema_version),
        );
        data.insert("complete".to_owned(), Value::Bool(digest.complete));
        data.insert(
            "observers".to_owned(),
            serde_json::to_value(&digest.observers)
                .map_err(|error| CliFailure::new(1, error.to_string()))?,
        );
        data.insert(
            "buckets".to_owned(),
            serde_json::to_value(&digest.buckets)
                .map_err(|error| CliFailure::new(1, error.to_string()))?,
        );
        data.insert(
            "errors".to_owned(),
            serde_json::to_value(&digest.errors)
                .map_err(|error| CliFailure::new(1, error.to_string()))?,
        );
        data.insert(
            "stale_after_seconds".to_owned(),
            Value::from(stale_after_seconds),
        );
        write_json_envelope(stdout, "queue-digest", data.into_iter().collect())
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        stdout
            .write_all(render_markdown(&digest).as_bytes())
            .map_err(|error| CliFailure::new(1, format!("write queue digest: {error}")))?;
    }
    if digest.complete {
        Ok(ExitCode::SUCCESS)
    } else {
        // The diagnostic digest is still printed, but an incomplete census is
        // never a successful all-clear result.
        Err(CliFailure::new(
            1,
            if digest.errors.is_empty() {
                "queue digest is incomplete".to_owned()
            } else {
                digest.errors.join("; ")
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_output_identifies_digest_schema() {
        let temp = tempfile::tempdir().expect("temporary state root");
        let observer_root = temp.path().join("queue-observer");
        std::fs::create_dir_all(&observer_root).expect("observer root");
        std::fs::write(
            observer_root.join("fixture.json"),
            include_str!("../../tests/fixtures/queue-digest/complete.json"),
        )
        .expect("fixture state");

        let mut output = Vec::new();
        let exit = queue_digest_command(temp.path(), u64::MAX, true, &mut output)
            .expect("complete digest");
        assert_eq!(exit, ExitCode::SUCCESS);
        let payload: Value = serde_json::from_slice(&output).expect("JSON envelope");
        assert_eq!(payload["digest_schema_version"], 2);
    }
}
