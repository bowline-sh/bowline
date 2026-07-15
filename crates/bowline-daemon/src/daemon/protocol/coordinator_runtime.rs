use super::super::*;
use super::ThreadJoinReport;

use super::super::coordinator::{
    COORDINATOR_EVENT_CAPACITY, CoordinatorAction, CoordinatorClock, CoordinatorDeadline,
    CoordinatorDeadlineKind, CoordinatorDriver, CoordinatorEvent, CoordinatorEventSendErrorKind,
    CoordinatorExecutor, CoordinatorExecutorConfig, CoordinatorHandle, CoordinatorJob,
    CoordinatorJobId, CoordinatorLane, CoordinatorMetrics, CoordinatorResourceKey,
    CoordinatorState, CoordinatorSubmitErrorKind, CoordinatorWorkFailure,
    CoordinatorWorkFailureCode, CoordinatorWorkerCompletion, CoordinatorWorkerOutcome, DirtyPath,
    DirtyScopeKey, FilesystemDirty, PendingDirtyBatch, SystemCoordinatorClock, coordinator_channel,
};
use super::super::sync::{
    DaemonWorkLane, NotificationPollCompletion, PreparedDaemonWork, StatusPublishCompletion,
    WorkerCompletion, WorkerLossRecovery,
};

mod side_lanes;
#[cfg(test)]
mod tests;
mod watcher_bridge;

use watcher_bridge::{WatcherBridge, WatcherWakeState, stop_and_join_watcher};

const COORDINATOR_LEASE_RENEW_INTERVAL: Duration = Duration::from_secs(15);
const MAX_DURABLE_CONTROL_PLANE_IN_FLIGHT: usize = 3;

pub(super) fn run_scheduler(
    runtime: DaemonRuntime,
    state: Arc<DaemonServerState>,
    ready: crossbeam_channel::Sender<io::Result<()>>,
    metrics: Arc<CoordinatorMetrics>,
) -> io::Result<ThreadJoinReport> {
    let (handle, receiver) = coordinator_channel(COORDINATOR_EVENT_CAPACITY);
    let mut runtime = runtime;
    let watcher_bridge = match WatcherBridge::start(&mut runtime, handle.clone()) {
        Ok(watcher_bridge) => watcher_bridge,
        Err(error) => {
            let startup_error = io::Error::new(error.kind(), error.to_string());
            let _receiver_gone = ready.send(Err(startup_error));
            return Err(error);
        }
    };
    let watcher_wake = watcher_bridge
        .as_ref()
        .map(WatcherBridge::wake_state)
        .unwrap_or_default();
    let watcher_scope = watcher_bridge.as_ref().map(WatcherBridge::scope);
    let runtime = Arc::new(Mutex::new(runtime));
    state.register_coordinator_wake(handle.clone());
    let clock = SystemCoordinatorClock::new();
    let coordinator_state = CoordinatorState::new(clock.clone(), Arc::clone(&metrics));
    let mut driver = CoordinatorDriver::new(coordinator_state, receiver);
    let executor = match CoordinatorExecutor::new(
        CoordinatorExecutorConfig::default(),
        handle.clone(),
        metrics,
    ) {
        Ok(executor) => executor,
        Err(error) => {
            let startup_error = io::Error::new(error.kind(), error.to_string());
            let _receiver_gone = ready.send(Err(startup_error));
            state.unregister_coordinator_wake();
            stop_and_join_watcher(&runtime, watcher_bridge)?;
            return Err(error);
        }
    };
    if ready.send(Ok(())).is_err() {
        state.unregister_coordinator_wake();
        executor.shutdown_and_join()?;
        stop_and_join_watcher(&runtime, watcher_bridge)?;
        return Err(io::Error::other(
            "daemon scheduler readiness receiver disconnected",
        ));
    }
    let lane_workers = executor.worker_count();
    let watcher_workers = usize::from(watcher_bridge.is_some());
    let (completion_tx, completion_rx) = crossbeam_channel::unbounded();
    let (loss_fallback_tx, loss_fallback_rx) = crossbeam_channel::unbounded();
    let mut scheduler = SchedulerCoordinator::new(
        runtime,
        Arc::clone(&state),
        handle,
        clock,
        SchedulerChannels {
            completion_tx,
            completion_rx,
            loss_fallback_tx,
            loss_fallback_rx,
        },
        watcher_wake,
        watcher_scope,
    );
    state.wake_durable_work();
    scheduler.wake_status();
    scheduler.schedule_periodic_deadlines(&mut driver);
    while !state.should_stop_durable_claims() && !driver.state().is_shutting_down() {
        let actions = match driver.run_turn() {
            Ok(actions) => actions,
            Err(_) => break,
        };
        let mut stop_requested = false;
        for action in actions {
            if scheduler.handle_action(action, &executor, &mut driver) {
                stop_requested = true;
                break;
            }
        }
        if stop_requested {
            break;
        }
        if state.take_durable_work_wake() {
            scheduler.drive(&executor, &mut driver);
        }
        if state.take_projection_wake() {
            scheduler.publish_status(&executor);
            scheduler.submit_notification(&executor);
        }
        if scheduler.recover_saturated_watcher_wake(&executor, &mut driver) {
            break;
        }
    }
    state.unregister_coordinator_wake();
    let executor_result = executor.shutdown_and_join();
    scheduler.settle_shutdown_completions();
    executor_result?;
    scheduler.stop_watcher();
    if let Some(watcher_bridge) = watcher_bridge {
        watcher_bridge.join()?;
    }
    let (remote_observers_started, remote_observers_joined) = scheduler
        .runtime
        .lock()
        .ok()
        .and_then(|mut runtime| {
            runtime
                .sync
                .as_mut()
                .map(|sync| sync.remote_ref_observer.shutdown_and_report())
        })
        .unwrap_or_default();
    if remote_observers_started != remote_observers_joined {
        return Err(io::Error::other(
            "daemon remote ref observers were not fully joined",
        ));
    }
    Ok(ThreadJoinReport {
        expected: lane_workers + watcher_workers + remote_observers_started,
        joined: lane_workers + watcher_workers + remote_observers_joined,
        forced_recovery: false,
    })
}

struct SchedulerCoordinator {
    runtime: Arc<Mutex<DaemonRuntime>>,
    state: Arc<DaemonServerState>,
    handle: CoordinatorHandle,
    clock: SystemCoordinatorClock,
    completion_tx: crossbeam_channel::Sender<(String, WorkerCompletion)>,
    completion_rx: crossbeam_channel::Receiver<(String, WorkerCompletion)>,
    loss_fallback_tx: crossbeam_channel::Sender<SchedulerFallback>,
    loss_fallback_rx: crossbeam_channel::Receiver<SchedulerFallback>,
    pending_completions: HashMap<String, WorkerCompletion>,
    in_flight: HashMap<String, WorkerLossRecovery>,
    lost_in_flight: BTreeSet<String>,
    watcher_wake: WatcherWakeState,
    watcher_scope: Option<DirtyScopeKey>,
    scheduled_runtime_deadline: Option<String>,
    deadline_sequence: u64,
    notification_in_flight: Option<CoordinatorJobId>,
    status_publish_in_flight: Option<CoordinatorJobId>,
    trust_refresh_in_flight: bool,
    side_lane_sequence: u64,
    prefer_work_view_accept: bool,
}

enum SchedulerFallback {
    DurableLoss {
        operation_id: String,
        recovery: WorkerLossRecovery,
    },
    DurableDispatchRecovered(String),
    NotificationCompleted {
        job_id: CoordinatorJobId,
        completion: Box<NotificationPollCompletion>,
    },
    NotificationWorkerLost(CoordinatorJobId),
    StatusPublishCompleted {
        job_id: CoordinatorJobId,
        completion: StatusPublishCompletion,
    },
    StatusPublishWorkerLost(CoordinatorJobId),
    TrustRefreshCompleted,
}

struct SchedulerChannels {
    completion_tx: crossbeam_channel::Sender<(String, WorkerCompletion)>,
    completion_rx: crossbeam_channel::Receiver<(String, WorkerCompletion)>,
    loss_fallback_tx: crossbeam_channel::Sender<SchedulerFallback>,
    loss_fallback_rx: crossbeam_channel::Receiver<SchedulerFallback>,
}

impl SchedulerCoordinator {
    fn new(
        runtime: Arc<Mutex<DaemonRuntime>>,
        state: Arc<DaemonServerState>,
        handle: CoordinatorHandle,
        clock: SystemCoordinatorClock,
        channels: SchedulerChannels,
        watcher_wake: WatcherWakeState,
        watcher_scope: Option<DirtyScopeKey>,
    ) -> Self {
        Self {
            runtime,
            state,
            handle,
            clock,
            completion_tx: channels.completion_tx,
            completion_rx: channels.completion_rx,
            loss_fallback_tx: channels.loss_fallback_tx,
            loss_fallback_rx: channels.loss_fallback_rx,
            pending_completions: HashMap::new(),
            in_flight: HashMap::new(),
            lost_in_flight: BTreeSet::new(),
            watcher_wake,
            watcher_scope,
            scheduled_runtime_deadline: None,
            deadline_sequence: 0,
            notification_in_flight: None,
            status_publish_in_flight: None,
            trust_refresh_in_flight: false,
            side_lane_sequence: 0,
            prefer_work_view_accept: true,
        }
    }

    fn wake_status(&self) {
        let _already_awake = self.handle.try_send(CoordinatorEvent::StatusInput(
            bowline_daemon::status_projection::StatusInputEvent::RefreshAll,
        ));
    }

    fn recover_saturated_watcher_wake(
        &mut self,
        executor: &CoordinatorExecutor,
        driver: &mut CoordinatorDriver<SystemCoordinatorClock>,
    ) -> bool {
        if !self.watcher_wake.delivery_failed() {
            return false;
        }
        let Some(scope) = self.watcher_scope.clone() else {
            return false;
        };
        let event = if self.watcher_wake.overflow_is_pending() {
            CoordinatorEvent::WatcherOverflow(scope)
        } else {
            CoordinatorEvent::FilesystemDirty(FilesystemDirty::one(
                scope,
                DirtyPath::new("watcher-event"),
            ))
        };
        let actions = driver.state_mut().handle_event(event);
        for action in actions {
            if self.handle_action(action, executor, driver) {
                return true;
            }
        }
        false
    }

    fn schedule_periodic_deadlines(&self, driver: &mut CoordinatorDriver<SystemCoordinatorClock>) {
        self.schedule_deadline(
            CoordinatorDeadlineKind::StatusRefresh,
            Duration::from_secs(1),
            driver,
        );
        self.schedule_deadline(
            CoordinatorDeadlineKind::HostedRefresh,
            Duration::from_secs(30),
            driver,
        );
        self.schedule_deadline(
            CoordinatorDeadlineKind::NotificationPoll,
            Duration::from_secs(30),
            driver,
        );
    }

    fn handle_action(
        &mut self,
        action: CoordinatorAction,
        executor: &CoordinatorExecutor,
        driver: &mut CoordinatorDriver<SystemCoordinatorClock>,
    ) -> bool {
        let side_lane_recovery = self.drain_loss_fallbacks();
        match action {
            CoordinatorAction::DiscoverDurableWork => {
                self.state.take_durable_work_wake();
                self.drive(executor, driver);
            }
            CoordinatorAction::DirtyReady(scope) => {
                if self.watcher_wake.reset_for_dirty_ready() {
                    let _upgrade_actions = driver
                        .state_mut()
                        .handle_event(CoordinatorEvent::WatcherOverflow(scope.clone()));
                }
                if let Some(batch) = driver.state_mut().take_dirty(&scope) {
                    match batch {
                        PendingDirtyBatch::Paths(paths) => {
                            let _coalesced_path_count = paths.len();
                        }
                        PendingDirtyBatch::FullScan(_) => {
                            if let Ok(mut runtime) = self.runtime.lock()
                                && let Some(sync) = runtime.sync.as_mut()
                            {
                                sync.begin_watcher_overflow_recovery(Instant::now());
                            }
                            self.schedule_deadline(
                                CoordinatorDeadlineKind::WatcherRearm(scope),
                                Duration::from_secs(2),
                                driver,
                            );
                        }
                    }
                }
                self.drive(executor, driver);
            }
            CoordinatorAction::ForwardStatusInput(input) => {
                self.state.forward_projection_input(input);
                self.publish_status(executor);
                self.submit_notification(executor);
            }
            CoordinatorAction::PublishProjection => {
                self.state.take_projection_wake();
                self.publish_status(executor);
                self.submit_notification(executor);
            }
            CoordinatorAction::WorkerCompleted(completion) => {
                self.handle_completion(&completion, executor, driver);
            }
            CoordinatorAction::WorkerLost(loss) => {
                eprintln!(
                    "bowline-daemon coordinator {} worker {} exited",
                    loss.lane.as_str(),
                    loss.worker_index
                );
                self.handle_worker_loss(loss.active_job_id.as_ref());
                self.submit_notification(executor);
                self.submit_trust_refresh(executor);
            }
            CoordinatorAction::DeadlineDue(kind) => self.handle_deadline(kind, executor, driver),
            CoordinatorAction::Shutdown => return true,
        }
        if self.state.should_stop_durable_claims() {
            let _already_awake = driver.state_mut().handle_event(CoordinatorEvent::Shutdown);
            return true;
        }
        if side_lane_recovery {
            self.publish_status(executor);
            self.submit_notification(executor);
            self.submit_trust_refresh(executor);
        }
        false
    }

    fn drive(
        &mut self,
        executor: &CoordinatorExecutor,
        driver: &mut CoordinatorDriver<SystemCoordinatorClock>,
    ) {
        self.publish_status(executor);
        self.submit_notification(executor);
        self.submit_trust_refresh(executor);
        let prefer_work_view_accept = self.prefer_work_view_accept;
        let durable_control_plane_in_flight = self
            .in_flight
            .values()
            .filter(|recovery| recovery.lane() == DaemonWorkLane::ControlPlane)
            .count();
        let side_capacity =
            MAX_DURABLE_CONTROL_PLANE_IN_FLIGHT.saturating_sub(durable_control_plane_in_flight);
        let (work, side_work) = self
            .runtime
            .lock()
            .map(|mut runtime| {
                let work = runtime.poll_prepare(prefer_work_view_accept);
                let side_work = (0..side_capacity)
                    .filter_map(|_| runtime.prepare_ready_side_work())
                    .collect::<Vec<_>>();
                (work, side_work)
            })
            .unwrap_or_default();
        if let Some(work) = work {
            self.prefer_work_view_accept = !prefer_work_view_accept;
            self.submit_durable_work(executor, work, driver);
        }
        for work in side_work {
            self.submit_durable_work(executor, work, driver);
        }
        self.schedule_runtime_deadline(driver);
    }

    fn submit_durable_work(
        &mut self,
        executor: &CoordinatorExecutor,
        work: PreparedDaemonWork,
        driver: &mut CoordinatorDriver<SystemCoordinatorClock>,
    ) {
        let operation_id = work.operation_id().to_string();
        let recovery = work.worker_loss_recovery();
        let lane = coordinator_lane(work.lane());
        let resource = CoordinatorResourceKey::new(work.resource_key().as_str());
        let task_completion = self.completion_tx.clone();
        let completion_id = operation_id.clone();
        let recovery_runtime = Arc::clone(&self.runtime);
        let recovery_id = operation_id.clone();
        let recovery_signal = self.loss_fallback_tx.clone();
        let completion_failure_tx = self.loss_fallback_tx.clone();
        let completion_failure_id = operation_id.clone();
        let completion_failure_recovery = recovery.clone();
        let loss_failure_tx = self.loss_fallback_tx.clone();
        let loss_failure_id = operation_id.clone();
        let loss_failure_recovery = recovery.clone();
        let job = CoordinatorJob::recoverable(
            CoordinatorJobId::new(operation_id.clone()),
            lane,
            Some(resource),
            work,
            move |work| {
                let completion = work.execute_caught();
                task_completion
                    .send((completion_id, completion))
                    .map_err(|_| {
                        CoordinatorWorkFailure::new(
                            CoordinatorWorkFailureCode::ExecutionFailed,
                            "daemon completion receiver disconnected",
                        )
                    })
            },
            move |work, kind: CoordinatorSubmitErrorKind| {
                if let Ok(mut runtime) = recovery_runtime.lock()
                    && runtime.requeue_dispatch_failure(work)
                {
                    let _coordinator_gone = recovery_signal
                        .send(SchedulerFallback::DurableDispatchRecovered(recovery_id));
                } else {
                    eprintln!("bowline-daemon could not requeue {recovery_id} after {kind:?}");
                }
            },
        )
        .on_completion_delivery_failure(move |_| {
            let _coordinator_gone = completion_failure_tx.send(SchedulerFallback::DurableLoss {
                operation_id: completion_failure_id,
                recovery: completion_failure_recovery,
            });
        })
        .on_worker_loss_delivery_failure(move |_| {
            let _coordinator_gone = loss_failure_tx.send(SchedulerFallback::DurableLoss {
                operation_id: loss_failure_id,
                recovery: loss_failure_recovery,
            });
        });
        match executor.submit(job) {
            Ok(()) => {
                self.in_flight.insert(operation_id.clone(), recovery);
                self.schedule_lease_renewal(&operation_id, driver);
            }
            Err(error) => {
                if error.recover().is_err() {
                    eprintln!("bowline-daemon lost dispatch recovery for {operation_id}");
                }
            }
        }
    }

    fn handle_completion(
        &mut self,
        completion: &CoordinatorWorkerCompletion,
        executor: &CoordinatorExecutor,
        driver: &mut CoordinatorDriver<SystemCoordinatorClock>,
    ) {
        let job_id = completion.job_id.as_str();
        if self.handle_side_lane_worker_completion(completion) {
            return;
        }
        if let Some(recovery) = self.in_flight.remove(job_id) {
            self.drain_domain_completions();
            if let Some(completion) = self.pending_completions.remove(job_id)
                && let Ok(mut runtime) = self.runtime.lock()
            {
                runtime.apply_worker_completion(completion);
                self.state.record_runtime_change(&runtime);
            } else if let Ok(mut runtime) = self.runtime.lock() {
                runtime.apply_worker_completion(WorkerCompletion::worker_lost(recovery));
                self.state.record_runtime_change(&runtime);
            }
            self.drive(executor, driver);
        } else if job_id == "trust-refresh" {
            self.trust_refresh_in_flight = false;
        } else if self.lost_in_flight.remove(job_id) {
            self.drain_domain_completions();
            self.pending_completions.remove(job_id);
            self.drive(executor, driver);
        }
    }

    fn handle_side_lane_worker_completion(
        &mut self,
        completion: &CoordinatorWorkerCompletion,
    ) -> bool {
        if self.notification_in_flight.as_ref() == Some(&completion.job_id) {
            if completion.outcome != CoordinatorWorkerOutcome::Succeeded {
                self.notification_in_flight = None;
            }
            return true;
        }
        if self.status_publish_in_flight.as_ref() == Some(&completion.job_id) {
            if completion.outcome != CoordinatorWorkerOutcome::Succeeded {
                self.status_publish_in_flight = None;
            }
            return true;
        }
        false
    }

    fn drain_domain_completions(&mut self) {
        while let Ok((completion_id, completion)) = self.completion_rx.try_recv() {
            self.pending_completions.insert(completion_id, completion);
        }
    }

    fn settle_shutdown_completions(&mut self) {
        self.drain_domain_completions();
        let completed = self
            .pending_completions
            .keys()
            .filter(|operation_id| self.in_flight.contains_key(*operation_id))
            .cloned()
            .collect::<Vec<_>>();
        for operation_id in completed {
            self.in_flight.remove(&operation_id);
            let Some(completion) = self.pending_completions.remove(&operation_id) else {
                continue;
            };
            if let Ok(mut runtime) = self.runtime.lock() {
                runtime.apply_worker_completion(completion);
                self.state.record_runtime_change(&runtime);
            }
        }
        let _side_lane_recovery = self.drain_loss_fallbacks();
        let abandoned = std::mem::take(&mut self.in_flight);
        for (_operation_id, recovery) in abandoned {
            if let Ok(mut runtime) = self.runtime.lock() {
                runtime.apply_worker_completion(WorkerCompletion::worker_lost(recovery));
                self.state.record_runtime_change(&runtime);
            }
        }
        self.pending_completions.clear();
    }

    fn drain_loss_fallbacks(&mut self) -> bool {
        let mut side_lane_recovery = false;
        while let Ok(fallback) = self.loss_fallback_rx.try_recv() {
            match fallback {
                SchedulerFallback::DurableLoss {
                    operation_id,
                    recovery,
                } => {
                    self.drain_domain_completions();
                    if self.in_flight.remove(&operation_id).is_none() {
                        continue;
                    }
                    if let Some(completion) = self.pending_completions.remove(&operation_id) {
                        if let Ok(mut runtime) = self.runtime.lock() {
                            runtime.apply_worker_completion(completion);
                            self.state.record_runtime_change(&runtime);
                        }
                        continue;
                    }
                    self.lost_in_flight.insert(operation_id);
                    if let Ok(mut runtime) = self.runtime.lock() {
                        runtime.apply_worker_completion(WorkerCompletion::worker_lost(recovery));
                        self.state.record_runtime_change(&runtime);
                    }
                }
                SchedulerFallback::DurableDispatchRecovered(operation_id) => {
                    self.in_flight.remove(&operation_id);
                    self.pending_completions.remove(&operation_id);
                }
                SchedulerFallback::NotificationCompleted { job_id, completion } => {
                    if self.notification_in_flight.as_ref() == Some(&job_id) {
                        self.notification_in_flight = None;
                        if let Ok(mut runtime) = self.runtime.lock() {
                            self.state
                                .complete_notification_poll(&mut runtime, *completion);
                        }
                        side_lane_recovery = true;
                    }
                }
                SchedulerFallback::NotificationWorkerLost(job_id) => {
                    if self.notification_in_flight.as_ref() == Some(&job_id) {
                        self.notification_in_flight = None;
                        side_lane_recovery = true;
                    }
                }
                SchedulerFallback::StatusPublishCompleted { job_id, completion } => {
                    if self.status_publish_in_flight.as_ref() == Some(&job_id) {
                        self.status_publish_in_flight = None;
                        if let Ok(mut runtime) = self.runtime.lock() {
                            self.state.complete_status_publish(&mut runtime, completion);
                        }
                        side_lane_recovery = true;
                    }
                }
                SchedulerFallback::StatusPublishWorkerLost(job_id) => {
                    if self.status_publish_in_flight.as_ref() == Some(&job_id) {
                        self.status_publish_in_flight = None;
                        side_lane_recovery = true;
                    }
                }
                SchedulerFallback::TrustRefreshCompleted => {
                    self.trust_refresh_in_flight = false;
                    side_lane_recovery = true;
                }
            }
        }
        side_lane_recovery
    }

    fn handle_worker_loss(&mut self, active_job_id: Option<&CoordinatorJobId>) {
        if active_job_id.is_some_and(|job_id| self.notification_in_flight.as_ref() == Some(job_id))
        {
            self.notification_in_flight = None;
            return;
        }
        if active_job_id
            .is_some_and(|job_id| self.status_publish_in_flight.as_ref() == Some(job_id))
        {
            self.status_publish_in_flight = None;
            return;
        }
        if active_job_id.is_some_and(|job_id| job_id.as_str() == "trust-refresh") {
            self.trust_refresh_in_flight = false;
            return;
        }
        if let Some((operation_id, recovery)) =
            take_exact_worker_loss(&mut self.in_flight, active_job_id)
        {
            self.lost_in_flight.insert(operation_id);
            if let Ok(mut runtime) = self.runtime.lock() {
                runtime.apply_worker_completion(WorkerCompletion::worker_lost(recovery));
                self.state.record_runtime_change(&runtime);
            }
        }
    }

    fn handle_deadline(
        &mut self,
        kind: CoordinatorDeadlineKind,
        executor: &CoordinatorExecutor,
        driver: &mut CoordinatorDriver<SystemCoordinatorClock>,
    ) {
        match kind {
            CoordinatorDeadlineKind::DurableRetry(job_id)
                if self.scheduled_runtime_deadline.as_deref() == Some(job_id.as_str()) =>
            {
                self.scheduled_runtime_deadline = None;
                self.drive(executor, driver);
            }
            CoordinatorDeadlineKind::LeaseRenewal(job_id) => {
                let operation_id = job_id.as_str();
                if let Some(recovery) = self.in_flight.get(operation_id).cloned() {
                    let renewed = self
                        .runtime
                        .lock()
                        .is_ok_and(|runtime| runtime.renew_worker_claim(&recovery));
                    if renewed {
                        self.schedule_lease_renewal(operation_id, driver);
                    } else {
                        self.in_flight.remove(operation_id);
                        self.lost_in_flight.insert(operation_id.to_string());
                        if let Ok(mut runtime) = self.runtime.lock() {
                            runtime
                                .apply_worker_completion(WorkerCompletion::worker_lost(recovery));
                            self.state.record_runtime_change(&runtime);
                        }
                    }
                }
            }
            CoordinatorDeadlineKind::HostedRefresh => {
                self.submit_trust_refresh(executor);
                self.schedule_deadline(
                    CoordinatorDeadlineKind::HostedRefresh,
                    Duration::from_secs(30),
                    driver,
                );
            }
            CoordinatorDeadlineKind::StatusRefresh => {
                self.publish_status(executor);
                self.submit_notification(executor);
                self.schedule_deadline(
                    CoordinatorDeadlineKind::StatusRefresh,
                    Duration::from_secs(1),
                    driver,
                );
            }
            CoordinatorDeadlineKind::NotificationPoll => {
                self.submit_notification(executor);
                self.schedule_deadline(
                    CoordinatorDeadlineKind::NotificationPoll,
                    Duration::from_secs(30),
                    driver,
                );
            }
            CoordinatorDeadlineKind::WatcherRearm(_) => self.drive(executor, driver),
            CoordinatorDeadlineKind::DurableRetry(_) => {}
        }
    }

    fn stop_watcher(&self) {
        if let Ok(mut runtime) = self.runtime.lock()
            && let Some(sync) = runtime.sync.as_mut()
        {
            sync.watcher.take();
            sync.change_rx.take();
        }
    }

    fn schedule_runtime_deadline(
        &mut self,
        driver: &mut CoordinatorDriver<SystemCoordinatorClock>,
    ) {
        let now = Instant::now();
        let deadline = self
            .runtime
            .lock()
            .map(|runtime| runtime.next_scheduler_deadline(now))
            .unwrap_or(now + Duration::from_secs(1));
        if !self.in_flight.is_empty() && deadline <= now {
            // The claimed operation's completion, watcher input, durable RPC
            // wake, and lease renewal are all causal wakes. Re-arming an
            // already-due discovery deadline while the claim is executing
            // would starve those events behind a zero-timeout loop.
            self.scheduled_runtime_deadline = None;
            return;
        }
        self.deadline_sequence = self.deadline_sequence.saturating_add(1);
        let job_id = CoordinatorJobId::new(format!("scheduler-{}", self.deadline_sequence));
        self.scheduled_runtime_deadline = Some(job_id.as_str().to_string());
        self.schedule_deadline(
            CoordinatorDeadlineKind::DurableRetry(job_id),
            deadline.saturating_duration_since(now),
            driver,
        );
    }

    fn schedule_lease_renewal(
        &self,
        operation_id: &str,
        driver: &mut CoordinatorDriver<SystemCoordinatorClock>,
    ) {
        self.schedule_deadline(
            CoordinatorDeadlineKind::LeaseRenewal(CoordinatorJobId::new(operation_id)),
            COORDINATOR_LEASE_RENEW_INTERVAL,
            driver,
        );
    }

    fn schedule_deadline(
        &self,
        kind: CoordinatorDeadlineKind,
        delay: Duration,
        driver: &mut CoordinatorDriver<SystemCoordinatorClock>,
    ) {
        let deadline = CoordinatorDeadline {
            due: self.clock.now().add(delay),
            kind,
        };
        let actions = driver
            .state_mut()
            .handle_event(CoordinatorEvent::ScheduleDeadline(deadline));
        debug_assert!(
            actions.is_empty(),
            "scheduling a deadline is side-effect free"
        );
    }
}

fn take_exact_worker_loss<T>(
    in_flight: &mut HashMap<String, T>,
    active_job_id: Option<&CoordinatorJobId>,
) -> Option<(String, T)> {
    let operation_id = active_job_id?.as_str();
    in_flight
        .remove(operation_id)
        .map(|recovery| (operation_id.to_string(), recovery))
}

fn coordinator_lane(lane: DaemonWorkLane) -> CoordinatorLane {
    match lane {
        DaemonWorkLane::Sync => CoordinatorLane::Sync,
        DaemonWorkLane::ControlPlane => CoordinatorLane::ControlPlane,
    }
}
