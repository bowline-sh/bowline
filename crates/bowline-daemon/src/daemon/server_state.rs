use super::sync::{NotificationPollCompletion, PreparedStatusPublish, StatusPublishCompletion};
use super::*;

mod projection;

use projection::{
    ProjectionSourceHandles, device_trust_status_facts, projection_io_error, start_projection,
};

#[cfg(test)]
use bowline_daemon::status_projection::ProjectionBuildReason;
use bowline_daemon::status_projection::{
    DaemonInstanceId, DaemonStatusProjection, DeviceTrustStatusFacts, LatestProjectionReceiver,
    LocalStatusProjectionCollector, ProjectionServiceConfig, SafetyRefreshInterval,
    SharedStatusSourceCollector, SharedStatusSourceHandle, StatusInputEvent, StatusProjectionInput,
    StatusProjectionService, StatusSource, StatusSourceCollector, StatusSourceFacts,
    StatusSourceState, StatusSourceStateFacts,
};

use bowline_core::{
    commands::StatusCommandOutput,
    status::{
        StatusFact, StatusFactScope, StatusItem, StatusItemKind, StatusSubject, StatusSubjectKind,
        status_fact_policy,
    },
    wire::generated::DeviceApprovalAffordance,
};
use crossbeam_channel::Sender;
use std::sync::atomic::AtomicU8;

const STATUS_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);
const DEVICE_TRUST_REFRESH_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub(super) enum ShutdownPhase {
    Running = 0,
    StopAccepting = 1,
    CancelRpcWork = 2,
    StopDurableClaims = 3,
    FlushBookkeeping = 4,
    JoinThreads = 5,
    RemoveSocketState = 6,
    Complete = 7,
    ForcedRecovery = 8,
}

impl ShutdownPhase {
    fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Running,
            1 => Self::StopAccepting,
            2 => Self::CancelRpcWork,
            3 => Self::StopDurableClaims,
            4 => Self::FlushBookkeeping,
            5 => Self::JoinThreads,
            6 => Self::RemoveSocketState,
            7 => Self::Complete,
            _ => Self::ForcedRecovery,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::StopAccepting => "stop-accepting",
            Self::CancelRpcWork => "cancel-rpc-work",
            Self::StopDurableClaims => "stop-durable-claims",
            Self::FlushBookkeeping => "flush-bookkeeping",
            Self::JoinThreads => "join-threads",
            Self::RemoveSocketState => "remove-socket-state",
            Self::Complete => "complete",
            Self::ForcedRecovery => "forced-recovery",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ShutdownReason {
    ClientRequest,
    ServeOnceComplete,
    AcceptorFailed,
    StartupRollback,
}

impl ShutdownReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::ClientRequest => "client-request",
            Self::ServeOnceComplete => "serve-once-complete",
            Self::AcceptorFailed => "acceptor-failed",
            Self::StartupRollback => "startup-rollback",
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct CachedDaemonStatus {
    pub(super) instance_id: String,
    pub(super) sequence: u64,
    pub(super) status: StatusCommandOutput,
}

#[derive(Debug)]
struct SubscriptionPending {
    snapshot: Option<CachedDaemonStatus>,
    gap: bool,
    cancelled: bool,
}

#[derive(Debug)]
pub(super) struct StatusSubscription {
    pub(super) id: String,
    pending: Mutex<SubscriptionPending>,
    changed: Condvar,
    wake: Option<Sender<()>>,
}

impl StatusSubscription {
    fn new(id: String, wake: Option<Sender<()>>) -> Self {
        Self {
            id,
            pending: Mutex::new(SubscriptionPending {
                snapshot: None,
                gap: false,
                cancelled: false,
            }),
            changed: Condvar::new(),
            wake,
        }
    }

    pub(super) fn take_pending(&self) -> Option<(CachedDaemonStatus, bool)> {
        let mut pending = self.pending.lock().ok()?;
        let snapshot = pending.snapshot.take()?;
        let gap = std::mem::take(&mut pending.gap);
        Some((snapshot, gap))
    }

    pub(super) fn is_cancelled(&self) -> bool {
        self.pending
            .lock()
            .map(|pending| pending.cancelled)
            .unwrap_or(true)
    }

    fn publish(&self, snapshot: CachedDaemonStatus) {
        let Ok(mut pending) = self.pending.lock() else {
            return;
        };
        if pending.cancelled {
            return;
        }
        pending.gap |= pending.snapshot.is_some();
        pending.snapshot = Some(snapshot);
        self.changed.notify_one();
        drop(pending);
        if let Some(wake) = &self.wake {
            // One queued wake represents all coalesced latest-only projection updates.
            let _already_awake = wake.try_send(());
        }
    }

    fn cancel(&self) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.cancelled = true;
            pending.snapshot = None;
            self.changed.notify_all();
            drop(pending);
            if let Some(wake) = &self.wake {
                let _already_awake = wake.try_send(());
            }
        }
    }
}

pub(super) struct DaemonServerState {
    instance_id: String,
    projection: StatusProjectionService,
    projection_input: StatusProjectionInput,
    projection_updates: Mutex<LatestProjectionReceiver>,
    projection_heartbeats: Mutex<LatestProjectionReceiver>,
    initial_notification_projection: Mutex<Option<Arc<DaemonStatusProjection>>>,
    projection_sources: ProjectionSourceHandles,
    finder_snapshot_path: Option<PathBuf>,
    status: Mutex<CachedDaemonStatus>,
    subscriptions: Mutex<HashMap<String, Arc<StatusSubscription>>>,
    connection_wakes: Mutex<HashMap<u64, Sender<()>>>,
    acceptor_wake: Mutex<Option<super::protocol::acceptor::AcceptorWake>>,
    coordinator_wake: Mutex<Option<super::coordinator::CoordinatorHandle>>,
    coordinator_metrics: Mutex<Option<Arc<super::coordinator::CoordinatorMetrics>>>,
    rpc_executor: Mutex<Option<Weak<super::protocol_v2::RpcExecutor>>>,
    durable_work_wake_pending: AtomicBool,
    projection_wake_pending: Arc<AtomicBool>,
    next_subscription_id: AtomicU64,
    next_source_observation: Mutex<Instant>,
    next_device_trust_refresh: Mutex<Instant>,
    pub(super) shutting_down: AtomicBool,
    shutdown_phase: AtomicU8,
    shutdown_reason: Mutex<Option<ShutdownReason>>,
    pub(super) active_connections: AtomicUsize,
    connection_readers_started: AtomicUsize,
    connection_readers_joined: AtomicUsize,
    sync_options: Option<ContinuousSyncOptions>,
}

impl DaemonServerState {
    pub(super) fn new(runtime: &DaemonRuntime) -> io::Result<Self> {
        let instance_id = runtime
            .sync
            .as_ref()
            .map(|sync| sync.claimant_id.clone())
            .unwrap_or_else(|| {
                format!(
                    "daemon-{}-{}",
                    std::process::id(),
                    OffsetDateTime::now_utc().unix_timestamp_nanos()
                )
            });
        let (projection, projection_sources) = start_projection(runtime, &instance_id)?;
        let projection_input = projection.input();
        let projection_updates = projection.subscribe().map_err(projection_io_error)?;
        let projection_heartbeats = projection
            .subscribe_heartbeats()
            .map_err(projection_io_error)?;
        let initial = projection.current().map_err(projection_io_error)?;
        let state = Self {
            instance_id: instance_id.clone(),
            projection,
            projection_input,
            projection_updates: Mutex::new(projection_updates.updates),
            projection_heartbeats: Mutex::new(projection_heartbeats.deadlines),
            initial_notification_projection: Mutex::new(Some(Arc::clone(&initial))),
            projection_sources,
            finder_snapshot_path: super::finder_status::default_snapshot_path(),
            status: Mutex::new(CachedDaemonStatus {
                instance_id,
                sequence: initial.sequence.get(),
                status: initial.status.clone(),
            }),
            subscriptions: Mutex::new(HashMap::new()),
            connection_wakes: Mutex::new(HashMap::new()),
            acceptor_wake: Mutex::new(None),
            coordinator_wake: Mutex::new(None),
            coordinator_metrics: Mutex::new(None),
            rpc_executor: Mutex::new(None),
            durable_work_wake_pending: AtomicBool::new(false),
            projection_wake_pending: Arc::new(AtomicBool::new(false)),
            next_subscription_id: AtomicU64::new(1),
            next_source_observation: Mutex::new(Instant::now()),
            next_device_trust_refresh: Mutex::new(Instant::now()),
            shutting_down: AtomicBool::new(false),
            shutdown_phase: AtomicU8::new(ShutdownPhase::Running as u8),
            shutdown_reason: Mutex::new(None),
            active_connections: AtomicUsize::new(0),
            connection_readers_started: AtomicUsize::new(0),
            connection_readers_joined: AtomicUsize::new(0),
            sync_options: runtime.sync.as_ref().map(|sync| sync.options.clone()),
        };
        state.projection_input.record_rpc_serialization();
        state.publish_finder_projection(&initial);
        Ok(state)
    }

    pub(super) fn instance_id(&self) -> &str {
        &self.instance_id
    }

    #[cfg(test)]
    pub(super) fn current_projection(&self) -> Arc<DaemonStatusProjection> {
        self.projection
            .current()
            .expect("test projection remains available")
    }

    #[cfg(test)]
    pub(super) fn test_projection_input(&self) -> StatusProjectionInput {
        self.projection_input.clone()
    }

    #[cfg(test)]
    pub(super) fn test_projection_metrics(
        &self,
    ) -> bowline_daemon::status_projection::StatusProjectionMetrics {
        self.projection.metrics().expect("test projection metrics")
    }

    pub(super) fn snapshot(&self) -> Option<CachedDaemonStatus> {
        self.status.lock().ok().map(|status| status.clone())
    }

    pub(super) fn register_runtime_metrics(
        &self,
        coordinator: Arc<super::coordinator::CoordinatorMetrics>,
        rpc_executor: Weak<super::protocol_v2::RpcExecutor>,
    ) {
        if let Ok(mut metrics) = self.coordinator_metrics.lock() {
            *metrics = Some(coordinator);
        }
        if let Ok(mut executor) = self.rpc_executor.lock() {
            *executor = Some(rpc_executor);
        }
    }

    pub(super) fn record_connection_reader_started(&self) {
        self.connection_readers_started
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn record_connection_reader_joined(&self) {
        self.connection_readers_joined
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn connection_reader_thread_counts(&self) -> (usize, usize) {
        (
            self.connection_readers_started.load(Ordering::Relaxed),
            self.connection_readers_joined.load(Ordering::Relaxed),
        )
    }

    pub(super) fn runtime_metrics(&self) -> serde_json::Value {
        let coordinator = self
            .coordinator_metrics
            .lock()
            .ok()
            .and_then(|metrics| metrics.as_ref().map(|metrics| metrics.snapshot().to_json()));
        let rpc = self
            .rpc_executor
            .lock()
            .ok()
            .and_then(|executor| executor.as_ref().and_then(Weak::upgrade))
            .map(|executor| executor.metrics().to_json());
        serde_json::json!({
            "coordinator": coordinator,
            "rpc": rpc,
            "shutdown": {
                "phase": self.shutdown_phase().as_str(),
                "reason": self.shutdown_reason().map(ShutdownReason::as_str),
            },
        })
    }

    pub(super) fn subscribe_with_snapshot(
        &self,
        wake: Option<Sender<()>>,
    ) -> Option<(Arc<StatusSubscription>, CachedDaemonStatus)> {
        // Registration owns the subscriber map while it captures status. A
        // publisher either lands before this snapshot or waits and delivers
        // after registration, so setup cannot lose a one-off transition.
        let mut subscriptions = self.subscriptions.lock().ok()?;
        let snapshot = self.status.lock().ok()?.clone();
        let sequence = self.next_subscription_id.fetch_add(1, Ordering::Relaxed);
        let id = format!("subscription-{}-{sequence}", std::process::id());
        let subscription = Arc::new(StatusSubscription::new(id.clone(), wake));
        subscriptions.insert(id, Arc::clone(&subscription));
        Some((subscription, snapshot))
    }

    pub(super) fn cancel_subscription(&self, id: &str) -> bool {
        let subscription = self
            .subscriptions
            .lock()
            .ok()
            .and_then(|mut subscriptions| subscriptions.remove(id));
        if let Some(subscription) = subscription {
            subscription.cancel();
            true
        } else {
            false
        }
    }

    pub(super) fn register_connection_wake(&self, connection_id: u64, wake: Sender<()>) {
        if let Ok(mut wakes) = self.connection_wakes.lock() {
            wakes.insert(connection_id, wake);
        }
    }

    pub(super) fn unregister_connection_wake(&self, connection_id: u64) {
        if let Ok(mut wakes) = self.connection_wakes.lock() {
            wakes.remove(&connection_id);
        }
    }

    pub(super) fn register_coordinator_wake(&self, wake: super::coordinator::CoordinatorHandle) {
        let projection_pending = Arc::clone(&self.projection_wake_pending);
        let projection_wake = wake.clone();
        let callback: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(move || {
            if projection_pending.swap(true, Ordering::AcqRel) {
                return;
            }
            let _already_awake =
                projection_wake.try_send(super::coordinator::CoordinatorEvent::ProjectionReady);
        });
        if let Ok(updates) = self.projection_updates.lock() {
            updates.set_wake(Some(Arc::clone(&callback)));
        }
        if let Ok(heartbeats) = self.projection_heartbeats.lock() {
            heartbeats.set_wake(Some(callback));
        }
        if let Ok(mut coordinator_wake) = self.coordinator_wake.lock() {
            *coordinator_wake = Some(wake);
        }
    }

    pub(super) fn unregister_coordinator_wake(&self) {
        if let Ok(updates) = self.projection_updates.lock() {
            updates.set_wake(None);
        }
        if let Ok(heartbeats) = self.projection_heartbeats.lock() {
            heartbeats.set_wake(None);
        }
        if let Ok(mut coordinator_wake) = self.coordinator_wake.lock() {
            *coordinator_wake = None;
        }
    }

    pub(super) fn wake_durable_work(&self) {
        if self.durable_work_wake_pending.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Ok(wake) = self.coordinator_wake.lock()
            && let Some(wake) = wake.as_ref()
        {
            let _already_awake =
                wake.try_send(super::coordinator::CoordinatorEvent::DurableWorkAvailable);
        }
    }

    pub(super) fn take_durable_work_wake(&self) -> bool {
        self.durable_work_wake_pending.swap(false, Ordering::AcqRel)
    }

    pub(super) fn take_projection_wake(&self) -> bool {
        self.projection_wake_pending.swap(false, Ordering::AcqRel)
    }

    pub(super) fn heartbeat_interval(&self) -> Duration {
        STATUS_HEARTBEAT_INTERVAL
    }

    pub(super) fn sync_identity(&self) -> Option<(WorkspaceId, DeviceId)> {
        self.sync_options.as_ref().map(|options| {
            (
                options.args.workspace_id(),
                DeviceId::new(options.args.device_id.clone()),
            )
        })
    }

    pub(super) fn refresh_device_trust_if_due(&self) {
        if self.cancels_side_work() {
            return;
        }
        let now = Instant::now();
        let due = self
            .next_device_trust_refresh
            .lock()
            .map(|mut next_refresh| {
                if now < *next_refresh {
                    return false;
                }
                *next_refresh = now + DEVICE_TRUST_REFRESH_INTERVAL;
                true
            })
            .unwrap_or(false);
        if !due {
            return;
        }
        if self.sync_options.is_none() {
            return;
        }
        let result = self.fetch_device_trust();
        if self.cancels_side_work() {
            return;
        }
        match result {
            Ok(trust) => {
                let facts = device_trust_status_facts(
                    &trust,
                    self.sync_options
                        .as_ref()
                        .map(|options| options.args.root.as_path()),
                );
                self.update_projection_source(
                    &self.projection_sources.device_trust,
                    StatusSourceFacts::DeviceTrustDetails(facts),
                );
            }
            Err(()) => self.mark_device_trust_degraded(),
        }
    }

    fn fetch_device_trust(&self) -> Result<DeviceApprovalRequestList, ()> {
        let (workspace_id, device_id) = self.sync_identity().ok_or(())?;
        let key_store = key_store().map_err(|_| ())?;
        let control_plane =
            hosted_control_plane(&*key_store, workspace_id.clone(), device_id).map_err(|_| ())?;
        control_plane
            .list_device_trust(&workspace_id)
            .map_err(|_| ())
    }

    fn mark_device_trust_degraded(&self) {
        let Some(current) = self.projection_sources.device_trust.current() else {
            return;
        };
        let degraded = match current {
            StatusSourceFacts::DeviceTrust(mut facts) => {
                facts.state = StatusSourceState::Degraded;
                StatusSourceFacts::DeviceTrust(facts)
            }
            StatusSourceFacts::DeviceTrustDetails(mut facts) => {
                facts.state.state = StatusSourceState::Degraded;
                StatusSourceFacts::DeviceTrustDetails(facts)
            }
            _ => return,
        };
        self.update_projection_source(&self.projection_sources.device_trust, degraded);
    }

    pub(super) fn scope_matches(
        &self,
        workspace_root: Option<&str>,
        project_path: Option<&str>,
    ) -> bool {
        let Some(options) = self.sync_options.as_ref() else {
            return workspace_root.is_none() && project_path.is_none();
        };
        if project_path.is_some() {
            return false;
        }
        workspace_root.is_none_or(|root| requested_root_matches(root, &options.args.root))
    }

    pub(super) fn request_shutdown(&self) {
        self.begin_shutdown(ShutdownReason::ClientRequest);
    }

    pub(super) fn begin_shutdown(&self, reason: ShutdownReason) {
        self.advance_shutdown(ShutdownPhase::StopAccepting);
        if let Ok(mut recorded) = self.shutdown_reason.lock() {
            recorded.get_or_insert(reason);
        }
        if let Ok(wake) = self.acceptor_wake.lock()
            && let Some(wake) = wake.as_ref()
            && let Err(error) = wake.stop()
        {
            eprintln!("bowline-daemon could not wake RPC acceptor for shutdown: {error}");
        }
    }

    pub(super) fn register_acceptor_wake(&self, wake: super::protocol::acceptor::AcceptorWake) {
        if let Ok(mut acceptor_wake) = self.acceptor_wake.lock() {
            *acceptor_wake = Some(wake);
        }
    }

    pub(super) fn shutdown_phase(&self) -> ShutdownPhase {
        ShutdownPhase::from_u8(self.shutdown_phase.load(Ordering::Acquire))
    }

    pub(super) fn shutdown_reason(&self) -> Option<ShutdownReason> {
        self.shutdown_reason.lock().ok().and_then(|reason| *reason)
    }

    pub(super) fn advance_shutdown(&self, phase: ShutdownPhase) {
        self.shutdown_phase.fetch_max(phase as u8, Ordering::AcqRel);
        if phase >= ShutdownPhase::StopAccepting {
            self.shutting_down.store(true, Ordering::Release);
        }
    }

    pub(super) fn accepts_mutations(&self) -> bool {
        self.shutdown_phase() == ShutdownPhase::Running
    }

    pub(super) fn cancels_side_work(&self) -> bool {
        self.shutdown_phase() >= ShutdownPhase::CancelRpcWork
    }

    pub(super) fn should_stop_connections(&self) -> bool {
        self.shutdown_phase() >= ShutdownPhase::CancelRpcWork
    }

    pub(super) fn should_stop_durable_claims(&self) -> bool {
        self.shutdown_phase() >= ShutdownPhase::StopDurableClaims
    }

    pub(super) fn cancel_rpc_work(&self) {
        self.advance_shutdown(ShutdownPhase::CancelRpcWork);
        if let Ok(wakes) = self.connection_wakes.lock() {
            for wake in wakes.values() {
                let _already_awake = wake.try_send(());
            }
        }
    }

    pub(super) fn stop_durable_claims(&self) {
        self.advance_shutdown(ShutdownPhase::StopDurableClaims);
        if let Ok(wake) = self.coordinator_wake.lock()
            && let Some(wake) = wake.as_ref()
        {
            let _already_awake = wake.try_send(super::coordinator::CoordinatorEvent::Shutdown);
        }
        let Some(options) = self.sync_options.as_ref() else {
            return;
        };
        let cancellation = MetadataStore::open(options.args.state_root.join(DEFAULT_DATABASE_FILE))
            .and_then(|store| {
                let workspace_id = options.args.workspace_id();
                let device_id = DeviceId::new(options.args.device_id.clone());
                let operations = store.sync_operations(&workspace_id)?;
                let now = current_timestamp();
                for operation in operations.into_iter().filter(|operation| {
                    operation.state == SyncOperationState::Claimed
                        && operation.claimed_by.as_deref() == Some(self.instance_id.as_str())
                }) {
                    store.request_sync_operation_cancellation(&operation.id, &now)?;
                }
                store.request_claimed_work_view_accept_cancellations(
                    &workspace_id,
                    &device_id,
                    self.instance_id.as_str(),
                    &now,
                )?;
                Ok(())
            });
        if let Err(error) = cancellation {
            eprintln!("bowline-daemon could not request active sync cancellation: {error}");
        }
    }

    pub(super) fn enqueue_sync(
        &self,
        idempotency_key: &str,
    ) -> Result<(SyncOperationRecord, bool), MetadataError> {
        let options = self.sync_options.as_ref().ok_or_else(|| {
            MetadataError::InvalidStorageMetadata("continuous sync is not configured".to_string())
        })?;
        let workspace_id = options.args.workspace_id();
        let store = MetadataStore::open(options.args.state_root.join(DEFAULT_DATABASE_FILE))?;
        let now = current_timestamp();
        let token = stable_token(&format!(
            "rpc-sync:{}:{}:{idempotency_key}",
            workspace_id.as_str(),
            options.args.device_id
        ));
        let operation = SyncOperationRecord {
            id: format!("rpc-sync-{token}"),
            workspace_id,
            kind: SyncOperationKind::Reconcile,
            resource_key: SyncResourceKey::workspace_sync(options.args.workspace_id()),
            state: SyncOperationState::Queued,
            idempotency_key: idempotency_key.to_string(),
            base_version: None,
            base_snapshot_id: None,
            target_snapshot_id: None,
            device_id: Some(DeviceId::new(options.args.device_id.clone())),
            payload_json: serde_json::json!({
                "root": options.args.root,
                "stateRoot": options.args.state_root,
                "source": "daemon-rpc-v2"
            })
            .to_string(),
            attempt_count: 0,
            claimed_by: None,
            claim_generation: 0,
            heartbeat_at: None,
            lease_expires_at: None,
            cancellation_requested_at: None,
            next_attempt_at: None,
            result_json: None,
            last_error_code: None,
            last_error: None,
            created_at: now.clone(),
            updated_at: now,
        };
        let outcome = match store.enqueue_sync_operation(&operation)? {
            SyncOperationEnqueueOutcome::Inserted(operation) => (operation, false),
            SyncOperationEnqueueOutcome::Existing(operation) => (operation, true),
        };
        self.wake_durable_work();
        Ok(outcome)
    }

    pub(super) fn sync_operation(
        &self,
        operation_id: &str,
    ) -> Result<Option<SyncOperationRecord>, MetadataError> {
        let Some(options) = self.sync_options.as_ref() else {
            return Ok(None);
        };
        MetadataStore::open(options.args.state_root.join(DEFAULT_DATABASE_FILE))?
            .sync_operation_by_id(operation_id)
    }

    pub(super) fn cancel_sync_operation(
        &self,
        operation_id: &str,
    ) -> Result<Option<(SyncCancellationOutcome, SyncOperationRecord)>, MetadataError> {
        let Some(options) = self.sync_options.as_ref() else {
            return Ok(None);
        };
        let store = MetadataStore::open(options.args.state_root.join(DEFAULT_DATABASE_FILE))?;
        let Some(outcome) =
            store.request_sync_operation_cancellation(operation_id, &current_timestamp())?
        else {
            return Ok(None);
        };
        Ok(store
            .sync_operation_by_id(operation_id)?
            .map(|operation| (outcome, operation)))
    }
}

fn requested_root_matches(requested: &str, configured: &Path) -> bool {
    if Path::new(requested) == configured {
        return true;
    }
    let Some(relative) = requested.strip_prefix("~/") else {
        return false;
    };
    env::var_os("HOME")
        .map(PathBuf::from)
        .is_some_and(|home| home.join(relative) == configured)
}

#[cfg(test)]
mod tests;
