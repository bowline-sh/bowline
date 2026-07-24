//! Watcher-kernel to manifest-engine routing integration.

use super::*;
#[cfg(target_os = "linux")]
use crate::daemon::protocol::WatcherBridge;
#[cfg(target_os = "linux")]
use crate::daemon::send_watcher_signal;
use crate::daemon::start_sync_watcher;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use bowline_local::sync::manifest_engine::{EngineEvent, WorkspacePath};
#[cfg(target_os = "linux")]
use notify::{
    Event, EventKind,
    event::{AccessKind, AccessMode},
};
#[cfg(target_os = "linux")]
use std::sync::mpsc;

#[cfg(target_os = "linux")]
fn daemon_runtime_with_sync(sync: ContinuousSyncRuntime) -> DaemonRuntime {
    DaemonRuntime {
        sync: Some(sync),
        notify_approvals: false,
        notification_dedupe: Arc::new(Mutex::new(NotificationDedupe::default())),
        next_notification_poll: Instant::now(),
        pending_notification_status: None,
    }
}

/// A manifest driver whose thread records forwarded engine events instead of
/// running the real engine, so the watcher bridge's output is observable.
#[cfg(target_os = "linux")]
fn recording_driver() -> (
    bowline_daemon::manifest_driver::ManifestDriver,
    Arc<Mutex<Vec<EngineEvent>>>,
) {
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&recorded);
    let driver = bowline_daemon::manifest_driver::ManifestDriver::spawn(move |inbox, _snapshot| {
        while let Ok(event) = inbox.recv() {
            if matches!(event, EngineEvent::Shutdown) {
                break;
            }
            if let Ok(mut recorded) = sink.lock() {
                recorded.push(event);
            }
        }
    })
    .expect("recording driver spawns");
    (driver, recorded)
}

#[cfg(target_os = "linux")]
fn await_recorded_paths(recorded: &Arc<Mutex<Vec<EngineEvent>>>, path: &str) {
    let deadline = Instant::now() + Duration::from_secs(2);
    let wanted = WorkspacePath::new(path.to_string());
    loop {
        if let Ok(events) = recorded.lock()
            && events.iter().any(|event| match event {
                EngineEvent::Paths(paths) => paths.contains(&wanted),
                EngineEvent::FullScanRequired(_) => true,
                _ => false,
            })
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "watcher never forwarded an engine event for {path}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(target_os = "linux")]
#[test]
fn linux_nested_edit_reaches_engine_through_watcher_bridge() {
    let fixture = watcher_fixture("bowline-daemon-watch-nested-edit", "ws_watch_nested_edit");
    let root = fixture.root.clone();
    let project = root.join("project");
    fs::create_dir_all(project.join("src/deep")).expect("project subtree");
    fs::write(project.join("src/deep/lib.rs"), "pub fn before() {}\n").expect("seed file");
    let (watcher, receiver) = start_sync_watcher(&root).expect("watcher starts");

    let mut sync = watcher_test_runtime(
        root,
        fixture.state_root.clone(),
        fixture.workspace_id.as_str(),
    );
    sync.watcher = Some(watcher);
    sync.change_rx = Some(receiver);
    let (driver, recorded) = recording_driver();
    sync.manifest_engine = crate::daemon::sync::ManifestEngineHost::Active(driver);
    let mut runtime = daemon_runtime_with_sync(sync);
    let bridge = WatcherBridge::start(&mut runtime)
        .expect("watcher bridge starts")
        .expect("watcher receiver creates bridge");

    fs::write(project.join("src/deep/lib.rs"), "pub fn after() {}\n").expect("nested user edit");
    await_recorded_paths(&recorded, "project/src/deep/lib.rs");

    runtime
        .sync
        .as_mut()
        .expect("sync runtime remains")
        .watcher
        .take();
    bridge.join().expect("watcher bridge joins");
    drop(runtime);
    let _ = fs::remove_dir_all(fixture.temp);
}

#[cfg(target_os = "linux")]
#[test]
fn linux_close_after_write_reaches_engine() {
    let fixture = watcher_fixture("bowline-daemon-watch-close-write", "ws_watch_close_write");
    let env_path = fixture.root.join(".env");
    fs::write(&env_path, "TOKEN=changed\n").expect("env file");
    let (signal_tx, signal_rx) = mpsc::sync_channel(1);
    send_watcher_signal(
        &signal_tx,
        Ok(Event::new(EventKind::Access(AccessKind::Close(AccessMode::Write))).add_path(env_path)),
    );
    drop(signal_tx);

    let mut sync = watcher_test_runtime(
        fixture.root.clone(),
        fixture.state_root.clone(),
        fixture.workspace_id.as_str(),
    );
    sync.change_rx = Some(signal_rx);
    let (driver, recorded) = recording_driver();
    sync.manifest_engine = crate::daemon::sync::ManifestEngineHost::Active(driver);
    let mut runtime = daemon_runtime_with_sync(sync);
    let bridge = WatcherBridge::start(&mut runtime)
        .expect("watcher bridge starts")
        .expect("watcher receiver creates bridge");

    await_recorded_paths(&recorded, ".env");

    bridge.join().expect("watcher bridge joins");
    drop(runtime);
    let _ = fs::remove_dir_all(fixture.temp);
}

#[cfg(target_os = "macos")]
#[test]
fn macos_watcher_kernel_arms_recursive_root_watch() {
    let fixture = watcher_fixture("bowline-daemon-watch-macos-kernel", "ws_watch_macos_kernel");
    fs::create_dir_all(fixture.root.join("project/src")).expect("project subtree");
    let (watcher, receiver) = start_sync_watcher(&fixture.root).expect("watcher starts");

    fs::write(fixture.root.join("project/src/lib.rs"), "pub fn a() {}\n").expect("nested write");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut observed = false;
    while Instant::now() < deadline {
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(_signal) => {
                observed = true;
                break;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    assert!(observed, "recursive root watch observes nested writes");
    drop(watcher);
    let _ = fs::remove_dir_all(fixture.temp);
}

#[cfg(target_os = "macos")]
#[test]
fn macos_dense_git_init_emits_recursive_engine_roots() {
    let fixture = watcher_fixture("bowline-daemon-watch-macos-dense-git", "ws_watch_macos_git");
    let (watcher, receiver) = start_sync_watcher(&fixture.root).expect("watcher starts");

    let project = fixture.root.join("project");
    fs::create_dir_all(project.join(".git/objects/ab")).expect("object tree");
    fs::create_dir_all(project.join(".git/refs/heads")).expect("ref tree");
    fs::write(project.join(".git/objects/ab/cdef"), b"opaque object").expect("object");
    fs::write(project.join(".git/refs/heads/main"), b"abc123\n").expect("ref");
    fs::write(project.join(".git/HEAD"), b"ref: refs/heads/main\n").expect("head");

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut quiet_intervals = 0;
    let mut policy_cache = std::collections::HashMap::new();
    let mut recursive_roots = std::collections::BTreeSet::new();
    while Instant::now() < deadline && quiet_intervals < 3 {
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(signal) => {
                quiet_intervals = 0;
                if let Some(EngineEvent::RecursivePaths(paths)) =
                    crate::daemon::watcher::watcher_signal_engine_event(
                        &fixture.root,
                        &signal,
                        &mut policy_cache,
                    )
                {
                    recursive_roots.extend(paths);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => quiet_intervals += 1,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    assert!(
        recursive_roots.iter().any(|path| {
            path == &WorkspacePath::new("project") || path == &WorkspacePath::new("project/.git")
        }),
        "dense Git creation must yield a recursive project or .git root; got {recursive_roots:?}"
    );
    drop(watcher);
    let _ = fs::remove_dir_all(fixture.temp);
}
