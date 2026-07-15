use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::PathBuf,
};

use crate::sync::PreparedContent;
use bowline_control_plane::{ControlPlaneTimestamp, WorkspaceRef as RemoteWorkspaceRef};
use bowline_core::{
    git_worktree_link::WORKSPACE_ROOT_MARKER,
    ids::{ContentId, DeviceId, PackId, SnapshotId, WorkspaceId},
    namespace_snapshot::{NamespaceCancellation, NamespaceReadError},
    policy::{AccessFlag, MaterializationMode, PathClassification},
    status::ObservedWorkspaceSummary,
    workspace_graph::{
        ContentLayout, ContentLocator, ContentStorage, FileExecutability, HydrationState,
        NamespaceEntry, NamespaceEntryKind,
    },
};

use crate::{
    metadata::{DEFAULT_DATABASE_FILE, MetadataStore},
    policy::classify_path_with_builtin_policy,
    scanner::{PathObservation, ScanReport},
    sync::{
        FullScanReason, RehashReason, ScanScope, StatCacheSession,
        stat_cache::{StatCacheDeleteScope, verify_shard_for_path},
    },
    workspace::TempWorkspace,
};

use super::{
    CoalesceScanRequest, CoalesceWorkspaceReportRequest, coalesce_workspace_report,
    coalesce_workspace_scan_cached,
};

#[test]
fn snapshot_id_stable_for_fixed_entries() {
    let workspace_id = WorkspaceId::new("ws_code");
    let entries = fixed_identity_entries();
    let snapshot_id =
        crate::sync::rebuild_manifest_identity(&workspace_id, &entries, "2026-07-03T12:00:00Z")
            .snapshot_id;

    // Golden chunked snapshot ID. If this assertion ever fails, the identity
    // encoding drifted, which re-identifies every snapshot fleet-wide.
    // Fix the encoding, never the constant except a deliberate vN bump.
    assert_eq!(snapshot_id.as_str(), "snap_e373a160fce26897219a383f");
}

#[test]
fn production_coalescing_uses_versioned_snapshot_identity() {
    let workspace = TempWorkspace::new("coalesce-snapshot-identity").expect("workspace");
    fs::create_dir_all(workspace.root().join("app/src")).expect("source parent");
    fs::write(workspace.root().join("app/src/main.rs"), b"fn main() {}\n").expect("source");

    let candidate = coalesce_workspace_report(CoalesceWorkspaceReportRequest {
        root: workspace.root(),
        report: stale_file_report("app/src/main.rs", Some(13)),
        workspace_id: WorkspaceId::new("ws_code"),
        base_ref: &base_ref(),
        device_id: DeviceId::new("device-test"),
        workspace_content_key: [14_u8; 32],
        created_at: "2026-07-03T12:00:00Z".to_string(),
        context: super::CoalesceContext::empty(),
    })
    .expect("coalesce");

    assert_eq!(
        candidate.snapshot.manifest.snapshot_id.as_str(),
        "snap_1d63766b4f2505fe94dec2d8"
    );
    assert_eq!(
        candidate.manifest_id.as_str(),
        "mf_ac3ba37a2b523a895dfca6a1"
    );
    assert!(candidate.snapshot.manifest.base_snapshot_id.is_none());
    assert_eq!(
        candidate
            .snapshot
            .namespace_snapshot()
            .changed
            .mutations_applied,
        candidate.snapshot.manifest().entry_count
    );
    assert!(
        candidate
            .snapshot
            .namespace_snapshot()
            .changed
            .namespace_pages_created
            > 0
    );
}

#[test]
fn production_coalescing_scopes_identity_by_workspace() {
    let workspace = TempWorkspace::new("coalesce-workspace-scope").expect("workspace");
    fs::create_dir_all(workspace.root().join("app/src")).expect("source parent");
    fs::write(workspace.root().join("app/src/main.rs"), b"fn main() {}\n").expect("source");
    let report = stale_file_report("app/src/main.rs", Some(13));

    let first = coalesce_workspace_report(CoalesceWorkspaceReportRequest {
        root: workspace.root(),
        report: report.clone(),
        workspace_id: WorkspaceId::new("ws_code"),
        base_ref: &base_ref(),
        device_id: DeviceId::new("device-test"),
        workspace_content_key: [14_u8; 32],
        created_at: "2026-07-03T12:00:00Z".to_string(),
        context: super::CoalesceContext::empty(),
    })
    .expect("first coalesce");
    let mut other_base = base_ref();
    other_base.workspace_id = WorkspaceId::new("ws_other");
    let second = coalesce_workspace_report(CoalesceWorkspaceReportRequest {
        root: workspace.root(),
        report,
        workspace_id: WorkspaceId::new("ws_other"),
        base_ref: &other_base,
        device_id: DeviceId::new("device-test"),
        workspace_content_key: [14_u8; 32],
        created_at: "2026-07-03T12:00:00Z".to_string(),
        context: super::CoalesceContext::empty(),
    })
    .expect("second coalesce");

    assert_ne!(
        first.snapshot.manifest.snapshot_id,
        second.snapshot.manifest.snapshot_id
    );
}

#[test]
fn no_op_incremental_coalesce_reuses_the_prior_root_without_retained_entries() {
    let workspace = TempWorkspace::new("coalesce-page-no-op").expect("workspace");
    write_file(workspace.root(), "src/main.rs", b"fn main() {}\n");
    let workspace_id = WorkspaceId::new("ws_page_no_op");
    let initial = super::coalesce_workspace_scan(
        workspace.root(),
        workspace_id.clone(),
        &base_ref(),
        DeviceId::new("device-test"),
        [31_u8; 32],
        "2026-07-14T10:00:00Z",
    )
    .expect("initial full coalesce");

    let candidate = super::coalesce_workspace_report_with_cache(
        CoalesceWorkspaceReportRequest {
            root: workspace.root(),
            report: ScanReport {
                root: workspace.root().to_path_buf(),
                projects: Vec::new(),
                paths: Vec::new(),
                summary: ObservedWorkspaceSummary::default(),
            },
            workspace_id,
            base_ref: &base_ref(),
            device_id: DeviceId::new("device-test"),
            workspace_content_key: [31_u8; 32],
            created_at: "2026-07-14T10:01:00Z".to_string(),
            context: super::CoalesceContext {
                paths: &BTreeSet::new(),
                prior_snapshot: Some(&initial.snapshot),
                namespace_cancellation: None,
                preserved_entries: &[],
                file_overrides: &BTreeMap::new(),
                base_locators: &BTreeMap::new(),
                preparation_root: None,
            },
        },
        None,
        StatCacheDeleteScope::All,
        None,
        ScanScope::DirtySubtrees {
            roots: BTreeSet::new(),
            root_shallow: false,
        },
    )
    .expect("no-op incremental coalesce");

    assert_eq!(
        candidate.snapshot.manifest().namespace_root_id,
        initial.snapshot.manifest().namespace_root_id
    );
    assert_eq!(
        candidate
            .snapshot
            .namespace_snapshot()
            .changed
            .mutations_applied,
        0
    );
    assert_eq!(
        candidate
            .snapshot
            .namespace_snapshot()
            .changed
            .namespace_pages_created,
        0
    );
    assert!(candidate.snapshot.prepared_content().is_empty());
}

#[test]
fn root_shallow_deletion_changes_identity_without_dropping_unowned_deep_entries() {
    let workspace = TempWorkspace::new("coalesce-root-delete").expect("workspace");
    write_file(workspace.root(), "README.md", b"remove me\n");
    write_file(workspace.root(), "src/main.rs", b"fn main() {}\n");
    let workspace_id = WorkspaceId::new("ws_root_delete");
    let initial = super::coalesce_workspace_scan(
        workspace.root(),
        workspace_id.clone(),
        &base_ref(),
        DeviceId::new("device-test"),
        [34_u8; 32],
        "2026-07-14T10:00:00Z",
    )
    .expect("initial full coalesce");
    fs::remove_file(workspace.root().join("README.md")).expect("delete root file");

    let candidate = coalesce_workspace_scan_cached(CoalesceScanRequest {
        root: workspace.root(),
        workspace_id,
        base_ref: &base_ref(),
        device_id: DeviceId::new("device-test"),
        workspace_content_key: [34_u8; 32],
        created_at: "2026-07-14T10:01:00Z".to_string(),
        context: super::CoalesceContext {
            paths: &BTreeSet::new(),
            prior_snapshot: Some(&initial.snapshot),
            namespace_cancellation: None,
            preserved_entries: &[],
            file_overrides: &BTreeMap::new(),
            base_locators: &BTreeMap::new(),
            preparation_root: None,
        },
        stat_cache: None,
        scan_scope: ScanScope::RootShallow,
    })
    .expect("root-shallow deletion coalesce");

    assert_ne!(
        candidate.snapshot.manifest().snapshot_id,
        initial.snapshot.manifest().snapshot_id
    );
    assert!(
        candidate
            .snapshot
            .entry_for_path("README.md")
            .expect("deleted path lookup")
            .is_none()
    );
    assert!(
        candidate
            .snapshot
            .entry_for_path("src/main.rs")
            .expect("preserved deep path lookup")
            .is_some()
    );
}

#[test]
fn scoped_coalesce_mutates_owned_root_and_reuses_unowned_pages() {
    let workspace = TempWorkspace::new("coalesce-page-scoped").expect("workspace");
    write_file(workspace.root(), "src/main.rs", b"fn main() { old(); }\n");
    for index in 0..300 {
        write_file(
            workspace.root(),
            &format!("vendor/pkg-{index:03}/lib.rs"),
            format!("pub const N: usize = {index};\n").as_bytes(),
        );
    }
    let workspace_id = WorkspaceId::new("ws_page_scoped");
    let initial = super::coalesce_workspace_scan(
        workspace.root(),
        workspace_id.clone(),
        &base_ref(),
        DeviceId::new("device-test"),
        [32_u8; 32],
        "2026-07-14T11:00:00Z",
    )
    .expect("initial full coalesce");
    write_file(workspace.root(), "src/main.rs", b"fn main() { new(); }\n");
    let roots = BTreeSet::from(["src".to_string()]);

    let candidate = coalesce_workspace_scan_cached(CoalesceScanRequest {
        root: workspace.root(),
        workspace_id: workspace_id.clone(),
        base_ref: &base_ref(),
        device_id: DeviceId::new("device-test"),
        workspace_content_key: [32_u8; 32],
        created_at: "2026-07-14T11:01:00Z".to_string(),
        context: super::CoalesceContext {
            paths: &BTreeSet::new(),
            prior_snapshot: Some(&initial.snapshot),
            namespace_cancellation: None,
            preserved_entries: &[],
            file_overrides: &BTreeMap::new(),
            base_locators: &BTreeMap::new(),
            preparation_root: None,
        },
        stat_cache: None,
        scan_scope: ScanScope::DirtySubtrees {
            roots,
            root_shallow: false,
        },
    })
    .expect("scoped incremental coalesce");

    let changed = &candidate.snapshot.namespace_snapshot().changed;
    assert!(changed.mutations_applied <= 3);
    assert!(changed.namespace_pages_reused > 0);
    assert!(
        changed.namespace_pages_created
            < candidate.snapshot.namespace_store().namespace_page_count()
    );
    assert!(
        candidate
            .snapshot
            .entry_for_path("vendor/pkg-299/lib.rs")
            .expect("page read")
            .is_some()
    );
    assert_eq!(candidate.snapshot.prepared_content().len(), 1);
    let entries = candidate.snapshot.entries_for_test();
    let rebuilt = crate::sync::rebuild_manifest_identity(&workspace_id, &entries, "parity");
    assert_eq!(
        candidate.snapshot.manifest().snapshot_id,
        *rebuilt.snapshot_id()
    );
    assert_eq!(
        candidate.snapshot.manifest().semantic_manifest_digest,
        *rebuilt.semantic_manifest_digest()
    );
}

#[test]
fn coalescer_namespace_builder_honors_cancellation() {
    struct Cancelled;
    impl NamespaceCancellation for Cancelled {
        fn is_cancelled(&self) -> bool {
            true
        }
    }

    let workspace = TempWorkspace::new("coalesce-page-cancelled").expect("workspace");
    write_file(workspace.root(), "src/main.rs", b"fn main() {}\n");
    let error = super::coalesce_workspace_scan_excluding(
        workspace.root(),
        WorkspaceId::new("ws_page_cancelled"),
        &base_ref(),
        DeviceId::new("device-test"),
        [33_u8; 32],
        "2026-07-14T12:00:00Z",
        super::CoalesceContext {
            paths: &BTreeSet::new(),
            prior_snapshot: None,
            namespace_cancellation: Some(&Cancelled),
            preserved_entries: &[],
            file_overrides: &BTreeMap::new(),
            base_locators: &BTreeMap::new(),
            preparation_root: None,
        },
    )
    .expect_err("cancelled namespace build");

    assert!(matches!(
        error,
        super::CoalesceError::Namespace(
            bowline_core::namespace_snapshot::NamespaceBuildError::Read(
                NamespaceReadError::Cancelled
            )
        )
    ));
}

#[cfg(unix)]
#[test]
fn coalescing_records_unsafe_symlinks_without_snapshot_entries() {
    let workspace = TempWorkspace::new("coalesce-unsafe-symlinks").expect("workspace");
    std::fs::write(workspace.root().join("package.json"), b"{}").expect("package json");
    std::os::unix::fs::symlink(
        "/tmp/bowline-outside",
        workspace.root().join("absolute-link"),
    )
    .expect("absolute symlink");
    std::os::unix::fs::symlink("../outside", workspace.root().join("escaping-link"))
        .expect("escaping symlink");
    std::os::unix::fs::symlink("..", workspace.root().join("bare-parent-link"))
        .expect("bare parent symlink");

    let candidate = super::coalesce_workspace_scan(
        workspace.root(),
        WorkspaceId::new("ws_code"),
        &base_ref(),
        DeviceId::new("device-test"),
        [14_u8; 32],
        "2026-07-07T12:00:00Z",
    )
    .expect("coalesce");

    assert_eq!(
        candidate.skipped_unsafe_symlinks,
        BTreeSet::from([
            "absolute-link".to_string(),
            "bare-parent-link".to_string(),
            "escaping-link".to_string(),
        ])
    );
    let entry_paths = candidate
        .snapshot
        .entries_for_test()
        .into_iter()
        .map(|entry| entry.path.as_str().to_string())
        .collect::<BTreeSet<_>>();
    assert!(!entry_paths.contains("absolute-link"));
    assert!(!entry_paths.contains("bare-parent-link"));
    assert!(!entry_paths.contains("escaping-link"));
}

#[test]
fn coalescing_uses_read_bytes_for_file_length_when_writer_races_scan_metadata() {
    let workspace = TempWorkspace::new("coalesce-concurrent-writer").expect("workspace");
    let source_path = workspace.root().join("app/src/main.ts");
    fs::create_dir_all(source_path.parent().expect("source parent")).expect("source parent");
    fs::write(&source_path, b"export const value = 'writer won';\n").expect("source");

    let report = stale_file_report("app/src/main.ts", Some(1));
    let candidate = coalesce_workspace_report(CoalesceWorkspaceReportRequest {
        root: workspace.root(),
        report,
        workspace_id: WorkspaceId::new("ws_code"),
        base_ref: &base_ref(),
        device_id: DeviceId::new("device-test"),
        workspace_content_key: [11_u8; 32],
        created_at: "2026-06-26T16:45:00Z".to_string(),
        context: super::CoalesceContext::empty(),
    })
    .expect("coalesce");
    let entry = candidate
        .snapshot
        .entries_for_test()
        .into_iter()
        .find(|entry| entry.path == "app/src/main.ts")
        .expect("source entry");
    let bytes = candidate
        .snapshot
        .file_bytes_for_path("app/src/main.ts")
        .expect("source bytes");

    assert_eq!(bytes, b"export const value = 'writer won';\n");
    assert_eq!(entry.byte_len, Some(bytes.len() as u64));
}

#[test]
fn verify_due_scan_rehashes_cached_files() {
    let workspace = TempWorkspace::new("coalesce-verify-due-stat-cache").expect("workspace");
    write_file(
        workspace.root(),
        "app/src/main.ts",
        b"export const value = 1;\n",
    );
    let workspace_id = WorkspaceId::new("ws_verify_due");
    let content_key = [33_u8; 32];
    let metadata_dir = workspace
        .root()
        .parent()
        .expect("workspace parent")
        .join("verify-due-state");
    let _ = fs::remove_dir_all(&metadata_dir);
    fs::create_dir_all(&metadata_dir).expect("metadata dir");
    let mut store = MetadataStore::open(metadata_dir.join(DEFAULT_DATABASE_FILE)).expect("store");
    store
        .insert_workspace(&workspace_id, "Code", "2026-07-04T10:00:00Z")
        .expect("workspace");

    let mut cold_session = StatCacheSession::empty_for_scan(1, &content_key);
    let cold = coalesce_workspace_scan_cached(CoalesceScanRequest {
        root: workspace.root(),
        workspace_id: workspace_id.clone(),
        base_ref: &base_ref(),
        device_id: DeviceId::new("device-test"),
        workspace_content_key: content_key,
        created_at: "2026-07-04T10:00:00Z".to_string(),
        context: super::CoalesceContext::empty(),
        stat_cache: Some(&mut cold_session),
        scan_scope: ScanScope::Full(FullScanReason::CliRequested),
    })
    .expect("cold coalesce");
    store
        .apply_stat_cache_write_back(
            &workspace_id,
            &cold.stat_cache_write_back.expect("cold write-back").upserts,
            &BTreeSet::new(),
        )
        .expect("cache write-back");

    let mut verify_session =
        StatCacheSession::load(&store, &workspace_id, 1, &content_key).expect("cache load");
    let verify = coalesce_workspace_scan_cached(CoalesceScanRequest {
        root: workspace.root(),
        workspace_id,
        base_ref: &base_ref(),
        device_id: DeviceId::new("device-test"),
        workspace_content_key: content_key,
        created_at: verify_timestamp_for_path("app/src/main.ts"),
        context: super::CoalesceContext::empty(),
        stat_cache: Some(&mut verify_session),
        scan_scope: ScanScope::Full(FullScanReason::VerifyDue),
    })
    .expect("verify coalesce");

    assert_eq!(verify.scan_stats.stat_hits, 0);
    assert_eq!(verify.scan_stats.files_hashed, 1);
    assert_eq!(
        verify
            .scan_stats
            .rehash_reasons
            .get(&RehashReason::VerifyShard),
        Some(&1)
    );
    assert!(
        verify
            .snapshot
            .file_bytes_for_path("app/src/main.ts")
            .is_some()
    );
}

#[test]
fn verify_due_scan_reports_poisoned_cache_row_divergence() {
    let workspace = TempWorkspace::new("coalesce-verify-divergence").expect("workspace");
    write_file(
        workspace.root(),
        "app/src/main.ts",
        b"export const value = 1;\n",
    );
    let workspace_id = WorkspaceId::new("ws_verify_divergence");
    let content_key = [34_u8; 32];
    let metadata_dir = workspace
        .root()
        .parent()
        .expect("workspace parent")
        .join("verify-divergence-state");
    let _ = fs::remove_dir_all(&metadata_dir);
    fs::create_dir_all(&metadata_dir).expect("metadata dir");
    let mut store = MetadataStore::open(metadata_dir.join(DEFAULT_DATABASE_FILE)).expect("store");
    store
        .insert_workspace(&workspace_id, "Code", "2026-07-04T10:00:00Z")
        .expect("workspace");

    let mut cold_session = StatCacheSession::empty_for_scan(1, &content_key);
    let mut cold = coalesce_workspace_scan_cached(CoalesceScanRequest {
        root: workspace.root(),
        workspace_id: workspace_id.clone(),
        base_ref: &base_ref(),
        device_id: DeviceId::new("device-test"),
        workspace_content_key: content_key,
        created_at: "2026-07-04T10:00:00Z".to_string(),
        context: super::CoalesceContext::empty(),
        stat_cache: Some(&mut cold_session),
        scan_scope: ScanScope::Full(FullScanReason::CliRequested),
    })
    .expect("cold coalesce");
    let mut write_back = cold.stat_cache_write_back.take().expect("cold write-back");
    write_back.upserts[0].content_id = ContentId::new("cid_poisoned");
    store
        .apply_stat_cache_write_back(&workspace_id, &write_back.upserts, &BTreeSet::new())
        .expect("cache write-back");

    let mut verify_session =
        StatCacheSession::load(&store, &workspace_id, 1, &content_key).expect("cache load");
    let verify = coalesce_workspace_scan_cached(CoalesceScanRequest {
        root: workspace.root(),
        workspace_id,
        base_ref: &base_ref(),
        device_id: DeviceId::new("device-test"),
        workspace_content_key: content_key,
        created_at: verify_timestamp_for_path("app/src/main.ts"),
        context: super::CoalesceContext::empty(),
        stat_cache: Some(&mut verify_session),
        scan_scope: ScanScope::Full(FullScanReason::VerifyDue),
    })
    .expect("verify coalesce");

    assert_eq!(verify.stat_cache_divergences.len(), 1);
    assert_eq!(verify.stat_cache_divergences[0].path, "app/src/main.ts");
    assert_eq!(
        verify.stat_cache_divergences[0].cached_content_id,
        ContentId::new("cid_poisoned")
    );
    assert_eq!(verify.scan_stats.divergence_count, 1);
}

#[test]
fn snapshot_id_changes_when_only_executability_flips() {
    let workspace = TempWorkspace::new("coalesce-exec-flip").expect("workspace");
    let source_path = workspace.root().join("app/src/main.ts");
    fs::create_dir_all(source_path.parent().expect("source parent")).expect("source parent");
    fs::write(&source_path, b"export const value = 1;\n").expect("source");

    let regular = coalesce_workspace_report(CoalesceWorkspaceReportRequest {
        root: workspace.root(),
        report: report_for_file_with_executability("app/src/main.ts", FileExecutability::Regular),
        workspace_id: WorkspaceId::new("ws_code"),
        base_ref: &base_ref(),
        device_id: DeviceId::new("device-test"),
        workspace_content_key: [22_u8; 32],
        created_at: "2026-07-03T12:00:00Z".to_string(),
        context: super::CoalesceContext::empty(),
    })
    .expect("regular coalesce");
    let executable = coalesce_workspace_report(CoalesceWorkspaceReportRequest {
        root: workspace.root(),
        report: report_for_file_with_executability(
            "app/src/main.ts",
            FileExecutability::Executable,
        ),
        workspace_id: WorkspaceId::new("ws_code"),
        base_ref: &base_ref(),
        device_id: DeviceId::new("device-test"),
        workspace_content_key: [22_u8; 32],
        created_at: "2026-07-03T12:00:00Z".to_string(),
        context: super::CoalesceContext::empty(),
    })
    .expect("executable coalesce");

    assert_ne!(
        regular.snapshot.manifest.snapshot_id,
        executable.snapshot.manifest.snapshot_id
    );
    assert_eq!(
        executable.snapshot.entries_for_test()[0].executability,
        FileExecutability::Executable
    );
}

#[test]
fn coalescing_skips_paths_that_vanish_between_scan_and_read() {
    let workspace = TempWorkspace::new("coalesce-vanished-path").expect("workspace");
    fs::create_dir_all(workspace.root().join("app/src")).expect("source parent");
    let report = stale_file_report("app/src/main.ts", Some(128));

    let candidate = coalesce_workspace_report(CoalesceWorkspaceReportRequest {
        root: workspace.root(),
        report,
        workspace_id: WorkspaceId::new("ws_code"),
        base_ref: &base_ref(),
        device_id: DeviceId::new("device-test"),
        workspace_content_key: [12_u8; 32],
        created_at: "2026-06-26T16:45:00Z".to_string(),
        context: super::CoalesceContext::empty(),
    })
    .expect("coalesce");

    assert!(
        candidate.snapshot.entries_for_test().is_empty(),
        "vanished observed paths should not fail or produce stale entries"
    );
    assert!(candidate.snapshot.prepared_content().is_empty());
}

#[test]
fn coalescing_ignores_materialization_temp_files() {
    let workspace = TempWorkspace::new("coalesce-materialize-temp").expect("workspace");
    fs::create_dir_all(workspace.root().join("app/src")).expect("source parent");
    fs::write(
        workspace
            .root()
            .join("app/src/.bowline-materialize-index_ts-abcdef123456.tmp"),
        b"stale temp bytes\n",
    )
    .expect("temp file");
    let report = stale_file_report(
        "app/src/.bowline-materialize-index_ts-abcdef123456.tmp",
        Some(17),
    );

    let candidate = coalesce_workspace_report(CoalesceWorkspaceReportRequest {
        root: workspace.root(),
        report,
        workspace_id: WorkspaceId::new("ws_code"),
        base_ref: &base_ref(),
        device_id: DeviceId::new("device-test"),
        workspace_content_key: [13_u8; 32],
        created_at: "2026-06-26T16:45:00Z".to_string(),
        context: super::CoalesceContext::empty(),
    })
    .expect("coalesce");

    assert!(candidate.snapshot.entries_for_test().is_empty());
    assert!(candidate.snapshot.prepared_content().is_empty());
}

#[test]
fn coalescing_skips_derivable_git_paths_but_keeps_index_and_repo_locks() {
    let workspace = TempWorkspace::new("coalesce-git-shape").expect("workspace");
    for (path, bytes) in [
        ("repo/.git/index", b"index".as_slice()),
        ("repo/.git/logs/HEAD", b"log"),
        ("repo/.git/index.lock", b"lock"),
        ("repo/.git/HEAD", b"ref: refs/heads/main\n"),
        ("repo/.git/objects/ab/cd", b"object"),
        ("repo/Cargo.lock", b"lockfile"),
    ] {
        let absolute = workspace.root().join(path);
        fs::create_dir_all(absolute.parent().expect("parent")).expect("parent");
        fs::write(absolute, bytes).expect("path bytes");
    }

    let candidate = coalesce_workspace_report(CoalesceWorkspaceReportRequest {
        root: workspace.root(),
        report: report_for_files(&[
            "repo/.git/index",
            "repo/.git/logs/HEAD",
            "repo/.git/index.lock",
            "repo/.git/HEAD",
            "repo/.git/objects/ab/cd",
            "repo/Cargo.lock",
        ]),
        workspace_id: WorkspaceId::new("ws_code"),
        base_ref: &base_ref(),
        device_id: DeviceId::new("device-test"),
        workspace_content_key: [14_u8; 32],
        created_at: "2026-06-26T16:45:00Z".to_string(),
        context: super::CoalesceContext::empty(),
    })
    .expect("coalesce");

    let paths = candidate
        .snapshot
        .entries_for_test()
        .into_iter()
        .map(|entry| entry.path.as_str().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        paths,
        vec![
            "repo/.git/HEAD",
            "repo/.git/index",
            "repo/.git/objects/ab/cd",
            "repo/Cargo.lock"
        ]
    );
}

#[test]
fn coalescing_normalizes_worktree_gitlink_before_hashing() {
    let first = TempWorkspace::new("coalesce-worktree-link-a").expect("workspace");
    let second = TempWorkspace::new("coalesce-worktree-link-b").expect("workspace");
    let path = "repo-wt/.git";
    write_file(
        first.root(),
        path,
        format!(
            "gitdir: {}/repo/.git/worktrees/feat\n",
            first.root().display()
        )
        .as_bytes(),
    );
    write_file(
        second.root(),
        path,
        format!(
            "gitdir: {}/repo/.git/worktrees/feat\n",
            second.root().display()
        )
        .as_bytes(),
    );

    let first_candidate = coalesce_workspace_report(CoalesceWorkspaceReportRequest {
        root: first.root(),
        report: report_for_files(&[path]),
        workspace_id: WorkspaceId::new("ws_code"),
        base_ref: &base_ref(),
        device_id: DeviceId::new("device-test"),
        workspace_content_key: [31_u8; 32],
        created_at: "2026-07-04T10:00:00Z".to_string(),
        context: super::CoalesceContext::empty(),
    })
    .expect("first coalesce");
    let second_candidate = coalesce_workspace_report(CoalesceWorkspaceReportRequest {
        root: second.root(),
        report: report_for_files(&[path]),
        workspace_id: WorkspaceId::new("ws_code"),
        base_ref: &base_ref(),
        device_id: DeviceId::new("device-test"),
        workspace_content_key: [31_u8; 32],
        created_at: "2026-07-04T10:00:00Z".to_string(),
        context: super::CoalesceContext::empty(),
    })
    .expect("second coalesce");

    let first_entry = first_candidate
        .snapshot
        .entries_for_test()
        .into_iter()
        .find(|entry| entry.path == path)
        .expect("gitlink entry");
    let second_entry = second_candidate
        .snapshot
        .entries_for_test()
        .into_iter()
        .find(|entry| entry.path == path)
        .expect("gitlink entry");
    let expected = format!("gitdir: {WORKSPACE_ROOT_MARKER}/repo/.git/worktrees/feat\n");

    assert_eq!(
        first_candidate.snapshot.file_bytes_for_path(path),
        Some(expected.as_bytes())
    );
    assert_eq!(first_entry.content_id, second_entry.content_id);
}

#[test]
fn coalescing_respects_ignored_worktree_link_paths() {
    let workspace = TempWorkspace::new("coalesce-ignored-worktree-gitlink").expect("workspace");
    let path = "repo-wt/.git";
    write_file(
        workspace.root(),
        path,
        format!(
            "gitdir: {}/repo/.git/worktrees/feat\n",
            workspace.root().display()
        )
        .as_bytes(),
    );
    let mut report = report_for_files(&[path]);
    report.paths[0].policy = crate::policy::PathPolicyDecision {
        classification: PathClassification::LocalOnly,
        mode: MaterializationMode::Ignore,
        access: vec![AccessFlag::HumanReadable, AccessFlag::AgentHidden],
    };

    let candidate = coalesce_workspace_report(CoalesceWorkspaceReportRequest {
        root: workspace.root(),
        report,
        workspace_id: WorkspaceId::new("ws_code"),
        base_ref: &base_ref(),
        device_id: DeviceId::new("device-test"),
        workspace_content_key: [24_u8; 32],
        created_at: "2026-07-04T10:00:00Z".to_string(),
        context: super::CoalesceContext::empty(),
    })
    .expect("coalesce");

    assert!(
        candidate
            .snapshot
            .entries_for_test()
            .iter()
            .all(|entry| entry.path != path)
    );
    assert!(candidate.snapshot.file_bytes_for_path(path).is_none());
}

#[test]
fn coalescing_normalizes_worktree_admin_pointer_and_keeps_relative_commondir() {
    let workspace = TempWorkspace::new("coalesce-worktree-admin").expect("workspace");
    let gitdir = "repo/.git/worktrees/feat/gitdir";
    let commondir = "repo/.git/worktrees/feat/commondir";
    write_file(
        workspace.root(),
        gitdir,
        format!("{}/repo-wt/.git\n", workspace.root().display()).as_bytes(),
    );
    write_file(workspace.root(), commondir, b"../..\n");

    let candidate = coalesce_workspace_report(CoalesceWorkspaceReportRequest {
        root: workspace.root(),
        report: report_for_files(&[gitdir, commondir]),
        workspace_id: WorkspaceId::new("ws_code"),
        base_ref: &base_ref(),
        device_id: DeviceId::new("device-test"),
        workspace_content_key: [32_u8; 32],
        created_at: "2026-07-04T10:00:00Z".to_string(),
        context: super::CoalesceContext::empty(),
    })
    .expect("coalesce");

    let expected_gitdir = format!("{WORKSPACE_ROOT_MARKER}/repo-wt/.git\n");
    assert_eq!(
        candidate.snapshot.file_bytes_for_path(gitdir),
        Some(expected_gitdir.as_bytes())
    );
    assert_eq!(
        candidate.snapshot.file_bytes_for_path(commondir),
        Some(b"../..\n".as_slice())
    );
}

#[test]
fn coalescing_excludes_out_of_root_worktree_registration() {
    let workspace = TempWorkspace::new("coalesce-external-worktree-admin").expect("workspace");
    let gitdir = "repo/.git/worktrees/feat/gitdir";
    let commondir = "repo/.git/worktrees/feat/commondir";
    let head = "repo/.git/worktrees/feat/HEAD";
    let reference = "repo/.git/worktrees/feat/refs/heads/feat";
    write_file(workspace.root(), gitdir, b"/elsewhere/repo/feat/.git\n");
    write_file(workspace.root(), commondir, b"../..\n");
    write_file(workspace.root(), head, b"ref: refs/heads/feat\n");
    write_file(
        workspace.root(),
        reference,
        b"0123456789012345678901234567890123456789\n",
    );

    let candidate = coalesce_workspace_report(CoalesceWorkspaceReportRequest {
        root: workspace.root(),
        report: report_for_files(&[gitdir, commondir, head, reference]),
        workspace_id: WorkspaceId::new("ws_code"),
        base_ref: &base_ref(),
        device_id: DeviceId::new("device-test"),
        workspace_content_key: [42_u8; 32],
        created_at: "2026-07-08T10:00:00Z".to_string(),
        context: super::CoalesceContext::empty(),
    })
    .expect("coalesce");

    for path in [gitdir, commondir, head, reference] {
        assert!(
            candidate
                .snapshot
                .entries_for_test()
                .iter()
                .all(|entry| entry.path != path),
            "{path} should stay machine-local"
        );
        assert!(candidate.snapshot.file_bytes_for_path(path).is_none());
    }
}

#[test]
fn coalescing_keeps_in_root_worktree_registration() {
    let workspace = TempWorkspace::new("coalesce-in-root-worktree-admin").expect("workspace");
    let gitdir = "repo/.git/worktrees/feat/gitdir";
    let head = "repo/.git/worktrees/feat/HEAD";
    write_file(
        workspace.root(),
        gitdir,
        format!(
            "{}/repo/.claude/worktrees/feat/.git\n",
            workspace.root().display()
        )
        .as_bytes(),
    );
    write_file(workspace.root(), head, b"ref: refs/heads/worktree-feat\n");

    let candidate = coalesce_workspace_report(CoalesceWorkspaceReportRequest {
        root: workspace.root(),
        report: report_for_files(&[gitdir, head]),
        workspace_id: WorkspaceId::new("ws_code"),
        base_ref: &base_ref(),
        device_id: DeviceId::new("device-test"),
        workspace_content_key: [43_u8; 32],
        created_at: "2026-07-08T10:00:00Z".to_string(),
        context: super::CoalesceContext::empty(),
    })
    .expect("coalesce");

    let expected_gitdir = format!("{WORKSPACE_ROOT_MARKER}/repo/.claude/worktrees/feat/.git\n");
    assert_eq!(
        candidate.snapshot.file_bytes_for_path(gitdir),
        Some(expected_gitdir.as_bytes())
    );
    assert_eq!(
        candidate.snapshot.file_bytes_for_path(head),
        Some(b"ref: refs/heads/worktree-feat\n".as_slice())
    );
}

#[test]
fn coalescing_normalizes_absolute_worktree_commondir() {
    let workspace = TempWorkspace::new("coalesce-worktree-commondir").expect("workspace");
    let commondir = "repo/.git/worktrees/feat/commondir";
    write_file(
        workspace.root(),
        commondir,
        format!("{}/repo/.git\n", workspace.root().display()).as_bytes(),
    );

    let candidate = coalesce_workspace_report(CoalesceWorkspaceReportRequest {
        root: workspace.root(),
        report: report_for_files(&[commondir]),
        workspace_id: WorkspaceId::new("ws_code"),
        base_ref: &base_ref(),
        device_id: DeviceId::new("device-test"),
        workspace_content_key: [35_u8; 32],
        created_at: "2026-07-04T10:00:00Z".to_string(),
        context: super::CoalesceContext::empty(),
    })
    .expect("coalesce");

    let expected = format!("{WORKSPACE_ROOT_MARKER}/repo/.git\n");
    assert_eq!(
        candidate.snapshot.file_bytes_for_path(commondir),
        Some(expected.as_bytes())
    );
}

#[test]
fn coalescing_keeps_normal_files_byte_identical_when_marker_like_text_appears() {
    let workspace = TempWorkspace::new("coalesce-normal-marker").expect("workspace");
    let path = "src/main.rs";
    let bytes = format!("const ROOT: &str = \"{WORKSPACE_ROOT_MARKER}\";\n");
    write_file(workspace.root(), path, bytes.as_bytes());

    let candidate = coalesce_workspace_report(CoalesceWorkspaceReportRequest {
        root: workspace.root(),
        report: report_for_files(&[path]),
        workspace_id: WorkspaceId::new("ws_code"),
        base_ref: &base_ref(),
        device_id: DeviceId::new("device-test"),
        workspace_content_key: [33_u8; 32],
        created_at: "2026-07-04T10:00:00Z".to_string(),
        context: super::CoalesceContext::empty(),
    })
    .expect("coalesce");

    assert_eq!(
        candidate.snapshot.file_bytes_for_path(path),
        Some(bytes.as_bytes())
    );
}

#[test]
fn coalescing_preserves_existing_git_index_entries() {
    let workspace = TempWorkspace::new("coalesce-git-preserved").expect("workspace");
    let preserved = NamespaceEntry {
        path: "repo/.git/index".to_string(),
        kind: NamespaceEntryKind::File,
        classification: PathClassification::WorkspaceSync,
        mode: MaterializationMode::EncryptedSync,
        access: vec![AccessFlag::HumanReadable, AccessFlag::AgentHidden],
        content_id: Some(ContentId::new("cid_git_index")),
        content_layout: None,
        symlink_target: None,
        byte_len: Some(5),
        executability: FileExecutability::Regular,
        hydration_state: HydrationState::Local,
    };

    let candidate = coalesce_workspace_report(CoalesceWorkspaceReportRequest {
        root: workspace.root(),
        report: report_for_files(&[]),
        workspace_id: WorkspaceId::new("ws_code"),
        base_ref: &base_ref(),
        device_id: DeviceId::new("device-test"),
        workspace_content_key: [15_u8; 32],
        created_at: "2026-06-26T16:45:00Z".to_string(),
        context: super::CoalesceContext {
            paths: &super::EMPTY_PATH_SET,
            prior_snapshot: None,
            namespace_cancellation: None,
            preserved_entries: std::slice::from_ref(&preserved),
            file_overrides: &super::EMPTY_FILE_OVERRIDES,
            base_locators: &super::EMPTY_BASE_LOCATORS,
            preparation_root: None,
        },
    })
    .expect("coalesce");

    assert_eq!(candidate.snapshot.entries_for_test(), vec![preserved]);
}

#[test]
fn coalescing_preserves_worktree_link_entries() {
    let workspace = TempWorkspace::new("coalesce-worktree-preserved").expect("workspace");
    let preserved = NamespaceEntry {
        path: "repo/.git/worktrees/feat/gitdir".to_string(),
        kind: NamespaceEntryKind::File,
        classification: PathClassification::WorkspaceSync,
        mode: MaterializationMode::EncryptedSync,
        access: vec![AccessFlag::HumanReadable, AccessFlag::AgentHidden],
        content_id: Some(ContentId::new("cid_worktree_gitdir")),
        content_layout: None,
        symlink_target: None,
        byte_len: Some(38),
        executability: FileExecutability::Regular,
        hydration_state: HydrationState::Local,
    };

    let candidate = coalesce_workspace_report(CoalesceWorkspaceReportRequest {
        root: workspace.root(),
        report: report_for_files(&[]),
        workspace_id: WorkspaceId::new("ws_code"),
        base_ref: &base_ref(),
        device_id: DeviceId::new("device-test"),
        workspace_content_key: [17_u8; 32],
        created_at: "2026-07-04T10:00:00Z".to_string(),
        context: super::CoalesceContext {
            paths: &super::EMPTY_PATH_SET,
            prior_snapshot: None,
            namespace_cancellation: None,
            preserved_entries: &[preserved],
            file_overrides: &super::EMPTY_FILE_OVERRIDES,
            base_locators: &super::EMPTY_BASE_LOCATORS,
            preparation_root: None,
        },
    })
    .expect("coalesce");

    assert_eq!(
        candidate
            .snapshot
            .entries_for_test()
            .iter()
            .map(|entry| entry.path.as_str())
            .collect::<Vec<_>>(),
        vec!["repo/.git/worktrees/feat/gitdir"]
    );
}

#[test]
fn coalesce_binds_base_locator_for_unchanged_content_and_keeps_merge_bytes() {
    let workspace = TempWorkspace::new("coalesce-reuse-unchanged").expect("workspace");
    fs::create_dir_all(workspace.root().join("app/src")).expect("source parent");
    fs::write(workspace.root().join("app/src/main.rs"), b"fn main() {}\n").expect("source");
    let content_id =
        bowline_core::workspace_graph::workspace_content_id([21_u8; 32], b"fn main() {}\n");
    let locator = packed_locator(
        content_id.clone(),
        PackId::new("pk_reused"),
        b"fn main() {}\n".len() as u64,
        0,
    );
    let layout = ContentLayout::single_segment(locator.clone()).expect("test layout");
    let base_locators = BTreeMap::from([(content_id.clone(), layout.clone())]);

    let candidate = coalesce_workspace_report(CoalesceWorkspaceReportRequest {
        root: workspace.root(),
        report: stale_file_report("app/src/main.rs", Some(1)),
        workspace_id: WorkspaceId::new("ws_code"),
        base_ref: &base_ref(),
        device_id: DeviceId::new("device-test"),
        workspace_content_key: [21_u8; 32],
        created_at: "2026-07-03T10:00:00Z".to_string(),
        context: super::CoalesceContext {
            paths: &BTreeSet::new(),
            prior_snapshot: None,
            namespace_cancellation: None,
            preserved_entries: &[],
            file_overrides: &BTreeMap::new(),
            base_locators: &base_locators,
            preparation_root: None,
        },
    })
    .expect("coalesce");

    let entry = candidate
        .snapshot
        .entries_for_test()
        .into_iter()
        .find(|entry| entry.path == "app/src/main.rs")
        .expect("source entry");
    assert_eq!(entry.content_layout, Some(layout));
    assert_eq!(entry.hydration_state, HydrationState::Local);
    assert_eq!(
        candidate
            .snapshot
            .prepared_content()
            .get(&content_id)
            .and_then(PreparedContent::resident_bytes)
            .expect("merge bytes retained"),
        b"fn main() {}\n"
    );
}

#[test]
fn coalesce_reuses_locator_for_renamed_identical_content() {
    let workspace = TempWorkspace::new("coalesce-reuse-rename").expect("workspace");
    fs::write(workspace.root().join("renamed.txt"), b"same bytes\n").expect("source");
    let content_id =
        bowline_core::workspace_graph::workspace_content_id([22_u8; 32], b"same bytes\n");
    let locator = packed_locator(
        content_id.clone(),
        PackId::new("pk_same"),
        b"same bytes\n".len() as u64,
        0,
    );
    let layout = ContentLayout::single_segment(locator).expect("test layout");
    let base_locators = BTreeMap::from([(content_id, layout.clone())]);

    let candidate = coalesce_workspace_report(CoalesceWorkspaceReportRequest {
        root: workspace.root(),
        report: stale_file_report("renamed.txt", Some(1)),
        workspace_id: WorkspaceId::new("ws_code"),
        base_ref: &base_ref(),
        device_id: DeviceId::new("device-test"),
        workspace_content_key: [22_u8; 32],
        created_at: "2026-07-03T10:00:00Z".to_string(),
        context: super::CoalesceContext {
            paths: &BTreeSet::new(),
            prior_snapshot: None,
            namespace_cancellation: None,
            preserved_entries: &[],
            file_overrides: &BTreeMap::new(),
            base_locators: &base_locators,
            preparation_root: None,
        },
    })
    .expect("coalesce");

    assert_eq!(
        candidate.snapshot.entries_for_test()[0]
            .content_layout
            .as_ref(),
        Some(&layout)
    );
}

#[test]
fn snapshot_id_identical_with_and_without_base_locators() {
    let workspace = TempWorkspace::new("coalesce-reuse-identity").expect("workspace");
    fs::write(workspace.root().join("file.txt"), b"stable\n").expect("source");
    let content_id = bowline_core::workspace_graph::workspace_content_id([23_u8; 32], b"stable\n");
    let base_locators = BTreeMap::from([(
        content_id.clone(),
        ContentLayout::single_segment(packed_locator(
            content_id,
            PackId::new("pk_same"),
            b"stable\n".len() as u64,
            0,
        ))
        .expect("test layout"),
    )]);
    let report = stale_file_report("file.txt", Some(1));

    let without = coalesce_workspace_report(CoalesceWorkspaceReportRequest {
        root: workspace.root(),
        report: report.clone(),
        workspace_id: WorkspaceId::new("ws_code"),
        base_ref: &base_ref(),
        device_id: DeviceId::new("device-test"),
        workspace_content_key: [23_u8; 32],
        created_at: "2026-07-03T10:00:00Z".to_string(),
        context: super::CoalesceContext::empty(),
    })
    .expect("coalesce without locators");
    let with = coalesce_workspace_report(CoalesceWorkspaceReportRequest {
        root: workspace.root(),
        report,
        workspace_id: WorkspaceId::new("ws_code"),
        base_ref: &base_ref(),
        device_id: DeviceId::new("device-test"),
        workspace_content_key: [23_u8; 32],
        created_at: "2026-07-03T10:00:00Z".to_string(),
        context: super::CoalesceContext {
            paths: &BTreeSet::new(),
            prior_snapshot: None,
            namespace_cancellation: None,
            preserved_entries: &[],
            file_overrides: &BTreeMap::new(),
            base_locators: &base_locators,
            preparation_root: None,
        },
    })
    .expect("coalesce with locators");

    assert_eq!(
        with.snapshot.manifest.snapshot_id,
        without.snapshot.manifest.snapshot_id
    );
}

#[test]
fn combined_dispatch_merges_scoped_and_shallow_reports() {
    let workspace = TempWorkspace::new("coalesce-combined-dispatch").expect("workspace");
    write_file(workspace.root(), "README.md", b"root file\n");
    write_file(workspace.root(), "src/app.rs", b"fn main() {}\n");
    let workspace_id = WorkspaceId::new("ws_combined");
    let content_key = [21_u8; 32];
    let roots = BTreeSet::from(["src".to_string()]);

    let mut session = StatCacheSession::empty_for_scan(1, &content_key);
    let candidate = coalesce_workspace_scan_cached(CoalesceScanRequest {
        root: workspace.root(),
        workspace_id,
        base_ref: &base_ref(),
        device_id: DeviceId::new("device-test"),
        workspace_content_key: content_key,
        created_at: "2026-07-06T10:00:00Z".to_string(),
        context: super::CoalesceContext::empty(),
        stat_cache: Some(&mut session),
        scan_scope: ScanScope::DirtySubtrees {
            roots,
            root_shallow: true,
        },
    })
    .expect("combined coalesce");

    let paths = candidate
        .snapshot
        .entries_for_test()
        .into_iter()
        .map(|entry| entry.path.as_str().to_string())
        .collect::<BTreeSet<_>>();
    // The root-level file (shallow pass) and the deep dirty file (scoped pass) are
    // both present in the merged manifest.
    assert!(
        paths.contains("README.md"),
        "root file present, got {paths:?}"
    );
    assert!(
        paths.contains("src/app.rs"),
        "deep dirty file present, got {paths:?}"
    );
}

#[test]
fn missing_scoped_root_prunes_manifest_and_stat_cache_without_masking_deletion() {
    let workspace = TempWorkspace::new("coalesce-missing-root").expect("workspace");
    // `oldrepo` was deleted/renamed away: it is absent from disk this tick.
    write_file(workspace.root(), "keep.txt", b"still here\n");
    let workspace_id = WorkspaceId::new("ws_missing_root");
    let content_key = [42_u8; 32];
    let metadata_dir = workspace
        .root()
        .parent()
        .expect("workspace parent")
        .join("missing-root-state");
    let _ = fs::remove_dir_all(&metadata_dir);
    fs::create_dir_all(&metadata_dir).expect("metadata dir");
    let mut store = MetadataStore::open(metadata_dir.join(DEFAULT_DATABASE_FILE)).expect("store");
    store
        .insert_workspace(&workspace_id, "Code", "2026-07-06T10:00:00Z")
        .expect("workspace");
    store
        .apply_stat_cache_write_back(
            &workspace_id,
            &[
                stale_cache_row("oldrepo/a.rs"),
                stale_cache_row("oldrepo/nested/b.rs"),
            ],
            &BTreeSet::new(),
        )
        .expect("seed stat cache");

    let roots = BTreeSet::from(["oldrepo".to_string()]);
    let mut session = StatCacheSession::load_scoped(&store, &workspace_id, &roots, 1, &content_key)
        .expect("scoped session");
    let candidate = coalesce_workspace_scan_cached(CoalesceScanRequest {
        root: workspace.root(),
        workspace_id: workspace_id.clone(),
        base_ref: &base_ref(),
        device_id: DeviceId::new("device-test"),
        workspace_content_key: content_key,
        created_at: "2026-07-06T10:01:00Z".to_string(),
        // The runner preserves only head entries outside the dirty roots; nothing
        // under `oldrepo` is re-injected, so the deletion is authoritative.
        context: super::CoalesceContext::empty(),
        stat_cache: Some(&mut session),
        scan_scope: ScanScope::DirtySubtrees {
            roots,
            root_shallow: false,
        },
    })
    .expect("scoped coalesce");

    let paths = candidate
        .snapshot
        .entries_for_test()
        .iter()
        .map(|entry| entry.path.clone())
        .collect::<BTreeSet<_>>();
    assert!(
        !paths.iter().any(|path| path.starts_with("oldrepo")),
        "manifest must drop all oldrepo/* entries, got {paths:?}"
    );

    let write_back = candidate
        .stat_cache_write_back
        .expect("scoped write-back present");
    assert_eq!(
        write_back.deletes,
        BTreeSet::from([
            "oldrepo/a.rs".to_string(),
            "oldrepo/nested/b.rs".to_string(),
        ]),
        "stat-cache rows under the missing root are pruned"
    );

    // Apply the write-back and confirm the store no longer holds oldrepo rows.
    store
        .apply_stat_cache_write_back(&workspace_id, &write_back.upserts, &write_back.deletes)
        .expect("apply write-back");
    let remaining = store.stat_cache_rows(&workspace_id).expect("rows");
    assert!(!remaining.keys().any(|path| path.starts_with("oldrepo")));
}

fn stale_cache_row(path: &str) -> crate::sync::stat_cache::StatCacheRow {
    use crate::sync::stat_cache::{ContentKeyFingerprint, FileTimestampNanos, StatFingerprint};
    crate::sync::stat_cache::StatCacheRow {
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
        content_id: ContentId::new("cid_old"),
        byte_len: 8,
        format_version: crate::sync::stat_cache::STAT_CACHE_FORMAT_VERSION,
        hashed_at_ns: FileTimestampNanos::new(20),
        last_verified_at: "2026-07-06T00:00:00Z".to_string(),
    }
}

fn stale_file_report(path: &str, byte_len: Option<u64>) -> ScanReport {
    ScanReport {
        root: PathBuf::from("/tmp/bowline-stale-report"),
        projects: Vec::new(),
        paths: vec![PathObservation {
            path: path.to_string(),
            project_id: None,
            is_dir: false,
            is_symlink: false,
            byte_len,
            stat: None,
            executability: FileExecutability::Regular,
            policy: classify_path_with_builtin_policy(path),
        }],
        summary: ObservedWorkspaceSummary::default(),
    }
}

fn report_for_files(paths: &[&str]) -> ScanReport {
    ScanReport {
        root: PathBuf::from("/tmp/bowline-report"),
        projects: Vec::new(),
        paths: paths
            .iter()
            .map(|path| PathObservation {
                path: (*path).to_string(),
                project_id: None,
                is_dir: false,
                is_symlink: false,
                byte_len: Some(1),
                stat: None,
                executability: FileExecutability::Regular,
                policy: classify_path_with_builtin_policy(*path),
            })
            .collect(),
        summary: ObservedWorkspaceSummary::default(),
    }
}

fn write_file(root: &std::path::Path, path: &str, bytes: &[u8]) {
    let absolute = root.join(path);
    fs::create_dir_all(absolute.parent().expect("parent")).expect("parent");
    fs::write(absolute, bytes).expect("file bytes");
}

fn verify_timestamp_for_path(path: &str) -> String {
    let seconds = i64::try_from(verify_shard_for_path(path)).expect("verify shard fits i64") * 600;
    time::OffsetDateTime::from_unix_timestamp(seconds)
        .expect("valid timestamp")
        .format(&time::format_description::well_known::Rfc3339)
        .expect("timestamp formats")
}

fn report_for_file_with_executability(path: &str, executability: FileExecutability) -> ScanReport {
    let mut report = stale_file_report(path, Some(1));
    report.paths[0].executability = executability;
    report
}

fn base_ref() -> RemoteWorkspaceRef {
    RemoteWorkspaceRef {
        workspace_id: WorkspaceId::new("ws_code"),
        version: 7,
        snapshot_id: SnapshotId::new("snap_base"),
        updated_at: ControlPlaneTimestamp { tick: 7 },
        updated_by_device_id: Some(DeviceId::new("device-peer")),
    }
}

fn fixed_identity_entries() -> Vec<NamespaceEntry> {
    vec![
        NamespaceEntry {
            path: "app".to_string(),
            kind: NamespaceEntryKind::Directory,
            classification: PathClassification::WorkspaceSync,
            mode: MaterializationMode::WorkspaceSync,
            access: vec![AccessFlag::HumanReadable],
            content_id: None,
            content_layout: None,
            symlink_target: None,
            byte_len: None,
            executability: FileExecutability::Regular,
            hydration_state: HydrationState::StructureOnly,
        },
        NamespaceEntry {
            path: "app/bin/tool".to_string(),
            kind: NamespaceEntryKind::Symlink,
            classification: PathClassification::WorkspaceSync,
            mode: MaterializationMode::Lazy,
            access: vec![AccessFlag::AgentHidden],
            content_id: None,
            content_layout: None,
            symlink_target: Some("../target/debug/tool".to_string()),
            byte_len: Some(20),
            executability: FileExecutability::Regular,
            hydration_state: HydrationState::Local,
        },
        NamespaceEntry {
            path: "app/src/main.rs".to_string(),
            kind: NamespaceEntryKind::File,
            classification: PathClassification::ProjectEnv,
            mode: MaterializationMode::EncryptedSync,
            access: vec![AccessFlag::HumanReadable, AccessFlag::AgentHidden],
            content_id: Some(ContentId::new("cid_main")),
            content_layout: None,
            symlink_target: None,
            byte_len: Some(42),
            executability: FileExecutability::Executable,
            hydration_state: HydrationState::Cold,
        },
    ]
}

fn packed_locator(
    content_id: ContentId,
    pack_id: PackId,
    raw_size: u64,
    offset: u64,
) -> ContentLocator {
    ContentLocator {
        content_id,
        storage: ContentStorage::Packed,
        raw_size,
        pack_id: Some(pack_id),
        offset: Some(offset),
        length: Some(raw_size),
    }
}
