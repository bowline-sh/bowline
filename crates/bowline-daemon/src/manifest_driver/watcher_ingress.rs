use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, TryLockError};

use bowline_local::sync::manifest_engine::{EngineEvent, FullScanReason, WorkspacePath};
use crossbeam_channel::{Receiver, Sender, TrySendError};

const MAX_ACCUMULATED_ROOTS: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatcherIngressLoss {
    LockContended,
    CapacityExceeded,
    ExplicitFullScan,
    ActivityWatermarkExhausted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatcherIngressObservation {
    Accumulated,
    DetailCollapsed(WatcherIngressLoss),
    EngineStopped,
}

#[derive(Debug, Default)]
struct AccumulatorState {
    paths: BTreeSet<WorkspacePath>,
    recursive_paths: BTreeSet<WorkspacePath>,
}

#[derive(Debug)]
pub struct WatcherIngressSnapshot {
    paths: BTreeSet<WorkspacePath>,
    recursive_paths: BTreeSet<WorkspacePath>,
    scan_required: bool,
}

impl WatcherIngressSnapshot {
    pub fn into_events(self) -> Vec<EngineEvent> {
        let mut events = Vec::with_capacity(3);
        if !self.paths.is_empty() {
            events.push(EngineEvent::Paths(self.paths));
        }
        if !self.recursive_paths.is_empty() {
            events.push(EngineEvent::RecursivePaths(self.recursive_paths));
        }
        if self.scan_required {
            events.push(EngineEvent::FullScanRequired(
                FullScanReason::IngressDetailCollapsed,
            ));
        }
        events
    }
}

/// One bounded, level-triggered watcher handoff for a workspace.
///
/// The wake contains no data. All variable-size path detail remains in this
/// capped accumulator, and any uncertainty collapses to one authoritative scan.
#[derive(Debug)]
pub struct WatcherIngressAccumulator {
    state: Mutex<AccumulatorState>,
    detail_collapsed: AtomicBool,
    activity_watermark: AtomicU64,
    wake_pending: AtomicBool,
    connected: AtomicBool,
    wake_tx: Sender<()>,
    wake_rx: Receiver<()>,
}

impl WatcherIngressAccumulator {
    pub fn new() -> Arc<Self> {
        let (wake_tx, wake_rx) = crossbeam_channel::bounded(1);
        Arc::new(Self {
            state: Mutex::new(AccumulatorState::default()),
            detail_collapsed: AtomicBool::new(false),
            activity_watermark: AtomicU64::new(0),
            wake_pending: AtomicBool::new(false),
            connected: AtomicBool::new(true),
            wake_tx,
            wake_rx,
        })
    }

    pub fn handle(self: &Arc<Self>) -> WatcherIngressHandle {
        WatcherIngressHandle(Arc::clone(self))
    }

    pub fn wake_receiver(&self) -> Receiver<()> {
        self.wake_rx.clone()
    }

    pub fn drain(&self) -> WatcherIngressSnapshot {
        let (paths, recursive_paths) = match self.state.lock() {
            Ok(mut state) => (
                std::mem::take(&mut state.paths),
                std::mem::take(&mut state.recursive_paths),
            ),
            Err(_) => {
                self.detail_collapsed.store(true, Ordering::Release);
                (BTreeSet::new(), BTreeSet::new())
            }
        };
        let scan_required = self.detail_collapsed.swap(false, Ordering::AcqRel);
        while self.wake_rx.try_recv().is_ok() {}
        self.wake_pending.store(false, Ordering::Release);
        self.rearm_if_needed();
        WatcherIngressSnapshot {
            paths,
            recursive_paths,
            scan_required,
        }
    }

    #[doc(hidden)]
    pub fn disconnect(&self) {
        self.connected.store(false, Ordering::Release);
        self.arm_wake();
    }

    fn observe(&self, event: EngineEvent) -> WatcherIngressObservation {
        if !self.connected.load(Ordering::Acquire) {
            return WatcherIngressObservation::EngineStopped;
        }
        if self
            .activity_watermark
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(1)
            })
            .is_err()
        {
            self.detail_collapsed.store(true, Ordering::Release);
            self.arm_wake();
            return WatcherIngressObservation::DetailCollapsed(
                WatcherIngressLoss::ActivityWatermarkExhausted,
            );
        }
        let outcome = match self.state.try_lock() {
            Ok(mut state) => self.merge_event(&mut state, event),
            Err(TryLockError::WouldBlock | TryLockError::Poisoned(_)) => {
                self.detail_collapsed.store(true, Ordering::Release);
                WatcherIngressObservation::DetailCollapsed(WatcherIngressLoss::LockContended)
            }
        };
        self.arm_wake();
        outcome
    }

    fn merge_event(
        &self,
        state: &mut AccumulatorState,
        event: EngineEvent,
    ) -> WatcherIngressObservation {
        match event {
            EngineEvent::Paths(paths) => {
                if state
                    .paths
                    .len()
                    .saturating_add(state.recursive_paths.len())
                    .saturating_add(paths.len())
                    > MAX_ACCUMULATED_ROOTS
                {
                    self.retain_latest_paths(state, paths, false);
                    return WatcherIngressObservation::DetailCollapsed(
                        WatcherIngressLoss::CapacityExceeded,
                    );
                }
                state.paths.extend(paths);
            }
            EngineEvent::RecursivePaths(paths) => {
                if state
                    .paths
                    .len()
                    .saturating_add(state.recursive_paths.len())
                    .saturating_add(paths.len())
                    > MAX_ACCUMULATED_ROOTS
                {
                    self.retain_latest_paths(state, paths, true);
                    return WatcherIngressObservation::DetailCollapsed(
                        WatcherIngressLoss::CapacityExceeded,
                    );
                }
                state.recursive_paths.extend(paths);
            }
            EngineEvent::FullScanRequired(_) => {
                self.detail_collapsed.store(true, Ordering::Release);
                return WatcherIngressObservation::DetailCollapsed(
                    WatcherIngressLoss::ExplicitFullScan,
                );
            }
            _ => {
                self.detail_collapsed.store(true, Ordering::Release);
                return WatcherIngressObservation::DetailCollapsed(
                    WatcherIngressLoss::ExplicitFullScan,
                );
            }
        }
        WatcherIngressObservation::Accumulated
    }

    fn retain_latest_paths(
        &self,
        state: &mut AccumulatorState,
        paths: BTreeSet<WorkspacePath>,
        recursive: bool,
    ) {
        state.paths.clear();
        state.recursive_paths.clear();
        if paths.len() <= MAX_ACCUMULATED_ROOTS {
            if recursive {
                state.recursive_paths = paths;
            } else {
                state.paths = paths;
            }
        }
        // The retained paths are only the newest exact observation. Everything
        // discarded before them is still recovered by the covering scan.
        self.detail_collapsed.store(true, Ordering::Release);
    }

    fn arm_wake(&self) {
        if self.wake_pending.swap(true, Ordering::AcqRel) {
            return;
        }
        match self.wake_tx.try_send(()) {
            Ok(()) | Err(TrySendError::Full(())) => {}
            Err(TrySendError::Disconnected(())) => {
                self.connected.store(false, Ordering::Release);
            }
        }
    }

    fn rearm_if_needed(&self) {
        let has_paths = self
            .state
            .try_lock()
            .map(|state| !state.paths.is_empty() || !state.recursive_paths.is_empty())
            .unwrap_or(true);
        if has_paths || self.detail_collapsed.load(Ordering::Acquire) {
            self.arm_wake();
        }
    }
}

#[derive(Clone, Debug)]
pub struct WatcherIngressHandle(Arc<WatcherIngressAccumulator>);

impl WatcherIngressHandle {
    pub fn observe(&self, event: EngineEvent) -> WatcherIngressObservation {
        self.0.observe(event)
    }

    #[cfg(test)]
    pub fn drain_for_test(&self) -> WatcherIngressSnapshot {
        self.0.drain()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(value: &str) -> WorkspacePath {
        WorkspacePath::new(value)
    }

    #[test]
    fn one_unit_wake_carries_a_bounded_deterministic_snapshot() {
        let accumulator = WatcherIngressAccumulator::new();
        let handle = accumulator.handle();
        assert_eq!(
            handle.observe(EngineEvent::Paths(BTreeSet::from([path("b"), path("a")]))),
            WatcherIngressObservation::Accumulated
        );
        assert_eq!(
            handle.observe(EngineEvent::RecursivePaths(BTreeSet::from([path("dir")]))),
            WatcherIngressObservation::Accumulated
        );
        assert!(accumulator.wake_receiver().try_recv().is_ok());
        let snapshot = accumulator.drain();
        assert_eq!(snapshot.paths, BTreeSet::from([path("a"), path("b")]));
        assert_eq!(snapshot.recursive_paths, BTreeSet::from([path("dir")]));
        assert!(!snapshot.scan_required);
    }

    #[test]
    fn capacity_collapse_preserves_the_newest_edit_and_requires_one_scan() {
        let accumulator = WatcherIngressAccumulator::new();
        let handle = accumulator.handle();
        for index in 0..MAX_ACCUMULATED_ROOTS {
            assert_eq!(
                handle.observe(EngineEvent::Paths(BTreeSet::from([path(&format!(
                    "root-{index:03}"
                ))]))),
                WatcherIngressObservation::Accumulated
            );
        }
        assert_eq!(
            handle.observe(EngineEvent::Paths(BTreeSet::from([path("overflow")]))),
            WatcherIngressObservation::DetailCollapsed(WatcherIngressLoss::CapacityExceeded)
        );
        assert_eq!(
            accumulator.drain().into_events(),
            vec![
                EngineEvent::Paths(BTreeSet::from([path("overflow")])),
                EngineEvent::FullScanRequired(FullScanReason::IngressDetailCollapsed),
            ]
        );
    }

    #[test]
    fn exact_detail_after_a_collapse_survives_the_same_drain() {
        let accumulator = WatcherIngressAccumulator::new();
        let handle = accumulator.handle();
        assert_eq!(
            handle.observe(EngineEvent::FullScanRequired(
                FullScanReason::WatcherOverflow
            )),
            WatcherIngressObservation::DetailCollapsed(WatcherIngressLoss::ExplicitFullScan)
        );
        assert_eq!(
            handle.observe(EngineEvent::Paths(BTreeSet::from([path("sentinel")]))),
            WatcherIngressObservation::Accumulated
        );
        assert_eq!(
            accumulator.drain().into_events(),
            vec![
                EngineEvent::Paths(BTreeSet::from([path("sentinel")])),
                EngineEvent::FullScanRequired(FullScanReason::IngressDetailCollapsed),
            ]
        );
    }

    #[test]
    fn lock_contention_collapses_detail_without_blocking_the_callback() {
        let accumulator = WatcherIngressAccumulator::new();
        let handle = accumulator.handle();
        let state_guard = accumulator
            .state
            .lock()
            .expect("test owns accumulator lock");

        assert_eq!(
            handle.observe(EngineEvent::Paths(BTreeSet::from([path("raced")]))),
            WatcherIngressObservation::DetailCollapsed(WatcherIngressLoss::LockContended)
        );
        drop(state_guard);

        assert_eq!(
            accumulator.drain().into_events(),
            vec![EngineEvent::FullScanRequired(
                FullScanReason::IngressDetailCollapsed
            )]
        );
    }

    #[test]
    fn callback_after_wake_disarm_arms_another_unit_wake() {
        let accumulator = WatcherIngressAccumulator::new();
        let handle = accumulator.handle();
        assert_eq!(
            handle.observe(EngineEvent::Paths(BTreeSet::from([path("first")]))),
            WatcherIngressObservation::Accumulated
        );
        assert_eq!(
            accumulator.drain().into_events(),
            vec![EngineEvent::Paths(BTreeSet::from([path("first")]))]
        );

        assert_eq!(
            handle.observe(EngineEvent::Paths(BTreeSet::from([path("second")]))),
            WatcherIngressObservation::Accumulated
        );
        assert!(
            accumulator.wake_receiver().try_recv().is_ok(),
            "a callback after wake disarm must arm another unit wake"
        );
        assert_eq!(
            accumulator.drain().into_events(),
            vec![EngineEvent::Paths(BTreeSet::from([path("second")]))]
        );
    }
}
