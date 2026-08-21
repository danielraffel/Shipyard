use super::validation::invalid_output;
use super::{
    RECOVERY_SCHEMA_VERSION, RecoveryError, RecoveryOutput, RecoveryRequest, RecoveryResult,
    RecoveryVerdict,
};

impl RecoveryOutput {
    /// Validate size, shape, and model-owned risk-routing constraints.
    pub fn validate(&self) -> RecoveryResult<()> {
        if self.schema_version != RECOVERY_SCHEMA_VERSION {
            return Err(RecoveryError::SchemaVersion {
                surface: "output",
                observed: self.schema_version,
            });
        }
        if !self.evidence.is_empty()
            || !self.candidate_paths.is_empty()
            || !self.focused_tests.is_empty()
        {
            return Err(invalid_output(
                "phase-1 classification cannot return evidence, candidate_paths, or focused_tests",
            ));
        }
        match self.verdict {
            RecoveryVerdict::BoundedRepair => {
                return Err(invalid_output(
                    "phase-1 classification has no repair authority; bounded_repair is forbidden",
                ));
            }
            RecoveryVerdict::Escalate => {}
            RecoveryVerdict::NoChange => {
                return Err(invalid_output(
                    "phase-1 routing has no diagnostic evidence; no_change is forbidden and every result must escalate",
                ));
            }
        }
        Ok(())
    }

    /// Validate the model result against the phase-1 request boundary.
    ///
    /// Phase 1 has no diagnostic evidence and therefore accepts only explicit
    /// escalation for every request, independently of model category or
    /// self-reported confidence.
    pub fn validate_for_request(&self, _request: &RecoveryRequest) -> RecoveryResult<()> {
        self.validate()
    }
}
