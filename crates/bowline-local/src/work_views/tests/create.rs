use super::*;

#[test]
fn work_create_materializes_case_variant_env_owner_only_without_local_regenerate_state() {
    let (temp, db_path) = seeded_store("phase9-work_create");
    let project_path = temp.root().join("Code").join("apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project src");
    fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("source file");
    fs::write(project_path.join(".ENV.Local"), "TOKEN=secret").expect("env file");
    fs::write(project_path.join("id_rsa"), "hidden key").expect("hidden key");
    fs::create_dir_all(project_path.join("node_modules/pkg")).expect("dependency dir");
    fs::write(
        project_path.join("node_modules/pkg/index.js"),
        "generated dependency",
    )
    .expect("dependency file");
    fs::write(
        project_path.join("node_modules/pkg/.env"),
        "NESTED=dependency",
    )
    .expect("nested dependency env");
    fs::create_dir_all(project_path.join("target/debug")).expect("build dir");
    fs::write(project_path.join("target/debug/app"), "generated build").expect("build file");
    fs::write(project_path.join("target/debug/.ENV.Local"), "NESTED=build")
        .expect("nested build env");
    fs::create_dir_all(project_path.join(".cache/tool")).expect("cache dir");
    fs::write(project_path.join(".cache/tool/state"), "generated cache").expect("cache file");
    fs::create_dir_all(project_path.join(".git")).expect("git dir");
    fs::write(project_path.join(".git/config"), "[core]\n").expect("git config");

    let output = create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "auth-fix".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("work view");

    assert_eq!(output.work_view.name, "auth-fix");
    let materialized = temp.root().join("Code/.work/apps/web/auth-fix");
    assert!(materialized.is_dir());
    assert_eq!(
        fs::read_to_string(materialized.join("src/index.ts")).expect("copied source"),
        "console.log('base')"
    );
    assert_eq!(
        fs::read_to_string(materialized.join(".ENV.Local")).expect("materialized env"),
        "TOKEN=secret"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        assert_eq!(
            fs::metadata(materialized.join(".ENV.Local"))
                .expect("env metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    assert!(!materialized.join(".git/config").exists());
    assert!(!materialized.join("id_rsa").exists());
    assert!(!materialized.join("node_modules").exists());
    assert!(!materialized.join("target").exists());
    assert!(!materialized.join(".cache").exists());
    assert_eq!(
        output.work_view.host_materializations,
        vec![display(&materialized)]
    );
    let store = MetadataStore::open(&db_path).expect("metadata");
    let descriptor = store
        .work_view_exposed_base(&output.work_view.workspace_id, &output.work_view.id)
        .expect("exposed base query")
        .expect("authoritative exposed base");
    assert_eq!(
        descriptor.base_snapshot_id,
        output.work_view.base_snapshot_id
    );
    let exposed = exposed_entries(&store, &descriptor);
    assert_eq!(descriptor.exposed_entry_count, exposed.len() as u64);
    assert!(
        exposed
            .iter()
            .any(|entry| entry.path == "apps/web/src/index.ts")
    );
    assert!(exposed.iter().all(|entry| entry.path != "apps/web/id_rsa"));
    drop(store);

    let diff = diff_work_view(WorkSelectorOptions {
        db_path: Some(db_path.clone()),
        selector: materialized.join("src").display().to_string(),
        paths: Vec::new(),
        generated_at: now(),
    })
    .expect("diff from inside work view");
    assert_eq!(diff.work_view.id, output.work_view.id);

    let sibling = temp.root().join("Code/.work/apps/web/auth-fix-old");
    fs::create_dir_all(&sibling).expect("sibling prefix path");
    let error = diff_work_view(WorkSelectorOptions {
        db_path: Some(db_path.clone()),
        selector: sibling.display().to_string(),
        paths: Vec::new(),
        generated_at: now(),
    })
    .expect_err("sibling prefix is not inside work view");
    assert!(matches!(error, WorkViewError::MissingWorkView { .. }));

    let escaped_sibling = materialized.join("../auth-fix-old");
    let error = diff_work_view(WorkSelectorOptions {
        db_path: Some(db_path),
        selector: escaped_sibling.display().to_string(),
        paths: Vec::new(),
        generated_at: now(),
    })
    .expect_err("parent traversal selector is not inside work view");
    assert!(matches!(error, WorkViewError::MissingWorkView { .. }));
}

#[test]
fn work_create_requires_latest_project_snapshot_before_materializing() {
    let (temp, db_path) = seeded_store_without_snapshot("phase9-work_create-empty-base");
    let project_path = temp.root().join("Code").join("apps/web");

    let error = create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "first-work".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect_err("missing base should block work view");

    assert!(matches!(error, WorkViewError::MissingBaseSnapshot { .. }));
    assert!(!temp.root().join("Code/.work/apps/web/first-work").exists());

    let store = MetadataStore::open(&db_path).expect("metadata");
    assert!(
        store
            .work_views(&WorkspaceId::new("ws_code"), true, None)
            .expect("work views")
            .is_empty()
    );
}

#[test]
fn work_create_fails_closed_when_large_live_file_has_no_canonical_locator() {
    let (temp, db_path) = seeded_store("phase108-large-exposure-needs-snapshot");
    let project_path = temp.root().join("Code/apps/web");
    let large_path = project_path.join("large.bin");
    let file = fs::File::create(&large_path).expect("large file");
    file.set_len(super::super::create_list::MAX_INLINE_EXPOSED_BASE_BYTES + 1)
        .expect("sparse large file");

    let error = create_work_view(WorkCreateOptions {
        db_path: Some(db_path),
        project_path: project_path.display().to_string(),
        name: "large-base".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect_err("large file without canonical locator must fail closed");

    assert!(matches!(
        error,
        WorkViewError::FreshCanonicalSnapshotRequired { path }
            if path == "apps/web/large.bin"
    ));
    assert!(!temp.root().join("Code/.work/apps/web/large-base").exists());
}

#[test]
fn work_create_large_live_file_uses_verified_canonical_bytes() {
    let (temp, db_path) = seeded_store("phase108-large-exposure-verified");
    let byte_len = super::super::create_list::MAX_INLINE_EXPOSED_BASE_BYTES as usize + 17;
    let bytes = vec![b'a'; byte_len];
    let content_id = seed_large_canonical_file(&temp, &db_path, &bytes, [41_u8; 32]);
    let project_path = temp.root().join("Code/apps/web");

    let output = create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "large-verified".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("verified large base");

    assert_eq!(
        fs::read(
            temp.root()
                .join("Code/.work/apps/web/large-verified/large.bin")
        )
        .expect("materialized large bytes"),
        bytes
    );
    let store = MetadataStore::open(&db_path).expect("metadata");
    let descriptor = store
        .work_view_exposed_base(&output.work_view.workspace_id, &output.work_view.id)
        .expect("exposed base")
        .expect("authoritative base");
    let exposed = exposed_entries(&store, &descriptor);
    let large = exposed
        .iter()
        .find(|entry| entry.path == "apps/web/large.bin")
        .expect("large exposed entry");
    assert_eq!(large.content_id.as_ref(), Some(&content_id));
    assert!(large.content_layout.is_some());
}

#[test]
fn work_create_rejects_same_length_large_edit_against_canonical_identity() {
    let (temp, db_path) = seeded_store("phase108-large-exposure-same-length-edit");
    let byte_len = super::super::create_list::MAX_INLINE_EXPOSED_BASE_BYTES as usize + 17;
    let canonical = vec![b'a'; byte_len];
    seed_large_canonical_file(&temp, &db_path, &canonical, [42_u8; 32]);
    let project_path = temp.root().join("Code/apps/web");
    fs::write(project_path.join("large.bin"), vec![b'b'; byte_len]).expect("same-length edit");

    let error = create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "large-stale".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect_err("same-length edit must not reuse canonical identity");

    assert!(matches!(
        error,
        WorkViewError::FreshCanonicalSnapshotRequired { .. }
    ));
    assert!(!temp.root().join("Code/.work/apps/web/large-stale").exists());
    let store = MetadataStore::open(&db_path).expect("metadata");
    assert!(
        store
            .work_views(&WorkspaceId::new("ws_code"), true, None)
            .expect("work views")
            .is_empty()
    );
}

#[test]
fn work_create_from_restore_point_materializes_retained_snapshot_bytes() {
    let (temp, db_path) = seeded_store_without_snapshot("phase18-work_create-from-restore-point");
    let project_path = temp.root().join("Code").join("apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project src");
    fs::write(project_path.join("src/index.ts"), "console.log('current')").expect("source file");
    let old_bytes = b"console.log('old snapshot')";
    let old_content_id = workspace_content_id([18_u8; 32], old_bytes);
    let cache = LocalContentCache::open(temp.root().join(".state/cache")).expect("cache");
    cache
        .put_content(&old_content_id, old_bytes)
        .expect("old content");
    cache
        .get_content(&old_content_id, [18_u8; 32])
        .expect("old content verified");
    let snapshot = snapshot_content_with_file(
        "apps/web/src/index.ts",
        old_content_id.clone(),
        old_bytes.len() as u64,
    );
    let snapshot_id = snapshot.manifest().snapshot_id.clone();
    let mut store = MetadataStore::open(&db_path).expect("metadata");
    crate::page_test_support::persist_cached_snapshot(
        &mut store,
        &snapshot,
        &temp.root().join(".state/metadata-pages-old"),
        &now(),
    );
    store
        .append_local_write_log(&LocalWriteLogRecord {
            id: "write-current-after-old-snapshot".to_string(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-1"),
            project_id: Some(ProjectId::new("proj_web")),
            path: display(&project_path.join("src/index.ts")),
            source_path: None,
            operation: "modify".to_string(),
            staged_content_id: None,
            policy_classification: PathClassification::WorkspaceSync,
            causation_id: "test".to_string(),
            settled_at: now(),
            created_at: now(),
        })
        .expect("pending current write");
    drop(store);

    let output = create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "from-old".to_string(),
        base_snapshot_selector: Some(format!("rp_{}", snapshot_id.as_str())),
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("historical work view");

    assert_eq!(output.work_view.base_snapshot_id, snapshot_id);
    assert!(output.work_view.attention.is_empty());
    assert_eq!(
        output.status.level,
        bowline_core::status::StatusLevel::Attention
    );
    assert!(
        output
            .status
            .attention_items
            .iter()
            .any(|item| { item.contains("historical snapshot") })
    );
    let materialized = temp.root().join("Code/.work/apps/web/from-old");
    assert!(materialized.is_dir());
    assert_eq!(
        fs::read(materialized.join("src/index.ts")).expect("materialized old bytes"),
        old_bytes
    );
    assert_eq!(
        fs::read(project_path.join("src/index.ts")).expect("current file untouched"),
        b"console.log('current')"
    );
    let diff = diff_work_view(WorkSelectorOptions {
        db_path: Some(db_path.clone()),
        selector: "from-old".to_string(),
        paths: Vec::new(),
        generated_at: now(),
    })
    .expect("untouched historical diff");
    assert!(diff.changes.is_empty());
    let store = MetadataStore::open(&db_path).expect("metadata");
    let upload = overlay_deltas_for_upload(&store, &output.work_view).expect("overlay plan");
    assert!(upload.deltas.is_empty());
}

#[test]
fn work_create_from_restore_point_rejects_incomplete_page_cache_typed() {
    let (temp, db_path) = seeded_store_without_snapshot("restore-point-incomplete-pages");
    let project_path = temp.root().join("Code/apps/web");
    let bytes = b"retained but not cached";
    let content_id = workspace_content_id([19_u8; 32], bytes);
    let snapshot =
        snapshot_content_with_file("apps/web/src/index.ts", content_id, bytes.len() as u64);
    let manifest = snapshot.manifest();
    let store = MetadataStore::open(&db_path).expect("metadata");
    store
        .upsert_snapshot(&SnapshotRecord {
            id: manifest.snapshot_id.clone(),
            workspace_id: manifest.workspace_id.clone(),
            project_id: manifest.project_id.clone(),
            kind: manifest.kind,
            base_snapshot_id: manifest.base_snapshot_id.clone(),
            root_id: manifest.namespace_root_id.clone(),
            semantic_manifest_digest: manifest.semantic_manifest_digest.clone(),
            entry_count: manifest.entry_count,
            refs: manifest.refs.clone(),
            created_at: now(),
        })
        .expect("snapshot metadata");
    drop(store);

    let error = create_work_view(WorkCreateOptions {
        db_path: Some(db_path),
        project_path: project_path.display().to_string(),
        name: "incomplete-pages".to_string(),
        base_snapshot_selector: Some(format!("rp_{}", manifest.snapshot_id.as_str())),
        owner_device_id: None,
        generated_at: now(),
    })
    .expect_err("incomplete cached graph must fail");

    assert!(matches!(
        error,
        WorkViewError::CachedSnapshot(crate::sync::CachedSnapshotError::IncompleteGraph)
    ));
    assert!(
        !temp
            .root()
            .join("Code/.work/apps/web/incomplete-pages")
            .exists()
    );
}

#[test]
fn work_create_from_restore_point_materializes_env_but_skips_local_regenerate_and_source_control() {
    let (temp, db_path) =
        seeded_store_without_snapshot("phase18-work_create-from-restore-point-policy");
    let project_path = temp.root().join("Code").join("apps/web");
    fs::create_dir_all(&project_path).expect("project");
    fs::write(
        temp.root().join("Code/.bowlineignore"),
        "apps/web/ignored-tree\napps/web/ignored-tree/**\n!apps/web/ignored-tree/kept.js\n!apps/web/id_rsa\n",
    )
    .expect("current policy");
    let cache = LocalContentCache::open(temp.root().join(".state/cache")).expect("cache");
    let source = retained_content(&cache, [31_u8; 32], b"console.log('old')");
    let env = retained_content(&cache, [32_u8; 32], b"TOKEN=old-secret");
    let git = retained_content(&cache, [33_u8; 32], b"[core]\n");
    let dependency = retained_content(&cache, [34_u8; 32], b"module.exports = {}");
    let included_dependency = retained_content(&cache, [35_u8; 32], b"module.exports = 'kept'");
    let hidden_secret = retained_content(&cache, [36_u8; 32], b"historical-private-key");
    let entries = vec![
        manifest_file(
            "apps/web/src/index.ts",
            source,
            b"console.log('old')".len() as u64,
            PathClassification::WorkspaceSync,
            MaterializationMode::WorkspaceSync,
        ),
        manifest_file(
            "apps/web/.env.local",
            env,
            b"TOKEN=old-secret".len() as u64,
            PathClassification::ProjectEnv,
            MaterializationMode::ProjectEnv,
        ),
        manifest_file(
            "apps/web/.git/config",
            git,
            b"[core]\n".len() as u64,
            PathClassification::WorkspaceSync,
            MaterializationMode::WorkspaceSync,
        ),
        manifest_file(
            "apps/web/ignored-tree/package/index.js",
            dependency,
            b"module.exports = {}".len() as u64,
            PathClassification::WorkspaceSync,
            MaterializationMode::WorkspaceSync,
        ),
        manifest_directory(
            "apps/web/ignored-tree",
            PathClassification::WorkspaceSync,
            MaterializationMode::WorkspaceSync,
        ),
        manifest_file(
            "apps/web/ignored-tree/kept.js",
            included_dependency,
            b"module.exports = 'kept'".len() as u64,
            PathClassification::WorkspaceSync,
            MaterializationMode::WorkspaceSync,
        ),
        manifest_file(
            "apps/web/id_rsa",
            hidden_secret,
            b"historical-private-key".len() as u64,
            PathClassification::WorkspaceSync,
            MaterializationMode::WorkspaceSync,
        ),
    ];
    let workspace_id = WorkspaceId::new("ws_code");
    let snapshot_id =
        crate::sync::rebuild_manifest_identity(&workspace_id, &entries, "test").snapshot_id;
    let snapshot = crate::sync::SnapshotContent::new(
        SnapshotDraft {
            schema_version: bowline_core::workspace_graph::SNAPSHOT_SCHEMA_VERSION,
            snapshot_id: snapshot_id.clone(),
            workspace_id,
            project_id: Some(ProjectId::new("proj_web")),
            kind: SnapshotKind::WorkspaceHead,
            base_snapshot_id: None,
            entries,
            refs: vec![GraphWorkspaceRef {
                name: "workspace".to_string(),
                target_snapshot_id: snapshot_id.clone(),
                kind: RefKind::Workspace,
            }],
        },
        BTreeMap::new(),
        [7; 32],
    )
    .expect("page-backed policy snapshot");
    let mut store = MetadataStore::open(&db_path).expect("metadata");
    crate::page_test_support::persist_cached_snapshot(
        &mut store,
        &snapshot,
        &temp.root().join(".state/metadata-pages-policy"),
        &now(),
    );
    drop(store);

    let output = create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "from-policy-old".to_string(),
        base_snapshot_selector: Some(format!("rp_{}", snapshot_id.as_str())),
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("historical work view");

    let materialized = temp.root().join("Code/.work/apps/web/from-policy-old");
    assert_eq!(output.work_view.base_snapshot_id, snapshot_id);
    assert_eq!(
        output.status.level,
        bowline_core::status::StatusLevel::Attention
    );
    assert!(materialized.join("src/index.ts").exists());
    assert_eq!(
        fs::read_to_string(materialized.join(".env.local")).expect("historical env"),
        "TOKEN=old-secret"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        assert_eq!(
            fs::metadata(materialized.join(".env.local"))
                .expect("historical env metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    assert!(!materialized.join(".git/config").exists());
    assert!(!materialized.join("ignored-tree/package/index.js").exists());
    assert_eq!(
        fs::read_to_string(materialized.join("ignored-tree/kept.js"))
            .expect("explicitly included historical file"),
        "module.exports = 'kept'"
    );
    assert!(!materialized.join("id_rsa").exists());

    let store = MetadataStore::open(&db_path).expect("metadata");
    let descriptor = store
        .work_view_exposed_base(&WorkspaceId::new("ws_code"), &output.work_view.id)
        .expect("exposed base")
        .expect("authoritative base");
    let exposed = exposed_entries(&store, &descriptor);
    assert!(
        exposed
            .iter()
            .any(|entry| entry.path == "apps/web/.env.local")
    );
    assert!(
        exposed
            .iter()
            .all(|entry| entry.path != "apps/web/ignored-tree/package/index.js")
    );
    assert!(
        exposed
            .iter()
            .any(|entry| entry.path == "apps/web/ignored-tree/kept.js")
    );
    assert!(exposed.iter().any(|entry| {
        entry.path == "apps/web/ignored-tree"
            && entry.classification == PathClassification::WorkspaceSync
            && entry.mode == MaterializationMode::StructureOnly
    }));
    assert!(exposed.iter().all(|entry| entry.path != "apps/web/id_rsa"));
}

#[test]
fn work_create_refuses_project_with_pending_local_writes() {
    let (temp, db_path) = seeded_store("phase9-work_create-dirty-project");
    let project_path = temp.root().join("Code").join("apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project src");
    fs::write(project_path.join("src/index.ts"), "console.log('dirty')").expect("dirty file");
    let store = MetadataStore::open(&db_path).expect("metadata");
    store
        .append_local_write_log(&LocalWriteLogRecord {
            id: "write-dirty-project".to_string(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-1"),
            project_id: Some(ProjectId::new("proj_web")),
            path: display(&project_path.join("src/index.ts")),
            source_path: None,
            operation: "modify".to_string(),
            staged_content_id: None,
            policy_classification: PathClassification::WorkspaceSync,
            causation_id: "test".to_string(),
            settled_at: now(),
            created_at: now(),
        })
        .expect("write log");
    drop(store);

    let error = create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "dirty-base".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect_err("dirty project should not become work-view base");

    assert!(matches!(error, WorkViewError::DirtyProject { .. }));
    assert!(!temp.root().join("Code/.work/apps/web/dirty-base").exists());
    let store = MetadataStore::open(&db_path).expect("metadata");
    assert!(
        store
            .work_views(&WorkspaceId::new("ws_code"), true, None)
            .expect("work views")
            .is_empty()
    );
}

#[test]
fn work_create_allows_historical_writes_before_synced_head() {
    let (temp, db_path) = seeded_store("phase9-work_create-historical-write");
    let project_path = temp.root().join("Code").join("apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project src");
    fs::write(project_path.join("src/index.ts"), "console.log('synced')").expect("synced file");
    let store = MetadataStore::open(&db_path).expect("metadata");
    store
        .append_local_write_log(&LocalWriteLogRecord {
            id: "write-synced-project".to_string(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-1"),
            project_id: Some(ProjectId::new("proj_web")),
            path: display(&project_path.join("src/index.ts")),
            source_path: None,
            operation: "modify".to_string(),
            staged_content_id: None,
            policy_classification: PathClassification::WorkspaceSync,
            causation_id: "test".to_string(),
            settled_at: "2026-06-25T01:00:00Z".to_string(),
            created_at: "2026-06-25T01:00:00Z".to_string(),
        })
        .expect("write log");
    store
        .upsert_workspace_sync_head(&WorkspaceSyncHeadRecord {
            workspace_ref: WorkspaceRef {
                workspace_id: WorkspaceId::new("ws_code"),
                version: 1,
                snapshot_id: SnapshotId::new("snap_project_base"),
                updated_at: ControlPlaneTimestamp { tick: 1 },
                updated_by_device_id: Some(DeviceId::new("device-1")),
            },
            observed_at: "2026-06-25T01:01:00Z".to_string(),
        })
        .expect("synced head");
    drop(store);

    let output = create_work_view(WorkCreateOptions {
        db_path: Some(db_path),
        project_path: project_path.display().to_string(),
        name: "after-sync".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("historical write should not block work view");

    assert_eq!(output.work_view.name, "after-sync");
    assert!(temp.root().join("Code/.work/apps/web/after-sync").exists());
}

#[test]
fn work_create_ignores_project_root_modify_noise_after_synced_head() {
    let (temp, db_path) = seeded_store("phase9-work_create-project-root-noise");
    let project_path = temp.root().join("Code").join("apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project src");
    fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("base file");
    let store = MetadataStore::open(&db_path).expect("metadata");
    store
        .append_local_write_log(&LocalWriteLogRecord {
            id: "write-project-root-noise".to_string(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-1"),
            project_id: None,
            path: "apps/web".to_string(),
            source_path: None,
            operation: "modify".to_string(),
            staged_content_id: None,
            policy_classification: PathClassification::WorkspaceSync,
            causation_id: "test".to_string(),
            settled_at: "2026-06-25T01:02:00Z".to_string(),
            created_at: "2026-06-25T01:02:00Z".to_string(),
        })
        .expect("write log");
    store
        .upsert_workspace_sync_head(&WorkspaceSyncHeadRecord {
            workspace_ref: WorkspaceRef {
                workspace_id: WorkspaceId::new("ws_code"),
                version: 1,
                snapshot_id: SnapshotId::new("snap_project_base"),
                updated_at: ControlPlaneTimestamp { tick: 1 },
                updated_by_device_id: Some(DeviceId::new("device-1")),
            },
            observed_at: "2026-06-25T01:01:00Z".to_string(),
        })
        .expect("synced head");
    drop(store);

    let output = create_work_view(WorkCreateOptions {
        db_path: Some(db_path),
        project_path: project_path.display().to_string(),
        name: "directory-noise".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("project root directory noise should not block work view");

    assert_eq!(output.work_view.name, "directory-noise");
    assert!(
        temp.root()
            .join("Code/.work/apps/web/directory-noise")
            .exists()
    );
}

#[test]
fn work_create_ignores_pending_writes_inside_other_work_views() {
    let (temp, db_path) = seeded_store("phase9-work_create-work-namespace-write");
    let project_path = temp.root().join("Code").join("apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project src");
    fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("base file");

    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "first".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("first work view");
    let first_work_file = temp.root().join("Code/.work/apps/web/first/src/index.ts");
    fs::write(&first_work_file, "console.log('overlay')").expect("overlay edit");
    let store = MetadataStore::open(&db_path).expect("metadata");
    store
        .append_local_write_log(&LocalWriteLogRecord {
            id: "write-first-work-view".to_string(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-1"),
            project_id: Some(ProjectId::new("proj_web")),
            path: display(&first_work_file),
            source_path: None,
            operation: "modify".to_string(),
            staged_content_id: None,
            policy_classification: PathClassification::WorkspaceSync,
            causation_id: "test".to_string(),
            settled_at: now(),
            created_at: now(),
        })
        .expect("work-view write log");
    drop(store);

    let second = create_work_view(WorkCreateOptions {
        db_path: Some(db_path),
        project_path: project_path.display().to_string(),
        name: "second".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("work-view overlay writes should not dirty main project");

    assert_eq!(second.work_view.name, "second");
    assert!(temp.root().join("Code/.work/apps/web/second").exists());
}

#[test]
fn work_create_reuses_active_named_view_without_rewriting_it() {
    let (temp, db_path) = seeded_store("phase9-work_create-duplicate");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(&project_path).expect("project");
    let first = create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "same-name".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("first work view");

    let repeated = create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "same-name".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: "2026-06-25T13:00:00Z".to_string(),
    })
    .expect("duplicate should reuse");
    assert_eq!(
        repeated.action,
        bowline_core::work_views::WorkCommandAction::Reused
    );
    assert_eq!(repeated.work_view.id, first.work_view.id);
    assert_eq!(repeated.work_view.created_at, first.work_view.created_at);

    let case_only = create_work_view(WorkCreateOptions {
        db_path: Some(db_path),
        project_path: project_path.display().to_string(),
        name: "Same-Name".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: "2026-06-25T13:00:01Z".to_string(),
    })
    .expect("case-only duplicate should reuse");
    assert_eq!(
        case_only.action,
        bowline_core::work_views::WorkCommandAction::Reused
    );
    assert_eq!(case_only.work_view.id, first.work_view.id);
}

#[test]
fn work_create_rejects_preexisting_non_empty_materialization() {
    let (temp, db_path) = seeded_store("phase9-work_create-stale-materialization");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(&project_path).expect("project");
    let stale = temp.root().join("Code/.work/apps/web/stale/src");
    fs::create_dir_all(&stale).expect("stale dir");
    fs::write(stale.join("old.ts"), "stale\n").expect("stale file");

    let error = create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "stale".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect_err("stale materialization should fail");

    assert!(
        error
            .to_string()
            .contains("materialization path is not empty")
    );
    let store = MetadataStore::open(&db_path).expect("metadata");
    let workspace = store
        .current_workspace()
        .expect("workspace query")
        .expect("workspace");
    assert!(
        store
            .work_views_by_name(&workspace.id, None, "stale")
            .expect("work views")
            .is_empty()
    );
}

#[test]
fn work_create_rejects_symlinked_work_namespace() {
    let (temp, db_path) = seeded_store("phase9-work_create-symlink-namespace");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(&project_path).expect("project");
    let work_root = temp.root().join("Code/.work");
    let outside = temp.root().join("outside-work");
    fs::create_dir_all(&outside).expect("outside");
    symlink(&outside, &work_root).expect("work symlink");

    let error = create_work_view(WorkCreateOptions {
        db_path: Some(db_path),
        project_path: project_path.display().to_string(),
        name: "escape".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect_err("symlinked namespace should fail");

    assert!(
        error
            .to_string()
            .contains("materialization escapes workspace")
    );
    assert!(!outside.join("apps/web/escape").exists());
}

#[test]
fn root_project_base_capture_skips_work_namespace() {
    let (temp, db_path) = seeded_store("phase9-root-project-work-skip");
    let code_root = temp.root().join("Code");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_id = ProjectId::new("proj_root");
    let store = MetadataStore::open(&db_path).expect("metadata");
    store
        .insert_project(
            &project_id,
            &workspace_id,
            "root_code",
            "",
            "2026-06-25T00:01:00Z",
        )
        .expect("root project");
    store
        .set_project_latest_snapshot_id(
            &workspace_id,
            &project_id,
            &SnapshotId::new("snap_root_base"),
        )
        .expect("root snapshot");
    drop(store);

    fs::create_dir_all(code_root.join("src")).expect("src");
    fs::write(code_root.join("src/app.ts"), "console.log('root')").expect("source");
    fs::create_dir_all(code_root.join(".work/apps/web/other/src")).expect("work namespace");
    fs::write(
        code_root.join(".work/apps/web/other/src/generated.ts"),
        "console.log('work')",
    )
    .expect("work file");

    let output = create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: code_root.display().to_string(),
        name: "root-edit".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("root work view");

    let store = MetadataStore::open(&db_path).expect("metadata");
    let descriptor = store
        .work_view_exposed_base(&workspace_id, &output.work_view.id)
        .expect("exposed base")
        .expect("authoritative base");
    let exposed = exposed_entries(&store, &descriptor);
    assert!(exposed.iter().any(|entry| entry.path == "src/app.ts"));
    assert!(
        exposed
            .iter()
            .all(|entry| entry.path != ".work/apps/web/other/src/generated.ts")
    );
}

#[test]
fn work_create_does_not_mutate_filesystem_when_exposed_base_persistence_fails() {
    let (temp, db_path) = seeded_store("phase9-work_create-metadata-rollback");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(&project_path).expect("project");
    fs::write(project_path.join("index.ts"), "console.log('base')\n").expect("base file");
    let materialized = temp.root().join("Code/.work/apps/web/rollback");
    let store = MetadataStore::open(&db_path).expect("metadata");
    store
        .connection()
        .execute(
            "CREATE TRIGGER fail_work_view_exposed_root_insert
                 BEFORE INSERT ON work_view_base_descriptors
                 BEGIN
                   SELECT RAISE(ABORT, 'forced base file insert failure');
                 END",
            [],
        )
        .expect("create failing trigger");
    drop(store);

    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "rollback".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect_err("metadata failure should abort work_create");

    assert!(!materialized.exists());
    let store = MetadataStore::open(&db_path).expect("metadata reopens");
    let work_view_count = store
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM work_views WHERE name = 'rollback'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("work view count");
    assert_eq!(work_view_count, 0);
    store
        .connection()
        .execute("DROP TRIGGER fail_work_view_exposed_root_insert", [])
        .expect("drop failing trigger");
    drop(store);

    create_work_view(WorkCreateOptions {
        db_path: Some(db_path),
        project_path: project_path.display().to_string(),
        name: "rollback".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("failed creation leaves the name reusable");
}

#[test]
fn work_create_recovers_atomic_publish_after_activation_failure() {
    let (temp, db_path) = seeded_store("phase108-work-create-activation-retry");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(&project_path).expect("project");
    fs::write(project_path.join("index.ts"), "console.log('base')\n").expect("base file");
    let visible = temp.root().join("Code/.work/apps/web/retry");
    let store = MetadataStore::open(&db_path).expect("metadata");
    store
        .connection()
        .execute(
            "CREATE TRIGGER fail_work_view_activation
                 BEFORE UPDATE OF lifecycle ON work_views
                 WHEN NEW.lifecycle = 'active'
                 BEGIN
                   SELECT RAISE(ABORT, 'forced activation failure');
                 END",
            [],
        )
        .expect("activation trigger");
    drop(store);

    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "retry".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect_err("activation failure is surfaced");

    assert_eq!(
        fs::read_to_string(visible.join("index.ts")).expect("atomically published tree"),
        "console.log('base')\n"
    );
    let store = MetadataStore::open(&db_path).expect("metadata reopens");
    let pending = store
        .work_views_by_name(
            &WorkspaceId::new("ws_code"),
            Some(&ProjectId::new("proj_web")),
            "retry",
        )
        .expect("pending work view")
        .pop()
        .expect("pending record");
    assert_eq!(pending.lifecycle, WorkViewLifecycle::ReviewReady);
    assert!(pending.host_materializations.is_empty());
    assert!(
        store
            .work_view_exposed_base(&pending.workspace_id, &pending.id)
            .expect("authoritative base")
            .is_some()
    );
    store
        .connection()
        .execute("DROP TRIGGER fail_work_view_activation", [])
        .expect("drop trigger");
    drop(store);

    let recovered = create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "retry".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("retry finalizes published tree");
    assert_eq!(recovered.action, WorkCommandAction::Reused);
    assert_eq!(recovered.work_view.lifecycle, WorkViewLifecycle::Active);
    assert_eq!(
        recovered.work_view.host_materializations,
        vec![display(&visible)]
    );
}

#[test]
fn work_create_recovery_rejects_tree_without_matching_publish_fence() {
    let (temp, db_path) = seeded_store("phase108-work-create-recovery-fence");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(&project_path).expect("project");
    fs::write(project_path.join("index.ts"), "console.log('base')\n").expect("base file");
    let visible = temp.root().join("Code/.work/apps/web/retry-fence");
    let store = MetadataStore::open(&db_path).expect("metadata");
    store
        .connection()
        .execute(
            "CREATE TRIGGER fail_work_view_activation_fence
                 BEFORE UPDATE OF lifecycle ON work_views
                 WHEN NEW.lifecycle = 'active'
                 BEGIN
                   SELECT RAISE(ABORT, 'forced activation failure');
                 END",
            [],
        )
        .expect("activation trigger");
    drop(store);

    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "retry-fence".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect_err("activation failure is surfaced");

    fs::write(visible.join("index.ts"), "unrelated tree\n").expect("replace published bytes");
    let store = MetadataStore::open(&db_path).expect("metadata reopens");
    store
        .connection()
        .execute("DROP TRIGGER fail_work_view_activation_fence", [])
        .expect("drop trigger");
    drop(store);

    let error = create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "retry-fence".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect_err("unverified visible tree must not activate");

    assert!(matches!(
        error,
        WorkViewError::UnsafeWorkViewPath {
            reason: "pending work-view publication has no matching durable checkpoint",
            ..
        }
    ));
    let pending = MetadataStore::open(&db_path)
        .expect("metadata")
        .work_views_by_name(
            &WorkspaceId::new("ws_code"),
            Some(&ProjectId::new("proj_web")),
            "retry-fence",
        )
        .expect("pending work view")
        .pop()
        .expect("pending record");
    assert_eq!(pending.lifecycle, WorkViewLifecycle::ReviewReady);
    assert!(pending.host_materializations.is_empty());
    assert_eq!(
        fs::read(visible.join("index.ts")).unwrap(),
        b"unrelated tree\n"
    );
}

fn retained_content(cache: &LocalContentCache, digest: [u8; 32], bytes: &[u8]) -> ContentId {
    let content_id = workspace_content_id(digest, bytes);
    cache.put_content(&content_id, bytes).expect("put content");
    cache
        .get_content(&content_id, digest)
        .expect("verified content");
    content_id
}

fn manifest_file(
    path: &str,
    content_id: ContentId,
    byte_len: u64,
    classification: PathClassification,
    mode: MaterializationMode,
) -> NamespaceEntry {
    NamespaceEntry {
        path: path.to_string(),
        kind: NamespaceEntryKind::File,
        classification,
        mode,
        access: vec![AccessFlag::HumanReadable, AccessFlag::AgentReadable],
        content_id: Some(content_id.clone()),
        content_layout: Some(
            ContentLayout::single_segment(ContentLocator {
                content_id,
                storage: ContentStorage::Packed,
                raw_size: byte_len,
                pack_id: Some(bowline_core::ids::PackId::new("pk_retained")),
                offset: Some(0),
                length: Some(byte_len),
            })
            .expect("test layout"),
        ),
        symlink_target: None,
        byte_len: Some(byte_len),
        executability: bowline_core::workspace_graph::FileExecutability::Regular,
        hydration_state: HydrationState::Local,
    }
}

fn manifest_directory(
    path: &str,
    classification: PathClassification,
    mode: MaterializationMode,
) -> NamespaceEntry {
    NamespaceEntry {
        path: path.to_string(),
        kind: NamespaceEntryKind::Directory,
        classification,
        mode,
        access: vec![AccessFlag::HumanReadable, AccessFlag::AgentReadable],
        content_id: None,
        content_layout: None,
        symlink_target: None,
        byte_len: None,
        executability: bowline_core::workspace_graph::FileExecutability::Regular,
        hydration_state: HydrationState::Local,
    }
}
