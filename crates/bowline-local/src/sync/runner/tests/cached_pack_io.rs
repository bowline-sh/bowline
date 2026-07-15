use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use bowline_control_plane::{
    ControlPlaneTimestamp, ObjectKind as ControlPlaneObjectKind, ObjectPointer,
};
use bowline_core::{
    ids::{ContentId, PackId, WorkspaceId},
    policy::{AccessFlag, MaterializationMode, PathClassification},
    workspace_graph::{
        ContentLayout, FileExecutability, HydrationState, NamespaceEntry, NamespaceEntryKind,
        SNAPSHOT_SCHEMA_VERSION, SnapshotDraft, SnapshotKind, workspace_content_id,
    },
};
use bowline_storage::{
    ByteRange, ByteStore, ByteStoreError, ByteStoreMetrics, CachedPackIoObserver,
    CachedPackReleaseState, LocalByteStore, LocalContentCache, ObjectKey, ObjectKind,
    ObjectMetadata, PackRecordInput, PackWriteOutput, PackWriter, StorageKey,
};

use super::*;
use crate::sync::rebuild_manifest_identity;

#[derive(Debug, Clone, PartialEq, Eq)]
enum IoEvent {
    Opened(String),
    Read(String, u64),
    Closed(String, bool),
    RemoteFull(String, u64),
    RemoteRange(String, ByteRange),
}

#[derive(Debug, Default)]
struct IoProbe {
    events: Mutex<Vec<IoEvent>>,
    release_states: Mutex<BTreeMap<String, Arc<CachedPackReleaseState>>>,
}

impl IoProbe {
    fn push(&self, event: IoEvent) {
        self.events.lock().expect("events lock").push(event);
    }

    fn events(&self) -> Vec<IoEvent> {
        self.events.lock().expect("events lock").clone()
    }

    fn release_state_for(&self, key: &ObjectKey) -> Arc<CachedPackReleaseState> {
        Arc::clone(
            self.release_states
                .lock()
                .expect("release states lock")
                .get(key.as_str())
                .expect("release state exists after open"),
        )
    }
}

impl CachedPackIoObserver for IoProbe {
    fn opened(&self, key: &ObjectKey) {
        self.push(IoEvent::Opened(key.as_str().to_string()));
    }

    fn read(&self, key: &ObjectKey, byte_len: u64) {
        self.push(IoEvent::Read(key.as_str().to_string(), byte_len));
    }

    fn closed(&self, key: &ObjectKey) {
        let released = self.release_state_for(key).is_released();
        self.push(IoEvent::Closed(key.as_str().to_string(), released));
    }

    fn release_state(&self, key: &ObjectKey) -> Option<Arc<CachedPackReleaseState>> {
        let state = Arc::new(CachedPackReleaseState::default());
        self.release_states
            .lock()
            .expect("release states lock")
            .insert(key.as_str().to_string(), Arc::clone(&state));
        Some(state)
    }
}

struct RecordingStore {
    inner: LocalByteStore,
    probe: Arc<IoProbe>,
}

impl ByteStore for RecordingStore {
    fn put_object(
        &self,
        key: ObjectKey,
        kind: ObjectKind,
        bytes: &[u8],
        created_by_device_id: Option<&bowline_core::ids::DeviceId>,
    ) -> Result<ObjectMetadata, ByteStoreError> {
        self.inner
            .put_object(key, kind, bytes, created_by_device_id)
    }

    fn get_object(&self, key: &ObjectKey) -> Result<Vec<u8>, ByteStoreError> {
        let bytes = self.inner.get_object(key)?;
        self.probe.push(IoEvent::RemoteFull(
            key.as_str().to_string(),
            bytes.len() as u64,
        ));
        Ok(bytes)
    }

    fn get_range(&self, key: &ObjectKey, range: ByteRange) -> Result<Vec<u8>, ByteStoreError> {
        self.probe
            .push(IoEvent::RemoteRange(key.as_str().to_string(), range));
        self.inner.get_range(key, range)
    }

    fn head_object(&self, key: &ObjectKey) -> Result<ObjectMetadata, ByteStoreError> {
        self.inner.head_object(key)
    }

    fn metrics(&self) -> ByteStoreMetrics {
        self.inner.metrics()
    }
}

#[test]
fn release_state_is_false_while_file_wrapper_lives_and_true_before_closed() {
    let harness = Harness::new("release-order");
    let pack = harness.pack("pk_dddddddddddddddd", &[b"record".to_vec()]);
    harness.cache_pack(&pack, None);

    let reader = harness
        .cache
        .open_cached_pack(&pack.object_key)
        .expect("reader opens");
    let release_state = harness.probe.release_state_for(&pack.object_key);
    assert!(
        !release_state.is_released(),
        "negative control while File is live"
    );

    drop(reader);

    assert!(
        release_state.is_released(),
        "wrapper marks release after dropping File"
    );
    assert!(matches!(
        harness.probe.events().last(),
        Some(IoEvent::Closed(_, true))
    ));
}

#[test]
fn imported_hydration_opens_one_pack_once_for_one_hundred_exact_reads() {
    let harness = Harness::new("one-pack");
    let records = (0_u16..100)
        .map(|index| format!("record-{index:03}").into_bytes())
        .collect::<Vec<_>>();
    let pack = harness.pack("pk_1000000000000001", &records);
    harness.store_pack(&pack);
    harness.cache_pack(&pack, None);
    let manifest = harness.manifest(&[(&pack, 0, records.len(), false)]);
    let expected_bytes: u64 = pack
        .locators
        .iter()
        .map(|locator| locator.length.expect("length"))
        .sum();

    harness.hydrate(
        manifest,
        &[pointer(&pack)],
        ImportedHydrationSelection::AllFiles,
    );

    let events = harness.probe.events();
    assert_eq!(count_opened(&events), 1);
    assert_eq!(count_reads(&events), 100);
    assert_eq!(read_bytes(&events), expected_bytes);
    assert_eq!(max_open_handles(&events), 1);
}

#[test]
fn pack_plans_have_exact_request_range_and_download_metrics() {
    let cases: &[(&str, Vec<usize>, usize, usize)] = &[
        ("full-100", (0..100).collect(), 1, 0),
        ("adjacent-90", (0..90).collect(), 1, 1),
        (
            "gapped-90",
            (0..100).filter(|index| index % 10 != 9).collect(),
            10,
            10,
        ),
        ("adjacent-10", (0..10).collect(), 1, 1),
        ("sparse-10", (0..100).step_by(10).collect(), 10, 10),
    ];
    for (name, selected, expected_requests, expected_ranges) in cases {
        let harness = Harness::new(name);
        let records = (0_u16..100)
            .map(|index| format!("record-{index:03}").into_bytes())
            .collect::<Vec<_>>();
        let pack = harness.pack("pk_2000000000000002", &records);
        harness.store_pack(&pack);
        let manifest = harness.manifest_for_indices(&pack, selected);

        let hydrated = harness.hydrate(
            manifest,
            &[pointer(&pack)],
            ImportedHydrationSelection::RequiredFiles,
        );

        for index in selected {
            assert_eq!(
                hydrated
                    .read_file_for_path(&entry_path(&pack, *index))
                    .expect("hydrated file reads"),
                Some(records[*index].clone()),
                "{name} record {index}"
            );
        }
        let events = harness.probe.events();
        let actual_ranges = events
            .iter()
            .filter(|event| matches!(event, IoEvent::RemoteRange(_, _)))
            .count();
        let requests = actual_ranges
            + events
                .iter()
                .filter(|event| matches!(event, IoEvent::RemoteFull(_, _)))
                .count();
        let downloaded_bytes = remote_download_bytes(&events);
        let expected_bytes = if *name == "full-100" {
            pack.bytes.len() as u64
        } else {
            selected
                .iter()
                .map(|index| pack.locators[*index].length.expect("length"))
                .sum()
        };
        eprintln!(
            "{name}: actions={requests} http_requests={requests} ranges={actual_ranges} downloaded_bytes={downloaded_bytes}"
        );
        assert_eq!(requests, *expected_requests, "{name} HTTP/actions");
        assert_eq!(actual_ranges, *expected_ranges, "{name} ranges");
        assert_eq!(downloaded_bytes, expected_bytes, "{name} bytes");
    }
}

#[test]
fn conflicting_cached_locators_abort_before_remote_io_or_local_result() {
    let harness = Harness::new("conflicting-cached-locators");
    let pack = harness.pack(
        "pk_3000000000000003",
        &[b"same-content-id".to_vec(), b"different-range".to_vec()],
    );
    harness.store_pack(&pack);
    harness
        .cache
        .put_content(&pack.locators[0].content_id, b"same-content-id")
        .expect("cached content");
    let mut manifest = harness.manifest(&[(&pack, 0, 2, false)]);
    let ambiguous_id = pack.locators[0].content_id.clone();
    let mut ambiguous_locator = pack.locators[1].clone();
    ambiguous_locator.content_id = ambiguous_id;
    manifest.mutate_entries_for_test(|entries| {
        entries[1].content_id = Some(pack.locators[0].content_id.clone());
        entries[1].content_layout =
            Some(ContentLayout::single_segment(ambiguous_locator).expect("single-segment layout"));
    });

    assert!(matches!(
        harness.hydrate_result(
            manifest,
            &[pointer(&pack)],
            ImportedHydrationSelection::AllFiles,
        ),
        Err(SyncRunnerError::InvalidImportedSnapshot(
            bowline_storage::HydrationPlanError::ConflictingContentLocator { .. }
        ))
    ));
    assert!(harness.probe.events().iter().all(|event| !matches!(
        event,
        IoEvent::RemoteFull(_, _) | IoEvent::RemoteRange(_, _)
    )));
}

#[test]
fn imported_hydration_processes_pack_groups_deterministically_one_at_a_time() {
    let harness = Harness::new("many-packs");
    let pack_b = harness.pack(
        "pk_bbbbbbbbbbbbbbbb",
        &[b"b-one".to_vec(), b"b-two".to_vec()],
    );
    let pack_a = harness.pack(
        "pk_aaaaaaaaaaaaaaaa",
        &[b"a-one".to_vec(), b"a-two".to_vec()],
    );
    for pack in [&pack_b, &pack_a] {
        harness.store_pack(pack);
        harness.cache_pack(pack, None);
    }
    let manifest = harness.manifest(&[(&pack_b, 0, 2, false), (&pack_a, 0, 2, false)]);

    harness.hydrate(
        manifest,
        &[pointer(&pack_b), pointer(&pack_a)],
        ImportedHydrationSelection::AllFiles,
    );

    let events = harness.probe.events();
    let lifecycle = events
        .iter()
        .filter_map(|event| match event {
            IoEvent::Opened(key) => Some(("open", key.clone())),
            IoEvent::Closed(key, released) => {
                assert!(*released, "OS handle is released before close observation");
                Some(("close", key.clone()))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let key_a = pack_a.object_key.as_str().to_string();
    let key_b = pack_b.object_key.as_str().to_string();
    assert_eq!(
        lifecycle,
        vec![
            ("open", key_a.clone()),
            ("close", key_a),
            ("open", key_b.clone()),
            ("close", key_b),
        ]
    );
    assert_eq!(max_open_handles(&events), 1);
}

#[test]
fn corrupt_middle_record_closes_then_retries_and_keeps_later_records_remote() {
    let harness = Harness::new("corrupt-middle");
    let pack = harness.pack(
        "pk_cccccccccccccccc",
        &[
            b"first".to_vec(),
            b"middle".to_vec(),
            b"later".to_vec(),
            b"lazy".to_vec(),
        ],
    );
    harness.store_pack(&pack);
    let mut corrupted = pack.bytes.clone();
    let middle = &pack.locators[1];
    corrupted[middle.offset.expect("offset") as usize] ^= 0xff;
    harness.cache_pack(&pack, Some(&corrupted));
    let manifest = harness.manifest(&[(&pack, 0, 3, false), (&pack, 3, 4, true)]);

    harness.hydrate(
        manifest,
        &[pointer(&pack)],
        ImportedHydrationSelection::RequiredFiles,
    );

    let events = harness.probe.events();
    assert_eq!(count_opened(&events), 1, "corrupt cache must never reopen");
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, IoEvent::RemoteRange(_, _)))
            .count(),
        2
    );
    let close_index = events
        .iter()
        .position(|event| matches!(event, IoEvent::Closed(_, true)))
        .expect("closed");
    let first_remote = events
        .iter()
        .position(|event| matches!(event, IoEvent::RemoteRange(_, _)))
        .expect("remote");
    assert!(
        close_index < first_remote,
        "handle closes before remote fallback and eviction"
    );
    assert_eq!(max_open_handles(&events), 1);
}

struct Harness {
    runner: SyncRunner<'static>,
    cache: LocalContentCache,
    store: &'static RecordingStore,
    probe: Arc<IoProbe>,
    workspace_id: WorkspaceId,
}

impl Harness {
    fn new(name: &str) -> Self {
        let workspace = Box::leak(Box::new(
            TempWorkspace::new(&format!("pack-io-work-{name}")).expect("workspace"),
        ));
        let state = Box::leak(Box::new(
            TempWorkspace::new(&format!("pack-io-state-{name}")).expect("state"),
        ));
        let probe = Arc::new(IoProbe::default());
        let store = Box::leak(Box::new(RecordingStore {
            inner: LocalByteStore::open(state.root().join("objects")).expect("store"),
            probe: Arc::clone(&probe),
        }));
        let workspace_id = WorkspaceId::new(format!("ws_{name}"));
        let control_plane = Box::leak(Box::new(
            bowline_control_plane::FakeControlPlaneClient::default(),
        ));
        let runner = SyncRunner::new(
            control_plane,
            store,
            SyncRunnerOptions {
                root: workspace.root().to_path_buf(),
                state_root: state.root().to_path_buf(),
                workspace_id: workspace_id.clone(),
                device_id: DeviceId::new("device_local"),
                workspace_content_key: [7_u8; 32],
                storage_key: StorageKey::from_bytes([8_u8; 32]),
                key_epoch: 1,
                generated_at: "2026-07-12T10:00:00Z".to_string(),
                sync_claim: None,
                scan_scope: Default::default(),
            },
        );
        let cache =
            LocalContentCache::open_with_io_observer(state.root().join("cache"), probe.clone())
                .expect("cache");
        Self {
            runner,
            cache,
            store,
            probe,
            workspace_id,
        }
    }

    fn pack(&self, pack_id: &str, records: &[Vec<u8>]) -> PackWriteOutput {
        let inputs = records
            .iter()
            .map(|bytes| PackRecordInput {
                content_id: workspace_content_id([7_u8; 32], bytes),
                bytes: bytes.clone(),
            })
            .collect::<Vec<_>>();
        PackWriter::new(
            self.workspace_id.clone(),
            PackId::new(pack_id),
            StorageKey::from_bytes([8_u8; 32]),
            1,
        )
        .write(&inputs)
        .expect("pack")
    }

    fn store_pack(&self, pack: &PackWriteOutput) {
        self.store
            .put_object(
                pack.object_key.clone(),
                ObjectKind::SourcePack,
                &pack.bytes,
                None,
            )
            .expect("stored");
    }

    fn cache_pack(&self, pack: &PackWriteOutput, replacement: Option<&[u8]>) {
        self.cache
            .put_pack(&pack.object_key, replacement.unwrap_or(&pack.bytes))
            .expect("cached");
    }

    fn manifest(&self, groups: &[(&PackWriteOutput, usize, usize, bool)]) -> SnapshotContent {
        let mut entries = Vec::new();
        for (pack, start, end, lazy) in groups {
            for (index, locator) in pack.locators[*start..*end].iter().enumerate() {
                entries.push(NamespaceEntry {
                    path: format!("files/{}/{}", pack.object_key.as_str(), start + index),
                    kind: NamespaceEntryKind::File,
                    classification: PathClassification::WorkspaceSync,
                    mode: if *lazy {
                        MaterializationMode::StructureOnly
                    } else {
                        MaterializationMode::WorkspaceSync
                    },
                    access: vec![AccessFlag::HumanReadable, AccessFlag::AgentReadable],
                    content_id: Some(locator.content_id.clone()),
                    content_layout: Some(
                        ContentLayout::single_segment(locator.clone())
                            .expect("single-segment layout"),
                    ),
                    symlink_target: None,
                    byte_len: Some(locator.raw_size),
                    executability: FileExecutability::Regular,
                    hydration_state: HydrationState::Cold,
                });
            }
        }
        let snapshot_id =
            rebuild_manifest_identity(&self.workspace_id, &entries, "test").snapshot_id;
        SnapshotContent::new(
            SnapshotDraft {
                schema_version: SNAPSHOT_SCHEMA_VERSION,
                snapshot_id,
                workspace_id: self.workspace_id.clone(),
                project_id: None,
                kind: SnapshotKind::WorkspaceHead,
                base_snapshot_id: None,
                entries,
                refs: Vec::new(),
            },
            BTreeMap::new(),
            [7; 32],
        )
        .expect("page-backed pack I/O snapshot")
    }

    fn manifest_for_indices(&self, pack: &PackWriteOutput, selected: &[usize]) -> SnapshotContent {
        let selected = selected
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        let groups = pack
            .locators
            .iter()
            .enumerate()
            .map(|(index, _)| (pack, index, index + 1, !selected.contains(&index)))
            .collect::<Vec<_>>();
        self.manifest(&groups)
    }

    fn hydrate(
        &self,
        manifest: SnapshotContent,
        pointers: &[ObjectPointer],
        selection: ImportedHydrationSelection,
    ) -> SnapshotContent {
        self.hydrate_result(manifest, pointers, selection)
            .expect("hydrate")
    }

    fn hydrate_result(
        &self,
        manifest: SnapshotContent,
        pointers: &[ObjectPointer],
        selection: ImportedHydrationSelection,
    ) -> Result<SnapshotContent, SyncRunnerError> {
        self.runner
            .hydrate_imported_snapshot_with_cache(manifest, pointers, selection, &self.cache)
    }
}

fn entry_path(pack: &PackWriteOutput, index: usize) -> String {
    format!("files/{}/{index}", pack.object_key.as_str())
}

fn remote_download_bytes(events: &[IoEvent]) -> u64 {
    events
        .iter()
        .map(|event| match event {
            IoEvent::RemoteFull(_, byte_len) => *byte_len,
            IoEvent::RemoteRange(_, range) => range.length,
            _ => 0,
        })
        .sum()
}

fn pointer(pack: &PackWriteOutput) -> ObjectPointer {
    ObjectPointer {
        object_key: pack.object_key.as_str().to_string(),
        content_id: ContentId::new("cid_pack_pointer"),
        byte_len: pack.bytes.len() as u64,
        hash: "blake3:test".to_string(),
        key_epoch: 1,
        kind: ControlPlaneObjectKind::SourcePack,
        created_at: ControlPlaneTimestamp { tick: 1 },
    }
}

fn count_opened(events: &[IoEvent]) -> usize {
    events
        .iter()
        .filter(|event| matches!(event, IoEvent::Opened(_)))
        .count()
}
fn count_reads(events: &[IoEvent]) -> usize {
    events
        .iter()
        .filter(|event| matches!(event, IoEvent::Read(_, _)))
        .count()
}
fn read_bytes(events: &[IoEvent]) -> u64 {
    events
        .iter()
        .filter_map(|event| match event {
            IoEvent::Read(_, bytes) => Some(*bytes),
            _ => None,
        })
        .sum()
}

fn max_open_handles(events: &[IoEvent]) -> usize {
    let mut open = 0_usize;
    let mut maximum = 0_usize;
    for event in events {
        match event {
            IoEvent::Opened(_) => {
                open += 1;
                maximum = maximum.max(open);
            }
            IoEvent::Closed(_, released) => {
                assert!(*released, "OS handle is released before close observation");
                open = open.checked_sub(1).expect("close follows open");
            }
            _ => {}
        }
    }
    assert_eq!(open, 0, "all handles close");
    maximum
}
