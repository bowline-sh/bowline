use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::thread;

use crossbeam_channel::{Receiver, Sender};
use notify::fsevent::{FsEventCoverageEvent, FsEventCoverageSignal, FsEventCursor, FsEventWatcher};
use notify::{Config, RecursiveMode, Watcher};

use super::{
    CoverageWait, DarwinCoverageBoundary, DarwinCoveragePreparation, DarwinCoverageStart,
    DarwinFlushGeneration, DarwinHistoryDone, FseventsCursor, NativeEventHandler,
    WatcherCallbackGeneration, WatcherCoverageAdapter, WatcherCoverageBoundary,
    WatcherCoverageError, WatcherCoverageHandoff, WatcherCoverageIds, WatcherCoverageLoss,
    WatcherCoveragePreparation, WatcherLossGeneration, WatcherStreamEpoch, wait_for_control,
};

const VALID: u8 = 0;
const USER_DROPPED: u8 = 1;
const KERNEL_DROPPED: u8 = 2;
const IDS_WRAPPED: u8 = 3;
const ROOT_CHANGED: u8 = 4;
const STREAM_STOPPED: u8 = 5;
const NON_MONOTONIC_CURSOR: u8 = 6;

pub(super) struct DarwinStreamState {
    epoch: WatcherStreamEpoch,
    ids: WatcherCoverageIds,
    started_from: FseventsCursor,
    last_delivered: AtomicU64,
    last_safe: AtomicU64,
    history_cursor: AtomicU64,
    history_generation: AtomicU64,
    history_done: AtomicBool,
    callback_generation: AtomicU64,
    flush_generation: AtomicU64,
    must_scan_subdirs: AtomicBool,
    started: AtomicBool,
    live: AtomicBool,
    retiring: AtomicBool,
    stopped_observed: AtomicBool,
    invalid: AtomicU8,
    wake_tx: Sender<()>,
    wake_rx: Receiver<()>,
}

impl DarwinStreamState {
    fn new(
        epoch: WatcherStreamEpoch,
        started_from: FseventsCursor,
        ids: WatcherCoverageIds,
    ) -> Arc<Self> {
        let (wake_tx, wake_rx) = crossbeam_channel::bounded(1);
        Arc::new(Self {
            epoch,
            ids,
            started_from,
            last_delivered: AtomicU64::new(started_from.get()),
            last_safe: AtomicU64::new(started_from.get()),
            history_cursor: AtomicU64::new(started_from.get()),
            history_generation: AtomicU64::new(0),
            history_done: AtomicBool::new(false),
            callback_generation: AtomicU64::new(0),
            flush_generation: AtomicU64::new(0),
            must_scan_subdirs: AtomicBool::new(false),
            started: AtomicBool::new(false),
            live: AtomicBool::new(false),
            retiring: AtomicBool::new(false),
            stopped_observed: AtomicBool::new(false),
            invalid: AtomicU8::new(VALID),
            wake_tx,
            wake_rx,
        })
    }

    fn observe(&self, signal: FsEventCoverageSignal) {
        match signal {
            FsEventCoverageSignal::Started => {
                self.stopped_observed.store(false, Ordering::Release);
                self.live.store(true, Ordering::Release);
                self.started.store(true, Ordering::Release);
            }
            FsEventCoverageSignal::Event(event) => self.observe_event(event),
            FsEventCoverageSignal::Stopped => {
                self.observe_worker_exit();
            }
        }
        let _ = self.wake_tx.try_send(());
    }

    fn observe_worker_exit(&self) {
        self.live.store(false, Ordering::Release);
        if self.retiring.load(Ordering::Acquire) {
            return;
        }
        if !self.stopped_observed.swap(true, Ordering::AcqRel) {
            self.invalidate(STREAM_STOPPED);
        }
    }

    fn callback_returned(&self) {
        let _ =
            self.callback_generation
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                    value.checked_add(1)
                });
        let _ = self.wake_tx.try_send(());
    }

    fn observe_event(&self, event: FsEventCoverageEvent) {
        let flags = event.flags();
        // Wrapped identifiers destroy cursor continuity and therefore dominate
        // every other bit carried by the same native record.
        let loss = if flags.ids_wrapped() {
            Some(IDS_WRAPPED)
        } else if flags.root_changed() {
            Some(ROOT_CHANGED)
        } else if flags.kernel_dropped() {
            Some(KERNEL_DROPPED)
        } else if flags.user_dropped() {
            Some(USER_DROPPED)
        } else {
            None
        };
        if let Some(loss) = loss {
            self.invalidate(loss);
        }
        if flags.must_scan_subdirs() {
            self.must_scan_subdirs.store(true, Ordering::Release);
        }

        if !flags.history_done() {
            self.advance_delivered(event.cursor().get(), loss.is_none());
        }
        if flags.history_done() {
            self.history_cursor.store(
                self.last_delivered.load(Ordering::Acquire),
                Ordering::Release,
            );
            self.history_generation
                .store(self.ids.current_authority_generation(), Ordering::Release);
            self.history_done.store(true, Ordering::Release);
        }
    }

    fn advance_delivered(&self, cursor: u64, advances_safe: bool) {
        let mut observed = self.last_delivered.load(Ordering::Acquire);
        loop {
            if cursor < observed {
                if advances_safe {
                    self.invalidate(NON_MONOTONIC_CURSOR);
                }
                return;
            }
            if cursor == observed {
                return;
            }
            match self.last_delivered.compare_exchange_weak(
                observed,
                cursor,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    if advances_safe && self.loss().is_none() {
                        self.last_safe.store(cursor, Ordering::Release);
                    }
                    return;
                }
                Err(current) => observed = current,
            }
        }
    }

    fn invalidate(&self, reason: u8) {
        self.ids.observe_loss(self.epoch, loss_for_reason(reason));
        let _ = self
            .invalid
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (loss_priority(reason) > loss_priority(current)).then_some(reason)
            });
    }

    fn loss(&self) -> Option<WatcherCoverageLoss> {
        match self.invalid.load(Ordering::Acquire) {
            VALID => None,
            USER_DROPPED => Some(WatcherCoverageLoss::UserDropped),
            KERNEL_DROPPED => Some(WatcherCoverageLoss::KernelDropped),
            IDS_WRAPPED => Some(WatcherCoverageLoss::EventIdsWrapped),
            ROOT_CHANGED => Some(WatcherCoverageLoss::RootChanged),
            STREAM_STOPPED => Some(WatcherCoverageLoss::StreamStopped),
            NON_MONOTONIC_CURSOR => Some(WatcherCoverageLoss::NonMonotonicCursor),
            _ => Some(WatcherCoverageLoss::BackendFailure),
        }
    }

    fn validate_live(&self) -> Result<(), WatcherCoverageError> {
        if let Some(loss) = self.loss() {
            return Err(WatcherCoverageError::Loss(loss));
        }
        if !self.started.load(Ordering::Acquire) || !self.live.load(Ordering::Acquire) {
            return Err(WatcherCoverageError::CoverageUnavailable);
        }
        Ok(())
    }

    fn inspect_history(&self) -> Option<Result<FseventsCursor, WatcherCoverageError>> {
        if let Err(error) = self.validate_live() {
            if self.started.load(Ordering::Acquire) || self.loss().is_some() {
                return Some(Err(error));
            }
            return None;
        }
        self.history_done
            .load(Ordering::Acquire)
            .then(|| Ok(FseventsCursor(self.history_cursor.load(Ordering::Acquire))))
    }
}

struct DarwinStream {
    watcher: FsEventWatcher,
    state: Arc<DarwinStreamState>,
    exit_supervisor: Option<thread::JoinHandle<()>>,
}

impl Drop for DarwinStream {
    fn drop(&mut self) {
        let _ = self.stop(false);
    }
}

impl DarwinStream {
    fn start(
        root: &Path,
        epoch: WatcherStreamEpoch,
        replay_from: FseventsCursor,
        event_handler: &NativeEventHandler,
        ids: WatcherCoverageIds,
    ) -> Result<Self, WatcherCoverageError> {
        let state = DarwinStreamState::new(epoch, replay_from, ids);
        let coverage_state = Arc::clone(&state);
        let callback_state = Arc::clone(&state);
        let callback = Arc::clone(event_handler);
        let mut watcher = FsEventWatcher::new_with_coverage(
            move |event| {
                callback(epoch, event);
                callback_state.callback_returned();
            },
            move |signal| coverage_state.observe(signal),
            FsEventCursor::from_raw(replay_from.get()),
            Config::default(),
        )
        .map_err(|_| WatcherCoverageError::CoverageUnavailable)?;
        watcher
            .watch(root, RecursiveMode::Recursive)
            .map_err(|_| WatcherCoverageError::CoverageUnavailable)?;
        let worker_exit = watcher
            .take_worker_exit_receiver()
            .ok_or(WatcherCoverageError::CoverageUnavailable)?;
        let exit_state = Arc::clone(&state);
        let exit_supervisor = thread::Builder::new()
            .name("bowline fsevents exit supervisor".to_owned())
            .spawn(move || {
                if worker_exit.recv().is_ok() {
                    exit_state.observe_worker_exit();
                }
            })
            .map_err(|_| WatcherCoverageError::CoverageUnavailable)?;
        Ok(Self {
            watcher,
            state,
            exit_supervisor: Some(exit_supervisor),
        })
    }

    fn wait_for_history(
        &self,
        wait: &CoverageWait,
    ) -> Result<FseventsCursor, WatcherCoverageError> {
        wait_for_control(wait, &self.state.wake_rx, || {
            if self.watcher.worker_is_finished() {
                self.state.invalidate(STREAM_STOPPED);
                return Some(Err(WatcherCoverageError::Loss(
                    WatcherCoverageLoss::StreamStopped,
                )));
            }
            self.state.inspect_history()
        })
    }

    fn validate(&self) -> Result<(), WatcherCoverageError> {
        if self.watcher.worker_is_finished() {
            self.state.invalidate(STREAM_STOPPED);
            return Err(WatcherCoverageError::Loss(
                WatcherCoverageLoss::StreamStopped,
            ));
        }
        self.state.validate_live()
    }

    fn flush_after_scan(
        &mut self,
    ) -> Result<
        (
            FseventsCursor,
            DarwinFlushGeneration,
            WatcherCallbackGeneration,
        ),
        WatcherCoverageError,
    > {
        self.validate()?;
        self.watcher
            .flush_sync()
            .map_err(|_| WatcherCoverageError::CoverageUnavailable)?;
        self.validate()?;
        let generation = self
            .state
            .flush_generation
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_add(1)
            })
            .map_err(|_| WatcherCoverageError::IdentifierExhausted)?
            .checked_add(1)
            .ok_or(WatcherCoverageError::IdentifierExhausted)?;
        Ok((
            FseventsCursor(self.state.last_delivered.load(Ordering::Acquire)),
            DarwinFlushGeneration(
                std::num::NonZeroU64::new(generation)
                    .ok_or(WatcherCoverageError::IdentifierExhausted)?,
            ),
            WatcherCallbackGeneration(self.state.callback_generation.load(Ordering::Acquire)),
        ))
    }

    fn stop(&mut self, retiring: bool) -> Result<(), WatcherCoverageError> {
        if retiring {
            self.state.retiring.store(true, Ordering::Release);
        }
        let watcher_result = self
            .watcher
            .shutdown()
            .map_err(|_| WatcherCoverageError::CoverageUnavailable);
        let supervisor_result = self.exit_supervisor.take().map_or(Ok(()), |supervisor| {
            supervisor
                .join()
                .map_err(|_| WatcherCoverageError::CoverageUnavailable)
        });
        watcher_result.and(supervisor_result)
    }

    fn shutdown(&mut self) -> Result<(), WatcherCoverageError> {
        self.stop(false)
    }

    fn retire(&mut self) -> Result<(), WatcherCoverageError> {
        self.stop(true)
    }
}

/// FSEvents adapter that overlaps A and B only through the mechanical handoff.
pub struct NativeWatcherCoverageAdapter {
    root: PathBuf,
    event_handler: NativeEventHandler,
    ids: WatcherCoverageIds,
    active: Option<DarwinStream>,
    current_boundary: Option<DarwinCoverageBoundary>,
    shutdown: bool,
}

impl NativeWatcherCoverageAdapter {
    pub(super) fn start(
        root: &Path,
        event_handler: NativeEventHandler,
        ids: WatcherCoverageIds,
        wait: &CoverageWait,
    ) -> Result<Self, WatcherCoverageError> {
        ids.invalidate_current_boundary();
        let epoch = ids.next_epoch()?;
        let replay_from = FseventsCursor(FsEventCursor::current_safe().get());
        let mut active =
            DarwinStream::start(root, epoch, replay_from, &event_handler, ids.clone())?;
        if let Err(error) = active.wait_for_history(wait) {
            let _ = active.shutdown();
            return Err(error);
        }
        active.validate()?;
        Ok(Self {
            root: root.to_path_buf(),
            event_handler,
            ids,
            active: Some(active),
            current_boundary: None,
            shutdown: false,
        })
    }

    fn begin_recovery_with(
        &mut self,
        wait: &CoverageWait,
        after_capture: impl FnOnce(),
        after_ack: impl FnOnce(&DarwinStreamState),
    ) -> Result<WatcherCoveragePreparation, WatcherCoverageError> {
        if self.shutdown {
            return Err(WatcherCoverageError::Shutdown);
        }
        self.ids.invalidate_current_boundary();
        let (covered_epoch, covered_last_safe, active_loss) = {
            let active = self
                .active
                .as_ref()
                .ok_or(WatcherCoverageError::CoverageUnavailable)?;
            let loss = if active.watcher.worker_is_finished() {
                active.state.invalidate(STREAM_STOPPED);
                Some(WatcherCoverageLoss::StreamStopped)
            } else {
                active.state.loss()
            };
            (
                active.state.epoch,
                FseventsCursor(active.state.last_safe.load(Ordering::Acquire)),
                loss,
            )
        };
        let start = match active_loss {
            // A loss marker can be part of the journal range beginning at the
            // last safe cursor. Replaying that range can therefore replay the
            // same poison marker forever. A fresh live stream plus the
            // protocol's mandatory authoritative scan covers the unknown gap
            // without treating damaged history as authoritative.
            Some(loss) => DarwinCoverageStart::FreshStream {
                fresh_from: FseventsCursor(FsEventCursor::current_safe().get()),
                discontinuity: loss,
            },
            None => DarwinCoverageStart::CursorReplay {
                covered_last_safe,
                replay_from: covered_last_safe,
                recovery_cause: None,
            },
        };
        let stream_start = match start {
            DarwinCoverageStart::CursorReplay { replay_from, .. } => replay_from,
            DarwinCoverageStart::FreshStream { fresh_from, .. } => fresh_from,
        };
        let live_epoch = self.ids.next_epoch()?;
        after_capture();

        let mut replacement = DarwinStream::start(
            &self.root,
            live_epoch,
            stream_start,
            &self.event_handler,
            self.ids.clone(),
        )?;
        let history_through = match replacement.wait_for_history(wait) {
            Ok(cursor) => cursor,
            Err(error) => {
                let _ = replacement.retire();
                return Err(error);
            }
        };
        after_ack(&replacement.state);
        if let Err(error) = replacement.validate() {
            let _ = replacement.retire();
            return Err(error);
        }
        if replacement.state.history_generation.load(Ordering::Acquire)
            != self.ids.current_authority_generation()
        {
            let _ = replacement.retire();
            return Err(WatcherCoverageError::StaleBoundary);
        }
        let preparation = DarwinCoveragePreparation {
            covered_epoch,
            live_epoch,
            start,
            history_through,
            must_scan_subdirs: replacement.state.must_scan_subdirs.load(Ordering::Acquire),
        };
        // Promote B before the scan. HistoryDone proves the replay stream is
        // live; only the later synchronous flush can seal scan coverage.
        let mut covered = self
            .active
            .replace(replacement)
            .ok_or(WatcherCoverageError::CoverageUnavailable)?;
        let shutdown_result = covered.retire();
        shutdown_result?;
        self.current_boundary = None;
        Ok(WatcherCoveragePreparation::Darwin(preparation))
    }

    #[cfg(test)]
    pub(super) fn begin_recovery_after_capture(
        &mut self,
        wait: &CoverageWait,
        after_capture: impl FnOnce(),
    ) -> Result<WatcherCoveragePreparation, WatcherCoverageError> {
        self.begin_recovery_with(wait, after_capture, |_| {})
    }

    #[cfg(test)]
    pub(super) fn begin_recovery_after_ack(
        &mut self,
        wait: &CoverageWait,
        after_ack: impl FnOnce(&DarwinStreamState),
    ) -> Result<WatcherCoveragePreparation, WatcherCoverageError> {
        self.begin_recovery_with(wait, || {}, after_ack)
    }
}

fn loss_priority(reason: u8) -> u8 {
    match reason {
        IDS_WRAPPED => 7,
        NON_MONOTONIC_CURSOR => 6,
        STREAM_STOPPED => 5,
        ROOT_CHANGED => 4,
        KERNEL_DROPPED => 3,
        USER_DROPPED => 2,
        VALID => 0,
        _ => 1,
    }
}

fn loss_for_reason(reason: u8) -> WatcherCoverageLoss {
    match reason {
        USER_DROPPED => WatcherCoverageLoss::UserDropped,
        KERNEL_DROPPED => WatcherCoverageLoss::KernelDropped,
        IDS_WRAPPED => WatcherCoverageLoss::EventIdsWrapped,
        ROOT_CHANGED => WatcherCoverageLoss::RootChanged,
        STREAM_STOPPED => WatcherCoverageLoss::StreamStopped,
        NON_MONOTONIC_CURSOR => WatcherCoverageLoss::NonMonotonicCursor,
        _ => WatcherCoverageLoss::BackendFailure,
    }
}

impl WatcherCoverageAdapter for NativeWatcherCoverageAdapter {
    fn begin_recovery(
        &mut self,
        wait: &CoverageWait,
    ) -> Result<WatcherCoveragePreparation, WatcherCoverageError> {
        self.begin_recovery_with(wait, || {}, |_| {})
    }

    fn seal_after_scan(
        &mut self,
        preparation: WatcherCoveragePreparation,
        _wait: &CoverageWait,
    ) -> Result<WatcherCoverageHandoff, WatcherCoverageError> {
        let WatcherCoveragePreparation::Darwin(preparation) = preparation else {
            return Err(WatcherCoverageError::StaleBoundary);
        };
        let active = self
            .active
            .as_mut()
            .ok_or(WatcherCoverageError::CoverageUnavailable)?;
        if active.state.epoch != preparation.live_epoch {
            return Err(WatcherCoverageError::StaleBoundary);
        }
        let (sealed_through, flush_generation, callback_generation) = active.flush_after_scan()?;
        let boundary_id = self.ids.next_boundary()?;
        let generation = self.ids.current_authority_generation();
        let loss_generation = WatcherLossGeneration(
            std::num::NonZeroU64::new(generation)
                .ok_or(WatcherCoverageError::IdentifierExhausted)?,
        );
        let close_guard = self.ids.close_guard_at(boundary_id, generation)?;
        if !close_guard.is_current() {
            return Err(WatcherCoverageError::StaleBoundary);
        }
        let boundary = DarwinCoverageBoundary {
            boundary_id,
            covered_epoch: preparation.covered_epoch,
            live_epoch: preparation.live_epoch,
            start: preparation.start,
            history_through: preparation.history_through,
            history_done: DarwinHistoryDone,
            must_scan_subdirs: preparation.must_scan_subdirs,
            sealed_through,
            flush_generation,
            loss_generation,
            callback_generation,
        };
        self.current_boundary = Some(boundary);
        Ok(WatcherCoverageHandoff::new(
            WatcherCoverageBoundary::Darwin(boundary),
            close_guard,
        ))
    }

    fn validate_boundary(
        &self,
        handoff: &WatcherCoverageHandoff,
    ) -> Result<(), WatcherCoverageError> {
        let WatcherCoverageBoundary::Darwin(boundary) = handoff.boundary() else {
            return Err(WatcherCoverageError::StaleBoundary);
        };
        let active = self
            .active
            .as_ref()
            .ok_or(WatcherCoverageError::StaleBoundary)?;
        let expected_start = match boundary.start {
            DarwinCoverageStart::CursorReplay {
                covered_last_safe,
                replay_from,
                ..
            } if covered_last_safe == replay_from => replay_from,
            DarwinCoverageStart::FreshStream { fresh_from, .. } => fresh_from,
            DarwinCoverageStart::CursorReplay { .. } => {
                return Err(WatcherCoverageError::StaleBoundary);
            }
        };
        if !handoff.close_guard().is_current()
            || self.current_boundary != Some(boundary)
            || active.state.epoch != boundary.live_epoch
            || active.state.started_from != expected_start
            || !active.state.history_done.load(Ordering::Acquire)
            || FseventsCursor(active.state.history_cursor.load(Ordering::Acquire))
                != boundary.history_through
            || active.state.flush_generation.load(Ordering::Acquire)
                != boundary.flush_generation.get()
            || active.state.callback_generation.load(Ordering::Acquire)
                != boundary.callback_generation.get()
            || handoff.close_guard().token().generation() != boundary.loss_generation.get()
        {
            return Err(WatcherCoverageError::StaleBoundary);
        }
        active.validate()
    }

    fn shutdown(&mut self) -> Result<(), WatcherCoverageError> {
        if self.shutdown {
            return Ok(());
        }
        self.shutdown = true;
        let mut result = Ok(());
        if let Some(mut active) = self.active.take()
            && let Err(error) = active.shutdown()
            && result.is_ok()
        {
            result = Err(error);
        }
        result
    }
}

impl Drop for NativeWatcherCoverageAdapter {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

#[cfg(test)]
#[path = "darwin/tests.rs"]
mod tests;
