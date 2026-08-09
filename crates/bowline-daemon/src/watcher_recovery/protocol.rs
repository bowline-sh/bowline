use std::{fmt, time::Duration};

use super::types::{
    ActivityWatermark, AttemptId, DependencyFailureClass, IncidentId, RecoveryClosureReceipt,
    RecoveryInstant, RecoveryPhase, RecoveryRevision, RecoverySourceIdentity,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackoffPolicy {
    initial: Duration,
    maximum: Duration,
}

impl BackoffPolicy {
    pub fn new(initial: Duration, maximum: Duration) -> Result<Self, RecoveryTypeError> {
        if initial.is_zero() || maximum < initial {
            return Err(RecoveryTypeError::InvalidBackoffPolicy);
        }
        Ok(Self { initial, maximum })
    }

    /// Attempts are cheap and safe to repeat under load, because closure is
    /// fenced independently of how often it is offered. A thirty second ceiling
    /// let a failure streak during a burst park recovery straight through the
    /// quiet that follows it, which is the moment the covering scan would have
    /// succeeded -- the same starvation `RECOVERY_ATTEMPT_DEBOUNCE_CEILING`
    /// bounds from the other side.
    pub fn standard() -> Self {
        Self {
            initial: Duration::from_millis(250),
            maximum: Duration::from_secs(5),
        }
    }

    pub fn initial(self) -> Duration {
        self.initial
    }

    pub fn maximum(self) -> Duration {
        self.maximum
    }

    pub(crate) fn delay_for(self, consecutive_failures: u32) -> Duration {
        let exponent = consecutive_failures.saturating_sub(1).min(31);
        self.initial
            .saturating_mul(1_u32 << exponent)
            .min(self.maximum)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityAdmission {
    Nominal,
    /// Seen while an incident was open, and forwarded normally. Does not
    /// invalidate the covering scan: a forwarded event was never lost.
    ObservedDuringIncident {
        incident_id: IncidentId,
    },
    /// Dropped while an incident was open, so the covering scan must be redone.
    /// This is the admission that keeps a suppressed write from being closed
    /// over.
    CoverageInvalidated {
        incident_id: IncidentId,
    },
    BlockedIncidentAdvanced {
        incident_id: IncidentId,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LossAdmission {
    Opened { incident_id: IncidentId },
    Coalesced { incident_id: IncidentId },
    BlockedIncidentUpdated { incident_id: IncidentId },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailureDisposition {
    RetryScheduled {
        incident_id: IncidentId,
        retry_at: RecoveryInstant,
        delay: Duration,
    },
    Blocked {
        incident_id: IncidentId,
        class: DependencyFailureClass,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseDisposition {
    Closed(Box<RecoveryClosureReceipt>),
    RetryRequired { incident_id: IncidentId },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorityRestoration {
    Restored { incident_id: IncidentId },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryClosureIdentity {
    incident_id: IncidentId,
    attempt_id: AttemptId,
    closing_recovery_revision: RecoveryRevision,
}

impl RecoveryClosureIdentity {
    pub(crate) fn from_receipt(receipt: &RecoveryClosureReceipt) -> Self {
        Self {
            incident_id: receipt.incident_id(),
            attempt_id: receipt.attempt_id(),
            closing_recovery_revision: receipt.closing_recovery_revision(),
        }
    }

    pub fn incident_id(self) -> IncidentId {
        self.incident_id
    }

    pub fn attempt_id(self) -> AttemptId {
        self.attempt_id
    }

    pub fn closing_recovery_revision(self) -> RecoveryRevision {
        self.closing_recovery_revision
    }
}

/// An exact process-local point that can only be captured while recovery is
/// nominal. Unit 3 composes its engine and observer barriers around this token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryFrontier {
    recovery_revision: RecoveryRevision,
    activity_watermark: ActivityWatermark,
    last_closure: Option<RecoveryClosureIdentity>,
}

impl RecoveryFrontier {
    pub(crate) fn new(
        recovery_revision: RecoveryRevision,
        activity_watermark: ActivityWatermark,
        last_closure: Option<RecoveryClosureIdentity>,
    ) -> Self {
        Self {
            recovery_revision,
            activity_watermark,
            last_closure,
        }
    }

    pub fn recovery_revision(self) -> RecoveryRevision {
        self.recovery_revision
    }

    pub fn activity_watermark(self) -> ActivityWatermark {
        self.activity_watermark
    }

    pub fn last_closure(self) -> Option<RecoveryClosureIdentity> {
        self.last_closure
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryAttestation {
    source_identity: RecoverySourceIdentity,
    frontier: RecoveryFrontier,
    last_closure: Option<RecoveryClosureReceipt>,
}

impl RecoveryAttestation {
    pub(crate) fn new(
        source_identity: RecoverySourceIdentity,
        frontier: RecoveryFrontier,
        last_closure: Option<RecoveryClosureReceipt>,
    ) -> Self {
        Self {
            source_identity,
            frontier,
            last_closure,
        }
    }

    pub fn source_identity(&self) -> &RecoverySourceIdentity {
        &self.source_identity
    }

    pub fn frontier(&self) -> RecoveryFrontier {
        self.frontier
    }

    pub fn last_closure(&self) -> Option<&RecoveryClosureReceipt> {
        self.last_closure.as_ref()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryIdentifierKind {
    Incident,
    Attempt,
    Worker,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryTypeError {
    ZeroIdentifier { kind: &'static str },
    SchemaIntegerOutOfRange { field: &'static str, value: u64 },
    RecoveryCountOutOfRange { value: u64 },
    InvalidTimestamp,
    InvalidManifestKey,
    HeadVersionMustBePositive,
    InvalidBackoffPolicy,
}

impl fmt::Display for RecoveryTypeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroIdentifier { kind } => write!(formatter, "{kind} must be non-zero"),
            Self::SchemaIntegerOutOfRange { field, value } => {
                write!(
                    formatter,
                    "{field} value {value} exceeds the schema integer range"
                )
            }
            Self::RecoveryCountOutOfRange { value } => {
                write!(formatter, "recovery count {value} exceeds the schema range")
            }
            Self::InvalidTimestamp => formatter.write_str("recovery timestamp is not RFC 3339"),
            Self::InvalidManifestKey => {
                formatter.write_str("manifest key must use the m_<64 lowercase hex> format")
            }
            Self::HeadVersionMustBePositive => {
                formatter.write_str("an authoritative head version must be positive")
            }
            Self::InvalidBackoffPolicy => formatter.write_str(
                "backoff initial delay must be positive and no greater than its maximum",
            ),
        }
    }
}

impl std::error::Error for RecoveryTypeError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryTransitionError {
    IdentifierExhausted {
        kind: RecoveryIdentifierKind,
    },
    ActivityWatermarkExhausted,
    RecoveryRevisionExhausted,
    RecoveryCountExhausted {
        field: &'static str,
    },
    NoOpenIncident,
    LifecycleBlocked,
    AttemptAlreadyInFlight,
    NoAttemptInFlight,
    AttemptMismatch,
    WorkerUnavailable,
    WorkerMismatch,
    NativeBoundaryMismatch,
    NativeBoundaryNotMonotonic,
    NativeStreamEpochRegressed,
    ScanRevisionMismatch,
    RetainedFailureMismatch,
    OutOfOrder {
        operation: &'static str,
        phase: RecoveryPhase,
    },
    RetryableFailureRequired,
    TerminalFailureRequired,
    RetryNotDue,
    AuthorityNotRestorable,
    MonotonicTimeReversed,
    RetryDeadlineOverflow,
    ClosureDurationOutOfRange,
    RecoveryNotNominal,
    RecoveryFrontierChanged,
}

impl fmt::Display for RecoveryTransitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IdentifierExhausted { kind } => {
                write!(formatter, "{kind:?} identifier space is exhausted")
            }
            Self::ActivityWatermarkExhausted => {
                formatter.write_str("activity watermark is exhausted")
            }
            Self::RecoveryRevisionExhausted => {
                formatter.write_str("recovery revision is exhausted")
            }
            Self::RecoveryCountExhausted { field } => {
                write!(formatter, "{field} recovery count is exhausted")
            }
            Self::NoOpenIncident => formatter.write_str("no recovery incident is open"),
            Self::LifecycleBlocked => formatter.write_str("recovery is blocked"),
            Self::AttemptAlreadyInFlight => {
                formatter.write_str("a recovery attempt is already in flight")
            }
            Self::NoAttemptInFlight => formatter.write_str("no recovery attempt is in flight"),
            Self::AttemptMismatch => formatter.write_str("recovery attempt does not match"),
            Self::WorkerUnavailable => formatter.write_str("no recovery worker owns this runtime"),
            Self::WorkerMismatch => formatter.write_str("recovery worker ownership is stale"),
            Self::NativeBoundaryMismatch => {
                formatter.write_str("native coverage boundary does not match the attempt")
            }
            Self::NativeBoundaryNotMonotonic => {
                formatter.write_str("native coverage boundary id did not advance")
            }
            Self::NativeStreamEpochRegressed => {
                formatter.write_str("native watcher stream epoch regressed")
            }
            Self::ScanRevisionMismatch => {
                formatter.write_str("authoritative scan revision does not match the attempt")
            }
            Self::RetainedFailureMismatch => {
                formatter.write_str("retained recovery failure no longer matches")
            }
            Self::OutOfOrder { operation, phase } => {
                write!(formatter, "{operation} is out of order during {phase:?}")
            }
            Self::RetryableFailureRequired => {
                formatter.write_str("this transition requires a retryable failure")
            }
            Self::TerminalFailureRequired => {
                formatter.write_str("this transition requires a terminal failure")
            }
            Self::RetryNotDue => formatter.write_str("recovery retry is not due"),
            Self::AuthorityNotRestorable => {
                formatter.write_str("blocked recovery does not await restored authority")
            }
            Self::MonotonicTimeReversed => {
                formatter.write_str("monotonic recovery time moved backwards")
            }
            Self::RetryDeadlineOverflow => {
                formatter.write_str("recovery retry deadline overflowed")
            }
            Self::ClosureDurationOutOfRange => {
                formatter.write_str("recovery closure duration exceeds the schema range")
            }
            Self::RecoveryNotNominal => formatter.write_str("recovery is not nominal"),
            Self::RecoveryFrontierChanged => {
                formatter.write_str("recovery frontier changed before linearization")
            }
        }
    }
}

impl std::error::Error for RecoveryTransitionError {}

#[cfg(test)]
mod backoff_tests {
    use super::*;

    // A failure streak during a burst must not park recovery straight through
    // the quiet that follows it: that quiet is when the covering scan would
    // finally succeed, and a ceiling long enough to span it starves recovery
    // from the opposite side to the attempt debounce.
    #[test]
    fn retryable_backoff_cannot_outlast_the_quiet_it_is_waiting_for() {
        assert!(
            BackoffPolicy::standard().maximum() <= Duration::from_secs(5),
            "a retryable dependency failure must retry inside the proof's edit budget"
        );
    }
}
