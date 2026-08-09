use std::fs;
use std::num::NonZeroU64;
use std::sync::{Mutex, MutexGuard, mpsc};
use std::time::{Duration, Instant};

use notify::fsevent::FsEventCoverageFlags;

use super::*;
use crate::watcher_coverage::{CoverageCancellation, unique_test_root};

const MUST_SCAN_SUBDIRS_FLAG: u32 = 0x0000_0001;
const USER_DROPPED_FLAG: u32 = 0x0000_0002;
const KERNEL_DROPPED_FLAG: u32 = 0x0000_0004;
const IDS_WRAPPED_FLAG: u32 = 0x0000_0008;
const HISTORY_DONE_FLAG: u32 = 0x0000_0010;
const ROOT_CHANGED_FLAG: u32 = 0x0000_0020;

static FSEVENTS_TEST_LEASE: Mutex<()> = Mutex::new(());

fn fsevents_test_lease() -> MutexGuard<'static, ()> {
    // A full-suite run otherwise asks macOS to establish several independent
    // HistoryDone streams at once, which can starve every test without testing
    // a product concurrency contract.
    FSEVENTS_TEST_LEASE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn epoch(value: u64) -> WatcherStreamEpoch {
    WatcherStreamEpoch(NonZeroU64::new(value).expect("test epoch is nonzero"))
}

fn wait_for(seconds: u64) -> CoverageWait {
    CoverageWait::new(
        Instant::now() + Duration::from_secs(seconds),
        CoverageCancellation::new(),
    )
}

fn coverage_event(cursor: u64, flags: u32) -> FsEventCoverageSignal {
    FsEventCoverageSignal::Event(FsEventCoverageEvent::new(
        FsEventCursor::from_raw(cursor),
        FsEventCoverageFlags::from_raw(flags),
    ))
}

fn establish_test_seal(
    adapter: &mut NativeWatcherCoverageAdapter,
    wait: &CoverageWait,
) -> Result<WatcherCoverageHandoff, WatcherCoverageError> {
    let preparation = adapter.begin_recovery(wait)?;
    adapter.seal_after_scan(preparation, wait)
}

#[test]
fn history_done_is_required_and_timeout_cannot_authorize_coverage() {
    let state = DarwinStreamState::new(epoch(1), FseventsCursor(10), WatcherCoverageIds::new());
    state.observe(FsEventCoverageSignal::Started);
    let wait = CoverageWait::new(
        Instant::now() + Duration::from_millis(20),
        CoverageCancellation::new(),
    );
    let result = wait_for_control(&wait, &state.wake_rx, || state.inspect_history());
    assert_eq!(result, Err(WatcherCoverageError::TimedOut));
    assert!(!state.history_done.load(Ordering::Acquire));
}

#[test]
fn history_loss_root_change_and_cursor_regression_invalidate_coverage() {
    for (flags, expected) in [
        (USER_DROPPED_FLAG, WatcherCoverageLoss::UserDropped),
        (KERNEL_DROPPED_FLAG, WatcherCoverageLoss::KernelDropped),
        (IDS_WRAPPED_FLAG, WatcherCoverageLoss::EventIdsWrapped),
        (ROOT_CHANGED_FLAG, WatcherCoverageLoss::RootChanged),
    ] {
        let state = DarwinStreamState::new(epoch(1), FseventsCursor(10), WatcherCoverageIds::new());
        state.observe(FsEventCoverageSignal::Started);
        state.observe(coverage_event(11, flags | HISTORY_DONE_FLAG));
        assert_eq!(
            state.inspect_history(),
            Some(Err(WatcherCoverageError::Loss(expected)))
        );
    }

    let state = DarwinStreamState::new(epoch(1), FseventsCursor(10), WatcherCoverageIds::new());
    state.observe(FsEventCoverageSignal::Started);
    state.observe(coverage_event(9, 0));
    assert_eq!(state.loss(), Some(WatcherCoverageLoss::NonMonotonicCursor));

    let state = DarwinStreamState::new(epoch(1), FseventsCursor(10), WatcherCoverageIds::new());
    state.observe(FsEventCoverageSignal::Started);
    state.observe(coverage_event(0, ROOT_CHANGED_FLAG));
    assert_eq!(state.loss(), Some(WatcherCoverageLoss::RootChanged));
    assert_eq!(state.last_safe.load(Ordering::Acquire), 10);
}

#[test]
fn wrapped_identifiers_dominate_combined_native_loss_flags() {
    for combined in [
        IDS_WRAPPED_FLAG | USER_DROPPED_FLAG,
        IDS_WRAPPED_FLAG | KERNEL_DROPPED_FLAG,
        IDS_WRAPPED_FLAG | ROOT_CHANGED_FLAG,
        IDS_WRAPPED_FLAG | USER_DROPPED_FLAG | KERNEL_DROPPED_FLAG | ROOT_CHANGED_FLAG,
    ] {
        let state = DarwinStreamState::new(epoch(1), FseventsCursor(10), WatcherCoverageIds::new());
        state.observe(FsEventCoverageSignal::Started);
        state.observe(coverage_event(11, combined));
        assert_eq!(state.loss(), Some(WatcherCoverageLoss::EventIdsWrapped));
    }
}

#[test]
fn post_close_native_losses_invalidate_and_publish_outward() {
    for (expected, observe) in [
        (
            WatcherCoverageLoss::EventIdsWrapped,
            coverage_event(11, IDS_WRAPPED_FLAG),
        ),
        (
            WatcherCoverageLoss::NonMonotonicCursor,
            coverage_event(9, 0),
        ),
    ] {
        let ids = WatcherCoverageIds::new();
        let observations = ids.observation_receiver();
        let boundary_id = ids.next_boundary().expect("test boundary id");
        let guard = ids.close_guard(boundary_id).expect("test close guard");
        let state = DarwinStreamState::new(epoch(1), FseventsCursor(10), ids);
        state.observe(FsEventCoverageSignal::Started);
        state.observe(observe);
        assert!(!guard.is_current());
        assert_eq!(
            observations
                .recv_timeout(Duration::from_secs(1))
                .expect("loss is published")
                .loss(),
            expected
        );
    }

    let ids = WatcherCoverageIds::new();
    let observations = ids.observation_receiver();
    let boundary_id = ids.next_boundary().expect("test boundary id");
    let guard = ids.close_guard(boundary_id).expect("test close guard");
    let state = DarwinStreamState::new(epoch(1), FseventsCursor(10), ids);
    state.observe(FsEventCoverageSignal::Started);
    state.observe_worker_exit();
    assert!(!guard.is_current());
    assert_eq!(
        observations
            .recv_timeout(Duration::from_secs(1))
            .expect("worker exit is published")
            .loss(),
        WatcherCoverageLoss::StreamStopped
    );
}

#[test]
fn loss_markers_and_later_events_do_not_advance_the_last_safe_cursor() {
    let state = DarwinStreamState::new(epoch(1), FseventsCursor(10), WatcherCoverageIds::new());
    state.observe(FsEventCoverageSignal::Started);
    state.observe(coverage_event(20, USER_DROPPED_FLAG));
    state.observe(coverage_event(21, 0));
    assert_eq!(state.last_delivered.load(Ordering::Acquire), 21);
    assert_eq!(state.last_safe.load(Ordering::Acquire), 10);
}

#[test]
fn must_scan_survives_the_history_done_marker_without_claiming_loss() {
    let state = DarwinStreamState::new(epoch(1), FseventsCursor(10), WatcherCoverageIds::new());
    state.observe(FsEventCoverageSignal::Started);
    state.observe(coverage_event(
        11,
        MUST_SCAN_SUBDIRS_FLAG | HISTORY_DONE_FLAG,
    ));
    assert_eq!(state.inspect_history(), Some(Ok(FseventsCursor(10))));
    assert!(state.must_scan_subdirs.load(Ordering::Acquire));
}

#[test]
fn physical_cursor_overlap_reaches_history_done_and_preserves_scan_state() {
    let _fsevents_lease = fsevents_test_lease();
    let root = unique_test_root("bowline-fsevents-overlap");
    fs::create_dir_all(&root).expect("test root exists");
    let root = fs::canonicalize(root).expect("canonical test root");
    let sentinel = root.join("scan-sentinel.txt");
    let post_boundary = root.join("post-boundary.txt");
    let (event_tx, event_rx) = mpsc::channel();
    let callback: NativeEventHandler = Arc::new(move |epoch, event| {
        let _ = event_tx.send((epoch, event));
    });
    let ids = WatcherCoverageIds::new();
    let observations = ids.observation_receiver();
    let mut adapter = NativeWatcherCoverageAdapter::start(&root, callback, ids, &wait_for(10))
        .expect("initial FSEvents history marker");

    let preparation = adapter
        .begin_recovery_after_capture(&wait_for(10), || {
            fs::write(&sentinel, b"captured between A and B").expect("overlap edit");
        })
        .expect("replacement reaches HistoryDone");
    let scanned_paths: Vec<_> = fs::read_dir(&root)
        .expect("scan root")
        .map(|entry| entry.expect("scan entry").path())
        .collect();
    assert!(scanned_paths.contains(&sentinel));
    let scanned = fs::read(&sentinel).expect("authoritative scan observes overlap edit");
    assert_eq!(scanned, b"captured between A and B");
    let handoff = adapter
        .seal_after_scan(preparation, &wait_for(10))
        .expect("post-scan synchronous flush seals callback delivery");
    let WatcherCoverageBoundary::Darwin(boundary) = handoff.boundary() else {
        panic!("macOS adapter returned a Linux boundary");
    };
    let DarwinCoverageStart::CursorReplay {
        covered_last_safe,
        replay_from,
        recovery_cause,
    } = boundary.start()
    else {
        panic!("healthy predecessor must use cursor replay");
    };
    assert_eq!(replay_from, covered_last_safe);
    assert_eq!(recovery_cause, None);
    assert!(boundary.history_through() >= replay_from);
    assert_ne!(boundary.covered_epoch(), boundary.live_epoch());
    assert!(boundary.boundary_id().get() > 0);
    assert_eq!(boundary.history_done(), DarwinHistoryDone);
    assert!(!boundary.must_scan_subdirs());
    assert!(matches!(
        observations.try_recv(),
        Err(crossbeam_channel::TryRecvError::Empty)
    ));
    adapter
        .validate_boundary(&handoff)
        .expect("the post-scan seal is current before later activity");
    assert!(boundary.flush_generation().get() > 0);
    assert_eq!(
        boundary.loss_generation().get(),
        handoff.close_guard().token().generation()
    );
    fs::write(&post_boundary, b"live after boundary").expect("post-boundary edit");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .expect("promoted stream observes post-boundary activity");
        let (epoch, event) = event_rx
            .recv_timeout(remaining)
            .expect("post-boundary callback arrives");
        if epoch == boundary.live_epoch()
            && event.is_ok_and(|event| event.paths.iter().any(|path| path == &post_boundary))
        {
            break;
        }
    }
    let invalidation_deadline = Instant::now() + Duration::from_secs(1);
    loop {
        if matches!(
            adapter.validate_boundary(&handoff),
            Err(WatcherCoverageError::StaleBoundary)
        ) {
            break;
        }
        assert!(
            Instant::now() < invalidation_deadline,
            "the callback-return generation invalidates the earlier seal"
        );
        std::thread::yield_now();
    }
    adapter.shutdown().expect("replacement worker joins");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn loss_after_history_ack_cannot_be_folded_into_a_current_guard() {
    let _fsevents_lease = fsevents_test_lease();
    let root = unique_test_root("bowline-fsevents-ack-race");
    fs::create_dir_all(&root).expect("test root exists");
    let root = fs::canonicalize(root).expect("canonical test root");
    let ids = WatcherCoverageIds::new();
    let observations = ids.observation_receiver();
    let mut adapter =
        NativeWatcherCoverageAdapter::start(&root, Arc::new(|_, _| {}), ids, &wait_for(10))
            .expect("initial stream starts");

    let result = adapter.begin_recovery_after_ack(&wait_for(10), |state| {
        state.invalidate(ROOT_CHANGED);
    });
    assert!(matches!(
        result,
        Err(WatcherCoverageError::Loss(WatcherCoverageLoss::RootChanged))
    ));
    assert_eq!(
        observations
            .recv_timeout(Duration::from_secs(1))
            .expect("post-ack loss is published")
            .loss(),
        WatcherCoverageLoss::RootChanged
    );
    adapter.shutdown().expect("active worker joins");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn saturated_data_lane_cannot_block_the_history_control_lane() {
    let _fsevents_lease = fsevents_test_lease();
    let root = unique_test_root("bowline-fsevents-control-lane");
    fs::create_dir_all(&root).expect("test root exists");
    let root = fs::canonicalize(root).expect("canonical test root");
    let (data_tx, _data_rx) = mpsc::sync_channel(1);
    data_tx.send(()).expect("fill data lane");
    let callback: NativeEventHandler = Arc::new(move |_epoch, _event| {
        let _ = data_tx.try_send(());
    });
    let mut adapter = NativeWatcherCoverageAdapter::start(
        &root,
        callback,
        WatcherCoverageIds::new(),
        &wait_for(10),
    )
    .expect("initial control marker bypasses full data lane");
    let preparation = adapter
        .begin_recovery_after_capture(&wait_for(10), || {
            fs::write(root.join("saturated.txt"), b"data").expect("test edit");
        })
        .expect("HistoryDone bypasses full data lane");
    let handoff = adapter
        .seal_after_scan(preparation, &wait_for(10))
        .expect("native flush bypasses the saturated data lane");
    adapter
        .validate_boundary(&handoff)
        .expect("boundary remains valid");
    adapter
        .validate_boundary(&handoff)
        .expect("replacement is already the promoted stream");
    adapter.shutdown().expect("active worker joins");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn degraded_streams_select_fresh_handoffs_and_promote_replacement() {
    let _fsevents_lease = fsevents_test_lease();
    let root = unique_test_root("bowline-fsevents-loss-handoff");
    fs::create_dir_all(&root).expect("test root exists");
    let root = fs::canonicalize(root).expect("canonical test root");
    let callback: NativeEventHandler = Arc::new(|_, _| {});
    let mut adapter = NativeWatcherCoverageAdapter::start(
        &root,
        callback,
        WatcherCoverageIds::new(),
        &wait_for(10),
    )
    .expect("initial stream starts");

    let safe = adapter
        .active
        .as_ref()
        .expect("active stream")
        .state
        .last_safe
        .load(Ordering::Acquire);
    adapter
        .active
        .as_ref()
        .expect("active stream")
        .state
        .observe(coverage_event(safe.saturating_add(1), USER_DROPPED_FLAG));
    let dropped_handoff = establish_test_seal(&mut adapter, &wait_for(10))
        .expect("dropped predecessor is covered by a fresh stream and full scan");
    let WatcherCoverageBoundary::Darwin(dropped) = dropped_handoff.boundary() else {
        panic!("macOS adapter returned Linux proof");
    };
    assert!(matches!(
        dropped.start(),
        DarwinCoverageStart::FreshStream {
            discontinuity: WatcherCoverageLoss::UserDropped,
            ..
        }
    ));
    assert_eq!(
        adapter.active.as_ref().expect("promoted B").state.epoch,
        dropped.live_epoch()
    );
    assert!(dropped_handoff.close_guard().is_current());

    adapter
        .active
        .as_ref()
        .expect("promoted B")
        .state
        .observe(coverage_event(0, ROOT_CHANGED_FLAG));
    assert!(
        !dropped_handoff.close_guard().is_current(),
        "native invalidation before close must reject the old handoff"
    );
    let fresh_handoff = establish_test_seal(&mut adapter, &wait_for(10))
        .expect("root discontinuity selects a fresh live stream");
    let WatcherCoverageBoundary::Darwin(fresh) = fresh_handoff.boundary() else {
        panic!("macOS adapter returned Linux proof");
    };
    assert!(matches!(
        fresh.start(),
        DarwinCoverageStart::FreshStream {
            discontinuity: WatcherCoverageLoss::RootChanged,
            ..
        }
    ));
    adapter
        .validate_boundary(&fresh_handoff)
        .expect("fresh replacement is current and live");
    adapter.shutdown().expect("replacement worker joins");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn shared_ids_advance_across_adapter_reconstruction() {
    let _fsevents_lease = fsevents_test_lease();
    let root = unique_test_root("bowline-fsevents-stable-identities");
    fs::create_dir_all(&root).expect("test root exists");
    let root = fs::canonicalize(root).expect("canonical test root");
    let ids = WatcherCoverageIds::new();
    let callback: NativeEventHandler = Arc::new(|_, _| {});
    let mut first = NativeWatcherCoverageAdapter::start(
        &root,
        Arc::clone(&callback),
        ids.clone(),
        &wait_for(10),
    )
    .expect("first adapter starts");
    let first_boundary = establish_test_seal(&mut first, &wait_for(10)).expect("first boundary");
    first.shutdown().expect("first adapter stops");

    let mut second = NativeWatcherCoverageAdapter::start(&root, callback, ids, &wait_for(10))
        .expect("second adapter starts");
    let second_boundary = establish_test_seal(&mut second, &wait_for(10)).expect("second boundary");
    assert!(second_boundary.boundary_id() > first_boundary.boundary_id());
    assert!(second_boundary.live_stream_epoch() > first_boundary.live_stream_epoch());
    second.shutdown().expect("second adapter stops");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn cancellation_fails_closed_and_shutdown_joins_both_workers() {
    let _fsevents_lease = fsevents_test_lease();
    let root = unique_test_root("bowline-fsevents-cancel");
    fs::create_dir_all(&root).expect("test root exists");
    let root = fs::canonicalize(root).expect("canonical test root");
    let callback: NativeEventHandler = Arc::new(|_, _| {});
    let mut adapter = NativeWatcherCoverageAdapter::start(
        &root,
        callback,
        WatcherCoverageIds::new(),
        &wait_for(10),
    )
    .expect("initial stream starts");
    let cancellation = CoverageCancellation::new();
    cancellation.cancel();
    let cancelled_wait = CoverageWait::new(Instant::now() + Duration::from_secs(10), cancellation);
    assert!(matches!(
        adapter.begin_recovery(&cancelled_wait),
        Err(WatcherCoverageError::Cancelled)
    ));
    adapter.shutdown().expect("active worker joins");
    adapter.shutdown().expect("shutdown is idempotent");
    let _ = fs::remove_dir_all(root);
}
