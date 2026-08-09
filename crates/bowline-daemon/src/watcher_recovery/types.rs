use std::{fmt, time::Duration};

use bowline_core::ids::WorkspaceId;
use serde::{Serialize, Serializer};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

mod native;
mod receipts;
mod refs;

pub use native::{AttemptCoverageBoundary, NativeAdapter};
pub(crate) use receipts::RecoveryClosureReceiptInput;
pub use receipts::{CloseOffer, RecoveryClosureReceipt};
pub use refs::{AuthoritativeRefIdentity, RefObservation};

pub use super::protocol::{
    ActivityAdmission, AuthorityRestoration, BackoffPolicy, CloseDisposition, FailureDisposition,
    LossAdmission, RecoveryAttestation, RecoveryClosureIdentity, RecoveryFrontier,
    RecoveryIdentifierKind, RecoveryTransitionError, RecoveryTypeError,
};

pub(crate) const MAX_SCHEMA_INTEGER: u64 = 9_007_199_254_740_991;
pub(crate) const MAX_RECOVERY_COUNT: u64 = 1_000_000;
pub(crate) const MAX_DURATION_MS: u64 = 31_536_000_000;

macro_rules! prefixed_id {
    ($name:ident, $prefix:literal) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u64);

        impl $name {
            pub fn new(value: u64) -> Result<Self, RecoveryTypeError> {
                if value == 0 {
                    return Err(RecoveryTypeError::ZeroIdentifier {
                        kind: stringify!($name),
                    });
                }
                Ok(Self(value))
            }

            pub fn get(self) -> u64 {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(formatter, concat!($prefix, ":{}"), self.0)
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.collect_str(self)
            }
        }
    };
}

prefixed_id!(IncidentId, "incident");
prefixed_id!(AttemptId, "attempt");
prefixed_id!(RecoveryWorkerId, "recovery-worker");
prefixed_id!(RecoveryProjectorId, "recovery-projector");
prefixed_id!(RecoveryProcessBootId, "process-boot");
prefixed_id!(RecoveryProcessSessionId, "process-session");

impl IncidentId {
    pub(crate) const fn from_valid(value: u64) -> Self {
        Self(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryProcessIdentity {
    boot_id: RecoveryProcessBootId,
    session_id: RecoveryProcessSessionId,
    started_at: RecoveryTimestamp,
}

impl RecoveryProcessIdentity {
    pub fn new(
        boot_id: RecoveryProcessBootId,
        session_id: RecoveryProcessSessionId,
        started_at: RecoveryTimestamp,
    ) -> Self {
        Self {
            boot_id,
            session_id,
            started_at,
        }
    }

    pub fn boot_id(&self) -> RecoveryProcessBootId {
        self.boot_id
    }

    pub fn session_id(&self) -> RecoveryProcessSessionId {
        self.session_id
    }

    pub fn started_at(&self) -> RecoveryTimestamp {
        self.started_at
    }
}

/// Closed process/workspace source tuple shared by recovery, engine, observer,
/// and exact-barrier evidence. A daemon restart allocates a new process
/// identity before any workspace coordinator is constructed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoverySourceIdentity {
    process_identity: RecoveryProcessIdentity,
    workspace_id: WorkspaceId,
}

impl RecoverySourceIdentity {
    pub fn new(process_identity: RecoveryProcessIdentity, workspace_id: WorkspaceId) -> Self {
        Self {
            process_identity,
            workspace_id,
        }
    }

    pub fn process_identity(&self) -> &RecoveryProcessIdentity {
        &self.process_identity
    }

    pub fn workspace_id(&self) -> &WorkspaceId {
        &self.workspace_id
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ActivityWatermark(u64);

impl ActivityWatermark {
    pub const INITIAL: Self = Self(0);

    pub fn new(value: u64) -> Result<Self, RecoveryTypeError> {
        if value > MAX_SCHEMA_INTEGER {
            return Err(RecoveryTypeError::SchemaIntegerOutOfRange {
                field: "activityWatermark",
                value,
            });
        }
        Ok(Self(value))
    }

    pub fn get(self) -> u64 {
        self.0
    }

    pub(crate) const fn from_valid(value: u64) -> Self {
        Self(value)
    }

    pub(crate) fn checked_next(self) -> Option<Self> {
        self.0
            .checked_add(1)
            .filter(|value| *value < MAX_SCHEMA_INTEGER)
            .map(Self)
    }

    pub(crate) const fn terminal() -> Self {
        Self(MAX_SCHEMA_INTEGER)
    }
}

/// Admissions of lost fidelity only: a suppressed drop, a saturated lane, a
/// collapsed ingress detail, a stream rescan flag, an adapter loss.
///
/// A close asks whether anything the covering scan was responsible for went
/// unobserved, which is exactly this. `ActivityWatermark` answers a different
/// question -- has anything at all been seen -- and exact barriers still fence
/// on that, because an event forwarded but not yet folded must invalidate a
/// linearized frontier. Fencing the close on it too meant a close could not
/// land while a user kept typing, which is when coverage matters most.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct LossWatermark(u64);

impl LossWatermark {
    pub const INITIAL: Self = Self(0);

    pub fn new(value: u64) -> Result<Self, RecoveryTypeError> {
        if value > MAX_SCHEMA_INTEGER {
            return Err(RecoveryTypeError::SchemaIntegerOutOfRange {
                field: "lossWatermark",
                value,
            });
        }
        Ok(Self(value))
    }

    pub fn get(self) -> u64 {
        self.0
    }

    pub(crate) const fn from_valid(value: u64) -> Self {
        Self(value)
    }

    pub(crate) fn checked_next(self) -> Option<Self> {
        self.0
            .checked_add(1)
            .filter(|value| *value < MAX_SCHEMA_INTEGER)
            .map(Self)
    }

    pub(crate) const fn terminal() -> Self {
        Self(MAX_SCHEMA_INTEGER)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct RecoveryRevision(u64);

impl RecoveryRevision {
    pub const INITIAL: Self = Self(0);

    pub fn new(value: u64) -> Result<Self, RecoveryTypeError> {
        if value > MAX_SCHEMA_INTEGER {
            return Err(RecoveryTypeError::SchemaIntegerOutOfRange {
                field: "snapshotRevision",
                value,
            });
        }
        Ok(Self(value))
    }

    pub fn get(self) -> u64 {
        self.0
    }

    pub(crate) const fn from_valid(value: u64) -> Self {
        Self(value)
    }

    pub(crate) fn checked_next(self) -> Option<Self> {
        self.0
            .checked_add(1)
            .filter(|value| *value < MAX_SCHEMA_INTEGER)
            .map(Self)
    }

    pub(crate) const fn terminal() -> Self {
        Self(MAX_SCHEMA_INTEGER)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct RecoveryScanRevision(u64);

impl RecoveryScanRevision {
    pub fn new(value: u64) -> Result<Self, RecoveryTypeError> {
        if value > MAX_SCHEMA_INTEGER {
            return Err(RecoveryTypeError::SchemaIntegerOutOfRange {
                field: "authoritativeScanRevision",
                value,
            });
        }
        Ok(Self(value))
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct RecoveryCount(u64);

impl RecoveryCount {
    pub const ZERO: Self = Self(0);
    pub(crate) const ONE: Self = Self(1);

    pub fn new(value: u64) -> Result<Self, RecoveryTypeError> {
        if value > MAX_RECOVERY_COUNT {
            return Err(RecoveryTypeError::RecoveryCountOutOfRange { value });
        }
        Ok(Self(value))
    }

    pub fn get(self) -> u64 {
        self.0
    }

    pub(crate) fn checked_next(self) -> Option<Self> {
        self.0
            .checked_add(1)
            .filter(|value| *value <= MAX_RECOVERY_COUNT)
            .map(Self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RecoveryInstant(u64);

impl RecoveryInstant {
    pub const ZERO: Self = Self(0);

    pub fn from_millis(value: u64) -> Self {
        Self(value)
    }

    pub fn as_millis(self) -> u64 {
        self.0
    }

    pub fn checked_add(self, duration: Duration) -> Option<Self> {
        let millis = u64::try_from(duration.as_millis()).ok()?;
        self.0.checked_add(millis).map(Self)
    }

    pub fn elapsed_since(self, earlier: Self) -> Option<Duration> {
        self.0.checked_sub(earlier.0).map(Duration::from_millis)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RecoveryTimestamp(OffsetDateTime);

impl RecoveryTimestamp {
    pub fn from_datetime(value: OffsetDateTime) -> Self {
        Self(value)
    }

    pub fn parse(value: &str) -> Result<Self, RecoveryTypeError> {
        OffsetDateTime::parse(value, &Rfc3339)
            .map(Self)
            .map_err(|_| RecoveryTypeError::InvalidTimestamp)
    }

    pub fn to_rfc3339(self) -> Result<String, RecoveryTypeError> {
        self.0
            .format(&Rfc3339)
            .map_err(|_| RecoveryTypeError::InvalidTimestamp)
    }
}

impl Serialize for RecoveryTimestamp {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let value = self.0.format(&Rfc3339).map_err(serde::ser::Error::custom)?;
        serializer.serialize_str(&value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryMoment {
    pub observed_at: RecoveryTimestamp,
    pub monotonic: RecoveryInstant,
}

impl RecoveryMoment {
    pub fn new(observed_at: RecoveryTimestamp, monotonic: RecoveryInstant) -> Self {
        Self {
            observed_at,
            monotonic,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryFailureCode {
    DependencyUnavailable,
    DependencyBusy,
    AuthenticationRequired,
    AuthorizationLost,
    IntegrityMismatch,
    ObserverStartUnavailable,
    ObserverInitialValueTimeout,
    ObserverStreamUnavailable,
    ObserverUnknownSigner,
    ObserverIntegrity,
    ObserverFatalContract,
    RecoveryWorkerIdExhausted,
    RecoveryActivityWatermarkExhausted,
    RecoveryCauseCountExhausted,
    RecoveryAttemptIdExhausted,
    RecoveryAttemptCountExhausted,
    RecoveryClosureDurationOutOfRange,
    RecoveryRetryCountExhausted,
    RecoveryRetryDeadlineOverflow,
    RecoveryIncidentIdExhausted,
    RecoveryRevisionExhausted,
    RecoveryScanCountExhausted,
}

impl RecoveryFailureCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DependencyUnavailable => "dependency_unavailable",
            Self::DependencyBusy => "dependency_busy",
            Self::AuthenticationRequired => "authentication_required",
            Self::AuthorizationLost => "authorization_lost",
            Self::IntegrityMismatch => "integrity_mismatch",
            Self::ObserverStartUnavailable => "observer_start_unavailable",
            Self::ObserverInitialValueTimeout => "observer_initial_value_timeout",
            Self::ObserverStreamUnavailable => "observer_stream_unavailable",
            Self::ObserverUnknownSigner => "observer_unknown_signer",
            Self::ObserverIntegrity => "observer_integrity",
            Self::ObserverFatalContract => "observer_fatal_contract",
            Self::RecoveryWorkerIdExhausted => "recovery_worker_id_exhausted",
            Self::RecoveryActivityWatermarkExhausted => "recovery_activity_watermark_exhausted",
            Self::RecoveryCauseCountExhausted => "recovery_cause_count_exhausted",
            Self::RecoveryAttemptIdExhausted => "recovery_attempt_id_exhausted",
            Self::RecoveryAttemptCountExhausted => "recovery_attempt_count_exhausted",
            Self::RecoveryClosureDurationOutOfRange => "recovery_closure_duration_out_of_range",
            Self::RecoveryRetryCountExhausted => "recovery_retry_count_exhausted",
            Self::RecoveryRetryDeadlineOverflow => "recovery_retry_deadline_overflow",
            Self::RecoveryIncidentIdExhausted => "recovery_incident_id_exhausted",
            Self::RecoveryRevisionExhausted => "recovery_revision_exhausted",
            Self::RecoveryScanCountExhausted => "recovery_scan_count_exhausted",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryCause {
    StartupReconciliation,
    NativeCallbackLaneSaturated,
    NativeEventBatchSaturated,
    IngressDetailCollapsed,
    NativeRescanRequired,
    RecoverableAdapterLoss,
    WatcherDisconnected,
    RootReplaced,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryLifecycle {
    Nominal,
    Recovering,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryPhase {
    Rearming,
    AwaitingCoverage,
    Scanning,
    AwaitingSeal,
    Closing,
    BackingOff,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyFailureClass {
    Retryable,
    AuthenticationRequired,
    AuthorizationLost,
    Integrity,
    FatalContract,
}

impl DependencyFailureClass {
    pub fn is_retryable(self) -> bool {
        self == Self::Retryable
    }

    pub fn is_authority_restorable(self) -> bool {
        matches!(self, Self::AuthenticationRequired | Self::AuthorizationLost)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyFailure {
    class: DependencyFailureClass,
    code: RecoveryFailureCode,
    observed_at: RecoveryTimestamp,
}

impl DependencyFailure {
    pub fn new(
        class: DependencyFailureClass,
        code: RecoveryFailureCode,
        observed_at: RecoveryTimestamp,
    ) -> Self {
        Self {
            class,
            code,
            observed_at,
        }
    }

    pub fn class(&self) -> DependencyFailureClass {
        self.class
    }

    pub fn code(&self) -> RecoveryFailureCode {
        self.code
    }

    pub fn observed_at(&self) -> RecoveryTimestamp {
        self.observed_at
    }

    pub(crate) fn fatal_contract(
        code: RecoveryFailureCode,
        observed_at: RecoveryTimestamp,
    ) -> Self {
        Self::new(DependencyFailureClass::FatalContract, code, observed_at)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttemptToken {
    incident_id: IncidentId,
    attempt_id: AttemptId,
    worker_id: RecoveryWorkerId,
}

impl AttemptToken {
    pub(crate) fn new(
        incident_id: IncidentId,
        attempt_id: AttemptId,
        worker_id: RecoveryWorkerId,
    ) -> Self {
        Self {
            incident_id,
            attempt_id,
            worker_id,
        }
    }

    pub fn incident_id(self) -> IncidentId {
        self.incident_id
    }

    pub fn attempt_id(self) -> AttemptId {
        self.attempt_id
    }

    pub fn worker_id(self) -> RecoveryWorkerId {
        self.worker_id
    }
}

/// Fences all work issued by one recovery-worker incarnation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryWorkerOwnership {
    worker_id: RecoveryWorkerId,
}

/// Fences one composite status/exact-barrier projector incarnation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryProjectorOwnership {
    projector_id: RecoveryProjectorId,
}

impl RecoveryProjectorOwnership {
    pub(crate) fn new(projector_id: RecoveryProjectorId) -> Self {
        Self { projector_id }
    }

    pub fn projector_id(self) -> RecoveryProjectorId {
        self.projector_id
    }
}

impl RecoveryWorkerOwnership {
    pub(crate) fn new(worker_id: RecoveryWorkerId) -> Self {
        Self { worker_id }
    }

    pub fn worker_id(self) -> RecoveryWorkerId {
        self.worker_id
    }
}
