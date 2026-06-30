use std::{collections::BTreeMap, fs};

use bowline_core::{
    ids::{ContentId, ManifestId, PackId, ProjectId, SnapshotId, WorkspaceId},
    policy::MaterializationMode,
    workspace_graph::{
        ContentLocator, HydrationState, NamespaceEntry, NamespaceEntryKind, RefKind, SnapshotKind,
        SnapshotManifest, WorkspaceRef, workspace_content_id,
    },
};
use bowline_local::{
    metadata::MetadataStore,
    scanner::{PathObservation, scan_workspace},
    workspace::TempWorkspace,
};
use bowline_storage::{
    ByteStore, LocalByteStore, LocalContentCache, ObjectKind, PackRecordInput,
    RangeHydrationRequest, StorageKey, seal_snapshot_manifest, write_source_packs,
};

#[test]
fn scanned_workspace_can_pack_manifest_persist_locators_and_range_hydrate() {
    let temp = TempWorkspace::new("local-storage-substrate").expect("temp workspace");
    temp.create_project("acme/web").expect("project root");
    temp.write_project_file("acme/web", "package.json", br#"{"name":"web"}"#)
        .expect("package");
    for index in 0..80 {
        temp.write_project_file(
            "acme/web",
            format!("src/file_{index:03}.ts"),
            format!("export const value{index} = {index};\n").as_bytes(),
        )
        .expect("source file");
    }
    temp.create_generated_folder("acme/web", "node_modules")
        .expect("generated folder");

    let report = scan_workspace(temp.root()).expect("workspace scan");
    let syncable_files = report
        .paths
        .iter()
        .filter(|path| {
            !path.is_dir
                && matches!(
                    path.policy.mode,
                    MaterializationMode::WorkspaceSync | MaterializationMode::EncryptedSync
                )
        })
        .collect::<Vec<_>>();
    assert!(syncable_files.len() >= 80);

    let workspace_id = WorkspaceId::new("ws_code");
    let state = TempWorkspace::new("local-storage-substrate-state").expect("state workspace");
    let state_root = state.root();
    let metadata = MetadataStore::open(state_root.join("local.sqlite3")).expect("metadata opens");
    metadata
        .insert_workspace(&workspace_id, "Theo Code", "2026-06-24T12:00:00Z")
        .expect("workspace insert");
    metadata
        .insert_root(
            "root_code",
            &workspace_id,
            &temp.root().display().to_string(),
            "2026-06-24T12:00:00Z",
        )
        .expect("root insert");

    let key = StorageKey::deterministic(21);
    let records = records_for_scan(temp.root(), &syncable_files);
    let packs =
        write_source_packs(workspace_id.clone(), &records, 1024, key, 1).expect("packs write");
    let object_store =
        LocalByteStore::open_deterministic(state_root.join("objects"), 1).expect("store opens");
    let cache = LocalContentCache::open(state_root.join("cache")).expect("cache opens");
    let mut locators =
        BTreeMap::<ContentId, (PackId, bowline_storage::ObjectKey, ContentLocator)>::new();

    for pack in &packs {
        object_store
            .put_object(
                pack.object_key.clone(),
                ObjectKind::SourcePack,
                &pack.bytes,
                None,
            )
            .expect("pack stored");
        metadata
            .put_pack_record(
                &workspace_id,
                &pack.pack_id,
                "source-pack",
                pack.bytes.len() as u64,
                "pending",
                "2026-06-24T12:01:00Z",
            )
            .expect("pack metadata");
        for locator in &pack.locators {
            metadata
                .put_content_locator(&workspace_id, locator, "2026-06-24T12:02:00Z")
                .expect("locator metadata");
            locators.insert(
                locator.content_id.clone(),
                (
                    pack.pack_id.clone(),
                    pack.object_key.clone(),
                    locator.clone(),
                ),
            );
        }
    }

    let manifest = manifest_for_scan(&workspace_id, &syncable_files, &records, &locators);
    let sealed_manifest =
        seal_snapshot_manifest(ManifestId::new("mf_0011223344556677"), &manifest, key, 1)
            .expect("manifest seals");
    object_store
        .put_object(
            sealed_manifest.pointer.object_key.clone(),
            ObjectKind::SnapshotManifest,
            &sealed_manifest.bytes,
            None,
        )
        .expect("manifest stored");

    assert!(packs.len() + 1 < syncable_files.len() / 5);
    let object_keys = object_store.list_object_keys().expect("object keys");
    assert_eq!(object_keys.len(), packs.len() + 1);
    assert!(object_keys.len() < syncable_files.len() / 5);
    assert_eq!(
        metadata
            .content_locators(&workspace_id)
            .expect("stored locators")
            .len(),
        records.len()
    );
    for key in object_keys {
        assert!(key.as_str().starts_with("packs_pk_") || key.as_str().starts_with("manifests_mf_"));
        for path in &syncable_files {
            for segment in path.path.split('/') {
                if segment.len() >= 3 {
                    assert!(
                        !key.as_str().contains(segment),
                        "object key leaked {segment}"
                    );
                }
            }
        }
    }

    let target = &records[20];
    let (_pack_id, object_key, locator) = locators
        .get(&target.content_id)
        .expect("target locator")
        .clone();
    let hydrated = cache
        .hydrate_record_from_range(
            &object_store,
            RangeHydrationRequest {
                object_key: &object_key,
                workspace_id: &workspace_id,
                locator: &locator,
                content_key: [21_u8; 32],
                key,
                key_epoch: 1,
            },
        )
        .expect("range hydrate");

    assert_eq!(hydrated, target.bytes);
    assert_eq!(object_store.metrics().range_read_count, 1);
    assert_eq!(object_store.metrics().full_read_count, 0);
}

fn records_for_scan(root: &std::path::Path, paths: &[&PathObservation]) -> Vec<PackRecordInput> {
    paths
        .iter()
        .map(|path| {
            let bytes = fs::read(root.join(&path.path)).expect("source file bytes");
            PackRecordInput {
                content_id: workspace_content_id([21_u8; 32], &bytes),
                bytes,
            }
        })
        .collect()
}

fn manifest_for_scan(
    workspace_id: &WorkspaceId,
    paths: &[&PathObservation],
    records: &[PackRecordInput],
    locators: &BTreeMap<ContentId, (PackId, bowline_storage::ObjectKey, ContentLocator)>,
) -> SnapshotManifest {
    let entries = paths
        .iter()
        .zip(records)
        .map(|(path, record)| NamespaceEntry {
            path: path.path.clone(),
            kind: NamespaceEntryKind::File,
            classification: path.policy.classification,
            mode: path.policy.mode,
            access: path.policy.access.clone(),
            content_id: Some(record.content_id.clone()),
            locator: Some(locators.get(&record.content_id).expect("locator").2.clone()),
            symlink_target: None,
            byte_len: path.byte_len,
            hydration_state: HydrationState::Cold,
        })
        .collect::<Vec<_>>();

    SnapshotManifest {
        schema_version: 1,
        snapshot_id: SnapshotId::new("snap_scanned_storage"),
        workspace_id: workspace_id.clone(),
        project_id: Some(ProjectId::new("proj_acme_web")),
        kind: SnapshotKind::WorkspaceHead,
        base_snapshot_id: None,
        entries,
        refs: vec![WorkspaceRef {
            name: "workspace".to_string(),
            target_snapshot_id: SnapshotId::new("snap_scanned_storage"),
            kind: RefKind::Workspace,
        }],
    }
}
