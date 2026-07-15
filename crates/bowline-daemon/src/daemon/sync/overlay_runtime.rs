use super::*;

impl ContinuousSyncRuntime {
    #[cfg(test)]
    pub(in crate::daemon) fn process_claimed_work_view_overlay_sync(
        &mut self,
        claimed: ClaimedSyncOperation,
        mut sync_overlays: impl FnMut(
            SyncOnceArgs,
            WorkViewOverlaySyncInput,
        ) -> Result<WorkViewOverlaySyncResult, SyncOnceError>,
    ) {
        let input = match decode_work_view_overlay_sync_operation(&claimed.operation) {
            Ok(input) => input,
            Err(error) => {
                self.fail_overlay_operation(
                    &claimed.claim,
                    &SyncOnceError::InvalidOperationPayload(error.to_string()),
                );
                return;
            }
        };
        if let Err(error) = validate_work_view_overlay_operation(&claimed.operation, &input) {
            self.fail_overlay_operation(&claimed.claim, &error);
            return;
        }
        let lease = match ClaimLeaseSupervisor::start(
            self.options.args.state_root.clone(),
            claimed.claim.clone(),
            ClaimLeasePolicy::default(),
        ) {
            Ok(lease) => lease,
            Err(error) => {
                eprintln!("bowline-daemon test claim supervisor could not start: {error}");
                self.record_sync_ownership_lost();
                return;
            }
        };
        match self.check_sync_claim_boundary(&claimed.claim, "before_work_view_overlay_sync") {
            Some(SyncClaimCheck::Owned) => {}
            Some(SyncClaimCheck::CancellationRequested)
                if claimed.claim.claimed_from_state()
                    == SyncOperationState::ReconciliationRequired => {}
            Some(SyncClaimCheck::CancellationRequested) => {
                drop(lease);
                self.cancel_daemon_sync_operation(&claimed.claim);
                self.set_work_view_overlay_component_state("degraded");
                return;
            }
            Some(SyncClaimCheck::OwnershipLost) | None => {
                drop(lease);
                self.record_sync_ownership_lost();
                return;
            }
        }
        let result = sync_overlays(self.options.args.clone(), input);
        if lease.stop() == ClaimOwnership::Lost {
            self.record_sync_ownership_lost();
            return;
        }
        match result {
            Ok(result) => {
                if !self.finish_work_view_overlay_operation(&claimed.claim, result) {
                    self.record_sync_ownership_lost();
                }
            }
            Err(error) => self.fail_overlay_operation(&claimed.claim, &error),
        }
    }

    pub(in crate::daemon) fn finish_work_view_overlay_operation(
        &self,
        claim: &SyncClaimHandle,
        result: WorkViewOverlaySyncResult,
    ) -> bool {
        self.metadata_store_for_write(
            "metadata_store(finish_work_view_overlay_operation)",
            |store| {
                let now_time = OffsetDateTime::now_utc();
                let now = format_timestamp(now_time);
                let lease_expires_at = format_timestamp(
                    now_time + time::Duration::seconds(SYNC_CLAIM_TIMEOUT_SECONDS),
                );
                if self.store_health.record(
                    "renew_sync_operation_claim(before_work_view_overlay_terminal_mark)",
                    store.renew_sync_operation_claim(claim, &now, &lease_expires_at),
                ) != Some(SyncClaimTransition::Applied)
                {
                    return Ok(false);
                }
                let result_json = work_view_overlay_sync_result(result)
                    .map_err(|error| MetadataError::InvalidStorageMetadata(error.to_string()))?;
                let committed_result: serde_json::Value = serde_json::from_str(&result_json)
                    .map_err(|error| MetadataError::InvalidStorageMetadata(error.to_string()))?;
                let mut transition = match store.check_sync_operation_claim(claim)? {
                    SyncClaimCheck::Owned => self.store_health.record(
                        "complete_sync_operation(work_view_overlay)",
                        store.complete_claimed_sync_operation(claim, &result_json, &now),
                    ),
                    SyncClaimCheck::CancellationRequested => self.store_health.record(
                        "complete_committed_cancelled_late(work_view_overlay)",
                        store.complete_committed_cancelled_late_sync_operation(
                            claim,
                            &SyncCommittedCancelledLateResult::new(
                                SyncOperationKind::WorkViewOverlaySync,
                                committed_result.clone(),
                            ),
                            &now,
                        ),
                    ),
                    SyncClaimCheck::OwnershipLost => Some(SyncClaimTransition::OwnershipLost),
                };
                if transition == Some(SyncClaimTransition::OwnershipLost)
                    && store.check_sync_operation_claim(claim)?
                        == SyncClaimCheck::CancellationRequested
                {
                    transition = self.store_health.record(
                        "complete_committed_cancelled_late(work_view_overlay_race)",
                        store.complete_committed_cancelled_late_sync_operation(
                            claim,
                            &SyncCommittedCancelledLateResult::new(
                                SyncOperationKind::WorkViewOverlaySync,
                                committed_result,
                            ),
                            &now,
                        ),
                    );
                }
                if transition == Some(SyncClaimTransition::Applied) {
                    store.set_component_state(
                        PostCommitSyncComponent::WorkViewOverlaySync.as_str(),
                        "ready",
                        &now,
                    )?;
                }
                Ok(transition == Some(SyncClaimTransition::Applied))
            },
        )
        .unwrap_or(false)
    }

    pub(in crate::daemon) fn fail_overlay_operation(
        &mut self,
        claim: &SyncClaimHandle,
        error: &SyncOnceError,
    ) {
        if self.defer_overlay_reconciliation_if_cancelled(claim, error) {
            self.set_work_view_overlay_component_state("degraded");
            return;
        }
        if !self.fail_daemon_sync_operation(claim, error) {
            self.record_sync_ownership_lost();
            return;
        }
        self.set_work_view_overlay_component_state("degraded");
    }

    fn defer_overlay_reconciliation_if_cancelled(
        &self,
        claim: &SyncClaimHandle,
        error: &SyncOnceError,
    ) -> bool {
        self.metadata_store_for_write(
            "metadata_store(defer_overlay_reconciliation_if_cancelled)",
            |store| {
                let now_time = OffsetDateTime::now_utc();
                let now = format_timestamp(now_time);
                let lease_expires_at = format_timestamp(
                    now_time + time::Duration::seconds(SYNC_CLAIM_TIMEOUT_SECONDS),
                );
                if self.store_health.record(
                    "renew_sync_operation_claim(before_overlay_failure)",
                    store.renew_sync_operation_claim(claim, &now, &lease_expires_at),
                ) != Some(SyncClaimTransition::Applied)
                {
                    return Ok(false);
                }
                if store.check_sync_operation_claim(claim)? != SyncClaimCheck::CancellationRequested
                {
                    return Ok(false);
                }
                Ok(self.store_health.record(
                    "defer_sync_operation_reconciliation(work_view_overlay)",
                    store.defer_claimed_sync_operation_reconciliation(
                        claim,
                        &error.to_string(),
                        &now,
                    ),
                ) == Some(SyncClaimTransition::Applied))
            },
        )
        .unwrap_or(false)
    }

    pub(in crate::daemon) fn set_work_view_overlay_component_state(&self, state: &'static str) {
        self.metadata_store_for_write(
            "metadata_store(set_work_view_overlay_component_state)",
            |store| {
                store.set_component_state(
                    PostCommitSyncComponent::WorkViewOverlaySync.as_str(),
                    state,
                    &current_timestamp(),
                )
            },
        );
    }
}

pub(in crate::daemon) fn validate_work_view_overlay_operation(
    operation: &SyncOperationRecord,
    input: &WorkViewOverlaySyncInput,
) -> Result<(), SyncOnceError> {
    let snapshot_id = input.snapshot_id.as_str();
    if operation.kind != SyncOperationKind::WorkViewOverlaySync
        || operation.workspace_id != input.workspace_id
        || operation.resource_key != SyncResourceKey::post_commit(input.workspace_id.clone())
        || operation.device_id.as_ref() != Some(&input.device_id)
        || operation.base_version != Some(input.workspace_version)
        || operation.base_snapshot_id.as_deref() != Some(snapshot_id)
        || operation.target_snapshot_id.as_deref() != Some(snapshot_id)
        || input.generated_at.is_empty()
    {
        return Err(SyncOnceError::InvalidOperationPayload(
            "work-view overlay operation envelope does not match its payload".to_string(),
        ));
    }
    Ok(())
}
