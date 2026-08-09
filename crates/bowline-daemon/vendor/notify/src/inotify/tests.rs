use std::{
    sync::{atomic::AtomicBool, mpsc, Arc},
    thread,
    thread::available_parallelism,
    time::{Duration, Instant},
};

use super::*;

#[test]
fn inotify_watcher_is_send_and_sync() {
    fn check<T: Send + Sync>() {}
    check::<INotifyWatcher>();
}

#[test]
fn native_error_type_on_missing_path() {
    let mut watcher = INotifyWatcher::new(|_| {}, Config::default()).unwrap();

    let result = watcher.watch(
        &PathBuf::from("/some/non/existant/path"),
        RecursiveMode::NonRecursive,
    );

    assert!(matches!(
        result,
        Err(Error {
            paths: _,
            kind: ErrorKind::PathNotFound
        })
    ))
}

/// Runs manually.
///
/// * Save actual value of the limit: `MAX_USER_WATCHES=$(sysctl -n fs.inotify.max_user_watches)`
/// * Run the test.
/// * Set the limit to 0: `sudo sysctl fs.inotify.max_user_watches=0` while test is running
/// * Wait for the test to complete
/// * Restore the limit `sudo sysctl fs.inotify.max_user_watches=$MAX_USER_WATCHES`
#[test]
#[ignore = "requires changing sysctl fs.inotify.max_user_watches while test is running"]
fn recursive_watch_calls_handler_if_creating_a_file_raises_max_files_watch() {
    use std::time::Duration;

    let tmpdir = tempfile::tempdir().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let (proc_changed_tx, proc_changed_rx) = std::sync::mpsc::channel();
    let proc_path = Path::new("/proc/sys/fs/inotify/max_user_watches");
    let mut watcher = INotifyWatcher::new(
        move |result: Result<Event>| match result {
            Ok(event) => {
                if event.paths.first().is_some_and(|path| path == proc_path) {
                    proc_changed_tx.send(()).unwrap();
                }
            }
            Err(e) => tx.send(e).unwrap(),
        },
        Config::default(),
    )
    .unwrap();

    watcher
        .watch(tmpdir.path(), RecursiveMode::Recursive)
        .unwrap();
    watcher
        .watch(proc_path, RecursiveMode::NonRecursive)
        .unwrap();

    // give the time to set the limit
    proc_changed_rx
        .recv_timeout(Duration::from_secs(30))
        .unwrap();

    let child_dir = tmpdir.path().join("child");
    std::fs::create_dir(child_dir).unwrap();

    let result = rx.recv_timeout(Duration::from_millis(500));

    assert!(
        matches!(
            &result,
            Ok(Error {
                kind: ErrorKind::MaxFilesWatch,
                paths: _,
            })
        ),
        "expected {:?}, found: {:#?}",
        ErrorKind::MaxFilesWatch,
        result
    );
}

/// https://github.com/notify-rs/notify/issues/678
#[test]
fn race_condition_on_unwatch_and_pending_events_with_deleted_descriptor() {
    let tmpdir = tempfile::tempdir().expect("tmpdir");
    let (tx, rx) = mpsc::channel();
    let mut inotify = INotifyWatcher::new(
        move |e: Result<Event>| {
            let e = match e {
                Ok(e) if e.paths.is_empty() => e,
                Ok(_) | Err(_) => return,
            };
            let _ = tx.send(e);
        },
        Config::default(),
    )
    .expect("inotify creation");

    let dir_path = tmpdir.path();
    let file_path = dir_path.join("foo");
    std::fs::File::create(&file_path).unwrap();

    let stop = Arc::new(AtomicBool::new(false));

    let handles: Vec<_> = (0..available_parallelism().unwrap().get().max(4))
        .map(|_| {
            let file_path = file_path.clone();
            let stop = stop.clone();
            thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let _ = std::fs::File::open(&file_path).unwrap();
                }
            })
        })
        .collect();

    let non_recursive = RecursiveMode::NonRecursive;
    for _ in 0..(handles.len() * 4) {
        inotify.watch(dir_path, non_recursive).unwrap();
        inotify.unwatch(dir_path).unwrap();
    }

    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    handles
        .into_iter()
        .for_each(|handle| handle.join().ok().unwrap_or_default());

    drop(inotify);

    let events: Vec<_> = rx.into_iter().map(|e| format!("{e:?}")).collect();

    const LOG_LEN: usize = 10;
    let events_len = events.len();
    assert!(
        events.is_empty(),
        "expected no events without path, but got {events_len}. first 10: {:#?}",
        &events[..LOG_LEN.min(events_len)]
    );
}

#[test]
fn shutdown_interrupts_a_coverage_drain_under_continuous_activity() {
    let root = tempfile::tempdir().expect("temporary watched root");
    let (control_tx, control_rx) = mpsc::channel();
    let mut watcher = INotifyWatcher::new_with_coverage(
        |_| thread::sleep(Duration::from_millis(1)),
        move |signal| {
            let _ = control_tx.send(signal);
        },
        Config::default(),
    )
    .expect("inotify watcher");
    let ready = INotifyCoverageToken::new(1).expect("nonzero ready token");
    watcher
        .request_watch_ready(root.path(), RecursiveMode::Recursive, ready)
        .expect("request initial ready marker");
    assert!(matches!(
        control_rx.recv_timeout(Duration::from_secs(5)),
        Ok(INotifyCoverageSignal::Ready(token)) if token == ready
    ));

    let producing = Arc::new(AtomicBool::new(true));
    let producers = (0..4)
        .map(|worker| {
            let producing = Arc::clone(&producing);
            let root = root.path().to_path_buf();
            thread::spawn(move || {
                let mut sequence = 0_u64;
                while producing.load(std::sync::atomic::Ordering::Acquire) {
                    let path = root.join(format!("worker-{worker}-{}.txt", sequence % 64));
                    let _ = std::fs::write(path, sequence.to_le_bytes());
                    sequence = sequence.wrapping_add(1);
                }
            })
        })
        .collect::<Vec<_>>();
    let boundary = INotifyCoverageToken::new(2).expect("nonzero boundary token");
    watcher
        .request_coverage_boundary(root.path(), boundary)
        .expect("request callback-drain boundary");
    thread::sleep(Duration::from_millis(30));

    let shutdown_started = Instant::now();
    watcher
        .shutdown()
        .expect("shutdown interrupts continuous callback draining");
    assert!(shutdown_started.elapsed() < Duration::from_secs(3));
    producing.store(false, std::sync::atomic::Ordering::Release);
    for producer in producers {
        producer.join().expect("producer joins");
    }
}
