use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use bowline_core::{
    ids::{
        ContentId, ManifestDigest, ManifestId, NamespacePageId, PackId, ProjectId, SnapshotId,
        WorkspaceId,
    },
    workspace_graph::{
        ContentLocator, RefKind, SnapshotKind, SnapshotManifest, WorkspaceRef, workspace_content_id,
    },
};
use bowline_storage::{
    ByteStore, LocalByteStore, LocalContentCache, LocatorIndexBinding, ObjectKind, PackRecordInput,
    RangeHydrationRequest, StorageKey, open_locator_index, seal_locator_index,
    seal_snapshot_manifest, write_source_packs,
};

#[test]
fn local_storage_substrate_packs_manifests_and_range_hydrates() {
    let temp = TempDir::new("substrate");
    let workspace_id = WorkspaceId::new("ws_code");
    let key = StorageKey::deterministic(13);
    let store =
        LocalByteStore::open_deterministic(temp.path().join("objects"), 1).expect("store opens");
    let cache = LocalContentCache::open(temp.path().join("cache")).expect("cache opens");
    let workload = tiny_file_workload(160);
    let records = workload
        .iter()
        .map(|(_, bytes)| PackRecordInput {
            content_id: workspace_content_id([42_u8; 32], bytes),
            bytes: bytes.clone(),
        })
        .collect::<Vec<_>>();

    let packs = write_source_packs(workspace_id.clone(), &records, 1024, key, 1)
        .expect("source packs write");
    assert!(packs.len() + 1 < workload.len() / 10);

    let mut locators =
        BTreeMap::<ContentId, (PackId, bowline_storage::ObjectKey, ContentLocator)>::new();
    for pack in &packs {
        store
            .put_object(
                pack.object_key.clone(),
                ObjectKind::SourcePack,
                &pack.bytes,
                None,
            )
            .expect("pack object stored");
        for locator in &pack.locators {
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

    let manifest = manifest_for_workload(&workspace_id, &workload);
    let sealed_manifest =
        seal_snapshot_manifest(ManifestId::new("mf_0011223344556677"), &manifest, key, 1)
            .expect("manifest seals");
    store
        .put_object(
            sealed_manifest.pointer.object_key.clone(),
            ObjectKind::SnapshotManifest,
            &sealed_manifest.bytes,
            None,
        )
        .expect("manifest stored");

    let object_keys = store.list_object_keys().expect("object keys");
    assert_eq!(object_keys.len(), packs.len() + 1);
    assert!(object_keys.len() < workload.len() / 10);
    for object_key in object_keys {
        assert!(
            object_key.as_str().starts_with("packs_pk_")
                || object_key.as_str().starts_with("manifests_mf_")
                || object_key.as_str().starts_with("indexes_ix_")
        );
        for (path, _) in &workload {
            for segment in path.split('/') {
                if segment.len() >= 3 {
                    assert!(
                        !object_key.as_str().contains(segment),
                        "object key leaked {segment}"
                    );
                }
            }
        }
    }
    for forbidden in ["acme/web", "src/file_001.ts", ".env.local", ".git"] {
        assert!(
            !sealed_manifest
                .bytes
                .windows(forbidden.len())
                .any(|window| window == forbidden.as_bytes()),
            "manifest leaked {forbidden}"
        );
    }
    let target_index = 42;
    let target_content_id = records[target_index].content_id.clone();
    let (_pack_id, object_key, locator) = locators
        .get(&target_content_id)
        .expect("target locator")
        .clone();
    let hydrated = cache
        .hydrate_record_from_range(
            &store,
            RangeHydrationRequest {
                object_key: &object_key,
                workspace_id: &workspace_id,
                locator: &locator,
                content_key: [42_u8; 32],
                content_verification: bowline_storage::ContentVerification::WorkspaceKeyed,
                key,
                key_epoch: 1,
            },
        )
        .expect("cold file hydrates by range");

    assert_eq!(hydrated, workload[target_index].1);
    assert_eq!(store.metrics().range_read_count, 1);
    assert_eq!(store.metrics().full_read_count, 0);

    cache
        .prefetch_pack(&store, &object_key)
        .expect("hot project pack prefetch");
    assert_eq!(store.metrics().full_read_count, 1);
}

#[test]
fn locator_index_is_bound_to_manifest_snapshot_and_digest() {
    let workspace_id = WorkspaceId::new("ws_code");
    let manifest_id = ManifestId::new("mf_0011223344556677");
    let snapshot_id = SnapshotId::new("snap_substrate");
    let key = StorageKey::deterministic(29);
    let locator_payload = br#"{"content":"content_0011223344556677","pack":"packs_pk_deadbeefdeadbeef","offset":42,"length":128}"#;

    let sealed = seal_locator_index(
        workspace_id.clone(),
        manifest_id.clone(),
        snapshot_id.clone(),
        locator_payload,
        key,
        1,
    )
    .expect("locator index seals");

    assert_eq!(sealed.pointer.manifest_id, manifest_id);
    assert_eq!(sealed.pointer.snapshot_id, snapshot_id);
    assert_eq!(sealed.pointer.format_version, 1);
    assert!(
        sealed
            .pointer
            .object_key
            .as_str()
            .starts_with("indexes_ix_")
    );
    for forbidden in ["content_0011223344556677", "packs_pk_deadbeefdeadbeef"] {
        assert!(
            !sealed
                .bytes
                .windows(forbidden.len())
                .any(|window| window == forbidden.as_bytes()),
            "locator index leaked {forbidden}"
        );
    }

    let opened = open_locator_index(&sealed, key, &workspace_id, &sealed.pointer.binding())
        .expect("locator index opens with matching binding");
    assert_eq!(opened, locator_payload);

    let wrong_manifest = LocatorIndexBinding {
        manifest_id: ManifestId::new("mf_8899aabbccddeeff"),
        ..sealed.pointer.binding()
    };
    assert!(open_locator_index(&sealed, key, &workspace_id, &wrong_manifest).is_err());

    let wrong_digest = LocatorIndexBinding {
        locator_table_digest: format!("b3_{}", "0".repeat(64)),
        ..sealed.pointer.binding()
    };
    assert!(open_locator_index(&sealed, key, &workspace_id, &wrong_digest).is_err());

    assert!(
        open_locator_index(
            &sealed,
            key,
            &WorkspaceId::new("ws_other"),
            &sealed.pointer.binding(),
        )
        .is_err()
    );
}

fn tiny_file_workload(count: usize) -> Vec<(String, Vec<u8>)> {
    (0..count)
        .map(|index| {
            (
                format!("acme/web/src/file_{index:03}.ts"),
                format!("export const value{index} = {index};\n").into_bytes(),
            )
        })
        .collect()
}

fn manifest_for_workload(
    workspace_id: &WorkspaceId,
    workload: &[(String, Vec<u8>)],
) -> SnapshotManifest {
    SnapshotManifest {
        schema_version: bowline_core::workspace_graph::SNAPSHOT_SCHEMA_VERSION,
        snapshot_id: SnapshotId::new("snap_substrate"),
        workspace_id: workspace_id.clone(),
        project_id: Some(ProjectId::new("proj_acme_web")),
        kind: SnapshotKind::WorkspaceHead,
        base_snapshot_id: None,
        namespace_root_id: NamespacePageId::new(format!("nsp_{}", "11".repeat(32))),
        semantic_manifest_digest: ManifestDigest::new(format!("md_{}", "22".repeat(32))),
        entry_count: workload.len() as u64,
        refs: vec![WorkspaceRef {
            name: "workspace".to_string(),
            target_snapshot_id: SnapshotId::new("snap_substrate"),
            kind: RefKind::Workspace,
        }],
    }
}

struct TempDir {
    path: PathBuf,
}

static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(1);

impl TempDir {
    fn new(name: &str) -> Self {
        let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "bowline-storage-{}-{}-{}",
            std::process::id(),
            name,
            sequence
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("temp dir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
