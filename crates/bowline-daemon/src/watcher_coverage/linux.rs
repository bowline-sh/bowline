use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crossbeam_channel::{Receiver, Sender};
use notify::event::{EventKind, ModifyKind, RenameMode};
use notify::inotify::{INotifyCoverageSignal, INotifyCoverageToken};
use notify::{Config, INotifyWatcher, RecursiveMode};

use super::{
    CoverageWait, LinuxCallbackDrain, LinuxCoverageBoundary, LinuxCoveragePreparation,
    LinuxWatcherReady, NativeEventHandler, WatcherBoundaryId, WatcherCoverageAdapter,
    WatcherCoverageBoundary, WatcherCoverageError, WatcherCoverageHandoff, WatcherCoverageIds,
    WatcherCoverageLoss, WatcherCoveragePreparation, WatcherStreamEpoch, wait_for_control,
};

const QUEUE_OVERFLOW: u8 = 1;
const ROOT_CHANGED: u8 = 2;
const BACKEND_FAILURE: u8 = 3;
const INVALID_REASON_BITS: u32 = 3;
const INVALID_REASON_MASK: u64 = (1 << INVALID_REASON_BITS) - 1;

pub(super) struct LinuxStreamState {
    epoch: WatcherStreamEpoch,
    ids: WatcherCoverageIds,
    root: PathBuf,
    current_token: AtomicU64,
    ready_token: AtomicU64,
    ready_generation: AtomicU64,
    invalid_state: AtomicU64,
    stopped: AtomicBool,
    wake_tx: Sender<()>,
    wake_rx: Receiver<()>,
}

impl LinuxStreamState {
    fn new(epoch: WatcherStreamEpoch, root: PathBuf, ids: WatcherCoverageIds) -> Arc<Self> {
        let (wake_tx, wake_rx) = crossbeam_channel::bounded(1);
        Arc::new(Self {
            epoch,
            ids,
            root,
            current_token: AtomicU64::new(0),
            ready_token: AtomicU64::new(0),
            ready_generation: AtomicU64::new(0),
            invalid_state: AtomicU64::new(0),
            stopped: AtomicBool::new(false),
            wake_tx,
            wake_rx,
        })
    }

    fn begin(&self, token: WatcherBoundaryId) {
        self.current_token.store(token.get(), Ordering::Release);
        let _ = self.wake_tx.try_send(());
    }

    fn observe_control(&self, signal: INotifyCoverageSignal) {
        match signal {
            INotifyCoverageSignal::Ready(token) => {
                self.ready_generation
                    .store(self.ids.current_authority_generation(), Ordering::Release);
                self.ready_token.store(token.get(), Ordering::Release);
            }
            INotifyCoverageSignal::Failed(token, _) => {
                self.invalidate_token(token.get(), BACKEND_FAILURE);
            }
            INotifyCoverageSignal::Stopped => {
                self.observe_worker_exit();
            }
        }
        let _ = self.wake_tx.try_send(());
    }

    fn observe_worker_exit(&self) {
        if !self.stopped.swap(true, Ordering::AcqRel) {
            self.ids
                .observe_loss(self.epoch, WatcherCoverageLoss::StreamStopped);
        }
    }

    fn observe_event(&self, event: &notify::Result<notify::Event>) {
        let reason = match event {
            Ok(event) if event.need_rescan() => Some(QUEUE_OVERFLOW),
            Ok(event)
                if event.paths.iter().any(|path| path == &self.root)
                    && matches!(
                        event.kind,
                        EventKind::Remove(_)
                            | EventKind::Modify(ModifyKind::Name(
                                RenameMode::From | RenameMode::Any
                            ))
                    ) =>
            {
                Some(ROOT_CHANGED)
            }
            Ok(_) => None,
            Err(_) => Some(BACKEND_FAILURE),
        };
        if let Some(reason) = reason {
            let token = self.current_token.load(Ordering::Acquire);
            self.invalidate_token(token, reason);
            let _ = self.wake_tx.try_send(());
        }
    }

    fn invalidate_token(&self, token: u64, reason: u8) {
        let loss = match reason {
            QUEUE_OVERFLOW => WatcherCoverageLoss::QueueOverflow,
            ROOT_CHANGED => WatcherCoverageLoss::RootChanged,
            _ => WatcherCoverageLoss::BackendFailure,
        };
        self.ids.observe_loss(self.epoch, loss);
        if token == 0 {
            return;
        }
        let Some(next) = pack_invalid_state(token, reason) else {
            return;
        };
        let mut invalid = self.invalid_state.load(Ordering::Acquire);
        while token > invalid >> INVALID_REASON_BITS {
            match self.invalid_state.compare_exchange_weak(
                invalid,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(current) => invalid = current,
            }
        }
    }

    fn inspect(&self, token: WatcherBoundaryId) -> Option<Result<(), WatcherCoverageError>> {
        if self.stopped.load(Ordering::Acquire) {
            return Some(Err(WatcherCoverageError::Loss(
                WatcherCoverageLoss::StreamStopped,
            )));
        }
        let invalid = self.invalid_state.load(Ordering::Acquire);
        if invalid >> INVALID_REASON_BITS == token.get() {
            let loss = match (invalid & INVALID_REASON_MASK) as u8 {
                QUEUE_OVERFLOW => WatcherCoverageLoss::QueueOverflow,
                ROOT_CHANGED => WatcherCoverageLoss::RootChanged,
                _ => WatcherCoverageLoss::BackendFailure,
            };
            return Some(Err(WatcherCoverageError::Loss(loss)));
        }
        (self.ready_token.load(Ordering::Acquire) == token.get()).then_some(Ok(()))
    }
}

fn pack_invalid_state(token: u64, reason: u8) -> Option<u64> {
    token
        .checked_mul(1 << INVALID_REASON_BITS)?
        .checked_add(u64::from(reason))
}

/// Inotify adapter with same-loop watcher-ready and callback-drain markers.
pub struct NativeWatcherCoverageAdapter {
    root: PathBuf,
    watcher: INotifyWatcher,
    state: Arc<LinuxStreamState>,
    watcher_ready: LinuxWatcherReady,
    ids: WatcherCoverageIds,
    current_boundary: Option<LinuxCoverageBoundary>,
    shutdown: bool,
}

impl NativeWatcherCoverageAdapter {
    pub(super) fn start(
        root: &Path,
        event_handler: NativeEventHandler,
        ids: WatcherCoverageIds,
        wait: &CoverageWait,
    ) -> Result<Self, WatcherCoverageError> {
        let root = root.to_path_buf();
        ids.invalidate_current_boundary();
        let epoch = ids.next_epoch()?;
        let ready_id = ids.next_boundary()?;
        let state = LinuxStreamState::new(epoch, root.clone(), ids.clone());
        let event_state = Arc::clone(&state);
        let control_state = Arc::clone(&state);
        let callback = Arc::clone(&event_handler);
        let mut watcher = INotifyWatcher::new_with_coverage(
            move |event| {
                event_state.observe_event(&event);
                callback(epoch, event);
            },
            move |signal| control_state.observe_control(signal),
            Config::default(),
        )
        .map_err(|_| WatcherCoverageError::CoverageUnavailable)?;

        let token = coverage_token(ready_id)?;
        state.begin(ready_id);
        if watcher
            .request_watch_ready(&root, RecursiveMode::Recursive, token)
            .is_err()
        {
            state.invalidate_token(ready_id.get(), BACKEND_FAILURE);
            let _ = watcher.shutdown();
            return Err(WatcherCoverageError::CoverageUnavailable);
        }
        if let Err(error) = wait_for_linux_control(&watcher, &state, ready_id, wait) {
            let _ = watcher.shutdown();
            return Err(error);
        }

        Ok(Self {
            root,
            watcher,
            state,
            watcher_ready: LinuxWatcherReady {
                control_id: ready_id,
            },
            ids,
            current_boundary: None,
            shutdown: false,
        })
    }

    fn establish_boundary_with(
        &mut self,
        wait: &CoverageWait,
        before_request: impl FnOnce(),
        after_ack: impl FnOnce(&LinuxStreamState),
    ) -> Result<WatcherCoverageHandoff, WatcherCoverageError> {
        if self.shutdown {
            return Err(WatcherCoverageError::Shutdown);
        }
        if self.watcher.worker_is_finished() {
            self.state.observe_worker_exit();
            return Err(WatcherCoverageError::Loss(
                WatcherCoverageLoss::StreamStopped,
            ));
        }

        self.ids.invalidate_current_boundary();
        let boundary_id = self.ids.next_boundary()?;
        let token = coverage_token(boundary_id)?;
        self.state.begin(boundary_id);
        before_request();
        if self
            .watcher
            .request_coverage_boundary(&self.root, token)
            .is_err()
        {
            self.state
                .invalidate_token(boundary_id.get(), BACKEND_FAILURE);
            return Err(WatcherCoverageError::CoverageUnavailable);
        }
        wait_for_linux_control(&self.watcher, &self.state, boundary_id, wait)?;
        after_ack(&self.state);
        let acknowledged_generation = self.state.ready_generation.load(Ordering::Acquire);
        let close_guard = self
            .ids
            .close_guard_at(boundary_id, acknowledged_generation)?;
        match self.state.inspect(boundary_id) {
            Some(Ok(())) => {}
            Some(Err(error)) => return Err(error),
            None => return Err(WatcherCoverageError::StaleBoundary),
        }
        if !close_guard.is_current() {
            return Err(WatcherCoverageError::StaleBoundary);
        }

        let boundary = LinuxCoverageBoundary {
            boundary_id,
            stream_epoch: self.state.epoch,
            watcher_ready: self.watcher_ready,
            callback_drain: LinuxCallbackDrain {
                control_id: boundary_id,
            },
        };
        self.current_boundary = Some(boundary);
        Ok(WatcherCoverageHandoff::new(
            WatcherCoverageBoundary::Linux(boundary),
            close_guard,
        ))
    }

    #[cfg(test)]
    pub(super) fn establish_boundary_before_request(
        &mut self,
        wait: &CoverageWait,
        before_request: impl FnOnce(),
    ) -> Result<WatcherCoverageHandoff, WatcherCoverageError> {
        self.establish_boundary_with(wait, before_request, |_| {})
    }

    #[cfg(test)]
    pub(super) fn establish_boundary_after_ack(
        &mut self,
        wait: &CoverageWait,
        after_ack: impl FnOnce(&LinuxStreamState),
    ) -> Result<WatcherCoverageHandoff, WatcherCoverageError> {
        self.establish_boundary_with(wait, || {}, after_ack)
    }

    #[cfg(test)]
    fn establish_boundary(
        &mut self,
        wait: &CoverageWait,
    ) -> Result<WatcherCoverageHandoff, WatcherCoverageError> {
        let preparation = self.begin_recovery(wait)?;
        self.seal_after_scan(preparation, wait)
    }
}

fn coverage_token(
    boundary_id: WatcherBoundaryId,
) -> Result<INotifyCoverageToken, WatcherCoverageError> {
    if pack_invalid_state(boundary_id.get(), BACKEND_FAILURE).is_none() {
        return Err(WatcherCoverageError::IdentifierExhausted);
    }
    INotifyCoverageToken::new(boundary_id.get()).ok_or(WatcherCoverageError::IdentifierExhausted)
}

fn wait_for_linux_control(
    watcher: &INotifyWatcher,
    state: &LinuxStreamState,
    token: WatcherBoundaryId,
    wait: &CoverageWait,
) -> Result<(), WatcherCoverageError> {
    wait_for_control(wait, &state.wake_rx, || {
        if watcher.worker_is_finished() {
            state.observe_worker_exit();
            return Some(Err(WatcherCoverageError::Loss(
                WatcherCoverageLoss::StreamStopped,
            )));
        }
        state.inspect(token)
    })
}

impl WatcherCoverageAdapter for NativeWatcherCoverageAdapter {
    fn begin_recovery(
        &mut self,
        _wait: &CoverageWait,
    ) -> Result<WatcherCoveragePreparation, WatcherCoverageError> {
        if self.shutdown {
            return Err(WatcherCoverageError::Shutdown);
        }
        if self.watcher.worker_is_finished() {
            self.state.observe_worker_exit();
            return Err(WatcherCoverageError::Loss(
                WatcherCoverageLoss::StreamStopped,
            ));
        }
        Ok(WatcherCoveragePreparation::Linux(
            LinuxCoveragePreparation {
                stream_epoch: self.state.epoch,
                watcher_ready: self.watcher_ready,
            },
        ))
    }

    fn seal_after_scan(
        &mut self,
        preparation: WatcherCoveragePreparation,
        wait: &CoverageWait,
    ) -> Result<WatcherCoverageHandoff, WatcherCoverageError> {
        let WatcherCoveragePreparation::Linux(preparation) = preparation else {
            return Err(WatcherCoverageError::StaleBoundary);
        };
        if preparation.stream_epoch != self.state.epoch
            || preparation.watcher_ready != self.watcher_ready
        {
            return Err(WatcherCoverageError::StaleBoundary);
        }
        self.establish_boundary_with(wait, || {}, |_| {})
    }

    fn validate_boundary(
        &self,
        handoff: &WatcherCoverageHandoff,
    ) -> Result<(), WatcherCoverageError> {
        let WatcherCoverageBoundary::Linux(boundary) = handoff.boundary() else {
            return Err(WatcherCoverageError::StaleBoundary);
        };
        if !handoff.close_guard().is_current()
            || self.shutdown
            || self.current_boundary != Some(boundary)
            || boundary.stream_epoch != self.state.epoch
            || boundary.watcher_ready != self.watcher_ready
            || boundary.callback_drain.control_id != boundary.boundary_id
            || self.state.ready_token.load(Ordering::Acquire) != boundary.boundary_id.get()
        {
            return Err(WatcherCoverageError::StaleBoundary);
        }
        if self.watcher.worker_is_finished() {
            self.state.observe_worker_exit();
            return Err(WatcherCoverageError::Loss(
                WatcherCoverageLoss::StreamStopped,
            ));
        }
        self.state
            .inspect(boundary.boundary_id)
            .unwrap_or(Err(WatcherCoverageError::StaleBoundary))
    }

    fn shutdown(&mut self) -> Result<(), WatcherCoverageError> {
        if self.shutdown {
            return Ok(());
        }
        self.shutdown = true;
        self.current_boundary = None;
        self.watcher
            .shutdown()
            .map_err(|_| WatcherCoverageError::CoverageUnavailable)
    }
}

impl Drop for NativeWatcherCoverageAdapter {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::num::NonZeroU64;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::watcher_coverage::{CoverageCancellation, unique_test_root};

    fn boundary_id(value: u64) -> WatcherBoundaryId {
        WatcherBoundaryId(NonZeroU64::new(value).expect("test boundary id is nonzero"))
    }

    fn wait_for(seconds: u64) -> CoverageWait {
        CoverageWait::new(
            Instant::now() + Duration::from_secs(seconds),
            CoverageCancellation::new(),
        )
    }

    #[test]
    fn packed_loss_state_preserves_the_token_and_typed_reason() {
        let state = LinuxStreamState::new(
            WatcherStreamEpoch(NonZeroU64::MIN),
            PathBuf::from("/tmp/bowline-linux-packed-state"),
            WatcherCoverageIds::new(),
        );
        let token = boundary_id(7);
        state.begin(token);
        state.invalidate_token(token.get(), QUEUE_OVERFLOW);
        assert_eq!(
            state.inspect(token),
            Some(Err(WatcherCoverageError::Loss(
                WatcherCoverageLoss::QueueOverflow
            )))
        );

        let next = boundary_id(8);
        state.begin(next);
        state.invalidate_token(next.get(), ROOT_CHANGED);
        assert_eq!(
            state.inspect(next),
            Some(Err(WatcherCoverageError::Loss(
                WatcherCoverageLoss::RootChanged
            )))
        );
    }

    #[test]
    fn watcher_ready_and_same_loop_drain_precede_the_boundary_marker() {
        let root = unique_test_root("bowline-inotify-boundary");
        fs::create_dir_all(&root).expect("test root exists");
        let target = root.join("before-boundary.txt");
        let (event_tx, event_rx) = mpsc::channel();
        let callback: NativeEventHandler = Arc::new(move |_epoch, event| {
            let _ = event_tx.send(event);
        });
        let mut adapter = NativeWatcherCoverageAdapter::start(
            &root,
            callback,
            WatcherCoverageIds::new(),
            &wait_for(10),
        )
        .expect("recursive watcher-ready marker");
        let handoff = adapter
            .establish_boundary_before_request(&wait_for(10), || {
                fs::write(&target, b"queued before marker").expect("test edit");
            })
            .expect("same-loop callback drain marker");
        let WatcherCoverageBoundary::Linux(boundary) = handoff.boundary() else {
            panic!("Linux adapter returned a Darwin boundary");
        };
        assert_ne!(
            boundary.watcher_ready().control_id(),
            boundary.callback_drain().control_id()
        );
        assert_eq!(
            boundary.callback_drain().control_id(),
            boundary.boundary_id()
        );
        assert!(boundary.stream_epoch().get() > 0);
        assert!(event_rx.try_iter().any(|event| {
            event.is_ok_and(|event| event.paths.iter().any(|path| path == &target))
        }));

        adapter
            .validate_boundary(&handoff)
            .expect("marker is current and live");
        adapter
            .validate_boundary(&handoff)
            .expect("same stream remains live after close");
        adapter
            .state
            .invalidate_token(handoff.boundary_id().get(), QUEUE_OVERFLOW);
        assert!(
            !handoff.close_guard().is_current(),
            "native invalidation before close must reject the old handoff"
        );
        adapter.shutdown().expect("inotify worker joins");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn post_close_worker_stop_invalidates_and_publishes_outward() {
        let ids = WatcherCoverageIds::new();
        let observations = ids.observation_receiver();
        let boundary_id = ids.next_boundary().expect("test boundary id");
        let guard = ids.close_guard(boundary_id).expect("test close guard");
        let state = LinuxStreamState::new(
            WatcherStreamEpoch(NonZeroU64::MIN),
            PathBuf::from("/tmp/bowline-linux-worker-stop"),
            ids,
        );
        state.observe_control(INotifyCoverageSignal::Stopped);
        assert!(!guard.is_current());
        assert_eq!(
            observations
                .recv_timeout(Duration::from_secs(1))
                .expect("worker stop is published")
                .loss(),
            WatcherCoverageLoss::StreamStopped
        );
    }

    #[test]
    fn loss_after_ready_ack_cannot_be_folded_into_a_current_guard() {
        let root = unique_test_root("bowline-inotify-ack-race");
        fs::create_dir_all(&root).expect("test root exists");
        let ids = WatcherCoverageIds::new();
        let observations = ids.observation_receiver();
        let mut adapter =
            NativeWatcherCoverageAdapter::start(&root, Arc::new(|_, _| {}), ids, &wait_for(10))
                .expect("watcher-ready marker");

        let result = adapter.establish_boundary_after_ack(&wait_for(10), |state| {
            let token = state.current_token.load(Ordering::Acquire);
            state.invalidate_token(token, QUEUE_OVERFLOW);
        });
        assert!(matches!(
            result,
            Err(WatcherCoverageError::Loss(
                WatcherCoverageLoss::QueueOverflow
            ))
        ));
        assert_eq!(
            observations
                .recv_timeout(Duration::from_secs(1))
                .expect("post-ack loss is published")
                .loss(),
            WatcherCoverageLoss::QueueOverflow
        );
        adapter.shutdown().expect("inotify worker joins");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn move_in_then_immediate_edit_is_covered_by_rewalk_scan_and_live_watch() {
        let base = unique_test_root("bowline-inotify-move-in");
        let root = base.join("workspace");
        let outside = base.join("outside");
        let nested = outside.join("deep");
        fs::create_dir_all(&root).expect("workspace exists");
        fs::create_dir_all(&nested).expect("outside subtree exists");
        fs::write(nested.join("note.txt"), b"before").expect("seed file");
        let (event_tx, event_rx) = mpsc::channel();
        let callback: NativeEventHandler = Arc::new(move |_epoch, event| {
            let _ = event_tx.send(event);
        });
        let mut adapter = NativeWatcherCoverageAdapter::start(
            &root,
            callback,
            WatcherCoverageIds::new(),
            &wait_for(10),
        )
        .expect("recursive watcher-ready marker");
        let incoming = root.join("incoming");
        let edited = incoming.join("deep/note.txt");
        let handoff = adapter
            .establish_boundary_before_request(&wait_for(10), || {
                fs::rename(&outside, &incoming).expect("move subtree into workspace");
                fs::write(&edited, b"after").expect("immediate descendant edit");
            })
            .expect("rewalk and callback drain cover move-in");

        assert_eq!(
            fs::read(&edited).expect("edited file remains present"),
            b"after"
        );
        adapter
            .validate_boundary(&handoff)
            .expect("coverage remains valid after move-in");
        let _ = event_rx.try_iter().count();
        fs::write(&edited, b"after boundary").expect("post-boundary descendant edit");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .expect("recursive watch observes descendant after boundary");
            let event = event_rx
                .recv_timeout(remaining)
                .expect("post-boundary callback arrives");
            if event.is_ok_and(|event| event.paths.iter().any(|path| path == &edited)) {
                break;
            }
        }
        adapter.shutdown().expect("inotify worker joins");
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn saturated_data_lane_does_not_block_the_control_acknowledgement() {
        let root = unique_test_root("bowline-inotify-control-lane");
        fs::create_dir_all(&root).expect("test root exists");
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
        .expect("watcher-ready bypasses full data lane");
        let handoff = adapter
            .establish_boundary_before_request(&wait_for(10), || {
                fs::write(root.join("saturated.txt"), b"data").expect("test edit");
            })
            .expect("drain acknowledgement bypasses full data lane");
        adapter
            .validate_boundary(&handoff)
            .expect("boundary remains valid");
        adapter.shutdown().expect("inotify worker joins");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn strict_root_failure_never_acks_ready_and_recreation_can_rearm() {
        let root = unique_test_root("bowline-inotify-root-recreate");
        fs::create_dir_all(&root).expect("test root exists");
        let callback: NativeEventHandler = Arc::new(|_, _| {});
        let mut adapter = NativeWatcherCoverageAdapter::start(
            &root,
            callback,
            WatcherCoverageIds::new(),
            &wait_for(10),
        )
        .expect("watcher-ready marker");

        let failed = adapter.establish_boundary_before_request(&wait_for(10), || {
            fs::remove_dir_all(&root).expect("remove watched root");
        });
        assert!(matches!(
            failed,
            Err(WatcherCoverageError::Loss(
                WatcherCoverageLoss::RootChanged | WatcherCoverageLoss::BackendFailure
            ))
        ));

        fs::create_dir_all(&root).expect("recreate watched root");
        let handoff = adapter
            .establish_boundary(&wait_for(10))
            .expect("strict rewalk rearms recreated root");
        adapter
            .validate_boundary(&handoff)
            .expect("recreated root boundary is live");
        adapter.shutdown().expect("inotify worker joins");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn shared_ids_advance_across_inotify_reconstruction() {
        let root = unique_test_root("bowline-inotify-stable-identities");
        fs::create_dir_all(&root).expect("test root exists");
        let ids = WatcherCoverageIds::new();
        let callback: NativeEventHandler = Arc::new(|_, _| {});
        let mut first = NativeWatcherCoverageAdapter::start(
            &root,
            Arc::clone(&callback),
            ids.clone(),
            &wait_for(10),
        )
        .expect("first watcher starts");
        let first_boundary = first
            .establish_boundary(&wait_for(10))
            .expect("first callback-drain boundary");
        first.shutdown().expect("first watcher stops");

        let mut second = NativeWatcherCoverageAdapter::start(&root, callback, ids, &wait_for(10))
            .expect("second watcher starts");
        let second_boundary = second
            .establish_boundary(&wait_for(10))
            .expect("second callback-drain boundary");
        assert!(second_boundary.boundary_id() > first_boundary.boundary_id());
        assert!(second_boundary.live_stream_epoch() > first_boundary.live_stream_epoch());
        second.shutdown().expect("second watcher stops");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cancellation_fails_closed_and_shutdown_joins_the_worker() {
        let root = unique_test_root("bowline-inotify-cancel");
        fs::create_dir_all(&root).expect("test root exists");
        let callback: NativeEventHandler = Arc::new(|_, _| {});
        let mut adapter = NativeWatcherCoverageAdapter::start(
            &root,
            callback,
            WatcherCoverageIds::new(),
            &wait_for(10),
        )
        .expect("watcher-ready marker");
        let cancellation = CoverageCancellation::new();
        cancellation.cancel();
        let cancelled_wait =
            CoverageWait::new(Instant::now() + Duration::from_secs(10), cancellation);
        assert!(matches!(
            adapter.establish_boundary(&cancelled_wait),
            Err(WatcherCoverageError::Cancelled)
        ));
        adapter.shutdown().expect("inotify worker joins");
        adapter.shutdown().expect("shutdown is idempotent");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn continuous_producer_cancellation_returns_and_shutdown_interrupts_drain() {
        let root = unique_test_root("bowline-inotify-continuous-cancel");
        fs::create_dir_all(&root).expect("test root exists");
        let callback: NativeEventHandler = Arc::new(|_, _| {
            thread::sleep(Duration::from_millis(1));
        });
        let adapter = NativeWatcherCoverageAdapter::start(
            &root,
            callback,
            WatcherCoverageIds::new(),
            &wait_for(10),
        )
        .expect("watcher-ready marker");

        let producing = Arc::new(AtomicBool::new(true));
        let producers = (0..4)
            .map(|worker| {
                let root = root.clone();
                let producing = Arc::clone(&producing);
                thread::spawn(move || {
                    let mut sequence = 0_u64;
                    while producing.load(Ordering::Acquire) {
                        let path = root.join(format!("worker-{worker}-{}.txt", sequence % 64));
                        let _ = fs::write(path, sequence.to_le_bytes());
                        sequence = sequence.wrapping_add(1);
                    }
                })
            })
            .collect::<Vec<_>>();

        let cancellation = CoverageCancellation::new();
        let cancel_from_test = cancellation.clone();
        let boundary_worker = thread::spawn(move || {
            let mut adapter = adapter;
            let wait = CoverageWait::new(Instant::now() + Duration::from_secs(10), cancellation);
            let result = adapter.establish_boundary(&wait);
            (adapter, result)
        });
        thread::sleep(Duration::from_millis(30));
        cancel_from_test.cancel();
        let (mut adapter, result) = boundary_worker
            .join()
            .expect("cancelled boundary worker joins");
        assert!(matches!(result, Err(WatcherCoverageError::Cancelled)));

        let shutdown_started = Instant::now();
        adapter
            .shutdown()
            .expect("shutdown interrupts a continuously readable drain");
        assert!(
            shutdown_started.elapsed() < Duration::from_secs(3),
            "shutdown must not wait for a producer-created WouldBlock"
        );
        producing.store(false, Ordering::Release);
        for producer in producers {
            producer.join().expect("producer joins");
        }
        let _ = fs::remove_dir_all(root);
    }
}
