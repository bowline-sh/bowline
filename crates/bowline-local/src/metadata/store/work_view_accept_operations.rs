use super::common::*;
use super::*;

mod claims;

const OPERATION_COLUMNS: &str =
    "id, workspace_id, project_id, work_view_id, device_id, resource_key,
    idempotency_key, state, selected_paths_json, input_json, observed_main_snapshot_id,
    observed_ref_version, observed_ref_snapshot_id, target_snapshot_id, result_json,
    review_reason, failure_reason, cancellation_requested_at, last_error, claimed_by, claim_token,
    claim_generation, heartbeat_at, lease_expires_at, attempt_count, next_attempt_at, created_at,
    updated_at";

impl MetadataStore {
    pub fn enqueue_work_view_accept(
        &self,
        record: &WorkViewAcceptOperationRecord,
    ) -> Result<WorkViewAcceptEnqueueOutcome, MetadataError> {
        validate_enqueue_record(record)?;
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        if let Some(existing) =
            operation_by_idempotency(&transaction, &record.workspace_id, &record.idempotency_key)?
        {
            ensure_same_input(&existing, record)?;
            transaction.commit()?;
            return Ok(WorkViewAcceptEnqueueOutcome::Existing(existing));
        }
        let eligible = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM work_views
             WHERE workspace_id = ?1 AND project_id = ?2 AND id = ?3
               AND lifecycle IN ('active', 'review-ready'))",
            params![
                record.workspace_id.as_str(),
                record.project_id.as_str(),
                record.work_view_id.as_str(),
            ],
            |row| row.get::<_, bool>(0),
        )?;
        if !eligible {
            return Err(MetadataError::InvalidStorageMetadata(
                "work-view accept requires an active or review-ready work view".into(),
            ));
        }
        if let Some(active) =
            active_operation(&transaction, &record.workspace_id, &record.work_view_id)?
        {
            ensure_equivalent_active_input(&active, record)?;
            transaction.commit()?;
            return Ok(WorkViewAcceptEnqueueOutcome::Existing(active));
        }
        let selected_paths_json = serde_json::to_string(&record.selected_paths)
            .map_err(|error| MetadataError::Sqlite(json_to_sql_error(error)))?;
        transaction.execute(
            "INSERT INTO work_view_accept_operations
             (id, workspace_id, project_id, work_view_id, device_id, resource_key, idempotency_key, state,
              selected_paths_json, input_json, observed_main_snapshot_id, observed_ref_version,
              observed_ref_snapshot_id, target_snapshot_id, result_json, review_reason,
              failure_reason, cancellation_requested_at, last_error, claimed_by, claim_token,
              claim_generation, heartbeat_at, lease_expires_at, attempt_count, next_attempt_at,
              created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                     ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28)",
            params![
                record.id,
                record.workspace_id.as_str(),
                record.project_id.as_str(),
                record.work_view_id.as_str(),
                record.device_id.as_str(),
                record.resource_key.as_string(),
                record.idempotency_key,
                serialize_json_variant(&record.state)?,
                selected_paths_json,
                record.input_json,
                record
                    .observed_main_snapshot_id
                    .as_ref()
                    .map(SnapshotId::as_str),
                record.observed_ref_version,
                record
                    .observed_ref_snapshot_id
                    .as_ref()
                    .map(SnapshotId::as_str),
                record.target_snapshot_id.as_ref().map(SnapshotId::as_str),
                record.result_json,
                record
                    .review_reason
                    .map(|value| serialize_json_variant(&value))
                    .transpose()?,
                record
                    .failure_reason
                    .map(|value| serialize_json_variant(&value))
                    .transpose()?,
                record.cancellation_requested_at,
                record.last_error,
                record.claimed_by,
                record.claim_token,
                record.claim_generation,
                record.heartbeat_at,
                record.lease_expires_at,
                record.attempt_count,
                record.next_attempt_at,
                record.created_at,
                record.updated_at,
            ],
        )?;
        transaction.commit()?;
        Ok(WorkViewAcceptEnqueueOutcome::Inserted(record.clone()))
    }

    pub fn work_view_accept_operation(
        &self,
        operation_id: &str,
    ) -> Result<Option<WorkViewAcceptOperationRecord>, MetadataError> {
        query_operation(&self.connection, "id = ?1", [operation_id])
    }

    pub fn active_work_view_accept(
        &self,
        workspace_id: &WorkspaceId,
        work_view_id: &WorkViewId,
    ) -> Result<Option<WorkViewAcceptOperationRecord>, MetadataError> {
        active_operation(&self.connection, workspace_id, work_view_id)
    }

    pub fn upsert_work_view_unless_accept_active(
        &self,
        record: &WorkViewRecord,
    ) -> Result<Option<WorkViewAcceptOperationRecord>, MetadataError> {
        let project_path =
            self.workspace_relative_path(&record.workspace_id, &record.project_path)?;
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let active = active_operation(&transaction, &record.workspace_id, &record.id)?;
        if active.is_none() {
            super::work_views::upsert_work_view_record(&transaction, record, &project_path)?;
        }
        transaction.commit()?;
        Ok(active)
    }

    pub fn upsert_work_view_under_accept_claim(
        &self,
        record: &WorkViewRecord,
        claim: &WorkViewAcceptClaimHandle,
        now: &str,
    ) -> Result<WorkViewAcceptClaimTransition, MetadataError> {
        let project_path =
            self.workspace_relative_path(&record.workspace_id, &record.project_path)?;
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let owned = accept_claim_owns_work_view(&transaction, record, claim, now)?;
        if !owned {
            transaction.commit()?;
            return Ok(WorkViewAcceptClaimTransition::OwnershipLost);
        }
        super::work_views::upsert_work_view_record(&transaction, record, &project_path)?;
        transaction.commit()?;
        Ok(WorkViewAcceptClaimTransition::Applied)
    }

    pub fn insert_work_view_with_exposed_base_under_accept_claim(
        &self,
        record: &WorkViewRecord,
        descriptor: &WorkViewBaseDescriptor,
        claim: &WorkViewAcceptClaimHandle,
        now: &str,
    ) -> Result<WorkViewAcceptClaimTransition, MetadataError> {
        let snapshot = self
            .snapshot(&record.workspace_id, &descriptor.exposed_snapshot_id)?
            .ok_or_else(|| {
                MetadataError::InvalidStorageMetadata(
                    "work-view exposed snapshot root is missing".to_string(),
                )
            })?;
        super::work_views::validate_exposed_base(record, descriptor, &snapshot)?;
        if !self
            .snapshot_root_completeness(&record.workspace_id, &snapshot.id)?
            .complete
        {
            return Err(MetadataError::InvalidStorageMetadata(
                "work-view exposed snapshot graph is incomplete".to_string(),
            ));
        }
        let project_path =
            self.workspace_relative_path(&record.workspace_id, &record.project_path)?;
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        if !accept_claim_owns_work_view(&transaction, record, claim, now)? {
            transaction.commit()?;
            return Ok(WorkViewAcceptClaimTransition::OwnershipLost);
        }
        super::work_views::upsert_work_view_record(&transaction, record, &project_path)?;
        super::work_views::replace_exposed_base_records(&transaction, descriptor)?;
        transaction.commit()?;
        Ok(WorkViewAcceptClaimTransition::Applied)
    }

    pub fn work_view_accept_checkpoints(
        &self,
        operation_id: &str,
    ) -> Result<Vec<WorkViewAcceptCheckpointRecord>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT id, workspace_id, operation_id, claim_generation, step, payload_json, created_at
             FROM work_view_accept_checkpoints
             WHERE operation_id = ?1
             ORDER BY julianday(created_at), created_at, id",
        )?;
        let rows = statement.query_map([operation_id], checkpoint_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn append_work_view_accept_checkpoint(
        &self,
        claim: &WorkViewAcceptClaimHandle,
        checkpoint: &WorkViewAcceptCheckpointRecord,
        now: &str,
    ) -> Result<WorkViewAcceptClaimTransition, MetadataError> {
        if checkpoint.operation_id != claim.operation_id()
            || checkpoint.claim_generation != claim.generation()
        {
            return Err(MetadataError::InvalidStorageMetadata(
                "accept checkpoint does not match its claim fence".into(),
            ));
        }
        self.in_immediate_transaction(|| append_checkpoint_if_owned(self, claim, checkpoint, now))
    }

    pub fn record_work_view_accept_candidate(
        &self,
        claim: &WorkViewAcceptClaimHandle,
        checkpoint: &WorkViewAcceptCheckpointRecord,
        observation: &WorkViewAcceptCandidateObservation,
        now: &str,
    ) -> Result<WorkViewAcceptClaimTransition, MetadataError> {
        if checkpoint.step != WorkViewAcceptCheckpointStep::CandidateBuilt {
            return Err(MetadataError::InvalidStorageMetadata(
                "candidate observation requires a candidate-built checkpoint".into(),
            ));
        }
        self.in_immediate_transaction(|| {
            let inserted = append_checkpoint_if_owned(self, claim, checkpoint, now)?;
            if inserted == WorkViewAcceptClaimTransition::OwnershipLost {
                return Ok(inserted);
            }
            let changed = self.connection.execute(
                "UPDATE work_view_accept_operations
                 SET observed_main_snapshot_id = ?5, observed_ref_version = ?6,
                     observed_ref_snapshot_id = ?7, target_snapshot_id = ?8, updated_at = ?9
                 WHERE id = ?1 AND state = 'claimed' AND claimed_by = ?2 AND claim_token = ?3
                   AND claim_generation = ?4",
                params![
                    claim.operation_id(),
                    claim.owner(),
                    claim.token(),
                    claim.generation(),
                    observation.observed_main_snapshot_id.as_str(),
                    observation.observed_ref_version,
                    observation.observed_ref_snapshot_id.as_str(),
                    observation.target_snapshot_id.as_str(),
                    now,
                ],
            )?;
            Ok(transition(changed))
        })
    }

    pub fn mark_work_view_accept_uploaded_or_staged(
        &self,
        claim: &WorkViewAcceptClaimHandle,
        checkpoint: &WorkViewAcceptCheckpointRecord,
        target_snapshot_id: &SnapshotId,
        now: &str,
    ) -> Result<WorkViewAcceptClaimTransition, MetadataError> {
        if !matches!(
            checkpoint.step,
            WorkViewAcceptCheckpointStep::ObjectsUploaded
                | WorkViewAcceptCheckpointStep::SnapshotStaged
        ) {
            return Err(MetadataError::InvalidStorageMetadata(
                "uploaded-or-staged checkpoint used an invalid step".into(),
            ));
        }
        self.in_immediate_transaction(|| {
            let inserted = append_checkpoint_if_owned(self, claim, checkpoint, now)?;
            if inserted == WorkViewAcceptClaimTransition::OwnershipLost {
                return Ok(inserted);
            }
            let changed = self.connection.execute(
                "UPDATE work_view_accept_operations SET target_snapshot_id = ?5, updated_at = ?6
                 WHERE id = ?1 AND state = 'claimed' AND claimed_by = ?2 AND claim_token = ?3
                   AND claim_generation = ?4",
                params![
                    claim.operation_id(),
                    claim.owner(),
                    claim.token(),
                    claim.generation(),
                    target_snapshot_id.as_str(),
                    now
                ],
            )?;
            Ok(transition(changed))
        })
    }

    pub fn mark_work_view_accept_review(
        &self,
        claim: &WorkViewAcceptClaimHandle,
        reason: WorkViewAcceptReviewReason,
        result_json: &str,
        now: &str,
    ) -> Result<WorkViewAcceptClaimTransition, MetadataError> {
        transition_claim(
            self,
            claim,
            ClaimTransition {
                state: "review-required",
                result_json: Some(result_json),
                review_reason: Some(reason),
                failure_reason: None,
                message: None,
                next_attempt_at: None,
                now,
            },
        )
    }

    pub fn complete_work_view_accept(
        &self,
        claim: &WorkViewAcceptClaimHandle,
        target_snapshot_id: &SnapshotId,
        result_json: &str,
        now: &str,
    ) -> Result<WorkViewAcceptClaimTransition, MetadataError> {
        transition_claim_with_target(self, claim, target_snapshot_id, result_json, now)
    }

    pub fn fail_work_view_accept(
        &self,
        claim: &WorkViewAcceptClaimHandle,
        reason: WorkViewAcceptFailureReason,
        message: &str,
        now: &str,
    ) -> Result<WorkViewAcceptClaimTransition, MetadataError> {
        transition_claim(
            self,
            claim,
            ClaimTransition {
                state: "failed",
                result_json: None,
                review_reason: None,
                failure_reason: Some(reason),
                message: Some(message),
                next_attempt_at: None,
                now,
            },
        )
    }

    pub fn retry_work_view_accept(
        &self,
        claim: &WorkViewAcceptClaimHandle,
        message: &str,
        next_attempt_at: &str,
        now: &str,
    ) -> Result<WorkViewAcceptClaimTransition, MetadataError> {
        transition_claim(
            self,
            claim,
            ClaimTransition {
                state: "waiting-retry",
                result_json: None,
                review_reason: None,
                failure_reason: Some(WorkViewAcceptFailureReason::Transient),
                message: Some(message),
                next_attempt_at: Some(next_attempt_at),
                now,
            },
        )
    }
}

fn append_checkpoint_if_owned(
    store: &MetadataStore,
    claim: &WorkViewAcceptClaimHandle,
    checkpoint: &WorkViewAcceptCheckpointRecord,
    now: &str,
) -> Result<WorkViewAcceptClaimTransition, MetadataError> {
    let changed = store.connection.execute(
        "INSERT INTO work_view_accept_checkpoints
         (id, workspace_id, operation_id, claim_generation, step, payload_json, created_at)
         SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7
         FROM work_view_accept_operations
         WHERE id = ?3 AND workspace_id = ?2 AND state = 'claimed'
           AND claimed_by = ?8 AND claim_token = ?9 AND claim_generation = ?4
           AND julianday(lease_expires_at) > julianday(?10)
         ON CONFLICT(id) DO NOTHING",
        params![
            checkpoint.id,
            checkpoint.workspace_id.as_str(),
            checkpoint.operation_id,
            checkpoint.claim_generation,
            serialize_json_variant(&checkpoint.step)?,
            checkpoint.payload_json,
            checkpoint.created_at,
            claim.owner(),
            claim.token(),
            now,
        ],
    )?;
    if changed == 1 {
        return Ok(WorkViewAcceptClaimTransition::Applied);
    }
    if store.check_work_view_accept_claim(claim, now)? == WorkViewAcceptClaimCheck::OwnershipLost {
        return Ok(WorkViewAcceptClaimTransition::OwnershipLost);
    }
    let existing = store
        .connection
        .query_row(
            "SELECT workspace_id, operation_id, claim_generation, step, payload_json, created_at
             FROM work_view_accept_checkpoints WHERE id = ?1",
            [&checkpoint.id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, u64>(2)?,
                    deserialize_json_variant::<WorkViewAcceptCheckpointStep>(row.get(3)?)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ))
            },
        )
        .optional()?;
    let expected = (
        checkpoint.workspace_id.as_str().to_string(),
        checkpoint.operation_id.clone(),
        checkpoint.claim_generation,
        checkpoint.step,
        checkpoint.payload_json.clone(),
        checkpoint.created_at.clone(),
    );
    if existing == Some(expected) {
        Ok(WorkViewAcceptClaimTransition::Applied)
    } else {
        Err(MetadataError::InvalidStorageMetadata(
            "accept checkpoint id was reused with different input".into(),
        ))
    }
}

struct ClaimTransition<'a> {
    state: &'static str,
    result_json: Option<&'a str>,
    review_reason: Option<WorkViewAcceptReviewReason>,
    failure_reason: Option<WorkViewAcceptFailureReason>,
    message: Option<&'a str>,
    next_attempt_at: Option<&'a str>,
    now: &'a str,
}

fn transition_claim(
    store: &MetadataStore,
    claim: &WorkViewAcceptClaimHandle,
    transition_input: ClaimTransition<'_>,
) -> Result<WorkViewAcceptClaimTransition, MetadataError> {
    let review_reason = transition_input
        .review_reason
        .map(|value| serialize_json_variant(&value))
        .transpose()?;
    let failure_reason = transition_input
        .failure_reason
        .map(|value| serialize_json_variant(&value))
        .transpose()?;
    let changed = store.connection.execute(
        "UPDATE work_view_accept_operations
         SET state = ?5, result_json = ?6, review_reason = ?7, failure_reason = ?8,
             last_error = ?9, next_attempt_at = ?10, claimed_by = NULL, claim_token = NULL,
             heartbeat_at = NULL, lease_expires_at = NULL, updated_at = ?11
         WHERE id = ?1 AND state = 'claimed' AND claimed_by = ?2 AND claim_token = ?3
           AND claim_generation = ?4 AND julianday(lease_expires_at) > julianday(?11)",
        params![
            claim.operation_id(),
            claim.owner(),
            claim.token(),
            claim.generation(),
            transition_input.state,
            transition_input.result_json,
            review_reason,
            failure_reason,
            transition_input.message,
            transition_input.next_attempt_at,
            transition_input.now
        ],
    )?;
    Ok(transition(changed))
}

fn transition_claim_with_target(
    store: &MetadataStore,
    claim: &WorkViewAcceptClaimHandle,
    target: &SnapshotId,
    result_json: &str,
    now: &str,
) -> Result<WorkViewAcceptClaimTransition, MetadataError> {
    let changed = store.connection.execute(
        "UPDATE work_view_accept_operations
         SET state = 'completed', target_snapshot_id = ?5, result_json = ?6,
             review_reason = NULL, failure_reason = NULL, last_error = NULL,
             next_attempt_at = NULL, claimed_by = NULL, claim_token = NULL,
             heartbeat_at = NULL, lease_expires_at = NULL, updated_at = ?7
         WHERE id = ?1 AND state = 'claimed' AND claimed_by = ?2 AND claim_token = ?3
           AND claim_generation = ?4 AND julianday(lease_expires_at) > julianday(?7)",
        params![
            claim.operation_id(),
            claim.owner(),
            claim.token(),
            claim.generation(),
            target.as_str(),
            result_json,
            now
        ],
    )?;
    Ok(transition(changed))
}

fn validate_enqueue_record(record: &WorkViewAcceptOperationRecord) -> Result<(), MetadataError> {
    if record.state != WorkViewAcceptOperationState::Queued
        || record.claim_generation != 0
        || record.attempt_count != 0
        || record.claimed_by.is_some()
        || record.claim_token.is_some()
        || record.result_json.is_some()
        || record.review_reason.is_some()
        || record.failure_reason.is_some()
        || record.cancellation_requested_at.is_some()
    {
        return Err(MetadataError::InvalidStorageMetadata(
            "new accept operation is not pristine".into(),
        ));
    }
    if let Some(paths) = &record.selected_paths
        && paths.windows(2).any(|pair| pair[0] >= pair[1])
    {
        return Err(MetadataError::InvalidStorageMetadata(
            "selected accept paths must be sorted and unique".into(),
        ));
    }
    if !record.resource_key.matches(record) {
        return Err(MetadataError::InvalidStorageMetadata(
            "accept resource key does not match operation identity".into(),
        ));
    }
    serde_json::from_str::<serde_json::Value>(&record.input_json).map_err(|error| {
        MetadataError::InvalidStorageMetadata(format!("accept input is not valid JSON: {error}"))
    })?;
    Ok(())
}

fn ensure_same_input(
    left: &WorkViewAcceptOperationRecord,
    right: &WorkViewAcceptOperationRecord,
) -> Result<(), MetadataError> {
    if left.id != right.id
        || left.workspace_id != right.workspace_id
        || left.project_id != right.project_id
        || left.work_view_id != right.work_view_id
        || left.device_id != right.device_id
        || left.resource_key != right.resource_key
        || left.selected_paths != right.selected_paths
        || left.input_json != right.input_json
    {
        return Err(MetadataError::InvalidStorageMetadata(format!(
            "work-view accept idempotency key `{}` was reused with different input",
            right.idempotency_key
        )));
    }
    Ok(())
}

fn ensure_equivalent_active_input(
    active: &WorkViewAcceptOperationRecord,
    requested: &WorkViewAcceptOperationRecord,
) -> Result<(), MetadataError> {
    if active.workspace_id != requested.workspace_id
        || active.project_id != requested.project_id
        || active.work_view_id != requested.work_view_id
        || active.device_id != requested.device_id
        || active.resource_key != requested.resource_key
        || active.selected_paths != requested.selected_paths
        || active.input_json != requested.input_json
    {
        return Err(MetadataError::InvalidStorageMetadata(format!(
            "work-view accept `{}` is already active with different input",
            active.id
        )));
    }
    Ok(())
}

fn accept_claim_owns_work_view(
    connection: &Connection,
    record: &WorkViewRecord,
    claim: &WorkViewAcceptClaimHandle,
    now: &str,
) -> Result<bool, MetadataError> {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM work_view_accept_operations
             WHERE id = ?1 AND workspace_id = ?2 AND work_view_id = ?3
               AND state = 'claimed' AND claimed_by = ?4 AND claim_token = ?5
               AND claim_generation = ?6 AND julianday(lease_expires_at) > julianday(?7))",
            params![
                claim.operation_id(),
                record.workspace_id.as_str(),
                record.id.as_str(),
                claim.owner(),
                claim.token(),
                claim.generation(),
                now,
            ],
            |row| row.get::<_, bool>(0),
        )
        .map_err(Into::into)
}

fn active_operation(
    connection: &Connection,
    workspace_id: &WorkspaceId,
    work_view_id: &WorkViewId,
) -> Result<Option<WorkViewAcceptOperationRecord>, MetadataError> {
    query_operation(
        connection,
        "workspace_id = ?1 AND work_view_id = ?2 AND state IN ('queued', 'claimed', 'waiting-retry')",
        params![workspace_id.as_str(), work_view_id.as_str()],
    )
}

fn operation_by_idempotency(
    connection: &Connection,
    workspace_id: &WorkspaceId,
    key: &str,
) -> Result<Option<WorkViewAcceptOperationRecord>, MetadataError> {
    query_operation(
        connection,
        "workspace_id = ?1 AND idempotency_key = ?2",
        params![workspace_id.as_str(), key],
    )
}

fn query_operation<P: rusqlite::Params>(
    connection: &Connection,
    predicate: &str,
    params: P,
) -> Result<Option<WorkViewAcceptOperationRecord>, MetadataError> {
    connection
        .query_row(
            &format!(
                "SELECT {OPERATION_COLUMNS} FROM work_view_accept_operations WHERE {predicate}"
            ),
            params,
            operation_from_row,
        )
        .optional()
        .map_err(Into::into)
}

fn operation_from_row(
    row: &rusqlite::Row<'_>,
) -> Result<WorkViewAcceptOperationRecord, rusqlite::Error> {
    let workspace_id = WorkspaceId::new(row.get::<_, String>(1)?);
    let project_id = ProjectId::new(row.get::<_, String>(2)?);
    let work_view_id = WorkViewId::new(row.get::<_, String>(3)?);
    let resource_key = WorkViewAcceptResourceKey::new(
        workspace_id.clone(),
        project_id.clone(),
        work_view_id.clone(),
    );
    if row.get::<_, String>(5)? != resource_key.as_string() {
        return Err(rusqlite::Error::FromSqlConversionFailure(
            5,
            rusqlite::types::Type::Text,
            Box::new(io::Error::new(
                io::ErrorKind::InvalidData,
                "work-view accept resource key drifted from its identity",
            )),
        ));
    }
    Ok(WorkViewAcceptOperationRecord {
        id: row.get(0)?,
        workspace_id,
        project_id,
        work_view_id,
        device_id: DeviceId::new(row.get::<_, String>(4)?),
        resource_key,
        idempotency_key: row.get(6)?,
        state: deserialize_json_variant(row.get(7)?)?,
        selected_paths: serde_json::from_str(&row.get::<_, String>(8)?)
            .map_err(json_to_sql_read_error)?,
        input_json: row.get(9)?,
        observed_main_snapshot_id: row.get::<_, Option<String>>(10)?.map(SnapshotId::new),
        observed_ref_version: row.get(11)?,
        observed_ref_snapshot_id: row.get::<_, Option<String>>(12)?.map(SnapshotId::new),
        target_snapshot_id: row.get::<_, Option<String>>(13)?.map(SnapshotId::new),
        result_json: row.get(14)?,
        review_reason: row
            .get::<_, Option<String>>(15)?
            .map(deserialize_json_variant)
            .transpose()?,
        failure_reason: row
            .get::<_, Option<String>>(16)?
            .map(deserialize_json_variant)
            .transpose()?,
        cancellation_requested_at: row.get(17)?,
        last_error: row.get(18)?,
        claimed_by: row.get(19)?,
        claim_token: row.get(20)?,
        claim_generation: row.get(21)?,
        heartbeat_at: row.get(22)?,
        lease_expires_at: row.get(23)?,
        attempt_count: row.get(24)?,
        next_attempt_at: row.get(25)?,
        created_at: row.get(26)?,
        updated_at: row.get(27)?,
    })
}

fn checkpoint_from_row(
    row: &rusqlite::Row<'_>,
) -> Result<WorkViewAcceptCheckpointRecord, rusqlite::Error> {
    Ok(WorkViewAcceptCheckpointRecord {
        id: row.get(0)?,
        workspace_id: WorkspaceId::new(row.get::<_, String>(1)?),
        operation_id: row.get(2)?,
        claim_generation: row.get(3)?,
        step: deserialize_json_variant(row.get(4)?)?,
        payload_json: row.get(5)?,
        created_at: row.get(6)?,
    })
}

fn transition(changed: usize) -> WorkViewAcceptClaimTransition {
    if changed == 1 {
        WorkViewAcceptClaimTransition::Applied
    } else {
        WorkViewAcceptClaimTransition::OwnershipLost
    }
}
