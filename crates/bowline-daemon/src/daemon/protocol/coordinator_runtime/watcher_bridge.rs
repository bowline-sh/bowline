use super::*;

pub(super) fn stop_and_join_watcher(
    runtime: &Arc<Mutex<DaemonRuntime>>,
    watcher_bridge: Option<WatcherBridge>,
) -> io::Result<()> {
    if let Ok(mut runtime) = runtime.lock()
        && let Some(sync) = runtime.sync.as_mut()
    {
        sync.watcher.take();
        sync.change_rx.take();
    }
    if let Some(watcher_bridge) = watcher_bridge {
        watcher_bridge.join()?;
    }
    Ok(())
}

pub(super) struct WatcherBridge {
    worker: Option<std::thread::JoinHandle<()>>,
    wake_state: WatcherWakeState,
    scope: DirtyScopeKey,
}

#[derive(Clone, Default)]
pub(super) struct WatcherWakeState {
    wake_pending: Arc<AtomicBool>,
    overflow_pending: Arc<AtomicBool>,
    delivery_failed: Arc<AtomicBool>,
}

impl WatcherWakeState {
    pub(super) fn record_overflow(&self) {
        self.overflow_pending.store(true, Ordering::Release);
    }

    pub(super) fn begin_wake(&self) -> bool {
        let acquired = self
            .wake_pending
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
        if acquired {
            self.delivery_failed.store(false, Ordering::Release);
        }
        acquired
    }

    pub(super) fn reset_for_dirty_ready(&self) -> bool {
        self.wake_pending.store(false, Ordering::Release);
        self.delivery_failed.store(false, Ordering::Release);
        self.overflow_pending.swap(false, Ordering::AcqRel)
    }

    pub(super) fn record_delivery_failure(&self) {
        self.delivery_failed.store(true, Ordering::Release);
    }

    pub(super) fn delivery_failed(&self) -> bool {
        self.delivery_failed.load(Ordering::Acquire)
    }

    pub(super) fn overflow_is_pending(&self) -> bool {
        self.overflow_pending.load(Ordering::Acquire)
    }
}

impl WatcherBridge {
    pub(super) fn start(
        runtime: &mut DaemonRuntime,
        coordinator: CoordinatorHandle,
    ) -> io::Result<Option<Self>> {
        let Some(sync) = runtime.sync.as_mut() else {
            return Ok(None);
        };
        let Some(source) = sync.change_rx.take() else {
            return Ok(None);
        };
        // Keep one forwarded signal buffered and apply backpressure until the
        // coordinator drains it. The notify-facing channel already owns the
        // bounded overflow contract; treating this internal handoff becoming
        // full as a second overflow threshold loses ordinary startup events.
        let (forward_tx, forward_rx) = mpsc::sync_channel(1);
        sync.change_rx = Some(forward_rx);
        let scope = DirtyScopeKey::new(sync.options.args.workspace_id.clone());
        let worker_scope = scope.clone();
        let wake_state = WatcherWakeState::default();
        let worker_wake = wake_state.clone();
        let worker = std::thread::Builder::new()
            .name("bowline-watcher-coordinator-wake".to_string())
            .spawn(move || {
                while let Ok(signal) = source.recv() {
                    let overflow = matches!(
                        &signal,
                        WatcherSignal::Changed(event) if event.need_rescan()
                    ) || matches!(&signal, WatcherSignal::Recoverable);
                    if !overflow && forward_tx.send(signal).is_err() {
                        break;
                    }
                    if overflow {
                        worker_wake.record_overflow();
                    }
                    if !worker_wake.begin_wake() {
                        continue;
                    }
                    let event = if worker_wake.overflow_is_pending() {
                        CoordinatorEvent::WatcherOverflow(worker_scope.clone())
                    } else {
                        CoordinatorEvent::FilesystemDirty(FilesystemDirty::one(
                            worker_scope.clone(),
                            DirtyPath::new("watcher-event"),
                        ))
                    };
                    if let Err(error) = coordinator.try_send(event) {
                        if error.kind == CoordinatorEventSendErrorKind::Disconnected {
                            break;
                        }
                        worker_wake.record_delivery_failure();
                    }
                }
            })?;
        Ok(Some(Self {
            worker: Some(worker),
            wake_state,
            scope,
        }))
    }

    pub(super) fn wake_state(&self) -> WatcherWakeState {
        self.wake_state.clone()
    }

    pub(super) fn scope(&self) -> DirtyScopeKey {
        self.scope.clone()
    }

    pub(super) fn join(mut self) -> io::Result<()> {
        self.worker
            .take()
            .expect("watcher bridge remains owned until strict join")
            .join()
            .map_err(|_| io::Error::other("watcher coordinator wake bridge panicked"))
    }
}

impl Drop for WatcherBridge {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            eprintln!("bowline-daemon watcher coordinator bridge panicked during ownership drop");
        }
    }
}
