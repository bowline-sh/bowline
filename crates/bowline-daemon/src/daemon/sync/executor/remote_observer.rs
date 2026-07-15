use super::*;

fn stream_remote_workspace_refs_with_context(
    args: SyncOnceArgs,
    resolver: HostedContextResolver,
    thread_metrics: Arc<OwnedThreadMetrics>,
) -> Result<RemoteRefStream, Box<dyn std::error::Error>> {
    let workspace_id = WorkspaceId::new(args.workspace_id.clone());
    let (sender, receiver) = mpsc::channel();
    let (shutdown, shutdown_rx) = bowline_control_plane::workspace_ref_stream_shutdown_pair();
    let worker = std::thread::Builder::new()
        .name("bowline-remote-ref-observer".to_string())
        .spawn(move || {
            let result = resolver(&args).and_then(|hosted| {
                hosted
                    .client
                    .stream_workspace_ref_updates_until(
                        workspace_id.as_str(),
                        sender.clone(),
                        shutdown_rx,
                    )
                    .map_err(|error| Box::new(error) as Box<dyn std::error::Error>)
            });
            if let Err(error) = result {
                let _ = sender.send(Err(ControlPlaneError::Storage(error.to_string())));
            }
        })?;
    thread_metrics.record_started();
    Ok(RemoteRefStream::owned(
        receiver,
        shutdown,
        worker,
        thread_metrics,
    ))
}

pub(in crate::daemon) struct RemoteRefStream {
    receiver: Receiver<bowline_control_plane::ControlPlaneResult<Option<WorkspaceRef>>>,
    _owner: Option<RemoteRefStreamOwner>,
}

struct RemoteRefStreamOwner {
    shutdown: Option<bowline_control_plane::WorkspaceRefStreamShutdown>,
    worker: Option<std::thread::JoinHandle<()>>,
    thread_metrics: Arc<OwnedThreadMetrics>,
}

impl RemoteRefStream {
    fn owned(
        receiver: Receiver<bowline_control_plane::ControlPlaneResult<Option<WorkspaceRef>>>,
        shutdown: bowline_control_plane::WorkspaceRefStreamShutdown,
        worker: std::thread::JoinHandle<()>,
        thread_metrics: Arc<OwnedThreadMetrics>,
    ) -> Self {
        Self {
            receiver,
            _owner: Some(RemoteRefStreamOwner {
                shutdown: Some(shutdown),
                worker: Some(worker),
                thread_metrics,
            }),
        }
    }

    fn try_recv(
        &self,
    ) -> Result<bowline_control_plane::ControlPlaneResult<Option<WorkspaceRef>>, mpsc::TryRecvError>
    {
        self.receiver.try_recv()
    }
}

impl From<Receiver<bowline_control_plane::ControlPlaneResult<Option<WorkspaceRef>>>>
    for RemoteRefStream
{
    fn from(
        receiver: Receiver<bowline_control_plane::ControlPlaneResult<Option<WorkspaceRef>>>,
    ) -> Self {
        Self {
            receiver,
            _owner: None,
        }
    }
}

impl Drop for RemoteRefStreamOwner {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            drop(shutdown);
        }
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() {
                eprintln!("bowline-daemon remote ref observer panicked during strict join");
            }
            self.thread_metrics.record_joined();
        }
    }
}

pub(in crate::daemon) fn hosted_remote_ref_observer_with_context(
    resolver: HostedContextResolver,
) -> RemoteRefObserver {
    let thread_metrics = Arc::new(OwnedThreadMetrics::default());
    let starter_metrics = Arc::clone(&thread_metrics);
    remote_ref_observer_with_stream_starter_and_refresh(
        Box::new(move |args| {
            stream_remote_workspace_refs_with_context(
                args,
                resolver.clone(),
                Arc::clone(&starter_metrics),
            )
        }),
        HOSTED_CONTEXT_TRUST_REFRESH_INTERVAL,
        thread_metrics,
    )
}

#[cfg(test)]
pub(super) type HostedObserverOperation = Arc<
    dyn Fn(
            HostedContextResolver,
            SyncOnceArgs,
        ) -> Result<
            Receiver<bowline_control_plane::ControlPlaneResult<Option<WorkspaceRef>>>,
            Box<dyn std::error::Error>,
        > + Send
        + Sync,
>;

#[cfg(test)]
pub(super) fn hosted_remote_ref_observer_with_operations_and_refresh(
    resolver: HostedContextResolver,
    operation: HostedObserverOperation,
    refresh_interval: Duration,
) -> RemoteRefObserver {
    remote_ref_observer_with_stream_starter_and_refresh(
        Box::new(move |args| operation(resolver.clone(), args).map(RemoteRefStream::from)),
        refresh_interval,
        Arc::new(OwnedThreadMetrics::default()),
    )
}

pub(in crate::daemon) type RemoteRefStreamStarter = Box<
    dyn FnMut(SyncOnceArgs) -> Result<RemoteRefStream, Box<dyn std::error::Error>> + Send + 'static,
>;

#[cfg(test)]
pub(in crate::daemon) fn remote_ref_observer_with_stream_starter(
    start_stream: RemoteRefStreamStarter,
) -> RemoteRefObserver {
    remote_ref_observer_with_stream_starter_and_refresh(
        start_stream,
        HOSTED_CONTEXT_TRUST_REFRESH_INTERVAL,
        Arc::new(OwnedThreadMetrics::default()),
    )
}

pub(super) fn remote_ref_observer_with_stream_starter_and_refresh(
    mut start_stream: RemoteRefStreamStarter,
    refresh_interval: Duration,
    thread_metrics: Arc<OwnedThreadMetrics>,
) -> RemoteRefObserver {
    let mut receiver = None;
    let mut latest = None;
    let mut reconnect_failure_count = 0_u32;
    let mut next_reconnect_attempt = Instant::now();
    let mut last_error = None::<String>;
    let mut ready = false;
    let mut refresh_at = Instant::now() + refresh_interval;
    let observe = Box::new(move |args: SyncOnceArgs| {
        let now = Instant::now();
        let refresh_due = receiver.is_some() && now >= refresh_at;
        if receiver.is_none() {
            if now < next_reconnect_attempt {
                let reason = last_error
                    .clone()
                    .unwrap_or_else(|| "remote ref observer is reconnecting".to_string());
                return Err(runtime_error(format!(
                    "remote ref observer reconnecting after failure: {reason}"
                )));
            }
            match start_stream(args.clone()) {
                Ok(stream) => {
                    receiver = Some(stream);
                    ready = false;
                    refresh_at = now + refresh_interval;
                }
                Err(error) => {
                    reconnect_failure_count = reconnect_failure_count.saturating_add(1);
                    next_reconnect_attempt =
                        now + remote_observer_reconnect_delay(reconnect_failure_count);
                    last_error = Some(error.to_string());
                    return Err(error);
                }
            }
        }
        let mut disconnected = false;
        let mut observer_error = None;
        if let Some(receiver) = &receiver {
            loop {
                match receiver.try_recv() {
                    Ok(Ok(Some(workspace_ref))) => {
                        latest = Some(workspace_ref);
                        ready = true;
                        reconnect_failure_count = 0;
                        last_error = None;
                    }
                    Ok(Ok(None)) => {
                        latest = None;
                        ready = true;
                        reconnect_failure_count = 0;
                        last_error = None;
                    }
                    Ok(Err(error)) => {
                        observer_error = Some(error);
                        break;
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }
        if let Some(error) = observer_error {
            reconnect_failure_count = reconnect_failure_count.saturating_add(1);
            next_reconnect_attempt =
                Instant::now() + remote_observer_reconnect_delay(reconnect_failure_count);
            last_error = Some(error.to_string());
            receiver = None;
            latest = None;
            ready = false;
            return Err(Box::new(error));
        }
        if disconnected {
            reconnect_failure_count = reconnect_failure_count.saturating_add(1);
            next_reconnect_attempt =
                Instant::now() + remote_observer_reconnect_delay(reconnect_failure_count);
            let error = runtime_error("remote ref subscription disconnected");
            last_error = Some(error.to_string());
            receiver = None;
            latest = None;
            ready = false;
            return Err(error);
        }
        if refresh_due && now >= next_reconnect_attempt {
            match start_stream(args) {
                Ok(stream) => {
                    receiver = Some(stream);
                    latest = None;
                    ready = false;
                    refresh_at = now + refresh_interval;
                    reconnect_failure_count = 0;
                    last_error = None;
                }
                Err(error) => {
                    reconnect_failure_count = reconnect_failure_count.saturating_add(1);
                    next_reconnect_attempt =
                        now + remote_observer_reconnect_delay(reconnect_failure_count);
                    refresh_at = next_reconnect_attempt;
                    last_error = Some(error.to_string());
                    return Ok(latest.clone());
                }
            }
        }
        if !ready {
            return Err(runtime_error("remote ref observer is connecting"));
        }
        Ok(latest.clone())
    });
    RemoteRefObserver::new(observe, thread_metrics)
}

pub(in crate::daemon) fn remote_observer_reconnect_delay(failure_count: u32) -> Duration {
    let exponent = failure_count.saturating_sub(1).min(6);
    let multiplier = 1_u64 << exponent;
    let delay_seconds = REMOTE_OBSERVER_RECONNECT_INITIAL
        .as_secs()
        .saturating_mul(multiplier)
        .min(REMOTE_OBSERVER_RECONNECT_MAX.as_secs());
    Duration::from_secs(delay_seconds)
}
