use super::{
    WatcherSignal, rename_source_dirty_path, watcher_destination, watcher_operation,
    watcher_recursive_root,
};
use bowline_core::git_paths::is_git_derivable_volatile_path;
use notify::{
    Event,
    event::{AccessKind, AccessMode, EventKind, Flag},
};
use std::path::Path;
use std::sync::{Arc, mpsc};

fn overflow_lane() -> Arc<super::WatcherOverflowLane> {
    Arc::new(super::WatcherOverflowLane::default())
}

#[test]
fn rename_signal_forwards_source_and_destination_paths() {
    use bowline_local::sync::manifest_engine::{EngineEvent, WorkspacePath};
    let temp = std::env::temp_dir().join(format!(
        "bowline-watcher-normalize-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    let root = temp.join("Code");
    std::fs::create_dir_all(root.join("src")).expect("workspace root");
    let source = root.join("src/old.rs");
    let destination = root.join("src/new.rs");
    std::fs::write(&destination, "fn renamed() {}\n").expect("destination");
    let event = Event::new(EventKind::Modify(notify::event::ModifyKind::Name(
        notify::event::RenameMode::Both,
    )))
    .add_path(source)
    .add_path(destination);
    let signal = super::WatcherSignal::Changed { event };
    let engine_event =
        super::watcher_signal_engine_event(&root, &signal, &mut std::collections::HashMap::new())
            .expect("rename yields an engine event");
    let EngineEvent::Paths(paths) = engine_event else {
        panic!("expected Paths event");
    };
    assert!(paths.contains(&WorkspacePath::new("src/old.rs")));
    assert!(paths.contains(&WorkspacePath::new("src/new.rs")));
    let _ = std::fs::remove_dir_all(temp);
}

#[test]
fn macos_directory_rename_any_signals_reconcile_both_roots() {
    use bowline_local::sync::manifest_engine::{EngineEvent, WorkspacePath};
    let temp = std::env::temp_dir().join(format!(
        "bowline-watcher-macos-directory-rename-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    let root = temp.join("Code");
    let source = root.join("src");
    let destination = root.join("source");
    std::fs::create_dir_all(source.join("nested")).expect("source tree");
    std::fs::write(source.join("nested/main.rs"), "fn main() {}\n").expect("source file");
    std::fs::rename(&source, &destination).expect("directory rename");

    for (path, expected) in [(&source, "src"), (&destination, "source")] {
        let event = Event::new(EventKind::Modify(notify::event::ModifyKind::Name(
            notify::event::RenameMode::Any,
        )))
        .add_path(path.clone());
        let signal = super::WatcherSignal::Changed { event };
        let engine_event = super::watcher_signal_engine_event(
            &root,
            &signal,
            &mut std::collections::HashMap::new(),
        )
        .expect("rename half yields an engine event");

        let EngineEvent::RecursivePaths(roots) = engine_event else {
            panic!("expected RecursivePaths event for {expected}");
        };
        assert_eq!(roots, [WorkspacePath::new(expected)].into_iter().collect());
    }
    let _ = std::fs::remove_dir_all(temp);
}

#[test]
fn created_directory_signal_requests_recursive_manifest_discovery() {
    use bowline_local::sync::manifest_engine::{EngineEvent, WorkspacePath};
    let temp = std::env::temp_dir().join(format!(
        "bowline-watcher-recursive-create-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    let root = temp.join("Code");
    let project = root.join("repo");
    std::fs::create_dir_all(project.join(".git/objects/ab")).expect("git tree");
    std::fs::write(project.join(".git/objects/ab/cdef"), b"opaque").expect("git object");
    let event = Event::new(EventKind::Create(notify::event::CreateKind::Folder)).add_path(project);
    let signal = super::WatcherSignal::Changed { event };

    let engine_event =
        super::watcher_signal_engine_event(&root, &signal, &mut std::collections::HashMap::new())
            .expect("directory creation yields an engine event");

    let EngineEvent::RecursivePaths(roots) = engine_event else {
        panic!("expected RecursivePaths event");
    };
    assert_eq!(roots, [WorkspacePath::new("repo")].into_iter().collect());
    let _ = std::fs::remove_dir_all(temp);
}

#[test]
fn excluded_directory_with_included_descendants_requests_recursive_discovery() {
    use bowline_local::sync::manifest_engine::{EngineEvent, WorkspacePath};
    let temp = std::env::temp_dir().join(format!(
        "bowline-watcher-recursive-include-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    let root = temp.join("Code");
    let vendor = root.join("vendor");
    std::fs::create_dir_all(vendor.join("kept")).expect("included tree");
    std::fs::write(root.join(".bowlineignore"), b"vendor/**\n!vendor/kept/**\n").expect("policy");
    std::fs::write(vendor.join("kept/source.rs"), b"pub fn kept() {}\n").expect("included child");
    let event = Event::new(EventKind::Create(notify::event::CreateKind::Folder)).add_path(vendor);
    let signal = super::WatcherSignal::Changed { event };

    let engine_event =
        super::watcher_signal_engine_event(&root, &signal, &mut std::collections::HashMap::new())
            .expect("included descendants keep the traversal root");

    let EngineEvent::RecursivePaths(roots) = engine_event else {
        panic!("expected RecursivePaths event");
    };
    assert_eq!(roots, [WorkspacePath::new("vendor")].into_iter().collect());
    let _ = std::fs::remove_dir_all(temp);
}

#[test]
fn watcher_git_churn_predicate_skips_derivable_state_only() {
    assert!(!is_git_derivable_volatile_path("repo/.git/index"));
    assert!(is_git_derivable_volatile_path("repo/.git/logs"));
    assert!(!is_git_derivable_volatile_path("repo/.git/HEAD"));
}

#[test]
fn read_access_events_do_not_wake_sync_or_saturate_the_backlog() {
    let root = Path::new("/ws");
    for kind in [
        AccessKind::Open(AccessMode::Read),
        AccessKind::Read,
        AccessKind::Close(AccessMode::Read),
    ] {
        let event = Event::new(EventKind::Access(kind)).add_path(root.join(".env"));
        assert_eq!(watcher_operation(&event.kind), None);
    }
    assert_eq!(
        watcher_operation(&EventKind::Access(AccessKind::Close(AccessMode::Write))),
        Some(super::WatcherOperation::Modify)
    );
}

#[test]
fn read_access_events_consume_no_watcher_channel_capacity() {
    let (sender, receiver) = mpsc::sync_channel(1);
    let overflow_lane = overflow_lane();
    for _ in 0..10 {
        super::send_watcher_signal(
            &sender,
            &overflow_lane,
            Ok(
                Event::new(EventKind::Access(AccessKind::Close(AccessMode::Read)))
                    .add_path("/ws/.env".into()),
            ),
        );
    }
    assert!(matches!(
        receiver.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));

    super::send_watcher_signal(
        &sender,
        &overflow_lane,
        Ok(
            Event::new(EventKind::Access(AccessKind::Close(AccessMode::Write)))
                .add_path("/ws/.env".into()),
        ),
    );
    assert!(matches!(
        receiver.try_recv(),
        Ok(WatcherSignal::Changed { .. })
    ));
}

#[test]
fn rescan_flag_takes_precedence_over_read_filtering() {
    let (sender, receiver) = mpsc::sync_channel(1);
    let overflow_lane = overflow_lane();
    super::send_watcher_signal(
        &sender,
        &overflow_lane,
        Ok(
            Event::new(EventKind::Access(AccessKind::Close(AccessMode::Read)))
                .add_path("/ws/.env".into())
                .set_flag(Flag::Rescan),
        ),
    );

    assert!(matches!(
        receiver.try_recv(),
        Ok(WatcherSignal::Changed { event, .. }) if event.need_rescan()
    ));
}

#[test]
fn saturated_watcher_callback_requests_recovery_without_blocking() {
    let (sender, receiver) = mpsc::sync_channel(1);
    sender
        .send(WatcherSignal::Recoverable)
        .expect("fill watcher channel");
    let overflow_lane = overflow_lane();
    let callback_lane = Arc::clone(&overflow_lane);
    let (done_tx, done_rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        super::send_watcher_signal(
            &sender,
            &callback_lane,
            Ok(
                Event::new(EventKind::Modify(notify::event::ModifyKind::Any))
                    .add_path("/ws/follow-up.txt".into()),
            ),
        );
        done_tx.send(()).expect("completion");
    });

    let completed_without_capacity = done_rx.recv_timeout(std::time::Duration::from_secs(1));
    if completed_without_capacity.is_err() {
        let _ = receiver.recv();
    }
    worker.join().expect("callback worker");

    assert!(
        completed_without_capacity.is_ok(),
        "a saturated native callback must return without channel capacity"
    );
    assert!(overflow_lane.recovery_requested());
}

#[test]
fn asserted_overflow_lane_coalesces_follow_on_callback_events() {
    let (sender, receiver) = mpsc::sync_channel(1);
    let overflow_lane = overflow_lane();
    overflow_lane.request_recovery();
    super::send_watcher_signal(
        &sender,
        &overflow_lane,
        Ok(
            Event::new(EventKind::Modify(notify::event::ModifyKind::Any))
                .add_path("/ws/follow-up.txt".into()),
        ),
    );

    assert!(
        matches!(receiver.try_recv(), Err(mpsc::TryRecvError::Empty)),
        "a latched recovery scan covers ordinary follow-on callback events"
    );
    assert!(overflow_lane.recovery_requested());
}

#[test]
fn rename_source_is_dirtied_even_when_destination_is_filtered() {
    // A tracked file moved anywhere must mark its source dirty so a scoped
    // reconcile drops the stale entry, regardless of the destination.
    assert_eq!(
        rename_source_dirty_path(Some("src/app.rs")),
        Some("src/app.rs")
    );
    assert_eq!(rename_source_dirty_path(None), None);
    // Sources that were never synced need no rescan.
    assert_eq!(rename_source_dirty_path(Some("")), None);
    assert_eq!(rename_source_dirty_path(Some(".bowline/state.json")), None);
    assert_eq!(
        rename_source_dirty_path(Some(".work/app/feature/src/auth.rs")),
        None
    );
    assert_eq!(
        rename_source_dirty_path(Some("repo/.work/feature/src/auth.rs")),
        None
    );
    assert_eq!(
        rename_source_dirty_path(Some("src/.bowline-materialize-app_rs-abcdef123456.tmp")),
        None
    );
    for ordinary_path in [
        ".env",
        ".git/HEAD",
        ".bowline-conflicts/conflict/local/app.env",
        "repo/.git/index",
    ] {
        assert_eq!(
            rename_source_dirty_path(Some(ordinary_path)),
            Some(ordinary_path)
        );
    }
}

#[test]
fn work_view_git_state_never_enters_watcher_reconciliation() {
    let temp = std::env::temp_dir().join(format!(
        "bowline-watcher-local-work-view-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    let root = temp.join("Code");
    let work_git = root.join(".work/app/feature/.git");
    std::fs::create_dir_all(&work_git).expect("create local work-view Git state");
    std::fs::write(work_git.join("HEAD"), "ref: refs/heads/main\n")
        .expect("write local work-view Git head");
    let mut policy_cache = std::collections::HashMap::new();

    assert!(!watcher_recursive_root(
        &root,
        ".work/app/feature/.git",
        &mut policy_cache,
    ));
    assert!(watcher_destination(&root, &work_git.join("HEAD"), &mut policy_cache).is_none());
    let _ = std::fs::remove_dir_all(temp);
}
