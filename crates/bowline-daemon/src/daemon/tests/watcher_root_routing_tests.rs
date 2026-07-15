//! Watcher-boundary integration for U8 root routing.
//!
//! These drive the real `drain_changes` -> `record_watcher_event` path against a
//! temp workspace with the same `notify::Event` shape the live watcher emits, so
//! the tri-state entry-kind derivation (regular file / symlink / directory /
//! vanished) and the rename-source=`Unknown` rule are exercised end-to-end, not
//! just via a direct `insert_path_parent` unit call.

use super::*;
use crate::daemon::sync::RootEntryKind;
use bowline_local::sync::{FullScanReason, ScanScope};
use std::collections::BTreeSet;

// Drive one watcher event through the real drain path and return the scan scope
// the accumulator resolves to.
fn scope_after_event(
    label: &str,
    workspace_id: &str,
    build_event: impl FnOnce(&Path) -> Event,
) -> (ScanScope, PathBuf) {
    let fixture = watcher_fixture(label, workspace_id);
    let root = fixture.root.clone();
    let event = build_event(&root);
    let (signal_tx, signal_rx) = mpsc::channel();
    signal_tx
        .send(WatcherSignal::Changed(event))
        .expect("watcher signal sends");
    drop(signal_tx);
    let mut runtime = watcher_test_runtime(root, fixture.state_root, fixture.workspace_id.as_str());
    runtime.change_rx = Some(signal_rx);

    let drained = runtime.drain_changes();
    assert!(drained.changed, "event should record a change");
    let scope = runtime
        .pending_dirty
        .take_scope(FullScanReason::CliRequested);
    (scope, fixture.temp)
}

#[test]
fn boundary_root_regular_file_edit_yields_root_shallow() {
    let (scope, temp) = scope_after_event(
        "bowline-daemon-watch-root-file",
        "ws_watch_root_file",
        |root| {
            let path = root.join("README.md");
            fs::write(&path, "# hi\n").expect("root file");
            Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Content))).add_path(path)
        },
    );
    assert_eq!(scope, ScanScope::RootShallow);
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn boundary_root_symlink_edit_yields_root_shallow() {
    let (scope, temp) = scope_after_event(
        "bowline-daemon-watch-root-symlink",
        "ws_watch_root_symlink",
        |root| {
            let target = root.join("target.txt");
            fs::write(&target, "data\n").expect("symlink target");
            let link = root.join("current");
            std::os::unix::fs::symlink(&target, &link).expect("symlink");
            Event::new(EventKind::Create(CreateKind::Any)).add_path(link)
        },
    );
    // A root symlink is file-like, not a subtree.
    assert_eq!(scope, ScanScope::RootShallow);
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn boundary_root_directory_move_in_yields_scoped_subtree() {
    let (scope, temp) = scope_after_event(
        "bowline-daemon-watch-root-dir-in",
        "ws_watch_root_dir_in",
        |root| {
            let dir = root.join("newrepo");
            fs::create_dir_all(dir.join("src")).expect("moved-in dir");
            Event::new(EventKind::Create(CreateKind::Folder)).add_path(dir)
        },
    );
    assert_eq!(
        scope,
        ScanScope::DirtySubtrees {
            roots: BTreeSet::from(["newrepo".to_string()]),
            root_shallow: false,
        }
    );
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn boundary_root_directory_delete_yields_scoped_subtree() {
    // The directory never exists on disk at derivation time (deletion), so
    // `symlink_metadata` returns None -> Unknown -> scoped, not a shallow pass
    // that would mask the vanished subtree.
    let (scope, temp) = scope_after_event(
        "bowline-daemon-watch-root-dir-del",
        "ws_watch_root_dir_del",
        |root| {
            let dir = root.join("oldrepo");
            Event::new(EventKind::Remove(RemoveKind::Folder)).add_path(dir)
        },
    );
    assert_eq!(
        scope,
        ScanScope::DirtySubtrees {
            roots: BTreeSet::from(["oldrepo".to_string()]),
            root_shallow: false,
        }
    );
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn boundary_root_directory_rename_away_source_arrives_unknown() {
    // A rename whose destination leaves the workspace: only the source push
    // survives, and it must arrive as Unknown (never the destination's kind),
    // routing the vanished subtree to a scoped scan.
    let (scope, temp) = scope_after_event(
        "bowline-daemon-watch-root-dir-rename",
        "ws_watch_root_dir_rename",
        |root| {
            let source = root.join("oldrepo");
            let destination = root.parent().expect("root parent").join("moved-oldrepo");
            Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
                .add_path(source)
                .add_path(destination)
        },
    );
    assert_eq!(
        scope,
        ScanScope::DirtySubtrees {
            roots: BTreeSet::from(["oldrepo".to_string()]),
            root_shallow: false,
        }
    );
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn boundary_root_policy_edit_forces_full_scan() {
    let (scope, temp) = scope_after_event(
        "bowline-daemon-watch-policy-edit",
        "ws_watch_policy_edit",
        |root| {
            let path = root.join(".bowlineignore");
            fs::write(&path, "build/\n").expect("policy file");
            Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Content))).add_path(path)
        },
    );
    assert_eq!(scope, ScanScope::Full(FullScanReason::PolicyChanged));
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn boundary_root_policy_delete_forces_full_scan() {
    // Deleted policy marker: Unknown kind, but the guard runs before the kind
    // split, so deep classification is still invalidated.
    let (scope, temp) = scope_after_event(
        "bowline-daemon-watch-policy-del",
        "ws_watch_policy_del",
        |root| {
            let path = root.join(".bowlineignore");
            Event::new(EventKind::Remove(RemoveKind::File)).add_path(path)
        },
    );
    assert_eq!(scope, ScanScope::Full(FullScanReason::PolicyChanged));
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn boundary_root_policy_rename_away_source_forces_full_scan() {
    // The policy marker is renamed out of the workspace: the surviving source
    // push arrives as Unknown and still forces a full scan.
    let (scope, temp) = scope_after_event(
        "bowline-daemon-watch-policy-rename",
        "ws_watch_policy_rename",
        |root| {
            let source = root.join(".bowlineignore");
            let destination = root
                .parent()
                .expect("root parent")
                .join("moved.bowlineignore");
            Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
                .add_path(source)
                .add_path(destination)
        },
    );
    assert_eq!(scope, ScanScope::Full(FullScanReason::PolicyChanged));
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn boundary_root_file_and_deep_subtree_yield_combined_scope() {
    let fixture = watcher_fixture("bowline-daemon-watch-combined", "ws_watch_combined");
    let root = fixture.root.clone();
    let readme = root.join("README.md");
    fs::write(&readme, "# hi\n").expect("root file");
    fs::create_dir_all(root.join("src")).expect("src dir");
    let deep = root.join("src/app.rs");
    fs::write(&deep, "fn main() {}\n").expect("deep file");

    let (signal_tx, signal_rx) = mpsc::channel();
    signal_tx
        .send(WatcherSignal::Changed(
            Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Content))).add_path(readme),
        ))
        .expect("root signal");
    signal_tx
        .send(WatcherSignal::Changed(
            Event::new(EventKind::Create(CreateKind::File)).add_path(deep),
        ))
        .expect("deep signal");
    drop(signal_tx);
    let mut runtime = watcher_test_runtime(root, fixture.state_root, fixture.workspace_id.as_str());
    runtime.change_rx = Some(signal_rx);

    assert!(runtime.drain_changes().changed);
    assert_eq!(
        runtime
            .pending_dirty
            .take_scope(FullScanReason::CliRequested),
        ScanScope::DirtySubtrees {
            roots: BTreeSet::from(["src".to_string()]),
            root_shallow: true,
        }
    );
    let _ = fs::remove_dir_all(fixture.temp);
}

// Guards against the entry-kind derivation drifting away from the tri-state
// contract the routing depends on. The daemon computes the same mapping from
// `symlink_metadata` in `record_watcher_event`.
#[test]
fn entry_kind_derivation_matches_filesystem_shape() {
    let dir = unique_temp_dir("bowline-daemon-entry-kind");
    let file = dir.join("file.txt");
    fs::write(&file, "x").expect("file");
    let subdir = dir.join("subdir");
    fs::create_dir_all(&subdir).expect("subdir");
    let link = dir.join("link");
    std::os::unix::fs::symlink(&file, &link).expect("symlink");
    let missing = dir.join("gone");

    let kind_of = |path: &Path| match std::fs::symlink_metadata(path).ok() {
        Some(metadata) if metadata.is_dir() => RootEntryKind::Directory,
        Some(_) => RootEntryKind::NonDirectory,
        None => RootEntryKind::Unknown,
    };

    assert_eq!(kind_of(&file), RootEntryKind::NonDirectory);
    assert_eq!(kind_of(&subdir), RootEntryKind::Directory);
    assert_eq!(kind_of(&link), RootEntryKind::NonDirectory);
    assert_eq!(kind_of(&missing), RootEntryKind::Unknown);
    let _ = fs::remove_dir_all(dir);
}
