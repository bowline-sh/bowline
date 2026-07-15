use super::*;

impl MetadataStore {
    pub fn claim_next_work_view_accept(
        &self,
        workspace_id: &WorkspaceId,
        device_id: &DeviceId,
        claimant: &str,
        now: &str,
        lease_expires_at: &str,
    ) -> Result<Option<ClaimedWorkViewAcceptOperation>, MetadataError> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let id = transaction
            .query_row(
                "SELECT id FROM work_view_accept_operations
                 WHERE workspace_id = ?1 AND device_id = ?2 AND (
                    state = 'queued'
                    OR (state = 'waiting-retry'
                        AND (next_attempt_at IS NULL OR julianday(next_attempt_at) <= julianday(?3)))
                 )
                   AND NOT EXISTS (
                     SELECT 1 FROM sync_operations claimed_sync
                     WHERE claimed_sync.workspace_id = work_view_accept_operations.workspace_id
                       AND claimed_sync.kind = 'daemon-reconcile'
                       AND claimed_sync.resource_key = ('workspace_sync:' || claimed_sync.workspace_id)
                       AND claimed_sync.state = 'claimed'
                   )
                 ORDER BY created_at, id LIMIT 1",
                params![workspace_id.as_str(), device_id.as_str(), now],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let Some(id) = id else {
            transaction.commit()?;
            return Ok(None);
        };
        let token = random_hex_token("work-view accept claim")?;
        let changed = transaction.execute(
            "UPDATE work_view_accept_operations
             SET state = 'claimed', claimed_by = ?2, claim_token = ?3,
                 claim_generation = claim_generation + 1, heartbeat_at = ?4,
                 lease_expires_at = ?5, attempt_count = attempt_count + 1,
                 next_attempt_at = NULL, failure_reason = NULL, last_error = NULL, updated_at = ?4
             WHERE id = ?1 AND workspace_id = ?6 AND device_id = ?7 AND (
                 state = 'queued' OR (state = 'waiting-retry'
                 AND (next_attempt_at IS NULL OR julianday(next_attempt_at) <= julianday(?4))))",
            params![
                id,
                claimant,
                token,
                now,
                lease_expires_at,
                workspace_id.as_str(),
                device_id.as_str(),
            ],
        )?;
        if changed != 1 {
            transaction.commit()?;
            return Ok(None);
        }
        let operation = query_operation(&transaction, "id = ?1", [&id])?.ok_or_else(|| {
            MetadataError::InvalidStorageMetadata("claimed accept vanished".into())
        })?;
        let claim = WorkViewAcceptClaimHandle {
            operation_id: id,
            owner: claimant.to_string(),
            token,
            generation: operation.claim_generation,
        };
        transaction.commit()?;
        Ok(Some(ClaimedWorkViewAcceptOperation { operation, claim }))
    }

    pub fn renew_work_view_accept_claim(
        &self,
        claim: &WorkViewAcceptClaimHandle,
        now: &str,
        lease_expires_at: &str,
    ) -> Result<WorkViewAcceptClaimTransition, MetadataError> {
        let changed = self.connection.execute(
            "UPDATE work_view_accept_operations
             SET heartbeat_at = ?5, lease_expires_at = ?6, updated_at = ?5
             WHERE id = ?1 AND state = 'claimed' AND claimed_by = ?2 AND claim_token = ?3
               AND claim_generation = ?4 AND julianday(lease_expires_at) > julianday(?5)",
            params![
                claim.operation_id(),
                claim.owner(),
                claim.token(),
                claim.generation(),
                now,
                lease_expires_at
            ],
        )?;
        Ok(transition(changed))
    }

    pub fn requeue_claimed_work_view_accept_after_dispatch_failure(
        &self,
        claim: &WorkViewAcceptClaimHandle,
        message: &str,
        now: &str,
    ) -> Result<WorkViewAcceptClaimTransition, MetadataError> {
        let changed = self.connection.execute(
            "UPDATE work_view_accept_operations
             SET state = 'queued', claimed_by = NULL, claim_token = NULL,
                 heartbeat_at = NULL, lease_expires_at = NULL, next_attempt_at = NULL,
                 failure_reason = NULL, last_error = ?5, updated_at = ?6
             WHERE id = ?1 AND state = 'claimed' AND claimed_by = ?2 AND claim_token = ?3
               AND claim_generation = ?4 AND julianday(lease_expires_at) > julianday(?6)",
            params![
                claim.operation_id(),
                claim.owner(),
                claim.token(),
                claim.generation(),
                message,
                now,
            ],
        )?;
        Ok(transition(changed))
    }

    pub fn record_claimed_work_view_accept_worker_failure(
        &self,
        claim: &WorkViewAcceptClaimHandle,
        message: &str,
        now: &str,
    ) -> Result<WorkViewAcceptClaimTransition, MetadataError> {
        let changed = self.connection.execute(
            "UPDATE work_view_accept_operations
             SET last_error = ?5, updated_at = ?6
             WHERE id = ?1 AND state = 'claimed' AND claimed_by = ?2 AND claim_token = ?3
               AND claim_generation = ?4 AND julianday(lease_expires_at) > julianday(?6)",
            params![
                claim.operation_id(),
                claim.owner(),
                claim.token(),
                claim.generation(),
                message,
                now,
            ],
        )?;
        Ok(transition(changed))
    }

    pub fn check_work_view_accept_claim(
        &self,
        claim: &WorkViewAcceptClaimHandle,
        now: &str,
    ) -> Result<WorkViewAcceptClaimCheck, MetadataError> {
        let cancellation_requested = self
            .connection
            .query_row(
                "SELECT cancellation_requested_at IS NOT NULL FROM work_view_accept_operations
             WHERE id = ?1 AND state = 'claimed' AND claimed_by = ?2 AND claim_token = ?3
               AND claim_generation = ?4 AND julianday(lease_expires_at) > julianday(?5)",
                params![
                    claim.operation_id(),
                    claim.owner(),
                    claim.token(),
                    claim.generation(),
                    now
                ],
                |row| row.get::<_, bool>(0),
            )
            .optional()?;
        Ok(match cancellation_requested {
            Some(true) => WorkViewAcceptClaimCheck::CancellationRequested,
            Some(false) => WorkViewAcceptClaimCheck::Owned,
            None => WorkViewAcceptClaimCheck::OwnershipLost,
        })
    }

    pub fn request_work_view_accept_cancellation(
        &self,
        operation_id: &str,
        now: &str,
    ) -> Result<Option<WorkViewAcceptCancellationOutcome>, MetadataError> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let state = transaction
            .query_row(
                "SELECT state FROM work_view_accept_operations WHERE id = ?1",
                [operation_id],
                |row| {
                    deserialize_json_variant::<WorkViewAcceptOperationState>(
                        row.get::<_, String>(0)?,
                    )
                },
            )
            .optional()?;
        let Some(state) = state else {
            transaction.commit()?;
            return Ok(None);
        };
        let outcome = match state {
            WorkViewAcceptOperationState::Queued | WorkViewAcceptOperationState::WaitingRetry => {
                let result = serde_json::json!({"outcome": "cancelled"}).to_string();
                transaction.execute(
                    "UPDATE work_view_accept_operations
                     SET state = 'cancelled', cancellation_requested_at = COALESCE(cancellation_requested_at, ?2),
                         result_json = ?3, review_reason = NULL, failure_reason = NULL,
                         last_error = NULL, next_attempt_at = NULL, claimed_by = NULL,
                         claim_token = NULL, heartbeat_at = NULL, lease_expires_at = NULL,
                         updated_at = ?2
                     WHERE id = ?1 AND state IN ('queued', 'waiting-retry')",
                    params![operation_id, now, result],
                )?;
                WorkViewAcceptCancellationOutcome::Cancelled
            }
            WorkViewAcceptOperationState::Claimed => {
                transaction.execute(
                    "UPDATE work_view_accept_operations
                     SET cancellation_requested_at = COALESCE(cancellation_requested_at, ?2),
                         updated_at = ?2
                     WHERE id = ?1 AND state = 'claimed'",
                    params![operation_id, now],
                )?;
                WorkViewAcceptCancellationOutcome::Requested
            }
            WorkViewAcceptOperationState::ReviewRequired
            | WorkViewAcceptOperationState::Completed
            | WorkViewAcceptOperationState::Cancelled
            | WorkViewAcceptOperationState::Failed => {
                WorkViewAcceptCancellationOutcome::AlreadyTerminal
            }
        };
        transaction.commit()?;
        Ok(Some(outcome))
    }

    pub fn request_claimed_work_view_accept_cancellations(
        &self,
        workspace_id: &WorkspaceId,
        device_id: &DeviceId,
        claimant: &str,
        now: &str,
    ) -> Result<u64, MetadataError> {
        let changed = self.connection.execute(
            "UPDATE work_view_accept_operations
             SET cancellation_requested_at = COALESCE(cancellation_requested_at, ?4),
                 updated_at = ?4
             WHERE workspace_id = ?1 AND device_id = ?2 AND claimed_by = ?3
               AND state = 'claimed'",
            params![workspace_id.as_str(), device_id.as_str(), claimant, now],
        )?;
        u64::try_from(changed).map_err(|_| {
            MetadataError::InvalidStorageMetadata(
                "cancelled work-view accept count overflowed".into(),
            )
        })
    }

    pub fn cancel_claimed_work_view_accept(
        &self,
        claim: &WorkViewAcceptClaimHandle,
        result_json: &str,
        now: &str,
    ) -> Result<WorkViewAcceptClaimTransition, MetadataError> {
        let changed = self.connection.execute(
            "UPDATE work_view_accept_operations
             SET state = 'cancelled', result_json = ?5, review_reason = NULL,
                 failure_reason = NULL, last_error = NULL, next_attempt_at = NULL,
                 cancellation_requested_at = COALESCE(cancellation_requested_at, ?6),
                 claimed_by = NULL, claim_token = NULL, heartbeat_at = NULL,
                 lease_expires_at = NULL, updated_at = ?6
             WHERE id = ?1 AND state = 'claimed' AND claimed_by = ?2 AND claim_token = ?3
               AND claim_generation = ?4 AND cancellation_requested_at IS NOT NULL
               AND julianday(lease_expires_at) > julianday(?6)",
            params![
                claim.operation_id(),
                claim.owner(),
                claim.token(),
                claim.generation(),
                result_json,
                now,
            ],
        )?;
        Ok(transition(changed))
    }

    pub fn requeue_expired_work_view_accepts(&self, now: &str) -> Result<u64, MetadataError> {
        let changed = self.connection.execute(
            "UPDATE work_view_accept_operations
             SET state = 'queued', claimed_by = NULL, claim_token = NULL, heartbeat_at = NULL,
                 lease_expires_at = NULL, failure_reason = NULL, last_error = NULL,
                 next_attempt_at = NULL, updated_at = ?1
             WHERE state = 'claimed' AND julianday(lease_expires_at) <= julianday(?1)",
            [now],
        )?;
        u64::try_from(changed).map_err(|_| {
            MetadataError::InvalidStorageMetadata("expired accept count overflowed".into())
        })
    }

    pub fn next_work_view_accept_deadline(
        &self,
        workspace_id: &WorkspaceId,
        device_id: &DeviceId,
    ) -> Result<Option<String>, MetadataError> {
        self.connection
            .query_row(
                "SELECT MIN(deadline) FROM (
                   SELECT next_attempt_at AS deadline
                   FROM work_view_accept_operations
                   WHERE workspace_id = ?1 AND device_id = ?2
                     AND state = 'waiting-retry' AND next_attempt_at IS NOT NULL
                   UNION ALL
                   SELECT lease_expires_at AS deadline
                   FROM work_view_accept_operations
                   WHERE workspace_id = ?1 AND device_id = ?2
                     AND state = 'claimed' AND lease_expires_at IS NOT NULL
                 )",
                params![workspace_id.as_str(), device_id.as_str()],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }
}
