//! How a failed cycle is classified, and the driver-level error that is left
//! when a failure is a genuine invariant violation rather than a condition the
//! engine recovers from.
//!
//! The classification is the whole point of this module: before it, every
//! non-transport failure — one unreadable file, one oversize file, one
//! unnormalizable filename — became `Fatal` and killed the engine thread.

use std::error::Error;
use std::fmt;

use super::{CycleError, ManifestStoreError, PullError, PushError};

/// Classify a failed push.
///
/// Deliberately written without a `_ =>` arm. A wildcard here means every future
/// variant defaults to `Fatal`, which is how a per-path condition acquires the
/// authority to kill the engine by omission. Push's own path-scoped channel is a
/// VALUE, not an error (`PathScan::Unsyncable`), so every variant listed below is
/// a genuine fault — but adding one must remain a decision, not a default.
pub(super) fn push_cycle_error(error: PushError) -> CycleError {
    match error {
        PushError::Transport(_) => CycleError::Transport,
        PushError::MassDeletionRefused {
            removals, entries, ..
        } => CycleError::MassDeletionBlocked { removals, entries },
        error @ (PushError::Io(_)
        | PushError::Store(_)
        | PushError::Manifest(_)
        | PushError::AncestorRowMissing { .. }
        | PushError::StreamSealUnsupported { .. }) => CycleError::Fatal(EngineError::Push(error)),
    }
}

/// What a failed push means for the REST of the cycle.
///
/// Refusing to publish and being unable to publish are different facts, and
/// collapsing them is what makes a blocked device go dark: a device parked on
/// the deletion breaker was skipping the pull that would have told it the remote
/// had already resolved those very paths, so the one thing that could clear the
/// block was the one thing the block prevented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PushFailureScope {
    /// Nothing is broken and nothing was published: the batch is waiting on a
    /// human. Receiving is still correct — and is how the wait usually ends.
    ParksPublishing,
    /// The transport, the store, or an engine invariant is faulted. A pull in the
    /// same cycle would either fail the same way or act on state we cannot trust,
    /// so the cycle ends here and the driver's own recovery takes over.
    StopsCycle,
}

impl CycleError {
    /// Exhaustive for the same reason as the classifiers above: a future variant
    /// must not acquire permission to keep pulling — or to stop the cycle — by
    /// falling into a wildcard nobody revisited.
    pub(super) fn push_failure_scope(&self) -> PushFailureScope {
        match self {
            Self::MassDeletionBlocked { .. } => PushFailureScope::ParksPublishing,
            Self::Transport
            | Self::Integrity
            | Self::RootUnavailable(_)
            | Self::PathScoped
            | Self::Fatal(_) => PushFailureScope::StopsCycle,
        }
    }
}

pub(super) fn pull_cycle_error(error: PullError) -> CycleError {
    classify_pull_error(&error).map_fatal(|| EngineError::Pull(error))
}

/// Classify a failed pull.
///
/// Also exhaustive, and for the same reason. The type carries the distinction
/// this function used to have to guess at: [`PullError::Path`] is a fact about
/// one workspace path and can never be fatal, while the remaining variants are
/// faults of the engine, its store, or the network. Before that split, the arm
/// here read `_ => Fatal(Internal)`, so every new call site that reached for the
/// convenient `PullError::Io` silently acquired the power to stop all sync — and,
/// behind a durable intent, to fail every startup after it.
pub(super) fn classify_pull_error(error: &PullError) -> CycleError {
    match error {
        PullError::Transport(_) => CycleError::Transport,
        PullError::RefRegressed { .. } | PullError::RefForked { .. } => CycleError::Integrity,
        PullError::Path(_) => CycleError::PathScoped,
        PullError::EngineScratchIo(_)
        | PullError::Store(_)
        | PullError::Manifest(_)
        | PullError::Push(_)
        | PullError::ManifestKeyMismatch
        | PullError::BlobKeyMismatch
        | PullError::NarrowingMissedChange { .. }
        | PullError::Internal { .. } => CycleError::Fatal(EngineError::Internal),
    }
}

impl CycleError {
    /// Replace a placeholder `Fatal` with the caller's real error, so the error
    /// value is built once at the call site that owns it.
    fn map_fatal(self, build: impl FnOnce() -> EngineError) -> Self {
        match self {
            CycleError::Fatal(_) => CycleError::Fatal(build()),
            other => other,
        }
    }
}

// ---- errors -----------------------------------------------------------------

/// A driver-level failure that is neither a retryable transport fault nor a
/// non-destructive integrity stall — i.e. a genuine bug the daemon must surface.
#[derive(Debug)]
pub enum EngineError {
    Io(std::io::Error),
    Store(ManifestStoreError),
    Push(PushError),
    Pull(PullError),
    Internal,
}

impl fmt::Display for EngineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "engine io failed: {error}"),
            Self::Store(error) => write!(formatter, "engine store failed: {error}"),
            Self::Push(error) => write!(formatter, "engine push failed: {error}"),
            Self::Pull(error) => write!(formatter, "engine pull failed: {error}"),
            Self::Internal => formatter.write_str("engine internal invariant violated"),
        }
    }
}

impl Error for EngineError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Store(error) => Some(error),
            Self::Push(error) => Some(error),
            Self::Pull(error) => Some(error),
            Self::Internal => None,
        }
    }
}
