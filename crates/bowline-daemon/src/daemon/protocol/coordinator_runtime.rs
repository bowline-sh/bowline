use super::super::*;
use super::ThreadJoinReport;

use super::super::coordinator::{
    COORDINATOR_EVENT_CAPACITY, CoordinatorAction, CoordinatorClock, CoordinatorDeadline,
    CoordinatorDeadlineKind, CoordinatorDriver, CoordinatorEvent, CoordinatorExecutor,
    CoordinatorExecutorConfig, CoordinatorHandle, CoordinatorJob, CoordinatorJobId,
    CoordinatorLane, CoordinatorMetrics, CoordinatorState, CoordinatorWorkerCompletion,
    CoordinatorWorkerOutcome, SystemCoordinatorClock, coordinator_channel,
};
use super::super::sync::{NotificationPollCompletion, StatusPublishCompletion};

mod side_lanes;
#[cfg(test)]
mod tests;
pub(in crate::daemon) mod watcher_bridge;

use watcher_bridge::{WatcherBridge, stop_and_join_watcher};

pub(super) fn run_scheduler(
    runtime: DaemonRuntime,
    state: Arc<DaemonServerState>,
    ready: crossbeam_channel::Sender<io::Result<()>>,
    metrics: Arc<CoordinatorMetrics>,
) -> io::Result<ThreadJoinReport> {
    let (handle, receiver) = coordinator_channel(COORDINATOR_EVENT_CAPACITY);
    let mut runtime = runtime;
    let watcher_bridge = match WatcherBridge::start(&mut runtime) {
        Ok(watcher_bridge) => watcher_bridge,
        Err(error) => {
            let error = error.into_io_error();
            let startup_error = io::Error::new(error.kind(), error.to_string());
            let _receiver_gone = ready.send(Err(startup_error));
            return Err(error);
        }
    };
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
    let (loss_fallback_tx, loss_fallback_rx) = crossbeam_channel::unbounded();
    let mut scheduler = SchedulerCoordinator::new(
        runtime,
        Arc::clone(&state),
        handle,
        clock,
        loss_fallback_tx,
        loss_fallback_rx,
    );
    scheduler.attach_watcher_bridge(watcher_bridge);
    state.wake_engine_work();
    scheduler.wake_status();
    scheduler.schedule_periodic_deadlines(&mut driver);
    while !state.should_stop_background_work() && !driver.state().is_shutting_down() {
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
        if state.take_engine_work_wake() {
            scheduler.drive(&executor, &mut driver);
        }
        if state.take_projection_wake() {
            scheduler.publish_status(&executor);
            scheduler.submit_notification(&executor);
        }
    }
    state.unregister_coordinator_wake();
    let executor_result = executor.shutdown_and_join();
    scheduler.settle_shutdown_fallbacks();
    executor_result?;
    scheduler.stop_watcher();
    let (watcher_workers_started, watcher_workers_joined) =
        scheduler.join_active_watcher_bridge()?;
    Ok(ThreadJoinReport {
        expected: lane_workers + watcher_workers_started,
        joined: lane_workers + watcher_workers_joined,
        forced_recovery: false,
    })
}

struct SchedulerCoordinator {
    runtime: Arc<Mutex<DaemonRuntime>>,
    state: Arc<DaemonServerState>,
    handle: CoordinatorHandle,
    clock: SystemCoordinatorClock,
    loss_fallback_tx: crossbeam_channel::Sender<SchedulerFallback>,
    loss_fallback_rx: crossbeam_channel::Receiver<SchedulerFallback>,
    watcher_bridge: Option<WatcherBridge>,
    watcher_workers_started: usize,
    watcher_workers_joined: usize,
    scheduled_engine_retry: Option<String>,
    deadline_sequence: u64,
    notification_in_flight: Option<CoordinatorJobId>,
    status_publish_in_flight: Option<CoordinatorJobId>,
    trust_refresh_in_flight: bool,
    side_lane_sequence: u64,
}

enum SchedulerFallback {
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

impl SchedulerCoordinator {
    fn new(
        runtime: Arc<Mutex<DaemonRuntime>>,
        state: Arc<DaemonServerState>,
        handle: CoordinatorHandle,
        clock: SystemCoordinatorClock,
        loss_fallback_tx: crossbeam_channel::Sender<SchedulerFallback>,
        loss_fallback_rx: crossbeam_channel::Receiver<SchedulerFallback>,
    ) -> Self {
        Self {
            runtime,
            state,
            handle,
            clock,
            loss_fallback_tx,
            loss_fallback_rx,
            watcher_bridge: None,
            watcher_workers_started: 0,
            watcher_workers_joined: 0,
            scheduled_engine_retry: None,
            deadline_sequence: 0,
            notification_in_flight: None,
            status_publish_in_flight: None,
            trust_refresh_in_flight: false,
            side_lane_sequence: 0,
        }
    }

    fn wake_status(&self) {
        let _already_awake = self.handle.try_send(CoordinatorEvent::StatusInput(
            bowline_daemon::status_projection::StatusInputEvent::RefreshAll,
        ));
    }

    fn attach_watcher_bridge(&mut self, bridge: Option<WatcherBridge>) {
        self.watcher_workers_started = usize::from(bridge.is_some());
        self.watcher_bridge = bridge;
    }

    /// Start the watcher→engine bridge if it is not already running. Called after
    /// a manifest-driver rebuild brings the engine up post-startup, when the
    /// bridge was skipped because no engine event sender existed yet. Also
    /// restarts a bridge whose worker already exited (engine death drops the
    /// event sender and kills the previous forwarder).
    fn ensure_watcher_bridge(&mut self) {
        if let Some(bridge) = self.watcher_bridge.as_ref() {
            if !bridge.is_finished() {
                return;
            }
            // Join the finished worker so shutdown accounting and panic
            // reporting stay accurate before we start a replacement.
            if let Some(finished) = self.watcher_bridge.take() {
                match finished.join() {
                    Ok(()) => {
                        self.watcher_workers_joined = self.watcher_workers_joined.saturating_add(1);
                    }
                    Err(error) => {
                        eprintln!(
                            "bowline-daemon watcher bridge join after engine loss failed: {error}"
                        );
                    }
                }
            }
        }
        let started = match self.runtime.lock() {
            Ok(mut runtime) => {
                // The previous bridge worker owns (and drops) change_rx. Rebuild
                // the watcher kernel so a new receiver is available to hand off.
                if let Some(sync) = runtime.sync.as_mut()
                    && sync.change_rx.is_none()
                {
                    match crate::daemon::watcher::start_sync_watcher(&sync.args.root) {
                        Ok((watcher, change_rx)) => {
                            sync.watcher = Some(watcher);
                            sync.change_rx = Some(change_rx);
                        }
                        Err(error) => {
                            eprintln!(
                                "bowline-daemon watcher restart after bridge loss failed: {error}"
                            );
                            return;
                        }
                    }
                }
                WatcherBridge::start(&mut runtime)
            }
            Err(_) => return,
        };
        match started {
            Ok(Some(bridge)) => {
                self.watcher_bridge = Some(bridge);
                self.watcher_workers_started = self.watcher_workers_started.saturating_add(1);
            }
            Ok(None) => {}
            Err(error) => {
                eprintln!(
                    "bowline-daemon watcher bridge late start failed: {}",
                    error.into_io_error()
                );
            }
        }
    }

    fn join_active_watcher_bridge(&mut self) -> io::Result<(usize, usize)> {
        if let Some(bridge) = self.watcher_bridge.take() {
            bridge.join()?;
            self.watcher_workers_joined = self.watcher_workers_joined.saturating_add(1);
        }
        Ok((self.watcher_workers_started, self.watcher_workers_joined))
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
            CoordinatorAction::DriveEngine => {
                self.state.take_engine_work_wake();
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
                self.handle_completion(&completion);
            }
            CoordinatorAction::WorkerLost(loss) => {
                eprintln!(
                    "bowline-daemon coordinator {} worker {} exited",
                    loss.lane.as_str(),
                    loss.worker_index
                );
                self.handle_worker_loss(loss.active_job_id.as_ref());
                // The lost job's side-lane in-flight flag is cleared; drive()
                // reschedules the periodic work on the next wake.
                self.drive(executor, driver);
            }
            CoordinatorAction::DeadlineDue(kind) => self.handle_deadline(kind, executor, driver),
            CoordinatorAction::Shutdown => return true,
        }
        if self.state.should_stop_background_work() {
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

    /// One engine-drive cycle: retry a pending manifest-driver build, then run
    /// the side lanes (status publish, notification, trust refresh) and re-arm
    /// the engine-retry deadline. The manifest engine owns sync itself — the
    /// coordinator only wakes it back up when its driver needs rebuilding.
    fn drive(
        &mut self,
        executor: &CoordinatorExecutor,
        driver: &mut CoordinatorDriver<SystemCoordinatorClock>,
    ) {
        // Retry a pending manifest-engine rebuild before publishing status, so
        // this cycle's status reflects a driver that just came up. A driver that
        // builds late needs the watcher bridge started now (it was skipped at
        // scheduler startup while the driver was unavailable).
        let newly_active = self
            .runtime
            .lock()
            .map(|mut runtime| runtime.retry_manifest_engine(Instant::now()))
            .unwrap_or(false);
        if newly_active {
            self.ensure_watcher_bridge();
        }
        self.publish_status(executor);
        self.submit_notification(executor);
        self.submit_trust_refresh(executor);
        self.schedule_engine_retry_deadline(driver);
    }

    fn handle_completion(&mut self, completion: &CoordinatorWorkerCompletion) {
        if self.handle_side_lane_worker_completion(completion) {
            return;
        }
        if completion.job_id.as_str() == "trust-refresh" {
            self.trust_refresh_in_flight = false;
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

    fn settle_shutdown_fallbacks(&mut self) {
        let _side_lane_recovery = self.drain_loss_fallbacks();
    }

    fn drain_loss_fallbacks(&mut self) -> bool {
        let mut side_lane_recovery = false;
        while let Ok(fallback) = self.loss_fallback_rx.try_recv() {
            match fallback {
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
        }
    }

    fn handle_deadline(
        &mut self,
        kind: CoordinatorDeadlineKind,
        executor: &CoordinatorExecutor,
        driver: &mut CoordinatorDriver<SystemCoordinatorClock>,
    ) {
        match kind {
            CoordinatorDeadlineKind::EngineRetry(job_id)
                if self.scheduled_engine_retry.as_deref() == Some(job_id.as_str()) =>
            {
                self.scheduled_engine_retry = None;
                self.drive(executor, driver);
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
            CoordinatorDeadlineKind::EngineRetry(_) => {}
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

    /// Re-arm the engine-retry deadline when a manifest-driver rebuild is
    /// pending, so the loop wakes at the backoff instant even while otherwise
    /// idle. No deadline is scheduled while the driver is active.
    fn schedule_engine_retry_deadline(
        &mut self,
        driver: &mut CoordinatorDriver<SystemCoordinatorClock>,
    ) {
        let now = Instant::now();
        let next_retry = self.runtime.lock().ok().and_then(|runtime| {
            runtime
                .sync
                .as_ref()
                .and_then(|sync| sync.next_manifest_retry())
        });
        let Some(deadline) = next_retry else {
            self.scheduled_engine_retry = None;
            return;
        };
        self.deadline_sequence = self.deadline_sequence.saturating_add(1);
        let job_id = CoordinatorJobId::new(format!("engine-retry-{}", self.deadline_sequence));
        self.scheduled_engine_retry = Some(job_id.as_str().to_string());
        self.schedule_deadline(
            CoordinatorDeadlineKind::EngineRetry(job_id),
            deadline.saturating_duration_since(now),
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
