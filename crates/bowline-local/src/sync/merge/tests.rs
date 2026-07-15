use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
};

use bowline_core::{
    ids::{ContentId, DeviceId, ManifestId, SnapshotId, WorkspaceId},
    policy::{AccessFlag, MaterializationMode, PathClassification},
    workspace_graph::{
        FileExecutability, HydrationState, NamespaceEntry, NamespaceEntryKind, RefKind,
        SnapshotDraft, SnapshotKind, WorkspaceRef, workspace_content_id,
    },
};

use super::*;
use crate::{
    sync::{ConflictKind, FullScanReason, ScanScope, merge_plugins::MergePluginRegistry},
    workspace::TempWorkspace,
};

const KEY: [u8; 32] = [7; 32];

#[test]
fn merge_hydration_plan_is_empty_for_equal_and_one_sided_changes() {
    let base = snapshot("base", "src/main.rs", b"base\n");
    let unchanged = snapshot("unchanged", "src/main.rs", b"base\n");
    let changed = snapshot("changed", "src/main.rs", b"changed\n");

    let equal = merge_required_content_paths(&MergeTreeInput {
        base: &base,
        left: &unchanged,
        right: &unchanged,
        workspace_content_key: KEY,
    })
    .expect("equal structure plan");
    let one_sided = merge_required_content_paths(&MergeTreeInput {
        base: &base,
        left: &changed,
        right: &unchanged,
        workspace_content_key: KEY,
    })
    .expect("one-sided structure plan");

    assert!(equal.is_empty());
    assert!(one_sided.is_empty());
}

#[test]
fn merge_hydration_plan_selects_only_dual_changed_file_bytes() {
    let base = snapshot("base", "src/main.rs", b"base\n");
    let left = snapshot("left", "src/main.rs", b"left\n");
    let right = snapshot("right", "src/main.rs", b"right\n");

    let required = merge_required_content_paths(&MergeTreeInput {
        base: &base,
        left: &left,
        right: &right,
        workspace_content_key: KEY,
    })
    .expect("dual-change structure plan");

    assert_eq!(required, BTreeSet::from(["src/main.rs".to_string()]));
}

#[test]
fn merge_tree_preserves_right_only_path_outside_projected_branch_base() {
    let exposed_base = empty_snapshot("exposed-base");
    let untouched_work = empty_snapshot("work");
    let current_main = snapshot("main", "id_rsa", b"private-key-sentinel\n");

    let merged = merge_tree(MergeTreeInput {
        base: &exposed_base,
        left: &untouched_work,
        right: &current_main,
        workspace_content_key: KEY,
    })
    .expect("projected namespace merge succeeds");

    let MergeTreeOutcome::Clean(merged) = merged else {
        panic!("untouched projected branch should merge cleanly");
    };
    assert_eq!(merged.entries.len(), 1);
    assert_eq!(merged.entries[0].path, "id_rsa");
    let content_id = merged.entries[0]
        .content_id
        .as_ref()
        .expect("preserved content id");
    assert_eq!(
        merged.prepared_content[content_id].resident_bytes(),
        Some(&b"private-key-sentinel\n"[..])
    );
}

#[test]
fn stale_sync_adapter_preserves_candidate_metadata_contract() {
    let base = snapshot("base", "src/main.rs", b"base\n");
    let mut local = candidate(&base, "local", "src/main.rs", b"local\n");
    local.causation_ids = vec!["scan:local".to_string()];
    local.stat_cache_hit_paths.insert("src/main.rs".to_string());
    let remote = snapshot("remote", "src/main.rs", b"base\n");
    let remote_base = CandidateBase {
        workspace_id: WorkspaceId::new("ws_code"),
        version: 9,
        snapshot_id: SnapshotId::new("remote"),
    };

    let merged = merge_snapshots(
        &base,
        &local,
        &remote,
        remote_base.clone(),
        KEY,
        "2026-07-13T12:00:00Z",
    )
    .expect("stale merge succeeds");

    let MergeOutcome::Clean(merged) = merged else {
        panic!("local-only edit should merge cleanly");
    };
    assert_eq!(merged.base, remote_base);
    assert_eq!(merged.device_id, local.device_id);
    assert_eq!(merged.scan_report, local.scan_report);
    assert_eq!(merged.scan_scope, local.scan_scope);
    assert_eq!(merged.stat_cache_hit_paths, local.stat_cache_hit_paths);
    assert_eq!(merged.stat_cache_divergences, local.stat_cache_divergences);
    assert_eq!(
        merged.skipped_unsafe_symlinks,
        local.skipped_unsafe_symlinks
    );
    assert_eq!(
        merged.causation_ids,
        vec![
            "scan:local".to_string(),
            format!("merge:{}", remote.manifest().snapshot_id.as_str()),
        ]
    );
    assert_eq!(merged.created_at, "2026-07-13T12:00:00Z");
}

#[test]
fn merge_snapshot_id_pins_identity_without_hydration() {
    let mut dir = entry("app", ContentId::new(""), 0);
    dir.kind = NamespaceEntryKind::Directory;
    (dir.content_id, dir.byte_len) = (None, None);
    dir.hydration_state = HydrationState::Cold;
    let mut symlink = entry("app/current", ContentId::new(""), 0);
    symlink.kind = NamespaceEntryKind::Symlink;
    (symlink.content_id, symlink.byte_len) = (None, None);
    symlink.symlink_target = Some("releases/current".to_string());
    let mut file = entry("app/src/main.rs", ContentId::new("cid_main"), 42);
    file.hydration_state = HydrationState::Cold;
    file.executability = FileExecutability::Executable;
    let entries = vec![dir, symlink, file];
    let workspace_id = WorkspaceId::new("ws_code");
    let id = merge_snapshot_id_for_entries(&workspace_id, &entries);

    // Golden snapshot ID for the plan-090 chunked identity (schema v3 re-key).
    // If this assertion ever fails, the identity encoding drifted, which
    // re-identifies every snapshot fleet-wide. Fix the encoding, never the
    // constant except a deliberate vN bump.
    assert_eq!(id.as_str(), "snap_2bedcaff8e1e596ff855b84e");

    for hydration_state in [
        HydrationState::Local,
        HydrationState::Cold,
        HydrationState::StructureOnly,
        HydrationState::Missing,
    ] {
        let mut entries_with_state = entries.clone();
        for entry in &mut entries_with_state {
            entry.hydration_state = hydration_state;
        }
        assert_eq!(
            merge_snapshot_id_for_entries(&workspace_id, &entries_with_state),
            id
        );
    }
}

#[test]
fn production_clean_merge_uses_versioned_snapshot_identity() {
    let base = snapshot("base", "app/src/main.rs", b"fn main() {}\n");
    let local = candidate(
        &base,
        "local",
        "app/src/main.rs",
        b"fn main() { println!(\"hi\"); }\n",
    );
    let remote = snapshot("remote", "app/src/main.rs", b"fn main() {}\n");

    let merged = merge_snapshots(
        &base,
        &local,
        &remote,
        CandidateBase {
            workspace_id: WorkspaceId::new("ws_code"),
            version: 3,
            snapshot_id: SnapshotId::new("remote"),
        },
        KEY,
        "2026-07-03T12:00:00Z",
    )
    .expect("merge succeeds");

    let MergeOutcome::Clean(merged) = merged else {
        panic!("local-only edit should merge cleanly");
    };
    assert_eq!(
        merged.snapshot.manifest.snapshot_id.as_str(),
        "snap_ca030b3164d5d5e83490c2cc"
    );
    assert_eq!(merged.manifest_id.as_str(), "mf_e60fe36b135fc3fd42b20fe1");
    assert!(merged.snapshot.manifest.base_snapshot_id.is_none());
}

#[test]
fn production_clean_merge_identity_ignores_hydration_state() {
    let base = snapshot("base", "app/src/main.rs", b"fn main() {}\n");
    let mut local = candidate(
        &base,
        "local",
        "app/src/main.rs",
        b"fn main() { println!(\"hi\"); }\n",
    );
    let remote = snapshot("remote", "app/src/main.rs", b"fn main() {}\n");
    let first = clean_merge_snapshot_id(&base, &local, &remote);

    for hydration_state in [
        HydrationState::Local,
        HydrationState::Cold,
        HydrationState::StructureOnly,
        HydrationState::Missing,
    ] {
        local.snapshot.mutate_entries_for_test(|entries| {
            for entry in entries {
                entry.hydration_state = hydration_state;
            }
        });
        assert_eq!(clean_merge_snapshot_id(&base, &local, &remote), first);
    }
}

#[test]
fn paged_merge_propagates_operation_cancellation() {
    struct Cancelled;
    impl bowline_core::namespace_snapshot::NamespaceCancellation for Cancelled {
        fn is_cancelled(&self) -> bool {
            true
        }
    }

    let base = snapshot("base-cancel", "src/lib.rs", b"base\n");
    let local = candidate(&base, "local-cancel", "src/lib.rs", b"local\n");
    let remote = snapshot("remote-cancel", "src/lib.rs", b"remote\n");
    let plugins = MergePluginRegistry::built_in();

    let error = merge_snapshots_with_plugins(
        &base,
        &local,
        &remote,
        MergeSnapshotsOptions {
            remote_base: CandidateBase {
                workspace_id: WorkspaceId::new("ws_code"),
                version: 1,
                snapshot_id: SnapshotId::new("remote-cancel"),
            },
            workspace_content_key: KEY,
            created_at: "2026-07-14T00:00:00Z".to_string(),
            plugins: &plugins,
            cancellation: Some(&Cancelled),
        },
    )
    .expect_err("cancelled merge");

    assert!(matches!(
        error,
        MergeError::NamespaceRead(NamespaceReadError::Cancelled)
            | MergeError::NamespaceBuild(
                bowline_core::namespace_snapshot::NamespaceBuildError::Read(
                    NamespaceReadError::Cancelled
                )
            )
    ));
}

#[test]
fn paged_merge_budget_includes_unchanged_result_entries() {
    let base_files = (0..113)
        .map(|index| (format!("src/file-{index:03}.txt"), b"base\n".to_vec()))
        .collect::<Vec<_>>();
    let mut local_files = base_files.clone();
    local_files[0].1 = b"local\n".to_vec();
    let mut remote_files = base_files.clone();
    remote_files[1].1 = b"remote\n".to_vec();
    let base = snapshot_with_files(base_files);
    let local = snapshot_with_files(local_files);
    let remote = snapshot_with_files(remote_files);

    let merged = merge_paged_snapshots(
        &base,
        &local,
        &remote,
        KEY,
        &MergePluginRegistry::built_in(),
        None,
    )
    .expect("large mostly-unchanged namespace merges within its semantic budget");
    let PagedMergeOutcome::Clean { namespace, .. } = merged else {
        panic!("different-path edits should merge cleanly");
    };
    assert_eq!(namespace.metadata.entry_count, 113);
}

#[test]
fn exec_flip_alone_merges_to_changed_side() {
    let base = snapshot("base", "app/bin/tool", b"tool\n");
    let mut local = candidate(&base, "local", "app/bin/tool", b"tool\n");
    local.snapshot.mutate_entries_for_test(|entries| {
        entries[0].executability = FileExecutability::Executable;
    });
    let remote = snapshot("remote", "app/bin/tool", b"tool\n");

    let merged = clean_merged_snapshot(&base, &local, &remote);

    assert_eq!(
        merged.snapshot.entries_for_test()[0].executability,
        FileExecutability::Executable
    );
    assert_eq!(
        merged.snapshot.file_bytes_for_path("app/bin/tool"),
        Some(&b"tool\n"[..])
    );
}

#[test]
fn exec_flip_survives_content_merge_from_other_side() {
    let base = snapshot("base", "app/bin/tool", b"line one\n");
    let mut local = candidate(&base, "local", "app/bin/tool", b"line one\n");
    local.snapshot.mutate_entries_for_test(|entries| {
        entries[0].executability = FileExecutability::Executable;
    });
    let remote = snapshot("remote", "app/bin/tool", b"line one\nremote line\n");

    let merged = clean_merged_snapshot(&base, &local, &remote);

    assert_eq!(
        merged.snapshot.entries_for_test()[0].executability,
        FileExecutability::Executable
    );
    assert_eq!(
        merged.snapshot.file_bytes_for_path("app/bin/tool"),
        Some(&b"line one\nremote line\n"[..])
    );
}

#[test]
fn remote_exec_flip_survives_content_merge_from_local_side() {
    let base = snapshot("base", "app/bin/tool", b"line one\n");
    let local = candidate(&base, "local", "app/bin/tool", b"line one\nlocal line\n");
    let mut remote = snapshot("remote", "app/bin/tool", b"line one\n");
    remote.mutate_entries_for_test(|entries| {
        entries[0].executability = FileExecutability::Executable;
    });

    let merged = clean_merged_snapshot(&base, &local, &remote);

    assert_eq!(
        merged.snapshot.entries_for_test()[0].executability,
        FileExecutability::Executable
    );
    assert_eq!(
        merged.snapshot.file_bytes_for_path("app/bin/tool"),
        Some(&b"line one\nlocal line\n"[..])
    );
}

#[test]
fn exec_flip_survives_binary_content_change_from_other_side() {
    let base = snapshot("base", "app/bin/tool", b"\x00base");
    let mut local = candidate(&base, "local", "app/bin/tool", b"\x00base");
    local.snapshot.mutate_entries_for_test(|entries| {
        entries[0].executability = FileExecutability::Executable;
    });
    let remote = snapshot("remote", "app/bin/tool", b"\x00remote");

    let merged = clean_merged_snapshot(&base, &local, &remote);

    assert_eq!(
        merged.snapshot.entries_for_test()[0].executability,
        FileExecutability::Executable
    );
    assert_eq!(
        merged.snapshot.file_bytes_for_path("app/bin/tool"),
        Some(&b"\x00remote"[..])
    );
}

#[test]
fn remote_exec_flip_survives_binary_content_change_from_local_side() {
    let base = snapshot("base", "app/bin/tool", b"\x00base");
    let local = candidate(&base, "local", "app/bin/tool", b"\x00local");
    let mut remote = snapshot("remote", "app/bin/tool", b"\x00base");
    remote.mutate_entries_for_test(|entries| {
        entries[0].executability = FileExecutability::Executable;
    });

    let merged = clean_merged_snapshot(&base, &local, &remote);

    assert_eq!(
        merged.snapshot.entries_for_test()[0].executability,
        FileExecutability::Executable
    );
    assert_eq!(
        merged.snapshot.file_bytes_for_path("app/bin/tool"),
        Some(&b"\x00local"[..])
    );
}

#[test]
fn git_index_divergence_and_delete_edit_conflict() {
    let base = snapshot("base", ".git/index", b"base-index");
    let local = candidate(&base, "local", ".git/index", b"local-index");
    let remote = snapshot("remote", ".git/index", b"remote-index");
    assert_git_index_merge_conflicts(&base, &local, &remote);

    let mut local = candidate(&base, "local", ".git/index", b"local-index");
    local.snapshot = empty_snapshot("local");
    assert_git_index_merge_conflicts(&base, &local, &remote);
}

fn assert_git_index_merge_conflicts(
    base: &SnapshotContent,
    local: &SnapshotCandidate,
    remote: &SnapshotContent,
) {
    let merged = merge_snapshots(
        base,
        local,
        remote,
        CandidateBase {
            workspace_id: WorkspaceId::new("ws_code"),
            version: 3,
            snapshot_id: SnapshotId::new("remote"),
        },
        KEY,
        "2026-06-27T12:00:00Z",
    )
    .expect("merge succeeds");

    let MergeOutcome::Conflicted(conflicts) = merged else {
        panic!("git index divergence should surface as an opaque Git conflict");
    };
    assert_eq!(conflicts[0].paths, vec![".git/index".to_string()]);
}

#[test]
fn other_git_state_still_conflicts_when_both_sides_change() {
    let base = snapshot("base", ".git/HEAD", b"ref: refs/heads/main\n");
    let local = candidate(&base, "local", ".git/HEAD", b"ref: refs/heads/local\n");
    let remote = snapshot("remote", ".git/HEAD", b"ref: refs/heads/remote\n");

    let merged = merge_snapshots(
        &base,
        &local,
        &remote,
        CandidateBase {
            workspace_id: WorkspaceId::new("ws_code"),
            version: 3,
            snapshot_id: SnapshotId::new("remote"),
        },
        KEY,
        "2026-06-27T12:00:00Z",
    )
    .expect("merge succeeds");

    let MergeOutcome::Conflicted(conflicts) = merged else {
        panic!("non-index git state should stay opaque");
    };
    assert_eq!(conflicts[0].paths, vec![".git/HEAD".to_string()]);
}

#[test]
fn opaque_git_state_conflicts_when_content_change_races_exec_flip() {
    let base = snapshot("base", ".git/HEAD", b"ref: refs/heads/main\n");
    let mut local = candidate(&base, "local", ".git/HEAD", b"ref: refs/heads/main\n");
    local.snapshot.mutate_entries_for_test(|entries| {
        entries[0].executability = FileExecutability::Executable;
    });
    let remote = snapshot("remote", ".git/HEAD", b"ref: refs/heads/remote\n");

    let merged = merge_snapshots(
        &base,
        &local,
        &remote,
        CandidateBase {
            workspace_id: WorkspaceId::new("ws_code"),
            version: 3,
            snapshot_id: SnapshotId::new("remote"),
        },
        KEY,
        "2026-06-27T12:00:00Z",
    )
    .expect("merge succeeds");

    let MergeOutcome::Conflicted(conflicts) = merged else {
        panic!("opaque git state should not merge exec/content races");
    };
    assert_eq!(conflicts[0].paths, vec![".git/HEAD".to_string()]);
}

#[test]
fn env_different_key_edits_merge_without_secret_conflict() {
    let base = snapshot("base", ".env.local", b"API_KEY=old\nDATABASE_URL=old\n");
    let local = candidate(
        &base,
        "local",
        ".env.local",
        b"API_KEY=local\nDATABASE_URL=old\n",
    );
    let remote = snapshot(
        "remote",
        ".env.local",
        b"API_KEY=old\nDATABASE_URL=remote\n",
    );

    let merged = merge_snapshots(
        &base,
        &local,
        &remote,
        CandidateBase {
            workspace_id: WorkspaceId::new("ws_code"),
            version: 3,
            snapshot_id: SnapshotId::new("remote"),
        },
        KEY,
        "2026-06-27T12:00:00Z",
    )
    .expect("merge succeeds");

    let MergeOutcome::Clean(merged) = merged else {
        panic!("different env keys should merge");
    };
    assert_eq!(
        merged.snapshot.file_bytes_for_path(".env.local"),
        Some(&b"API_KEY=local\nDATABASE_URL=remote\n"[..])
    );
}

#[test]
fn env_merge_preserves_remote_non_key_edits() {
    let base = snapshot("base", ".env.local", b"API_KEY=old\n# old comment\n");
    let local = candidate(
        &base,
        "local",
        ".env.local",
        b"API_KEY=local\n# old comment\n",
    );
    let remote = snapshot("remote", ".env.local", b"API_KEY=old\n# new comment\n");

    let merged = merge_snapshots(
        &base,
        &local,
        &remote,
        CandidateBase {
            workspace_id: WorkspaceId::new("ws_code"),
            version: 3,
            snapshot_id: SnapshotId::new("remote"),
        },
        KEY,
        "2026-06-27T12:00:00Z",
    )
    .expect("merge succeeds");

    let MergeOutcome::Clean(merged) = merged else {
        panic!("key edit plus remote comment edit should merge");
    };
    assert_eq!(
        merged.snapshot.file_bytes_for_path(".env.local"),
        Some(&b"API_KEY=local\n# new comment\n"[..])
    );
}

#[test]
fn env_delete_single_key_and_edit_different_key_merges() {
    let base = snapshot("base", ".env.local", b"API_KEY=old\nDATABASE_URL=old\n");
    let local = candidate(&base, "local", ".env.local", b"DATABASE_URL=old\n");
    let remote = snapshot(
        "remote",
        ".env.local",
        b"API_KEY=old\nDATABASE_URL=remote\n",
    );

    let merged = merge_snapshots(
        &base,
        &local,
        &remote,
        CandidateBase {
            workspace_id: WorkspaceId::new("ws_code"),
            version: 3,
            snapshot_id: SnapshotId::new("remote"),
        },
        KEY,
        "2026-06-27T12:00:00Z",
    )
    .expect("merge succeeds");

    let MergeOutcome::Clean(merged) = merged else {
        panic!("single env key deletion plus different key edit should merge");
    };
    assert_eq!(
        merged.snapshot.file_bytes_for_path(".env.local"),
        Some(&b"DATABASE_URL=remote\n"[..])
    );
}

#[test]
fn env_duplicate_key_deletion_stays_secret_bearing_conflict() {
    let base = snapshot(
        "base",
        ".env.local",
        b"API_KEY=old\nAPI_KEY=older\nDATABASE_URL=old\n",
    );
    let local = candidate(
        &base,
        "local",
        ".env.local",
        b"API_KEY=old\nAPI_KEY=older\nDATABASE_URL=local\n",
    );
    let remote = snapshot("remote", ".env.local", b"API_KEY=older\nDATABASE_URL=old\n");

    let merged = merge_snapshots(
        &base,
        &local,
        &remote,
        CandidateBase {
            workspace_id: WorkspaceId::new("ws_code"),
            version: 3,
            snapshot_id: SnapshotId::new("remote"),
        },
        KEY,
        "2026-06-27T12:00:00Z",
    )
    .expect("merge succeeds");

    let MergeOutcome::Conflicted(conflicts) = merged else {
        panic!("duplicate env key deletion should stay ambiguous");
    };
    assert_eq!(conflicts[0].conflict_kind, ConflictKind::EnvKey);
    assert!(conflicts[0].contains_secrets);
}

#[test]
fn env_same_key_edits_create_secret_bearing_key_conflict() {
    let base = snapshot("base", ".env.local", b"API_KEY=old\n");
    let local = candidate(&base, "local", ".env.local", b"API_KEY=local\n");
    let remote = snapshot("remote", ".env.local", b"API_KEY=remote\n");

    let merged = merge_snapshots(
        &base,
        &local,
        &remote,
        CandidateBase {
            workspace_id: WorkspaceId::new("ws_code"),
            version: 3,
            snapshot_id: SnapshotId::new("remote"),
        },
        KEY,
        "2026-06-27T12:00:00Z",
    )
    .expect("merge succeeds");

    let MergeOutcome::Conflicted(conflicts) = merged else {
        panic!("same env key should conflict");
    };
    assert_eq!(conflicts[0].conflict_kind, ConflictKind::EnvKey);
    assert!(conflicts[0].contains_secrets);
}

#[test]
fn lockfile_edits_conflict_even_when_line_mergeable() {
    let base = snapshot("base", "pnpm-lock.yaml", b"a: 1\nb: 1\n");
    let local = candidate(&base, "local", "pnpm-lock.yaml", b"a: 2\nb: 1\n");
    let remote = snapshot("remote", "pnpm-lock.yaml", b"a: 1\nb: 2\n");

    let merged = merge_snapshots(
        &base,
        &local,
        &remote,
        CandidateBase {
            workspace_id: WorkspaceId::new("ws_code"),
            version: 3,
            snapshot_id: SnapshotId::new("remote"),
        },
        KEY,
        "2026-06-27T12:00:00Z",
    )
    .expect("merge succeeds");

    let MergeOutcome::Conflicted(conflicts) = merged else {
        panic!("lockfiles need semantic validation before automatic merge");
    };
    assert_eq!(conflicts[0].conflict_kind, ConflictKind::StructuredText);
}

#[test]
fn lockfile_guard_runs_before_external_plugin_matching() {
    let workspace = TempWorkspace::new("merge-lockfile-plugin-guard").expect("workspace");
    fs::write(
        workspace.root().join(".bowlinemerge.toml"),
        r#"
schema = 1

[[plugins]]
id = "lockfile"
version = "1.0.0"
digest = "blake3:missing"
module = ".bowline/plugins/missing.wasm"
match = ["pnpm-lock.yaml"]
"#,
    )
    .expect("config");
    let plugins =
        MergePluginRegistry::load_project(workspace.root(), &WorkspaceId::new("ws_code"), &[])
            .expect("registry")
            .registry;
    let base = snapshot("base", "pnpm-lock.yaml", b"a: 1\nb: 1\n");
    let local = candidate(&base, "local", "pnpm-lock.yaml", b"a: 2\nb: 1\n");
    let remote = snapshot("remote", "pnpm-lock.yaml", b"a: 1\nb: 2\n");

    let merged = merge_snapshots_with_plugins(
        &base,
        &local,
        &remote,
        MergeSnapshotsOptions {
            remote_base: CandidateBase {
                workspace_id: WorkspaceId::new("ws_code"),
                version: 3,
                snapshot_id: SnapshotId::new("remote"),
            },
            workspace_content_key: KEY,
            created_at: "2026-07-02T12:00:00Z".to_string(),
            plugins: &plugins,
            cancellation: None,
        },
    )
    .expect("merge succeeds");

    let MergeOutcome::Conflicted(conflicts) = merged else {
        panic!("lockfile should conflict before external plugins");
    };
    assert_eq!(conflicts[0].conflict_kind, ConflictKind::StructuredText);
}

#[test]
fn non_utf8_same_path_edits_create_binary_conflict() {
    let base = snapshot("base", "image.bin", &[0, 1, 2, 3]);
    let local = candidate(&base, "local", "image.bin", &[0, 1, 255, 3]);
    let remote = snapshot("remote", "image.bin", &[0, 1, 254, 3]);

    let merged = merge_snapshots(
        &base,
        &local,
        &remote,
        CandidateBase {
            workspace_id: WorkspaceId::new("ws_code"),
            version: 3,
            snapshot_id: SnapshotId::new("remote"),
        },
        KEY,
        "2026-06-27T12:00:00Z",
    )
    .expect("merge succeeds");

    let MergeOutcome::Conflicted(conflicts) = merged else {
        panic!("binary divergence should conflict");
    };
    assert_eq!(conflicts[0].conflict_kind, ConflictKind::Binary);
    assert_eq!(
        conflicts[0].reason,
        "text classification failed: binary-control-byte"
    );
}

#[test]
fn divergent_same_point_insertions_create_a_stable_text_conflict() {
    let base = snapshot("base", "notes.txt", b"a\nb\n");
    let local = candidate(&base, "local", "notes.txt", b"a\nlocal\nb\n");
    let remote = snapshot("remote", "notes.txt", b"a\nremote\nb\n");
    let merge = || {
        merge_snapshots(
            &base,
            &local,
            &remote,
            CandidateBase {
                workspace_id: WorkspaceId::new("ws_code"),
                version: 3,
                snapshot_id: SnapshotId::new("remote"),
            },
            KEY,
            "2026-07-13T12:00:00Z",
        )
        .expect("merge succeeds")
    };

    let MergeOutcome::Conflicted(first) = merge() else {
        panic!("divergent same-point insertions must conflict");
    };
    let MergeOutcome::Conflicted(second) = merge() else {
        panic!("repeated merge must conflict");
    };
    assert_eq!(first[0].conflict_kind, ConflictKind::Text);
    assert_eq!(first[0].reason, "text merge failed: incompatible-overlap");
    assert_eq!(first[0].id, second[0].id);
    assert!(!first[0].spans.is_empty());
}

#[test]
fn text_resource_exhaustion_creates_a_deterministic_durable_conflict() {
    let base_bytes = (0..2_048)
        .map(|index| format!("base-{index:04}\n"))
        .collect::<String>();
    let local_bytes = (0..2_048)
        .map(|index| format!("local-{index:04}\n"))
        .collect::<String>();
    let remote_bytes = (0..2_048)
        .map(|index| format!("remote-{index:04}\n"))
        .collect::<String>();
    let base = snapshot("base", "generated.txt", base_bytes.as_bytes());
    let local = candidate(&base, "local", "generated.txt", local_bytes.as_bytes());
    let remote = snapshot("remote", "generated.txt", remote_bytes.as_bytes());

    let MergeOutcome::Conflicted(conflicts) = merge_snapshots(
        &base,
        &local,
        &remote,
        CandidateBase {
            workspace_id: WorkspaceId::new("ws_code"),
            version: 3,
            snapshot_id: SnapshotId::new("remote"),
        },
        KEY,
        "2026-07-13T12:00:00Z",
    )
    .expect("resource exhaustion is a conflict, not an engine error") else {
        panic!("pathological text must stop conservatively");
    };
    assert_eq!(conflicts[0].conflict_kind, ConflictKind::Text);
    assert_eq!(conflicts[0].reason, "text merge failed: myers-trace-cells");
}

#[test]
fn missing_local_side_bytes_error_instead_of_merging_empty_content() {
    let base = snapshot("base", "app/src/main.rs", b"base\n");
    let mut local = candidate(&base, "local", "app/src/main.rs", b"local\n");
    let remote = snapshot("remote", "app/src/main.rs", b"remote\n");
    remove_bytes_for_path(&mut local.snapshot, "app/src/main.rs");

    let error = merge_snapshots(
        &base,
        &local,
        &remote,
        CandidateBase {
            workspace_id: WorkspaceId::new("ws_code"),
            version: 3,
            snapshot_id: SnapshotId::new("remote"),
        },
        KEY,
        "2026-06-27T12:00:00Z",
    )
    .expect_err("missing local bytes should error");

    assert!(matches!(error, MergeError::MissingSideBytes { path } if path == "app/src/main.rs"));
}

#[test]
fn missing_remote_side_bytes_error_instead_of_merging_empty_content() {
    let base = snapshot("base", "app/src/main.rs", b"base\n");
    let local = candidate(&base, "local", "app/src/main.rs", b"local\n");
    let mut remote = snapshot("remote", "app/src/main.rs", b"remote\n");
    remove_bytes_for_path(&mut remote, "app/src/main.rs");

    let error = merge_snapshots(
        &base,
        &local,
        &remote,
        CandidateBase {
            workspace_id: WorkspaceId::new("ws_code"),
            version: 3,
            snapshot_id: SnapshotId::new("remote"),
        },
        KEY,
        "2026-06-27T12:00:00Z",
    )
    .expect_err("missing remote bytes should error");

    assert!(matches!(error, MergeError::MissingSideBytes { path } if path == "app/src/main.rs"));
}

#[test]
fn matching_side_fast_path_errors_when_neither_side_has_bytes() {
    let base = empty_snapshot("base");
    let mut local = candidate(&base, "local", "app/src/main.rs", b"same\n");
    let mut remote = snapshot("remote", "app/src/main.rs", b"same\n");
    remove_bytes_for_path(&mut local.snapshot, "app/src/main.rs");
    remove_bytes_for_path(&mut remote, "app/src/main.rs");

    let error = merge_snapshots(
        &base,
        &local,
        &remote,
        CandidateBase {
            workspace_id: WorkspaceId::new("ws_code"),
            version: 3,
            snapshot_id: SnapshotId::new("remote"),
        },
        KEY,
        "2026-06-27T12:00:00Z",
    )
    .expect_err("matching side fast path should require bytes");

    assert!(matches!(error, MergeError::MissingSideBytes { path } if path == "app/src/main.rs"));
}

#[test]
fn matching_side_fast_path_uses_other_matching_side_when_it_has_bytes() {
    let base = empty_snapshot("base");
    let mut local = candidate(&base, "local", "app/src/main.rs", b"same\n");
    let remote = snapshot("remote", "app/src/main.rs", b"same\n");
    remove_bytes_for_path(&mut local.snapshot, "app/src/main.rs");

    let merged = merge_snapshots(
        &base,
        &local,
        &remote,
        CandidateBase {
            workspace_id: WorkspaceId::new("ws_code"),
            version: 3,
            snapshot_id: SnapshotId::new("remote"),
        },
        KEY,
        "2026-06-27T12:00:00Z",
    )
    .expect("matching remote bytes should satisfy clean fast path");

    let MergeOutcome::Clean(merged) = merged else {
        panic!("matching side should merge cleanly");
    };
    assert_eq!(
        merged.snapshot.file_bytes_for_path("app/src/main.rs"),
        Some(&b"same\n"[..])
    );
}

#[test]
fn executability_fast_path_errors_when_selected_side_lacks_bytes() {
    let base = snapshot("base", "app/bin/tool", b"tool\n");
    let mut local = candidate(&base, "local", "app/bin/tool", b"tool\n");
    local.snapshot.mutate_entries_for_test(|entries| {
        entries[0].executability = FileExecutability::Executable;
    });
    remove_bytes_for_path(&mut local.snapshot, "app/bin/tool");
    let remote = snapshot("remote", "app/bin/tool", b"tool\n");

    let error = merge_snapshots(
        &base,
        &local,
        &remote,
        CandidateBase {
            workspace_id: WorkspaceId::new("ws_code"),
            version: 3,
            snapshot_id: SnapshotId::new("remote"),
        },
        KEY,
        "2026-06-27T12:00:00Z",
    )
    .expect_err("executability fast path should require selected side bytes");

    assert!(matches!(error, MergeError::MissingSideBytes { path } if path == "app/bin/tool"));
}

#[test]
fn missing_base_add_add_case_still_conflicts_without_error() {
    let base = empty_snapshot("base");
    let local = candidate(&base, "local", "package-lock.json", b"{\"local\":true}\n");
    let remote = snapshot("remote", "package-lock.json", b"{\"remote\":true}\n");

    let merged = merge_snapshots(
        &base,
        &local,
        &remote,
        CandidateBase {
            workspace_id: WorkspaceId::new("ws_code"),
            version: 3,
            snapshot_id: SnapshotId::new("remote"),
        },
        KEY,
        "2026-06-27T12:00:00Z",
    )
    .expect("missing base is a valid add/add merge input");

    let MergeOutcome::Conflicted(conflicts) = merged else {
        panic!("add/add with divergent content should conflict");
    };
    assert_eq!(conflicts[0].paths, vec!["package-lock.json".to_string()]);
    assert_eq!(conflicts[0].conflict_kind, ConflictKind::StructuredText);
}

#[test]
fn conflict_span_excludes_shifted_identical_trailing_lines() {
    let span = conflict_span(
        "test.txt",
        b"line one\nline two\nline three\n",
        b"line one\nlocal insert a\nlocal insert b\nline two\nline three\n",
        b"line one\nremote change\nline three\n",
    );

    assert_eq!(span.base_start_line, 2);
    assert_eq!(span.base_end_line, 2);
    assert_eq!(span.local_start_line, 2);
    assert_eq!(span.local_end_line, 4);
    assert_eq!(span.remote_start_line, 2);
    assert_eq!(span.remote_end_line, 2);
}

fn candidate(
    base: &SnapshotContent,
    snapshot_id: &str,
    path: &str,
    bytes: &[u8],
) -> SnapshotCandidate {
    let snapshot = snapshot(snapshot_id, path, bytes);
    let manifest_identity = crate::sync::rebuild_manifest_identity(
        &snapshot.manifest().workspace_id,
        &snapshot.entries_for_test(),
        "2026-06-27T12:00:00Z",
    );
    SnapshotCandidate {
        base: CandidateBase {
            workspace_id: WorkspaceId::new("ws_code"),
            version: 1,
            snapshot_id: base.manifest.snapshot_id.clone(),
        },
        device_id: DeviceId::new("device_local"),
        manifest_id: ManifestId::new(format!("manifest_{snapshot_id}")),
        snapshot,
        scan_report: crate::scanner::ScanReport {
            root: std::path::PathBuf::new(),
            projects: Vec::new(),
            paths: Vec::new(),
            summary: bowline_core::status::ObservedWorkspaceSummary::default(),
        },
        scan_scope: ScanScope::Full(FullScanReason::CliRequested),
        stat_cache_hit_paths: BTreeSet::new(),
        stat_cache_divergences: Vec::new(),
        scan_stats: Default::default(),
        manifest_identity,
        stat_cache_write_back: None,
        causation_ids: Vec::new(),
        skipped_unsafe_symlinks: BTreeSet::new(),
        created_at: "2026-06-27T12:00:00Z".to_string(),
    }
}

fn snapshot(_snapshot_id: &str, path: &str, bytes: &[u8]) -> SnapshotContent {
    let content_id = workspace_content_id(KEY, bytes);
    let mut files = BTreeMap::new();
    files.insert(content_id.clone(), bytes.to_vec());
    let workspace_id = WorkspaceId::new("ws_code");
    let entries = vec![entry(path, content_id, bytes.len())];
    let canonical_snapshot_id =
        crate::sync::rebuild_manifest_identity(&workspace_id, &entries, "test").snapshot_id;
    SnapshotContent::new(
        SnapshotDraft {
            schema_version: 1,
            snapshot_id: canonical_snapshot_id.clone(),
            workspace_id,
            project_id: None,
            kind: SnapshotKind::WorkspaceHead,
            base_snapshot_id: None,
            entries,
            refs: vec![WorkspaceRef {
                name: "workspace".to_string(),
                target_snapshot_id: canonical_snapshot_id,
                kind: RefKind::Workspace,
            }],
        },
        files,
        KEY,
    )
    .expect("page-backed merge snapshot")
}

fn snapshot_with_files(files: Vec<(String, Vec<u8>)>) -> SnapshotContent {
    let workspace_id = WorkspaceId::new("ws_code");
    let mut prepared = BTreeMap::new();
    let entries = files
        .into_iter()
        .map(|(path, bytes)| {
            let content_id = workspace_content_id(KEY, &bytes);
            prepared.insert(content_id.clone(), bytes.clone());
            entry(&path, content_id, bytes.len())
        })
        .collect::<Vec<_>>();
    let snapshot_id =
        crate::sync::rebuild_manifest_identity(&workspace_id, &entries, "test").snapshot_id;
    SnapshotContent::new(
        SnapshotDraft {
            schema_version: 1,
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
        },
        prepared,
        KEY,
    )
    .expect("page-backed multi-file merge snapshot")
}

fn empty_snapshot(_snapshot_id: &str) -> SnapshotContent {
    let workspace_id = WorkspaceId::new("ws_code");
    let entries = Vec::new();
    let snapshot_id =
        crate::sync::rebuild_manifest_identity(&workspace_id, &entries, "test").snapshot_id;
    SnapshotContent::new(
        SnapshotDraft {
            schema_version: 1,
            snapshot_id: snapshot_id.clone(),
            workspace_id,
            project_id: None,
            kind: SnapshotKind::WorkspaceHead,
            base_snapshot_id: None,
            entries,
            refs: vec![WorkspaceRef {
                name: "workspace".to_string(),
                target_snapshot_id: SnapshotId::new(snapshot_id),
                kind: RefKind::Workspace,
            }],
        },
        BTreeMap::new(),
        KEY,
    )
    .expect("page-backed empty merge snapshot")
}

fn remove_bytes_for_path(snapshot: &mut SnapshotContent, path: &str) {
    let content_id = snapshot
        .entry_for_path(path)
        .expect("page read")
        .expect("entry")
        .content_id
        .expect("file content id");
    snapshot.prepared_content_mut().remove(&content_id);
}

fn entry(path: &str, content_id: ContentId, len: usize) -> NamespaceEntry {
    NamespaceEntry {
        path: path.to_string(),
        kind: NamespaceEntryKind::File,
        classification: PathClassification::WorkspaceSync,
        mode: MaterializationMode::EncryptedSync,
        access: vec![AccessFlag::HumanReadable, AccessFlag::AgentHidden],
        content_id: Some(content_id),
        content_layout: None,
        symlink_target: None,
        byte_len: Some(len as u64),
        executability: bowline_core::workspace_graph::FileExecutability::Regular,
        hydration_state: HydrationState::Local,
    }
}

fn merge_snapshot_id_for_entries(
    workspace_id: &WorkspaceId,
    entries: &[NamespaceEntry],
) -> SnapshotId {
    // Pins the production chunked identity path (plan 090); the timestamp only
    // stamps cache rows and does not feed the identity.
    crate::sync::rebuild_manifest_identity(workspace_id, entries, "2026-07-03T12:00:00Z")
        .snapshot_id
}

fn clean_merge_snapshot_id(
    base: &SnapshotContent,
    local: &SnapshotCandidate,
    remote: &SnapshotContent,
) -> SnapshotId {
    let merged = merge_snapshots(
        base,
        local,
        remote,
        CandidateBase {
            workspace_id: WorkspaceId::new("ws_code"),
            version: 3,
            snapshot_id: remote.manifest.snapshot_id.clone(),
        },
        KEY,
        "2026-07-03T12:00:00Z",
    )
    .expect("merge succeeds");
    let MergeOutcome::Clean(merged) = merged else {
        panic!("merge should be clean");
    };
    merged.snapshot.manifest.snapshot_id.clone()
}

fn clean_merged_snapshot(
    base: &SnapshotContent,
    local: &SnapshotCandidate,
    remote: &SnapshotContent,
) -> Box<SnapshotCandidate> {
    let merged = merge_snapshots(
        base,
        local,
        remote,
        CandidateBase {
            workspace_id: WorkspaceId::new("ws_code"),
            version: 3,
            snapshot_id: remote.manifest.snapshot_id.clone(),
        },
        KEY,
        "2026-07-03T12:00:00Z",
    )
    .expect("merge succeeds");
    let MergeOutcome::Clean(merged) = merged else {
        panic!("merge should be clean");
    };
    merged
}
