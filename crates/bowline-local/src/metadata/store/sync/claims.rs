use super::*;

impl MetadataStore {
    pub fn claim_next_sync_operation(
        &self,
        workspace_id: &WorkspaceId,
        claimant: &str,
        now: &str,
        lease_expires_at: &str,
    ) -> Result<Option<ClaimedSyncOperation>, MetadataError> {
        self.claim_next_sync_operation_filtered(
            workspace_id,
            claimant,
            now,
            lease_expires_at,
            SyncOperationClaimFilter::Any,
        )
    }

    pub fn claim_next_control_plane_sync_operation(
        &self,
        workspace_id: &WorkspaceId,
        claimant: &str,
        now: &str,
        lease_expires_at: &str,
    ) -> Result<Option<ClaimedSyncOperation>, MetadataError> {
        self.claim_next_sync_operation_filtered(
            workspace_id,
            claimant,
            now,
            lease_expires_at,
            SyncOperationClaimFilter::ControlPlane,
        )
    }

    pub fn claim_next_reconcile_sync_operation(
        &self,
        workspace_id: &WorkspaceId,
        claimant: &str,
        now: &str,
        lease_expires_at: &str,
    ) -> Result<Option<ClaimedSyncOperation>, MetadataError> {
        self.claim_next_sync_operation_filtered(
            workspace_id,
            claimant,
            now,
            lease_expires_at,
            SyncOperationClaimFilter::Reconcile,
        )
    }

    fn claim_next_sync_operation_filtered(
        &self,
        workspace_id: &WorkspaceId,
        claimant: &str,
        now: &str,
        lease_expires_at: &str,
        filter: SyncOperationClaimFilter,
    ) -> Result<Option<ClaimedSyncOperation>, MetadataError> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let Some((id, claimed_from_state)) = transaction
            .query_row(
                "SELECT id, state FROM sync_operations
                 WHERE workspace_id = ?1
                   AND (?3 != 1 OR kind IN (
                     'conflict-occurrence-reconcile',
                     'work-view-overlay-sync'
                   ))
                   AND (?3 != 2 OR kind = 'daemon-reconcile')
                   AND (
                     state = 'queued'
                     OR (state = 'waiting_retry' AND (next_attempt_at IS NULL OR next_attempt_at <= ?2))
                     OR (state = 'blocked_offline' AND (next_attempt_at IS NULL OR next_attempt_at <= ?2))
                     OR state = 'reconciliation_required'
                   )
                   AND NOT EXISTS (
                     SELECT 1 FROM sync_operations claimed
                     WHERE claimed.resource_key = sync_operations.resource_key
                       AND claimed.state = 'claimed'
                   )
                   AND NOT EXISTS (
                     SELECT 1 FROM work_view_accept_operations claimed_accept
                     WHERE sync_operations.kind = 'daemon-reconcile'
                       AND sync_operations.resource_key = ('workspace_sync:' || sync_operations.workspace_id)
                       AND claimed_accept.workspace_id = sync_operations.workspace_id
                       AND claimed_accept.state = 'claimed'
                   )
                 ORDER BY created_at, id
                 LIMIT 1",
                params![workspace_id.as_str(), now, filter.as_i64()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        deserialize_json_variant::<SyncOperationState>(row.get::<_, String>(1)?)?,
                    ))
                },
            )
            .optional()?
        else {
            transaction.commit()?;
            return Ok(None);
        };

        let token = SyncClaimToken::random()?;
        let changed = transaction.execute(
            "UPDATE sync_operations
             SET state = 'claimed',
                 claimed_by = ?1,
                 claim_token = ?2,
                 claim_generation = claim_generation + 1,
                 heartbeat_at = ?3,
                 lease_expires_at = ?4,
                 attempt_count = attempt_count + 1,
                 updated_at = ?3
             WHERE id = ?5
               AND workspace_id = ?6
               AND (
                 state = 'queued'
                 OR (state = 'waiting_retry' AND (next_attempt_at IS NULL OR next_attempt_at <= ?3))
                 OR (state = 'blocked_offline' AND (next_attempt_at IS NULL OR next_attempt_at <= ?3))
                 OR state = 'reconciliation_required'
               )",
            params![
                claimant,
                token.as_str(),
                now,
                lease_expires_at,
                id,
                workspace_id.as_str(),
            ],
        )?;
        if changed == 0 {
            transaction.commit()?;
            return Ok(None);
        }
        let (operation, generation) = transaction.query_row(
            "SELECT id, workspace_id, kind, resource_key, state, idempotency_key, base_version, base_snapshot_id,
                    target_snapshot_id, device_id, payload_json, attempt_count, claimed_by,
                    claim_generation, heartbeat_at, lease_expires_at, cancellation_requested_at,
                    next_attempt_at, result_json, last_error_code, last_error, created_at, updated_at
             FROM sync_operations
             WHERE id = ?1",
            [&id],
            |row| {
                let operation = sync_operation_from_row(row)?;
                let generation = operation.claim_generation;
                Ok((operation, generation))
            },
        )?;
        transaction.commit()?;
        Ok(Some(ClaimedSyncOperation {
            operation,
            claim: SyncClaimHandle {
                operation_id: id,
                owner: claimant.to_string(),
                token,
                generation,
                claimed_from_state,
            },
            lease_expires_at: lease_expires_at.to_string(),
        }))
    }

    pub fn renew_sync_operation_claim(
        &self,
        claim: &SyncClaimHandle,
        now: &str,
        lease_expires_at: &str,
    ) -> Result<SyncClaimTransition, MetadataError> {
        let changed = self.connection.execute(
            "UPDATE sync_operations
             SET heartbeat_at = ?5, lease_expires_at = ?6, updated_at = ?5
             WHERE id = ?1
               AND state = 'claimed'
               AND claimed_by = ?2
               AND claim_token = ?3
               AND claim_generation = ?4
               AND julianday(lease_expires_at) > julianday('now')",
            params![
                claim.operation_id(),
                claim.owner(),
                claim.token().as_str(),
                claim.generation(),
                now,
                lease_expires_at,
            ],
        )?;
        Ok(claim_transition(changed))
    }

    pub fn requeue_claimed_sync_operation_after_dispatch_failure(
        &self,
        claim: &SyncClaimHandle,
        error_code: &str,
        message: &str,
        now: &str,
    ) -> Result<SyncClaimTransition, MetadataError> {
        let changed = self.connection.execute(
            "UPDATE sync_operations
             SET state = 'queued',
                 claimed_by = NULL,
                 claim_token = NULL,
                 heartbeat_at = NULL,
                 lease_expires_at = NULL,
                 next_attempt_at = NULL,
                 last_error_code = ?5,
                 last_error = ?6,
                 updated_at = ?7
             WHERE id = ?1
               AND state = 'claimed'
               AND claimed_by = ?2
               AND claim_token = ?3
               AND claim_generation = ?4
               AND cancellation_requested_at IS NULL
               AND julianday(lease_expires_at) > julianday(?7)",
            params![
                claim.operation_id(),
                claim.owner(),
                claim.token().as_str(),
                claim.generation(),
                error_code,
                message,
                now,
            ],
        )?;
        Ok(claim_transition(changed))
    }

    pub fn record_claimed_sync_operation_worker_failure(
        &self,
        claim: &SyncClaimHandle,
        error_code: &str,
        message: &str,
        now: &str,
    ) -> Result<SyncClaimTransition, MetadataError> {
        let changed = self.connection.execute(
            "UPDATE sync_operations
             SET last_error_code = ?5, last_error = ?6, updated_at = ?7
             WHERE id = ?1
               AND state = 'claimed'
               AND claimed_by = ?2
               AND claim_token = ?3
               AND claim_generation = ?4
               AND julianday(lease_expires_at) > julianday(?7)",
            params![
                claim.operation_id(),
                claim.owner(),
                claim.token().as_str(),
                claim.generation(),
                error_code,
                message,
                now,
            ],
        )?;
        Ok(claim_transition(changed))
    }

    pub fn authorize_sync_operation_boundary(
        &self,
        claim: &SyncClaimHandle,
    ) -> Result<SyncClaimCheck, MetadataError> {
        self.renew_sync_operation_boundary(claim, false)
    }

    pub fn renew_sync_operation_reconciliation_boundary(
        &self,
        claim: &SyncClaimHandle,
    ) -> Result<SyncClaimCheck, MetadataError> {
        self.renew_sync_operation_boundary(claim, true)
    }

    fn renew_sync_operation_boundary(
        &self,
        claim: &SyncClaimHandle,
        allow_cancellation_requested: bool,
    ) -> Result<SyncClaimCheck, MetadataError> {
        self.in_immediate_transaction(|| {
            let changed = self.connection.execute(
                "UPDATE sync_operations
                 SET heartbeat_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                     lease_expires_at = strftime(
                       '%Y-%m-%dT%H:%M:%fZ',
                       'now',
                       '+' || ?5 || ' seconds'
                     ),
                     updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                 WHERE id = ?1
                   AND state = 'claimed'
                   AND claimed_by = ?2
                   AND claim_token = ?3
                   AND claim_generation = ?4
                   AND (?6 = 1 OR cancellation_requested_at IS NULL)
                   AND julianday(lease_expires_at) > julianday('now')",
                params![
                    claim.operation_id(),
                    claim.owner(),
                    claim.token().as_str(),
                    claim.generation(),
                    SYNC_BOUNDARY_SAFETY_LEASE_SECONDS,
                    i64::from(allow_cancellation_requested),
                ],
            )?;
            if changed == 1 {
                return self.check_sync_operation_claim(claim);
            }
            self.check_sync_operation_claim(claim)
        })
    }

    pub fn check_sync_operation_claim(
        &self,
        claim: &SyncClaimHandle,
    ) -> Result<SyncClaimCheck, MetadataError> {
        let cancellation_requested = self
            .connection
            .query_row(
                "SELECT cancellation_requested_at IS NOT NULL
                 FROM sync_operations
                 WHERE id = ?1
                   AND state = 'claimed'
                   AND claimed_by = ?2
                   AND claim_token = ?3
                   AND claim_generation = ?4
                   AND julianday(lease_expires_at) > julianday('now')",
                params![
                    claim.operation_id(),
                    claim.owner(),
                    claim.token().as_str(),
                    claim.generation(),
                ],
                |row| row.get::<_, bool>(0),
            )
            .optional()?;
        Ok(match cancellation_requested {
            Some(true) => SyncClaimCheck::CancellationRequested,
            Some(false) => SyncClaimCheck::Owned,
            None => SyncClaimCheck::OwnershipLost,
        })
    }

    pub fn request_sync_operation_cancellation(
        &self,
        operation_id: &str,
        now: &str,
    ) -> Result<Option<SyncCancellationOutcome>, MetadataError> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let state = transaction
            .query_row(
                "SELECT state FROM sync_operations WHERE id = ?1",
                [operation_id],
                |row| deserialize_json_variant::<SyncOperationState>(row.get::<_, String>(0)?),
            )
            .optional()?;
        let Some(state) = state else {
            transaction.commit()?;
            return Ok(None);
        };
        let outcome = match state {
            SyncOperationState::Queued
            | SyncOperationState::WaitingRetry
            | SyncOperationState::BlockedOffline
            | SyncOperationState::Attention => {
                let result = serde_json::json!({"outcome": "cancelled"}).to_string();
                transaction.execute(
                    "UPDATE sync_operations
                     SET state = 'cancelled',
                         cancellation_requested_at = COALESCE(cancellation_requested_at, ?2),
                         claimed_by = NULL,
                         claim_token = NULL,
                         heartbeat_at = NULL,
                         lease_expires_at = NULL,
                         next_attempt_at = NULL,
                         result_json = ?3,
                         last_error_code = NULL,
                         last_error = NULL,
                         updated_at = ?2
                     WHERE id = ?1 AND state IN ('queued', 'waiting_retry', 'blocked_offline', 'attention')",
                    params![operation_id, now, result],
                )?;
                SyncCancellationOutcome::Cancelled
            }
            SyncOperationState::Claimed => {
                transaction.execute(
                    "UPDATE sync_operations
                     SET cancellation_requested_at = COALESCE(cancellation_requested_at, ?2),
                         updated_at = ?2
                     WHERE id = ?1 AND state = 'claimed'",
                    params![operation_id, now],
                )?;
                SyncCancellationOutcome::Requested
            }
            SyncOperationState::ReconciliationRequired => {
                transaction.execute(
                    "UPDATE sync_operations
                     SET cancellation_requested_at = COALESCE(cancellation_requested_at, ?2),
                         updated_at = ?2
                     WHERE id = ?1 AND state = 'reconciliation_required'",
                    params![operation_id, now],
                )?;
                SyncCancellationOutcome::Requested
            }
            SyncOperationState::Completed => SyncCancellationOutcome::AlreadyCompleted,
            SyncOperationState::Cancelled => SyncCancellationOutcome::AlreadyCancelled,
        };
        transaction.commit()?;
        Ok(Some(outcome))
    }
}

#[derive(Clone, Copy)]
enum SyncOperationClaimFilter {
    Any,
    ControlPlane,
    Reconcile,
}

impl SyncOperationClaimFilter {
    const fn as_i64(self) -> i64 {
        match self {
            Self::Any => 0,
            Self::ControlPlane => 1,
            Self::Reconcile => 2,
        }
    }
}
