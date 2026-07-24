use super::super::coordinator::{CoordinatorMetrics, CoordinatorMetricsSnapshot};
use super::*;

#[cfg(test)]
mod tests;

const DEFAULT_SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ShutdownOutcome {
    Clean,
    ForcedRecovery,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ShutdownReport {
    pub(super) outcome: ShutdownOutcome,
    pub(super) expected_threads: usize,
    pub(super) joined_threads: usize,
    pub(super) elapsed: Duration,
    pub(super) grace: Duration,
    pub(super) coordinator_metrics: CoordinatorMetricsSnapshot,
    pub(super) rpc_metrics: super::super::protocol_v2::RpcExecutorMetricsSnapshot,
}

struct SchedulerThread {
    handle: std::thread::JoinHandle<()>,
    done: crossbeam_channel::Receiver<io::Result<ThreadJoinReport>>,
}

struct ShutdownWatchdog {
    cancel: crossbeam_channel::Sender<()>,
    expired: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl ShutdownWatchdog {
    fn start(socket: PathBuf, state: Arc<DaemonServerState>, grace: Duration) -> io::Result<Self> {
        let (cancel, cancelled) = crossbeam_channel::bounded(1);
        let expired = Arc::new(AtomicBool::new(false));
        let worker_expired = Arc::clone(&expired);
        let handle = std::thread::Builder::new()
            .name("bowline-shutdown-watchdog".to_string())
            .spawn(move || {
                if matches!(
                    cancelled.recv_timeout(grace),
                    Err(crossbeam_channel::RecvTimeoutError::Timeout)
                ) {
                    worker_expired.store(true, Ordering::Release);
                    state.advance_shutdown(ShutdownPhase::ForcedRecovery);
                    super::force_process_shutdown(&socket);
                }
            })?;
        Ok(Self {
            cancel,
            expired,
            handle: Some(handle),
        })
    }

    fn finish(mut self) -> io::Result<bool> {
        let _already_finished = self.cancel.try_send(());
        self.handle
            .take()
            .expect("shutdown watchdog remains owned until join")
            .join()
            .map_err(|_| io::Error::other("bowline shutdown watchdog panicked"))?;
        Ok(self.expired.load(Ordering::Acquire))
    }
}

impl Drop for ShutdownWatchdog {
    fn drop(&mut self) {
        let _already_finished = self.cancel.try_send(());
        if let Some(handle) = self.handle.take() {
            let _join_result = handle.join();
        }
    }
}

pub(super) struct DaemonThreads {
    acceptor: Option<BlockingAcceptor>,
    connections: Option<ConnectionExecutor>,
    rpc_executor: Arc<super::super::protocol_v2::RpcExecutor>,
    scheduler: Option<SchedulerThread>,
    coordinator_metrics: Arc<CoordinatorMetrics>,
    socket_path: PathBuf,
    socket_owner_uid: Option<u32>,
    state: Arc<DaemonServerState>,
}

impl DaemonThreads {
    pub(super) fn start(
        socket: &Path,
        once: bool,
        runtime: DaemonRuntime,
        state: Arc<DaemonServerState>,
    ) -> io::Result<Self> {
        let rpc_executor = Arc::new(super::super::protocol_v2::RpcExecutor::new(
            super::super::protocol_v2::RpcExecutorConfig::default(),
        )?);
        let connection_workers = if once { 1 } else { MAX_CONCURRENT_CONNECTIONS };
        let connections = match ConnectionExecutor::start(connection_workers) {
            Ok(connections) => connections,
            Err(error) => {
                state.begin_shutdown(ShutdownReason::StartupRollback);
                state.cancel_rpc_work();
                state.stop_background_work();
                let _rpc_workers = rpc_executor.shutdown_strict(DEFAULT_SHUTDOWN_GRACE);
                return Err(error);
            }
        };
        let scheduler_state = Arc::clone(&state);
        let coordinator_metrics = Arc::new(CoordinatorMetrics::default());
        state.register_runtime_metrics(
            Arc::clone(&coordinator_metrics),
            Arc::downgrade(&rpc_executor),
        );
        // The engine's cost meters are a persistent handle on the sync runtime
        // (stable across driver rebuilds), so registering once here is correct.
        if let Some(sync) = runtime.sync.as_ref() {
            state.register_manifest_counters(sync.manifest_counters());
        }
        let scheduler_metrics = Arc::clone(&coordinator_metrics);
        let (scheduler_ready_tx, scheduler_ready) = crossbeam_channel::bounded(1);
        let (scheduler_done_tx, scheduler_done) = crossbeam_channel::bounded(1);
        let scheduler_handle = match std::thread::Builder::new()
            .name("bowline-sync-scheduler".to_string())
            .spawn(move || {
                let result = run_scheduler(
                    runtime,
                    scheduler_state,
                    scheduler_ready_tx,
                    scheduler_metrics,
                );
                let _receiver_gone = scheduler_done_tx.send(result);
            }) {
            Ok(handle) => handle,
            Err(error) => {
                state.begin_shutdown(ShutdownReason::StartupRollback);
                state.cancel_rpc_work();
                state.stop_background_work();
                let _connections = connections.shutdown_and_join(DEFAULT_SHUTDOWN_GRACE);
                let _rpc_workers = rpc_executor.shutdown_strict(DEFAULT_SHUTDOWN_GRACE);
                return Err(error);
            }
        };
        let scheduler = SchedulerThread {
            handle: scheduler_handle,
            done: scheduler_done,
        };
        match scheduler_ready.recv() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                rollback_startup(&state, connections, &rpc_executor, scheduler);
                return Err(error);
            }
            Err(_) => {
                rollback_startup(&state, connections, &rpc_executor, scheduler);
                return Err(io::Error::other(
                    "daemon scheduler exited before reporting readiness",
                ));
            }
        }
        let listener = match UnixListener::bind(socket) {
            Ok(listener) => listener,
            Err(error) => {
                rollback_startup(&state, connections, &rpc_executor, scheduler);
                return Err(error);
            }
        };
        let socket_owner_uid = fs::metadata(socket).ok().map(|metadata| metadata.uid());
        let acceptor = match BlockingAcceptor::start(listener, socket, Arc::clone(&state)) {
            Ok(acceptor) => acceptor,
            Err(error) => {
                rollback_startup(&state, connections, &rpc_executor, scheduler);
                if let Err(cleanup_error) = fs::remove_file(socket)
                    && cleanup_error.kind() != io::ErrorKind::NotFound
                {
                    eprintln!(
                        "bowline-daemon could not remove socket after startup rollback: {cleanup_error}"
                    );
                }
                return Err(error);
            }
        };
        state.register_acceptor_wake(acceptor.wake());
        Ok(Self {
            acceptor: Some(acceptor),
            connections: Some(connections),
            rpc_executor,
            scheduler: Some(scheduler),
            coordinator_metrics,
            socket_path: socket.to_path_buf(),
            socket_owner_uid,
            state,
        })
    }

    pub(super) fn acceptor(&self) -> &BlockingAcceptor {
        self.acceptor
            .as_ref()
            .expect("acceptor remains owned until strict shutdown")
    }

    pub(super) fn connections(&self) -> &ConnectionExecutor {
        self.connections
            .as_ref()
            .expect("connection workers remain owned until strict shutdown")
    }

    pub(super) fn rpc_executor(&self) -> &Arc<super::super::protocol_v2::RpcExecutor> {
        &self.rpc_executor
    }

    pub(super) fn socket_owner_uid(&self) -> Option<u32> {
        self.socket_owner_uid
    }

    pub(super) fn shutdown(self, reason: ShutdownReason) -> io::Result<ShutdownReport> {
        self.shutdown_with_grace(reason, DEFAULT_SHUTDOWN_GRACE)
    }

    fn shutdown_with_grace(
        mut self,
        reason: ShutdownReason,
        grace: Duration,
    ) -> io::Result<ShutdownReport> {
        let state = Arc::clone(&self.state);
        let shutdown_started = Instant::now();
        state.begin_shutdown(reason);
        let watchdog =
            ShutdownWatchdog::start(self.socket_path.clone(), Arc::clone(&state), grace)?;
        let grace_deadline = Instant::now() + grace;
        let mut aggregate = ThreadJoinReport::default();
        let acceptor = self
            .acceptor
            .take()
            .expect("acceptor remains owned until strict shutdown");
        acceptor.ensure_stopped()?;
        acceptor.join()?;
        aggregate.record_joined(1);

        state.cancel_rpc_work();
        state.stop_background_work();

        let connections = self
            .connections
            .take()
            .expect("connection workers remain owned until strict shutdown");
        aggregate.merge(
            connections
                .shutdown_and_join(grace_deadline.saturating_duration_since(Instant::now()))?,
        );
        let (readers_started, readers_joined) = state.connection_reader_thread_counts();
        aggregate.expected += readers_started;
        aggregate.joined += readers_joined;
        if readers_started != readers_joined {
            return Err(io::Error::other(
                "daemon connection readers were not fully joined",
            ));
        }

        state.advance_shutdown(ShutdownPhase::FlushBookkeeping);
        aggregate.merge(join_scheduler(
            self.scheduler
                .take()
                .expect("scheduler remains owned until strict shutdown"),
            grace_deadline.saturating_duration_since(Instant::now()),
        )?);

        let projection_timed_out =
            state.shutdown_projection(grace_deadline.saturating_duration_since(Instant::now()))?;
        if projection_timed_out {
            state.advance_shutdown(ShutdownPhase::ForcedRecovery);
            super::handle_shutdown_grace_expiry("status projection worker");
            state.join_projection_after_shutdown()?;
            aggregate.forced_recovery = true;
        }
        aggregate.record_joined(1);

        state.advance_shutdown(ShutdownPhase::JoinThreads);
        aggregate.merge(
            self.rpc_executor
                .shutdown_strict(grace_deadline.saturating_duration_since(Instant::now()))?,
        );
        let rpc_metrics = self.rpc_executor.metrics();
        let watchdog_expired = watchdog.finish()?;
        aggregate.record_joined(1);
        aggregate.forced_recovery |= watchdog_expired;

        let outcome = if aggregate.forced_recovery {
            state.advance_shutdown(ShutdownPhase::ForcedRecovery);
            ShutdownOutcome::ForcedRecovery
        } else {
            ShutdownOutcome::Clean
        };
        Ok(ShutdownReport {
            outcome,
            expected_threads: aggregate.expected,
            joined_threads: aggregate.joined,
            elapsed: shutdown_started.elapsed(),
            grace,
            coordinator_metrics: self.coordinator_metrics.snapshot(),
            rpc_metrics,
        })
    }
}

fn join_scheduler(scheduler: SchedulerThread, grace: Duration) -> io::Result<ThreadJoinReport> {
    let (result, forced_recovery) = match scheduler.done.recv_timeout(grace) {
        Ok(result) => (result, false),
        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => (
            Err(io::Error::other(
                "daemon scheduler completion channel disconnected",
            )),
            false,
        ),
        Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
            super::handle_shutdown_grace_expiry("daemon coordinator");
            let result = scheduler.done.recv().map_err(|_| {
                io::Error::other("daemon scheduler completion channel disconnected")
            })?;
            (result, true)
        }
    };
    scheduler
        .handle
        .join()
        .map_err(|_| io::Error::other("bowline sync scheduler panicked"))?;
    let mut report = result?;
    report.expected += 1;
    report.joined += 1;
    report.forced_recovery |= forced_recovery;
    Ok(report)
}

fn rollback_startup(
    state: &DaemonServerState,
    connections: ConnectionExecutor,
    rpc_executor: &super::super::protocol_v2::RpcExecutor,
    scheduler: SchedulerThread,
) {
    state.begin_shutdown(ShutdownReason::StartupRollback);
    state.cancel_rpc_work();
    state.stop_background_work();
    let _connections = connections.shutdown_and_join(DEFAULT_SHUTDOWN_GRACE);
    let _scheduler = join_scheduler(scheduler, DEFAULT_SHUTDOWN_GRACE);
    let _rpc_workers = rpc_executor.shutdown_strict(DEFAULT_SHUTDOWN_GRACE);
}

impl Drop for DaemonThreads {
    fn drop(&mut self) {
        if self.acceptor.is_none() && self.connections.is_none() && self.scheduler.is_none() {
            return;
        }
        self.state.begin_shutdown(ShutdownReason::StartupRollback);
        if let Some(acceptor) = self.acceptor.take() {
            let _stop = acceptor.ensure_stopped();
            let _join = acceptor.join();
        }
        self.state.cancel_rpc_work();
        self.state.stop_background_work();
        if let Some(connections) = self.connections.take() {
            let _join = connections.shutdown_and_join(DEFAULT_SHUTDOWN_GRACE);
        }
        if let Some(scheduler) = self.scheduler.take() {
            let _join = join_scheduler(scheduler, DEFAULT_SHUTDOWN_GRACE);
        }
        let _join = self.rpc_executor.shutdown_strict(DEFAULT_SHUTDOWN_GRACE);
    }
}
