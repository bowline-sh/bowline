use std::{
    fs,
    io::{Read as _, Write as _},
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use bowline_control_plane::{
    FakeControlPlaneClient, ObjectControlPlaneClient as _, ObjectMetadataCommit, ObjectPointer,
    UploadIntentRequest,
};
use bowline_core::ids::{ContentId, DeviceId, WorkspaceId};
use bowline_storage::{
    ByteStore, ByteStoreMetrics, LocalByteStore, ObjectKey, RetentionState, open,
};

use super::packs::UploadPackSpoolBuilder;
use super::*;

fn temp_root(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is after epoch")
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "bowline-upload-test-{name}-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&root).expect("create test root");
    root
}

#[test]
fn conflict_bundle_upload_seals_commits_and_round_trips_payload() {
    let workspace_id = WorkspaceId::new("ws_conflict_bundle");
    let device_id = DeviceId::new("device_detector");
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace(workspace_id.as_str());
    let object_root = temp_root("conflict-bundle-object");
    let byte_store = LocalByteStore::open_deterministic(&object_root, 100).expect("byte store");
    let storage_key = StorageKey::from_bytes([8_u8; 32]);
    let mut record = ConflictRecord::same_path("src/main.rs");
    record.base_snapshot_id = Some("snap_base".to_string());
    record.remote_snapshot_id = Some("snap_remote".to_string());
    let files = vec![ConflictFile {
        relative_path: "src/main.rs".to_string(),
        base: Some(b"base".to_vec()),
        local: Some(b"local secret".to_vec()),
        remote: Some(b"remote".to_vec()),
    }];

    let pointer = upload_conflict_bundle_object(UploadConflictBundleRequest {
        record: &record,
        files: &files,
        workspace_id: &workspace_id,
        device_id: &device_id,
        control_plane: &control_plane,
        byte_store: &byte_store,
        storage_key,
        key_epoch: 1,
    })
    .expect("conflict bundle uploads");

    assert_eq!(pointer.kind, ObjectKind::ConflictBundle);
    assert!(pointer.object_key.starts_with("conflicts_cb_"));
    assert_eq!(pointer.content_id, conflict_bundle_object_id(&record));
    let object_key = ObjectKey::new(pointer.object_key.clone()).expect("object key");
    let sealed = byte_store.get_object(&object_key).expect("sealed bytes");
    assert!(
        !sealed
            .windows(b"local secret".len())
            .any(|window| window == b"local secret"),
        "sealed bundle must not contain plaintext conflict bytes"
    );
    let opened = open(
        &sealed,
        storage_key,
        &conflict_bundle_envelope_context(
            &workspace_id,
            conflict_bundle_object_id(&record).as_str(),
            1,
        ),
    )
    .expect("bundle opens on another trusted device");
    let payload: ConflictBundlePayload = serde_json::from_slice(&opened).expect("payload decodes");
    assert_eq!(payload.record.id, record.id);
    assert_eq!(payload.files, files);

    let committed = control_plane
        .head_object_metadata(&workspace_id, &pointer.object_key)
        .expect("committed metadata");
    assert_eq!(committed.kind, StorageObjectKind::ConflictBundle);

    fs::remove_dir_all(object_root).unwrap();
}

#[test]
fn recurring_conflict_occurrences_upload_distinct_bundle_objects() {
    let workspace_id = WorkspaceId::new("ws_recurring_conflict_bundle");
    let device_id = DeviceId::new("device_detector");
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace(workspace_id.as_str());
    let object_root = temp_root("recurring-conflict-bundle-object");
    let byte_store = LocalByteStore::open_deterministic(&object_root, 100).expect("byte store");
    let storage_key = StorageKey::from_bytes([8_u8; 32]);
    let mut record = ConflictRecord::same_path("src/main.rs");
    record.base_snapshot_id = Some("snap_base".to_string());
    record.remote_snapshot_id = Some("snap_remote_1".to_string());
    let first_files = vec![ConflictFile {
        relative_path: "src/main.rs".to_string(),
        base: Some(b"base".to_vec()),
        local: Some(b"first local".to_vec()),
        remote: Some(b"first remote".to_vec()),
    }];
    let first = upload_conflict_bundle_object(UploadConflictBundleRequest {
        record: &record,
        files: &first_files,
        workspace_id: &workspace_id,
        device_id: &device_id,
        control_plane: &control_plane,
        byte_store: &byte_store,
        storage_key,
        key_epoch: 1,
    })
    .expect("first occurrence uploads");

    record.occurrence_version = 2;
    record.remote_snapshot_id = Some("snap_remote_2".to_string());
    let second_files = vec![ConflictFile {
        relative_path: "src/main.rs".to_string(),
        base: Some(b"base".to_vec()),
        local: Some(b"second local".to_vec()),
        remote: Some(b"second remote".to_vec()),
    }];
    let second = upload_conflict_bundle_object(UploadConflictBundleRequest {
        record: &record,
        files: &second_files,
        workspace_id: &workspace_id,
        device_id: &device_id,
        control_plane: &control_plane,
        byte_store: &byte_store,
        storage_key,
        key_epoch: 1,
    })
    .expect("second occurrence uploads");

    assert_ne!(first.object_key, second.object_key);
    assert_ne!(first.content_id, second.content_id);
    let sealed = byte_store
        .get_object(&ObjectKey::new(second.object_key).expect("second object key"))
        .expect("second sealed bundle");
    let opened = open(
        &sealed,
        storage_key,
        &conflict_bundle_envelope_context(&workspace_id, second.content_id.as_str(), 1),
    )
    .expect("second bundle opens");
    let payload: ConflictBundlePayload = serde_json::from_slice(&opened).expect("payload");
    assert_eq!(payload.record.occurrence_version, 2);
    assert_eq!(payload.files, second_files);
    fs::remove_dir_all(object_root).expect("cleanup");
}

fn object_key(suffix: u32) -> ObjectKey {
    ObjectKey::new(format!("packs_pk_{suffix:016x}")).expect("valid object key")
}

fn stable_hash(bytes: &[u8]) -> String {
    format!("b3_{}", blake3::hash(bytes).to_hex())
}

fn metadata(
    key: ObjectKey,
    kind: StorageObjectKind,
    bytes: &[u8],
    key_epoch: u32,
) -> ObjectMetadata {
    ObjectMetadata {
        key,
        kind,
        byte_len: bytes.len() as u64,
        hash: stable_hash(bytes),
        key_epoch,
        created_by_device_id: None,
        created_at_unix_ms: 42,
        retention_state: RetentionState::Pending,
        retain_until_unix_ms: None,
    }
}

struct ReservingByteStore<'a> {
    control_plane: &'a FakeControlPlaneClient,
    workspace_id: &'a str,
}

impl ByteStore for ReservingByteStore<'_> {
    fn put_object(
        &self,
        key: ObjectKey,
        kind: StorageObjectKind,
        bytes: &[u8],
        created_by_device_id: Option<&DeviceId>,
    ) -> Result<ObjectMetadata, ByteStoreError> {
        self.put_object_with_content_id_at_epoch(
            key,
            kind,
            "cid_test",
            bytes,
            1,
            created_by_device_id,
        )
    }

    fn put_object_with_content_id_at_epoch(
        &self,
        key: ObjectKey,
        kind: StorageObjectKind,
        content_id: &str,
        bytes: &[u8],
        key_epoch: u32,
        created_by_device_id: Option<&DeviceId>,
    ) -> Result<ObjectMetadata, ByteStoreError> {
        self.control_plane
            .create_upload_intent(
                UploadIntentRequest::new(
                    self.workspace_id,
                    ObjectKind::try_from(kind)?,
                    bytes.len() as u64,
                )
                .with_object_key(key.as_str())
                .with_content_id(content_id),
            )
            .map_err(|_| ByteStoreError::UnsupportedOperation("fake upload intent"))?;
        Ok(ObjectMetadata {
            key,
            kind,
            byte_len: bytes.len() as u64,
            hash: stable_hash(bytes),
            key_epoch,
            created_by_device_id: created_by_device_id.cloned(),
            created_at_unix_ms: 42,
            retention_state: RetentionState::Pending,
            retain_until_unix_ms: None,
        })
    }

    fn get_object(&self, key: &ObjectKey) -> Result<Vec<u8>, ByteStoreError> {
        Err(ByteStoreError::MissingObject {
            key: key.clone(),
            component: "test byte store",
        })
    }

    fn get_range(
        &self,
        key: &ObjectKey,
        _range: bowline_storage::ByteRange,
    ) -> Result<Vec<u8>, ByteStoreError> {
        Err(ByteStoreError::MissingObject {
            key: key.clone(),
            component: "test byte store",
        })
    }

    fn head_object(&self, key: &ObjectKey) -> Result<ObjectMetadata, ByteStoreError> {
        Err(ByteStoreError::MissingObject {
            key: key.clone(),
            component: "test byte store",
        })
    }

    fn creates_upload_intents(&self) -> bool {
        true
    }

    fn metrics(&self) -> ByteStoreMetrics {
        ByteStoreMetrics::default()
    }
}

fn commit_pointer(
    control_plane: &FakeControlPlaneClient,
    workspace_id: &WorkspaceId,
    key: &ObjectKey,
    content_id: &str,
    bytes: &[u8],
    hash: String,
) {
    control_plane
        .create_upload_intent(
            UploadIntentRequest::new(
                workspace_id.as_str(),
                ObjectKind::SourcePack,
                bytes.len() as u64,
            )
            .with_object_key(key.as_str())
            .with_content_id(content_id),
        )
        .expect("create upload intent");
    control_plane
        .commit_uploaded_object_metadata(ObjectMetadataCommit {
            workspace_id: workspace_id.clone(),
            object: ObjectPointer {
                object_key: key.as_str().to_string(),
                content_id: ContentId::new(content_id),
                byte_len: bytes.len() as u64,
                hash,
                key_epoch: 1,
                kind: ObjectKind::SourcePack,
                created_at: ControlPlaneTimestamp { tick: 99 },
            },
            committed_by_device_id: DeviceId::new("device-a"),
        })
        .expect("commit uploaded metadata");
}

#[test]
fn sealed_upload_source_ignores_backing_path_replacement_before_and_during_reads() {
    let bytes = b"immutable stream source".to_vec();
    let key = object_key(12);
    let (builder, mut writer) = UploadPackSpoolBuilder::create().expect("spool creates");
    writer.write_all(&bytes).expect("spool writes");
    writer.sync_all().expect("spool syncs");
    drop(writer);
    let spool = builder
        .seal(&key, bytes.len() as u64, &stable_hash(&bytes))
        .expect("spool seals");

    let mut expected = Vec::new();
    spool
        .reader()
        .expect("sealed source reader")
        .read_to_end(&mut expected)
        .expect("read sealed source");
    fs::write(spool.original_path(), vec![0xa5; expected.len()])
        .expect("replace removed backing path");

    let mut first_reader = spool.reader().expect("first reader");
    assert_eq!(
        first_reader.read(&mut []).expect("empty read before EOF"),
        0
    );
    let mut first_prefix = [0_u8; 7];
    first_reader
        .read_exact(&mut first_prefix)
        .expect("read first prefix");
    fs::write(spool.original_path(), vec![0x5a; expected.len()]).expect("replace path during read");
    let mut first = first_prefix.to_vec();
    first_reader
        .read_to_end(&mut first)
        .expect("finish first read");
    assert_eq!(first_reader.read(&mut []).expect("empty read at EOF"), 0);
    assert_eq!(
        first_reader
            .read(&mut [0_u8; 1])
            .expect("non-empty read at EOF"),
        0
    );
    let mut retry = Vec::new();
    spool
        .reader()
        .expect("retry reader")
        .read_to_end(&mut retry)
        .expect("read retry");

    assert_eq!(first, expected);
    assert_eq!(retry, expected);
    assert_eq!(stable_hash(&first), stable_hash(&bytes));
    fs::remove_file(spool.original_path()).expect("remove test replacement");
}

#[test]
fn put_or_read_existing_writes_and_reuses_matching_object() {
    let root = temp_root("reuse");
    let store = LocalByteStore::open_deterministic(&root, 7).expect("open byte store");
    let key = object_key(1);
    let device_id = DeviceId::new("device-a");
    let bytes = b"hello source pack";

    let first = put_or_read_existing(
        &store,
        key.clone(),
        StorageObjectKind::SourcePack,
        "pk_1",
        bytes,
        1,
        Some(&device_id),
    )
    .expect("write object");
    let second = put_or_read_existing(
        &store,
        key.clone(),
        StorageObjectKind::SourcePack,
        "pk_1",
        bytes,
        1,
        Some(&device_id),
    )
    .expect("read existing object");

    assert!(first.wrote_object);
    assert!(!second.wrote_object);
    assert_eq!(first.metadata, second.metadata);
    assert_eq!(second.metadata.key, key);
    assert_eq!(second.metadata.kind, StorageObjectKind::SourcePack);
    assert_eq!(second.metadata.created_by_device_id, Some(device_id));

    fs::remove_dir_all(root).expect("remove test root");
}

#[test]
fn ensure_uploaded_object_creates_single_intent_for_reserving_store() {
    let workspace_id = WorkspaceId::new("ws_single_intent");
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace(workspace_id.as_str());
    let store = ReservingByteStore {
        control_plane: &control_plane,
        workspace_id: workspace_id.as_str(),
    };
    let key = object_key(6);
    let bytes = b"single intent bytes";

    ensure_uploaded_object(
        &control_plane,
        &store,
        UploadObjectRequest {
            workspace_id: &workspace_id,
            storage_kind: StorageObjectKind::SourcePack,
            key: key.clone(),
            content_id: "pk_single_intent",
            bytes,
            key_epoch: 1,
            device_id: None,
            reusable_snapshot_manifest: None,
        },
    )
    .expect("upload object");

    let requests = control_plane.upload_intent_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].workspace_id, workspace_id);
    assert_eq!(requests[0].object_kind, ObjectKind::SourcePack);
    assert_eq!(requests[0].object_key.as_deref(), Some(key.as_str()));
}

#[test]
fn validate_uploaded_metadata_accepts_exact_deterministic_metadata() {
    let key = object_key(2);
    let bytes = b"manifest bytes";
    let metadata = metadata(key.clone(), StorageObjectKind::SnapshotManifest, bytes, 3);

    validate_uploaded_metadata(
        &metadata,
        &key,
        StorageObjectKind::SnapshotManifest,
        bytes,
        3,
    )
    .expect("metadata matches deterministic upload contract");
}

#[test]
fn validate_uploaded_metadata_rejects_mismatched_metadata() {
    let key = object_key(3);
    let bytes = b"source pack bytes";
    let mut metadata = metadata(key.clone(), StorageObjectKind::SourcePack, bytes, 1);
    metadata.hash = stable_hash(b"different bytes");

    let error =
        validate_uploaded_metadata(&metadata, &key, StorageObjectKind::SourcePack, bytes, 1)
            .expect_err("metadata hash mismatch must be rejected");

    assert!(matches!(
        error,
        UploadError::ControlPlane(ControlPlaneError::Conflict {
            resource: "object metadata",
            ..
        })
    ));
}

#[test]
fn ensure_uploaded_object_reuses_committed_control_plane_metadata() {
    let workspace_id = WorkspaceId::new("ws_upload");
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace(workspace_id.as_str());
    let root = temp_root("committed");
    let store = LocalByteStore::open_deterministic(&root, 11).expect("open byte store");
    let key = object_key(4);
    let bytes = b"already uploaded bytes";
    commit_pointer(
        &control_plane,
        &workspace_id,
        &key,
        "pk_committed",
        bytes,
        stable_hash(bytes),
    );

    let upload = ensure_uploaded_object(
        &control_plane,
        &store,
        UploadObjectRequest {
            workspace_id: &workspace_id,
            storage_kind: StorageObjectKind::SourcePack,
            key: key.clone(),
            content_id: "pk_committed",
            bytes,
            key_epoch: 1,
            device_id: None,
            reusable_snapshot_manifest: None,
        },
    )
    .expect("committed metadata is reusable");

    assert!(!upload.wrote_object);
    let metadata = upload.metadata;
    assert_eq!(metadata.key, key);
    assert!(matches!(
        store.head_object(&metadata.key),
        Err(ByteStoreError::MissingObject { .. })
    ));

    fs::remove_dir_all(root).expect("remove test root");
}

#[test]
fn ensure_uploaded_object_rejects_committed_metadata_mismatch() {
    let workspace_id = WorkspaceId::new("ws_mismatch");
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace(workspace_id.as_str());
    let root = temp_root("mismatch");
    let store = LocalByteStore::open_deterministic(&root, 13).expect("open byte store");
    let key = object_key(5);
    let bytes = b"expected bytes";
    commit_pointer(
        &control_plane,
        &workspace_id,
        &key,
        "pk_mismatch",
        bytes,
        stable_hash(b"not the expected bytes"),
    );

    let error = ensure_uploaded_object(
        &control_plane,
        &store,
        UploadObjectRequest {
            workspace_id: &workspace_id,
            storage_kind: StorageObjectKind::SourcePack,
            key,
            content_id: "pk_mismatch",
            bytes,
            key_epoch: 1,
            device_id: None,
            reusable_snapshot_manifest: None,
        },
    )
    .expect_err("control-plane metadata mismatch must fail before local upload");

    assert!(matches!(
        error,
        UploadError::ControlPlane(ControlPlaneError::Conflict {
            resource: "object metadata",
            ..
        })
    ));

    fs::remove_dir_all(root).expect("remove test root");
}
