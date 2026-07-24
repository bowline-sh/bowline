use super::sync::{drain_policy, invalidate_policy_cache_for_path};
use super::*;
use bowline_core::git_paths::{is_git_derivable_volatile_path, is_git_directory_path};
use bowline_local::policy::{
    is_private_workspace_state_path, is_work_view_namespace_path, policy_should_recurse,
};
use notify::event::{AccessKind, AccessMode};

/// A watcher-kernel signal. The manifest engine treats any lost-fidelity signal
/// (overflow, adapter loss) as `FullScanRequired`, so signals carry no epoch or
/// generation token.
#[derive(Debug)]
pub(super) enum WatcherSignal {
    Changed { event: Event },
    Recoverable,
    Limited { reason: String },
}

/// The workspace filesystem watcher kernel: a recursive notify watch on the
/// workspace root whose callback read-filters events into [`WatcherSignal`]s.
/// Dropping it tears down the native watch.
#[derive(Debug)]
pub(in crate::daemon) struct SyncWatcher {
    _watcher: RecommendedWatcher,
}

pub(in crate::daemon) fn start_sync_watcher(
    root: &Path,
) -> Result<(SyncWatcher, Receiver<WatcherSignal>), notify::Error> {
    let (change_tx, change_rx) = mpsc::sync_channel(WATCHER_DRAIN_BUDGET);
    let callback_tx = change_tx.clone();
    let reported_root = root.to_path_buf();
    let watch_root = fs::canonicalize(root).unwrap_or_else(|_| reported_root.clone());
    let callback_watch_root = watch_root.clone();
    let mut watcher =
        notify::recommended_watcher(move |mut event: notify::Result<notify::Event>| {
            if let Ok(event) = &mut event {
                remap_watcher_event_root(event, &callback_watch_root, &reported_root);
            }
            send_watcher_signal(&callback_tx, event);
        })?;
    watcher.watch(&watch_root, RecursiveMode::Recursive)?;
    Ok((SyncWatcher { _watcher: watcher }, change_rx))
}

fn remap_watcher_event_root(event: &mut Event, watched_root: &Path, reported_root: &Path) {
    for path in &mut event.paths {
        if let Ok(relative) = path.strip_prefix(watched_root) {
            *path = reported_root.join(relative);
        }
    }
}

pub(super) fn send_watcher_signal(
    change_tx: &mpsc::SyncSender<WatcherSignal>,
    event: notify::Result<notify::Event>,
) {
    let signal = match event {
        Ok(event) if event.need_rescan() => WatcherSignal::Changed { event },
        Ok(event) if watcher_operation(&event.kind).is_none() => return,
        Ok(event) => WatcherSignal::Changed { event },
        Err(error) if watcher_error_needs_rescan(&error) => WatcherSignal::Recoverable,
        Err(error) => WatcherSignal::Limited {
            reason: error.to_string(),
        },
    };
    match change_tx.try_send(signal) {
        Ok(()) => {}
        Err(mpsc::TrySendError::Full(_)) => {
            // One blocking overflow marker bounds retained history and makes
            // the next consumer collapse the entire backlog to a full scan.
            if let Err(error) = change_tx.send(WatcherSignal::Recoverable) {
                eprintln!("bowline-daemon watcher overflow marker dropped: {error}");
            }
        }
        Err(mpsc::TrySendError::Disconnected(_)) => {
            eprintln!("bowline-daemon watcher signal receiver disconnected");
        }
    }
}

fn watcher_error_needs_rescan(error: &notify::Error) -> bool {
    match &error.kind {
        notify::ErrorKind::Generic(reason) => {
            let normalized = reason.to_ascii_lowercase();
            normalized.contains("overflow") || normalized.contains("rescan")
        }
        _ => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WatcherOperation {
    Create,
    Delete,
    Rename,
    Metadata,
    Modify,
}

fn watcher_operation(kind: &EventKind) -> Option<WatcherOperation> {
    match kind {
        EventKind::Access(
            AccessKind::Open(_) | AccessKind::Read | AccessKind::Close(AccessMode::Read),
        ) => None,
        EventKind::Create(_) => Some(WatcherOperation::Create),
        EventKind::Remove(
            RemoveKind::Any | RemoveKind::File | RemoveKind::Folder | RemoveKind::Other,
        ) => Some(WatcherOperation::Delete),
        EventKind::Modify(ModifyKind::Name(_)) => Some(WatcherOperation::Rename),
        EventKind::Modify(ModifyKind::Metadata(_)) => Some(WatcherOperation::Metadata),
        _ => Some(WatcherOperation::Modify),
    }
}

fn watcher_event_paths<'a>(
    root: &Path,
    operation: WatcherOperation,
    event: &'a Event,
) -> Vec<(usize, &'a Path, Option<String>)> {
    if operation == WatcherOperation::Rename && event.paths.len() >= 2 {
        return vec![(
            1,
            event.paths[1].as_path(),
            watcher_relative_path(root, &event.paths[0]),
        )];
    }
    event
        .paths
        .iter()
        .enumerate()
        .map(|(index, path)| (index, path.as_path(), None))
        .collect()
}

pub(super) fn watcher_relative_path(root: &Path, path: &Path) -> Option<String> {
    let relative = match path.strip_prefix(root) {
        Ok(relative) => relative,
        Err(_) if path.is_absolute() => return None,
        Err(_) => path,
    };
    let normalized = normalize_workspace_path(&relative.display().to_string());
    if normalized.starts_with("..") {
        return None;
    }
    Some(normalized)
}

/// Translate one watcher signal into an engine event (Plan 111 Step 1b),
/// preserving the watcher kernel's read/private/git filtering. A recordable
/// change yields `Paths`; a lost-fidelity signal (overflow, adapter loss) yields
/// `FullScanRequired`, which the engine recovers with one cheap stat walk.
pub(in crate::daemon) fn watcher_signal_engine_event(
    root: &Path,
    signal: &WatcherSignal,
    policy_cache: &mut HashMap<String, UserPolicy>,
) -> Option<bowline_local::sync::manifest_engine::EngineEvent> {
    use bowline_local::sync::manifest_engine::{EngineEvent, FullScanReason};
    match signal {
        WatcherSignal::Changed { event } if event.need_rescan() => Some(
            EngineEvent::FullScanRequired(FullScanReason::WatcherOverflow),
        ),
        WatcherSignal::Changed { event } => {
            let recursive_roots = watcher_event_recursive_roots(root, event, policy_cache);
            if !recursive_roots.is_empty() {
                return Some(EngineEvent::RecursivePaths(recursive_roots));
            }
            let paths = watcher_event_engine_paths(root, event, policy_cache);
            (!paths.is_empty()).then_some(EngineEvent::Paths(paths))
        }
        WatcherSignal::Recoverable => Some(EngineEvent::FullScanRequired(
            FullScanReason::WatcherOverflow,
        )),
        WatcherSignal::Limited { reason } => {
            eprintln!("bowline-daemon watcher adapter is unavailable: {reason}");
            Some(EngineEvent::FullScanRequired(
                FullScanReason::WatcherDisconnected,
            ))
        }
    }
}

fn watcher_event_recursive_roots(
    root: &Path,
    event: &Event,
    policy_cache: &mut HashMap<String, UserPolicy>,
) -> std::collections::BTreeSet<bowline_local::sync::manifest_engine::WorkspacePath> {
    use bowline_local::sync::manifest_engine::WorkspacePath;
    let mut roots = std::collections::BTreeSet::new();
    let Some(operation) = watcher_operation(&event.kind) else {
        return roots;
    };
    let recursive_without_metadata = operation == WatcherOperation::Delete
        || matches!(
            event.kind,
            EventKind::Modify(ModifyKind::Name(notify::event::RenameMode::From))
        );
    for (_, path, source_path) in watcher_event_paths(root, operation, event) {
        let destination_is_directory =
            fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir());
        if !recursive_without_metadata && !destination_is_directory {
            continue;
        }
        if let Some(source) = source_path
            && watcher_recursive_root(root, &source, policy_cache)
        {
            roots.insert(WorkspacePath::new(source));
        }
        if let Some(relative) = watcher_relative_path(root, path)
            && watcher_recursive_root(root, &relative, policy_cache)
        {
            roots.insert(WorkspacePath::new(relative));
        }
    }
    roots
}

fn watcher_recursive_root(
    root: &Path,
    relative_path: &str,
    policy_cache: &mut HashMap<String, UserPolicy>,
) -> bool {
    if relative_path.is_empty()
        || is_private_workspace_state_path(relative_path)
        || is_work_view_namespace_path(relative_path)
    {
        return false;
    }
    if is_git_derivable_volatile_path(relative_path) {
        return is_git_directory_path(relative_path);
    }
    invalidate_policy_cache_for_path(relative_path, policy_cache);
    let absolute = root.join(relative_path);
    let metadata = fs::symlink_metadata(&absolute).ok();
    let is_dir = metadata.as_ref().is_some_and(|metadata| metadata.is_dir());
    let byte_len = metadata
        .as_ref()
        .filter(|metadata| !metadata.is_dir())
        .map(|metadata| metadata.len());
    let policy = drain_policy(root, relative_path, policy_cache);
    let decision = classify_path(
        &PathFacts {
            relative_path: relative_path.to_string(),
            is_dir,
            byte_len,
        },
        policy,
    );
    policy_should_recurse(&decision, policy, relative_path)
}

/// The recordable workspace paths a watcher event touches, read-filtered by the
/// same policy classification the old journal path used. A rename dirties both
/// its source (so the stale entry drops) and its recordable destination.
fn watcher_event_engine_paths(
    root: &Path,
    event: &Event,
    policy_cache: &mut HashMap<String, UserPolicy>,
) -> std::collections::BTreeSet<bowline_local::sync::manifest_engine::WorkspacePath> {
    use bowline_local::sync::manifest_engine::WorkspacePath;
    let mut paths = std::collections::BTreeSet::new();
    let Some(operation) = watcher_operation(&event.kind) else {
        return paths;
    };
    for (_, path, source_path) in watcher_event_paths(root, operation, event) {
        if let Some(source) = rename_source_dirty_path(source_path.as_deref()) {
            paths.insert(WorkspacePath::new(source.to_string()));
        }
        if let Some(destination) = watcher_destination(root, path, policy_cache) {
            paths.insert(WorkspacePath::new(destination.relative_path));
        }
    }
    paths
}

pub(super) fn watcher_should_record(
    classification: PathClassification,
    mode: MaterializationMode,
) -> bool {
    matches!(
        (classification, mode),
        (PathClassification::WorkspaceSync, _)
            | (PathClassification::ProjectEnv, _)
            | (PathClassification::SecretLooking, _)
            | (PathClassification::LargeFile, MaterializationMode::Lazy)
    )
}

// A rename's source is where a tracked file used to live. It must be rescanned
// when the file leaves — independent of whether the rename *destination* is
// recordable — or a scoped reconcile never observes the removal and the stale
// head-manifest entry survives, reappearing on the user's other machines.
// Returns the source path to mark dirty, or None when the source was never a
// synced location (non-rename event, empty, private state, or git-volatile).
pub(super) fn rename_source_dirty_path(source_path: Option<&str>) -> Option<&str> {
    let source = source_path?;
    if source.is_empty()
        || is_private_workspace_state_path(source)
        || is_work_view_namespace_path(source)
        || is_git_derivable_volatile_path(source)
    {
        return None;
    }
    Some(source)
}

struct WatcherDestination {
    relative_path: String,
}

fn watcher_destination(
    root: &Path,
    path: &Path,
    policy_cache: &mut HashMap<String, UserPolicy>,
) -> Option<WatcherDestination> {
    let relative_path = watcher_relative_path(root, path)?;
    if relative_path.is_empty()
        || is_private_workspace_state_path(&relative_path)
        || is_work_view_namespace_path(&relative_path)
        || is_git_derivable_volatile_path(&relative_path)
    {
        return None;
    }
    invalidate_policy_cache_for_path(&relative_path, policy_cache);
    let metadata = fs::symlink_metadata(path).ok();
    let is_dir = metadata.as_ref().is_some_and(|metadata| metadata.is_dir());
    let byte_len = metadata
        .as_ref()
        .filter(|metadata| !metadata.is_dir())
        .map(|metadata| metadata.len());
    let policy = drain_policy(root, &relative_path, policy_cache);
    let decision = classify_path(
        &PathFacts {
            relative_path: relative_path.clone(),
            is_dir,
            byte_len,
        },
        policy,
    );
    watcher_should_record(decision.classification, decision.mode)
        .then_some(WatcherDestination { relative_path })
}

#[cfg(test)]
mod tests {
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
    use std::sync::mpsc;

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
        let engine_event = super::watcher_signal_engine_event(
            &root,
            &signal,
            &mut std::collections::HashMap::new(),
        )
        .expect("rename yields an engine event");
        let EngineEvent::Paths(paths) = engine_event else {
            panic!("expected Paths event");
        };
        assert!(paths.contains(&WorkspacePath::new("src/old.rs")));
        assert!(paths.contains(&WorkspacePath::new("src/new.rs")));
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
        let event =
            Event::new(EventKind::Create(notify::event::CreateKind::Folder)).add_path(project);
        let signal = super::WatcherSignal::Changed { event };

        let engine_event = super::watcher_signal_engine_event(
            &root,
            &signal,
            &mut std::collections::HashMap::new(),
        )
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
        std::fs::write(root.join(".bowlineignore"), b"vendor/**\n!vendor/kept/**\n")
            .expect("policy");
        std::fs::write(vendor.join("kept/source.rs"), b"pub fn kept() {}\n")
            .expect("included child");
        let event =
            Event::new(EventKind::Create(notify::event::CreateKind::Folder)).add_path(vendor);
        let signal = super::WatcherSignal::Changed { event };

        let engine_event = super::watcher_signal_engine_event(
            &root,
            &signal,
            &mut std::collections::HashMap::new(),
        )
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
        for _ in 0..10 {
            super::send_watcher_signal(
                &sender,
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
        super::send_watcher_signal(
            &sender,
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
}
