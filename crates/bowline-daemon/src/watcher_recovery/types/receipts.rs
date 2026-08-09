use std::time::Duration;

use super::{
    ActivityWatermark, AttemptCoverageBoundary, AttemptId, AttemptToken, IncidentId,
    MAX_DURATION_MS, RecoveryCount, RecoveryRevision, RecoveryScanRevision, RecoveryTimestamp,
    RecoveryTransitionError,
};

/// Evidence offered while the engine coverage-scan lease is still held.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseOffer {
    attempt: AttemptToken,
    native_boundary: AttemptCoverageBoundary,
    scan_revision: RecoveryScanRevision,
}

impl CloseOffer {
    /// Bind the authoritative local scan and post-scan native seal to one attempt.
    pub(crate) fn from_attempt_evidence(
        attempt: AttemptToken,
        native_boundary: AttemptCoverageBoundary,
        scan_revision: RecoveryScanRevision,
    ) -> Self {
        Self {
            attempt,
            native_boundary,
            scan_revision,
        }
    }

    pub fn attempt(&self) -> AttemptToken {
        self.attempt
    }

    pub fn native_boundary(&self) -> &AttemptCoverageBoundary {
        &self.native_boundary
    }

    pub fn scan_revision(&self) -> RecoveryScanRevision {
        self.scan_revision
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryClosureReceipt {
    incident_id: IncidentId,
    attempt_id: AttemptId,
    attempt_count: RecoveryCount,
    scan_count: RecoveryCount,
    activity_watermark: ActivityWatermark,
    authoritative_scan_revision: RecoveryScanRevision,
    native_boundary: AttemptCoverageBoundary,
    started_at: RecoveryTimestamp,
    completed_at: RecoveryTimestamp,
    duration_ms: u64,
    closing_recovery_revision: RecoveryRevision,
}

pub(crate) struct RecoveryClosureReceiptInput {
    pub incident_id: IncidentId,
    pub attempt_id: AttemptId,
    pub attempt_count: RecoveryCount,
    pub scan_count: RecoveryCount,
    pub activity_watermark: ActivityWatermark,
    pub authoritative_scan_revision: RecoveryScanRevision,
    pub native_boundary: AttemptCoverageBoundary,
    pub started_at: RecoveryTimestamp,
    pub completed_at: RecoveryTimestamp,
    pub duration: Duration,
    pub closing_recovery_revision: RecoveryRevision,
}

impl RecoveryClosureReceipt {
    pub(crate) fn try_from_input(
        input: RecoveryClosureReceiptInput,
    ) -> Result<Self, RecoveryTransitionError> {
        let duration_ms = u64::try_from(input.duration.as_millis())
            .map_err(|_| RecoveryTransitionError::ClosureDurationOutOfRange)?;
        if duration_ms > MAX_DURATION_MS {
            return Err(RecoveryTransitionError::ClosureDurationOutOfRange);
        }
        Ok(Self {
            incident_id: input.incident_id,
            attempt_id: input.attempt_id,
            attempt_count: input.attempt_count,
            scan_count: input.scan_count,
            activity_watermark: input.activity_watermark,
            authoritative_scan_revision: input.authoritative_scan_revision,
            native_boundary: input.native_boundary,
            started_at: input.started_at,
            completed_at: input.completed_at,
            duration_ms,
            closing_recovery_revision: input.closing_recovery_revision,
        })
    }

    pub fn incident_id(&self) -> IncidentId {
        self.incident_id
    }

    pub fn attempt_id(&self) -> AttemptId {
        self.attempt_id
    }

    pub fn attempt_count(&self) -> RecoveryCount {
        self.attempt_count
    }

    pub fn scan_count(&self) -> RecoveryCount {
        self.scan_count
    }

    pub fn activity_watermark(&self) -> ActivityWatermark {
        self.activity_watermark
    }

    pub fn authoritative_scan_revision(&self) -> RecoveryScanRevision {
        self.authoritative_scan_revision
    }

    pub fn native_boundary(&self) -> &AttemptCoverageBoundary {
        &self.native_boundary
    }

    pub fn started_at(&self) -> RecoveryTimestamp {
        self.started_at
    }

    pub fn completed_at(&self) -> RecoveryTimestamp {
        self.completed_at
    }

    pub fn duration_ms(&self) -> u64 {
        self.duration_ms
    }

    pub fn closing_recovery_revision(&self) -> RecoveryRevision {
        self.closing_recovery_revision
    }
}
