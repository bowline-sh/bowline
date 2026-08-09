//! What the daemon tells the engine, and what one command answers.
//!
//! Split from `mod.rs` at the seam between the engine's vocabulary and its
//! driver: every value here crosses the thread boundary between the daemon and
//! the engine, and none of them carries durable authority. Paths are re-derived
//! from disk; a ref observation is a freshness-checked hint; a confirmation
//! names no batch of its own.

use std::collections::BTreeSet;

use super::{RefObservation, WorkspacePath};

/// Why a full stat walk was demanded. A lost watcher event, an overflow, a
/// disconnect, or a root replacement all reduce to the same cheap recovery: one
/// stat-only pass. The variant is carried so the snapshot can explain the state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FullScanReason {
    WatcherOverflow,
    IngressDetailCollapsed,
    WatcherDisconnected,
    RootReplaced,
    PeriodicAudit,
    /// An explicit caller boundary: re-observe disk and the hosted ref before
    /// acknowledging that sync is caught up.
    EngineConvergenceBarrier,
}

/// Opaque identity for one engine-internal convergence barrier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct EngineConvergenceBarrierId(pub u64);

/// Identity of the daemon engine endpoint that admitted a barrier.
///
/// A restarted engine may reach the same engine revision as its predecessor;
/// binding every receipt to this separately allocated generation prevents a
/// stale completion from being mistaken for convergence of the replacement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct EngineEndpointGeneration(pub u64);

/// The events the daemon (Plan 111) feeds the engine. No event carries durable
/// authority: paths are re-derived from disk, while a verified ref observation
/// is only a freshness-checked hint for a scheduled pull.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineEvent {
    /// Watcher-reported paths to re-observe (re-observed even on a stat match).
    Paths(BTreeSet<WorkspacePath>),
    /// Watcher-reported directory roots whose current descendants must be
    /// discovered after the normal burst debounce.
    RecursivePaths(BTreeSet<WorkspacePath>),
    /// The watcher lost fidelity; fall back to a full stat walk immediately.
    FullScanRequired(FullScanReason),
    /// The ref subscription fired: pull and reconcile.
    RefChanged,
    /// The ref subscription delivered a signature-verified real head. The
    /// engine may consume this hint instead of repeating the same hosted query.
    RefObserved(RefObservation),
    /// The network came back; retry any pending work now, preempting backoff.
    ConnectivityRestored,
    /// Re-observe both authorities and acknowledge this exact request only after
    /// the resulting work has settled.
    EngineConvergenceBarrier {
        id: EngineConvergenceBarrierId,
        endpoint_generation: EngineEndpointGeneration,
    },
    /// Withdraw an exact barrier whose requester stopped waiting. The endpoint
    /// generation prevents a stale waiter from cancelling an identically
    /// numbered request admitted by a replacement engine.
    CancelEngineConvergenceBarrier {
        id: EngineConvergenceBarrierId,
        endpoint_generation: EngineEndpointGeneration,
    },
    /// An operator authorised the currently refused removal batch. Carries no
    /// payload: what is authorised is whatever the engine is refusing right now,
    /// never a batch the caller describes.
    ConfirmMassDeletion,
    /// Stop the run loop.
    Shutdown,
}

/// What one confirmation actually authorised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeletionConfirmation {
    /// One push may now publish the refused batch, which had these counts.
    Authorized { removals: usize, entries: usize },
    /// Nothing was refused, so nothing was authorised. Deliberately not an
    /// error: confirming an already-cleared block is a no-op, not a failure.
    NotBlocked,
}
