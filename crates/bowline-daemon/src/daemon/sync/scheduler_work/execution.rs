use super::*;

pub(super) fn execute_reconcile(mut work: PreparedReconcileWork) -> WorkerCompletion {
    let result = (work.executor)(work.args, work.observed_ref);
    WorkerCompletion::Reconcile(ReconcileCompletion {
        claimed: work.claimed,
        attempted_scan_scope: work.attempted_scan_scope,
        result,
        executor: work.executor,
    })
}

pub(super) fn execute_conflict(work: PreparedSyncClaimWork) -> WorkerCompletion {
    let claimed = work.claimed;
    let result = run_conflict_worker(&work.args, &work.resolver, &claimed);
    WorkerCompletion::Conflict(SyncClaimCompletion { claimed, result })
}

fn run_conflict_worker(
    args: &SyncOnceArgs,
    resolver: &HostedContextResolver,
    claimed: &ClaimedSyncOperation,
) -> ConflictWorkerResult {
    let input = match decode_conflict_occurrence_operation(&claimed.operation) {
        Ok(input) => input,
        Err(error) => {
            return ConflictWorkerResult::Failed(SyncOnceError::InvalidOperationPayload(
                error.to_string(),
            ));
        }
    };
    if let Err(error) = validate_conflict_operation(&claimed.operation, &input) {
        return ConflictWorkerResult::Failed(error);
    }
    match check_sync_claim(&args.state_root, &claimed.claim) {
        Ok(SyncClaimCheck::Owned) => {}
        Ok(SyncClaimCheck::CancellationRequested) => {
            return ConflictWorkerResult::Cancelled;
        }
        Ok(SyncClaimCheck::OwnershipLost) | Err(_) => {
            return ConflictWorkerResult::OwnershipLost;
        }
    }
    let local_state = local_conflict_state(input.desired_state);
    match conflict_occurrence_is_current(
        &args.state_root,
        input.conflict_id.as_str(),
        input.occurrence_version,
        local_state,
    ) {
        Ok(false) => ConflictWorkerResult::Completed {
            input,
            outcome: ConflictReconcileOutcome::Superseded,
            mark_local: false,
        },
        Err(error) => {
            ConflictWorkerResult::Failed(SyncOnceError::InvalidOperationPayload(error.to_string()))
        }
        Ok(true) => {
            match check_sync_claim(&args.state_root, &claimed.claim) {
                Ok(SyncClaimCheck::Owned) => {}
                Ok(SyncClaimCheck::CancellationRequested) => {
                    return ConflictWorkerResult::Cancelled;
                }
                Ok(SyncClaimCheck::OwnershipLost) | Err(_) => {
                    return ConflictWorkerResult::OwnershipLost;
                }
            }
            match reconcile_conflict_occurrence_with_context(resolver, args, input.clone()) {
                Ok(result) => ConflictWorkerResult::Completed {
                    input,
                    outcome: result.outcome,
                    mark_local: matches!(
                        result.outcome,
                        ConflictReconcileOutcome::Applied | ConflictReconcileOutcome::Idempotent
                    ),
                },
                Err(error) => ConflictWorkerResult::Failed(error),
            }
        }
    }
}

pub(super) fn execute_overlay(work: PreparedSyncClaimWork) -> WorkerCompletion {
    let claimed = work.claimed;
    let result = run_overlay_worker(&work.args, &work.resolver, &claimed);
    WorkerCompletion::Overlay(SyncClaimCompletion { claimed, result })
}

fn run_overlay_worker(
    args: &SyncOnceArgs,
    resolver: &HostedContextResolver,
    claimed: &ClaimedSyncOperation,
) -> OverlayWorkerResult {
    let input = match decode_work_view_overlay_sync_operation(&claimed.operation) {
        Ok(input) => input,
        Err(error) => {
            return OverlayWorkerResult::Failed(SyncOnceError::InvalidOperationPayload(
                error.to_string(),
            ));
        }
    };
    if let Err(error) = validate_work_view_overlay_operation(&claimed.operation, &input) {
        return OverlayWorkerResult::Failed(error);
    }
    match check_sync_claim(&args.state_root, &claimed.claim) {
        Ok(SyncClaimCheck::Owned) => {}
        Ok(SyncClaimCheck::CancellationRequested)
            if claimed.claim.claimed_from_state() == SyncOperationState::ReconciliationRequired => {
        }
        Ok(SyncClaimCheck::CancellationRequested) => {
            return OverlayWorkerResult::Cancelled;
        }
        Ok(SyncClaimCheck::OwnershipLost) | Err(_) => {
            return OverlayWorkerResult::OwnershipLost;
        }
    }
    let result =
        sync_work_view_overlays_with_context(resolver, args.clone(), input, &claimed.claim);
    match result {
        Ok(result) => OverlayWorkerResult::Completed(result),
        Err(SyncOnceError::Runner(SyncRunnerError::WorkViewOverlay(
            WorkViewOverlaySyncError::CancellationRequested,
        ))) => OverlayWorkerResult::Cancelled,
        Err(SyncOnceError::Runner(SyncRunnerError::WorkViewOverlay(
            WorkViewOverlaySyncError::ClaimOwnershipLost,
        ))) => OverlayWorkerResult::OwnershipLost,
        Err(error) => OverlayWorkerResult::Failed(error),
    }
}

pub(super) fn execute_accept(work: PreparedAcceptWork) -> WorkerCompletion {
    let claimed = work.claimed;
    let result = execute_work_view_accept_with_context(&work.resolver, work.args, claimed.clone());
    WorkerCompletion::WorkViewAccept(AcceptCompletion { claimed, result })
}

fn check_sync_claim(
    state_root: &Path,
    claim: &SyncClaimHandle,
) -> Result<SyncClaimCheck, MetadataError> {
    let store = MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE))?;
    store.check_sync_operation_claim(claim)
}
