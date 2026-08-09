use super::*;

use std::time::Duration;

#[derive(Clone, Copy)]
enum FakeFailure {
    Timeout,
    Backend,
}

struct FaultInjectableAdapter {
    failure: FakeFailure,
    recovery_open: bool,
}

impl FaultInjectableAdapter {
    fn new(failure: FakeFailure) -> Self {
        Self {
            failure,
            recovery_open: true,
        }
    }
}

impl WatcherCoverageAdapter for FaultInjectableAdapter {
    fn begin_recovery(
        &mut self,
        _wait: &CoverageWait,
    ) -> Result<WatcherCoveragePreparation, WatcherCoverageError> {
        match self.failure {
            FakeFailure::Timeout => Err(WatcherCoverageError::TimedOut),
            FakeFailure::Backend => Err(WatcherCoverageError::Loss(
                WatcherCoverageLoss::BackendFailure,
            )),
        }
    }

    fn seal_after_scan(
        &mut self,
        _preparation: WatcherCoveragePreparation,
        _wait: &CoverageWait,
    ) -> Result<WatcherCoverageHandoff, WatcherCoverageError> {
        unreachable!("the fake never prepares a live stream")
    }

    fn validate_boundary(
        &self,
        _handoff: &WatcherCoverageHandoff,
    ) -> Result<(), WatcherCoverageError> {
        unreachable!("the fake never produces a provisional boundary")
    }

    fn shutdown(&mut self) -> Result<(), WatcherCoverageError> {
        Ok(())
    }
}

fn attempt_fake_coverage(adapter: &mut FaultInjectableAdapter, wait: &CoverageWait) {
    if let Ok(preparation) = adapter.begin_recovery(wait)
        && let Ok(boundary) = adapter.seal_after_scan(preparation, wait)
        && adapter.validate_boundary(&boundary).is_ok()
    {
        adapter.recovery_open = false;
    }
}

fn future_wait() -> CoverageWait {
    CoverageWait::new(
        Instant::now() + Duration::from_secs(5),
        CoverageCancellation::new(),
    )
}

#[test]
fn fake_timeout_and_error_keep_recovery_open() {
    for failure in [FakeFailure::Timeout, FakeFailure::Backend] {
        let mut adapter = FaultInjectableAdapter::new(failure);
        attempt_fake_coverage(&mut adapter, &future_wait());
        assert!(adapter.recovery_open);
    }
}

#[test]
fn deterministic_test_boundary_preserves_native_identities() {
    let handoff = test_linux_handoff(2, 3, 1);
    assert_eq!(handoff.boundary_id().get(), 2);
    assert_eq!(handoff.live_stream_epoch().get(), 3);
    assert!(handoff.close_guard().is_current());
    invalidate_test_handoff(&handoff);
    assert!(!handoff.close_guard().is_current());
}

#[test]
fn timeout_is_only_a_fail_closed_exit() {
    let (_wake_tx, wake_rx) = crossbeam_channel::bounded::<()>(1);
    let wait = CoverageWait::new(Instant::now(), CoverageCancellation::new());
    let result = wait_for_control(&wait, &wake_rx, || None::<Result<(), WatcherCoverageError>>);
    assert_eq!(result, Err(WatcherCoverageError::TimedOut));
}

#[test]
fn acknowledgement_at_or_after_the_deadline_cannot_authorize_coverage() {
    let (wake_tx, wake_rx) = crossbeam_channel::bounded::<()>(1);
    wake_tx.send(()).expect("late acknowledgement is queued");
    let wait = CoverageWait::new(Instant::now(), CoverageCancellation::new());
    let inspected = std::cell::Cell::new(false);
    let result = wait_for_control(&wait, &wake_rx, || {
        inspected.set(true);
        Some(Ok(()))
    });
    assert_eq!(result, Err(WatcherCoverageError::TimedOut));
    assert!(
        !inspected.get(),
        "deadline expiry must be observed before a queued acknowledgement"
    );
}

#[test]
fn cancellation_wakes_and_joins_a_blocked_control_wait() {
    let cancellation = CoverageCancellation::new();
    let wait = CoverageWait::new(
        Instant::now() + Duration::from_secs(5),
        cancellation.clone(),
    );
    let (_wake_tx, wake_rx) = crossbeam_channel::bounded::<()>(1);
    let worker = std::thread::spawn(move || {
        wait_for_control(&wait, &wake_rx, || None::<Result<(), WatcherCoverageError>>)
    });

    cancellation.cancel();
    assert_eq!(
        worker.join().expect("control wait worker joins"),
        Err(WatcherCoverageError::Cancelled)
    );
}

#[test]
fn outward_native_loss_lane_is_bounded_nonblocking_and_invalidates_first() {
    let ids = WatcherCoverageIds::new();
    let observations = ids.observation_receiver();
    let boundary_id = ids.next_boundary().expect("test boundary id");
    let guard = ids.close_guard(boundary_id).expect("test close guard");
    let stream_epoch = ids.next_epoch().expect("test stream epoch");

    ids.observe_loss(stream_epoch, WatcherCoverageLoss::QueueOverflow);
    ids.observe_loss(stream_epoch, WatcherCoverageLoss::BackendFailure);

    assert!(!guard.is_current());
    assert_eq!(
        observations
            .try_recv()
            .expect("the first typed loss remains pending"),
        NativeCoverageObservation {
            stream_epoch,
            loss: WatcherCoverageLoss::QueueOverflow,
        }
    );
    assert!(matches!(
        observations.try_recv(),
        Err(crossbeam_channel::TryRecvError::Empty)
    ));
}

#[test]
fn sibling_recovery_tests_can_construct_typed_native_observations() {
    let observation = test_native_coverage_observation(7, WatcherCoverageLoss::QueueOverflow);

    assert_eq!(observation.stream_epoch().get(), 7);
    assert_eq!(observation.loss(), WatcherCoverageLoss::QueueOverflow);
}

#[test]
fn native_identity_allocators_fail_closed_at_exhaustion() {
    let ids = WatcherCoverageIds::new();
    ids.next_epoch.store(u64::MAX, Ordering::Release);
    ids.next_boundary.store(u64::MAX, Ordering::Release);
    assert_eq!(
        ids.next_epoch(),
        Err(WatcherCoverageError::IdentifierExhausted)
    );
    assert_eq!(
        ids.next_boundary(),
        Err(WatcherCoverageError::IdentifierExhausted)
    );

    ids.authority.generation.store(u64::MAX, Ordering::Release);
    let boundary_id = WatcherBoundaryId(NonZeroU64::MIN);
    assert!(matches!(
        ids.close_guard(boundary_id),
        Err(WatcherCoverageError::IdentifierExhausted)
    ));
}
