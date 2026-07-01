use super::*;

mod executor;
mod operations;

pub(in crate::daemon) use executor::*;
pub(in crate::daemon) use operations::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SyncOnceArgs {
    pub(super) root: PathBuf,
    pub(super) state_root: PathBuf,
    pub(super) workspace_id: String,
    pub(super) device_id: String,
    pub(super) sync_operation_id: Option<String>,
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
    pub(super) notification_dedupe: NotificationDedupe,
    pub(super) next_notification_poll: Instant,
}

pub(super) struct ContinuousSyncRuntime {
    pub(super) options: ContinuousSyncOptions,
    pub(super) next_tick: Instant,
    pub(super) next_remote_observe: Instant,
    pub(super) tick_count: u64,
    pub(super) last_json: String,
    pub(super) watcher: Option<RecommendedWatcher>,
    pub(super) change_rx: Option<Receiver<WatcherSignal>>,
    pub(super) watcher_state: WatcherRuntimeState,
    pub(super) sync_once: SyncExecutor,
    pub(super) remote_ref_observer: RemoteRefObserver,
    pub(super) latest_observed_ref: Option<WorkspaceRef>,
    pub(super) status_publisher: StatusPublisher,
    pub(super) next_status_publish: Instant,
}

pub(super) type SyncExecutor = Box<
    dyn FnMut(
            SyncOnceArgs,
            Option<WorkspaceRef>,
        ) -> Result<SyncOnceSummary, Box<dyn std::error::Error>>
        + 'static,
>;
pub(super) type RemoteRefObserver = Box<
    dyn FnMut(SyncOnceArgs) -> Result<Option<WorkspaceRef>, Box<dyn std::error::Error>> + 'static,
>;
pub(super) struct SyncOnceSummary {
    pub(super) workspace_id: String,
    pub(super) snapshot_id: String,
    pub(super) version: u64,
    pub(super) object_manifest_id: String,
    pub(super) manifest_object_key: String,
    pub(super) pack_object_keys: Vec<String>,
    pub(super) stale: bool,
    pub(super) merged: bool,
    pub(super) conflict_count: usize,
    pub(super) conflicts: Vec<ConflictSummary>,
}

pub(super) struct ConflictSummary {
    pub(super) id: String,
    pub(super) paths: Vec<String>,
}

impl DaemonRuntime {
    pub(super) fn poll_sync(&mut self) {
        if let Some(sync) = &mut self.sync {
            sync.poll();
        }
    }

    pub(super) fn poll_notifications(&mut self) {
        if !self.notify_approvals {
            return;
        }
        let now = Instant::now();
        if now < self.next_notification_poll {
            return;
        }
        self.next_notification_poll = now + NOTIFICATION_POLL_INTERVAL;
        let sender = DesktopNotificationSender;
        match self.poll_notifications_with(&sender) {
            Ok(report) if !report.failures.is_empty() => {
                for failure in report.failures {
                    eprintln!(
                        "bowline-daemon notification failed for {}: {}",
                        failure.title, failure.message
                    );
                }
            }
            Err(error) => eprintln!("bowline-daemon notifications unavailable: {error}"),
            _ => {}
        }
    }

    pub(super) fn poll_notifications_with<S>(
        &mut self,
        sender: &S,
    ) -> Result<NotificationDispatchReport, String>
    where
        S: NotificationSender,
    {
        if !self.notify_approvals {
            return Ok(NotificationDispatchReport::default());
        }
        let Some(sync) = self.sync.as_ref() else {
            return Ok(NotificationDispatchReport::default());
        };
        let args = &sync.options.args;
        let status = bowline_local::status::compose_status(StatusOptions {
            db_path: Some(args.state_root.join(DEFAULT_DATABASE_FILE)),
            requested_path: Some(args.root.display().to_string()),
            workspace_scope: true,
            generated_at: current_timestamp(),
        })
        .map_err(|error| error.to_string())?;
        let payloads = pending_device_payloads(&status);
        Ok(dispatch_new_notifications(
            &payloads,
            &mut self.notification_dedupe,
            sender,
        ))
    }

    pub(super) fn sync_json_field(&self) -> String {
        self.sync
            .as_ref()
            .map(|sync| {
                format!(
                    ",\"sync\":{}",
                    sync_status_with_hosted_calls(sync.status_json())
                )
            })
            .unwrap_or_default()
    }
}

impl ContinuousSyncRuntime {
    pub(super) fn new(options: ContinuousSyncOptions) -> Self {
        requeue_startup_sync_claims(&options);
        let (watcher, change_rx, watcher_state) = match start_sync_watcher(&options.args.root) {
            Ok((watcher, change_rx)) => {
                (Some(watcher), Some(change_rx), WatcherRuntimeState::Ready)
            }
            Err(error) => (None, None, WatcherRuntimeState::Limited(error.to_string())),
        };
        let last_json = initial_sync_status_json(&watcher_state);
        Self {
            options,
            next_tick: Instant::now(),
            next_remote_observe: Instant::now(),
            tick_count: 0,
            last_json,
            watcher,
            change_rx,
            watcher_state,
            sync_once: hosted_sync_executor(),
            remote_ref_observer: hosted_remote_ref_observer(),
            latest_observed_ref: None,
            status_publisher: hosted_status_publisher(),
            next_status_publish: Instant::now(),
        }
    }

    pub(super) fn poll(&mut self) {
        let now = Instant::now();
        self.maybe_publish_status_heartbeat(now);
        let watcher_drain = self.drain_changes();
        if watcher_drain.sync_now {
            self.next_tick = now;
        } else if watcher_drain.changed {
            self.next_tick = now + WATCHER_SETTLE_WINDOW;
        }
        let remote_observe_due = now >= self.next_remote_observe;
        if now < self.next_tick && !remote_observe_due {
            return;
        }
        if self
            .options
            .max_ticks
            .is_some_and(|max_ticks| self.tick_count >= max_ticks)
        {
            self.next_tick = now + self.options.interval;
            return;
        }
        if remote_observe_due && self.observe_remote_ref_cursor() {
            self.next_tick = now;
        }
        if Instant::now() < self.next_tick {
            return;
        }

        self.tick_count += 1;
        self.requeue_expired_sync_claims();
        let Some(claimed_operation) = self.claim_daemon_sync_operation() else {
            self.record_component_states("ready", self.watcher_component_state(), "online");
            self.last_json = self.waiting_for_sync_queue_json();
            self.next_tick = Instant::now() + self.options.interval;
            return;
        };
        let mut sync_args = self.options.args.clone();
        sync_args.sync_operation_id = Some(claimed_operation.clone());
        match (self.sync_once)(sync_args, self.latest_observed_ref.clone()) {
            Ok(summary) => {
                self.complete_daemon_sync_operation(&claimed_operation, &summary);
                self.record_remote_ref_cursor(&summary);
                self.record_component_states("ready", self.watcher_component_state(), "online");
                // Publish live status right after a successful ref advance so the
                // dashboard reflects the new head immediately.
                self.publish_status(
                    Some("ready"),
                    Some(self.watcher_component_state()),
                    Some("online"),
                );
                let queue_json = self.queue_counts_json();
                let head_json = self.local_head_json();
                let remote_head_json = self.remote_head_json();
                self.last_json = format!(
                    "{{\"state\":\"{}\",\"tickCount\":{},\"watcherState\":{},\"lastOutcome\":\"{}\",\"workspaceId\":{},\"snapshotId\":{},\"version\":{},\"conflictCount\":{},\"queueCounts\":{},\"localHead\":{},\"remoteHead\":{}}}",
                    summary.daemon_state(),
                    self.tick_count,
                    self.watcher_state_json(),
                    summary.sync_state(),
                    json_string(&summary.workspace_id),
                    json_string(&summary.snapshot_id),
                    summary.version,
                    summary.conflict_count,
                    queue_json,
                    head_json,
                    remote_head_json,
                );
            }
            Err(error) => {
                self.fail_daemon_sync_operation(&claimed_operation, &error.to_string());
                if self.queue_counts().has_no_pending_work() {
                    self.record_component_states("ready", self.watcher_component_state(), "online");
                    self.last_json = self.waiting_for_sync_queue_json();
                } else {
                    self.record_component_states(
                        "degraded",
                        self.watcher_component_state(),
                        "degraded",
                    );
                    let queue_json = self.queue_counts_json();
                    let head_json = self.local_head_json();
                    let remote_head_json = self.remote_head_json();
                    self.last_json = format!(
                        "{{\"state\":\"limited\",\"tickCount\":{},\"watcherState\":{},\"limitedCapability\":\"continuous sync\",\"unavailableBecause\":{},\"blockedAction\":\"sync ~/Code\",\"stillWorks\":[\"local edits\",\"status\",\"manual sync-once diagnostics\"],\"queueCounts\":{},\"localHead\":{},\"remoteHead\":{}}}",
                        self.tick_count,
                        self.watcher_state_json(),
                        json_string(&error.to_string()),
                        queue_json,
                        head_json,
                        remote_head_json,
                    );
                }
            }
        }
        self.next_tick = Instant::now() + self.options.interval;
    }

    pub(super) fn status_json(&self) -> &str {
        &self.last_json
    }

    pub(super) fn waiting_for_sync_queue_json(&self) -> String {
        let counts = self.queue_counts();
        let queue_json = sync_operation_counts_json(&counts);
        let head_json = self.local_head_json();
        let remote_head_json = self.remote_head_json();
        let (state, unavailable_because, blocked_action, still_works) =
            waiting_queue_status_parts(&counts);
        format!(
            "{{\"state\":{},\"tickCount\":{},\"watcherState\":{},\"limitedCapability\":\"continuous sync\",\"unavailableBecause\":{},\"blockedAction\":{},\"stillWorks\":{},\"queueCounts\":{},\"localHead\":{},\"remoteHead\":{}}}",
            json_string(state),
            self.tick_count,
            self.watcher_state_json(),
            json_string(unavailable_because),
            json_string(blocked_action),
            json_string_array(&still_works),
            queue_json,
            head_json,
            remote_head_json,
        )
    }

    pub(super) fn drain_changes(&mut self) -> WatcherDrain {
        let Some(change_rx) = &self.change_rx else {
            return WatcherDrain::default();
        };
        let mut drain = WatcherDrain::default();
        let mut drained_count = 0;
        for _ in 0..WATCHER_DRAIN_BUDGET {
            let Ok(signal) = change_rx.try_recv() else {
                break;
            };
            drained_count += 1;
            match signal {
                WatcherSignal::Changed(event) => {
                    drain.changed = true;
                    if let Err(error) = self.record_watcher_event(&event) {
                        self.watcher_state = WatcherRuntimeState::Limited(error.to_string());
                        drain.sync_now = true;
                    }
                }
                WatcherSignal::Limited(reason) => {
                    self.watcher_state = WatcherRuntimeState::Limited(reason);
                    drain.changed = true;
                    drain.sync_now = true;
                }
            }
        }
        if drained_count == WATCHER_DRAIN_BUDGET && change_rx.try_recv().is_ok() {
            self.watcher_state =
                WatcherRuntimeState::Limited("watch queue saturated; watcher disabled".to_string());
            self.change_rx = None;
            self.watcher = None;
            drain.changed = true;
            drain.sync_now = true;
        }
        drain
    }

    pub(super) fn watcher_state_json(&self) -> String {
        let _keep_watcher_alive = self.watcher.as_ref();
        watcher_runtime_state_json(&self.watcher_state)
    }

    pub(super) fn record_watcher_event(
        &self,
        event: &Event,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let operation = watcher_operation(&event.kind);
        let store = self.metadata_store()?;
        let workspace_id = self.options.args.workspace_id();
        let device_id = DeviceId::new(self.options.args.device_id.clone());
        let now = current_timestamp();
        let causation_id = format!("watch_{}_{}", self.tick_count, stable_token(&now));

        let paths = watcher_event_paths(&self.options.args.root, operation, event);
        for (index, path, source_path) in paths {
            let Some(relative_path) = watcher_relative_path(&self.options.args.root, path) else {
                continue;
            };
            if relative_path.is_empty() || is_private_state_path(&relative_path) {
                continue;
            }
            let metadata = fs::symlink_metadata(path).ok();
            let is_dir = metadata.as_ref().is_some_and(|metadata| metadata.is_dir());
            let byte_len = metadata
                .as_ref()
                .filter(|metadata| !metadata.is_dir())
                .map(|metadata| metadata.len());
            let policy = UserPolicy::load_for_path(&self.options.args.root, &relative_path)
                .unwrap_or_else(|_| UserPolicy::empty());
            let decision = classify_path(
                &PathFacts {
                    relative_path: relative_path.clone(),
                    is_dir,
                    byte_len,
                },
                &policy,
            );
            if !watcher_should_record(decision.classification, decision.mode) {
                continue;
            }
            store.append_local_write_log(&LocalWriteLogRecord {
                id: format!(
                    "watch_{}_{}_{}",
                    stable_token(&relative_path),
                    stable_token(operation),
                    stable_token(&format!("{now}-{index}")),
                ),
                workspace_id: workspace_id.clone(),
                device_id: device_id.clone(),
                project_id: None,
                path: relative_path,
                source_path,
                operation: operation.to_string(),
                staged_content_id: None,
                policy_classification: decision.classification,
                causation_id: causation_id.clone(),
                settled_at: now.clone(),
                created_at: now.clone(),
            })?;
        }
        Ok(())
    }

    pub(super) fn watcher_component_state(&self) -> &'static str {
        match self.watcher_state {
            WatcherRuntimeState::Ready => "ready",
            WatcherRuntimeState::Limited(_) => "degraded",
        }
    }

    pub(super) fn metadata_store(
        &self,
    ) -> Result<MetadataStore, bowline_local::metadata::MetadataError> {
        MetadataStore::open(self.options.args.state_root.join(DEFAULT_DATABASE_FILE))
    }

    pub(super) fn record_component_states(&self, sync: &str, watcher: &str, network: &str) {
        let Ok(store) = self.metadata_store() else {
            return;
        };
        let now = current_timestamp();
        let _ = store.set_component_state("sync", sync, &now);
        let _ = store.set_component_state("watcher", watcher, &now);
        let _ = store.set_component_state("network", network, &now);
    }

    /// Publish a redacted status snapshot. Any of the component states may be
    /// supplied to attach the daemon's live in-memory view; `None` lets the
    /// composed snapshot keep whatever state it read from the store. Failures are
    /// logged and swallowed so publishing never breaks the sync loop.
    pub(super) fn publish_status(
        &mut self,
        sync_state: Option<&str>,
        watcher_state: Option<&str>,
        network_state: Option<&str>,
    ) {
        let request = StatusPublishRequest {
            args: self.options.args.clone(),
            sync_state: sync_state.map(str::to_string),
            watcher_state: watcher_state.map(str::to_string),
            network_state: network_state.map(str::to_string),
        };
        if let Err(error) = (self.status_publisher)(request) {
            eprintln!("bowline-daemon status publish skipped: {error}");
        }
        self.next_status_publish = Instant::now() + STATUS_PUBLISH_INTERVAL;
    }

    pub(super) fn maybe_publish_status_heartbeat(&mut self, now: Instant) {
        if now < self.next_status_publish {
            return;
        }
        let watcher_state = self.watcher_component_state();
        self.publish_status(None, Some(watcher_state), None);
    }

    pub(super) fn queue_counts_json(&self) -> String {
        let counts = self.queue_counts();
        sync_operation_counts_json(&counts)
    }

    pub(super) fn queue_counts(&self) -> SyncOperationCounts {
        self.metadata_store()
            .and_then(|store| {
                self.complete_obsolete_daemon_reconciles_if_heads_match(&store);
                store.sync_operation_counts_for_device(
                    &self.options.args.workspace_id(),
                    &DeviceId::new(self.options.args.device_id.clone()),
                )
            })
            .unwrap_or_default()
    }

    pub(super) fn complete_obsolete_daemon_reconciles_if_heads_match(&self, store: &MetadataStore) {
        let workspace_id = self.options.args.workspace_id();
        let Ok(Some(local_head)) = store.workspace_sync_head(&workspace_id) else {
            return;
        };
        let Ok(Some(remote_head)) = store.remote_ref_cursor(&workspace_id) else {
            return;
        };
        if remote_head.last_observed_version != Some(local_head.workspace_ref.version)
            || remote_head.last_observed_snapshot_id.as_deref()
                != Some(local_head.workspace_ref.snapshot_id.as_str())
        {
            return;
        }
        let now = current_timestamp();
        let payload = format!(
            "{{\"repaired\":\"heads-match\",\"workspaceId\":{},\"snapshotId\":{},\"version\":{}}}",
            json_string(workspace_id.as_str()),
            json_string(local_head.workspace_ref.snapshot_id.as_str()),
            local_head.workspace_ref.version,
        );
        let _ = store.complete_obsolete_daemon_reconciles_for_device(
            &workspace_id,
            &DeviceId::new(self.options.args.device_id.clone()),
            &payload,
            &now,
        );
    }

    pub(super) fn local_head_json(&self) -> String {
        match self
            .metadata_store()
            .and_then(|store| store.workspace_sync_head(&self.options.args.workspace_id()))
        {
            Ok(Some(head)) => format!(
                "{{\"workspaceId\":{},\"snapshotId\":{},\"version\":{},\"updatedAtTick\":{}}}",
                json_string(&head.workspace_ref.workspace_id),
                json_string(&head.workspace_ref.snapshot_id),
                head.workspace_ref.version,
                head.workspace_ref.updated_at.tick,
            ),
            _ => "null".to_string(),
        }
    }

    pub(super) fn remote_head_json(&self) -> String {
        match self
            .metadata_store()
            .and_then(|store| store.remote_ref_cursor(&self.options.args.workspace_id()))
        {
            Ok(Some(cursor)) => format!(
                "{{\"workspaceId\":{},\"snapshotId\":{},\"version\":{}}}",
                json_string(cursor.workspace_id.as_str()),
                json_string(
                    cursor
                        .last_observed_snapshot_id
                        .as_deref()
                        .unwrap_or_default()
                ),
                cursor.last_observed_version.unwrap_or_default(),
            ),
            _ => "null".to_string(),
        }
    }

    pub(super) fn claim_daemon_sync_operation(&self) -> Option<String> {
        let store = self.metadata_store().ok()?;
        let now = current_timestamp();
        let workspace_id = self.options.args.workspace_id();
        let device_id = DeviceId::new(self.options.args.device_id.clone());
        let has_active_reconcile = store
            .active_sync_operation_for_device(&workspace_id, "daemon-reconcile", &device_id)
            .ok()
            .flatten()
            .is_some();
        if !has_active_reconcile
            && self.should_enqueue_daemon_reconcile(&store, &workspace_id, &device_id, &now)
        {
            let operation_nonce = stable_token(&format!(
                "{}:{}:{}:{}",
                self.options.args.device_id,
                self.tick_count,
                now,
                std::process::id()
            ));
            let operation_id = format!("daemon-sync-{}", operation_nonce);
            let idempotency_key = format!(
                "daemon-sync:{}:{}:{}",
                self.options.args.device_id, self.tick_count, operation_nonce
            );
            let record = SyncOperationRecord {
                id: operation_id,
                workspace_id: workspace_id.clone(),
                kind: "daemon-reconcile".to_string(),
                state: "queued".to_string(),
                idempotency_key,
                base_version: None,
                base_snapshot_id: None,
                target_snapshot_id: None,
                device_id: Some(device_id),
                payload_json: format!(
                    "{{\"root\":{},\"stateRoot\":{},\"tickCount\":{}}}",
                    json_string(&self.options.args.root.display().to_string()),
                    json_string(&self.options.args.state_root.display().to_string()),
                    self.tick_count,
                ),
                attempt_count: 0,
                claimed_by: None,
                heartbeat_at: None,
                next_attempt_at: None,
                last_error: None,
                created_at: now.clone(),
                updated_at: now.clone(),
            };
            let _ = store.enqueue_sync_operation(&record);
        }
        store
            .claim_next_sync_operation(&workspace_id, &self.options.args.device_id, &now)
            .ok()
            .flatten()
            .map(|operation| operation.id)
    }

    pub(super) fn should_enqueue_daemon_reconcile(
        &self,
        store: &MetadataStore,
        workspace_id: &WorkspaceId,
        device_id: &DeviceId,
        now: &str,
    ) -> bool {
        let Some(last_completed) =
            latest_completed_daemon_reconcile(store, workspace_id, device_id)
        else {
            return true;
        };
        if local_writes_after(store, workspace_id, device_id, &last_completed.updated_at) {
            return true;
        }
        if remote_cursor_ahead_of_local_head(store, workspace_id) {
            return true;
        }
        safety_reconcile_due(&last_completed.updated_at, self.options.interval, now)
    }

    pub(super) fn requeue_expired_sync_claims(&self) {
        let Ok(store) = self.metadata_store() else {
            return;
        };
        let now = OffsetDateTime::now_utc();
        let expired_before =
            format_timestamp(now - time::Duration::seconds(SYNC_CLAIM_TIMEOUT_SECONDS));
        let updated_at = format_timestamp(now);
        let _ = store.requeue_expired_sync_claims(
            &self.options.args.workspace_id(),
            &expired_before,
            &updated_at,
        );
    }

    pub(super) fn complete_daemon_sync_operation(
        &self,
        operation_id: &str,
        summary: &SyncOnceSummary,
    ) {
        let Ok(store) = self.metadata_store() else {
            return;
        };
        let now = current_timestamp();
        let payload = format!(
            "{{\"outcome\":\"{}\",\"workspaceId\":{},\"snapshotId\":{},\"version\":{},\"conflictCount\":{}}}",
            summary.sync_state(),
            json_string(&summary.workspace_id),
            json_string(&summary.snapshot_id),
            summary.version,
            summary.conflict_count,
        );
        let _ = store.complete_sync_operation(operation_id, &payload, &now);
        self.append_sync_completed_event(&store, operation_id, summary, &now);
    }

    pub(super) fn record_remote_ref_cursor(&self, summary: &SyncOnceSummary) {
        let Ok(store) = self.metadata_store() else {
            return;
        };
        let _ = store.put_remote_ref_cursor(&RemoteRefCursorRecord {
            workspace_id: WorkspaceId::new(summary.workspace_id.clone()),
            cursor: None,
            last_observed_version: Some(summary.version),
            last_observed_snapshot_id: Some(summary.snapshot_id.clone()),
            updated_at: current_timestamp(),
        });
    }

    pub(super) fn observe_remote_ref_cursor(&mut self) -> bool {
        self.next_remote_observe = Instant::now() + REMOTE_OBSERVER_DRAIN_INTERVAL;
        let workspace_id = self.options.args.workspace_id();
        let observed = match (self.remote_ref_observer)(self.options.args.clone()) {
            Ok(observed) => observed,
            Err(error) => {
                self.latest_observed_ref = None;
                self.record_component_states("idle", self.watcher_component_state(), "degraded");
                self.last_json = format!(
                    "{{\"state\":\"limited\",\"tickCount\":{},\"unavailableBecause\":{},\"nextAction\":\"check network or hosted auth\",\"queue\":{},\"localHead\":{},\"remoteHead\":{}}}",
                    self.tick_count,
                    json_string(&error.to_string()),
                    self.queue_counts_json(),
                    self.local_head_json(),
                    self.remote_head_json(),
                );
                return false;
            }
        };
        let Some(remote_ref) = observed else {
            self.latest_observed_ref = None;
            return false;
        };
        self.latest_observed_ref = Some(remote_ref.clone());
        let Ok(store) = self.metadata_store() else {
            return false;
        };
        let _ = store.put_remote_ref_cursor(&RemoteRefCursorRecord {
            workspace_id: workspace_id.clone(),
            cursor: None,
            last_observed_version: Some(remote_ref.version),
            last_observed_snapshot_id: Some(remote_ref.snapshot_id),
            updated_at: current_timestamp(),
        });
        remote_cursor_ahead_of_local_head(&store, &workspace_id)
    }

    pub(super) fn fail_daemon_sync_operation(&self, operation_id: &str, message: &str) {
        let Ok(store) = self.metadata_store() else {
            return;
        };
        let now = current_timestamp();
        let action = sync_failure_action(message);
        match action {
            SyncFailureAction::Attention => {
                let _ = store.mark_sync_operation_attention(operation_id, message, &now);
            }
            SyncFailureAction::Offline => {
                let retry_at = self.next_sync_attempt_at(&store, operation_id);
                let _ = store.block_sync_operation_offline(operation_id, message, &retry_at, &now);
            }
            SyncFailureAction::Retry => {
                let retry_at = self.next_sync_attempt_at(&store, operation_id);
                let _ = store.fail_sync_operation_for_retry(operation_id, message, &retry_at, &now);
            }
        }
        self.append_sync_failure_event(&store, operation_id, action, &now);
    }

    pub(super) fn next_sync_attempt_at(&self, store: &MetadataStore, operation_id: &str) -> String {
        let attempt_count = store
            .sync_operation_by_id(operation_id)
            .ok()
            .flatten()
            .map(|operation| operation.attempt_count)
            .unwrap_or(1);
        format_timestamp(
            OffsetDateTime::now_utc()
                + time::Duration::seconds(retry_delay_seconds(operation_id, attempt_count)),
        )
    }

    pub(super) fn append_sync_completed_event(
        &self,
        store: &MetadataStore,
        operation_id: &str,
        summary: &SyncOnceSummary,
        now: &str,
    ) {
        let workspace_id = self.options.args.workspace_id();
        let mut event = sync_event(
            EventName::SyncCompleted,
            EventSeverity::Info,
            format!(
                "Continuous sync completed with outcome `{}`.",
                summary.sync_state()
            ),
            &workspace_id,
            &self.options.args.device_id,
            operation_id,
            now,
        );
        event.payload.insert(
            "outcome".to_string(),
            serde_json::Value::String(summary.sync_state().to_string()),
        );
        event.payload.insert(
            "snapshotId".to_string(),
            serde_json::Value::String(summary.snapshot_id.clone()),
        );
        event.payload.insert(
            "version".to_string(),
            serde_json::Value::from(summary.version),
        );
        event.payload.insert(
            "conflictCount".to_string(),
            serde_json::Value::from(summary.conflict_count),
        );
        let _ = store.append_event(event);
        for conflict in &summary.conflicts {
            self.append_conflict_created_event(store, operation_id, conflict, now);
        }
    }

    pub(super) fn append_conflict_created_event(
        &self,
        store: &MetadataStore,
        operation_id: &str,
        conflict: &ConflictSummary,
        now: &str,
    ) {
        let workspace_id = self.options.args.workspace_id();
        let event_operation_id = format!("{operation_id}:{}", conflict.id);
        let mut event = WorkspaceEvent::new(
            sync_event_id(EventName::ConflictCreated, &event_operation_id, now),
            EventName::ConflictCreated,
            now,
            EventSeverity::Attention,
            format!(
                "Continuous sync detected a conflict in {} path(s).",
                conflict.paths.len()
            ),
            workspace_id,
        );
        event.device_id = Some(DeviceId::new(self.options.args.device_id.clone()));
        event.path = conflict.paths.first().cloned();
        event.subject = Some(EventSubject {
            kind: EventSubjectKind::Conflict,
            id: conflict.id.clone(),
            path: event.path.clone(),
        });
        event.payload.insert(
            "operationId".to_string(),
            serde_json::Value::String(operation_id.to_string()),
        );
        event.payload.insert(
            "conflictId".to_string(),
            serde_json::Value::String(conflict.id.clone()),
        );
        event.payload.insert(
            "pathCount".to_string(),
            serde_json::Value::from(conflict.paths.len()),
        );
        let _ = store.append_event(event);
    }

    pub(super) fn append_sync_failure_event(
        &self,
        store: &MetadataStore,
        operation_id: &str,
        action: SyncFailureAction,
        now: &str,
    ) {
        let (name, severity, outcome) = match action {
            SyncFailureAction::Attention => (
                EventName::SyncDegraded,
                EventSeverity::Attention,
                "attention",
            ),
            SyncFailureAction::Offline => {
                (EventName::SyncLimited, EventSeverity::Limited, "offline")
            }
            SyncFailureAction::Retry => (EventName::SyncLimited, EventSeverity::Limited, "retry"),
        };
        let workspace_id = self.options.args.workspace_id();
        let mut event = sync_event(
            name,
            severity,
            format!("Continuous sync is waiting for {outcome}."),
            &workspace_id,
            &self.options.args.device_id,
            operation_id,
            now,
        );
        event.payload.insert(
            "outcome".to_string(),
            serde_json::Value::String(outcome.to_string()),
        );
        event.redaction = EventRedaction::applied(["error-message-not-included"]);
        let _ = store.append_event(event);
    }
}
