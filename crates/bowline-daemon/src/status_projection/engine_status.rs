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
    Degradation, EnginePhase, EngineSnapshot, WorkspacePath,
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
    }
}

fn scoped_readiness_state(
    snapshot: &EngineSnapshot,
    queue: &SyncQueueStatus,
) -> ConvergenceReadinessState {
    match snapshot.degradation {
        Degradation::IntegrityStalled => ConvergenceReadinessState::Limited,
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
        Degradation::IntegrityStalled => {
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
        Degradation::IntegrityStalled => ConvergenceReadinessState::Limited,
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
        Degradation::IntegrityStalled => {
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

/// Reads the engine's live snapshot into the projection at the `Convergence`
/// source slot. Change detection keys on the engine revision, which bumps only
/// on a real state transition, so an idle poll never republishes.
#[derive(Debug)]
pub struct EngineStatusCollector {
    snapshot: EngineSnapshotHandle,
    committed_revision: Option<u64>,
    staged: Option<StatusSourceCollection>,
}

impl EngineStatusCollector {
    pub fn new(snapshot: EngineSnapshotHandle) -> Self {
        Self {
            snapshot,
            committed_revision: None,
            staged: None,
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
        if self.committed_revision == Some(snapshot.revision) {
            return Ok(StatusSourceCollection::Unchanged);
        }
        let facts = engine_convergence_facts(&snapshot);
        let staged = StatusSourceCollection::Updated {
            revision: StatusSourceRevision::new(snapshot.revision),
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
            self.committed_revision = Some(facts.revision);
        }
    }

    fn abort_staged(&mut self) {}

    fn reject_staged(&mut self) {
        self.staged = None;
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, sync::Arc};

    use bowline_core::status::{ConvergenceReadinessReason, ConvergenceReadinessState};
    use bowline_local::sync::manifest_engine::{
        Degradation, EnginePhase, EngineSnapshot, FullScanReason, WorkspacePath,
    };

    use super::{engine_convergence_facts, scoped_engine_convergence_facts};

    fn snapshot(
        phase: EnginePhase,
        degradation: Degradation,
        dirty: usize,
        pending_intents: usize,
    ) -> EngineSnapshot {
        EngineSnapshot {
            revision: 7,
            phase,
            observed_ref: None,
            applied_manifest: None,
            pending_intents,
            dirty,
            dirty_paths: Arc::new(BTreeSet::new()),
            dirty_subtree_paths: Arc::new(BTreeSet::new()),
            pending_intent_paths: Arc::new(BTreeSet::new()),
            scan_required: false,
            unattributed_pull_pending: false,
            cycle_active: false,
            last_success_at: None,
            degradation,
        }
    }

    fn scoped_snapshot(
        dirty: &[&str],
        dirty_subtrees: &[&str],
        intents: &[&str],
    ) -> EngineSnapshot {
        let paths = |values: &[&str]| {
            Arc::new(
                values
                    .iter()
                    .map(|value| WorkspacePath::new(*value))
                    .collect::<BTreeSet<_>>(),
            )
        };
        EngineSnapshot {
            dirty: dirty.len().saturating_add(dirty_subtrees.len()),
            pending_intents: intents.len(),
            dirty_paths: paths(dirty),
            dirty_subtree_paths: paths(dirty_subtrees),
            pending_intent_paths: paths(intents),
            ..snapshot(EnginePhase::Syncing, Degradation::Nominal, 0, 0)
        }
    }

    #[test]
    fn idle_maps_to_ready_settled() {
        let facts =
            engine_convergence_facts(&snapshot(EnginePhase::Idle, Degradation::Nominal, 0, 0));
        assert!(facts.ready);
        assert_eq!(facts.summary.state, ConvergenceReadinessState::Ready);
        assert!(facts.summary.reasons.is_empty());
        // The settledness inputs `classify_daemon_sync` reads: Ready, no reasons,
        // and an empty queue.
        assert!(!facts.queue.has_pending_work());
    }

    #[test]
    fn syncing_maps_to_converging_with_causes() {
        let facts =
            engine_convergence_facts(&snapshot(EnginePhase::Syncing, Degradation::Nominal, 3, 0));
        assert!(!facts.ready);
        assert_eq!(facts.summary.state, ConvergenceReadinessState::Converging);
        assert!(
            facts
                .summary
                .reasons
                .contains(&ConvergenceReadinessReason::CausesPending)
        );
        assert_eq!(facts.queue.queued, 3);
    }

    #[test]
    fn debounced_dirty_work_cannot_present_as_ready() {
        let facts =
            engine_convergence_facts(&snapshot(EnginePhase::Idle, Degradation::Nominal, 3, 0));
        assert!(!facts.ready);
        assert_eq!(facts.summary.state, ConvergenceReadinessState::Converging);
        assert!(
            facts
                .summary
                .reasons
                .contains(&ConvergenceReadinessReason::CausesPending)
        );
        assert_eq!(facts.queue.queued, 3);
    }

    #[test]
    fn offline_retry_maps_to_recovering() {
        let facts = engine_convergence_facts(&snapshot(
            EnginePhase::BackingOff,
            Degradation::OfflineRetrying { attempt: 2 },
            1,
            0,
        ));
        assert_eq!(facts.summary.state, ConvergenceReadinessState::Recovering);
        assert!(
            facts
                .summary
                .reasons
                .contains(&ConvergenceReadinessReason::AttemptWaitingRetry)
        );
        assert_eq!(facts.queue.waiting_retry, 1);
    }

    #[test]
    fn stopped_maps_to_limited_with_attention() {
        // The daemon publishes a `Stopped`/`Nominal` host-status snapshot while the
        // manifest driver is waiting to rebuild (lazy-rebuild path). It must read
        // as `limited` with a truthful reason, never as settled.
        let facts =
            engine_convergence_facts(&snapshot(EnginePhase::Stopped, Degradation::Nominal, 0, 0));
        assert!(!facts.ready);
        assert!(facts.limited);
        assert_eq!(facts.summary.state, ConvergenceReadinessState::Limited);
        assert!(
            facts
                .summary
                .reasons
                .contains(&ConvergenceReadinessReason::AttentionRequired)
        );
    }

    #[test]
    fn integrity_stall_maps_to_limited() {
        let facts = engine_convergence_facts(&snapshot(
            EnginePhase::Stalled,
            Degradation::IntegrityStalled,
            0,
            2,
        ));
        assert!(facts.limited);
        assert_eq!(facts.summary.state, ConvergenceReadinessState::Limited);
        assert!(
            facts
                .summary
                .reasons
                .contains(&ConvergenceReadinessReason::AttentionRequired)
        );
        assert_eq!(facts.queue.attention, 1);
        assert_eq!(facts.queue.reconciliation_required, 2);
    }

    #[test]
    fn project_scope_ignores_sibling_work_but_counts_its_own_paths() {
        let project = WorkspacePath::new("projects/app");
        let sibling_only = scoped_snapshot(
            &["projects/app2/src/main.rs"],
            &["projects/other"],
            &["projects/app2/config.json"],
        );
        let ready = scoped_engine_convergence_facts(&sibling_only, &project);
        assert!(ready.ready);
        assert_eq!(ready.queue.queued, 0);
        assert_eq!(ready.queue.reconciliation_required, 0);

        let relevant = scoped_snapshot(
            &["projects/app/src/main.rs"],
            &[],
            &["projects/app/config.json"],
        );
        let converging = scoped_engine_convergence_facts(&relevant, &project);
        assert!(!converging.ready);
        assert_eq!(converging.queue.queued, 1);
        assert_eq!(converging.queue.reconciliation_required, 1);
    }

    #[test]
    fn project_scope_counts_an_ancestor_recursive_dirty_root() {
        let snapshot = scoped_snapshot(&[], &["projects"], &[]);
        let facts = scoped_engine_convergence_facts(&snapshot, &WorkspacePath::new("projects/app"));
        assert!(!facts.ready);
        assert_eq!(facts.queue.queued, 1);
    }

    #[test]
    fn project_scope_fails_closed_while_attribution_is_incomplete() {
        let project = WorkspacePath::new("projects/app");
        let mut scan = scoped_snapshot(&[], &[], &[]);
        scan.scan_required = true;
        assert!(!scoped_engine_convergence_facts(&scan, &project).ready);

        let mut pull = scoped_snapshot(&[], &[], &[]);
        pull.unattributed_pull_pending = true;
        assert!(!scoped_engine_convergence_facts(&pull, &project).ready);
        assert!(
            scoped_engine_convergence_facts(&pull, &project)
                .summary
                .reasons
                .contains(&ConvergenceReadinessReason::MaterializationIncomplete)
        );

        let mut active = scoped_snapshot(&[], &[], &[]);
        active.cycle_active = true;
        assert!(!scoped_engine_convergence_facts(&active, &project).ready);

        for (phase, degradation) in [
            (EnginePhase::Starting, Degradation::Nominal),
            (EnginePhase::Stopped, Degradation::Nominal),
            (
                EnginePhase::Syncing,
                Degradation::FullScanRequired(FullScanReason::WatcherOverflow),
            ),
            (
                EnginePhase::BackingOff,
                Degradation::OfflineRetrying { attempt: 1 },
            ),
            (EnginePhase::Stalled, Degradation::IntegrityStalled),
        ] {
            let degraded = snapshot(phase, degradation, 0, 0);
            assert!(
                !scoped_engine_convergence_facts(&degraded, &project).ready,
                "{phase:?}/{degradation:?} must block project readiness"
            );
        }
    }
}
