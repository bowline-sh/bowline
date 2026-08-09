//! TEMPORARY v8-compat adapter: projects the manifest engine's [`EngineSnapshot`]
//! onto the existing `contractVersion: 8` convergence/sync-queue wire shape
//! (Plan 111 Step 1c). This adapter is deliberately minimal — it acquires no
//! storage, keeps no histories, builds no cause graph, and gains no new
//! consumers. A future truthful contract bump replaces it wholesale; do not grow
//! it.
//!
//! The mapping is a small flat function of the snapshot: engine phase +
//! degradation choose the readiness state and a tiny set of truthful reason
//! codes; the dirty count and in-flight intents become the sync-queue counters.
//! Automatic recovery (a full rescan or an offline backoff) presents as
//! `converging`/`recovering` — never a user-facing "recovery mode".

use std::time::Instant;

use bowline_core::status::{
    ConvergenceReadinessReason, ConvergenceReadinessState, ConvergenceStatusSummary,
    StatusAttention, StatusFactAvailabilityImpact, SyncQueueStatus,
};
use bowline_local::sync::manifest_engine::{
    Degradation, EnginePhase, EngineSnapshot, WorkspacePath, mass_deletion_threshold,
};

use crate::manifest_driver::EngineSnapshotHandle;

use super::{
    StatusCollectorFailure, StatusSource, StatusSourceCollection, StatusSourceCollector,
    StatusSourceFacts, StatusSourceFailurePolicy, StatusSourceRevision, StatusTimestamp,
};

/// The engine-derived facts the reducer folds into the v8 wire status. Every
/// field is already mapped to the wire shape; the reducer copies them verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineConvergenceFacts {
    pub revision: u64,
    pub summary: ConvergenceStatusSummary,
    pub queue: SyncQueueStatus,
    pub ready: bool,
    pub presentation_summary: String,
    pub availability: StatusFactAvailabilityImpact,
    pub attention: StatusAttention,
    pub limited: bool,
    /// Present exactly while the deletion breaker is refusing a push. Carried
    /// apart from the readiness reasons because `attention-required` is shared by
    /// four unrelated conditions: a user who is told only that cannot tell a
    /// blocked deletion from an unmounted root, and the two have opposite
    /// remedies.
    pub blocked_deletions: Option<BlockedDeletions>,
}

/// The refused removal batch, as status reports it: what would be deleted, what
/// it was measured against, and the ceiling it exceeded. Counts only — the paths
/// stay on the device, behind `bowline deletions`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockedDeletions {
    pub removals: usize,
    pub entries: usize,
    pub threshold: usize,
}

impl BlockedDeletions {
    /// Read the block off a snapshot, deriving the ceiling from the same
    /// function the guard applied rather than storing a second copy of it.
    fn from_snapshot(snapshot: &EngineSnapshot) -> Option<Self> {
        match snapshot.degradation {
            Degradation::MassDeletionBlocked { removals, entries } => Some(Self {
                removals,
                entries,
                threshold: mass_deletion_threshold(entries),
            }),
            _ => None,
        }
    }
}

/// Whether the daemon can still see the workspace it reports on.
///
/// The engine only knows about work that reached it. While a watcher overflow
/// request is asserted the callback deliberately withholds ordinary changes, so
/// an engine with nothing to do is indistinguishable from an engine that is
/// being told nothing. Readiness has to compose both, or a blind daemon reports
/// itself ready and no surface anywhere contradicts it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObservationAuthority {
    pub recovery_open: bool,
    pub overflow_pending: bool,
}

impl ObservationAuthority {
    /// Authority is intact only when no recovery is in flight and no overflow
    /// request is still withholding changes from the engine.
    pub fn is_authoritative(self) -> bool {
        !self.recovery_open && !self.overflow_pending
    }
}

/// Downgrade convergence facts that an engine snapshot alone would call ready.
///
/// This never upgrades: an engine that knows it has work stays not-ready
/// regardless of observation authority.
pub fn apply_observation_authority(
    facts: &mut EngineConvergenceFacts,
    authority: ObservationAuthority,
) {
    if authority.is_authoritative() {
        return;
    }
    // Only a state the engine called Ready is downgraded. Anything else already
    // says the workspace is not settled, and several of those states are more
    // severe than Converging: overwriting them would hide a limited or blocked
    // workspace behind a milder word.
    if facts.summary.state != ConvergenceReadinessState::Ready {
        facts.ready = false;
        return;
    }
    facts.summary.state = ConvergenceReadinessState::Converging;
    // readiness_reasons sorts and dedupes through a BTreeSet before collecting,
    // so keep that ordering rather than appending out of band.
    if !facts
        .summary
        .reasons
        .contains(&ConvergenceReadinessReason::WatcherRecoveryRequired)
    {
        facts
            .summary
            .reasons
            .push(ConvergenceReadinessReason::WatcherRecoveryRequired);
        facts.summary.reasons.sort();
        facts.summary.reasons.dedup();
    }
    facts.ready = false;
    let (availability, attention) = presentation_impacts(facts.summary.state);
    facts.availability = availability;
    facts.attention = attention;
    facts.presentation_summary = presentation_summary(facts.summary.state, facts.revision);
    facts.limited = facts.summary.state == ConvergenceReadinessState::Limited;
}

/// Map one engine snapshot to the v8 convergence + sync-queue facts.
pub fn engine_convergence_facts(snapshot: &EngineSnapshot) -> EngineConvergenceFacts {
    let state = readiness_state(snapshot);
    let reasons = readiness_reasons(snapshot, state);
    let queue = sync_queue(snapshot);
    let ready = state == ConvergenceReadinessState::Ready
        && reasons.is_empty()
        && !queue.has_pending_work();
    let (availability, attention) = presentation_impacts(state);
    EngineConvergenceFacts {
        revision: snapshot.revision,
        summary: ConvergenceStatusSummary {
            revision: snapshot.revision,
            state,
            reasons,
        },
        queue,
        ready,
        presentation_summary: presentation_summary(state, snapshot.revision),
        availability,
        attention,
        limited: state == ConvergenceReadinessState::Limited,
        blocked_deletions: BlockedDeletions::from_snapshot(snapshot),
    }
}

/// Project-scoped convergence keeps the workspace's global health gates but
/// counts only work that can affect `prefix`. An ancestor recursive dirty root
/// is relevant because its undiscovered descendants may live inside the
/// project. A pending full scan blocks every scope: attributed sets are not
/// complete until that scan has run.
pub fn scoped_engine_convergence_facts(
    snapshot: &EngineSnapshot,
    prefix: &WorkspacePath,
) -> EngineConvergenceFacts {
    let queued = snapshot
        .dirty_paths
        .iter()
        .filter(|path| path_is_within(path, prefix))
        .count()
        .saturating_add(
            snapshot
                .dirty_subtree_paths
                .iter()
                .filter(|path| paths_overlap(path, prefix))
                .count(),
        );
    let pending_intents = snapshot
        .pending_intent_paths
        .iter()
        .filter(|path| path_is_within(path, prefix))
        .count();
    let queue = SyncQueueStatus {
        queued: saturated_u64(queued),
        claimed: 0,
        waiting_retry: u64::from(matches!(
            snapshot.degradation,
            Degradation::OfflineRetrying { .. }
        )),
        blocked_offline: 0,
        reconciliation_required: saturated_u64(pending_intents),
        attention: u64::from(matches!(
            snapshot.degradation,
            Degradation::IntegrityStalled
        )),
        completed: 0,
    };
    let state = scoped_readiness_state(snapshot, &queue);
    let reasons = scoped_readiness_reasons(snapshot, &queue, state);
    let ready = state == ConvergenceReadinessState::Ready
        && reasons.is_empty()
        && !queue.has_pending_work();
    let (availability, attention) = presentation_impacts(state);
    EngineConvergenceFacts {
        revision: snapshot.revision,
        summary: ConvergenceStatusSummary {
            revision: snapshot.revision,
            state,
            reasons,
        },
        queue,
        ready,
        presentation_summary: presentation_summary(state, snapshot.revision),
        availability,
        attention,
        limited: state == ConvergenceReadinessState::Limited,
        blocked_deletions: BlockedDeletions::from_snapshot(snapshot),
    }
}

fn scoped_readiness_state(
    snapshot: &EngineSnapshot,
    queue: &SyncQueueStatus,
) -> ConvergenceReadinessState {
    match snapshot.degradation {
        // Each of these blocks publishing until a human or the engine's own
        // guard clears it, so none of them may read as transient progress.
        Degradation::IntegrityStalled
        | Degradation::RootUnavailable(_)
        | Degradation::MassDeletionBlocked { .. }
        | Degradation::StoreUnavailable => ConvergenceReadinessState::Limited,
        Degradation::OfflineRetrying { .. } => ConvergenceReadinessState::Recovering,
        Degradation::FullScanRequired(_) => ConvergenceReadinessState::Converging,
        Degradation::Nominal => match snapshot.phase {
            EnginePhase::Stopped => ConvergenceReadinessState::Limited,
            EnginePhase::Starting | EnginePhase::BackingOff | EnginePhase::Stalled => {
                ConvergenceReadinessState::Converging
            }
            EnginePhase::Idle | EnginePhase::Syncing
                if !snapshot.scan_required
                    && !snapshot.unattributed_pull_pending
                    && !snapshot.cycle_active
                    && !queue.has_pending_work() =>
            {
                ConvergenceReadinessState::Ready
            }
            EnginePhase::Idle | EnginePhase::Syncing => ConvergenceReadinessState::Converging,
        },
    }
}

fn scoped_readiness_reasons(
    snapshot: &EngineSnapshot,
    queue: &SyncQueueStatus,
    state: ConvergenceReadinessState,
) -> Vec<ConvergenceReadinessReason> {
    if state == ConvergenceReadinessState::Ready {
        return Vec::new();
    }
    let mut reasons = std::collections::BTreeSet::new();
    match snapshot.degradation {
        Degradation::FullScanRequired(_) => {
            reasons.insert(ConvergenceReadinessReason::WatcherRecoveryRequired);
        }
        Degradation::OfflineRetrying { .. } => {
            reasons.insert(ConvergenceReadinessReason::AttemptWaitingRetry);
        }
        Degradation::IntegrityStalled
        | Degradation::RootUnavailable(_)
        | Degradation::MassDeletionBlocked { .. }
        | Degradation::StoreUnavailable => {
            reasons.insert(ConvergenceReadinessReason::AttentionRequired);
        }
        Degradation::Nominal => {}
    }
    if snapshot.scan_required {
        reasons.insert(ConvergenceReadinessReason::WatcherRecoveryRequired);
    }
    if snapshot.unattributed_pull_pending || snapshot.cycle_active {
        reasons.insert(ConvergenceReadinessReason::MaterializationIncomplete);
    }
    if snapshot.phase == EnginePhase::Starting {
        reasons.insert(ConvergenceReadinessReason::StartupRecovery);
    }
    if snapshot.phase == EnginePhase::Stopped {
        reasons.insert(ConvergenceReadinessReason::AttentionRequired);
    }
    if queue.reconciliation_required > 0 {
        reasons.insert(ConvergenceReadinessReason::MaterializationIncomplete);
    }
    if queue.queued > 0 {
        reasons.insert(ConvergenceReadinessReason::CausesPending);
    }
    reasons.into_iter().collect()
}

fn paths_overlap(left: &WorkspacePath, right: &WorkspacePath) -> bool {
    path_is_within(left, right) || path_is_within(right, left)
}

fn path_is_within(path: &WorkspacePath, prefix: &WorkspacePath) -> bool {
    path.as_str() == prefix.as_str()
        || path
            .as_str()
            .strip_prefix(prefix.as_str())
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn saturated_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

/// Phase + degradation choose the readiness rung. Degradation dominates: an
/// integrity stall is `limited` (needs a human), an offline backoff is
/// `recovering`, a full rescan is transient `converging`. With no degradation an
/// idle engine is `ready` and any in-progress phase is `converging`.
fn readiness_state(snapshot: &EngineSnapshot) -> ConvergenceReadinessState {
    match snapshot.degradation {
        // Each of these blocks publishing until a human or the engine's own
        // guard clears it, so none of them may read as transient progress.
        Degradation::IntegrityStalled
        | Degradation::RootUnavailable(_)
        | Degradation::MassDeletionBlocked { .. }
        | Degradation::StoreUnavailable => ConvergenceReadinessState::Limited,
        Degradation::OfflineRetrying { .. } => ConvergenceReadinessState::Recovering,
        Degradation::FullScanRequired(_) => ConvergenceReadinessState::Converging,
        Degradation::Nominal => match snapshot.phase {
            EnginePhase::Idle
                if snapshot.dirty == 0
                    && snapshot.pending_intents == 0
                    && !snapshot.scan_required
                    && !snapshot.unattributed_pull_pending
                    && !snapshot.cycle_active =>
            {
                ConvergenceReadinessState::Ready
            }
            EnginePhase::Idle => ConvergenceReadinessState::Converging,
            EnginePhase::Stopped => ConvergenceReadinessState::Limited,
            EnginePhase::Starting
            | EnginePhase::Syncing
            | EnginePhase::BackingOff
            | EnginePhase::Stalled => ConvergenceReadinessState::Converging,
        },
    }
}

/// A small flat set of truthful reason codes, reusing existing v8 codes. A
/// `ready` state has no reasons, so `sync wait` settles only when the engine is
/// genuinely caught up.
fn readiness_reasons(
    snapshot: &EngineSnapshot,
    state: ConvergenceReadinessState,
) -> Vec<ConvergenceReadinessReason> {
    if state == ConvergenceReadinessState::Ready {
        return Vec::new();
    }
    // BTreeSet keeps the codes sorted and deduped without a second copy.
    let mut reasons = std::collections::BTreeSet::new();
    match snapshot.degradation {
        Degradation::FullScanRequired(_) => {
            reasons.insert(ConvergenceReadinessReason::WatcherRecoveryRequired);
        }
        Degradation::OfflineRetrying { .. } => {
            reasons.insert(ConvergenceReadinessReason::AttemptWaitingRetry);
        }
        Degradation::IntegrityStalled
        | Degradation::RootUnavailable(_)
        | Degradation::MassDeletionBlocked { .. }
        | Degradation::StoreUnavailable => {
            reasons.insert(ConvergenceReadinessReason::AttentionRequired);
        }
        Degradation::Nominal => {}
    }
    if snapshot.phase == EnginePhase::Starting {
        reasons.insert(ConvergenceReadinessReason::StartupRecovery);
    }
    if snapshot.scan_required {
        reasons.insert(ConvergenceReadinessReason::WatcherRecoveryRequired);
    }
    if snapshot.unattributed_pull_pending || snapshot.cycle_active {
        reasons.insert(ConvergenceReadinessReason::MaterializationIncomplete);
    }
    // A stopped engine is `limited`; give that rung a truthful reason. This also
    // covers the daemon's host-status snapshot published while the manifest driver
    // is waiting to rebuild (workspace key or hosted context not yet available).
    if snapshot.phase == EnginePhase::Stopped {
        reasons.insert(ConvergenceReadinessReason::AttentionRequired);
    }
    if snapshot.pending_intents > 0 {
        reasons.insert(ConvergenceReadinessReason::MaterializationIncomplete);
    }
    if snapshot.dirty > 0 {
        reasons.insert(ConvergenceReadinessReason::CausesPending);
    }
    reasons.into_iter().collect()
}

/// Dirty paths are the outbound push queue; in-flight intents are the inbound
/// apply work. Offline backoff and integrity stall raise their honest lanes.
fn sync_queue(snapshot: &EngineSnapshot) -> SyncQueueStatus {
    SyncQueueStatus {
        queued: snapshot.dirty as u64,
        claimed: 0,
        waiting_retry: u64::from(matches!(
            snapshot.degradation,
            Degradation::OfflineRetrying { .. }
        )),
        blocked_offline: 0,
        reconciliation_required: snapshot.pending_intents as u64,
        attention: u64::from(matches!(
            snapshot.degradation,
            Degradation::IntegrityStalled
        )),
        completed: 0,
    }
}

fn presentation_impacts(
    state: ConvergenceReadinessState,
) -> (StatusFactAvailabilityImpact, StatusAttention) {
    match state {
        ConvergenceReadinessState::Ready => {
            (StatusFactAvailabilityImpact::None, StatusAttention::None)
        }
        ConvergenceReadinessState::Converging => (
            StatusFactAvailabilityImpact::None,
            StatusAttention::Recommended,
        ),
        ConvergenceReadinessState::Recovering => (
            StatusFactAvailabilityImpact::Degraded,
            StatusAttention::Recommended,
        ),
        ConvergenceReadinessState::Limited => (
            StatusFactAvailabilityImpact::Unavailable,
            StatusAttention::Required,
        ),
    }
}

fn presentation_summary(state: ConvergenceReadinessState, revision: u64) -> String {
    let label = match state {
        ConvergenceReadinessState::Ready => "ready",
        ConvergenceReadinessState::Converging => "syncing",
        ConvergenceReadinessState::Recovering => "recovering",
        ConvergenceReadinessState::Limited => "needs attention",
    };
    format!("Workspace sync is {label} at revision {revision}.")
}

/// Supplies whether the daemon can still see its workspace.
///
/// Kept as a trait so this projection does not depend on the recovery
/// coordinator directly, and so tests can drive authority without a live
/// watcher. `revision` must change whenever `authority` could: the collector
/// keys change detection on it, and recovery moves while the engine sits still.
pub trait ObservationAuthoritySource: Send + Sync + std::fmt::Debug {
    fn authority(&self) -> ObservationAuthority;
    fn revision(&self) -> u64;
}

/// Reads the engine's live snapshot into the projection at the `Convergence`
/// source slot. Change detection keys on the engine revision paired with the
/// observation-authority revision: recovery can open and close while the engine
/// revision never moves, and keying on the engine alone would pin a stale ready
/// onto a daemon that has stopped seeing the workspace.
#[derive(Debug)]
pub struct EngineStatusCollector {
    snapshot: EngineSnapshotHandle,
    authority: Option<Box<dyn ObservationAuthoritySource>>,
    committed_key: Option<(u64, u64)>,
    staged: Option<StatusSourceCollection>,
}

impl EngineStatusCollector {
    pub fn new(snapshot: EngineSnapshotHandle) -> Self {
        Self {
            snapshot,
            authority: None,
            committed_key: None,
            staged: None,
        }
    }

    /// Compose observation authority into convergence readiness. Without this
    /// the collector reports whatever the engine believes, which is the exact
    /// shape of a daemon that has been made blind and calls itself ready.
    pub fn with_observation_authority(
        mut self,
        authority: Box<dyn ObservationAuthoritySource>,
    ) -> Self {
        self.authority = Some(authority);
        self
    }

    fn current_authority(&self) -> (ObservationAuthority, u64) {
        match self.authority.as_ref() {
            Some(source) => (source.authority(), source.revision()),
            None => (
                ObservationAuthority {
                    recovery_open: false,
                    overflow_pending: false,
                },
                0,
            ),
        }
    }
}

impl StatusSourceCollector for EngineStatusCollector {
    fn source(&self) -> StatusSource {
        StatusSource::Convergence
    }

    fn failure_policy(&self) -> StatusSourceFailurePolicy {
        StatusSourceFailurePolicy::RetainLastKnown
    }

    fn mark_dirty(&mut self) {}

    fn stage(
        &mut self,
        observed_at: StatusTimestamp,
        _now: Instant,
    ) -> Result<StatusSourceCollection, StatusCollectorFailure> {
        if let Some(staged) = self.staged.as_ref() {
            return Ok(staged.clone());
        }
        let snapshot = self.snapshot.current();
        let (authority, authority_revision) = self.current_authority();
        // Recovery can open and close while the engine revision never moves, so
        // keying on the engine alone would pin a stale ready to a blind daemon.
        let key = (snapshot.revision, authority_revision);
        if self.committed_key == Some(key) {
            return Ok(StatusSourceCollection::Unchanged);
        }
        let mut facts = engine_convergence_facts(&snapshot);
        apply_observation_authority(&mut facts, authority);
        let staged = StatusSourceCollection::Updated {
            revision: StatusSourceRevision::new(snapshot.revision.max(authority_revision)),
            observed_at,
            facts: StatusSourceFacts::Convergence(Box::new(facts)),
        };
        self.staged = Some(staged.clone());
        Ok(staged)
    }

    fn commit_staged(&mut self) {
        if let Some(StatusSourceCollection::Updated { facts, .. }) = self.staged.take()
            && let StatusSourceFacts::Convergence(facts) = facts
        {
            let (_, authority_revision) = self.current_authority();
            self.committed_key = Some((facts.revision, authority_revision));
        }
    }

    fn abort_staged(&mut self) {}

    fn reject_staged(&mut self) {
        self.staged = None;
    }
}

#[cfg(test)]
#[path = "engine_status/tests.rs"]
mod tests;
