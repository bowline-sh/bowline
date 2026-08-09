//! Typed waits for engine-internal convergence and the temporary v8 adapter.

use std::collections::BTreeMap;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bowline_control_plane::DependencyFailureClass;
use bowline_local::sync::manifest_engine::{
    EngineConvergenceBarrierId, EngineConvergenceReceipt, EngineEndpointGeneration,
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use super::{EngineBarrierCompletion, ObserverBarrierGuard, WorkspaceControlRegistry};
use crate::manifest_transport::{RefObserverFailureCode, RefObserverFrontier};
use crate::watcher_recovery::{
    DependencyFailureClass as RecoveryDependencyFailureClass, RecoveryFailureCode,
};

/// Why an exact sync barrier produced no snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncBarrierError {
    /// The engine slot exists but cannot answer right now.
    Unavailable { reason: &'static str },
    /// The observer cannot recover without a typed authority or integrity
    /// remediation. Retrying the same public barrier must not hide that action.
    ObserverBlocked {
        class: DependencyFailureClass,
        code: RefObserverFailureCode,
    },
    /// The workspace recovery protocol is waiting for explicit authority or
    /// integrity remediation. Time cannot make this exact barrier succeed.
    RecoveryBlocked {
        class: RecoveryDependencyFailureClass,
        code: RecoveryFailureCode,
    },
    /// This daemon serves no sync workspace at all.
    WorkspaceNotServed,
    /// The caller's cancellation predicate fired.
    Cancelled,
    /// The barrier did not converge before the caller's timeout.
    TimedOut,
    /// The engine stopped before completing the barrier.
    EngineStopped,
    /// Component provenance disagreed inside one daemon process. Retrying the
    /// same process cannot repair a forged or misbound receipt.
    FatalContract { reason: &'static str },
}

impl SyncBarrierError {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unavailable { reason } => reason,
            Self::ObserverBlocked { class, .. } => match class {
                DependencyFailureClass::Retryable => {
                    "remote manifest observer is temporarily unavailable"
                }
                DependencyFailureClass::AuthenticationRequired => {
                    "remote manifest observer requires authentication"
                }
                DependencyFailureClass::AuthorizationLost => {
                    "remote manifest observer requires renewed authorization"
                }
                DependencyFailureClass::Integrity => {
                    "remote manifest observer stopped on an integrity failure"
                }
                DependencyFailureClass::FatalContract => {
                    "remote manifest observer stopped on a contract failure"
                }
            },
            Self::RecoveryBlocked { class, .. } => match class {
                RecoveryDependencyFailureClass::Retryable => {
                    "watcher recovery dependency is temporarily unavailable"
                }
                RecoveryDependencyFailureClass::AuthenticationRequired => {
                    "watcher recovery requires authentication"
                }
                RecoveryDependencyFailureClass::AuthorizationLost => {
                    "watcher recovery requires renewed authorization"
                }
                RecoveryDependencyFailureClass::Integrity => {
                    "watcher recovery stopped on an integrity failure"
                }
                RecoveryDependencyFailureClass::FatalContract => {
                    "watcher recovery stopped on a contract failure"
                }
            },
            Self::WorkspaceNotServed => "daemon is not serving a sync workspace",
            Self::Cancelled => "the sync barrier request was cancelled",
            Self::TimedOut => "sync barrier did not converge before the deadline",
            Self::EngineStopped => "manifest sync engine stopped before the barrier completed",
            Self::FatalContract { reason } => reason,
        }
    }

    pub(super) fn from_engine(error: EngineConvergenceBarrierError) -> Self {
        match error {
            EngineConvergenceBarrierError::Unavailable { reason } => Self::Unavailable { reason },
            EngineConvergenceBarrierError::Cancelled => Self::Cancelled,
            EngineConvergenceBarrierError::TimedOut => Self::TimedOut,
            EngineConvergenceBarrierError::EngineStopped
            | EngineConvergenceBarrierError::IdentityExhausted
            | EngineConvergenceBarrierError::CompletionMismatch => Self::EngineStopped,
            EngineConvergenceBarrierError::ResourceExhausted => Self::Unavailable {
                reason: "public exact barrier registry is full",
            },
        }
    }
}

impl std::fmt::Display for SyncBarrierError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::error::Error for SyncBarrierError {}

/// Failures of the engine-owned barrier, before observer/recovery composition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineConvergenceBarrierError {
    Unavailable { reason: &'static str },
    Cancelled,
    TimedOut,
    EngineStopped,
    IdentityExhausted,
    CompletionMismatch,
    ResourceExhausted,
}

impl EngineConvergenceBarrierError {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unavailable { reason } => reason,
            Self::Cancelled => "engine convergence barrier was cancelled",
            Self::TimedOut => "engine convergence barrier did not finish before the deadline",
            Self::EngineStopped => "manifest sync engine stopped before convergence",
            Self::IdentityExhausted => "engine convergence identity space exhausted",
            Self::CompletionMismatch => "engine convergence completion identity did not match",
            Self::ResourceExhausted => "public exact barrier registry is full",
        }
    }
}

impl std::fmt::Display for EngineConvergenceBarrierError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::error::Error for EngineConvergenceBarrierError {}

/// How often an interruptible wait re-checks cancellation.
const BARRIER_POLL: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExactBarrierTimestamp(String);

impl ExactBarrierTimestamp {
    pub(super) fn now() -> Self {
        let value = OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string());
        Self(value)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Reactive completion handle for one engine-internal exact barrier.
pub struct EngineConvergenceBarrierWaiter {
    pub(super) id: EngineConvergenceBarrierId,
    pub(super) generation: EngineEndpointGeneration,
    pub(super) receiver: Receiver<EngineBarrierCompletion>,
    pub(super) pending:
        Arc<Mutex<BTreeMap<EngineConvergenceBarrierId, Sender<EngineBarrierCompletion>>>>,
    pub(super) controls: Arc<WorkspaceControlRegistry>,
}

impl EngineConvergenceBarrierWaiter {
    pub fn wait(
        self,
        timeout: Duration,
        cancelled: impl FnMut() -> bool,
    ) -> Result<EngineConvergenceReceipt, EngineConvergenceBarrierError> {
        let completion = wait_for_engine_barrier(&self.receiver, timeout, cancelled)?;
        if completion.receipt.barrier_id() != self.id
            || completion.receipt.endpoint_generation() != self.generation
        {
            return Err(EngineConvergenceBarrierError::CompletionMismatch);
        }
        Ok(completion.receipt)
    }
}

impl Drop for EngineConvergenceBarrierWaiter {
    fn drop(&mut self) {
        let should_cancel = self
            .pending
            .lock()
            .map(|mut pending| pending.remove(&self.id).is_some())
            .unwrap_or(true);
        if should_cancel {
            self.controls.cancel_barrier(self.id, self.generation);
        }
    }
}

/// Temporary v8 adapter pending public workspace-barrier composition.
pub struct SyncBarrierWaiter {
    pub(super) inner: EngineConvergenceBarrierWaiter,
    pub(super) observer: ObserverBarrierGuard,
    pub(super) engine_admitted_at: ExactBarrierTimestamp,
}

/// Exact public workspace convergence receipt. It binds the engine's durable
/// frontier to the observer frontier that remained live and unchanged across
/// the wait. Recovery closure is composed by the workspace runtime before RPC
/// serialization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineObserverConvergenceReceipt {
    engine: EngineConvergenceReceipt,
    observer_frontier: Option<RefObserverFrontier>,
    engine_admitted_at: ExactBarrierTimestamp,
    engine_completed_at: ExactBarrierTimestamp,
    observer_admitted_at: Option<ExactBarrierTimestamp>,
    observer_completed_at: Option<ExactBarrierTimestamp>,
}

impl EngineObserverConvergenceReceipt {
    pub const fn engine(&self) -> &EngineConvergenceReceipt {
        &self.engine
    }

    pub const fn observer_frontier(&self) -> Option<&RefObserverFrontier> {
        self.observer_frontier.as_ref()
    }

    pub const fn engine_admitted_at(&self) -> &ExactBarrierTimestamp {
        &self.engine_admitted_at
    }

    pub const fn engine_completed_at(&self) -> &ExactBarrierTimestamp {
        &self.engine_completed_at
    }

    pub const fn observer_admitted_at(&self) -> Option<&ExactBarrierTimestamp> {
        self.observer_admitted_at.as_ref()
    }

    pub const fn observer_completed_at(&self) -> Option<&ExactBarrierTimestamp> {
        self.observer_completed_at.as_ref()
    }
}

impl SyncBarrierWaiter {
    pub fn wait(
        self,
        timeout: Duration,
        cancelled: impl FnMut() -> bool,
    ) -> Result<EngineObserverConvergenceReceipt, SyncBarrierError> {
        let completion =
            wait_for_workspace_barrier(&self.inner.receiver, timeout, cancelled, &self.observer)?;
        if completion.receipt.barrier_id() != self.inner.id
            || completion.receipt.endpoint_generation() != self.inner.generation
        {
            return Err(SyncBarrierError::EngineStopped);
        }
        let engine_completed_at = ExactBarrierTimestamp::now();
        let observer_frontier = self.observer.validate_completion(&completion.receipt)?;
        let observer_completed_at = observer_frontier
            .as_ref()
            .map(|_| ExactBarrierTimestamp::now());
        Ok(EngineObserverConvergenceReceipt {
            engine: completion.receipt,
            observer_frontier,
            engine_admitted_at: self.engine_admitted_at,
            engine_completed_at,
            observer_admitted_at: self.observer.admitted_at().cloned(),
            observer_completed_at,
        })
    }
}

fn wait_for_engine_barrier(
    receiver: &Receiver<EngineBarrierCompletion>,
    timeout: Duration,
    mut cancelled: impl FnMut() -> bool,
) -> Result<EngineBarrierCompletion, EngineConvergenceBarrierError> {
    let Some(deadline) = std::time::Instant::now().checked_add(timeout) else {
        return Err(EngineConvergenceBarrierError::TimedOut);
    };
    loop {
        if cancelled() {
            return Err(EngineConvergenceBarrierError::Cancelled);
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(EngineConvergenceBarrierError::TimedOut);
        }
        match receiver.recv_timeout(remaining.min(BARRIER_POLL)) {
            Ok(completion) => return Ok(completion),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return Err(EngineConvergenceBarrierError::EngineStopped);
            }
        }
    }
}

fn wait_for_workspace_barrier(
    receiver: &Receiver<EngineBarrierCompletion>,
    timeout: Duration,
    mut cancelled: impl FnMut() -> bool,
    observer: &ObserverBarrierGuard,
) -> Result<EngineBarrierCompletion, SyncBarrierError> {
    let Some(deadline) = std::time::Instant::now().checked_add(timeout) else {
        return Err(SyncBarrierError::TimedOut);
    };
    loop {
        observer.require_unchanged()?;
        if cancelled() {
            return Err(SyncBarrierError::Cancelled);
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(SyncBarrierError::TimedOut);
        }
        match receiver.recv_timeout(remaining.min(BARRIER_POLL)) {
            Ok(completion) => {
                observer.require_unchanged()?;
                return Ok(completion);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Err(SyncBarrierError::EngineStopped),
        }
    }
}
