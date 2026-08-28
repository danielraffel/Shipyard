//! Durable, restart-visible health for the silent shadow observer.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{SHADOW_PASS_TIMEOUT, ShadowTrigger};

const SCHEMA_VERSION: u32 = 1;

/// Restart-visible health for the otherwise silent shadow-observation lane.
///
/// This receipt is operational evidence only. It cannot activate work, deliver
/// a wake, authorize a mutation, or invoke a model.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ShadowObserverHealth {
    schema_version: u32,
    /// Last pass that completed without a target fetch failure.
    pub last_success_at: Option<u64>,
    /// Last observer failure, including a pass with one or more fetch failures.
    pub last_failure_at: Option<u64>,
    /// Stable, redacted class for the last failure.
    pub last_failure_class: Option<String>,
    /// Exact policy-enrolled targets visible during the latest enumeration.
    pub exact_target_count: usize,
    /// Durable round-robin cursor for the next periodic selection.
    pub periodic_cursor: usize,
    /// Wall-clock epoch second at which the next periodic or webhook pass is due.
    pub next_due_at: Option<u64>,
    /// Wall-clock epoch second at which the current pass began.
    pub in_flight_since: Option<u64>,
    /// Trigger for the current or most recently completed pass.
    pub last_trigger: Option<ShadowTrigger>,
    /// Exact targets selected by the current or most recently completed pass.
    pub last_selected_targets: usize,
    /// Worst-case request reservation currently held by an in-flight pass.
    pub reserved_requests: usize,
    /// Worst-case reservation held by the most recently completed pass.
    pub last_reserved_requests: usize,
    /// Actual GitHub requests consumed by the most recently completed pass.
    pub last_actual_requests: usize,
    /// Conservatively accounted requests in the rolling-hour budget.
    pub rolling_hour_requests: usize,
    /// Activation remains impossible in the shadow phase.
    pub activation_enabled: bool,
    /// Dispatch and wake delivery remain impossible in the shadow phase.
    pub dispatch_enabled: bool,
    /// Routine observation never invokes a model.
    pub model_calls: u64,
}

impl Default for ShadowObserverHealth {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            last_success_at: None,
            last_failure_at: None,
            last_failure_class: None,
            exact_target_count: 0,
            periodic_cursor: 0,
            next_due_at: None,
            in_flight_since: None,
            last_trigger: None,
            last_selected_targets: 0,
            reserved_requests: 0,
            last_reserved_requests: 0,
            last_actual_requests: 0,
            rolling_hour_requests: 0,
            activation_enabled: false,
            dispatch_enabled: false,
            model_calls: 0,
        }
    }
}

impl ShadowObserverHealth {
    /// Normalize a loaded receipt to the current schema after validation.
    pub(super) fn normalize_schema(&mut self) {
        self.schema_version = SCHEMA_VERSION;
    }

    /// Render status with a live stalled verdict without rewriting the durable
    /// receipt on every daemon-status query.
    #[must_use]
    pub fn status_value(&self, now_epoch_seconds: u64) -> Value {
        let mut value = serde_json::to_value(self).expect("shadow health serializes");
        let stalled = self.in_flight_since.is_some_and(|started| {
            now_epoch_seconds.saturating_sub(started) > SHADOW_PASS_TIMEOUT.as_secs()
        });
        value
            .as_object_mut()
            .expect("shadow health is an object")
            .insert("stalled".to_owned(), Value::Bool(stalled));
        value
    }
}

pub(super) fn load(path: &Path) -> Result<ShadowObserverHealth, String> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ShadowObserverHealth::default());
        }
        Err(error) => return Err(error.to_string()),
    };
    let health = serde_json::from_slice::<ShadowObserverHealth>(&bytes)
        .map_err(|error| error.to_string())?;
    if health.schema_version != SCHEMA_VERSION {
        return Err(format!(
            "unsupported shadow observer health schema {}",
            health.schema_version
        ));
    }
    Ok(health)
}

pub(super) fn save(path: &Path, health: &ShadowObserverHealth) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "shadow health path has no parent".to_owned())?;
    let mut temporary =
        tempfile::NamedTempFile::new_in(parent).map_err(|error| error.to_string())?;
    serde_json::to_writer_pretty(temporary.as_file_mut(), health)
        .map_err(|error| error.to_string())?;
    temporary
        .as_file_mut()
        .sync_all()
        .map_err(|error| error.to_string())?;
    temporary.persist(path).map_err(|error| error.to_string())?;
    #[cfg(unix)]
    fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| error.to_string())?;
    Ok(())
}
