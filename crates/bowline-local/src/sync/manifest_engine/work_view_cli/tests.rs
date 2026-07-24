use bowline_core::work_views::{OVERLAY_HEAD_EMPTY, WorkViewLifecycle as WireLifecycle};

use crate::workspace::TempWorkspace;

use super::super::aux_index::{AuxIndex, WorkViewLifecycle, WorkViewRecord};
use super::super::manifest::ManifestKey;
use super::*;

fn record(base: &str, overlay: &str, lifecycle: WorkViewLifecycle) -> WorkViewRecord {
    WorkViewRecord {
        project_id: bowline_core::ids::ProjectId::new("proj_test"),
        project_path: "apps/web".to_string(),
        name: "auth-fix".to_string(),
        owner_device_id: bowline_core::ids::DeviceId::new("dev_test"),
        created_at: "2026-07-21T00:00:00Z".to_string(),
        updated_at: "2026-07-21T00:00:00Z".to_string(),
        base_manifest_key: ManifestKey::new(base),
        overlay_manifest_key: ManifestKey::new(overlay),
        lifecycle,
    }
}

fn wire_view() -> bowline_core::work_views::WorkView {
    use bowline_core::ids::{ProjectId, SnapshotId, WorkspaceId};
    use bowline_core::work_views::{
        WorkView, WorkViewRetention, WorkViewRetentionState, WorkViewSyncState, WorkViewVisibility,
    };
    WorkView {
        id: bowline_core::ids::WorkViewId::new("work_test"),
        workspace_id: WorkspaceId::new("ws_test"),
        project_id: ProjectId::new("proj_test"),
        project_path: "apps/web".to_string(),
        name: "auth-fix".to_string(),
        visible_path: "~/Code/.work/apps/web/auth-fix".to_string(),
        base_snapshot_id: SnapshotId::new("stale"),
        overlay_head: "stale".to_string(),
        overlay_version: 9,
        env_profile: "default".to_string(),
        lifecycle: WireLifecycle::ReviewReady,
        visibility: WorkViewVisibility::DefaultVisible,
        sync_state: WorkViewSyncState::LocalOnly,
        retention: WorkViewRetention {
            state: WorkViewRetentionState::Current,
            retain_until: None,
            restorable: false,
        },
        owner_device_id: None,
        followed_by: Vec::new(),
        host_materializations: Vec::new(),
        attention: Vec::new(),
        created_at: "2026-07-21T00:00:00Z".to_string(),
        updated_at: "2026-07-21T00:00:00Z".to_string(),
    }
}

#[test]
fn aux_file_round_trips_and_missing_file_is_empty() {
    let temp = TempWorkspace::new("work-view-cli-aux-file").expect("temp workspace");
    let root = temp.root();

    let empty = read_aux_index_file(root).expect("missing file reads as empty");
    assert_eq!(empty, AuxIndex::empty());

    let mut aux = AuxIndex::empty();
    aux.upsert(
        WorkViewId::new("work_a"),
        record("m_base", "m_base", WorkViewLifecycle::Active),
    );
    write_aux_index_file(root, &aux).expect("aux writes");
    assert!(aux_index_file_path(root).is_file());

    let read_back = read_aux_index_file(root).expect("aux reads");
    assert_eq!(read_back, aux);
}

#[test]
fn aux_write_refuses_symlinked_meta_dir_and_leaves_external_untouched() {
    use std::os::unix::fs::symlink;

    let temp = TempWorkspace::new("work-view-cli-meta-symlink").expect("temp workspace");
    let root = temp.root();
    let external = TempWorkspace::new("work-view-cli-external").expect("external dir");
    let external_root = external.root();

    // `.bowline-meta` is a symlink pointing OUTSIDE the workspace. A naive
    // create_dir_all + write would follow it and drop the aux index into the
    // external directory as the Bowline user; the no-follow guard must refuse.
    symlink(external_root, root.join(".bowline-meta")).expect("symlink meta dir");

    let mut aux = AuxIndex::empty();
    aux.upsert(
        WorkViewId::new("work_a"),
        record("m_base", "m_base", WorkViewLifecycle::Active),
    );
    let result = write_aux_index_file(root, &aux);
    assert!(
        matches!(
            &result,
            Err(WorkViewCliError::WorkspacePathBlocked { path }) if *path == AUX_INDEX_PATH
        ),
        "expected a blocked-path refusal, got {result:?}"
    );
    // Nothing was written through the symlink into the external directory.
    assert!(!external_root.join("aux-index").exists());
    assert!(!external_root.join(".aux-index.tmp").exists());
}

#[test]
fn aux_write_refuses_dangling_symlinked_meta_dir() {
    use std::os::unix::fs::symlink;

    let temp = TempWorkspace::new("work-view-cli-meta-dangling").expect("temp workspace");
    let root = temp.root();
    // A symlink whose target does not even exist is still refused: the guard keys
    // off the on-disk symlink shape (symlink_metadata), not the target.
    symlink(
        "/nonexistent-bowline-external-target",
        root.join(".bowline-meta"),
    )
    .expect("dangling symlink");

    let result = write_aux_index_file(root, &AuxIndex::empty());
    assert!(
        matches!(
            &result,
            Err(WorkViewCliError::WorkspacePathBlocked { path }) if *path == AUX_INDEX_PATH
        ),
        "expected a blocked-path refusal, got {result:?}"
    );
    assert!(!aux_index_file_path(root).exists());
}

#[test]
fn overlay_engine_truth_maps_base_and_empty_overlay() {
    let mut view = wire_view();
    overlay_engine_truth(
        &mut view,
        &record("m_base", "m_base", WorkViewLifecycle::Active),
    );
    assert_eq!(view.base_snapshot_id.as_str(), "m_base");
    assert_eq!(view.overlay_head, OVERLAY_HEAD_EMPTY);
    assert_eq!(view.overlay_version, 0);
    assert_eq!(view.lifecycle, WireLifecycle::Active);

    overlay_engine_truth(
        &mut view,
        &record("m_base", "m_over", WorkViewLifecycle::Accepted),
    );
    assert_eq!(view.overlay_head, "m_over");
    assert_eq!(view.overlay_version, 1);
    assert_eq!(view.lifecycle, WireLifecycle::Accepted);
}

#[test]
fn restore_reactivates_discarded_but_refuses_accepted() {
    let mut aux = AuxIndex::empty();
    let id = WorkViewId::new("work_a");
    aux.upsert(
        id.clone(),
        record("m_base", "m_base", WorkViewLifecycle::Discarded),
    );
    restore_record(&mut aux, &id).expect("discarded restores");
    assert_eq!(
        aux.get(&id).expect("record").lifecycle,
        WorkViewLifecycle::Active
    );

    aux.upsert(
        id.clone(),
        record("m_base", "m_over", WorkViewLifecycle::Accepted),
    );
    assert!(matches!(
        restore_record(&mut aux, &id),
        Err(WorkViewCliError::Unrestorable { .. })
    ));

    assert!(matches!(
        restore_record(&mut aux, &WorkViewId::new("work_missing")),
        Err(WorkViewCliError::UnknownView { .. })
    ));
}
