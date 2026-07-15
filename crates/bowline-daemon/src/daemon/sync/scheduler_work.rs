use super::*;
use super::{
    overlay_runtime::validate_work_view_overlay_operation, work_accept_runtime::WorkAcceptRunResult,
};
use bowline_local::metadata::{
    ClaimedWorkViewAcceptOperation, WorkViewAcceptClaimCheck, WorkViewAcceptClaimHandle,
    WorkViewAcceptClaimTransition,
};

mod completion;
mod execution;

use execution::{execute_accept, execute_conflict, execute_overlay, execute_reconcile};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::daemon) enum DaemonWorkLane {
    Sync,
    ControlPlane,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(in crate::daemon) struct DaemonResourceKey(String);

impl DaemonResourceKey {
    pub(in crate::daemon) fn as_str(&self) -> &str {
        &self.0
    }

    fn sync(resource: &SyncResourceKey) -> Self {
        Self(resource.as_string())
    }

    fn work_accept(operation: &ClaimedWorkViewAcceptOperation) -> Self {
        // A durable accept reconciles and publishes canonical main. It must
        // exclude ordinary workspace sync even though its persistence table
        // also carries a more specific per-view resource key.
        Self(SyncResourceKey::workspace_sync(operation.operation.workspace_id.clone()).as_string())
    }
}

#[derive(Clone)]
pub(in crate::daemon) enum PreparedDaemonClaim {
    Sync(SyncClaimHandle),
    WorkViewAccept(WorkViewAcceptClaimHandle),
}

impl PreparedDaemonClaim {
    pub(in crate::daemon) fn operation_id(&self) -> &str {
        match self {
            Self::Sync(claim) => claim.operation_id(),
            Self::WorkViewAccept(claim) => claim.operation_id(),
        }
    }
}

#[derive(Clone)]
pub(in crate::daemon) struct WorkerLossRecovery {
    lane: DaemonWorkLane,
    claim: PreparedDaemonClaim,
    attempted_scan_scope: Option<ScanScope>,
}

impl WorkerLossRecovery {
    pub(in crate::daemon) fn operation_id(&self) -> &str {
        self.claim.operation_id()
    }

    pub(in crate::daemon) fn lane(&self) -> DaemonWorkLane {
        self.lane
    }
}

pub(in crate::daemon) struct PreparedDaemonWork {
    resource_key: DaemonResourceKey,
    recovery: WorkerLossRecovery,
    task: PreparedDaemonTask,
}

enum PreparedDaemonTask {
    Reconcile(PreparedReconcileWork),
    Conflict(PreparedSyncClaimWork),
    Overlay(PreparedSyncClaimWork),
    Accept(PreparedAcceptWork),
}

struct PreparedReconcileWork {
    claimed: ClaimedSyncOperation,
    args: SyncOnceArgs,
    observed_ref: Option<WorkspaceRef>,
    attempted_scan_scope: ScanScope,
    executor: SyncExecutor,
}

struct PreparedSyncClaimWork {
    claimed: ClaimedSyncOperation,
    args: SyncOnceArgs,
    resolver: HostedContextResolver,
}

struct PreparedAcceptWork {
    claimed: ClaimedWorkViewAcceptOperation,
    args: SyncOnceArgs,
    resolver: HostedContextResolver,
}

impl PreparedDaemonWork {
    pub(in crate::daemon) const fn lane(&self) -> DaemonWorkLane {
        self.recovery.lane
    }

    pub(in crate::daemon) fn resource_key(&self) -> &DaemonResourceKey {
        &self.resource_key
    }

    pub(in crate::daemon) fn operation_id(&self) -> &str {
        self.recovery.operation_id()
    }

    pub(in crate::daemon) fn worker_loss_recovery(&self) -> WorkerLossRecovery {
        self.recovery.clone()
    }

    pub(in crate::daemon) fn execute_caught(self) -> WorkerCompletion {
        let recovery = self.recovery.clone();
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.execute()))
            .unwrap_or(WorkerCompletion::Panicked(recovery))
    }

    fn execute(self) -> WorkerCompletion {
        match self.task {
            PreparedDaemonTask::Reconcile(work) => execute_reconcile(work),
            PreparedDaemonTask::Conflict(work) => execute_conflict(work),
            PreparedDaemonTask::Overlay(work) => execute_overlay(work),
            PreparedDaemonTask::Accept(work) => execute_accept(work),
        }
    }
}

pub(in crate::daemon) enum WorkerCompletion {
    Reconcile(ReconcileCompletion),
    Conflict(SyncClaimCompletion<ConflictWorkerResult>),
    Overlay(SyncClaimCompletion<OverlayWorkerResult>),
    WorkViewAccept(AcceptCompletion),
    Panicked(WorkerLossRecovery),
    WorkerLost(WorkerLossRecovery),
}

pub(in crate::daemon) struct ReconcileCompletion {
    claimed: ClaimedSyncOperation,
    attempted_scan_scope: ScanScope,
    result: Result<SyncOnceSummary, SyncOnceError>,
    executor: SyncExecutor,
}

pub(in crate::daemon) struct SyncClaimCompletion<T> {
    claimed: ClaimedSyncOperation,
    result: T,
}

pub(in crate::daemon) enum ConflictWorkerResult {
    Completed {
        input: ConflictOccurrenceReconcile,
        outcome: ConflictReconcileOutcome,
        mark_local: bool,
    },
    Cancelled,
    Failed(SyncOnceError),
    OwnershipLost,
}

pub(in crate::daemon) enum OverlayWorkerResult {
    Completed(WorkViewOverlaySyncResult),
    Cancelled,
    Failed(SyncOnceError),
    OwnershipLost,
}

pub(in crate::daemon) struct AcceptCompletion {
    claimed: ClaimedWorkViewAcceptOperation,
    result: Result<WorkAcceptRunResult, SyncOnceError>,
}

impl WorkerCompletion {
    pub(in crate::daemon) fn worker_lost(recovery: WorkerLossRecovery) -> Self {
        Self::WorkerLost(recovery)
    }
}

impl ContinuousSyncRuntime {
    pub(in crate::daemon) fn next_scheduler_deadline(&self, now: Instant) -> Instant {
        let mut deadline = self
            .next_tick
            .min(self.next_remote_observe)
            .min(self.next_dispatch_claim)
            .min(self.next_status_publish);
        if let Some(rearm_at) = self.watcher_recovery.rearm_at {
            deadline = deadline.min(rearm_at);
        }
        let persisted = self
            .store
            .with_store(|store| {
                let workspace_id = self.options.args.workspace_id();
                let device_id = DeviceId::new(self.options.args.device_id.clone());
                let sync = store.next_sync_operation_deadline(&workspace_id)?;
                let accept = store.next_work_view_accept_deadline(&workspace_id, &device_id)?;
                Ok([sync, accept].into_iter().flatten().min())
            })
            .ok()
            .flatten();
        if let Some(persisted) = persisted.and_then(|value| persisted_instant(now, &value)) {
            deadline = deadline.min(persisted);
        }
        deadline.max(now)
    }

    pub(in crate::daemon) fn renew_worker_claim(&self, recovery: &WorkerLossRecovery) -> bool {
        let now_time = OffsetDateTime::now_utc();
        let now = format_timestamp(now_time);
        let lease_expires_at =
            format_timestamp(now_time + time::Duration::seconds(SYNC_CLAIM_TIMEOUT_SECONDS));
        self.metadata_store_for_write(
            "metadata_store(renew_worker_claim)",
            |store| match &recovery.claim {
                PreparedDaemonClaim::Sync(claim) => store
                    .renew_sync_operation_claim(claim, &now, &lease_expires_at)
                    .map(|transition| transition == SyncClaimTransition::Applied),
                PreparedDaemonClaim::WorkViewAccept(claim) => store
                    .renew_work_view_accept_claim(claim, &now, &lease_expires_at)
                    .map(|transition| transition == WorkViewAcceptClaimTransition::Applied),
            },
        ) == Some(true)
    }

    pub(in crate::daemon) fn prepare_work_view_accept(&self) -> Option<PreparedDaemonWork> {
        let now_time = OffsetDateTime::now_utc();
        let now = format_timestamp(now_time);
        let lease_expires_at =
            format_timestamp(now_time + time::Duration::seconds(SYNC_CLAIM_TIMEOUT_SECONDS));
        let workspace_id = self.options.args.workspace_id();
        let device_id = DeviceId::new(self.options.args.device_id.clone());
        let claimed = self
            .metadata_store_for_write("metadata_store(claim_work_view_accept)", |store| {
                store.requeue_expired_work_view_accepts(&now)?;
                store.claim_next_work_view_accept(
                    &workspace_id,
                    &device_id,
                    &self.claimant_id,
                    &now,
                    &lease_expires_at,
                )
            })
            .flatten()?;
        let resource_key = DaemonResourceKey::work_accept(&claimed);
        Some(PreparedDaemonWork {
            resource_key,
            recovery: WorkerLossRecovery {
                lane: DaemonWorkLane::Sync,
                claim: PreparedDaemonClaim::WorkViewAccept(claimed.claim.clone()),
                attempted_scan_scope: None,
            },
            task: PreparedDaemonTask::Accept(PreparedAcceptWork {
                claimed,
                args: self.options.args.clone(),
                resolver: self.hosted_resolver.clone(),
            }),
        })
    }

    pub(in crate::daemon) fn prepare_ready_side_work(&mut self) -> Option<PreparedDaemonWork> {
        let now_time = OffsetDateTime::now_utc();
        let now = format_timestamp(now_time);
        let lease_expires_at =
            format_timestamp(now_time + time::Duration::seconds(SYNC_CLAIM_TIMEOUT_SECONDS));
        let workspace_id = self.options.args.workspace_id();
        let claimed = self
            .metadata_store_for_write("metadata_store(claim_ready_side_work)", |store| {
                store.requeue_expired_sync_claims(&workspace_id, &now)?;
                store.claim_next_control_plane_sync_operation(
                    &workspace_id,
                    &self.claimant_id,
                    &now,
                    &lease_expires_at,
                )
            })
            .flatten()?;
        Some(self.prepare_claimed_operation(claimed))
    }

    pub(in crate::daemon) fn prepare_claimed_operation(
        &mut self,
        claimed: ClaimedSyncOperation,
    ) -> PreparedDaemonWork {
        let resource_key = DaemonResourceKey::sync(&claimed.operation.resource_key);
        let claim = PreparedDaemonClaim::Sync(claimed.claim.clone());
        match claimed.operation.kind {
            SyncOperationKind::Reconcile => {
                let mut args = self.options.args.clone();
                args.sync_claim = Some(claimed.claim.clone());
                let raw_scan_scope = self.pending_dirty.take_scope(if self.tick_count == 1 {
                    FullScanReason::Startup
                } else if matches!(self.watcher_state, WatcherRuntimeState::Limited(_)) {
                    FullScanReason::WatcherUnavailable
                } else {
                    FullScanReason::ReconcileFallback
                });
                args.scan_scope = self.resolve_dirty_batch_scope(raw_scan_scope);
                let attempted_scan_scope = args.scan_scope.clone();
                let replacement = hosted_sync_executor_with_context(self.hosted_resolver.clone());
                let executor = std::mem::replace(&mut self.sync_once, replacement);
                PreparedDaemonWork {
                    resource_key,
                    recovery: WorkerLossRecovery {
                        lane: DaemonWorkLane::Sync,
                        claim,
                        attempted_scan_scope: Some(attempted_scan_scope.clone()),
                    },
                    task: PreparedDaemonTask::Reconcile(PreparedReconcileWork {
                        claimed,
                        args,
                        observed_ref: self.latest_observed_ref.clone(),
                        attempted_scan_scope,
                        executor,
                    }),
                }
            }
            SyncOperationKind::ConflictOccurrenceReconcile => PreparedDaemonWork {
                resource_key,
                recovery: WorkerLossRecovery {
                    lane: DaemonWorkLane::ControlPlane,
                    claim,
                    attempted_scan_scope: None,
                },
                task: PreparedDaemonTask::Conflict(PreparedSyncClaimWork {
                    claimed,
                    args: self.options.args.clone(),
                    resolver: self.hosted_resolver.clone(),
                }),
            },
            SyncOperationKind::WorkViewOverlaySync => PreparedDaemonWork {
                resource_key,
                recovery: WorkerLossRecovery {
                    lane: DaemonWorkLane::ControlPlane,
                    claim,
                    attempted_scan_scope: None,
                },
                task: PreparedDaemonTask::Overlay(PreparedSyncClaimWork {
                    claimed,
                    args: self.options.args.clone(),
                    resolver: self.hosted_resolver.clone(),
                }),
            },
        }
    }
}

fn persisted_instant(now: Instant, value: &str) -> Option<Instant> {
    let deadline = OffsetDateTime::parse(value, &Rfc3339).ok()?;
    let wall_now = OffsetDateTime::now_utc();
    if deadline <= wall_now {
        return Some(now);
    }
    let duration = Duration::try_from(deadline - wall_now).ok()?;
    Some(now + duration)
}

impl DaemonRuntime {
    pub(in crate::daemon) fn next_scheduler_deadline(&self, now: Instant) -> Instant {
        self.sync
            .as_ref()
            .map_or(self.next_notification_poll, |sync| {
                sync.next_scheduler_deadline(now)
                    .min(self.next_notification_poll)
            })
            .max(now)
    }

    pub(in crate::daemon) fn poll_prepare(
        &mut self,
        prefer_work_view_accept: bool,
    ) -> Option<PreparedDaemonWork> {
        self.sync
            .as_mut()?
            .poll_prepare_with_preference(prefer_work_view_accept)
    }

    pub(in crate::daemon) fn prepare_ready_side_work(&mut self) -> Option<PreparedDaemonWork> {
        self.sync.as_mut()?.prepare_ready_side_work()
    }

    pub(in crate::daemon) fn apply_worker_completion(&mut self, completion: WorkerCompletion) {
        if let Some(sync) = &mut self.sync {
            sync.apply_worker_completion(completion);
        }
    }

    pub(in crate::daemon) fn requeue_dispatch_failure(&mut self, work: PreparedDaemonWork) -> bool {
        self.sync
            .as_mut()
            .is_some_and(|sync| sync.requeue_dispatch_failure(work))
    }

    pub(in crate::daemon) fn renew_worker_claim(&self, recovery: &WorkerLossRecovery) -> bool {
        self.sync
            .as_ref()
            .is_some_and(|sync| sync.renew_worker_claim(recovery))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::tests::{unique_temp_dir, watcher_test_runtime};
    use bowline_core::{
        ids::{ProjectId, SnapshotId, WorkViewId},
        work_views::{
            OVERLAY_HEAD_EMPTY, WorkView, WorkViewLifecycle, WorkViewRetention,
            WorkViewRetentionState, WorkViewSyncState, WorkViewVisibility,
        },
    };
    use bowline_local::metadata::{
        WorkViewAcceptOperationRecord, WorkViewAcceptOperationState, WorkViewAcceptResourceKey,
    };

    #[test]
    fn prepared_work_claims_before_dispatch_and_fenced_requeue_restores_it() {
        let (temp, mut runtime, store, workspace_id) = seeded_runtime("prepared-dispatch");
        enqueue_reconcile(&store, &workspace_id, "prepared-dispatch-op");
        runtime.next_remote_observe = Instant::now() + Duration::from_secs(60);

        let before_prepare = Instant::now();
        let work = runtime.poll_prepare().expect("prepared durable work");

        assert_eq!(work.lane(), DaemonWorkLane::Sync);
        assert_eq!(
            work.resource_key().as_str(),
            SyncResourceKey::workspace_sync(workspace_id.clone()).as_string()
        );
        assert_eq!(work.operation_id(), "prepared-dispatch-op");
        assert!(runtime.next_tick > before_prepare);
        assert_eq!(
            store
                .sync_operation_by_id("prepared-dispatch-op")
                .expect("operation reads")
                .expect("operation exists")
                .state,
            SyncOperationState::Claimed
        );

        assert!(runtime.requeue_dispatch_failure(work));
        let requeued = store
            .sync_operation_by_id("prepared-dispatch-op")
            .expect("operation reads")
            .expect("operation exists");
        assert_eq!(requeued.state, SyncOperationState::Queued);
        assert_eq!(
            requeued.last_error_code.as_deref(),
            Some("dispatch-unavailable")
        );
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn durable_wake_discovers_a_queued_reconcile_before_the_periodic_tick() {
        let (temp, mut runtime, store, workspace_id) = seeded_runtime("event-driven-discovery");
        enqueue_reconcile(&store, &workspace_id, "event-driven-op");
        runtime.next_tick = Instant::now() + Duration::from_secs(600);
        runtime.next_remote_observe = Instant::now() + Duration::from_secs(600);

        let work = runtime
            .poll_prepare_with_preference(false)
            .expect("durable wake claims already-queued work immediately");

        assert_eq!(work.operation_id(), "event-driven-op");
        assert_eq!(work.lane(), DaemonWorkLane::Sync);
        assert_eq!(
            store
                .sync_operation_by_id("event-driven-op")
                .expect("operation reads")
                .expect("operation exists")
                .state,
            SyncOperationState::Claimed
        );
        assert!(runtime.requeue_dispatch_failure(work));
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn durable_discovery_alternates_accept_and_reconcile_with_a_future_tick() {
        let (temp, mut runtime, store, workspace_id) = seeded_runtime("durable-fairness");
        enqueue_work_accept(&store, &workspace_id, "accept-fairness");
        enqueue_reconcile(&store, &workspace_id, "reconcile-fairness");
        runtime.next_tick = Instant::now() + Duration::from_secs(600);
        runtime.next_remote_observe = Instant::now() + Duration::from_secs(600);

        let accept = runtime
            .poll_prepare_with_preference(true)
            .expect("accept preference claims accept");
        assert_eq!(accept.operation_id(), "accept-fairness");
        assert!(runtime.requeue_dispatch_failure(accept));

        let reconcile = runtime
            .poll_prepare_with_preference(false)
            .expect("reconcile preference bypasses the future periodic tick");
        assert_eq!(reconcile.operation_id(), "reconcile-fairness");
        assert!(runtime.requeue_dispatch_failure(reconcile));
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn caught_worker_panic_keeps_claim_for_lease_recovery_and_advances_tick() {
        let (temp, mut runtime, store, workspace_id) = seeded_runtime("prepared-panic");
        enqueue_reconcile(&store, &workspace_id, "prepared-panic-op");
        runtime.next_remote_observe = Instant::now() + Duration::from_secs(60);
        runtime.sync_once = Box::new(|_, _| panic!("injected sync worker panic"));
        let work = runtime.poll_prepare().expect("prepared durable work");
        runtime.next_tick = Instant::now() - Duration::from_secs(1);

        let completion = work.execute_caught();
        assert!(matches!(completion, WorkerCompletion::Panicked(_)));
        runtime.apply_worker_completion(completion);

        assert!(runtime.next_tick > Instant::now());
        let claimed = store
            .sync_operation_by_id("prepared-panic-op")
            .expect("operation reads")
            .expect("operation exists");
        assert_eq!(claimed.state, SyncOperationState::Claimed);
        assert_eq!(claimed.last_error_code.as_deref(), Some("worker-panicked"));
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn successful_completion_is_fenced_and_advances_tick() {
        let (temp, mut runtime, store, workspace_id) = seeded_runtime("prepared-complete");
        enqueue_reconcile(&store, &workspace_id, "prepared-complete-op");
        runtime.next_remote_observe = Instant::now() + Duration::from_secs(60);
        let workspace = workspace_id.as_str().to_string();
        runtime.sync_once = Box::new(move |_, _| {
            Ok(SyncOnceSummary {
                workspace_id: workspace.clone(),
                snapshot_id: "snapshot-complete".to_string(),
                version: 1,
                outcome: SyncSummaryOutcome::NoChanges,
                snapshot_root_manifest_id: None,
                namespace_root_id: None,
                manifest_object_key: None,
                conflict_count: 0,
                conflicts: Vec::new(),
                scan: SyncScanSummary::default(),
                cancelled_late: false,
            })
        });
        let work = runtime.poll_prepare().expect("prepared durable work");
        runtime.next_tick = Instant::now() - Duration::from_secs(1);

        runtime.apply_worker_completion(work.execute_caught());

        assert!(runtime.next_tick > Instant::now());
        assert_eq!(
            store
                .sync_operation_by_id("prepared-complete-op")
                .expect("operation reads")
                .expect("operation exists")
                .state,
            SyncOperationState::Completed
        );
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn forced_side_discovery_skips_reconcile_and_claims_control_plane_work() {
        let (temp, mut runtime, store, workspace_id) = seeded_runtime("prepared-side-work");
        enqueue_reconcile(&store, &workspace_id, "older-reconcile");
        enqueue_operation(
            &store,
            &workspace_id,
            "newer-overlay",
            SyncOperationKind::WorkViewOverlaySync,
            SyncResourceKey::post_commit(workspace_id.clone()),
            "2026-07-14T00:00:01Z",
        );
        let next_tick = Instant::now() + Duration::from_secs(600);
        runtime.next_tick = next_tick;

        let side_work = runtime
            .prepare_ready_side_work()
            .expect("ready control-plane operation");

        assert_eq!(side_work.lane(), DaemonWorkLane::ControlPlane);
        assert_eq!(side_work.operation_id(), "newer-overlay");
        assert_eq!(runtime.next_tick, next_tick);
        assert_eq!(
            store
                .sync_operation_by_id("older-reconcile")
                .expect("reconcile reads")
                .expect("reconcile exists")
                .state,
            SyncOperationState::Queued
        );
        assert_eq!(
            store
                .sync_operation_by_id("newer-overlay")
                .expect("overlay reads")
                .expect("overlay exists")
                .state,
            SyncOperationState::Claimed
        );
        assert!(runtime.requeue_dispatch_failure(side_work));
        let _ = fs::remove_dir_all(temp);
    }

    fn seeded_runtime(label: &str) -> (PathBuf, ContinuousSyncRuntime, MetadataStore, WorkspaceId) {
        let temp = unique_temp_dir(label);
        let root = temp.join("Code");
        let state_root = temp.join("state");
        fs::create_dir_all(&root).expect("workspace root");
        let workspace_id = WorkspaceId::new(format!("workspace-{label}"));
        let store = MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).expect("store");
        store
            .insert_workspace(&workspace_id, "Code", "2026-07-14T00:00:00Z")
            .expect("workspace");
        let runtime = watcher_test_runtime(root, state_root, workspace_id.as_str());
        (temp, runtime, store, workspace_id)
    }

    fn enqueue_reconcile(store: &MetadataStore, workspace_id: &WorkspaceId, id: &str) {
        enqueue_operation(
            store,
            workspace_id,
            id,
            SyncOperationKind::Reconcile,
            SyncResourceKey::workspace_sync(workspace_id.clone()),
            "2026-07-14T00:00:00Z",
        );
    }

    fn enqueue_work_accept(store: &MetadataStore, workspace_id: &WorkspaceId, id: &str) {
        let project_id = ProjectId::new("project-durable-fairness");
        let work_view_id = WorkViewId::new("view-durable-fairness");
        store
            .insert_root(
                "root-durable-fairness",
                workspace_id,
                "/tmp/bowline-durable-fairness",
                "2026-07-14T00:00:00Z",
            )
            .expect("root inserted");
        store
            .insert_project(
                &project_id,
                workspace_id,
                "root-durable-fairness",
                "apps/web",
                "2026-07-14T00:00:00Z",
            )
            .expect("project inserted");
        store
            .upsert_work_view(&WorkView {
                id: work_view_id.clone(),
                workspace_id: workspace_id.clone(),
                project_id: project_id.clone(),
                project_path: "apps/web".to_string(),
                name: "durable-fairness".to_string(),
                visible_path: "/tmp/bowline-durable-fairness/.work/view".to_string(),
                base_snapshot_id: SnapshotId::new("snapshot-durable-fairness"),
                overlay_head: OVERLAY_HEAD_EMPTY.to_string(),
                overlay_version: 0,
                env_profile: "default".to_string(),
                lifecycle: WorkViewLifecycle::Active,
                visibility: WorkViewVisibility::DefaultVisible,
                sync_state: WorkViewSyncState::LocalOnly,
                retention: WorkViewRetention {
                    state: WorkViewRetentionState::Current,
                    retain_until: None,
                    restorable: true,
                },
                owner_device_id: None,
                followed_by: Vec::new(),
                host_materializations: Vec::new(),
                attention: Vec::new(),
                created_at: "2026-07-14T00:00:00Z".to_string(),
                updated_at: "2026-07-14T00:00:00Z".to_string(),
            })
            .expect("work view inserted");
        store
            .enqueue_work_view_accept(&WorkViewAcceptOperationRecord {
                id: id.to_string(),
                workspace_id: workspace_id.clone(),
                project_id: project_id.clone(),
                work_view_id: work_view_id.clone(),
                device_id: DeviceId::new("device-test"),
                resource_key: WorkViewAcceptResourceKey::new(
                    workspace_id.clone(),
                    project_id,
                    work_view_id,
                ),
                idempotency_key: format!("idempotency-{id}"),
                state: WorkViewAcceptOperationState::Queued,
                selected_paths: None,
                input_json: "{}".to_string(),
                observed_main_snapshot_id: None,
                observed_ref_version: None,
                observed_ref_snapshot_id: None,
                target_snapshot_id: None,
                result_json: None,
                review_reason: None,
                failure_reason: None,
                cancellation_requested_at: None,
                last_error: None,
                claimed_by: None,
                claim_token: None,
                claim_generation: 0,
                heartbeat_at: None,
                lease_expires_at: None,
                attempt_count: 0,
                next_attempt_at: None,
                created_at: "2026-07-14T00:00:00Z".to_string(),
                updated_at: "2026-07-14T00:00:00Z".to_string(),
            })
            .expect("accept enqueued");
    }

    fn enqueue_operation(
        store: &MetadataStore,
        workspace_id: &WorkspaceId,
        id: &str,
        kind: SyncOperationKind,
        resource_key: SyncResourceKey,
        created_at: &str,
    ) {
        store
            .enqueue_sync_operation(&SyncOperationRecord {
                id: id.to_string(),
                workspace_id: workspace_id.clone(),
                kind,
                resource_key,
                state: SyncOperationState::Queued,
                idempotency_key: format!("idempotency-{id}"),
                base_version: None,
                base_snapshot_id: None,
                target_snapshot_id: None,
                device_id: Some(DeviceId::new("device-test")),
                payload_json: "{}".to_string(),
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
                created_at: created_at.to_string(),
                updated_at: created_at.to_string(),
            })
            .expect("operation enqueued");
    }
}
