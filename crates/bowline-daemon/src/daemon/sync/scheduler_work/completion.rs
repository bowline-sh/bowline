use super::*;

impl ContinuousSyncRuntime {
    pub(in crate::daemon) fn apply_worker_completion(&mut self, completion: WorkerCompletion) {
        self.next_tick = Instant::now() + self.options.interval;
        match completion {
            WorkerCompletion::Reconcile(completion) => {
                self.sync_once = completion.executor;
                self.apply_reconcile_result(
                    &completion.claimed,
                    completion.attempted_scan_scope,
                    completion.result,
                );
            }
            WorkerCompletion::Conflict(completion) => {
                self.apply_conflict_result(&completion.claimed, completion.result);
            }
            WorkerCompletion::Overlay(completion) => {
                self.apply_overlay_result(&completion.claimed, completion.result);
            }
            WorkerCompletion::WorkViewAccept(completion) => {
                self.finish_work_view_accept(&completion.claimed, completion.result);
            }
            WorkerCompletion::Panicked(recovery) => self.apply_worker_loss(
                recovery,
                "worker-panicked",
                "daemon worker panicked before reporting completion",
            ),
            WorkerCompletion::WorkerLost(recovery) => self.apply_worker_loss(
                recovery,
                "worker-lost",
                "daemon worker terminated before reporting completion",
            ),
        }
    }

    pub(in crate::daemon) fn requeue_dispatch_failure(&mut self, work: PreparedDaemonWork) -> bool {
        let PreparedDaemonWork { recovery, task, .. } = work;
        if let PreparedDaemonTask::Reconcile(work) = task {
            self.sync_once = work.executor;
        }
        if let Some(scope) = recovery.attempted_scan_scope.clone() {
            self.restore_attempted_scan_scope(scope);
        }
        let now = current_timestamp();
        let transition =
            self.metadata_store_for_write("metadata_store(requeue_dispatch_failure)", |store| {
                match &recovery.claim {
                    PreparedDaemonClaim::Sync(claim) => {
                        if store.check_sync_operation_claim(claim)?
                            == SyncClaimCheck::CancellationRequested
                        {
                            return store.cancel_claimed_sync_operation(
                                claim,
                                r#"{"outcome":"cancelled"}"#,
                                &now,
                            );
                        }
                        store.requeue_claimed_sync_operation_after_dispatch_failure(
                            claim,
                            "dispatch-unavailable",
                            "daemon lane disconnected before execution",
                            &now,
                        )
                    }
                    PreparedDaemonClaim::WorkViewAccept(claim) => {
                        let transition = if store.check_work_view_accept_claim(claim, &now)?
                            == WorkViewAcceptClaimCheck::CancellationRequested
                        {
                            store.cancel_claimed_work_view_accept(
                                claim,
                                r#"{"outcome":"cancelled"}"#,
                                &now,
                            )
                        } else {
                            store.requeue_claimed_work_view_accept_after_dispatch_failure(
                                claim,
                                "daemon sync lane disconnected before execution",
                                &now,
                            )
                        }?;
                        Ok(match transition {
                            WorkViewAcceptClaimTransition::Applied => SyncClaimTransition::Applied,
                            WorkViewAcceptClaimTransition::OwnershipLost => {
                                SyncClaimTransition::OwnershipLost
                            }
                        })
                    }
                }
            });
        transition == Some(SyncClaimTransition::Applied)
    }

    fn apply_worker_loss(
        &mut self,
        recovery: WorkerLossRecovery,
        error_code: &'static str,
        message: &'static str,
    ) {
        if let Some(scope) = recovery.attempted_scan_scope {
            self.restore_attempted_scan_scope(scope);
        }
        let now = current_timestamp();
        self.metadata_store_for_write(
            "metadata_store(record_worker_loss)",
            |store| match &recovery.claim {
                PreparedDaemonClaim::Sync(claim) => store
                    .record_claimed_sync_operation_worker_failure(claim, error_code, message, &now)
                    .map(|_| ()),
                PreparedDaemonClaim::WorkViewAccept(claim) => store
                    .record_claimed_work_view_accept_worker_failure(claim, message, &now)
                    .map(|_| ()),
            },
        );
        self.record_sync_ownership_lost();
    }

    fn apply_conflict_result(
        &mut self,
        claimed: &ClaimedSyncOperation,
        result: ConflictWorkerResult,
    ) {
        let applied = match result {
            ConflictWorkerResult::Completed {
                input,
                outcome,
                mark_local,
            } => self.finish_conflict_occurrence_operation(
                &claimed.claim,
                &input,
                outcome,
                mark_local,
            ),
            ConflictWorkerResult::Cancelled => self.cancel_daemon_sync_operation(&claimed.claim),
            ConflictWorkerResult::Failed(error) => {
                self.fail_daemon_sync_operation(&claimed.claim, &error)
            }
            ConflictWorkerResult::OwnershipLost => false,
        };
        if !applied {
            self.record_sync_ownership_lost();
        }
    }

    fn apply_overlay_result(
        &mut self,
        claimed: &ClaimedSyncOperation,
        result: OverlayWorkerResult,
    ) {
        match result {
            OverlayWorkerResult::Completed(result) => {
                if !self.finish_work_view_overlay_operation(&claimed.claim, result) {
                    self.record_sync_ownership_lost();
                }
            }
            OverlayWorkerResult::Cancelled => {
                if !self.cancel_daemon_sync_operation(&claimed.claim) {
                    self.record_sync_ownership_lost();
                }
                self.set_work_view_overlay_component_state("degraded");
            }
            OverlayWorkerResult::Failed(error) => {
                self.fail_overlay_operation(&claimed.claim, &error);
            }
            OverlayWorkerResult::OwnershipLost => self.record_sync_ownership_lost(),
        }
    }

    fn apply_reconcile_result(
        &mut self,
        claimed: &ClaimedSyncOperation,
        attempted_scan_scope: ScanScope,
        result: Result<SyncOnceSummary, SyncOnceError>,
    ) {
        match result {
            Ok(summary) => self.apply_reconcile_success(claimed, summary),
            Err(error) => self.apply_reconcile_failure(claimed, attempted_scan_scope, error),
        }
    }

    fn apply_reconcile_success(
        &mut self,
        claimed: &ClaimedSyncOperation,
        summary: SyncOnceSummary,
    ) {
        if !self.complete_daemon_sync_operation(&claimed.claim, &summary) {
            self.record_sync_ownership_lost();
            return;
        }
        self.watcher_recovery.full_reconcile_required = false;
        self.record_remote_ref_cursor(&summary);
        self.record_component_states(
            SyncComponentState::Ready,
            self.watcher_component_state(),
            "online",
        );
        if let Err(error) = self.claim_pending_dispatch_lease_if_due(self.awaiting_handoff) {
            eprintln!("bowline-daemon dispatch claim failed: {error}");
        }
        self.last_json = if self.remote_observer_is_unavailable() {
            self.remote_observer_failure_status_json()
        } else {
            daemon_json(&SyncSuccessStatusJson {
                state: summary.daemon_state(),
                tick_count: self.tick_count,
                watcher_state: self.watcher_state_json(),
                last_outcome: summary.sync_state(),
                workspace_id: summary.workspace_id.as_str(),
                snapshot_id: summary.snapshot_id.as_str(),
                version: summary.version,
                conflict_count: summary.conflict_count,
                scan: summary.scan.clone(),
                queue_counts: SyncOperationCountsJson::from(&self.queue_counts()),
                local_head: self.local_head_payload(),
                remote_head: self.remote_head_payload(),
            })
        };
    }

    fn apply_reconcile_failure(
        &mut self,
        claimed: &ClaimedSyncOperation,
        attempted_scan_scope: ScanScope,
        error: SyncOnceError,
    ) {
        if error.is_stat_cache_divergence() {
            self.pending_dirty
                .force_full(FullScanReason::DivergenceRecovery);
        } else {
            self.restore_attempted_scan_scope(attempted_scan_scope);
        }
        if !self.fail_daemon_sync_operation(&claimed.claim, &error) {
            self.record_sync_ownership_lost();
            return;
        }
        if self.queue_counts().has_no_pending_work() {
            self.record_component_states(
                SyncComponentState::Ready,
                self.watcher_component_state(),
                "online",
            );
            self.last_json = self.waiting_for_sync_queue_json();
        } else {
            self.record_component_states(
                SyncComponentState::Degraded,
                self.watcher_component_state(),
                error.network_state_label(),
            );
            self.last_json = daemon_json(&LimitedSyncStatusJson {
                state: "limited",
                tick_count: self.tick_count,
                watcher_state: self.watcher_state_json(),
                limited_capability: "continuous sync",
                unavailable_because: error.external_failure_code().as_code(),
                blocked_action: "sync ~/Code",
                still_works: &["local edits", "status", "manual sync-once diagnostics"],
                queue_counts: SyncOperationCountsJson::from(&self.queue_counts()),
                local_head: self.local_head_payload(),
                remote_head: self.remote_head_payload(),
            });
        }
    }

    fn restore_attempted_scan_scope(&mut self, attempted_scan_scope: ScanScope) {
        match attempted_scan_scope {
            ScanScope::Full(reason) if forced_full_reason_survives_retry(reason) => {
                self.pending_dirty.force_full(reason);
            }
            ScanScope::DirtySubtrees {
                roots,
                root_shallow,
            } => {
                self.pending_dirty.restore_roots(roots);
                if root_shallow {
                    let files = self.pending_dirty.take_drained_root_dirty_files();
                    self.pending_dirty.restore_root_dirty_files(files);
                }
            }
            ScanScope::RootShallow => {
                let files = self.pending_dirty.take_drained_root_dirty_files();
                self.pending_dirty.restore_root_dirty_files(files);
            }
            ScanScope::Full(_) => {}
        }
    }
}
