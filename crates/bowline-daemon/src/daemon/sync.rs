use super::store_health::StoreHealth;
use super::*;

#[cfg(test)]
mod claim_lease;
mod component_state;
mod conflict_runtime;
mod dirty_batch;
mod dirty_scope;
mod durable_runtime;
mod executor;
mod failure;
mod notification_status;
mod operations;
mod overlay_runtime;
mod policy_cache;
mod remote_observer;
mod scan_summary;
mod scheduler_poll;
mod scheduler_work;
mod status_json;
mod status_publish;
mod work_accept_runtime;

#[cfg(test)]
pub(super) use claim_lease::{ClaimLeasePolicy, ClaimLeaseSupervisor, ClaimOwnership};
pub(super) use component_state::SyncComponentState;
pub(in crate::daemon) use dirty_batch::PendingDirtyRoots;
pub(in crate::daemon) use dirty_scope::{DaemonReconcileRequest, DirtyScope, RootEntryKind};
#[cfg(test)]
pub(in crate::daemon) use durable_runtime::local_metadata_sweep_due;
use durable_runtime::{
    forced_full_reason_survives_retry, local_conflict_state, validate_conflict_operation,
};
pub(in crate::daemon) use executor::*;
pub(in crate::daemon) use failure::*;
pub(in crate::daemon) use operations::*;
pub(in crate::daemon) use policy_cache::{drain_policy, invalidate_policy_cache_for_path};
pub(in crate::daemon) use scan_summary::SyncScanSummary;
pub(in crate::daemon) use scheduler_work::*;
use status_json::*;

// Local metadata is append-heavy, but pruning on every filesystem tick would
// fight the hot path. Derive an hourly cadence from the configured sync loop.
const LOCAL_METADATA_SWEEP_SECONDS: u64 = 60 * 60;
pub(in crate::daemon) const MAX_SYNC_RETRY_ATTEMPTS: u32 = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SyncOnceArgs {
    pub(super) root: PathBuf,
    pub(super) state_root: PathBuf,
    pub(super) workspace_id: String,
    pub(super) device_id: String,
    pub(super) sync_claim: Option<SyncClaimHandle>,
    pub(super) scan_scope: ScanScope,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ContinuousSyncOptions {
    pub(super) args: SyncOnceArgs,
    pub(super) interval: Duration,
    pub(super) max_ticks: Option<u64>,
}

pub(super) struct DaemonRuntime {
    pub(super) sync: Option<ContinuousSyncRuntime>,
    pub(super) notify_approvals: bool,
    pub(super) notification_dedupe: Arc<Mutex<NotificationDedupe>>,
    pub(super) next_notification_poll: Instant,
    pub(super) pending_notification_status: Option<bowline_core::commands::StatusCommandOutput>,
}

pub(super) struct PreparedNotificationPoll {
    status: bowline_core::commands::StatusCommandOutput,
    dedupe: Arc<Mutex<NotificationDedupe>>,
}

pub(super) struct NotificationPollCompletion {
    status: bowline_core::commands::StatusCommandOutput,
    result: Result<NotificationDispatchReport, String>,
}

pub(super) struct PreparedStatusPublish {
    payload: StatusPublishPayload,
    published_at: Instant,
    publisher: StatusPublisher,
}

pub(super) struct StatusPublishCompletion {
    published_at: Instant,
    result: Result<StatusPublishOutcome, String>,
}

pub(super) struct ContinuousSyncRuntime {
    pub(super) options: ContinuousSyncOptions,
    pub(super) next_tick: Instant,
    pub(super) next_remote_observe: Instant,
    pub(super) next_dispatch_claim: Instant,
    pub(super) awaiting_handoff: bool,
    pub(super) tick_count: u64,
    pub(super) last_json: String,
    pub(super) watcher: Option<RecommendedWatcher>,
    pub(super) change_rx: Option<Receiver<WatcherSignal>>,
    pub(super) watcher_state: WatcherRuntimeState,
    pub(super) watcher_recovery: WatcherRecovery,
    pub(super) sync_once: SyncExecutor,
    pub(super) remote_ref_observer: RemoteRefObserver,
    pub(super) dispatch_claimer: DispatchClaimer,
    pub(super) latest_observed_ref: Option<WorkspaceRef>,
    pub(super) remote_observer_state: RemoteObserverState,
    pub(super) status_publisher: StatusPublisher,
    pub(super) next_status_publish: Instant,
    pub(super) last_status_publish_fingerprint: Option<String>,
    pub(super) last_status_publish_at: Option<Instant>,
    pub(super) last_status_publish_failed_at: Option<Instant>,
    pub(super) hosted_resolver: HostedContextResolver,
    pub(super) store_health: StoreHealth,
    pub(super) claimant_id: String,
    pub(super) store: CachedStore,
    pub(super) pending_dirty: DirtyScope,
    // Roots deferred from a bounded dirty batch, carried across ticks with
    // fairness bookkeeping so cost-first scheduling cannot starve a large root.
    pub(super) pending_dirty_roots: PendingDirtyRoots,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RemoteObserverState {
    Ready,
    Unavailable,
}

pub(super) type SyncExecutor = Box<
    dyn FnMut(SyncOnceArgs, Option<WorkspaceRef>) -> Result<SyncOnceSummary, SyncOnceError>
        + Send
        + 'static,
>;
type RemoteRefObserverFn = dyn FnMut(SyncOnceArgs) -> Result<Option<WorkspaceRef>, Box<dyn std::error::Error>>
    + Send
    + 'static;

#[derive(Default)]
pub(in crate::daemon) struct OwnedThreadMetrics {
    started: AtomicUsize,
    joined: AtomicUsize,
}

impl OwnedThreadMetrics {
    pub(in crate::daemon) fn record_started(&self) {
        self.started.fetch_add(1, Ordering::Relaxed);
    }

    pub(in crate::daemon) fn record_joined(&self) {
        self.joined.fetch_add(1, Ordering::Relaxed);
    }

    fn snapshot(&self) -> (usize, usize) {
        (
            self.started.load(Ordering::Relaxed),
            self.joined.load(Ordering::Relaxed),
        )
    }
}

pub(super) struct RemoteRefObserver {
    observe: Box<RemoteRefObserverFn>,
    thread_metrics: Arc<OwnedThreadMetrics>,
}

impl RemoteRefObserver {
    pub(in crate::daemon) fn new(
        observe: Box<RemoteRefObserverFn>,
        thread_metrics: Arc<OwnedThreadMetrics>,
    ) -> Self {
        Self {
            observe,
            thread_metrics,
        }
    }

    pub(super) fn observe(
        &mut self,
        args: SyncOnceArgs,
    ) -> Result<Option<WorkspaceRef>, Box<dyn std::error::Error>> {
        (self.observe)(args)
    }

    pub(super) fn shutdown_and_report(&mut self) -> (usize, usize) {
        self.observe = Box::new(|_| Ok(None));
        self.thread_metrics.snapshot()
    }
}
pub(super) type DispatchClaimer = Box<
    dyn FnMut(SyncOnceArgs) -> Result<Option<Lease>, Box<dyn std::error::Error>> + Send + 'static,
>;

const DISPATCH_CLAIM_IDLE_INTERVAL: Duration = Duration::from_secs(30);
pub(super) struct SyncOnceSummary {
    pub(super) workspace_id: String,
    pub(super) snapshot_id: String,
    pub(super) version: u64,
    pub(super) outcome: SyncSummaryOutcome,
    pub(super) snapshot_root_manifest_id: Option<String>,
    pub(super) manifest_object_key: Option<String>,
    pub(super) namespace_root_id: Option<String>,
    pub(super) conflict_count: usize,
    pub(super) conflicts: Vec<ConflictSummary>,
    pub(super) scan: SyncScanSummary,
    pub(super) cancelled_late: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SyncSummaryOutcome {
    NoWorkspaceRef,
    NoChanges,
    Imported,
    Uploaded { stale: bool },
    Merged { stale: bool },
    Conflicted,
}

pub(super) struct ConflictSummary {
    pub(super) id: String,
    pub(super) paths: Vec<String>,
}

impl DaemonRuntime {
    pub(super) fn prepare_projection_status(
        &mut self,
        projection: &bowline_daemon::status_projection::DaemonStatusProjection,
        heartbeat: bool,
        now: Instant,
        projection_input: &bowline_daemon::status_projection::StatusProjectionInput,
    ) -> Option<PreparedStatusPublish> {
        self.sync
            .as_mut()?
            .prepare_projection_status(projection, heartbeat, now, projection_input)
    }

    pub(super) fn prepare_projection_status_retry_if_due(
        &mut self,
        projection: &bowline_daemon::status_projection::DaemonStatusProjection,
        now: Instant,
        projection_input: &bowline_daemon::status_projection::StatusProjectionInput,
    ) -> Option<PreparedStatusPublish> {
        self.sync.as_mut()?.prepare_projection_status_retry_if_due(
            projection,
            now,
            projection_input,
        )
    }

    pub(super) fn complete_status_publish(
        &mut self,
        completion: StatusPublishCompletion,
        projection_input: &bowline_daemon::status_projection::StatusProjectionInput,
    ) {
        if let Some(sync) = self.sync.as_mut() {
            sync.complete_status_publish(completion, projection_input);
        }
    }

    pub(super) fn prepare_notification_poll(&mut self) -> Option<PreparedNotificationPoll> {
        if !self.notify_approvals {
            return None;
        }
        let now = Instant::now();
        if now < self.next_notification_poll {
            return None;
        }
        let status = self.pending_notification_status.as_ref()?.clone();
        self.next_notification_poll = now + NOTIFICATION_POLL_INTERVAL;
        Some(PreparedNotificationPoll {
            status,
            dedupe: Arc::clone(&self.notification_dedupe),
        })
    }
}

impl PreparedNotificationPoll {
    #[cfg(test)]
    pub(super) fn execute(self) -> NotificationPollCompletion {
        self.execute_cancellable(|| true)
    }

    pub(super) fn execute_cancellable(
        self,
        checkpoint: impl FnMut() -> bool,
    ) -> NotificationPollCompletion {
        let sender = DesktopNotificationSender;
        let result = self.execute_with(&sender, checkpoint);
        NotificationPollCompletion {
            status: self.status,
            result,
        }
    }

    fn execute_with<S>(
        &self,
        sender: &S,
        checkpoint: impl FnMut() -> bool,
    ) -> Result<NotificationDispatchReport, String>
    where
        S: NotificationSender,
    {
        let payloads = pending_device_payloads(&self.status);
        let mut dedupe = self
            .dedupe
            .lock()
            .map_err(|_| "notification dedupe state is unavailable".to_string())?;
        Ok(dispatch_new_notifications_with_checkpoint(
            &payloads,
            &mut dedupe,
            sender,
            checkpoint,
        ))
    }
}

impl ContinuousSyncRuntime {
    pub(super) fn new(options: ContinuousSyncOptions) -> Self {
        requeue_startup_sync_claims(&options);
        let store = CachedStore::new(options.args.state_root.join(DEFAULT_DATABASE_FILE));
        let (watcher, change_rx, watcher_state) = match start_sync_watcher(&options.args.root) {
            Ok((watcher, change_rx)) => {
                (Some(watcher), Some(change_rx), WatcherRuntimeState::Ready)
            }
            Err(error) => (None, None, WatcherRuntimeState::Limited(error.to_string())),
        };
        let watcher_recovery = WatcherRecovery::default();
        let last_json = initial_sync_status_json(&watcher_state, &watcher_recovery);
        let hosted_context = Arc::new(HostedContextCache::new());
        let hosted_resolver = hosted_context_resolver(hosted_context);
        Self {
            claimant_id: format!(
                "daemon-{}-{}",
                std::process::id(),
                OffsetDateTime::now_utc().unix_timestamp_nanos()
            ),
            options,
            next_tick: Instant::now(),
            next_remote_observe: Instant::now(),
            next_dispatch_claim: Instant::now(),
            awaiting_handoff: false,
            tick_count: 0,
            last_json,
            watcher,
            change_rx,
            watcher_state,
            watcher_recovery,
            sync_once: hosted_sync_executor_with_context(hosted_resolver.clone()),
            remote_ref_observer: hosted_remote_ref_observer_with_context(hosted_resolver.clone()),
            dispatch_claimer: hosted_dispatch_claimer_with_context(hosted_resolver.clone()),
            latest_observed_ref: None,
            remote_observer_state: RemoteObserverState::Unavailable,
            status_publisher: hosted_status_publisher_with_context(hosted_resolver.clone()),
            next_status_publish: Instant::now(),
            last_status_publish_fingerprint: None,
            last_status_publish_at: None,
            last_status_publish_failed_at: None,
            hosted_resolver,
            store_health: StoreHealth::new(),
            store,
            pending_dirty: DirtyScope::default(),
            pending_dirty_roots: PendingDirtyRoots::default(),
        }
    }

    #[cfg(test)]
    pub(super) fn status_json(&self) -> &str {
        &self.last_json
    }

    pub(super) fn claim_pending_dispatch_lease(
        &mut self,
    ) -> Result<Option<Lease>, Box<dyn std::error::Error>> {
        (self.dispatch_claimer)(self.options.args.clone())
    }

    fn claim_pending_dispatch_lease_if_due(
        &mut self,
        force: bool,
    ) -> Result<Option<Lease>, Box<dyn std::error::Error>> {
        let now = Instant::now();
        if !force && now < self.next_dispatch_claim {
            return Ok(None);
        }
        let result = self.claim_pending_dispatch_lease();
        self.next_dispatch_claim = match result {
            Ok(Some(_)) => {
                self.awaiting_handoff = true;
                now
            }
            Ok(None) if self.options.interval.is_zero() => {
                self.awaiting_handoff = false;
                now
            }
            Ok(None) => {
                self.awaiting_handoff = false;
                now + DISPATCH_CLAIM_IDLE_INTERVAL
            }
            Err(_) => {
                self.awaiting_handoff = true;
                now + DISPATCH_CLAIM_IDLE_INTERVAL
            }
        };
        result
    }

    pub(super) fn waiting_for_sync_queue_json(&self) -> String {
        if self.remote_observer_is_unavailable() {
            return self.remote_observer_failure_status_json();
        }
        let counts = self.queue_counts();
        let (state, unavailable_because, blocked_action, still_works) =
            waiting_queue_status_parts(&counts);
        daemon_json(&WaitingQueueStatusJson {
            state,
            tick_count: self.tick_count,
            watcher_state: self.watcher_state_json(),
            limited_capability: "continuous sync",
            unavailable_because,
            blocked_action,
            still_works,
            queue_counts: SyncOperationCountsJson::from(&counts),
            local_head: self.local_head_payload(),
            remote_head: self.remote_head_payload(),
        })
    }

    pub(super) fn metadata_store_for_write<T>(
        &self,
        context: &'static str,
        f: impl FnOnce(&MetadataStore) -> Result<T, MetadataError>,
    ) -> Option<T> {
        self.store_health
            .record(context, self.with_store_clearing_swallowed_failures(f))
    }

    pub(super) fn metadata_store_for_maintenance<T>(
        &self,
        context: &'static str,
        f: impl FnOnce(&mut MetadataStore) -> Result<T, MetadataError>,
    ) -> Option<T> {
        let failures_before = self.store_health.total_failure_count();
        let result = self.store.with_store_mut(f);
        if self.store_health.total_failure_count() > failures_before {
            self.store.clear();
        }
        self.store_health.record(context, result)
    }

    pub(super) fn queue_counts(&self) -> SyncOperationCounts {
        self.with_store_clearing_swallowed_failures(|store| {
            store.sync_operation_counts_for_device(
                &self.options.args.workspace_id(),
                &DeviceId::new(self.options.args.device_id.clone()),
            )
        })
        .unwrap_or_default()
    }

    /// Run a store access closure, and drop the cached SQLite handle when the
    /// closure swallowed a store failure via `StoreHealth::record` instead of
    /// propagating it. `CachedStore::with_store` only clears on a propagated
    /// `Err`, so without this, a handle that failed a write would be reused
    /// forever (plan 021's recovery contract requires reopening on any store
    /// error).
    fn with_store_clearing_swallowed_failures<T>(
        &self,
        f: impl FnOnce(&MetadataStore) -> Result<T, MetadataError>,
    ) -> Result<T, MetadataError> {
        let failures_before = self.store_health.total_failure_count();
        let result = self.store.with_store(f);
        if self.store_health.total_failure_count() > failures_before {
            self.store.clear();
        }
        result
    }

    #[cfg(test)]
    pub(super) fn local_head_json(&self) -> String {
        self.local_head_payload()
            .map(|payload| daemon_json(&payload))
            .unwrap_or_else(|| "null".to_string())
    }

    #[cfg(test)]
    pub(super) fn remote_head_json(&self) -> String {
        self.remote_head_payload()
            .map(|payload| daemon_json(&payload))
            .unwrap_or_else(|| "null".to_string())
    }

    fn local_head_payload(&self) -> Option<LocalHeadJson> {
        self.store
            .with_store(|store| store.workspace_sync_head(&self.options.args.workspace_id()))
            .ok()
            .flatten()
            .map(|head| LocalHeadJson {
                workspace_id: head.workspace_ref.workspace_id.into(),
                snapshot_id: head.workspace_ref.snapshot_id.into(),
                version: head.workspace_ref.version,
                updated_at_tick: head.workspace_ref.updated_at.tick,
            })
    }

    fn remote_head_payload(&self) -> Option<RemoteHeadJson> {
        self.store
            .with_store(|store| store.remote_ref_cursor(&self.options.args.workspace_id()))
            .ok()
            .flatten()
            .map(|cursor| RemoteHeadJson {
                workspace_id: cursor.workspace_id.as_str().to_string(),
                snapshot_id: cursor.last_observed_snapshot_id.unwrap_or_default(),
                version: cursor.last_observed_version.unwrap_or_default(),
            })
    }
}
