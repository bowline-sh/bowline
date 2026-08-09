//! Watcher-kernel to manifest-engine routing integration.

use std::fs;
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
use crate::daemon::send_watcher_signal;
#[cfg(target_os = "linux")]
use crate::daemon::socket_server::{WatcherBridge, WatcherBridgeStart};
use crate::daemon::start_sync_watcher;
#[cfg(target_os = "linux")]
use crate::daemon::sync::{ContinuousSyncRuntime, DaemonRuntime};
use crate::daemon::tests::watcher_fixture;
#[cfg(target_os = "linux")]
use crate::daemon::tests::watcher_test_runtime;
#[cfg(target_os = "linux")]
use crate::daemon::watcher::WatcherOverflowLane;
#[cfg(target_os = "linux")]
use bowline_local::notifications::NotificationDedupe;
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
use std::sync::{Arc, Mutex};

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

/// A manifest driver whose watcher accumulator remains observable without
/// running the real engine, so the bridge's bounded handoff can be asserted.
#[cfg(target_os = "linux")]
fn recording_driver() -> (
    bowline_daemon::manifest_driver::ManifestDriver,
    Arc<Mutex<Vec<EngineEvent>>>,
) {
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&recorded);
    let driver = bowline_daemon::manifest_driver::ManifestDriver::spawn(move |inbox, snapshot| {
        let Some(ingress) = snapshot.watcher_ingress_endpoint() else {
            return;
        };
        let watcher_wake = ingress.wake_receiver();
        loop {
            crossbeam_channel::select! {
                recv(inbox) -> event => match event {
                    Ok(EngineEvent::Shutdown) | Err(_) => break,
                    Ok(event) => {
                        if let Ok(mut recorded) = sink.lock() {
                            recorded.push(event);
                        }
                    }
                },
                recv(watcher_wake) -> _ => {
                    if let Ok(mut recorded) = sink.lock() {
                        recorded.extend(ingress.drain().into_events());
                    }
                },
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
        if recorded.lock().is_ok_and(|events| {
            events.iter().any(|event| match event {
                EngineEvent::Paths(paths) => paths.contains(&wanted),
                EngineEvent::FullScanRequired(_) => true,
                _ => false,
            })
        }) {
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
    let (watcher, signals) = start_sync_watcher(
        &root,
        bowline_daemon::watcher_coverage::WatcherCoverageIds::new(),
    )
    .expect("native watcher starts with watcher-ready coverage");
    let mut sync = watcher_test_runtime(
        root.clone(),
        fixture.state_root.clone(),
        fixture.workspace_id.as_str(),
    );
    // This test owns native path routing, not startup recovery. Supplying the
    // ready native stream directly keeps the open startup incident from
    // withholding ordinary events while its synthetic driver cannot publish
    // an engine convergence receipt.
    sync.watcher = crate::daemon::sync::WatcherHost::armed_with_signals(signals);
    let (driver, ingress) = recording_driver();
    sync.manifest_engine = crate::daemon::sync::ManifestEngineHost::Active(driver);
    let mut runtime = daemon_runtime_with_sync(sync);
    let WatcherBridgeStart::Started(bridge) =
        WatcherBridge::start(&mut runtime).expect("watcher bridge starts")
    else {
        panic!("watcher receiver creates bridge");
    };

    fs::write(project.join("src/deep/lib.rs"), "pub fn after() {}\n").expect("nested user edit");
    await_recorded_paths(&ingress, "project/src/deep/lib.rs");

    drop(watcher);
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
    let overflow_lane = WatcherOverflowLane::default();
    send_watcher_signal(
        &signal_tx,
        &overflow_lane,
        Ok(Event::new(EventKind::Access(AccessKind::Close(AccessMode::Write))).add_path(env_path)),
    );
    drop(signal_tx);

    let mut sync = watcher_test_runtime(
        fixture.root.clone(),
        fixture.state_root.clone(),
        fixture.workspace_id.as_str(),
    );
    sync.watcher = crate::daemon::sync::WatcherHost::armed_with_signals(signal_rx);
    let (driver, ingress) = recording_driver();
    sync.manifest_engine = crate::daemon::sync::ManifestEngineHost::Active(driver);
    let mut runtime = daemon_runtime_with_sync(sync);
    let WatcherBridgeStart::Started(bridge) =
        WatcherBridge::start(&mut runtime).expect("watcher bridge starts")
    else {
        panic!("watcher receiver creates bridge");
    };

    await_recorded_paths(&ingress, ".env");

    bridge.join().expect("watcher bridge joins");
    drop(runtime);
    let _ = fs::remove_dir_all(fixture.temp);
}

#[cfg(target_os = "macos")]
#[test]
fn macos_watcher_kernel_arms_recursive_root_watch() {
    let fixture = watcher_fixture("bowline-daemon-watch-macos-kernel", "ws_watch_macos_kernel");
    fs::create_dir_all(fixture.root.join("project/src")).expect("project subtree");
    let (watcher, receiver) = start_sync_watcher(
        &fixture.root,
        bowline_daemon::watcher_coverage::WatcherCoverageIds::new(),
    )
    .expect("watcher starts");

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
    let (watcher, receiver) = start_sync_watcher(
        &fixture.root,
        bowline_daemon::watcher_coverage::WatcherCoverageIds::new(),
    )
    .expect("watcher starts");

    // `start_sync_watcher` returns before FSEvents has finished arming, so the
    // dense burst below can land entirely in the unarmed window and never be
    // reported. Wait for the stream to prove itself on a throwaway path first.
    // This test used to pass alone and fail beside
    // `macos_watcher_kernel_arms_recursive_root_watch`, because tearing that
    // test's stream down lengthens the arm here — a race, not a slow machine,
    // and one no timeout can fix.
    let armed = fixture.root.join(".arming-probe");
    let arm_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        fs::write(&armed, b"probe").expect("arming probe");
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(_) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                assert!(
                    Instant::now() < arm_deadline,
                    "watcher never armed on the fixture root"
                );
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                panic!("watcher stream closed before arming")
            }
        }
    }
    let _ = fs::remove_file(&armed);

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
    // Three quiet intervals mean the event stream has SETTLED, which is only a
    // meaningful signal once it has started. FSEvents coalesces with latency,
    // so on a loaded machine the first signal can arrive later than 300ms —
    // and counting silence from the top treated "not started yet" as "finished",
    // abandoning the wait with almost all of the 5s deadline unspent. Until the
    // first signal lands the deadline is the only bound.
    let mut stream_started = false;
    while Instant::now() < deadline && (!stream_started || quiet_intervals < 3) {
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(signal) => {
                stream_started = true;
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
