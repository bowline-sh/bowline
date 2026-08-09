//! Tests for `engine_status`: convergence facts, scoped readiness, and the
//! observation-authority composition. Split from the source file at the
//! source/tests seam to stay under the source length gate.

use std::{collections::BTreeSet, sync::Arc};

use bowline_core::status::{ConvergenceReadinessReason, ConvergenceReadinessState};
use bowline_local::sync::manifest_engine::{
    Degradation, EnginePhase, EngineSnapshot, FullScanReason, WorkspacePath,
};

use super::{
    ObservationAuthority, apply_observation_authority, engine_convergence_facts,
    scoped_engine_convergence_facts,
};

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
        applied_ref: bowline_local::sync::manifest_engine::EngineRef::Genesis,
        materialization_revision:
            bowline_local::sync::manifest_engine::MaterializationRevision::INITIAL,
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
        unsyncable: Arc::new(std::collections::BTreeMap::new()),
        refused_removals: Arc::new(BTreeSet::new()),
    }
}

fn scoped_snapshot(dirty: &[&str], dirty_subtrees: &[&str], intents: &[&str]) -> EngineSnapshot {
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
    let facts = engine_convergence_facts(&snapshot(EnginePhase::Idle, Degradation::Nominal, 0, 0));
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
    let facts = engine_convergence_facts(&snapshot(EnginePhase::Idle, Degradation::Nominal, 3, 0));
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
fn a_blocked_deletion_is_distinguishable_from_the_other_attention_states() {
    let blocked = engine_convergence_facts(&snapshot(
        EnginePhase::Stalled,
        Degradation::MassDeletionBlocked {
            removals: 72,
            entries: 121,
        },
        0,
        0,
    ));

    assert!(blocked.limited);
    assert_eq!(
        blocked.blocked_deletions,
        Some(super::BlockedDeletions {
            removals: 72,
            entries: 121,
            // max(64, 121/4) — derived, never a stored second copy.
            threshold: 64,
        }),
        "status carries the counts a user needs to judge the batch"
    );
    // The three other conditions that share `attention-required` carry no
    // block, so a consumer can tell them apart on this field alone.
    for degradation in [
        Degradation::IntegrityStalled,
        Degradation::StoreUnavailable,
        Degradation::RootUnavailable(bowline_local::sync::manifest_engine::RootFault::Missing),
    ] {
        let other = engine_convergence_facts(&snapshot(EnginePhase::Stalled, degradation, 0, 0));
        assert!(other.limited);
        assert_eq!(other.blocked_deletions, None, "{degradation:?}");
    }
}

#[test]
fn a_project_scope_reports_the_workspace_wide_deletion_block() {
    let blocked = scoped_engine_convergence_facts(
        &snapshot(
            EnginePhase::Stalled,
            Degradation::MassDeletionBlocked {
                removals: 72,
                entries: 121,
            },
            0,
            0,
        ),
        &WorkspacePath::new("projects/app"),
    );

    assert!(!blocked.ready);
    assert!(
        blocked.blocked_deletions.is_some(),
        "sync publishes nothing for any project while the batch is refused"
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

// An idle engine is only evidence of a settled workspace when the daemon is
// still being told about changes. A rewrite once left the watcher's overflow
// request asserted after recovery closed, so the callback withheld every
// later write, the engine went genuinely idle, and this projection reported
// a ready workspace that was in fact blind.
#[test]
fn an_idle_engine_is_not_ready_while_observation_authority_is_lost() {
    let settled = snapshot(EnginePhase::Idle, Degradation::Nominal, 0, 0);
    let baseline = engine_convergence_facts(&settled);
    assert!(baseline.ready, "an idle engine alone should read as ready");

    for authority in [
        ObservationAuthority {
            recovery_open: true,
            overflow_pending: false,
        },
        ObservationAuthority {
            recovery_open: false,
            overflow_pending: true,
        },
        ObservationAuthority {
            recovery_open: true,
            overflow_pending: true,
        },
    ] {
        let mut facts = engine_convergence_facts(&settled);
        apply_observation_authority(&mut facts, authority);
        assert!(!facts.ready, "blind daemon reported ready: {authority:?}");
        assert_eq!(facts.summary.state, ConvergenceReadinessState::Converging);
        assert!(
            facts
                .summary
                .reasons
                .contains(&ConvergenceReadinessReason::WatcherRecoveryRequired),
            "no reason surfaced for {authority:?}"
        );
        assert!(
            facts.queue.queued == 0,
            "an empty queue is legitimate while blind"
        );
    }
}

#[test]
fn intact_observation_authority_leaves_convergence_untouched() {
    let settled = snapshot(EnginePhase::Idle, Degradation::Nominal, 0, 0);
    let mut facts = engine_convergence_facts(&settled);
    let before = facts.clone();
    apply_observation_authority(
        &mut facts,
        ObservationAuthority {
            recovery_open: false,
            overflow_pending: false,
        },
    );
    assert_eq!(facts, before);
    assert!(facts.ready);
}

// Authority never upgrades readiness: an engine that knows it has work stays
// not-ready even when the watcher is fully authoritative.
// A workspace that is already limited or blocked must not be softened to
// converging just because the watcher also lost authority. The milder word
// would hide the more serious condition.
#[test]
fn lost_authority_never_masks_a_more_severe_state() {
    let limited = snapshot(EnginePhase::Stalled, Degradation::IntegrityStalled, 0, 0);
    let baseline = engine_convergence_facts(&limited);
    assert_ne!(baseline.summary.state, ConvergenceReadinessState::Ready);
    let mut facts = engine_convergence_facts(&limited);
    apply_observation_authority(
        &mut facts,
        ObservationAuthority {
            recovery_open: true,
            overflow_pending: true,
        },
    );
    assert_eq!(
        facts.summary.state, baseline.summary.state,
        "a more severe state was overwritten"
    );
    assert!(!facts.ready);
}

#[test]
fn observation_authority_never_upgrades_a_busy_engine() {
    let busy = snapshot(EnginePhase::Idle, Degradation::Nominal, 3, 0);
    let mut facts = engine_convergence_facts(&busy);
    assert!(!facts.ready);
    apply_observation_authority(
        &mut facts,
        ObservationAuthority {
            recovery_open: false,
            overflow_pending: false,
        },
    );
    assert!(!facts.ready);
}

#[test]
fn reasons_stay_sorted_and_deduped_when_authority_is_lost() {
    let scanning = snapshot(
        EnginePhase::Idle,
        Degradation::FullScanRequired(FullScanReason::WatcherOverflow),
        0,
        0,
    );
    let mut facts = engine_convergence_facts(&scanning);
    apply_observation_authority(
        &mut facts,
        ObservationAuthority {
            recovery_open: false,
            overflow_pending: true,
        },
    );
    let mut sorted = facts.summary.reasons.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(facts.summary.reasons, sorted);
}
