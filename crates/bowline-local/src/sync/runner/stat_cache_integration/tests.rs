use super::*;
use bowline_core::{
    ids::PackId,
    policy::{AccessFlag, PathClassification},
    workspace_graph::{
        ContentLayout, ContentLocator, ContentStorage, FileExecutability, RefKind, SnapshotDraft,
    },
};

use crate::scanner::ScanReport;
use crate::sync::stat_cache::{
    ContentKeyFingerprint, FileTimestampNanos, STAT_CACHE_FORMAT_VERSION, StatCacheRow,
    StatFingerprint, VERIFY_SHARD_COUNT, verify_shard_for_path,
};
use crate::workspace::TempWorkspace;

const KEY: [u8; 32] = [7; 32];

mod scope_tests;

#[test]
fn root_shallow_degrades_when_local_head_absent() {
    let harness = Harness::with_operation("effective-root-shallow-no-head", ScanScope::RootShallow);

    let effective = harness
        .runner
        .effective_scan_scope(None, None)
        .expect("scope");

    assert_eq!(
        effective,
        ScanScope::Full(FullScanReason::HeadManifestUnavailable)
    );
    assert!(harness.degraded_with_reason("head-manifest-unavailable"));
}

#[test]
fn root_shallow_does_not_collect_head_entries_as_preservation_exceptions() {
    let harness = Harness::new("preserved-root-shallow", ScanScope::RootShallow);
    harness.seed_head(vec![
        file_entry("README.md", false),
        file_entry("app/x.rs", true),
    ]);

    let preserved = harness
        .runner
        .preserved_exception_entries(&empty_base_ref(), &BTreeSet::new())
        .expect("preserved");

    assert!(preserved.is_empty());
}

#[test]
fn combined_scope_does_not_collect_unowned_head_entries() {
    let roots = BTreeSet::from(["src".to_string()]);
    let scope = ScanScope::DirtySubtrees {
        roots: roots.clone(),
        root_shallow: true,
    };
    let harness = Harness::new("preserved-combined", scope.clone());
    harness.seed_head(vec![
        file_entry("README.md", false),
        file_entry("src/a.rs", true),
        file_entry("docs/b.md", true),
    ]);

    let preserved = harness
        .runner
        .preserved_exception_entries(&empty_base_ref(), &BTreeSet::new())
        .expect("preserved");

    assert!(preserved.is_empty());
}

#[test]
fn load_stat_cache_session_routes_root_shallow_to_root_level_loader() {
    let harness = Harness::new("loader-routing", ScanScope::RootShallow);
    harness.seed_stat_cache(&[
        cache_row("README.md"),
        cache_row("Cargo.toml"),
        cache_row("app/src/main.rs"),
        cache_row("app/src/lib.rs"),
    ]);

    let session = harness
        .runner
        .load_stat_cache_session(&ScanScope::RootShallow)
        .expect("session");

    // Only the two root-level rows load; the deep rows are never consulted.
    assert_eq!(session.loaded_row_count(), 2);
}

#[test]
fn upload_scope_reads_only_in_shard_bound_hits_and_all_unbound_entries() {
    let harness = Harness::new(
        "fill-upload-sampled",
        ScanScope::Full(FullScanReason::CliRequested),
    );
    let path = "src/cache-hit.txt";
    let bytes = b"cached bytes";
    write_workspace_file(&harness, path, bytes);
    let content_id = workspace_content_id(KEY, bytes);
    let path_shard = verify_shard_for_path(path);
    let other_shard = (path_shard + 1) % VERIFY_SHARD_COUNT;

    let mut out_of_shard = fill_candidate(path, content_id.clone(), bytes.len(), true, true);
    harness
        .runner
        .fill_candidate_bytes(
            &mut out_of_shard,
            FillBytesScope::UploadShardSampled {
                verify_shard: other_shard,
            },
        )
        .expect("out-of-shard fill");
    assert!(
        !out_of_shard
            .snapshot
            .prepared_content()
            .contains_key(&content_id),
        "out-of-shard bound hit should not be read"
    );

    let mut in_shard = fill_candidate(path, content_id.clone(), bytes.len(), true, true);
    harness
        .runner
        .fill_candidate_bytes(
            &mut in_shard,
            FillBytesScope::UploadShardSampled {
                verify_shard: path_shard,
            },
        )
        .expect("in-shard fill");
    assert_eq!(
        in_shard
            .snapshot
            .read_file_for_path(path)
            .expect("read prepared content")
            .as_deref(),
        Some(bytes.as_slice()),
        "in-shard bound hit should be read"
    );

    let unbound_path = "src/new.txt";
    let unbound_bytes = b"new bytes";
    write_workspace_file(&harness, unbound_path, unbound_bytes);
    let unbound_content_id = workspace_content_id(KEY, unbound_bytes);
    let mut unbound = fill_candidate(
        unbound_path,
        unbound_content_id.clone(),
        unbound_bytes.len(),
        false,
        true,
    );
    harness
        .runner
        .fill_candidate_bytes(
            &mut unbound,
            FillBytesScope::UploadShardSampled {
                verify_shard: other_shard,
            },
        )
        .expect("unbound fill");
    assert_eq!(
        unbound
            .snapshot
            .read_file_for_path(unbound_path)
            .expect("read prepared content")
            .as_deref(),
        Some(unbound_bytes.as_slice()),
        "unbound entries must always be read for packing"
    );
}

#[test]
fn upload_scope_still_reports_divergence_for_in_shard_bound_hit() {
    let harness = Harness::new(
        "fill-upload-divergence",
        ScanScope::Full(FullScanReason::CliRequested),
    );
    let path = "src/diverged.txt";
    write_workspace_file(&harness, path, b"actual bytes");
    let cached_content_id = workspace_content_id(KEY, b"cached bytes");
    let mut candidate = fill_candidate(path, cached_content_id, b"cached bytes".len(), true, true);

    let error = harness
        .runner
        .fill_candidate_bytes(
            &mut candidate,
            FillBytesScope::UploadShardSampled {
                verify_shard: verify_shard_for_path(path),
            },
        )
        .expect_err("divergent sampled hit should fail");

    assert!(
        matches!(error, SyncRunnerError::StatCacheDivergence { path: ref error_path, .. } if error_path == path),
        "unexpected error: {error:?}"
    );
}

struct Harness {
    runner: SyncRunner<'static>,
    state: TempWorkspace,
    workspace_id: WorkspaceId,
    operation_id: Option<String>,
}

impl Harness {
    fn new(label: &str, scope: ScanScope) -> Self {
        Self::build(label, scope, None)
    }

    fn with_operation(label: &str, scope: ScanScope) -> Self {
        Self::build(label, scope, Some(format!("op_{label}")))
    }

    fn build(label: &str, scope: ScanScope, operation_id: Option<String>) -> Self {
        let workspace = TempWorkspace::new(&format!("{label}-ws")).expect("workspace");
        let state = TempWorkspace::new(&format!("{label}-state")).expect("state");
        let workspace_id = WorkspaceId::new("ws_code");
        let store = MetadataStore::open(state.root().join(DEFAULT_DATABASE_FILE)).expect("store");
        store
            .insert_workspace(&workspace_id, "Code", "2026-07-06T00:00:00Z")
            .expect("workspace");
        let sync_claim = if let Some(operation_id) = &operation_id {
            store
                .enqueue_sync_operation(&SyncOperationRecord {
                    id: operation_id.clone(),
                    workspace_id: workspace_id.clone(),
                    kind: SyncOperationKind::Reconcile,
                    resource_key: crate::metadata::SyncResourceKey::workspace_sync(
                        workspace_id.clone(),
                    ),
                    state: SyncOperationState::Queued,
                    idempotency_key: operation_id.clone(),
                    base_version: None,
                    base_snapshot_id: None,
                    target_snapshot_id: None,
                    device_id: Some(DeviceId::new("device_local")),
                    payload_json: "{}".to_string(),
                    attempt_count: 1,
                    claimed_by: None,
                    claim_generation: 0,
                    heartbeat_at: None,
                    lease_expires_at: None,
                    cancellation_requested_at: None,
                    next_attempt_at: None,
                    result_json: None,
                    last_error_code: None,
                    last_error: None,
                    created_at: "2026-07-06T00:00:00Z".to_string(),
                    updated_at: "2026-07-06T00:00:00Z".to_string(),
                })
                .expect("seed sync operation");
            Some(
                store
                    .claim_next_sync_operation(
                        &workspace_id,
                        "stat-cache-harness",
                        "2026-07-06T00:00:01Z",
                        "2999-01-01T00:00:00Z",
                    )
                    .expect("claim sync operation")
                    .expect("queued sync operation")
                    .claim,
            )
        } else {
            None
        };
        drop(store);
        let control_plane = Box::leak(Box::new(
            bowline_control_plane::FakeControlPlaneClient::default(),
        ));
        let byte_store = Box::leak(Box::new(
            bowline_storage::LocalByteStore::open(state.root().join("objects"))
                .expect("byte store"),
        ));
        let runner = SyncRunner::new(
            control_plane,
            byte_store,
            SyncRunnerOptions {
                root: workspace.root().to_path_buf(),
                state_root: state.root().to_path_buf(),
                workspace_id: workspace_id.clone(),
                device_id: DeviceId::new("device_local"),
                workspace_content_key: KEY,
                storage_key: StorageKey::from_bytes([8_u8; 32]),
                key_epoch: 1,
                generated_at: "2026-07-06T00:01:00Z".to_string(),
                sync_claim,
                scan_scope: scope,
            },
        );
        std::mem::forget(workspace);
        Self {
            runner,
            state,
            workspace_id,
            operation_id,
        }
    }

    fn seed_head(&self, entries: Vec<NamespaceEntry>) -> WorkspaceRef {
        let snapshot_id =
            crate::sync::rebuild_manifest_identity(&self.workspace_id, &entries, "test")
                .snapshot_id;
        let snapshot = SnapshotContent::new(
            SnapshotDraft {
                schema_version: bowline_core::workspace_graph::SNAPSHOT_SCHEMA_VERSION,
                snapshot_id: snapshot_id.clone(),
                workspace_id: self.workspace_id.clone(),
                project_id: None,
                kind: SnapshotKind::WorkspaceHead,
                base_snapshot_id: None,
                entries,
                refs: vec![SnapshotRef {
                    name: "workspace".to_string(),
                    target_snapshot_id: snapshot_id.clone(),
                    kind: RefKind::Workspace,
                }],
            },
            BTreeMap::new(),
            [7; 32],
        )
        .expect("page-backed head snapshot");
        let mut store =
            MetadataStore::open(self.state.root().join(DEFAULT_DATABASE_FILE)).expect("store");
        crate::page_test_support::persist_cached_snapshot(
            &mut store,
            &snapshot,
            &self.state.root().join("stat-cache-pages"),
            "2026-07-06T00:00:30Z",
        );
        drop(store);
        WorkspaceRef {
            workspace_id: WorkspaceId::new(self.workspace_id.as_str()),
            version: 1,
            snapshot_id: SnapshotId::new(snapshot_id.as_str()),
            updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 1 },
            updated_by_device_id: Some(DeviceId::new("device_local")),
        }
    }

    fn seed_stat_cache(&self, rows: &[StatCacheRow]) {
        let mut store =
            MetadataStore::open(self.state.root().join(DEFAULT_DATABASE_FILE)).expect("store");
        store
            .apply_stat_cache_write_back(&self.workspace_id, rows, &BTreeSet::new())
            .expect("seed stat cache");
    }

    fn head_snapshot(&self, head: &WorkspaceRef) -> SnapshotContent {
        let store =
            MetadataStore::open(self.state.root().join(DEFAULT_DATABASE_FILE)).expect("store");
        let record = store
            .snapshot(
                &self.workspace_id,
                &SnapshotId::new(head.snapshot_id.clone()),
            )
            .expect("snapshot query")
            .expect("head snapshot");
        crate::sync::load_cached_snapshot(&store, &record).expect("page-backed head snapshot")
    }

    fn checkpoint_payloads_are_pathless(&self) -> bool {
        let operation_id = self.operation_id.as_deref().expect("operation id set");
        let store =
            MetadataStore::open(self.state.root().join(DEFAULT_DATABASE_FILE)).expect("store");
        store
            .sync_operation_checkpoints(operation_id)
            .expect("checkpoints")
            .iter()
            .all(|checkpoint| {
                !checkpoint.payload_json.contains('/')
                    && !checkpoint.payload_json.contains(".env")
                    && !checkpoint.payload_json.contains("main.rs")
            })
    }

    fn degraded_with_reason(&self, reason: &str) -> bool {
        let operation_id = self.operation_id.as_deref().expect("operation id set");
        let store =
            MetadataStore::open(self.state.root().join(DEFAULT_DATABASE_FILE)).expect("store");
        store
            .sync_operation_checkpoints(operation_id)
            .expect("checkpoints")
            .iter()
            .any(|checkpoint| {
                checkpoint.step == "scoped-scan-degraded"
                    && checkpoint.payload_json.contains(reason)
            })
    }
}

fn empty_base_ref() -> WorkspaceRef {
    WorkspaceRef {
        workspace_id: WorkspaceId::new("ws_code"),
        version: 0,
        snapshot_id: SnapshotId::new(EMPTY_SNAPSHOT_ID),
        updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 0 },
        updated_by_device_id: None,
    }
}

fn file_entry(path: &str, bound: bool) -> NamespaceEntry {
    let content_id = ContentId::new(format!("cid_{}", path.replace('/', "_")));
    let locator = bound.then(|| {
        ContentLayout::single_segment(ContentLocator {
            content_id: content_id.clone(),
            storage: ContentStorage::Packed,
            raw_size: 8,
            pack_id: Some(PackId::new("pk_0011223344556677")),
            offset: Some(0),
            length: Some(8),
        })
        .expect("test layout")
    });
    NamespaceEntry {
        path: path.to_string(),
        kind: NamespaceEntryKind::File,
        classification: PathClassification::WorkspaceSync,
        mode: MaterializationMode::WorkspaceSync,
        access: vec![AccessFlag::HumanReadable],
        content_id: Some(content_id),
        content_layout: locator,
        symlink_target: None,
        byte_len: Some(8),
        executability: FileExecutability::Regular,
        hydration_state: HydrationState::Cold,
    }
}

fn write_workspace_file(harness: &Harness, path: &str, bytes: &[u8]) {
    let absolute = harness.runner.options.root.join(path);
    fs::create_dir_all(absolute.parent().expect("parent")).expect("parent");
    fs::write(absolute, bytes).expect("write workspace file");
}

fn fill_candidate(
    path: &str,
    content_id: ContentId,
    byte_len: usize,
    bound: bool,
    hit: bool,
) -> crate::sync::SnapshotCandidate {
    let locator = bound.then(|| {
        ContentLayout::single_segment(ContentLocator {
            content_id: content_id.clone(),
            storage: ContentStorage::Packed,
            raw_size: byte_len as u64,
            pack_id: Some(PackId::new("pk_0011223344556677")),
            offset: Some(0),
            length: Some(byte_len as u64),
        })
        .expect("test layout")
    });
    let entries = vec![NamespaceEntry {
        path: path.to_string(),
        kind: NamespaceEntryKind::File,
        classification: PathClassification::WorkspaceSync,
        mode: MaterializationMode::WorkspaceSync,
        access: vec![AccessFlag::HumanReadable],
        content_id: Some(content_id),
        content_layout: locator,
        symlink_target: None,
        byte_len: Some(byte_len as u64),
        executability: FileExecutability::Regular,
        hydration_state: HydrationState::Local,
    }];
    let workspace_id = WorkspaceId::new("ws_code");
    let manifest_identity =
        crate::sync::rebuild_manifest_identity(&workspace_id, &entries, "2026-07-06T00:01:00Z");
    let snapshot_id = manifest_identity.snapshot_id.clone();
    let draft = SnapshotDraft {
        schema_version: bowline_core::workspace_graph::SNAPSHOT_SCHEMA_VERSION,
        snapshot_id: snapshot_id.clone(),
        workspace_id,
        project_id: None,
        kind: SnapshotKind::WorkspaceHead,
        base_snapshot_id: None,
        entries,
        refs: vec![SnapshotRef {
            name: "workspace".to_string(),
            target_snapshot_id: snapshot_id.clone(),
            kind: RefKind::Workspace,
        }],
    };
    let mut stat_cache_hit_paths = BTreeSet::new();
    if hit {
        stat_cache_hit_paths.insert(path.to_string());
    }
    crate::sync::SnapshotCandidate {
        base: CandidateBase {
            workspace_id: WorkspaceId::new("ws_code"),
            version: 0,
            snapshot_id: SnapshotId::new(EMPTY_SNAPSHOT_ID),
        },
        device_id: DeviceId::new("device_local"),
        manifest_id: crate::sync::manifest_id_for_snapshot(&snapshot_id),
        snapshot: SnapshotContent::new(draft, BTreeMap::new(), [7; 32])
            .expect("page-backed candidate"),
        scan_report: ScanReport {
            root: PathBuf::new(),
            projects: Vec::new(),
            paths: Vec::new(),
            summary: Default::default(),
        },
        scan_scope: ScanScope::Full(FullScanReason::CliRequested),
        stat_cache_hit_paths,
        stat_cache_divergences: Vec::new(),
        scan_stats: Default::default(),
        manifest_identity,
        stat_cache_write_back: None,
        causation_ids: Vec::new(),
        skipped_unsafe_symlinks: BTreeSet::new(),
        created_at: "2026-07-06T00:01:00Z".to_string(),
    }
}

fn cache_row(path: &str) -> StatCacheRow {
    StatCacheRow {
        path: path.to_string(),
        fingerprint: StatFingerprint {
            size: 8,
            mtime_ns: FileTimestampNanos::new(11),
            ctime_ns: FileTimestampNanos::new(12),
            inode: 13,
            dev: 14,
            file_mode: 0o100644,
        },
        key_epoch: 1,
        content_key_fingerprint: ContentKeyFingerprint::new("0123456789abcdef".to_string()),
        content_id: ContentId::new("cid_seed"),
        byte_len: 8,
        format_version: STAT_CACHE_FORMAT_VERSION,
        hashed_at_ns: FileTimestampNanos::new(20),
        last_verified_at: "2026-07-06T00:00:00Z".to_string(),
    }
}
