use std::{
    cell::Cell,
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use arc_swap::ArcSwapOption;
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TrySendError, bounded};

use super::{
    reducer::RecoveryStateMachine,
    snapshot::WatcherRecoverySnapshot,
    types::{
        ActivityAdmission, AttemptToken, AuthorityRestoration, BackoffPolicy, CloseDisposition,
        CloseOffer, DependencyFailure, FailureDisposition, IncidentId, LossAdmission,
        RecoveryAttestation, RecoveryCause, RecoveryFrontier, RecoveryMoment, RecoveryProjectorId,
        RecoveryProjectorOwnership, RecoveryScanRevision, RecoverySourceIdentity,
        RecoveryTransitionError, RecoveryWorkerOwnership,
    },
};
use crate::watcher_coverage::{
    NativeCoverageObservation, WatcherCoverageHandoff, WatcherCoverageLoss,
};

struct CoordinatorState {
    model: RecoveryStateMachine,
    snapshot: Arc<WatcherRecoverySnapshot>,
    #[cfg(test)]
    snapshot_materializations: usize,
}

impl CoordinatorState {
    fn materialize_snapshot(&mut self) -> Arc<WatcherRecoverySnapshot> {
        if self.snapshot.snapshot_revision() != self.model.revision() {
            self.snapshot = Arc::new(self.model.snapshot());
            #[cfg(test)]
            {
                self.snapshot_materializations += 1;
            }
        }
        Arc::clone(&self.snapshot)
    }
}

struct RevisionSignal {
    revision: AtomicU64,
    worker: ArcSwapOption<WakeTarget>,
    projector: ArcSwapOption<WakeTarget>,
    next_projector_id: AtomicU64,
}

struct WakeTarget {
    generation: u64,
    claimed: AtomicBool,
    sender: Sender<()>,
    receiver: Receiver<()>,
}

impl RevisionSignal {
    fn new(
        revision: super::types::RecoveryRevision,
        worker: Option<RecoveryWorkerOwnership>,
    ) -> Self {
        let signal = Self {
            revision: AtomicU64::new(revision.get()),
            worker: ArcSwapOption::empty(),
            projector: ArcSwapOption::empty(),
            next_projector_id: AtomicU64::new(1),
        };
        if let Some(worker) = worker {
            signal.install_worker(worker);
        }
        signal
    }

    fn publish(&self, revision: super::types::RecoveryRevision) {
        let prior = self.revision.fetch_max(revision.get(), Ordering::Release);
        if prior >= revision.get() {
            return;
        }
        Self::wake(self.worker.load().as_deref());
        Self::wake(self.projector.load().as_deref());
    }

    fn load(&self) -> super::types::RecoveryRevision {
        super::types::RecoveryRevision::from_valid(self.revision.load(Ordering::Acquire))
    }

    fn install_worker(&self, ownership: RecoveryWorkerOwnership) {
        let previous = self.worker.load_full();
        self.worker
            .store(Some(Self::target(ownership.worker_id().get())));
        Self::wake(previous.as_deref());
    }

    fn revoke_worker(&self, ownership: RecoveryWorkerOwnership) {
        let current = self.worker.load_full();
        if current
            .as_ref()
            .is_some_and(|target| target.generation == ownership.worker_id().get())
        {
            self.worker.store(None);
            Self::wake(current.as_deref());
        }
    }

    fn replace_projector(
        &self,
    ) -> Result<RecoveryProjectorOwnership, WatcherRecoveryCoordinatorError> {
        let generation = self
            .next_projector_id
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_add(1)
            })
            .map_err(|_| WatcherRecoveryCoordinatorError::ProjectorIdentifierExhausted)?;
        let projector_id = RecoveryProjectorId::new(generation)
            .map_err(|_| WatcherRecoveryCoordinatorError::ProjectorIdentifierExhausted)?;
        let previous = self.projector.load_full();
        self.projector.store(Some(Self::target(generation)));
        Self::wake(previous.as_deref());
        Ok(RecoveryProjectorOwnership::new(projector_id))
    }

    fn claim_worker(
        &self,
        ownership: RecoveryWorkerOwnership,
    ) -> Result<Receiver<()>, WatcherRecoveryCoordinatorError> {
        Self::claim(
            self.worker.load_full(),
            ownership.worker_id().get(),
            RecoverySubscriptionRole::Worker,
        )
    }

    fn claim_projector(
        &self,
        ownership: RecoveryProjectorOwnership,
    ) -> Result<Receiver<()>, WatcherRecoveryCoordinatorError> {
        Self::claim(
            self.projector.load_full(),
            ownership.projector_id().get(),
            RecoverySubscriptionRole::Projector,
        )
    }

    fn claim(
        target: Option<Arc<WakeTarget>>,
        expected_generation: u64,
        role: RecoverySubscriptionRole,
    ) -> Result<Receiver<()>, WatcherRecoveryCoordinatorError> {
        let target = target.ok_or(WatcherRecoveryCoordinatorError::SubscriptionRevoked { role })?;
        if target.generation != expected_generation {
            return Err(WatcherRecoveryCoordinatorError::SubscriptionRevoked { role });
        }
        target
            .claimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| WatcherRecoveryCoordinatorError::RoleAlreadySubscribed { role })?;
        while target.receiver.try_recv().is_ok() {}
        Ok(target.receiver.clone())
    }

    fn is_current(&self, fence: SubscriptionFence) -> bool {
        let (target, generation) = match fence {
            SubscriptionFence::Worker(ownership) => {
                (self.worker.load_full(), ownership.worker_id().get())
            }
            SubscriptionFence::Projector(ownership) => {
                (self.projector.load_full(), ownership.projector_id().get())
            }
        };
        target
            .as_ref()
            .is_some_and(|target| target.generation == generation)
    }

    fn release(&self, fence: SubscriptionFence) {
        let (target, generation) = match fence {
            SubscriptionFence::Worker(ownership) => {
                (self.worker.load_full(), ownership.worker_id().get())
            }
            SubscriptionFence::Projector(ownership) => {
                (self.projector.load_full(), ownership.projector_id().get())
            }
        };
        if let Some(target) = target
            && target.generation == generation
        {
            target.claimed.store(false, Ordering::Release);
        }
    }

    fn target(generation: u64) -> Arc<WakeTarget> {
        let (sender, receiver) = bounded(1);
        Arc::new(WakeTarget {
            generation,
            claimed: AtomicBool::new(false),
            sender,
            receiver,
        })
    }

    fn wake(target: Option<&WakeTarget>) {
        let Some(target) = target else {
            return;
        };
        match target.sender.try_send(()) {
            Ok(()) | Err(TrySendError::Full(())) | Err(TrySendError::Disconnected(())) => {}
        }
    }
}

/// The one process-local authority for a workspace's recovery incident.
///
/// Callback-facing methods hold the state lock only while reducing an in-memory
/// event. They publish one shared scalar revision and one nonblocking wake to
/// the generation-fenced worker and composite-projector roles. Immutable
/// snapshot materialization is coalesced onto the next reader, outside callback
/// ingress. Consumers always re-read `snapshot` after observing a newer
/// revision.
pub struct WatcherRecoveryCoordinator {
    state: Mutex<CoordinatorState>,
    signal: Arc<RevisionSignal>,
}

impl WatcherRecoveryCoordinator {
    #[cfg(test)]
    pub(crate) fn nominal(
        source_identity: RecoverySourceIdentity,
        initial_at: super::types::RecoveryTimestamp,
        backoff_policy: BackoffPolicy,
    ) -> Self {
        Self::from_model(RecoveryStateMachine::nominal(
            source_identity,
            initial_at,
            backoff_policy,
        ))
    }

    pub fn startup_reconciliation(
        source_identity: RecoverySourceIdentity,
        moment: RecoveryMoment,
        backoff_policy: BackoffPolicy,
    ) -> Self {
        Self::from_model(RecoveryStateMachine::startup_reconciliation(
            source_identity,
            moment,
            backoff_policy,
        ))
    }

    pub fn snapshot(
        &self,
    ) -> Result<Arc<WatcherRecoverySnapshot>, WatcherRecoveryCoordinatorError> {
        self.state
            .lock()
            .map(|mut state| state.materialize_snapshot())
            .map_err(|_| WatcherRecoveryCoordinatorError::StateUnavailable)
    }

    pub fn subscribe_worker(
        &self,
        ownership: RecoveryWorkerOwnership,
    ) -> Result<RecoverySubscription, WatcherRecoveryCoordinatorError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| WatcherRecoveryCoordinatorError::StateUnavailable)?;
        state.model.require_worker(ownership).map_err(|_| {
            WatcherRecoveryCoordinatorError::SubscriptionRevoked {
                role: RecoverySubscriptionRole::Worker,
            }
        })?;
        let initial = state.materialize_snapshot();
        let wake = self.signal.claim_worker(ownership)?;
        Ok(RecoverySubscription {
            initial,
            signal: Arc::clone(&self.signal),
            fence: SubscriptionFence::Worker(ownership),
            wake,
            last_seen: Cell::new(super::types::RecoveryRevision::INITIAL),
        })
    }

    /// Replace the composite projector incarnation. The old projector is
    /// revoked even while it retains its receiver; the replacement starts from
    /// the current immutable revision rather than waiting for another change.
    pub fn replace_projector(
        &self,
    ) -> Result<RecoveryProjectorOwnership, WatcherRecoveryCoordinatorError> {
        self.signal.replace_projector()
    }

    pub fn subscribe_projector(
        &self,
        ownership: RecoveryProjectorOwnership,
    ) -> Result<RecoverySubscription, WatcherRecoveryCoordinatorError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| WatcherRecoveryCoordinatorError::StateUnavailable)?;
        let initial = state.materialize_snapshot();
        let wake = self.signal.claim_projector(ownership)?;
        Ok(RecoverySubscription {
            initial,
            signal: Arc::clone(&self.signal),
            fence: SubscriptionFence::Projector(ownership),
            wake,
            last_seen: Cell::new(super::types::RecoveryRevision::INITIAL),
        })
    }

    pub fn capture_nominal_frontier(
        &self,
    ) -> Result<RecoveryFrontier, WatcherRecoveryCoordinatorError> {
        let state = self
            .state
            .lock()
            .map_err(|_| WatcherRecoveryCoordinatorError::StateUnavailable)?;
        state
            .model
            .nominal_frontier()
            .ok_or(WatcherRecoveryCoordinatorError::Transition(
                RecoveryTransitionError::RecoveryNotNominal,
            ))
    }

    /// Linearizes the recovery side of an exact barrier. Ordinary callback
    /// activity advances the frontier even while no recovery incident is open.
    pub fn linearize_nominal(
        &self,
        expected: RecoveryFrontier,
    ) -> Result<RecoveryAttestation, WatcherRecoveryCoordinatorError> {
        let state = self
            .state
            .lock()
            .map_err(|_| WatcherRecoveryCoordinatorError::StateUnavailable)?;
        let current =
            state
                .model
                .nominal_frontier()
                .ok_or(WatcherRecoveryCoordinatorError::Transition(
                    RecoveryTransitionError::RecoveryNotNominal,
                ))?;
        if current != expected {
            return Err(WatcherRecoveryCoordinatorError::Transition(
                RecoveryTransitionError::RecoveryFrontierChanged,
            ));
        }
        Ok(RecoveryAttestation::new(
            state.model.source_identity().clone(),
            current,
            state.model.last_completed_receipt().cloned(),
        ))
    }

    /// Callback-facing ordinary activity admission. This performs no I/O, queue
    /// wait, scan, barrier, serialization, or task join.
    pub fn observe_activity(
        &self,
        moment: RecoveryMoment,
    ) -> Result<ActivityAdmission, WatcherRecoveryCoordinatorError> {
        self.mutate(|model| model.observe_activity(moment))
    }

    /// Admit an event the overflow latch dropped, which the covering scan must
    /// therefore redo.
    pub fn observe_suppressed(
        &self,
        moment: RecoveryMoment,
    ) -> Result<ActivityAdmission, WatcherRecoveryCoordinatorError> {
        self.mutate(|model| model.observe_suppressed(moment))
    }

    /// Callback-facing fidelity-loss admission. This performs no I/O, queue
    /// wait, scan, barrier, serialization, or task join.
    pub fn observe_loss(
        &self,
        cause: RecoveryCause,
        moment: RecoveryMoment,
    ) -> Result<LossAdmission, WatcherRecoveryCoordinatorError> {
        self.mutate(|model| model.observe_loss(cause, moment))
    }

    /// Typed ingress from Unit 1's bounded native-loss lane. Native stream
    /// identity remains in Unit 1's evidence; the coordinator owns the one
    /// incident transition and maps only the closed loss vocabulary.
    pub fn observe_native_coverage(
        &self,
        observation: NativeCoverageObservation,
        moment: RecoveryMoment,
    ) -> Result<LossAdmission, WatcherRecoveryCoordinatorError> {
        let cause = match observation.loss() {
            WatcherCoverageLoss::UserDropped
            | WatcherCoverageLoss::KernelDropped
            | WatcherCoverageLoss::EventIdsWrapped
            | WatcherCoverageLoss::NonMonotonicCursor
            | WatcherCoverageLoss::QueueOverflow => RecoveryCause::NativeRescanRequired,
            WatcherCoverageLoss::RootChanged => RecoveryCause::RootReplaced,
            WatcherCoverageLoss::StreamStopped => RecoveryCause::WatcherDisconnected,
            WatcherCoverageLoss::BackendFailure => RecoveryCause::RecoverableAdapterLoss,
        };
        self.mutate(|model| model.observe_loss(cause, moment))
    }

    /// Fence a newly constructed recovery worker. Replacing an existing owner
    /// invalidates its in-flight attempt and rearms the same incident.
    pub fn replace_worker(
        &self,
        moment: RecoveryMoment,
    ) -> Result<RecoveryWorkerOwnership, WatcherRecoveryCoordinatorError> {
        let (result, published_revision) = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| WatcherRecoveryCoordinatorError::StateUnavailable)?;
            let prior_revision = state.model.revision();
            let prior_worker = state.model.current_worker_ownership();
            let result = state.model.replace_worker(moment);
            match result.as_ref() {
                Ok(ownership) => self.signal.install_worker(*ownership),
                Err(_) if state.model.current_worker_ownership().is_none() => {
                    if let Some(prior_worker) = prior_worker {
                        self.signal.revoke_worker(prior_worker);
                    }
                }
                Err(_) => {}
            }
            let published_revision =
                (state.model.revision() != prior_revision).then(|| state.model.revision());
            (result, published_revision)
        };
        if let Some(revision) = published_revision {
            self.signal.publish(revision);
        }
        result.map_err(WatcherRecoveryCoordinatorError::Transition)
    }

    /// Report an owned worker exit. Stale exits are rejected; a current exit
    /// rearms the same incident or opens watcher-disconnected recovery when the
    /// runtime had been nominal.
    pub fn worker_exited(
        &self,
        ownership: RecoveryWorkerOwnership,
        moment: RecoveryMoment,
    ) -> Result<IncidentId, WatcherRecoveryCoordinatorError> {
        let (result, published_revision) = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| WatcherRecoveryCoordinatorError::StateUnavailable)?;
            let prior_revision = state.model.revision();
            let result = state.model.worker_exited(ownership, moment);
            if result.is_ok() {
                self.signal.revoke_worker(ownership);
            }
            let published_revision =
                (state.model.revision() != prior_revision).then(|| state.model.revision());
            (result, published_revision)
        };
        if let Some(revision) = published_revision {
            self.signal.publish(revision);
        }
        result.map_err(WatcherRecoveryCoordinatorError::Transition)
    }

    pub fn start_attempt(
        &self,
        ownership: RecoveryWorkerOwnership,
        moment: RecoveryMoment,
    ) -> Result<AttemptToken, WatcherRecoveryCoordinatorError> {
        self.mutate(|model| {
            model.require_worker(ownership)?;
            model.start_attempt(moment)
        })
    }

    pub fn record_native_boundary(
        &self,
        ownership: RecoveryWorkerOwnership,
        token: AttemptToken,
        handoff: WatcherCoverageHandoff,
        moment: RecoveryMoment,
    ) -> Result<(), WatcherRecoveryCoordinatorError> {
        self.mutate(|model| {
            model.require_worker(ownership)?;
            model.record_native_boundary(token, handoff, moment)
        })
    }

    pub fn record_scan_started(
        &self,
        ownership: RecoveryWorkerOwnership,
        token: AttemptToken,
        moment: RecoveryMoment,
    ) -> Result<(), WatcherRecoveryCoordinatorError> {
        self.mutate(|model| {
            model.require_worker(ownership)?;
            model.record_scan_started(token, moment)
        })
    }

    pub fn record_scan_completed(
        &self,
        ownership: RecoveryWorkerOwnership,
        token: AttemptToken,
        scan_revision: RecoveryScanRevision,
        moment: RecoveryMoment,
    ) -> Result<(), WatcherRecoveryCoordinatorError> {
        self.mutate(|model| {
            model.require_worker(ownership)?;
            model.record_scan_completed(token, scan_revision, moment)
        })
    }

    pub fn offer_close(
        &self,
        ownership: RecoveryWorkerOwnership,
        token: AttemptToken,
        scan_revision: RecoveryScanRevision,
        moment: RecoveryMoment,
    ) -> Result<CloseDisposition, WatcherRecoveryCoordinatorError> {
        self.mutate(|model| {
            model.require_worker(ownership)?;
            if model.current_attempt_token() != Some(token) {
                return Err(RecoveryTransitionError::AttemptMismatch);
            }
            let native_boundary = model
                .current_native_boundary()
                .cloned()
                .ok_or(RecoveryTransitionError::NativeBoundaryMismatch)?;
            let offer = CloseOffer::from_attempt_evidence(token, native_boundary, scan_revision);
            if !offer.native_boundary().is_current() {
                return model.reject_native_close(offer.attempt(), moment);
            }
            model.offer_close(&offer, moment)
        })
    }

    pub fn record_dependency_failure(
        &self,
        ownership: RecoveryWorkerOwnership,
        token: AttemptToken,
        failure: DependencyFailure,
        moment: RecoveryMoment,
    ) -> Result<FailureDisposition, WatcherRecoveryCoordinatorError> {
        self.mutate(|model| {
            model.require_worker(ownership)?;
            model.record_dependency_failure(token, failure, moment)
        })
    }

    pub(crate) fn restore_matching_authority(
        &self,
        ownership: RecoveryWorkerOwnership,
        expected_failure: &DependencyFailure,
        moment: RecoveryMoment,
    ) -> Result<AuthorityRestoration, WatcherRecoveryCoordinatorError> {
        self.mutate(|model| {
            model.require_worker(ownership)?;
            model.restore_matching_authority(expected_failure, moment)
        })
    }

    pub fn retry_if_due(
        &self,
        ownership: RecoveryWorkerOwnership,
        moment: RecoveryMoment,
    ) -> Result<IncidentId, WatcherRecoveryCoordinatorError> {
        self.mutate(|model| {
            model.require_worker(ownership)?;
            model.retry_if_due(moment)
        })
    }

    fn from_model(model: RecoveryStateMachine) -> Self {
        let snapshot = Arc::new(model.snapshot());
        let signal = Arc::new(RevisionSignal::new(
            snapshot.snapshot_revision(),
            model.current_worker_ownership(),
        ));
        Self {
            state: Mutex::new(CoordinatorState {
                model,
                snapshot,
                #[cfg(test)]
                snapshot_materializations: 1,
            }),
            signal,
        }
    }

    #[cfg(test)]
    pub(crate) fn from_model_for_test(model: RecoveryStateMachine) -> Self {
        Self::from_model(model)
    }

    fn mutate<T>(
        &self,
        transition: impl FnOnce(&mut RecoveryStateMachine) -> Result<T, RecoveryTransitionError>,
    ) -> Result<T, WatcherRecoveryCoordinatorError> {
        self.mutate_state(|state| transition(&mut state.model))
    }

    fn mutate_state<T>(
        &self,
        transition: impl FnOnce(&mut CoordinatorState) -> Result<T, RecoveryTransitionError>,
    ) -> Result<T, WatcherRecoveryCoordinatorError> {
        let (result, published_revision) = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| WatcherRecoveryCoordinatorError::StateUnavailable)?;
            let prior_revision = state.model.revision();
            let result = transition(&mut state);
            let published_revision = if state.model.revision() != prior_revision {
                Some(state.model.revision())
            } else {
                None
            };
            (result, published_revision)
        };
        if let Some(revision) = published_revision {
            // The reducer transition and immutable snapshot are already
            // committed. Notification is therefore monotonic and infallible:
            // mutex poisoning cannot make callers believe the transition was
            // rejected, and a delayed older publisher cannot regress the
            // shared advisory revision.
            self.signal.publish(revision);
        }
        result.map_err(WatcherRecoveryCoordinatorError::Transition)
    }
}

pub struct RecoverySubscription {
    initial: Arc<WatcherRecoverySnapshot>,
    signal: Arc<RevisionSignal>,
    fence: SubscriptionFence,
    wake: Receiver<()>,
    last_seen: Cell<super::types::RecoveryRevision>,
}

impl RecoverySubscription {
    pub fn initial(&self) -> Arc<WatcherRecoverySnapshot> {
        Arc::clone(&self.initial)
    }

    pub fn try_recv(&self) -> Result<super::types::RecoveryRevision, RecoverySubscriptionError> {
        self.admit_revision(self.signal.load())
    }

    pub fn recv(&self) -> Result<super::types::RecoveryRevision, RecoverySubscriptionError> {
        loop {
            self.require_current()?;
            let published = self.signal.load();
            if published > self.last_seen.get() {
                return self.admit_revision(published);
            }
            if self.wake.recv().is_err() {
                return if self.signal.is_current(self.fence) {
                    Err(RecoverySubscriptionError::WakeDisconnected)
                } else {
                    Err(RecoverySubscriptionError::Revoked)
                };
            }
        }
    }

    pub fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Result<super::types::RecoveryRevision, RecoverySubscriptionError> {
        let deadline = Instant::now().checked_add(timeout);
        loop {
            self.require_current()?;
            let published = self.signal.load();
            if published > self.last_seen.get() {
                return self.admit_revision(published);
            }
            let Some(deadline) = deadline else {
                self.wake
                    .recv()
                    .map_err(|_| RecoverySubscriptionError::WakeDisconnected)?;
                continue;
            };
            let now = Instant::now();
            if now >= deadline {
                return self.admit_after_timeout();
            }
            match self.wake.recv_timeout(deadline.duration_since(now)) {
                Ok(()) => {}
                Err(RecvTimeoutError::Timeout) => return self.admit_after_timeout(),
                Err(RecvTimeoutError::Disconnected) => {
                    return if self.signal.is_current(self.fence) {
                        Err(RecoverySubscriptionError::WakeDisconnected)
                    } else {
                        Err(RecoverySubscriptionError::Revoked)
                    };
                }
            }
        }
    }

    fn admit_after_timeout(
        &self,
    ) -> Result<super::types::RecoveryRevision, RecoverySubscriptionError> {
        match self.admit_revision(self.signal.load()) {
            Err(RecoverySubscriptionError::WouldBlock) => Err(RecoverySubscriptionError::TimedOut),
            result => result,
        }
    }

    fn admit_revision(
        &self,
        published: super::types::RecoveryRevision,
    ) -> Result<super::types::RecoveryRevision, RecoverySubscriptionError> {
        self.require_current()?;
        if published <= self.last_seen.get() {
            return Err(RecoverySubscriptionError::WouldBlock);
        }
        self.last_seen.set(published);
        Ok(published)
    }

    fn require_current(&self) -> Result<(), RecoverySubscriptionError> {
        if self.signal.is_current(self.fence) {
            Ok(())
        } else {
            Err(RecoverySubscriptionError::Revoked)
        }
    }
}

impl Drop for RecoverySubscription {
    fn drop(&mut self) {
        self.signal.release(self.fence);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubscriptionFence {
    Worker(RecoveryWorkerOwnership),
    Projector(RecoveryProjectorOwnership),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoverySubscriptionRole {
    Worker,
    Projector,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoverySubscriptionError {
    WouldBlock,
    TimedOut,
    WakeDisconnected,
    Revoked,
}

impl fmt::Display for RecoverySubscriptionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WouldBlock => formatter.write_str("no newer recovery revision is available"),
            Self::TimedOut => formatter.write_str("waiting for a recovery revision timed out"),
            Self::WakeDisconnected => {
                formatter.write_str("recovery revision wake channel disconnected")
            }
            Self::Revoked => formatter.write_str("recovery subscription ownership is stale"),
        }
    }
}

impl std::error::Error for RecoverySubscriptionError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatcherRecoveryCoordinatorError {
    StateUnavailable,
    ProjectorIdentifierExhausted,
    RoleAlreadySubscribed { role: RecoverySubscriptionRole },
    SubscriptionRevoked { role: RecoverySubscriptionRole },
    Transition(RecoveryTransitionError),
}

impl fmt::Display for WatcherRecoveryCoordinatorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StateUnavailable => formatter.write_str("recovery coordinator is unavailable"),
            Self::ProjectorIdentifierExhausted => {
                formatter.write_str("recovery projector identifier space is exhausted")
            }
            Self::RoleAlreadySubscribed { role } => {
                write!(formatter, "{role:?} recovery role already has a subscriber")
            }
            Self::SubscriptionRevoked { role } => {
                write!(
                    formatter,
                    "{role:?} recovery subscription ownership is stale"
                )
            }
            Self::Transition(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for WatcherRecoveryCoordinatorError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::StateUnavailable => None,
            Self::ProjectorIdentifierExhausted
            | Self::RoleAlreadySubscribed { .. }
            | Self::SubscriptionRevoked { .. } => None,
            Self::Transition(error) => Some(error),
        }
    }
}

#[cfg(test)]
mod tests;
