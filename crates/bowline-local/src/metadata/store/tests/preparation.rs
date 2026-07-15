use super::*;
use crate::metadata::{
    OwnedStagedPath, PreparationLeaseId, PreparationLeaseRecord, PreparationLeaseState,
    PreparationOwnerMarker, PreparedStagedContentRecord, SourceFingerprint,
};

#[test]
fn preparation_lifecycle_is_owner_fenced_and_reservation_bounded() {
    let temp = TempWorkspace::new("preparation-lifecycle").expect("temp workspace");
    let store = MetadataStore::open(temp.root().join("state.sqlite3")).expect("metadata opens");
    let workspace_id = WorkspaceId::new("ws_prepare");
    store
        .insert_workspace(&workspace_id, "Code", "2026-07-13T00:00:00Z")
        .expect("workspace insert");
    let owner = PreparationOwnerMarker::new("owner-a");
    let lease = lease("lease-a", &workspace_id, &owner, 12);
    assert!(
        store
            .create_preparation_lease(&lease)
            .expect("lease create")
    );
    assert!(
        !store
            .create_preparation_lease(&lease)
            .expect("duplicate lease is idempotent")
    );
    let mut colliding = lease.clone();
    colliding.owner_marker = PreparationOwnerMarker::new("owner-b");
    assert!(store.create_preparation_lease(&colliding).is_err());

    let first = staged("lease-a", "content-a", "/staging/a", 8, &owner);
    store
        .upsert_prepared_staged_content(&first)
        .expect("first staged content");
    let oversized = staged("lease-a", "content-b", "/staging/b", 5, &owner);
    assert!(store.upsert_prepared_staged_content(&oversized).is_err());

    assert!(
        !store
            .transition_preparation_lease(
                &lease.id,
                &PreparationOwnerMarker::new("owner-b"),
                PreparationLeaseState::Preparing,
                PreparationLeaseState::Prepared,
                "2026-07-13T00:01:00Z",
            )
            .expect("wrong owner is fenced")
    );
    assert!(
        store
            .transition_preparation_lease(
                &lease.id,
                &owner,
                PreparationLeaseState::Preparing,
                PreparationLeaseState::Prepared,
                "2026-07-13T00:01:00Z",
            )
            .expect("prepared transition")
    );
    assert!(store.upsert_prepared_staged_content(&first).is_err());
    assert!(
        store
            .transition_preparation_lease(
                &lease.id,
                &owner,
                PreparationLeaseState::Prepared,
                PreparationLeaseState::ReferencedByUpload,
                "2026-07-13T00:02:00Z",
            )
            .expect("upload reference transition")
    );
    assert!(
        store
            .transition_preparation_lease(
                &lease.id,
                &owner,
                PreparationLeaseState::ReferencedByUpload,
                PreparationLeaseState::Committed,
                "2026-07-13T00:03:00Z",
            )
            .expect("commit transition")
    );
    assert!(
        store
            .transition_preparation_lease(
                &lease.id,
                &owner,
                PreparationLeaseState::Committed,
                PreparationLeaseState::Preparing,
                "2026-07-13T00:04:00Z",
            )
            .is_err(),
        "terminal leases cannot be revived"
    );

    let persisted = store
        .preparation_lease(&lease.id)
        .expect("lease query")
        .expect("persisted lease");
    assert_eq!(persisted.state, PreparationLeaseState::Committed);
    assert_eq!(persisted.reservation_bytes, 12);
    assert_eq!(
        store
            .prepared_staged_content(&lease.id, &owner)
            .expect("content list"),
        vec![first]
    );
}

#[test]
fn orphan_reconciliation_returns_only_owned_aged_terminal_content() {
    let temp = TempWorkspace::new("preparation-orphans").expect("temp workspace");
    let store = MetadataStore::open(temp.root().join("state.sqlite3")).expect("metadata opens");
    let workspace_id = WorkspaceId::new("ws_orphans");
    store
        .insert_workspace(&workspace_id, "Code", "2026-07-13T00:00:00Z")
        .expect("workspace insert");
    let owner = PreparationOwnerMarker::new("owner-a");
    let lease = lease("lease-orphan", &workspace_id, &owner, 10);
    store
        .create_preparation_lease(&lease)
        .expect("lease create");
    let content = staged(
        "lease-orphan",
        "content-orphan",
        "/staging/orphan",
        10,
        &owner,
    );
    store
        .upsert_prepared_staged_content(&content)
        .expect("content insert");
    store
        .transition_preparation_lease(
            &lease.id,
            &owner,
            PreparationLeaseState::Preparing,
            PreparationLeaseState::Abandoned,
            "2026-07-13T00:03:00Z",
        )
        .expect("abandon transition");

    assert!(
        store
            .reconcile_preparation_orphans(&owner, "2026-07-13T00:02:00Z")
            .expect("early reconciliation")
            .is_empty()
    );
    assert!(
        store
            .reconcile_preparation_orphans(
                &PreparationOwnerMarker::new("owner-b"),
                "2026-07-13T00:04:00Z",
            )
            .expect("foreign owner reconciliation")
            .is_empty()
    );
    let orphans = store
        .reconcile_preparation_orphans(&owner, "2026-07-13T00:04:00Z")
        .expect("eligible reconciliation");
    assert_eq!(orphans.len(), 1);
    assert_eq!(orphans[0].content, content);
    assert_eq!(orphans[0].terminal_state, PreparationLeaseState::Abandoned);

    assert!(
        !store
            .forget_reconciled_preparation_orphan(
                &lease.id,
                &ContentId::new("content-orphan"),
                &PreparationOwnerMarker::new("owner-b"),
            )
            .expect("foreign owner forget is fenced")
    );
    assert!(
        store
            .forget_reconciled_preparation_orphan(
                &lease.id,
                &ContentId::new("content-orphan"),
                &owner,
            )
            .expect("owned orphan forgotten")
    );
    assert!(
        store
            .reconcile_preparation_orphans(&owner, "2026-07-13T00:05:00Z")
            .expect("post-cleanup reconciliation")
            .is_empty()
    );
}

fn lease(
    id: &str,
    workspace_id: &WorkspaceId,
    owner_marker: &PreparationOwnerMarker,
    reservation_bytes: u64,
) -> PreparationLeaseRecord {
    PreparationLeaseRecord {
        id: PreparationLeaseId::new(id),
        workspace_id: workspace_id.clone(),
        project_id: None,
        snapshot_candidate_id: SnapshotId::new(format!("candidate-{id}")),
        owner_marker: owner_marker.clone(),
        state: PreparationLeaseState::Preparing,
        reservation_bytes,
        prepared_at: None,
        referenced_at: None,
        finished_at: None,
        created_at: "2026-07-13T00:00:00Z".to_string(),
        updated_at: "2026-07-13T00:00:00Z".to_string(),
    }
}

fn staged(
    lease_id: &str,
    content_id: &str,
    staged_path: &str,
    logical_size: u64,
    owner_marker: &PreparationOwnerMarker,
) -> PreparedStagedContentRecord {
    PreparedStagedContentRecord {
        lease_id: PreparationLeaseId::new(lease_id),
        content_id: ContentId::new(content_id),
        staged_path: OwnedStagedPath::new(staged_path),
        logical_size,
        source_fingerprint: SourceFingerprint::new(format!("fingerprint-{content_id}")),
        owner_marker: owner_marker.clone(),
        created_at: "2026-07-13T00:00:00Z".to_string(),
        updated_at: "2026-07-13T00:00:00Z".to_string(),
    }
}
