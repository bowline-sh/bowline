use super::*;

pub(super) fn temp_root(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is after epoch")
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "bowline-upload-reuse-test-{name}-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&root).expect("create test root");
    root
}

pub(super) fn stable_hash(bytes: &[u8]) -> String {
    format!("b3_{}", blake3::hash(bytes).to_hex())
}

pub(super) fn deterministic_bytes(len: usize) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(len);
    let mut counter = 0_u64;
    while bytes.len() < len {
        let digest = blake3::hash(&counter.to_le_bytes());
        let remaining = len - bytes.len();
        bytes.extend_from_slice(&digest.as_bytes()[..remaining.min(digest.as_bytes().len())]);
        counter += 1;
    }
    bytes
}

pub(super) fn commit_pointer(
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

pub(super) fn file_entry(path: &str, content_id: ContentId) -> NamespaceEntry {
    NamespaceEntry {
        path: path.to_string(),
        kind: NamespaceEntryKind::File,
        classification: PathClassification::WorkspaceSync,
        mode: MaterializationMode::EncryptedSync,
        access: vec![AccessFlag::HumanReadable],
        content_id: Some(content_id),
        content_layout: None,
        symlink_target: None,
        byte_len: Some(8),
        executability: bowline_core::workspace_graph::FileExecutability::Regular,
        hydration_state: HydrationState::Local,
    }
}

pub(super) fn file_entry_with_locator(
    path: &str,
    content_id: ContentId,
    locator: ContentLocator,
) -> NamespaceEntry {
    NamespaceEntry {
        content_layout: Some(ContentLayout::single_segment(locator).expect("test layout")),
        hydration_state: HydrationState::Local,
        ..file_entry(path, content_id)
    }
}

pub(super) fn manifest_for_entries(entries: Vec<NamespaceEntry>) -> SnapshotDraft {
    let workspace_id = WorkspaceId::new("ws_upload");
    let snapshot_id = rebuild_manifest_identity(&workspace_id, &entries, "test").snapshot_id;
    SnapshotDraft {
        schema_version: bowline_core::workspace_graph::SNAPSHOT_SCHEMA_VERSION,
        snapshot_id: snapshot_id.clone(),
        workspace_id,
        project_id: None,
        kind: SnapshotKind::WorkspaceHead,
        base_snapshot_id: None,
        entries,
        refs: vec![WorkspaceRef {
            name: "workspace".to_string(),
            target_snapshot_id: snapshot_id,
            kind: RefKind::Workspace,
        }],
    }
}

pub(super) fn candidate_with_manifest(
    workspace_id: WorkspaceId,
    mut manifest: SnapshotDraft,
    files: BTreeMap<ContentId, Vec<u8>>,
) -> SnapshotCandidate {
    manifest.workspace_id = workspace_id.clone();
    for entry in &mut manifest.entries {
        let Some(content_id) = entry.content_id.as_ref() else {
            continue;
        };
        let Some(bytes) = files.get(content_id) else {
            continue;
        };
        entry.byte_len = Some(bytes.len() as u64);
        if let Some(layout) = entry.content_layout.as_ref()
            && let Some(segment) = layout.segments().first()
        {
            entry.content_layout = Some(
                ContentLayout::single_segment(ContentLocator {
                    content_id: content_id.clone(),
                    storage: ContentStorage::Packed,
                    raw_size: bytes.len() as u64,
                    pack_id: Some(segment.pack_id.clone()),
                    offset: Some(segment.offset),
                    length: Some(bytes.len() as u64),
                })
                .expect("test layout matches prepared bytes"),
            );
        }
    }
    let manifest_identity = rebuild_manifest_identity(
        &manifest.workspace_id,
        &manifest.entries,
        "2026-07-03T10:00:00Z",
    );
    manifest.snapshot_id = manifest_identity.snapshot_id.clone();
    for reference in &mut manifest.refs {
        reference.target_snapshot_id = manifest.snapshot_id.clone();
    }
    SnapshotCandidate {
        base: CandidateBase {
            workspace_id,
            version: 0,
            snapshot_id: SnapshotId::new("empty"),
        },
        device_id: DeviceId::new("device-a"),
        manifest_id: manifest_id_for_snapshot(&manifest.snapshot_id),
        snapshot: SnapshotContent::new(manifest, files, [9; 32])
            .expect("page-backed upload snapshot"),
        scan_report: ScanReport {
            root: PathBuf::new(),
            projects: Vec::new(),
            paths: Vec::new(),
            summary: Default::default(),
        },
        scan_scope: ScanScope::Full(FullScanReason::CliRequested),
        stat_cache_hit_paths: BTreeSet::new(),
        stat_cache_divergences: Vec::new(),
        scan_stats: Default::default(),
        manifest_identity,
        stat_cache_write_back: None,
        causation_ids: Vec::new(),
        skipped_unsafe_symlinks: BTreeSet::new(),
        created_at: "2026-07-03T10:00:00Z".to_string(),
    }
}

pub(super) fn locator(content_id: ContentId, pack_id: PackId, offset: u64) -> ContentLocator {
    ContentLocator {
        content_id,
        storage: ContentStorage::Packed,
        raw_size: 8,
        pack_id: Some(pack_id),
        offset: Some(offset),
        length: Some(8),
    }
}
