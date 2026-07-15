use super::*;

impl ContinuousSyncRuntime {
    #[cfg(test)]
    pub(in crate::daemon) fn claim_daemon_sync_operation(
        &mut self,
    ) -> Option<ClaimedSyncOperation> {
        self.claim_daemon_sync_operation_filtered(false)
    }

    pub(in crate::daemon) fn claim_daemon_reconcile_operation(
        &mut self,
    ) -> Option<ClaimedSyncOperation> {
        self.claim_daemon_sync_operation_filtered(true)
    }

    fn claim_daemon_sync_operation_filtered(
        &mut self,
        reconcile_only: bool,
    ) -> Option<ClaimedSyncOperation> {
        let mut full_scan_reason = None;
        self.metadata_store_for_write("metadata_store(prepare_daemon_sync_operations)", |store| {
            let now_time = OffsetDateTime::now_utc();
            let now = format_timestamp(now_time);
            let workspace_id = self.options.args.workspace_id();
            let device_id = DeviceId::new(self.options.args.device_id.clone());
            let conflict_operations = pending_conflict_occurrence_operations(
                &self.options.args.state_root,
                &workspace_id,
                &device_id,
                &now,
            )
            .map_err(|error| MetadataError::InvalidStorageMetadata(error.to_string()))?;
            for operation in &conflict_operations {
                store.enqueue_sync_operation(operation)?;
            }
            if let Some(operation) =
                pending_work_view_overlay_sync_operation(store, &workspace_id, &device_id)?
            {
                store.enqueue_sync_operation(&operation)?;
            }
            let conflict_preparation_required =
                conflict_occurrence_preparation_required(&self.options.args.state_root)
                    .map_err(|error| MetadataError::InvalidStorageMetadata(error.to_string()))?;
            let operation_kind = SyncOperationKind::Reconcile;
            let has_active_reconcile = store
                .active_sync_operation_for_device(&workspace_id, operation_kind, &device_id)
                .ok()
                .flatten()
                .is_some();
            if !has_active_reconcile
                && let Some(request) = self.daemon_reconcile_request(
                    store,
                    &workspace_id,
                    &device_id,
                    &now,
                    conflict_preparation_required,
                )
            {
                let operation_nonce = stable_token(&format!(
                    "{}:{}:{}:{}",
                    self.options.args.device_id,
                    self.tick_count,
                    now,
                    std::process::id()
                ));
                let operation_id = format!("daemon-sync-{}", operation_nonce);
                let idempotency_key = format!(
                    "daemon-sync:{}:{}:{}",
                    self.options.args.device_id, self.tick_count, operation_nonce
                );
                let record = SyncOperationRecord {
                    id: operation_id,
                    workspace_id: workspace_id.clone(),
                    kind: operation_kind,
                    resource_key: SyncResourceKey::workspace_sync(workspace_id.clone()),
                    state: SyncOperationState::Queued,
                    idempotency_key,
                    base_version: None,
                    base_snapshot_id: None,
                    target_snapshot_id: None,
                    device_id: Some(device_id),
                    payload_json: daemon_json(&DaemonReconcilePayloadJson {
                        root: self.options.args.root.display().to_string(),
                        state_root: self.options.args.state_root.display().to_string(),
                        tick_count: self.tick_count,
                    }),
                    attempt_count: 0,
                    claimed_by: None,
                    claim_generation: 0,
                    heartbeat_at: None,
                    lease_expires_at: None,
                    cancellation_requested_at: None,
                    next_attempt_at: None,
                    result_json: None,
                    last_error_code: None,
                    last_error: None,
                    created_at: now.clone(),
                    updated_at: now.clone(),
                };
                let enqueue_result = store.enqueue_sync_operation(&record);
                if enqueue_result.is_ok()
                    && let DaemonReconcileRequest::Full(reason) = request
                {
                    full_scan_reason = Some(reason);
                }
                self.store_health
                    .record("enqueue_sync_operation", enqueue_result);
            }
            Ok(())
        });
        if let Some(reason) = full_scan_reason {
            self.pending_dirty.force_full(reason);
        }
        if reconcile_only {
            self.claim_ready_reconcile_sync_operation()
        } else {
            self.claim_ready_daemon_sync_operation()
        }
    }

    pub(in crate::daemon) fn claim_ready_daemon_sync_operation(
        &self,
    ) -> Option<ClaimedSyncOperation> {
        self.metadata_store_for_write(
            "metadata_store(claim_ready_daemon_sync_operation)",
            |store| {
                let now_time = OffsetDateTime::now_utc();
                let now = format_timestamp(now_time);
                let lease_expires_at = format_timestamp(
                    now_time + time::Duration::seconds(SYNC_CLAIM_TIMEOUT_SECONDS),
                );
                let workspace_id = self.options.args.workspace_id();
                Ok(self
                    .store_health
                    .record(
                        "claim_next_sync_operation",
                        store.claim_next_sync_operation(
                            &workspace_id,
                            &self.claimant_id,
                            &now,
                            &lease_expires_at,
                        ),
                    )
                    .flatten())
            },
        )
        .flatten()
    }

    pub(in crate::daemon) fn claim_ready_reconcile_sync_operation(
        &self,
    ) -> Option<ClaimedSyncOperation> {
        self.metadata_store_for_write(
            "metadata_store(claim_ready_reconcile_sync_operation)",
            |store| {
                let now_time = OffsetDateTime::now_utc();
                let now = format_timestamp(now_time);
                let lease_expires_at = format_timestamp(
                    now_time + time::Duration::seconds(SYNC_CLAIM_TIMEOUT_SECONDS),
                );
                let workspace_id = self.options.args.workspace_id();
                Ok(self
                    .store_health
                    .record(
                        "claim_next_reconcile_sync_operation",
                        store.claim_next_reconcile_sync_operation(
                            &workspace_id,
                            &self.claimant_id,
                            &now,
                            &lease_expires_at,
                        ),
                    )
                    .flatten())
            },
        )
        .flatten()
    }

    #[cfg(test)]
    pub(in crate::daemon) fn should_enqueue_daemon_reconcile(
        &self,
        store: &MetadataStore,
        workspace_id: &WorkspaceId,
        device_id: &DeviceId,
        now: &str,
    ) -> bool {
        let conflict_preparation_required =
            conflict_occurrence_preparation_required(&self.options.args.state_root).unwrap_or(true);
        self.daemon_reconcile_request(
            store,
            workspace_id,
            device_id,
            now,
            conflict_preparation_required,
        )
        .is_some()
    }

    fn daemon_reconcile_request(
        &self,
        store: &MetadataStore,
        workspace_id: &WorkspaceId,
        device_id: &DeviceId,
        now: &str,
        conflict_preparation_required: bool,
    ) -> Option<DaemonReconcileRequest> {
        if conflict_preparation_required {
            return Some(DaemonReconcileRequest::Full(
                FullScanReason::DivergenceRecovery,
            ));
        }
        if self.watcher_recovery.full_reconcile_required {
            return Some(DaemonReconcileRequest::Full(
                FullScanReason::WatcherOverflow,
            ));
        }
        let Some(last_completed) =
            latest_completed_daemon_reconcile(store, workspace_id, device_id)
        else {
            return Some(DaemonReconcileRequest::Normal);
        };
        if local_writes_after(store, workspace_id, device_id, &last_completed.updated_at) {
            return Some(DaemonReconcileRequest::Normal);
        }
        if remote_cursor_ahead_of_local_head(store, workspace_id) {
            return Some(DaemonReconcileRequest::Normal);
        }
        safety_reconcile_due(&last_completed.updated_at, self.options.interval, now)
            .then_some(DaemonReconcileRequest::Full(FullScanReason::VerifyDue))
    }

    pub(in crate::daemon) fn requeue_expired_sync_claims(&self) {
        self.metadata_store_for_write("metadata_store(requeue_expired_sync_claims)", |store| {
            let now = current_timestamp();
            self.store_health.record(
                "requeue_expired_sync_claims",
                store.requeue_expired_sync_claims(&self.options.args.workspace_id(), &now),
            );
            Ok(())
        });
    }

    pub(in crate::daemon) fn sweep_local_metadata_if_due(&self) {
        if !local_metadata_sweep_due(self.tick_count, self.options.interval) {
            return;
        }
        let conflicts = match load_conflict_records(&self.options.args.state_root) {
            Ok(records) => records
                .into_iter()
                .filter(|record| record.state == ConflictState::Unresolved)
                .map(|record| ConflictSnapshotRetention {
                    conflict_id: record.id,
                    base_snapshot_id: record.base_snapshot_id.map(SnapshotId::new),
                    remote_snapshot_id: record.remote_snapshot_id.map(SnapshotId::new),
                })
                .collect::<Vec<_>>(),
            Err(_) => {
                eprintln!("bowline-daemon metadata retention skipped: conflict-state-unavailable");
                return;
            }
        };
        self.metadata_store_for_maintenance("metadata_store(prune_local_metadata)", |store| {
            let now = current_timestamp();
            let policy = LocalMetadataRetentionPolicy::default();
            let report = self.store_health.record(
                "prune_local_metadata",
                store.prune_local_metadata(&self.options.args.workspace_id(), &policy, &now),
            );
            if let Some(report) = report
                && (report.local_writes_deleted > 0 || report.completed_sync_deleted > 0)
            {
                eprintln!(
                    "bowline-daemon pruned local metadata: localWrites={}, completedSync={}",
                    report.local_writes_deleted, report.completed_sync_deleted
                );
            }
            let maintenance = self.store_health.record(
                "maintain_snapshot_retention",
                store.maintain_snapshot_retention(
                    &self.options.args.workspace_id(),
                    &conflicts,
                    &policy,
                    &now,
                ),
            );
            if let Some(report) = maintenance
                && (report.pins_acquired > 0
                    || report.pins_updated > 0
                    || report.pins_released > 0
                    || report.snapshots_deleted > 0
                    || report.metadata_records_deleted > 0
                    || report.cache_files_deleted > 0)
            {
                eprintln!(
                    "bowline-daemon maintained snapshot metadata: pinsActive={}, pinsAcquired={}, pinsUpdated={}, pinsReleased={}, snapshotsDeleted={}, gcProcessed={}, gcMarked={}, recordsDeleted={}, cacheFilesDeleted={}, cacheBytesDeleted={}, gcComplete={}",
                    report.active_pins,
                    report.pins_acquired,
                    report.pins_updated,
                    report.pins_released,
                    report.snapshots_deleted,
                    report.gc_records_processed,
                    report.gc_records_marked,
                    report.metadata_records_deleted,
                    report.cache_files_deleted,
                    report.cache_bytes_deleted,
                    report.gc_complete,
                );
            }
            Ok(())
        });
    }

    pub(in crate::daemon) fn complete_daemon_sync_operation(
        &self,
        claim: &SyncClaimHandle,
        summary: &SyncOnceSummary,
    ) -> bool {
        self.metadata_store_for_write("metadata_store(complete_daemon_sync_operation)", |store| {
            let now_time = OffsetDateTime::now_utc();
            let now = format_timestamp(now_time);
            let lease_expires_at =
                format_timestamp(now_time + time::Duration::seconds(SYNC_CLAIM_TIMEOUT_SECONDS));
            let ownership = self.store_health.record(
                "renew_sync_operation_claim(before_complete)",
                store.renew_sync_operation_claim(claim, &now, &lease_expires_at),
            );
            if ownership != Some(SyncClaimTransition::Applied) {
                return Ok(false);
            }
            let operation = store
                .sync_operation_by_id(claim.operation_id())?
                .ok_or_else(|| {
                    MetadataError::InvalidStorageMetadata(
                        "claimed sync operation disappeared before completion".to_string(),
                    )
                })?;
            let reconciliation_required =
                claim.claimed_from_state() == SyncOperationState::ReconciliationRequired;
            let payload = daemon_json(&SyncCompletionPayloadJson {
                outcome: summary.sync_state(),
                workspace_id: summary.workspace_id.as_str(),
                snapshot_id: summary.snapshot_id.as_str(),
                version: summary.version,
                conflict_count: summary.conflict_count,
                scan: summary.scan.clone(),
            });
            let mut transition = match store.check_sync_operation_claim(claim)? {
                SyncClaimCheck::Owned => self.store_health.record(
                    "complete_sync_operation",
                    store.complete_claimed_sync_operation(claim, &payload, &now),
                ),
                SyncClaimCheck::CancellationRequested
                    if summary.has_committed_effect() || reconciliation_required =>
                {
                    let committed_result = serde_json::from_str(&payload).map_err(|error| {
                        MetadataError::InvalidStorageMetadata(error.to_string())
                    })?;
                    self.store_health.record(
                        "complete_committed_cancelled_late",
                        store.complete_committed_cancelled_late_sync_operation(
                            claim,
                            &SyncCommittedCancelledLateResult::new(
                                operation.kind,
                                committed_result,
                            ),
                            &now,
                        ),
                    )
                }
                SyncClaimCheck::CancellationRequested => self.store_health.record(
                    "cancel_sync_operation(before_complete)",
                    store.cancel_claimed_sync_operation(claim, r#"{"outcome":"cancelled"}"#, &now),
                ),
                SyncClaimCheck::OwnershipLost => Some(SyncClaimTransition::OwnershipLost),
            };
            if transition == Some(SyncClaimTransition::OwnershipLost)
                && store.check_sync_operation_claim(claim)? == SyncClaimCheck::CancellationRequested
            {
                transition = if summary.has_committed_effect() || reconciliation_required {
                    let committed_result = serde_json::from_str(&payload).map_err(|error| {
                        MetadataError::InvalidStorageMetadata(error.to_string())
                    })?;
                    self.store_health.record(
                        "complete_committed_cancelled_late(race)",
                        store.complete_committed_cancelled_late_sync_operation(
                            claim,
                            &SyncCommittedCancelledLateResult::new(
                                operation.kind,
                                committed_result,
                            ),
                            &now,
                        ),
                    )
                } else {
                    self.store_health.record(
                        "cancel_sync_operation(before_complete_race)",
                        store.cancel_claimed_sync_operation(
                            claim,
                            r#"{"outcome":"cancelled"}"#,
                            &now,
                        ),
                    )
                };
            }
            if transition == Some(SyncClaimTransition::Applied)
                && store
                    .sync_operation_by_id(claim.operation_id())?
                    .is_some_and(|operation| operation.state == SyncOperationState::Completed)
            {
                self.append_sync_completed_event(store, claim.operation_id(), summary, &now);
            }
            Ok(transition == Some(SyncClaimTransition::Applied))
        })
        .unwrap_or(false)
    }

    pub(in crate::daemon) fn record_remote_ref_cursor(&self, summary: &SyncOnceSummary) {
        self.metadata_store_for_write("metadata_store(record_remote_ref_cursor)", |store| {
            self.store_health.record(
                "put_remote_ref_cursor(sync_summary)",
                store.put_remote_ref_cursor(&RemoteRefCursorRecord {
                    workspace_id: WorkspaceId::new(summary.workspace_id.clone()),
                    cursor: None,
                    last_observed_version: Some(summary.version),
                    last_observed_snapshot_id: Some(summary.snapshot_id.clone()),
                    updated_at: current_timestamp(),
                }),
            );
            Ok(())
        });
    }

    pub(in crate::daemon) fn fail_daemon_sync_operation(
        &self,
        claim: &SyncClaimHandle,
        error: &SyncOnceError,
    ) -> bool {
        self.metadata_store_for_write("metadata_store(fail_daemon_sync_operation)", |store| {
            let now_time = OffsetDateTime::now_utc();
            let now = format_timestamp(now_time);
            let lease_expires_at =
                format_timestamp(now_time + time::Duration::seconds(SYNC_CLAIM_TIMEOUT_SECONDS));
            let ownership = self.store_health.record(
                "renew_sync_operation_claim(before_failure)",
                store.renew_sync_operation_claim(claim, &now, &lease_expires_at),
            );
            if ownership != Some(SyncClaimTransition::Applied) {
                return Ok(false);
            }
            if claim.claimed_from_state() == SyncOperationState::ReconciliationRequired {
                return Ok(self.store_health.record(
                    "defer_sync_operation_reconciliation",
                    store.defer_claimed_sync_operation_reconciliation(
                        claim,
                        &error.to_string(),
                        &now,
                    ),
                ) == Some(SyncClaimTransition::Applied));
            }
            match store.check_sync_operation_claim(claim)? {
                SyncClaimCheck::CancellationRequested => {
                    if error.remote_domain_committed() {
                        return Ok(self.store_health.record(
                            "defer_sync_operation_reconciliation(after_remote_commit)",
                            store.defer_claimed_sync_operation_reconciliation(
                                claim,
                                &error.to_string(),
                                &now,
                            ),
                        ) == Some(SyncClaimTransition::Applied));
                    }
                    return Ok(self.store_health.record(
                        "cancel_sync_operation(before_failure)",
                        store.cancel_claimed_sync_operation(
                            claim,
                            r#"{"outcome":"cancelled"}"#,
                            &now,
                        ),
                    ) == Some(SyncClaimTransition::Applied));
                }
                SyncClaimCheck::Owned => {}
                SyncClaimCheck::OwnershipLost => return Ok(false),
            }
            let original_action = error.disposition();
            let attempt_count = store
                .sync_operation_by_id(claim.operation_id())?
                .map(|operation| operation.attempt_count)
                .unwrap_or_default();
            let action = original_action.bounded_by_retry_budget(attempt_count);
            let retry_budget_exhausted = original_action == SyncFailureAction::Retry
                && action == SyncFailureAction::Attention;
            let message = if retry_budget_exhausted {
                "sync operation exhausted its automatic retry budget".to_string()
            } else {
                error.to_string()
            };
            let error_code = if retry_budget_exhausted {
                "retry-budget-exhausted"
            } else {
                error.external_failure_code().as_code()
            };
            let transition = match action {
                SyncFailureAction::Attention => self.store_health.record(
                    "mark_sync_operation_attention",
                    store.mark_claimed_sync_operation_attention(claim, error_code, &message, &now),
                ),
                SyncFailureAction::Offline => {
                    let retry_at = self.next_sync_attempt_at(store, claim.operation_id());
                    self.store_health.record(
                        "block_sync_operation_offline",
                        store.block_claimed_sync_operation_offline(
                            claim, error_code, &message, &retry_at, &now,
                        ),
                    )
                }
                SyncFailureAction::Retry => {
                    let retry_at = self.next_sync_attempt_at(store, claim.operation_id());
                    self.store_health.record(
                        "fail_sync_operation_for_retry",
                        store.fail_claimed_sync_operation_for_retry(
                            claim, error_code, &message, &retry_at, &now,
                        ),
                    )
                }
            };
            if transition == Some(SyncClaimTransition::Applied) {
                self.append_sync_failure_event(store, claim.operation_id(), action, &now);
            }
            Ok(transition == Some(SyncClaimTransition::Applied))
        })
        .unwrap_or(false)
    }

    pub(in crate::daemon) fn cancel_daemon_sync_operation(&self, claim: &SyncClaimHandle) -> bool {
        self.metadata_store_for_write("metadata_store(cancel_daemon_sync_operation)", |store| {
            Ok(self.store_health.record(
                "cancel_sync_operation",
                store.cancel_claimed_sync_operation(
                    claim,
                    r#"{"outcome":"cancelled"}"#,
                    &current_timestamp(),
                ),
            ) == Some(SyncClaimTransition::Applied))
        })
        .unwrap_or(false)
    }

    pub(super) fn record_sync_ownership_lost(&mut self) {
        self.next_remote_observe = Instant::now();
        self.record_component_states(
            SyncComponentState::Degraded,
            self.watcher_component_state(),
            "ownership-uncertain",
        );
        self.last_json = self.waiting_for_sync_queue_json();
        self.next_tick = Instant::now() + self.options.interval;
    }

    pub(in crate::daemon) fn next_sync_attempt_at(
        &self,
        store: &MetadataStore,
        operation_id: &str,
    ) -> String {
        let attempt_count = store
            .sync_operation_by_id(operation_id)
            .ok()
            .flatten()
            .map(|operation| operation.attempt_count)
            .unwrap_or(1);
        format_timestamp(
            OffsetDateTime::now_utc()
                + time::Duration::seconds(retry_delay_seconds(operation_id, attempt_count)),
        )
    }
}

pub(in crate::daemon) fn local_metadata_sweep_due(tick_count: u64, interval: Duration) -> bool {
    if tick_count == 0 {
        return false;
    }
    let interval_seconds = interval.as_secs().max(1);
    let ticks_per_sweep = LOCAL_METADATA_SWEEP_SECONDS
        .div_ceil(interval_seconds)
        .max(1);
    tick_count.is_multiple_of(ticks_per_sweep)
}

pub(super) fn forced_full_reason_survives_retry(reason: FullScanReason) -> bool {
    matches!(
        reason,
        FullScanReason::PolicyChanged
            | FullScanReason::WatcherUnavailable
            | FullScanReason::WatcherOverflow
            | FullScanReason::DirtyCapExceeded
            | FullScanReason::HeadManifestUnavailable
            | FullScanReason::VerifyDue
            | FullScanReason::DivergenceRecovery
    )
}

pub(super) fn validate_conflict_operation(
    operation: &SyncOperationRecord,
    input: &ConflictOccurrenceReconcile,
) -> Result<(), SyncOnceError> {
    let expected_resource =
        SyncResourceKey::conflict_followup(input.workspace_id.clone(), input.conflict_id.clone());
    if operation.kind != SyncOperationKind::ConflictOccurrenceReconcile
        || operation.workspace_id != input.workspace_id
        || operation.device_id.as_ref() != Some(&input.device_id)
        || operation.resource_key != expected_resource
    {
        return Err(SyncOnceError::InvalidOperationPayload(
            "conflict occurrence operation envelope does not match its payload".to_string(),
        ));
    }
    Ok(())
}

pub(super) fn local_conflict_state(state: ConflictOccurrenceState) -> ConflictState {
    match state {
        ConflictOccurrenceState::Unresolved => ConflictState::Unresolved,
        ConflictOccurrenceState::Accepted => ConflictState::Accepted,
        ConflictOccurrenceState::Rejected => ConflictState::Rejected,
    }
}
