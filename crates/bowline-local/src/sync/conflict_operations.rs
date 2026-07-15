use std::path::Path;

use bowline_control_plane::{
    ConflictOccurrenceReconcile, ConflictOccurrenceState, ConflictReconcileOutcome, ObjectPointer,
};
use bowline_core::ids::{ConflictId, DeviceId, SnapshotId, WorkspaceId};
use serde::{Deserialize, Serialize};

use crate::metadata::{
    SyncOperationKind, SyncOperationRecord, SyncOperationState, SyncResourceKey,
};

use super::conflicts::{
    ConflictBundleError, ConflictFile, ConflictKind, ConflictRecord, ConflictState,
    load_conflict_files, load_conflict_records,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConflictOccurrenceQueueResult {
    pub outcome: ConflictReconcileOutcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConflictReconcileStep {
    PublishOccurrence,
    ResolveAccepted,
    ResolveRejected,
}

impl ConflictReconcileStep {
    fn desired_state(self) -> ConflictOccurrenceState {
        match self {
            Self::PublishOccurrence => ConflictOccurrenceState::Unresolved,
            Self::ResolveAccepted => ConflictOccurrenceState::Accepted,
            Self::ResolveRejected => ConflictOccurrenceState::Rejected,
        }
    }
}

struct PreparedConflictOccurrence {
    record: ConflictRecord,
    bundle_object: ObjectPointer,
    step: ConflictReconcileStep,
}

pub fn pending_conflict_occurrence_operations(
    state_root: &Path,
    workspace_id: &WorkspaceId,
    device_id: &DeviceId,
    now: &str,
) -> Result<Vec<SyncOperationRecord>, ConflictBundleError> {
    prepared_operations(
        load_conflict_records(state_root)?,
        workspace_id,
        device_id,
        now,
    )
}

pub fn conflict_occurrence_preparation_required(
    state_root: &Path,
) -> Result<bool, ConflictBundleError> {
    Ok(load_conflict_records(state_root)?
        .iter()
        .any(|record| next_reconcile_step(record).is_some() && record.bundle_object.is_none()))
}

pub(crate) fn prepare_pending_conflict_occurrence_operations<E>(
    state_root: &Path,
    workspace_id: &WorkspaceId,
    device_id: &DeviceId,
    now: &str,
    mut ensure_bundle: impl FnMut(&mut ConflictRecord, &[ConflictFile]) -> Result<(), E>,
) -> Result<Vec<SyncOperationRecord>, E>
where
    E: From<ConflictBundleError>,
{
    let mut records = load_conflict_records(state_root)?;
    for record in records
        .iter_mut()
        .filter(|record| next_reconcile_step(record).is_some())
    {
        if record.bundle_object.is_none() {
            let files = load_conflict_files(record)?;
            ensure_bundle(record, &files)?;
        }
        if record.bundle_object.is_none() {
            return Err(ConflictBundleError::MissingOccurrenceField {
                conflict_id: record.id.clone(),
                field: "bundleObject",
            }
            .into());
        }
    }
    prepared_operations(records, workspace_id, device_id, now).map_err(Into::into)
}

pub fn decode_conflict_occurrence_operation(
    operation: &SyncOperationRecord,
) -> Result<ConflictOccurrenceReconcile, serde_json::Error> {
    serde_json::from_str(&operation.payload_json)
}

pub fn conflict_occurrence_queue_result(
    outcome: ConflictReconcileOutcome,
) -> Result<String, serde_json::Error> {
    serde_json::to_string(&ConflictOccurrenceQueueResult { outcome })
}

fn next_reconcile_step(record: &ConflictRecord) -> Option<ConflictReconcileStep> {
    if record.remote_conflict_published_at.is_none() {
        return Some(ConflictReconcileStep::PublishOccurrence);
    }
    if record.remote_resolution_synced_at.is_some() {
        return None;
    }
    match record.state {
        ConflictState::Unresolved => None,
        ConflictState::Accepted => Some(ConflictReconcileStep::ResolveAccepted),
        ConflictState::Rejected => Some(ConflictReconcileStep::ResolveRejected),
    }
}

fn prepared_operations(
    records: Vec<ConflictRecord>,
    workspace_id: &WorkspaceId,
    device_id: &DeviceId,
    now: &str,
) -> Result<Vec<SyncOperationRecord>, ConflictBundleError> {
    records
        .into_iter()
        .filter_map(|record| {
            let step = next_reconcile_step(&record)?;
            let bundle_object = record.bundle_object.clone()?;
            Some(conflict_occurrence_operation(
                PreparedConflictOccurrence {
                    record,
                    bundle_object,
                    step,
                },
                workspace_id,
                device_id,
                now,
            ))
        })
        .collect()
}

fn conflict_occurrence_operation(
    prepared: PreparedConflictOccurrence,
    workspace_id: &WorkspaceId,
    device_id: &DeviceId,
    now: &str,
) -> Result<SyncOperationRecord, ConflictBundleError> {
    let PreparedConflictOccurrence {
        record,
        bundle_object,
        step,
    } = prepared;
    let base_snapshot_id = record.base_snapshot_id.clone().ok_or_else(|| {
        ConflictBundleError::MissingOccurrenceField {
            conflict_id: record.id.clone(),
            field: "baseSnapshotId",
        }
    })?;
    let remote_snapshot_id = record.remote_snapshot_id.clone().ok_or_else(|| {
        ConflictBundleError::MissingOccurrenceField {
            conflict_id: record.id.clone(),
            field: "remoteSnapshotId",
        }
    })?;
    let desired_state = step.desired_state();
    let conflict_id = ConflictId::new(record.id.clone());
    let input = ConflictOccurrenceReconcile {
        workspace_id: workspace_id.clone(),
        conflict_id: conflict_id.clone(),
        conflict_kind: conflict_kind_name(record.conflict_kind).to_string(),
        paths: record.paths,
        contains_secrets: record.contains_secrets,
        base_snapshot_id: SnapshotId::new(base_snapshot_id.clone()),
        remote_snapshot_id: SnapshotId::new(remote_snapshot_id.clone()),
        occurrence_version: record.occurrence_version,
        desired_state,
        device_id: device_id.clone(),
        reason: record.reason,
        bundle_object: Some(bundle_object),
    };
    let idempotency_key = format!(
        "conflict-occurrence:{}:{}:{}:{}",
        workspace_id.as_str(),
        conflict_id.as_str(),
        input.occurrence_version,
        desired_state.as_str()
    );
    let operation_id = format!(
        "conflict-followup-{}",
        super::short_hash([idempotency_key.as_bytes()])
    );
    Ok(SyncOperationRecord {
        id: operation_id,
        workspace_id: workspace_id.clone(),
        kind: SyncOperationKind::ConflictOccurrenceReconcile,
        resource_key: SyncResourceKey::conflict_followup(workspace_id.clone(), conflict_id),
        state: SyncOperationState::Queued,
        idempotency_key,
        base_version: None,
        base_snapshot_id: Some(base_snapshot_id),
        target_snapshot_id: Some(remote_snapshot_id),
        device_id: Some(device_id.clone()),
        payload_json: serde_json::to_string(&input)?,
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
        created_at: now.to_string(),
        updated_at: now.to_string(),
    })
}

fn conflict_kind_name(kind: ConflictKind) -> &'static str {
    match kind {
        ConflictKind::Text => "text",
        ConflictKind::StructuredText => "structured-text",
        ConflictKind::Binary => "binary",
        ConflictKind::OpaqueGit => "opaque-git",
        ConflictKind::DeleteEdit => "delete-edit",
        ConflictKind::PathShape => "path-shape",
        ConflictKind::EnvKey => "env-key",
        ConflictKind::MergePlugin => "merge-plugin",
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use bowline_control_plane::{ControlPlaneTimestamp, ObjectKind, ObjectPointer};

    use crate::{
        metadata::{MetadataStore, SyncOperationKind, SyncOperationState, SyncResourceKey},
        sync::{
            ConflictFile, ConflictRecord, conflict_bundle_object_id, create_conflict_bundle,
            mark_conflict_occurrence_reconciled, set_conflict_bundle_object,
            transition_conflict_occurrence_state,
        },
    };

    use super::*;

    fn temp_state_root(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("bowline-{name}-{nonce}"));
        fs::create_dir_all(&root).expect("state root");
        root
    }

    fn test_pointer(record: &ConflictRecord) -> ObjectPointer {
        let object_id = conflict_bundle_object_id(record);
        ObjectPointer {
            object_key: bowline_storage::ObjectKey::from_conflict_bundle_id(object_id.as_str())
                .expect("conflict bundle object key")
                .as_str()
                .to_string(),
            content_id: object_id,
            byte_len: 128,
            hash: "b3_0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            key_epoch: 1,
            kind: ObjectKind::ConflictBundle,
            created_at: ControlPlaneTimestamp { tick: 42 },
        }
    }

    fn seed_unprepared_conflict(root: &Path) -> ConflictRecord {
        let mut record = ConflictRecord::same_path("src/main.rs");
        record.base_snapshot_id = Some("snap_base".to_string());
        record.remote_snapshot_id = Some("snap_remote".to_string());
        create_conflict_bundle(
            root,
            record,
            &[ConflictFile {
                relative_path: "src/main.rs".to_string(),
                base: Some(b"base".to_vec()),
                local: Some(b"local".to_vec()),
                remote: Some(b"remote".to_vec()),
            }],
        )
        .expect("conflict bundle")
        .record
    }

    fn seed_conflict(root: &Path) -> ConflictRecord {
        let mut record = seed_unprepared_conflict(root);
        let pointer = test_pointer(&record);
        assert!(set_conflict_bundle_object(&record, pointer.clone()).expect("persist pointer"));
        record.bundle_object = Some(pointer);
        record
    }

    #[test]
    fn scanner_recreates_same_exact_operation_after_crash_gap() {
        let root = temp_state_root("conflict-crash-gap");
        let record = seed_conflict(&root);
        let workspace_id = WorkspaceId::new("ws_code");
        let device_id = DeviceId::new("device_local");
        let first = pending_conflict_occurrence_operations(
            &root,
            &workspace_id,
            &device_id,
            "2026-07-13T10:00:00Z",
        )
        .expect("scan")
        .pop()
        .expect("pending operation");
        assert_eq!(first.kind, SyncOperationKind::ConflictOccurrenceReconcile);
        assert_eq!(
            first.resource_key,
            SyncResourceKey::conflict_followup(
                workspace_id.clone(),
                ConflictId::new(record.id.clone()),
            )
        );
        let store = MetadataStore::open(root.join("local.sqlite3")).expect("store");
        store.enqueue_sync_operation(&first).expect("enqueue");
        store
            .connection()
            .execute("DELETE FROM sync_operations WHERE id = ?1", [&first.id])
            .expect("simulate enqueue crash gap");

        let recreated = pending_conflict_occurrence_operations(
            &root,
            &workspace_id,
            &device_id,
            "2026-07-13T10:01:00Z",
        )
        .expect("rescan")
        .pop()
        .expect("recreated operation");
        assert_eq!(recreated.id, first.id);
        assert_eq!(recreated.idempotency_key, first.idempotency_key);
        assert_eq!(recreated.payload_json, first.payload_json);
        store.enqueue_sync_operation(&recreated).expect("reenqueue");
        assert_eq!(
            store
                .sync_operations(&workspace_id)
                .expect("operations")
                .len(),
            1
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn crash_after_bundle_write_prepares_encrypted_bundle_before_enqueue() {
        let root = temp_state_root("conflict-bundle-upload-crash-gap");
        let record = seed_unprepared_conflict(&root);
        let workspace_id = WorkspaceId::new("ws_code");
        let device_id = DeviceId::new("device_local");
        assert!(
            pending_conflict_occurrence_operations(
                &root,
                &workspace_id,
                &device_id,
                "2026-07-13T10:00:00Z",
            )
            .expect("unprepared scan")
            .is_empty()
        );
        assert!(conflict_occurrence_preparation_required(&root).expect("preparation requirement"));

        let operations = prepare_pending_conflict_occurrence_operations(
            &root,
            &workspace_id,
            &device_id,
            "2026-07-13T10:01:00Z",
            |current, files| {
                assert_eq!(files.len(), 1);
                assert_eq!(files[0].relative_path, "src/main.rs");
                assert_eq!(files[0].base.as_deref(), Some(b"base".as_slice()));
                assert_eq!(files[0].local.as_deref(), Some(b"local".as_slice()));
                assert_eq!(files[0].remote.as_deref(), Some(b"remote".as_slice()));
                let pointer = test_pointer(current);
                assert!(
                    set_conflict_bundle_object(current, pointer.clone())
                        .expect("persist recovered pointer")
                );
                current.bundle_object = Some(pointer);
                Ok::<(), ConflictBundleError>(())
            },
        )
        .expect("prepare recovered occurrence");

        assert_eq!(operations.len(), 1);
        let input = decode_conflict_occurrence_operation(&operations[0]).expect("payload");
        assert_eq!(input.desired_state, ConflictOccurrenceState::Unresolved);
        assert_eq!(input.bundle_object, Some(test_pointer(&record)));
        let persisted = load_conflict_records(&root)
            .expect("records")
            .pop()
            .expect("record");
        assert_eq!(persisted.bundle_object, Some(test_pointer(&record)));
        assert!(!conflict_occurrence_preparation_required(&root).expect("preparation completed"));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn terminal_local_state_publishes_before_accept_or_reject() {
        for terminal_state in [ConflictState::Accepted, ConflictState::Rejected] {
            let root = temp_state_root(match terminal_state {
                ConflictState::Accepted => "conflict-accepted-before-publication",
                ConflictState::Rejected => "conflict-rejected-before-publication",
                ConflictState::Unresolved => unreachable!("terminal fixture"),
            });
            let record = seed_unprepared_conflict(&root);
            assert!(
                transition_conflict_occurrence_state(
                    record.bundle_path.as_deref().expect("bundle root"),
                    &record.id,
                    record.occurrence_version,
                    terminal_state,
                    "2026-07-13T10:00:00Z",
                )
                .expect("local terminal transition")
            );
            let workspace_id = WorkspaceId::new("ws_code");
            let device_id = DeviceId::new("device_local");
            let publish = prepare_pending_conflict_occurrence_operations(
                &root,
                &workspace_id,
                &device_id,
                "2026-07-13T10:01:00Z",
                |current, _files| {
                    let pointer = test_pointer(current);
                    assert!(
                        set_conflict_bundle_object(current, pointer.clone())
                            .expect("persist pointer")
                    );
                    current.bundle_object = Some(pointer);
                    Ok::<(), ConflictBundleError>(())
                },
            )
            .expect("prepare publication")
            .pop()
            .expect("publication operation");
            let publish_input =
                decode_conflict_occurrence_operation(&publish).expect("publication payload");
            assert_eq!(
                publish_input.desired_state,
                ConflictOccurrenceState::Unresolved
            );
            assert!(
                mark_conflict_occurrence_reconciled(
                    &root,
                    &record.id,
                    record.occurrence_version,
                    ConflictState::Unresolved,
                    "2026-07-13T10:02:00Z",
                )
                .expect("publication marker")
            );

            let resolve = prepare_pending_conflict_occurrence_operations(
                &root,
                &workspace_id,
                &device_id,
                "2026-07-13T10:03:00Z",
                |_current, _files| -> Result<(), ConflictBundleError> {
                    panic!("persisted publication bundle must be reused")
                },
            )
            .expect("prepare resolution")
            .pop()
            .expect("resolution operation");
            let resolve_input =
                decode_conflict_occurrence_operation(&resolve).expect("resolution payload");
            assert_eq!(
                resolve_input.desired_state,
                match terminal_state {
                    ConflictState::Accepted => ConflictOccurrenceState::Accepted,
                    ConflictState::Rejected => ConflictOccurrenceState::Rejected,
                    ConflictState::Unresolved => unreachable!("terminal fixture"),
                }
            );
            fs::remove_dir_all(root).expect("cleanup");
        }
    }

    #[test]
    fn duplicate_enqueue_is_immutable_and_cross_kind_reuse_is_rejected() {
        let root = temp_state_root("conflict-deduplication");
        seed_conflict(&root);
        let workspace_id = WorkspaceId::new("ws_code");
        let device_id = DeviceId::new("device_local");
        let operation = pending_conflict_occurrence_operations(
            &root,
            &workspace_id,
            &device_id,
            "2026-07-13T10:00:00Z",
        )
        .expect("scan")
        .pop()
        .expect("pending operation");
        let store = MetadataStore::open(root.join("local.sqlite3")).expect("store");
        store.enqueue_sync_operation(&operation).expect("enqueue");
        store
            .claim_next_sync_operation(
                &workspace_id,
                "worker",
                "2026-07-13T10:00:01Z",
                "2999-01-01T00:00:00Z",
            )
            .expect("claim")
            .expect("claimed");
        store
            .enqueue_sync_operation(&operation)
            .expect("exact duplicate");
        let stored = store
            .sync_operations(&workspace_id)
            .expect("operations")
            .pop()
            .expect("operation");
        assert_eq!(stored.state, SyncOperationState::Claimed);
        assert_eq!(stored.attempt_count, 1);

        let mut cross_kind = operation.clone();
        cross_kind.kind = SyncOperationKind::Reconcile;
        cross_kind.resource_key = SyncResourceKey::workspace_sync(workspace_id.clone());
        assert!(store.enqueue_sync_operation(&cross_kind).is_err());
        fs::remove_dir_all(root).expect("cleanup");
    }
}
