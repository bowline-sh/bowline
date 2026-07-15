use super::common::*;
use super::*;

mod claims;
mod observations;
mod retention;

pub(crate) use retention::retention_cutoff;
pub use retention::{LocalMetadataPruneReport, LocalMetadataRetentionPolicy};

// Domain calls may outlive a regular heartbeat interval, so authorization
// reserves the same full safety window used by the daemon claim supervisor.
const SYNC_BOUNDARY_SAFETY_LEASE_SECONDS: i64 = 60;

fn same_sync_operation_input(left: &SyncOperationRecord, right: &SyncOperationRecord) -> bool {
    left.id == right.id
        && left.workspace_id == right.workspace_id
        && left.kind == right.kind
        && left.resource_key == right.resource_key
        && left.idempotency_key == right.idempotency_key
        && left.base_version == right.base_version
        && left.base_snapshot_id == right.base_snapshot_id
        && left.target_snapshot_id == right.target_snapshot_id
        && left.device_id == right.device_id
        && left.payload_json == right.payload_json
}

impl MetadataStore {
    pub fn enqueue_sync_operation(
        &self,
        record: &SyncOperationRecord,
    ) -> Result<SyncOperationEnqueueOutcome, MetadataError> {
        self.insert_workspace(&record.workspace_id, "Code", &record.updated_at)?;
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "INSERT INTO sync_operations
             (id, workspace_id, kind, resource_key, state, idempotency_key, base_version, base_snapshot_id,
              target_snapshot_id, device_id, payload_json, attempt_count, claimed_by,
              claim_generation, heartbeat_at, lease_expires_at, cancellation_requested_at,
              next_attempt_at, result_json, last_error_code, last_error, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23)
             ON CONFLICT(workspace_id, idempotency_key) DO NOTHING",
            params![
                record.id.as_str(),
                record.workspace_id.as_str(),
                serialize_json_variant(&record.kind)?,
                record.resource_key.as_string(),
                serialize_json_variant(&record.state)?,
                record.idempotency_key.as_str(),
                record.base_version,
                record.base_snapshot_id.as_deref(),
                record.target_snapshot_id.as_deref(),
                record.device_id.as_ref().map(|id| id.as_str()),
                record.payload_json.as_str(),
                record.attempt_count,
                record.claimed_by.as_deref(),
                record.claim_generation,
                record.heartbeat_at.as_deref(),
                record.lease_expires_at.as_deref(),
                record.cancellation_requested_at.as_deref(),
                record.next_attempt_at.as_deref(),
                record.result_json.as_deref(),
                record.last_error_code.as_deref(),
                record.last_error.as_deref(),
                record.created_at.as_str(),
                record.updated_at.as_str(),
            ],
        )?;
        let outcome = if changed == 0 {
            let existing = transaction.query_row(
                "SELECT id, workspace_id, kind, resource_key, state, idempotency_key, base_version,
                        base_snapshot_id, target_snapshot_id, device_id, payload_json, attempt_count,
                        claimed_by, claim_generation, heartbeat_at, lease_expires_at,
                        cancellation_requested_at, next_attempt_at, result_json, last_error_code,
                        last_error, created_at, updated_at
                 FROM sync_operations
                 WHERE workspace_id = ?1 AND idempotency_key = ?2",
                params![record.workspace_id.as_str(), record.idempotency_key.as_str()],
                sync_operation_from_row,
            )?;
            if !same_sync_operation_input(&existing, record) {
                return Err(MetadataError::InvalidStorageMetadata(format!(
                    "sync operation idempotency key `{}` was reused with different input",
                    record.idempotency_key
                )));
            }
            SyncOperationEnqueueOutcome::Existing(existing)
        } else {
            SyncOperationEnqueueOutcome::Inserted(record.clone())
        };
        transaction.commit()?;
        Ok(outcome)
    }

    pub fn sync_operations(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Vec<SyncOperationRecord>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT id, workspace_id, kind, resource_key, state, idempotency_key, base_version, base_snapshot_id,
                    target_snapshot_id, device_id, payload_json, attempt_count, claimed_by,
                    claim_generation, heartbeat_at, lease_expires_at, cancellation_requested_at,
                    next_attempt_at, result_json, last_error_code, last_error, created_at, updated_at
             FROM sync_operations
             WHERE workspace_id = ?1
             ORDER BY created_at, id",
        )?;
        let rows = statement.query_map([workspace_id.as_str()], sync_operation_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn completed_sync_operations(
        &self,
        workspace_id: &WorkspaceId,
        since: Option<&str>,
        until: Option<&str>,
        limit: Option<u64>,
    ) -> Result<Vec<SyncOperationRecord>, MetadataError> {
        self.completed_sync_operations_page(workspace_id, since, until, None, None, limit)
    }

    pub fn completed_sync_operations_page(
        &self,
        workspace_id: &WorkspaceId,
        since: Option<&str>,
        until: Option<&str>,
        before_updated_at: Option<&str>,
        before_id: Option<&str>,
        limit: Option<u64>,
    ) -> Result<Vec<SyncOperationRecord>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT id, workspace_id, kind, resource_key, state, idempotency_key, base_version, base_snapshot_id,
                    target_snapshot_id, device_id, payload_json, attempt_count, claimed_by,
                    claim_generation, heartbeat_at, lease_expires_at, cancellation_requested_at,
                    next_attempt_at, result_json, last_error_code, last_error, created_at, updated_at
             FROM sync_operations
             WHERE workspace_id = ?1
               AND state = 'completed'
               AND (?2 IS NULL OR updated_at >= ?2)
               AND (?3 IS NULL OR updated_at <= ?3)
               AND (
                   ?4 IS NULL
                   OR updated_at < ?4
                   OR (updated_at = ?4 AND id < ?5)
               )
             ORDER BY updated_at DESC, id DESC
             LIMIT ?6",
        )?;
        let rows = statement.query_map(
            params![
                workspace_id.as_str(),
                since,
                until,
                before_updated_at,
                before_id,
                sql_limit(limit),
            ],
            sync_operation_from_row,
        )?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn completed_sync_operation_for_snapshot(
        &self,
        workspace_id: &WorkspaceId,
        snapshot_id: &SnapshotId,
    ) -> Result<Option<SyncOperationRecord>, MetadataError> {
        self.connection
            .query_row(
                "SELECT id, workspace_id, kind, resource_key, state, idempotency_key, base_version, base_snapshot_id,
                        target_snapshot_id, device_id, payload_json, attempt_count, claimed_by,
                        claim_generation, heartbeat_at, lease_expires_at, cancellation_requested_at,
                        next_attempt_at, result_json, last_error_code, last_error, created_at, updated_at
                 FROM sync_operations
                 WHERE workspace_id = ?1
                   AND state = 'completed'
                   AND target_snapshot_id = ?2
                 ORDER BY updated_at DESC, id DESC
                 LIMIT 1",
                params![workspace_id.as_str(), snapshot_id.as_str()],
                sync_operation_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn latest_completed_sync_operation_for_device_kind(
        &self,
        workspace_id: &WorkspaceId,
        kind: SyncOperationKind,
        device_id: &DeviceId,
    ) -> Result<Option<SyncOperationRecord>, MetadataError> {
        let kind = serialize_json_variant(&kind)?;
        self.connection
            .query_row(
                "SELECT id, workspace_id, kind, resource_key, state, idempotency_key, base_version, base_snapshot_id,
                        target_snapshot_id, device_id, payload_json, attempt_count, claimed_by,
                        claim_generation, heartbeat_at, lease_expires_at, cancellation_requested_at,
                        next_attempt_at, result_json, last_error_code, last_error, created_at, updated_at
                 FROM sync_operations
                 WHERE workspace_id = ?1
                   AND kind = ?2
                   AND device_id = ?3
                   AND state = 'completed'
                 ORDER BY updated_at DESC, id DESC
                 LIMIT 1",
                params![workspace_id.as_str(), kind, device_id.as_str()],
                sync_operation_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn active_sync_operation_for_device(
        &self,
        workspace_id: &WorkspaceId,
        kind: SyncOperationKind,
        device_id: &DeviceId,
    ) -> Result<Option<SyncOperationRecord>, MetadataError> {
        let kind = serialize_json_variant(&kind)?;
        self.connection
            .query_row(
                "SELECT id, workspace_id, kind, resource_key, state, idempotency_key, base_version, base_snapshot_id,
                        target_snapshot_id, device_id, payload_json, attempt_count, claimed_by,
                        claim_generation, heartbeat_at, lease_expires_at, cancellation_requested_at,
                        next_attempt_at, result_json, last_error_code, last_error, created_at, updated_at
                 FROM sync_operations
                 WHERE workspace_id = ?1
                   AND kind = ?2
                   AND device_id = ?3
                   AND state NOT IN ('completed', 'cancelled')
                 ORDER BY created_at, id
                 LIMIT 1",
                params![workspace_id.as_str(), kind, device_id.as_str()],
                sync_operation_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn complete_claimed_sync_operation(
        &self,
        claim: &SyncClaimHandle,
        result_json: &str,
        now: &str,
    ) -> Result<SyncClaimTransition, MetadataError> {
        self.transition_claimed_sync_operation(ClaimedTransitionRequest {
            claim,
            state: "completed",
            result_json: Some(result_json),
            error_code: None,
            message: None,
            next_attempt_at: None,
            allow_cancellation_requested: false,
            require_cancellation_requested: false,
            now,
        })
    }

    pub fn complete_committed_cancelled_late_sync_operation(
        &self,
        claim: &SyncClaimHandle,
        result: &SyncCommittedCancelledLateResult,
        now: &str,
    ) -> Result<SyncClaimTransition, MetadataError> {
        let result_json = serde_json::to_string(result).map_err(|error| {
            MetadataError::InvalidStorageMetadata(format!(
                "committed-cancelled-late result could not be serialized: {error}"
            ))
        })?;
        self.transition_claimed_sync_operation(ClaimedTransitionRequest {
            claim,
            state: "completed",
            result_json: Some(&result_json),
            error_code: None,
            message: None,
            next_attempt_at: None,
            allow_cancellation_requested: true,
            require_cancellation_requested: true,
            now,
        })
    }

    pub fn fail_claimed_sync_operation_for_retry(
        &self,
        claim: &SyncClaimHandle,
        error_code: &str,
        message: &str,
        next_attempt_at: &str,
        now: &str,
    ) -> Result<SyncClaimTransition, MetadataError> {
        self.transition_claimed_sync_operation(ClaimedTransitionRequest {
            claim,
            state: "waiting_retry",
            result_json: None,
            error_code: Some(error_code),
            message: Some(message),
            next_attempt_at: Some(next_attempt_at),
            allow_cancellation_requested: false,
            require_cancellation_requested: false,
            now,
        })
    }

    pub fn block_claimed_sync_operation_offline(
        &self,
        claim: &SyncClaimHandle,
        error_code: &str,
        message: &str,
        next_attempt_at: &str,
        now: &str,
    ) -> Result<SyncClaimTransition, MetadataError> {
        self.transition_claimed_sync_operation(ClaimedTransitionRequest {
            claim,
            state: "blocked_offline",
            result_json: None,
            error_code: Some(error_code),
            message: Some(message),
            next_attempt_at: Some(next_attempt_at),
            allow_cancellation_requested: false,
            require_cancellation_requested: false,
            now,
        })
    }

    pub fn mark_claimed_sync_operation_attention(
        &self,
        claim: &SyncClaimHandle,
        error_code: &str,
        message: &str,
        now: &str,
    ) -> Result<SyncClaimTransition, MetadataError> {
        self.transition_claimed_sync_operation(ClaimedTransitionRequest {
            claim,
            state: "attention",
            result_json: None,
            error_code: Some(error_code),
            message: Some(message),
            next_attempt_at: None,
            allow_cancellation_requested: false,
            require_cancellation_requested: false,
            now,
        })
    }

    pub fn cancel_claimed_sync_operation(
        &self,
        claim: &SyncClaimHandle,
        result_json: &str,
        now: &str,
    ) -> Result<SyncClaimTransition, MetadataError> {
        self.transition_claimed_sync_operation(ClaimedTransitionRequest {
            claim,
            state: "cancelled",
            result_json: Some(result_json),
            error_code: None,
            message: None,
            next_attempt_at: None,
            allow_cancellation_requested: true,
            require_cancellation_requested: true,
            now,
        })
    }

    pub fn defer_claimed_sync_operation_reconciliation(
        &self,
        claim: &SyncClaimHandle,
        message: &str,
        now: &str,
    ) -> Result<SyncClaimTransition, MetadataError> {
        let changed = self.connection.execute(
            "UPDATE sync_operations
             SET state = 'reconciliation_required',
                 claimed_by = NULL,
                 claim_token = NULL,
                 heartbeat_at = NULL,
                 lease_expires_at = NULL,
                 next_attempt_at = NULL,
                 result_json = json_object('outcome', 'reconciliation-required'),
                 last_error_code = NULL,
                 last_error = ?6,
                 updated_at = ?5
             WHERE id = ?1
               AND state = 'claimed'
               AND claimed_by = ?2
               AND claim_token = ?3
               AND claim_generation = ?4
               AND cancellation_requested_at IS NOT NULL",
            params![
                claim.operation_id(),
                claim.owner(),
                claim.token().as_str(),
                claim.generation(),
                now,
                message,
            ],
        )?;
        Ok(claim_transition(changed))
    }

    fn transition_claimed_sync_operation(
        &self,
        request: ClaimedTransitionRequest<'_>,
    ) -> Result<SyncClaimTransition, MetadataError> {
        let changed = self.connection.execute(
            "UPDATE sync_operations
             SET state = ?5,
                 result_json = ?6,
                 last_error_code = ?7,
                 last_error = ?8,
                 next_attempt_at = ?9,
                 claimed_by = NULL,
                 claim_token = NULL,
                 heartbeat_at = NULL,
                 lease_expires_at = NULL,
                 updated_at = ?10
             WHERE id = ?1
               AND state = 'claimed'
               AND claimed_by = ?2
               AND claim_token = ?3
               AND claim_generation = ?4
               AND julianday(lease_expires_at) > julianday('now')
               AND (?11 = 1 OR cancellation_requested_at IS NULL)
               AND (?12 = 0 OR cancellation_requested_at IS NOT NULL)",
            params![
                request.claim.operation_id(),
                request.claim.owner(),
                request.claim.token().as_str(),
                request.claim.generation(),
                request.state,
                request.result_json,
                request.error_code,
                request.message,
                request.next_attempt_at,
                request.now,
                i64::from(request.allow_cancellation_requested),
                i64::from(request.require_cancellation_requested),
            ],
        )?;
        Ok(claim_transition(changed))
    }

    pub fn append_claimed_sync_operation_checkpoint(
        &self,
        claim: &SyncClaimHandle,
        record: &SyncOperationCheckpointRecord,
    ) -> Result<SyncClaimTransition, MetadataError> {
        if record.operation_id != claim.operation_id() {
            return Ok(SyncClaimTransition::OwnershipLost);
        }
        let changed = self.connection.execute(
            "INSERT INTO sync_operation_checkpoints
             (id, workspace_id, operation_id, step, state, payload_json, created_at, updated_at)
             SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8
             WHERE EXISTS (
               SELECT 1 FROM sync_operations
               WHERE id = ?3
                 AND state = 'claimed'
                 AND claimed_by = ?9
                 AND claim_token = ?10
                 AND claim_generation = ?11
                 AND julianday(lease_expires_at) > julianday('now')
             )
             ON CONFLICT(id) DO UPDATE SET
               state = excluded.state,
               payload_json = excluded.payload_json,
               updated_at = excluded.updated_at
             WHERE sync_operation_checkpoints.operation_id = excluded.operation_id
               AND sync_operation_checkpoints.workspace_id = excluded.workspace_id",
            params![
                record.id.as_str(),
                record.workspace_id.as_str(),
                record.operation_id.as_str(),
                record.step.as_str(),
                record.state.as_str(),
                record.payload_json.as_str(),
                record.created_at.as_str(),
                record.updated_at.as_str(),
                claim.owner(),
                claim.token().as_str(),
                claim.generation(),
            ],
        )?;
        Ok(claim_transition(changed))
    }

    pub fn sync_operation_checkpoints(
        &self,
        operation_id: &str,
    ) -> Result<Vec<SyncOperationCheckpointRecord>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT id, workspace_id, operation_id, step, state, payload_json, created_at, updated_at
             FROM sync_operation_checkpoints
             WHERE operation_id = ?1
             ORDER BY created_at, id",
        )?;
        let rows = statement.query_map([operation_id], sync_operation_checkpoint_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn requeue_expired_sync_claims(
        &self,
        workspace_id: &WorkspaceId,
        now: &str,
    ) -> Result<u64, MetadataError> {
        let reconciliation_required =
            serde_json::json!({"outcome": "reconciliation-required"}).to_string();
        let changed = self.connection.execute(
            "UPDATE sync_operations
             SET state = CASE
                   WHEN cancellation_requested_at IS NULL THEN 'queued'
                   ELSE 'reconciliation_required'
                 END,
                 claimed_by = NULL,
                 claim_token = NULL,
                 heartbeat_at = NULL,
                 lease_expires_at = NULL,
                 next_attempt_at = CASE WHEN cancellation_requested_at IS NULL THEN next_attempt_at ELSE ?2 END,
                 result_json = CASE WHEN cancellation_requested_at IS NULL THEN result_json ELSE ?3 END,
                 last_error_code = CASE WHEN cancellation_requested_at IS NULL THEN last_error_code END,
                 last_error = CASE
                   WHEN cancellation_requested_at IS NULL THEN last_error
                   ELSE 'claim expired after cancellation; reconcile remote commit state before completion'
                 END,
                 updated_at = ?2
             WHERE workspace_id = ?1
               AND state = 'claimed'
               AND lease_expires_at IS NOT NULL
               AND julianday(lease_expires_at) <= julianday('now')",
            params![workspace_id.as_str(), now, reconciliation_required],
        )?;
        Ok(changed as u64)
    }

    pub fn requeue_attention_sync_operations_for_device_kind_with_error(
        &self,
        workspace_id: &WorkspaceId,
        kind: SyncOperationKind,
        device_id: &DeviceId,
        error_substring: &str,
        now: &str,
    ) -> Result<u64, MetadataError> {
        let kind = serialize_json_variant(&kind)?;
        let changed = self.connection.execute(
            "UPDATE sync_operations
             SET state = 'queued',
                 claimed_by = NULL,
                 heartbeat_at = NULL,
                 next_attempt_at = NULL,
                 last_error = NULL,
                 updated_at = ?5
             WHERE workspace_id = ?1
               AND kind = ?2
               AND device_id = ?3
               AND state = 'attention'
               AND last_error LIKE ?4 ESCAPE '\\'",
            params![
                workspace_id.as_str(),
                kind,
                device_id.as_str(),
                format!("%{}%", escape_like(error_substring)),
                now,
            ],
        )?;
        Ok(changed as u64)
    }
}

struct ClaimedTransitionRequest<'a> {
    claim: &'a SyncClaimHandle,
    state: &'a str,
    result_json: Option<&'a str>,
    error_code: Option<&'a str>,
    message: Option<&'a str>,
    next_attempt_at: Option<&'a str>,
    allow_cancellation_requested: bool,
    require_cancellation_requested: bool,
    now: &'a str,
}

fn claim_transition(changed: usize) -> SyncClaimTransition {
    if changed == 0 {
        SyncClaimTransition::OwnershipLost
    } else {
        SyncClaimTransition::Applied
    }
}
