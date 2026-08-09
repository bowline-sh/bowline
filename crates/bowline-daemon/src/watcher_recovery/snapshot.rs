use serde::Serialize;

use crate::watcher_coverage::{DarwinCoverageStart, WatcherCoverageBoundary, WatcherCoverageLoss};

use super::types::{
    ActivityWatermark, AttemptCoverageBoundary, AttemptId, DependencyFailure,
    DependencyFailureClass, IncidentId, NativeAdapter, RecoveryCause, RecoveryCount,
    RecoveryFailureCode, RecoveryLifecycle, RecoveryPhase, RecoveryProcessIdentity,
    RecoveryRevision, RecoveryScanRevision, RecoveryTimestamp, RecoveryWorkerId,
};

const DOCUMENT_KIND: &str = "watcher-recovery-snapshot";
const SCHEMA_VERSION: u8 = 3;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CurrentAttemptSnapshot {
    attempt_id: AttemptId,
    activity_watermark: Option<ActivityWatermark>,
    started_at: RecoveryTimestamp,
    authoritative_scan_revision: Option<RecoveryScanRevision>,
}

impl CurrentAttemptSnapshot {
    pub(crate) fn new(
        attempt_id: AttemptId,
        activity_watermark: Option<ActivityWatermark>,
        started_at: RecoveryTimestamp,
        authoritative_scan_revision: Option<RecoveryScanRevision>,
    ) -> Self {
        Self {
            attempt_id,
            activity_watermark,
            started_at,
            authoritative_scan_revision,
        }
    }

    pub fn attempt_id(&self) -> AttemptId {
        self.attempt_id
    }

    pub fn activity_watermark(&self) -> Option<ActivityWatermark> {
        self.activity_watermark
    }

    pub fn started_at(&self) -> RecoveryTimestamp {
        self.started_at
    }

    pub fn authoritative_scan_revision(&self) -> Option<RecoveryScanRevision> {
        self.authoritative_scan_revision
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "platform", rename_all = "snake_case")]
pub enum NativeCoverageSnapshot {
    Darwin {
        #[serde(rename = "boundaryId")]
        boundary_id: u64,
        #[serde(rename = "activityWatermark")]
        activity_watermark: ActivityWatermark,
        #[serde(rename = "coveredEpoch")]
        covered_epoch: u64,
        #[serde(rename = "liveEpoch")]
        live_epoch: u64,
        start: DarwinCoverageStartSnapshot,
        #[serde(rename = "historyThrough")]
        history_through: u64,
        #[serde(rename = "historyDone")]
        history_done: bool,
        #[serde(rename = "mustScanSubdirs")]
        must_scan_subdirs: bool,
        #[serde(rename = "sealedThrough")]
        sealed_through: u64,
        #[serde(rename = "flushGeneration")]
        flush_generation: u64,
        #[serde(rename = "lossGeneration")]
        loss_generation: u64,
        #[serde(rename = "callbackGeneration")]
        callback_generation: u64,
    },
    Linux {
        #[serde(rename = "boundaryId")]
        boundary_id: u64,
        #[serde(rename = "activityWatermark")]
        activity_watermark: ActivityWatermark,
        #[serde(rename = "streamEpoch")]
        stream_epoch: u64,
        #[serde(rename = "watcherReadyControlId")]
        watcher_ready_control_id: u64,
        #[serde(rename = "callbackDrainControlId")]
        callback_drain_control_id: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DarwinCoverageStartSnapshot {
    CursorReplay {
        #[serde(rename = "coveredLastSafe")]
        covered_last_safe: u64,
        #[serde(rename = "replayFrom")]
        replay_from: u64,
        #[serde(rename = "recoveryCause")]
        recovery_cause: Option<NativeCoverageLossSnapshot>,
    },
    FreshStream {
        #[serde(rename = "freshFrom")]
        fresh_from: u64,
        discontinuity: NativeCoverageLossSnapshot,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeCoverageLossSnapshot {
    UserDropped,
    KernelDropped,
    EventIdsWrapped,
    RootChanged,
    StreamStopped,
    NonMonotonicCursor,
    QueueOverflow,
    BackendFailure,
}

impl NativeCoverageSnapshot {
    pub(crate) fn from_boundary(boundary: &AttemptCoverageBoundary) -> Self {
        let activity_watermark = boundary.activity_watermark();
        match boundary.proof() {
            WatcherCoverageBoundary::Darwin(proof) => Self::Darwin {
                boundary_id: proof.boundary_id().get(),
                activity_watermark,
                covered_epoch: proof.covered_epoch().get(),
                live_epoch: proof.live_epoch().get(),
                start: DarwinCoverageStartSnapshot::from_native(proof.start()),
                history_through: proof.history_through().get(),
                history_done: true,
                must_scan_subdirs: proof.must_scan_subdirs(),
                sealed_through: proof.sealed_through().get(),
                flush_generation: proof.flush_generation().get(),
                loss_generation: proof.loss_generation().get(),
                callback_generation: proof.callback_generation().get(),
            },
            WatcherCoverageBoundary::Linux(proof) => Self::Linux {
                boundary_id: proof.boundary_id().get(),
                activity_watermark,
                stream_epoch: proof.stream_epoch().get(),
                watcher_ready_control_id: proof.watcher_ready().control_id().get(),
                callback_drain_control_id: proof.callback_drain().control_id().get(),
            },
        }
    }

    pub fn adapter(&self) -> NativeAdapter {
        match self {
            Self::Darwin { .. } => NativeAdapter::Darwin,
            Self::Linux { .. } => NativeAdapter::Linux,
        }
    }

    pub fn activity_watermark(&self) -> ActivityWatermark {
        match self {
            Self::Darwin {
                activity_watermark, ..
            }
            | Self::Linux {
                activity_watermark, ..
            } => *activity_watermark,
        }
    }

    pub fn boundary_id(&self) -> u64 {
        match self {
            Self::Darwin { boundary_id, .. } | Self::Linux { boundary_id, .. } => *boundary_id,
        }
    }
}

impl DarwinCoverageStartSnapshot {
    fn from_native(start: DarwinCoverageStart) -> Self {
        match start {
            DarwinCoverageStart::CursorReplay {
                covered_last_safe,
                replay_from,
                recovery_cause,
            } => Self::CursorReplay {
                covered_last_safe: covered_last_safe.get(),
                replay_from: replay_from.get(),
                recovery_cause: recovery_cause.map(NativeCoverageLossSnapshot::from_native),
            },
            DarwinCoverageStart::FreshStream {
                fresh_from,
                discontinuity,
            } => Self::FreshStream {
                fresh_from: fresh_from.get(),
                discontinuity: NativeCoverageLossSnapshot::from_native(discontinuity),
            },
        }
    }
}

impl NativeCoverageLossSnapshot {
    const fn from_native(loss: WatcherCoverageLoss) -> Self {
        match loss {
            WatcherCoverageLoss::UserDropped => Self::UserDropped,
            WatcherCoverageLoss::KernelDropped => Self::KernelDropped,
            WatcherCoverageLoss::EventIdsWrapped => Self::EventIdsWrapped,
            WatcherCoverageLoss::RootChanged => Self::RootChanged,
            WatcherCoverageLoss::StreamStopped => Self::StreamStopped,
            WatcherCoverageLoss::NonMonotonicCursor => Self::NonMonotonicCursor,
            WatcherCoverageLoss::QueueOverflow => Self::QueueOverflow,
            WatcherCoverageLoss::BackendFailure => Self::BackendFailure,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecoveryFailureSnapshot {
    class: DependencyFailureClass,
    code: RecoveryFailureCode,
    #[serde(rename = "observedAt")]
    observed_at: RecoveryTimestamp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryClosureSnapshot {
    incident_id: IncidentId,
    closing_attempt_id: AttemptId,
    attempt_count: RecoveryCount,
    scan_count: RecoveryCount,
    activity_watermark: ActivityWatermark,
    duration_ms: u64,
    closing_recovery_revision: RecoveryRevision,
    native_coverage: NativeCoverageSnapshot,
    authoritative_scan_revision: RecoveryScanRevision,
}

impl RecoveryClosureSnapshot {
    pub(crate) fn from_receipt(receipt: &super::types::RecoveryClosureReceipt) -> Self {
        Self {
            incident_id: receipt.incident_id(),
            closing_attempt_id: receipt.attempt_id(),
            attempt_count: receipt.attempt_count(),
            scan_count: receipt.scan_count(),
            activity_watermark: receipt.activity_watermark(),
            duration_ms: receipt.duration_ms(),
            closing_recovery_revision: receipt.closing_recovery_revision(),
            native_coverage: NativeCoverageSnapshot::from_boundary(receipt.native_boundary()),
            authoritative_scan_revision: receipt.authoritative_scan_revision(),
        }
    }

    pub fn incident_id(&self) -> IncidentId {
        self.incident_id
    }

    pub fn closing_attempt_id(&self) -> AttemptId {
        self.closing_attempt_id
    }

    pub fn attempt_count(&self) -> RecoveryCount {
        self.attempt_count
    }

    pub fn scan_count(&self) -> RecoveryCount {
        self.scan_count
    }
}

impl RecoveryFailureSnapshot {
    pub(crate) fn from_failure(failure: &DependencyFailure) -> Self {
        Self {
            class: failure.class(),
            code: failure.code(),
            observed_at: failure.observed_at(),
        }
    }

    pub fn class(&self) -> DependencyFailureClass {
        self.class
    }

    pub fn code(&self) -> &RecoveryFailureCode {
        &self.code
    }

    pub fn observed_at(&self) -> RecoveryTimestamp {
        self.observed_at
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WatcherRecoverySnapshot {
    document_kind: &'static str,
    schema_version: u8,
    process_identity: RecoveryProcessIdentity,
    workspace_id: bowline_core::ids::WorkspaceId,
    snapshot_revision: RecoveryRevision,
    activity_watermark: ActivityWatermark,
    lifecycle: RecoveryLifecycle,
    worker_id: Option<RecoveryWorkerId>,
    incident_id: Option<IncidentId>,
    primary_cause: Option<RecoveryCause>,
    phase: Option<RecoveryPhase>,
    started_at: Option<RecoveryTimestamp>,
    last_transition_at: RecoveryTimestamp,
    attempt_count: RecoveryCount,
    scan_count: RecoveryCount,
    rescan_required: bool,
    current_attempt: Option<CurrentAttemptSnapshot>,
    native_coverage: Option<NativeCoverageSnapshot>,
    failure: Option<RecoveryFailureSnapshot>,
    last_closure: Option<RecoveryClosureSnapshot>,
}

pub(crate) struct RecoverySnapshotInput {
    pub process_identity: RecoveryProcessIdentity,
    pub workspace_id: bowline_core::ids::WorkspaceId,
    pub snapshot_revision: RecoveryRevision,
    pub activity_watermark: ActivityWatermark,
    pub lifecycle: RecoveryLifecycle,
    pub worker_id: Option<RecoveryWorkerId>,
    pub incident_id: Option<IncidentId>,
    pub primary_cause: Option<RecoveryCause>,
    pub phase: Option<RecoveryPhase>,
    pub started_at: Option<RecoveryTimestamp>,
    pub last_transition_at: RecoveryTimestamp,
    pub attempt_count: RecoveryCount,
    pub scan_count: RecoveryCount,
    pub rescan_required: bool,
    pub current_attempt: Option<CurrentAttemptSnapshot>,
    pub native_coverage: Option<NativeCoverageSnapshot>,
    pub failure: Option<RecoveryFailureSnapshot>,
    pub last_closure: Option<RecoveryClosureSnapshot>,
}

impl WatcherRecoverySnapshot {
    pub(crate) fn from_input(input: RecoverySnapshotInput) -> Self {
        Self {
            document_kind: DOCUMENT_KIND,
            schema_version: SCHEMA_VERSION,
            process_identity: input.process_identity,
            workspace_id: input.workspace_id,
            snapshot_revision: input.snapshot_revision,
            activity_watermark: input.activity_watermark,
            lifecycle: input.lifecycle,
            worker_id: input.worker_id,
            incident_id: input.incident_id,
            primary_cause: input.primary_cause,
            phase: input.phase,
            started_at: input.started_at,
            last_transition_at: input.last_transition_at,
            attempt_count: input.attempt_count,
            scan_count: input.scan_count,
            rescan_required: input.rescan_required,
            current_attempt: input.current_attempt,
            native_coverage: input.native_coverage,
            failure: input.failure,
            last_closure: input.last_closure,
        }
    }

    pub fn document_kind(&self) -> &'static str {
        self.document_kind
    }

    pub fn schema_version(&self) -> u8 {
        self.schema_version
    }

    pub fn process_identity(&self) -> &RecoveryProcessIdentity {
        &self.process_identity
    }

    pub fn workspace_id(&self) -> &bowline_core::ids::WorkspaceId {
        &self.workspace_id
    }

    pub fn snapshot_revision(&self) -> RecoveryRevision {
        self.snapshot_revision
    }

    pub fn recovery_revision(&self) -> RecoveryRevision {
        self.snapshot_revision
    }

    pub fn activity_watermark(&self) -> ActivityWatermark {
        self.activity_watermark
    }

    pub fn lifecycle(&self) -> RecoveryLifecycle {
        self.lifecycle
    }

    pub fn last_closure(&self) -> Option<&RecoveryClosureSnapshot> {
        self.last_closure.as_ref()
    }

    pub fn worker_id(&self) -> Option<RecoveryWorkerId> {
        self.worker_id
    }

    pub fn incident_id(&self) -> Option<IncidentId> {
        self.incident_id
    }

    pub fn primary_cause(&self) -> Option<RecoveryCause> {
        self.primary_cause
    }

    pub fn phase(&self) -> Option<RecoveryPhase> {
        self.phase
    }

    pub fn started_at(&self) -> Option<RecoveryTimestamp> {
        self.started_at
    }

    pub fn last_transition_at(&self) -> RecoveryTimestamp {
        self.last_transition_at
    }

    pub fn attempt_count(&self) -> RecoveryCount {
        self.attempt_count
    }

    pub fn scan_count(&self) -> RecoveryCount {
        self.scan_count
    }

    pub fn rescan_required(&self) -> bool {
        self.rescan_required
    }

    pub fn current_attempt(&self) -> Option<&CurrentAttemptSnapshot> {
        self.current_attempt.as_ref()
    }

    pub fn native_coverage(&self) -> Option<&NativeCoverageSnapshot> {
        self.native_coverage.as_ref()
    }

    pub fn failure(&self) -> Option<&RecoveryFailureSnapshot> {
        self.failure.as_ref()
    }
}
