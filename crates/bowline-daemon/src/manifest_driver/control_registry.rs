use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bowline_local::sync::manifest_engine::{
    AuthoritativeScanError, AuthoritativeScanReceipt, EngineConvergenceBarrierId,
    EngineEndpointGeneration, EngineEvent,
};
use crossbeam_channel::{Receiver, Sender, TrySendError};

use super::EngineConvergenceBarrierError;

const MAX_PUBLIC_BARRIERS: usize = 64;

#[derive(Debug)]
struct ControlState {
    connected: bool,
    barriers: VecDeque<(EngineConvergenceBarrierId, EngineEndpointGeneration)>,
    next_scan_id: u64,
    recovery: Option<RecoverySlot>,
}

/// Runs on the engine thread when a scan is dispatched, immediately before the
/// authoritative walk observes the filesystem.
///
/// The recovery fence has to start where coverage starts. Recording it at scan
/// *request* time put the engine's whole publication pass inside the fenced
/// window, so a write dropped while blobs were uploading doomed an attempt whose
/// own walk had not begun and would have seen that write on disk. A storm
/// therefore cost a doomed cycle every time.
pub struct WalkStartHook(Box<dyn FnOnce() + Send>);

impl WalkStartHook {
    pub fn new(hook: impl FnOnce() + Send + 'static) -> Self {
        Self(Box::new(hook))
    }

    fn run(self) {
        (self.0)();
    }
}

impl std::fmt::Debug for WalkStartHook {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("WalkStartHook")
    }
}

#[derive(Debug)]
enum RecoverySlot {
    Requested {
        id: CoverageScanLeaseId,
        completion: std::sync::mpsc::Sender<Result<AuthoritativeScanReceipt, CoverageScanFailure>>,
        on_walk_start: Option<WalkStartHook>,
    },
    Running {
        id: CoverageScanLeaseId,
        completion: std::sync::mpsc::Sender<Result<AuthoritativeScanReceipt, CoverageScanFailure>>,
    },
    Paused {
        id: CoverageScanLeaseId,
    },
    // A retry that must let the engine publish before it scans again. The pause
    // spans one attempt's scan, seal and close offer; holding it across attempts
    // as well meant a close that kept being invalidated by continuing activity
    // withheld publication for the whole chase -- measured at 66s over 9
    // attempts, against a 30s budget for an edit to reach a peer. Yielding
    // `Release` and re-arming as `Requested` gives the engine exactly one work
    // pass between attempts.
    ReleaseThenScan {
        id: CoverageScanLeaseId,
        completion: std::sync::mpsc::Sender<Result<AuthoritativeScanReceipt, CoverageScanFailure>>,
        on_walk_start: Option<WalkStartHook>,
    },
    ReleaseRequested {
        id: CoverageScanLeaseId,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CoverageScanLeaseId(u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoverageScanFailure {
    CycleActive,
    RootUnavailable,
    Fatal,
}

impl From<AuthoritativeScanError> for CoverageScanFailure {
    fn from(error: AuthoritativeScanError) -> Self {
        match error {
            AuthoritativeScanError::CycleActive => Self::CycleActive,
            AuthoritativeScanError::RootUnavailable(_) => Self::RootUnavailable,
            AuthoritativeScanError::Fatal(_) => Self::Fatal,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoverageScanError {
    ResourceExhausted,
    EngineStopped,
    TimedOut,
    Cancelled,
    Scan(CoverageScanFailure),
}

pub(super) enum RecoveryControlAction {
    Scan(CoverageScanLeaseId),
    Release,
}

pub struct CoverageScanWaiter {
    id: CoverageScanLeaseId,
    receiver: std::sync::mpsc::Receiver<Result<AuthoritativeScanReceipt, CoverageScanFailure>>,
    registry: Arc<WorkspaceControlRegistry>,
    active: bool,
}

impl CoverageScanWaiter {
    pub fn wait(
        mut self,
        timeout: Duration,
        mut cancelled: impl FnMut() -> bool,
    ) -> Result<CoverageScanLease, CoverageScanError> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(CoverageScanError::TimedOut)?;
        loop {
            if cancelled() {
                return Err(CoverageScanError::Cancelled);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(CoverageScanError::TimedOut);
            }
            match self
                .receiver
                .recv_timeout(remaining.min(Duration::from_millis(100)))
            {
                Ok(Ok(receipt)) => {
                    self.active = false;
                    return Ok(CoverageScanLease {
                        id: self.id,
                        receipt,
                        registry: Arc::clone(&self.registry),
                        released: false,
                    });
                }
                Ok(Err(failure)) => {
                    self.active = false;
                    return Err(CoverageScanError::Scan(failure));
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    self.active = false;
                    return Err(CoverageScanError::EngineStopped);
                }
            }
        }
    }
}

impl Drop for CoverageScanWaiter {
    fn drop(&mut self) {
        if self.active {
            self.registry.release_scan(self.id);
        }
    }
}

pub struct CoverageScanLease {
    id: CoverageScanLeaseId,
    receipt: AuthoritativeScanReceipt,
    registry: Arc<WorkspaceControlRegistry>,
    released: bool,
}

impl CoverageScanLease {
    pub const fn receipt(&self) -> AuthoritativeScanReceipt {
        self.receipt
    }

    pub fn release(mut self) {
        self.registry.release_scan(self.id);
        self.released = true;
    }

    /// Let the engine publish once, then request another authoritative scan.
    ///
    /// The pause spans one attempt so its scan, seal and close offer stay
    /// atomic. It is not held across attempts: a close that keeps being
    /// invalidated by continuing activity would otherwise withhold publication
    /// for the whole chase, which is what kept a post-burst edit off its peer
    /// for 66s against a 30s budget.
    pub fn release_and_request_rescan(
        mut self,
        on_walk_start: Option<WalkStartHook>,
    ) -> Result<CoverageScanWaiter, CoverageScanError> {
        let waiter = self.registry.request_rescan(self.id, on_walk_start)?;
        self.released = true;
        Ok(waiter)
    }
}

impl Drop for CoverageScanLease {
    fn drop(&mut self) {
        if !self.released {
            self.registry.release_scan(self.id);
        }
    }
}

/// Independent, level-triggered control admission for one engine endpoint.
///
/// Watcher data never enters this registry, so a full data inbox cannot delay
/// registration or consume the public-barrier capacity.
#[derive(Debug)]
pub(super) struct WorkspaceControlRegistry {
    state: Mutex<ControlState>,
    wake_tx: Sender<()>,
    wake_rx: Receiver<()>,
}

impl WorkspaceControlRegistry {
    pub(super) fn new() -> Self {
        let (wake_tx, wake_rx) = crossbeam_channel::bounded(1);
        Self {
            state: Mutex::new(ControlState {
                connected: true,
                barriers: VecDeque::new(),
                next_scan_id: 1,
                recovery: None,
            }),
            wake_tx,
            wake_rx,
        }
    }

    pub(super) fn wake_receiver(&self) -> Receiver<()> {
        self.wake_rx.clone()
    }

    pub(super) fn register_barrier(
        &self,
        id: EngineConvergenceBarrierId,
        generation: EngineEndpointGeneration,
    ) -> Result<(), EngineConvergenceBarrierError> {
        let mut state =
            self.state
                .lock()
                .map_err(|_| EngineConvergenceBarrierError::Unavailable {
                    reason: "workspace control registry is unavailable",
                })?;
        if !state.connected {
            return Err(EngineConvergenceBarrierError::EngineStopped);
        }
        if state.barriers.len() >= MAX_PUBLIC_BARRIERS {
            return Err(EngineConvergenceBarrierError::ResourceExhausted);
        }
        state.barriers.push_back((id, generation));
        drop(state);
        self.arm_wake();
        Ok(())
    }

    pub(super) fn cancel_barrier(
        &self,
        id: EngineConvergenceBarrierId,
        generation: EngineEndpointGeneration,
    ) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state
            .barriers
            .retain(|pending| *pending != (id, generation));
    }

    pub(super) fn drain_barriers(&self) -> Vec<EngineEvent> {
        let Ok(mut state) = self.state.lock() else {
            return Vec::new();
        };
        let events = state
            .barriers
            .drain(..)
            .map(
                |(id, endpoint_generation)| EngineEvent::EngineConvergenceBarrier {
                    id,
                    endpoint_generation,
                },
            )
            .collect();
        while self.wake_rx.try_recv().is_ok() {}
        let recovery_action_pending = matches!(
            state.recovery,
            Some(
                RecoverySlot::Requested { .. }
                    | RecoverySlot::ReleaseThenScan { .. }
                    | RecoverySlot::ReleaseRequested { .. }
            )
        );
        drop(state);
        // The engine checks recovery before draining this shared wake. A
        // recovery request can land between those operations, so preserve the
        // level-triggered signal whenever actionable state survived the drain.
        // Requests arriving after the lock is released arm their own wake.
        if recovery_action_pending {
            self.arm_wake();
        }
        events
    }

    pub(super) fn request_scan(
        self: &Arc<Self>,
        on_walk_start: Option<WalkStartHook>,
    ) -> Result<CoverageScanWaiter, CoverageScanError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| CoverageScanError::EngineStopped)?;
        if !state.connected {
            return Err(CoverageScanError::EngineStopped);
        }
        // Admission is refused on behalf of a live consumer only. A slot left
        // `ReleaseRequested` has none by construction: `release_scan` is the sole
        // abandonment sink and every waiter and lease routes its `Drop` through
        // it. Refusing that residue stalled recovery -- a scan wait that timed
        // out while the engine was mid-cycle poisoned the slot the engine could
        // not yet drain, and every retry then failed `dependency_busy` and backed
        // off, so the covering scan the workspace was waiting on kept receding.
        // Reclaim as `ReleaseThenScan` so the abandoned unpause is still
        // delivered before the new scan runs.
        match state.recovery {
            None => {}
            Some(RecoverySlot::ReleaseRequested { .. }) => {}
            Some(_) => return Err(CoverageScanError::ResourceExhausted),
        }
        let reclaimed = matches!(state.recovery, Some(RecoverySlot::ReleaseRequested { .. }));
        let id = CoverageScanLeaseId(state.next_scan_id);
        state.next_scan_id = state
            .next_scan_id
            .checked_add(1)
            .ok_or(CoverageScanError::ResourceExhausted)?;
        let (completion, receiver) = std::sync::mpsc::channel();
        state.recovery = Some(if reclaimed {
            RecoverySlot::ReleaseThenScan {
                id,
                completion,
                on_walk_start,
            }
        } else {
            RecoverySlot::Requested {
                id,
                completion,
                on_walk_start,
            }
        });
        drop(state);
        self.arm_wake();
        Ok(CoverageScanWaiter {
            id,
            receiver,
            registry: Arc::clone(self),
            active: true,
        })
    }

    fn request_rescan(
        self: &Arc<Self>,
        id: CoverageScanLeaseId,
        on_walk_start: Option<WalkStartHook>,
    ) -> Result<CoverageScanWaiter, CoverageScanError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| CoverageScanError::EngineStopped)?;
        if !state.connected {
            return Err(CoverageScanError::EngineStopped);
        }
        if !matches!(state.recovery, Some(RecoverySlot::Paused { id: current }) if current == id) {
            return Err(CoverageScanError::ResourceExhausted);
        }
        let (completion, receiver) = std::sync::mpsc::channel();
        state.recovery = Some(RecoverySlot::ReleaseThenScan {
            id,
            completion,
            on_walk_start,
        });
        drop(state);
        self.arm_wake();
        Ok(CoverageScanWaiter {
            id,
            receiver,
            registry: Arc::clone(self),
            active: true,
        })
    }

    pub(super) fn take_recovery_action(&self) -> Option<RecoveryControlAction> {
        let mut state = self.state.lock().ok()?;
        match state.recovery.take()? {
            RecoverySlot::Requested {
                id,
                completion,
                on_walk_start,
            } => {
                state.recovery = Some(RecoverySlot::Running { id, completion });
                drop(state);
                // Close the fence here, on the engine thread, immediately before
                // the walk observes the filesystem -- not when the scan was
                // requested, which left the publication pass inside the window.
                if let Some(hook) = on_walk_start {
                    hook.run();
                }
                Some(RecoveryControlAction::Scan(id))
            }
            RecoverySlot::ReleaseThenScan {
                id,
                completion,
                on_walk_start,
            } => {
                // The engine loop skips its blocking select after a release, so
                // the work pass it runs next is guaranteed to happen before the
                // following iteration takes this Scan.
                state.recovery = Some(RecoverySlot::Requested {
                    id,
                    completion,
                    on_walk_start,
                });
                Some(RecoveryControlAction::Release)
            }
            RecoverySlot::ReleaseRequested { .. } => Some(RecoveryControlAction::Release),
            slot @ (RecoverySlot::Running { .. } | RecoverySlot::Paused { .. }) => {
                state.recovery = Some(slot);
                None
            }
        }
    }

    pub(super) fn complete_scan(
        &self,
        id: CoverageScanLeaseId,
        result: Result<AuthoritativeScanReceipt, AuthoritativeScanError>,
    ) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        // A waiter that times out mid-scan drops to `ReleaseRequested` while this
        // scan is still executing. Preserve that residue so the abandoned lease
        // is retired before a later consumer reuses the single scan slot. Put
        // back anything that is not this scan's `Running`.
        let taken = state.recovery.take();
        let Some(RecoverySlot::Running {
            id: running_id,
            completion,
        }) = taken
        else {
            state.recovery = taken;
            return;
        };
        if running_id != id {
            state.recovery = Some(RecoverySlot::Running {
                id: running_id,
                completion,
            });
            return;
        }
        match result {
            Ok(receipt) => {
                state.recovery = Some(RecoverySlot::Paused { id });
                let _waiter_gone = completion.send(Ok(receipt));
            }
            Err(error) => {
                let _waiter_gone = completion.send(Err(error.into()));
            }
        }
    }

    fn release_scan(&self, id: CoverageScanLeaseId) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let matches = match state.recovery.as_ref() {
            Some(
                RecoverySlot::Requested { id: current, .. }
                | RecoverySlot::Running { id: current, .. }
                | RecoverySlot::Paused { id: current }
                | RecoverySlot::ReleaseThenScan { id: current, .. }
                | RecoverySlot::ReleaseRequested { id: current },
            ) => *current == id,
            None => false,
        };
        if matches {
            state.recovery = Some(RecoverySlot::ReleaseRequested { id });
            drop(state);
            self.arm_wake();
        }
    }

    pub(super) fn disconnect(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.connected = false;
            state.barriers.clear();
            if let Some(
                RecoverySlot::Requested { completion, .. }
                | RecoverySlot::Running { completion, .. },
            ) = state.recovery.take()
            {
                let _waiter_gone = completion.send(Err(CoverageScanFailure::Fatal));
            }
        }
        self.arm_wake();
    }

    fn arm_wake(&self) {
        match self.wake_tx.try_send(()) {
            Ok(()) | Err(TrySendError::Full(())) | Err(TrySendError::Disconnected(())) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A scan wait that timed out while the engine was mid-cycle left the slot
    // `ReleaseRequested` with no live consumer, and the engine could not drain it
    // until it reached a loop top. Refusing that residue failed every retry with
    // `dependency_busy` and backed recovery off, so the covering scan the
    // workspace was waiting on kept receding while the overflow latch withheld
    // its writes.
    // The recovery fence has to begin where coverage begins. Recorded when the
    // scan was requested, it covered the engine's entire publication pass, so a
    // write dropped while blobs uploaded doomed an attempt whose walk had not
    // started and would have seen that write on disk -- a guaranteed wasted cycle
    // per burst. The hook must therefore run at dispatch, not at request.
    #[test]
    fn the_walk_start_hook_runs_at_dispatch_not_at_request() {
        let registry = Arc::new(WorkspaceControlRegistry::new());
        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hook_ran = Arc::clone(&ran);
        let waiter = registry
            .request_scan(Some(WalkStartHook::new(move || {
                hook_ran.store(true, std::sync::atomic::Ordering::Relaxed);
            })))
            .expect("scan is admitted");

        assert!(
            !ran.load(std::sync::atomic::Ordering::Relaxed),
            "requesting a scan must not close the fence; the publication pass still has to run"
        );

        let action = registry.take_recovery_action();
        assert!(
            matches!(action, Some(RecoveryControlAction::Scan(_))),
            "dispatch yields the scan"
        );
        assert!(
            ran.load(std::sync::atomic::Ordering::Relaxed),
            "the fence must close as the walk is dispatched, before it observes anything"
        );
        drop(waiter);
    }

    // A release still has to reach the engine before the scan it precedes, and
    // the hook must survive that hop rather than firing on the release.
    #[test]
    fn a_reclaimed_slot_defers_its_hook_until_the_scan_is_dispatched() {
        let registry = Arc::new(WorkspaceControlRegistry::new());
        drop(registry.request_scan(None).expect("first scan is admitted"));
        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hook_ran = Arc::clone(&ran);
        let waiter = registry
            .request_scan(Some(WalkStartHook::new(move || {
                hook_ran.store(true, std::sync::atomic::Ordering::Relaxed);
            })))
            .expect("the abandoned slot is reclaimed");

        assert!(matches!(
            registry.take_recovery_action(),
            Some(RecoveryControlAction::Release)
        ));
        assert!(
            !ran.load(std::sync::atomic::Ordering::Relaxed),
            "the release is not the walk; firing here would re-cover the publication pass"
        );
        assert!(matches!(
            registry.take_recovery_action(),
            Some(RecoveryControlAction::Scan(_))
        ));
        assert!(
            ran.load(std::sync::atomic::Ordering::Relaxed),
            "the fence closes when the walk is dispatched"
        );
        drop(waiter);
    }

    #[test]
    fn an_abandoned_scan_slot_is_reclaimed_rather_than_refused() {
        let registry = Arc::new(WorkspaceControlRegistry::new());
        let waiter = registry.request_scan(None).expect("first scan is admitted");
        drop(waiter);

        let reclaimed = registry
            .request_scan(None)
            .expect("an abandoned slot has no live consumer, so it must be reclaimable");

        // The abandoned release still has to reach the engine: it is what
        // unpauses it. Reclaiming straight to `Requested` would strand the pause.
        assert!(
            matches!(
                registry.take_recovery_action(),
                Some(RecoveryControlAction::Release)
            ),
            "the reclaimed slot must deliver the pending release first"
        );
        assert!(
            matches!(
                registry.take_recovery_action(),
                Some(RecoveryControlAction::Scan(_))
            ),
            "and then run the scan it was reclaimed for"
        );
        drop(reclaimed);
    }

    // Reclaiming must stay narrow: a slot with a live waiter or lease is still
    // occupied, and admitting a second consumer would let two scans believe they
    // own the same engine pause.
    #[test]
    fn a_live_scan_slot_is_still_refused() {
        let registry = Arc::new(WorkspaceControlRegistry::new());
        let held = registry.request_scan(None).expect("first scan is admitted");
        assert_eq!(
            registry.request_scan(None).err(),
            Some(CoverageScanError::ResourceExhausted),
            "a live consumer must still refuse admission"
        );
        drop(held);
    }

    // A waiter that times out mid-scan drops to `ReleaseRequested` while the scan
    // is still running. Discarding that residue on completion left nothing to
    // unpause the engine, so publication stopped until some later incident
    // happened to close.
    #[test]
    fn a_scan_completing_into_an_abandoned_slot_still_releases_the_engine() {
        let registry = Arc::new(WorkspaceControlRegistry::new());
        let waiter = registry.request_scan(None).expect("scan is admitted");
        let Some(RecoveryControlAction::Scan(id)) = registry.take_recovery_action() else {
            panic!("the admitted scan is dispatched");
        };
        drop(waiter);

        // The completion result is immaterial: the residue is restored before
        // the outcome is even inspected.
        registry.complete_scan(id, Err(AuthoritativeScanError::CycleActive));

        assert!(
            matches!(
                registry.take_recovery_action(),
                Some(RecoveryControlAction::Release)
            ),
            "the abandoned release must survive completion, or the engine stays paused"
        );
    }

    // The engine checks recovery controls before it drains the shared control
    // wake. A lease can be released in that gap, so draining must preserve a
    // level-triggered wake for the still-pending release; otherwise the final
    // recovery close has no successor scan to wake publication back up.
    #[test]
    fn a_release_queued_between_control_check_and_wake_drain_rearms_the_engine() {
        let registry = Arc::new(WorkspaceControlRegistry::new());
        let id = CoverageScanLeaseId(7);
        registry
            .state
            .lock()
            .expect("control state is available")
            .recovery = Some(RecoverySlot::Paused { id });

        assert!(
            registry.take_recovery_action().is_none(),
            "the engine sees no action before the lease is released"
        );
        registry.release_scan(id);
        assert!(registry.drain_barriers().is_empty());

        assert!(
            registry.wake_receiver().try_recv().is_ok(),
            "draining the edge must rearm a wake while the release remains pending"
        );
        assert!(matches!(
            registry.take_recovery_action(),
            Some(RecoveryControlAction::Release)
        ));
    }

    #[test]
    fn control_registration_is_bounded_and_independent() {
        let registry = Arc::new(WorkspaceControlRegistry::new());
        for value in 1..=MAX_PUBLIC_BARRIERS {
            registry
                .register_barrier(
                    EngineConvergenceBarrierId(value as u64),
                    EngineEndpointGeneration(7),
                )
                .expect("the bounded public registry has room");
        }
        assert_eq!(
            registry.register_barrier(EngineConvergenceBarrierId(65), EngineEndpointGeneration(7)),
            Err(EngineConvergenceBarrierError::ResourceExhausted)
        );
        let recovery = registry
            .request_scan(None)
            .expect("recovery retains its dedicated slot at the public limit");
        drop(recovery);
        assert_eq!(registry.drain_barriers().len(), MAX_PUBLIC_BARRIERS);
    }
}
