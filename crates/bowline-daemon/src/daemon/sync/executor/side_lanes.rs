use super::*;

use bowline_control_plane::WorkspaceControlPlaneClient;
use bowline_local::{
    metadata::{
        ClaimedWorkViewAcceptOperation, WorkViewAcceptCheckpointStep, WorkViewAcceptClaimCheck,
    },
    sync::{WorkViewAcceptExecutionInput, WorkViewAcceptExecutionOutcome},
};

use super::super::work_accept_runtime::WorkAcceptRunResult;

pub(in crate::daemon) fn execute_work_view_accept_with_context(
    resolver: &HostedContextResolver,
    args: SyncOnceArgs,
    claimed: ClaimedWorkViewAcceptOperation,
) -> Result<WorkAcceptRunResult, SyncOnceError> {
    validate_hosted_operation_scope(
        &args,
        &claimed.operation.workspace_id,
        &claimed.operation.device_id,
    )?;
    let workspace_key = require_local_workspace_key(&args)?;
    let hosted = resolver(&args).map_err(|error| {
        SyncOnceError::ControlPlane(ControlPlaneError::Storage(error.to_string()))
    })?;
    for _ in 0..MAX_SYNC_RETRY_ATTEMPTS {
        if let Some(terminal) = work_accept_boundary(&args, &claimed)? {
            return Ok(terminal);
        }
        let base_ref = match hosted
            .client
            .get_workspace_ref(&claimed.operation.workspace_id)?
        {
            Some(workspace_ref) => workspace_ref,
            None => {
                if let Some(terminal) = work_accept_boundary(&args, &claimed)? {
                    return Ok(terminal);
                }
                hosted
                    .client
                    .create_workspace_ref(&claimed.operation.workspace_id)?
            }
        };
        if let Some(terminal) = work_accept_boundary(&args, &claimed)? {
            return Ok(terminal);
        }
        let summary = run_sync_once_observed_with_hosted_and_accept_claim(
            hosted.clone(),
            args.clone(),
            Some(base_ref),
            workspace_key,
            Some(claimed.claim.clone()),
        )?;
        if summary.stale() {
            continue;
        }
        if !accept_reconciliation_ready(summary.outcome) {
            return Err(SyncOnceError::Runner(SyncRunnerError::StateIo(
                std::io::Error::other(
                    "workspace reconciliation is conflicted before work-view accept",
                ),
            )));
        }
        if let Some(terminal) = work_accept_boundary(&args, &claimed)? {
            return Ok(terminal);
        }
        let current_ref = hosted
            .client
            .get_workspace_ref(&claimed.operation.workspace_id)?
            .ok_or_else(|| {
                SyncOnceError::InvalidOperationPayload(
                    "canonical workspace ref disappeared during work accept".to_string(),
                )
            })?;
        let byte_store = SignedUrlByteStore::with_http_client(
            hosted.client.as_ref(),
            claimed.operation.workspace_id.as_str(),
            hosted.http.clone(),
        );
        let runner = SyncRunner::new_with_base_ref(
            hosted.client.as_ref(),
            &byte_store,
            SyncRunnerOptions {
                root: args.root.clone(),
                state_root: args.state_root.clone(),
                workspace_id: claimed.operation.workspace_id.clone(),
                device_id: claimed.operation.device_id.clone(),
                workspace_content_key: workspace_key.bytes,
                storage_key: StorageKey::from_bytes(workspace_key.bytes),
                key_epoch: workspace_key.key_epoch,
                generated_at: current_timestamp(),
                sync_claim: None,
                scan_scope: ScanScope::Full(FullScanReason::ReconcileFallback),
            },
            current_ref,
        );
        let input = WorkViewAcceptExecutionInput {
            operation_id: claimed.operation.id.clone(),
            work_view_id: claimed.operation.work_view_id.clone(),
            selected_paths: claimed.operation.selected_paths.clone().unwrap_or_default(),
            claim: claimed.claim.clone(),
        };
        let outcome = runner.execute_work_view_accept(input)?;
        match outcome {
            WorkViewAcceptExecutionOutcome::RetryStale { .. } => continue,
            WorkViewAcceptExecutionOutcome::Cancelled => {
                return Ok(WorkAcceptRunResult::Cancelled);
            }
            WorkViewAcceptExecutionOutcome::OwnershipLost => {
                return Ok(WorkAcceptRunResult::OwnershipLost);
            }
            WorkViewAcceptExecutionOutcome::ReviewRequired {
                reason,
                result_json,
            } => {
                return Ok(WorkAcceptRunResult::Review {
                    reason,
                    result_json,
                });
            }
            WorkViewAcceptExecutionOutcome::Completed {
                workspace_ref,
                snapshot_id,
                cancelled_late,
            } => {
                let result_json = serde_json::to_string(&serde_json::json!({
                    "snapshotId": snapshot_id.as_str(),
                    "workspaceRefVersion": workspace_ref.version,
                    "outcome": if cancelled_late {
                        "completed-after-cancel"
                    } else {
                        "completed"
                    },
                }))
                .map_err(|error| SyncOnceError::InvalidOperationPayload(error.to_string()))?;
                return Ok(WorkAcceptRunResult::Completed {
                    snapshot_id,
                    result_json,
                });
            }
        }
    }
    Err(SyncOnceError::InvalidOperationPayload(
        "work-view accept exhausted stale-ref retries".to_string(),
    ))
}

fn work_accept_boundary(
    args: &SyncOnceArgs,
    claimed: &ClaimedWorkViewAcceptOperation,
) -> Result<Option<WorkAcceptRunResult>, SyncOnceError> {
    Ok(match work_accept_claim_check(args, claimed)? {
        WorkViewAcceptClaimCheck::Owned => None,
        WorkViewAcceptClaimCheck::CancellationRequested => Some(WorkAcceptRunResult::Cancelled),
        WorkViewAcceptClaimCheck::OwnershipLost => Some(WorkAcceptRunResult::OwnershipLost),
    })
}

fn accept_reconciliation_ready(outcome: SyncSummaryOutcome) -> bool {
    !matches!(outcome, SyncSummaryOutcome::Conflicted)
}

fn work_accept_claim_check(
    args: &SyncOnceArgs,
    claimed: &ClaimedWorkViewAcceptOperation,
) -> Result<WorkViewAcceptClaimCheck, SyncOnceError> {
    let store = MetadataStore::open(args.state_root.join(DEFAULT_DATABASE_FILE))
        .map_err(SyncRunnerError::from)
        .map_err(SyncOnceError::from)?;
    let check = store
        .check_work_view_accept_claim(&claimed.claim, &current_timestamp())
        .map_err(SyncRunnerError::from)
        .map_err(SyncOnceError::from)?;
    if check != WorkViewAcceptClaimCheck::CancellationRequested {
        return Ok(check);
    }
    let committed = store
        .work_view_accept_checkpoints(&claimed.operation.id)
        .map_err(SyncRunnerError::from)
        .map_err(SyncOnceError::from)?
        .iter()
        .any(|checkpoint| {
            matches!(
                checkpoint.step,
                WorkViewAcceptCheckpointStep::WorkspaceRefPublished
                    | WorkViewAcceptCheckpointStep::MainPublished
                    | WorkViewAcceptCheckpointStep::LifecyclePublished
            )
        });
    Ok(if committed {
        WorkViewAcceptClaimCheck::Owned
    } else {
        check
    })
}

pub(in crate::daemon) fn reconcile_conflict_occurrence_with_context(
    resolver: &HostedContextResolver,
    args: &SyncOnceArgs,
    input: ConflictOccurrenceReconcile,
) -> Result<ConflictReconcileResult, SyncOnceError> {
    validate_hosted_operation_scope(args, &input.workspace_id, &input.device_id)?;
    resolver(args)
        .map_err(|error| {
            SyncOnceError::ControlPlane(ControlPlaneError::Storage(error.to_string()))
        })?
        .client
        .reconcile_conflict_occurrence(input)
        .map_err(Into::into)
}

pub(in crate::daemon) fn sync_work_view_overlays_with_context(
    resolver: &HostedContextResolver,
    args: SyncOnceArgs,
    input: WorkViewOverlaySyncInput,
    claim: &SyncClaimHandle,
) -> Result<WorkViewOverlaySyncResult, SyncOnceError> {
    validate_hosted_operation_scope(&args, &input.workspace_id, &input.device_id)?;
    overlay_claim_checkpoint(&args, claim).map_err(SyncRunnerError::from)?;
    let key_store = key_store()?;
    let workspace_key = key_store
        .load_workspace_key(&input.workspace_id)?
        .ok_or(SyncOnceError::WorkspaceKeyMissing)?;
    let workspace_key_bytes = workspace_key_bytes(&workspace_key.key_bytes)
        .map_err(|_| SyncOnceError::WorkspaceKeyInvalid)?;
    let hosted = resolver(&args).map_err(|error| {
        SyncOnceError::ControlPlane(ControlPlaneError::Storage(error.to_string()))
    })?;
    let byte_store = SignedUrlByteStore::with_http_client(
        hosted.client.as_ref(),
        input.workspace_id.as_str(),
        hosted.http.clone(),
    );
    let report = bowline_local::work_views::sync_local_work_view_overlays_with_checkpoint(
        bowline_local::work_views::WorkViewOverlaySyncOptions {
            db_path: args.state_root.join(DEFAULT_DATABASE_FILE),
            device_id: input.device_id.clone(),
            workspace_content_key: workspace_key_bytes,
            storage_key: StorageKey::from_bytes(workspace_key_bytes),
            key_epoch: workspace_key.key_epoch,
            generated_at: input.generated_at.clone(),
        },
        hosted.client.as_ref(),
        &byte_store,
        &input.workspace_ref(),
        || overlay_claim_checkpoint(&args, claim),
    )
    .map_err(|error| SyncOnceError::Runner(SyncRunnerError::from(error)))?;
    Ok(overlay_sync_result(report))
}

fn overlay_claim_checkpoint(
    args: &SyncOnceArgs,
    claim: &SyncClaimHandle,
) -> Result<(), WorkViewOverlaySyncError> {
    let check = MetadataStore::open(args.state_root.join(DEFAULT_DATABASE_FILE))
        .and_then(|store| store.check_sync_operation_claim(claim));
    match check {
        Ok(SyncClaimCheck::Owned) => Ok(()),
        Ok(SyncClaimCheck::CancellationRequested) => {
            Err(WorkViewOverlaySyncError::CancellationRequested)
        }
        Ok(SyncClaimCheck::OwnershipLost) | Err(_) => {
            Err(WorkViewOverlaySyncError::ClaimOwnershipLost)
        }
    }
}

fn overlay_sync_result(
    report: bowline_local::work_views::WorkViewOverlaySyncReport,
) -> WorkViewOverlaySyncResult {
    WorkViewOverlaySyncResult {
        uploaded: u64::try_from(report.uploaded)
            .expect("usize overlay count always fits in u64 on supported targets"),
        attention: u64::try_from(report.attention)
            .expect("usize overlay count always fits in u64 on supported targets"),
        entries_total: u64::try_from(report.entries_total)
            .expect("usize overlay count always fits in u64 on supported targets"),
        entries_completed: u64::try_from(report.entries_completed)
            .expect("usize overlay count always fits in u64 on supported targets"),
        content_objects_uploaded: u64::try_from(report.content_objects_uploaded)
            .expect("usize overlay count always fits in u64 on supported targets"),
        content_objects_reused: u64::try_from(report.content_objects_reused)
            .expect("usize overlay count always fits in u64 on supported targets"),
        plaintext_bytes: report.plaintext_bytes,
        uploaded_bytes: report.uploaded_bytes,
    }
}

pub(super) fn validate_hosted_operation_scope(
    args: &SyncOnceArgs,
    workspace_id: &WorkspaceId,
    device_id: &DeviceId,
) -> Result<(), SyncOnceError> {
    if workspace_id != &args.workspace_id() || device_id.as_str() != args.device_id.as_str() {
        return Err(SyncOnceError::InvalidOperationPayload(
            "hosted operation scope does not match the daemon runner".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_conflict_is_a_retry_prerequisite_not_accept_review() {
        assert!(!accept_reconciliation_ready(SyncSummaryOutcome::Conflicted));
        assert!(accept_reconciliation_ready(SyncSummaryOutcome::NoChanges));
    }

    #[test]
    fn daemon_adapter_preserves_every_overlay_progress_counter() {
        let result = overlay_sync_result(bowline_local::work_views::WorkViewOverlaySyncReport {
            uploaded: 1,
            attention: 2,
            entries_total: 3,
            entries_completed: 4,
            content_objects_uploaded: 5,
            content_objects_reused: 6,
            plaintext_bytes: 7,
            uploaded_bytes: 8,
        });

        assert_eq!(
            result,
            WorkViewOverlaySyncResult {
                uploaded: 1,
                attention: 2,
                entries_total: 3,
                entries_completed: 4,
                content_objects_uploaded: 5,
                content_objects_reused: 6,
                plaintext_bytes: 7,
                uploaded_bytes: 8,
            }
        );
    }
}
