use super::*;

impl ContinuousSyncRuntime {
    #[cfg(test)]
    pub(in crate::daemon) fn process_claimed_conflict_occurrence(
        &mut self,
        claimed: ClaimedSyncOperation,
        mut reconcile: impl FnMut(
            ConflictOccurrenceReconcile,
        ) -> Result<ConflictReconcileResult, SyncOnceError>,
    ) {
        let input = match decode_conflict_occurrence_operation(&claimed.operation) {
            Ok(input) => input,
            Err(error) => {
                self.fail_daemon_sync_operation(
                    &claimed.claim,
                    &SyncOnceError::InvalidOperationPayload(error.to_string()),
                );
                return;
            }
        };
        if let Err(error) = validate_conflict_operation(&claimed.operation, &input) {
            self.fail_daemon_sync_operation(&claimed.claim, &error);
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
        match self.check_sync_claim_boundary(&claimed.claim, "before_conflict_remote_call") {
            Some(SyncClaimCheck::Owned) => {}
            Some(SyncClaimCheck::CancellationRequested) => {
                drop(lease);
                self.cancel_daemon_sync_operation(&claimed.claim);
                return;
            }
            Some(SyncClaimCheck::OwnershipLost) | None => {
                drop(lease);
                self.record_sync_ownership_lost();
                return;
            }
        }
        let local_state = local_conflict_state(input.desired_state);
        let current = conflict_occurrence_is_current(
            &self.options.args.state_root,
            input.conflict_id.as_str(),
            input.occurrence_version,
            local_state,
        );
        let current = match current {
            Ok(current) => current,
            Err(error) => {
                let ownership = lease.stop();
                if ownership == ClaimOwnership::Lost
                    || !self.fail_daemon_sync_operation(
                        &claimed.claim,
                        &SyncOnceError::InvalidOperationPayload(error.to_string()),
                    )
                {
                    self.record_sync_ownership_lost();
                }
                return;
            }
        };
        if !current {
            if lease.stop() == ClaimOwnership::Lost
                || !self.finish_conflict_occurrence_operation(
                    &claimed.claim,
                    &input,
                    ConflictReconcileOutcome::Superseded,
                    false,
                )
            {
                self.record_sync_ownership_lost();
            }
            return;
        }
        let result = reconcile(input.clone());
        if lease.stop() == ClaimOwnership::Lost {
            self.record_sync_ownership_lost();
            return;
        }
        match result {
            Ok(result) => {
                let mark_local = matches!(
                    result.outcome,
                    ConflictReconcileOutcome::Applied | ConflictReconcileOutcome::Idempotent
                );
                if !self.finish_conflict_occurrence_operation(
                    &claimed.claim,
                    &input,
                    result.outcome,
                    mark_local,
                ) {
                    self.record_sync_ownership_lost();
                }
            }
            Err(error) => {
                if !self.fail_daemon_sync_operation(&claimed.claim, &error) {
                    self.record_sync_ownership_lost();
                }
            }
        }
    }

    #[cfg(test)]
    pub(super) fn check_sync_claim_boundary(
        &self,
        claim: &SyncClaimHandle,
        context: &'static str,
    ) -> Option<SyncClaimCheck> {
        self.metadata_store_for_write("metadata_store(renew_sync_claim)", |store| {
            let now_time = OffsetDateTime::now_utc();
            let now = format_timestamp(now_time);
            let lease_expires_at =
                format_timestamp(now_time + time::Duration::seconds(SYNC_CLAIM_TIMEOUT_SECONDS));
            let renewed = self.store_health.record(
                context,
                store.renew_sync_operation_claim(claim, &now, &lease_expires_at),
            );
            if renewed != Some(SyncClaimTransition::Applied) {
                return Ok(SyncClaimCheck::OwnershipLost);
            }
            store.check_sync_operation_claim(claim)
        })
    }

    pub(in crate::daemon) fn finish_conflict_occurrence_operation(
        &self,
        claim: &SyncClaimHandle,
        input: &ConflictOccurrenceReconcile,
        remote_outcome: ConflictReconcileOutcome,
        mark_local: bool,
    ) -> bool {
        self.metadata_store_for_write(
            "metadata_store(finish_conflict_occurrence_operation)",
            |store| {
                let now_time = OffsetDateTime::now_utc();
                let now = format_timestamp(now_time);
                let lease_expires_at = format_timestamp(
                    now_time + time::Duration::seconds(SYNC_CLAIM_TIMEOUT_SECONDS),
                );
                if self.store_health.record(
                    "renew_sync_operation_claim(before_conflict_terminal_mark)",
                    store.renew_sync_operation_claim(claim, &now, &lease_expires_at),
                ) != Some(SyncClaimTransition::Applied)
                {
                    return Ok(false);
                }
                let outcome = if mark_local {
                    match mark_conflict_occurrence_reconciled(
                        &self.options.args.state_root,
                        input.conflict_id.as_str(),
                        input.occurrence_version,
                        local_conflict_state(input.desired_state),
                        &now,
                    ) {
                        Ok(true) => remote_outcome,
                        Ok(false) => ConflictReconcileOutcome::Superseded,
                        Err(error) => {
                            return Err(MetadataError::InvalidStorageMetadata(error.to_string()));
                        }
                    }
                } else {
                    ConflictReconcileOutcome::Superseded
                };
                let result_json = conflict_occurrence_queue_result(outcome)
                    .map_err(|error| MetadataError::InvalidStorageMetadata(error.to_string()))?;
                let mut transition = match store.check_sync_operation_claim(claim)? {
                    SyncClaimCheck::Owned => self.store_health.record(
                        "complete_sync_operation(conflict_occurrence)",
                        store.complete_claimed_sync_operation(claim, &result_json, &now),
                    ),
                    SyncClaimCheck::CancellationRequested if mark_local => {
                        let committed_result =
                            serde_json::from_str(&result_json).map_err(|error| {
                                MetadataError::InvalidStorageMetadata(error.to_string())
                            })?;
                        self.store_health.record(
                            "complete_committed_cancelled_late(conflict_occurrence)",
                            store.complete_committed_cancelled_late_sync_operation(
                                claim,
                                &SyncCommittedCancelledLateResult::new(
                                    SyncOperationKind::ConflictOccurrenceReconcile,
                                    committed_result,
                                ),
                                &now,
                            ),
                        )
                    }
                    SyncClaimCheck::CancellationRequested => self.store_health.record(
                        "cancel_sync_operation(conflict_occurrence)",
                        store.cancel_claimed_sync_operation(
                            claim,
                            r#"{"outcome":"cancelled"}"#,
                            &now,
                        ),
                    ),
                    SyncClaimCheck::OwnershipLost => Some(SyncClaimTransition::OwnershipLost),
                };
                if transition == Some(SyncClaimTransition::OwnershipLost)
                    && store.check_sync_operation_claim(claim)?
                        == SyncClaimCheck::CancellationRequested
                {
                    transition = if mark_local {
                        let committed_result =
                            serde_json::from_str(&result_json).map_err(|error| {
                                MetadataError::InvalidStorageMetadata(error.to_string())
                            })?;
                        self.store_health.record(
                            "complete_committed_cancelled_late(conflict_occurrence_race)",
                            store.complete_committed_cancelled_late_sync_operation(
                                claim,
                                &SyncCommittedCancelledLateResult::new(
                                    SyncOperationKind::ConflictOccurrenceReconcile,
                                    committed_result,
                                ),
                                &now,
                            ),
                        )
                    } else {
                        self.store_health.record(
                            "cancel_sync_operation(conflict_occurrence_race)",
                            store.cancel_claimed_sync_operation(
                                claim,
                                r#"{"outcome":"cancelled"}"#,
                                &now,
                            ),
                        )
                    };
                }
                Ok(transition == Some(SyncClaimTransition::Applied))
            },
        )
        .unwrap_or(false)
    }
}
