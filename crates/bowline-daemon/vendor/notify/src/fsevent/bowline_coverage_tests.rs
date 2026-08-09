use super::*;
use std::ffi::CString;
use std::fs as std_fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

#[test]
fn ordinary_watcher_flags_remain_upstream_compatible() {
    let watcher =
        FsEventWatcher::from_event_handler(Arc::new(Mutex::new(|_event: Result<Event>| {})))
            .expect("ordinary watcher construction succeeds");
    assert_eq!(
        watcher.flags,
        fs::kFSEventStreamCreateFlagFileEvents | fs::kFSEventStreamCreateFlagNoDefer
    );
    assert_eq!(watcher.flags & fs::kFSEventStreamCreateFlagWatchRoot, 0);
}

#[test]
fn data_callbacks_are_serialized_around_history_done() {
    let observations = Arc::new(Mutex::new(Vec::new()));
    let event_observations = Arc::clone(&observations);
    let coverage_observations = Arc::clone(&observations);
    let event_handler: Arc<Mutex<dyn EventHandler>> =
        Arc::new(Mutex::new(move |_event: Result<Event>| {
            event_observations
                .lock()
                .expect("test event observations")
                .push("data".to_string());
        }));
    let coverage_handler: Arc<Mutex<dyn FsEventCoverageHandler>> =
        Arc::new(Mutex::new(move |signal| {
            let label = match signal {
                FsEventCoverageSignal::Event(event) if event.flags().history_done() => {
                    "history-done"
                }
                FsEventCoverageSignal::Event(_) => "coverage-data",
                FsEventCoverageSignal::Started => "started",
                FsEventCoverageSignal::Stopped => "stopped",
            };
            coverage_observations
                .lock()
                .expect("test coverage observations")
                .push(label.to_string());
        }));
    let mut context = StreamContextInfo {
        event_handler,
        coverage_handler: Some(coverage_handler),
        recursive_info: HashMap::from([(PathBuf::from("/tmp"), true)]),
    };
    let paths = [
        CString::new("/tmp/before").expect("test path"),
        CString::new("/tmp").expect("history path"),
        CString::new("/tmp/after").expect("test path"),
    ];
    let mut path_ptrs: Vec<_> = paths.iter().map(|path| path.as_ptr()).collect();
    let flags = [
        fs::kFSEventStreamEventFlagItemModified | fs::kFSEventStreamEventFlagItemIsFile,
        fs::kFSEventStreamEventFlagHistoryDone,
        fs::kFSEventStreamEventFlagItemModified | fs::kFSEventStreamEventFlagItemIsFile,
    ];
    let ids = [11, 12, 13];

    unsafe {
        callback_impl(
            ptr::null_mut(),
            (&raw mut context).cast(),
            path_ptrs.len(),
            path_ptrs.as_mut_ptr().cast(),
            flags.as_ptr(),
            ids.as_ptr(),
        );
    }

    assert_eq!(
        *observations.lock().expect("test observations"),
        [
            "coverage-data",
            "data",
            "history-done",
            "coverage-data",
            "data",
        ]
    );
}

#[test]
fn patched_backend_never_purges_the_shared_volume_journal() {
    let source = include_str!("../fsevent.rs");
    let forbidden = ["FSEventsPurgeEvents", "ForDeviceUpToEventId"].concat();
    assert!(!source.contains(&forbidden));
}

#[test]
fn worker_exit_signal_survives_the_lifecycle_callback_path() {
    let root = tempfile::tempdir().expect("temporary watched root");
    let mut watcher = FsEventWatcher::new_with_coverage(
        |_| {},
        |_| {},
        FsEventCursor::current_safe(),
        Config::default(),
    )
    .expect("coverage watcher");
    watcher
        .watch(root.path(), RecursiveMode::Recursive)
        .expect("watch root");
    let worker_exit = watcher
        .take_worker_exit_receiver()
        .expect("independent worker exit signal");
    watcher.shutdown().expect("worker shutdown joins");
    worker_exit
        .recv_timeout(Duration::from_secs(1))
        .expect("worker exit signal is retained independently");
}

#[test]
fn synchronous_flush_returns_after_the_data_callback() {
    let root = tempfile::tempdir().expect("temporary watched root");
    let callback_returned = Arc::new(AtomicBool::new(false));
    let callback_state = Arc::clone(&callback_returned);
    let mut watcher = FsEventWatcher::new_with_coverage(
        move |_| {
            callback_state.store(true, Ordering::Release);
        },
        |_| {},
        FsEventCursor::current_safe(),
        Config::default(),
    )
    .expect("coverage watcher");
    watcher
        .watch(root.path(), RecursiveMode::Recursive)
        .expect("watch root");
    let before = FsEventCursor::current_safe();
    std_fs::write(root.path().join("flush.txt"), b"flush me").expect("write watched file");
    let journal_deadline = std::time::Instant::now() + Duration::from_secs(2);
    while FsEventCursor::current_safe() <= before && std::time::Instant::now() < journal_deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(FsEventCursor::current_safe() > before);

    watcher.flush_sync().expect("synchronous native flush");

    assert!(callback_returned.load(Ordering::Acquire));
    watcher.shutdown().expect("worker shutdown joins");
}
