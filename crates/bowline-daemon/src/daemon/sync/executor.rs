use super::*;

use bowline_control_plane::WorkspaceControlPlaneClient;

mod remote_observer;
mod side_lanes;
mod workspace_key_context;
#[cfg(test)]
use workspace_key_context::HostedSyncOperation;
pub(in crate::daemon) use workspace_key_context::LocalWorkspaceKey;
use workspace_key_context::{hosted_sync_executor_with_operations, require_local_workspace_key};

#[cfg(test)]
use side_lanes::validate_hosted_operation_scope;
pub(in crate::daemon) use side_lanes::{
    execute_work_view_accept_with_context, reconcile_conflict_occurrence_with_context,
    sync_work_view_overlays_with_context,
};

pub(in crate::daemon) use remote_observer::hosted_remote_ref_observer_with_context;
#[cfg(test)]
use remote_observer::{
    HostedObserverOperation, hosted_remote_ref_observer_with_operations_and_refresh,
    remote_ref_observer_with_stream_starter_and_refresh,
};
#[cfg(test)]
pub(in crate::daemon) use remote_observer::{
    RemoteRefStreamStarter, remote_observer_reconnect_delay,
    remote_ref_observer_with_stream_starter,
};

pub(in crate::daemon) fn run_sync_once(
    args: SyncOnceArgs,
) -> Result<SyncOnceSummary, SyncOnceError> {
    run_sync_once_observed(args, None)
}

pub(in crate::daemon) fn run_sync_once_observed(
    args: SyncOnceArgs,
    observed_base_ref: Option<WorkspaceRef>,
) -> Result<SyncOnceSummary, SyncOnceError> {
    let workspace_id = WorkspaceId::new(args.workspace_id.clone());
    let device_id = DeviceId::new(args.device_id.clone());
    require_convex_url().map_err(|_| SyncOnceError::HostedConfigUnavailable)?;
    let key_store = key_store()?;
    let workspace_key = key_store
        .load_workspace_key(&workspace_id)?
        .ok_or(SyncOnceError::WorkspaceKeyMissing)?;
    let workspace_key_bytes = workspace_key_bytes(&workspace_key.key_bytes)
        .map_err(|_| SyncOnceError::WorkspaceKeyInvalid)?;
    let control_plane = hosted_control_plane(&*key_store, workspace_id.clone(), device_id.clone())?;
    let base_ref = match observed_base_ref {
        Some(workspace_ref) => workspace_ref,
        None => match control_plane.get_workspace_ref(&workspace_id)? {
            Some(workspace_ref) => workspace_ref,
            None => control_plane.create_workspace_ref(&workspace_id)?,
        },
    };
    let byte_store = SignedUrlByteStore::new(&control_plane, workspace_id.as_str());

    run_sync_once_with(
        args,
        &control_plane,
        &byte_store,
        base_ref,
        workspace_id,
        device_id,
        LocalWorkspaceKey {
            bytes: workspace_key_bytes,
            key_epoch: workspace_key.key_epoch,
        },
    )
}

#[cfg(test)]
pub(in crate::daemon) fn hosted_sync_executor() -> SyncExecutor {
    hosted_sync_executor_with_context(hosted_context_resolver(Arc::new(HostedContextCache::new())))
}

pub(in crate::daemon) fn hosted_sync_executor_with_context(
    resolver: HostedContextResolver,
) -> SyncExecutor {
    hosted_sync_executor_with_operations(
        resolver,
        Arc::new(require_local_workspace_key),
        Arc::new(run_sync_once_observed_with_hosted),
    )
}

fn run_sync_once_observed_with_hosted(
    hosted: Arc<HostedContext>,
    args: SyncOnceArgs,
    observed_base_ref: Option<WorkspaceRef>,
    workspace_key: LocalWorkspaceKey,
) -> Result<SyncOnceSummary, SyncOnceError> {
    run_sync_once_observed_with_hosted_and_accept_claim(
        hosted,
        args,
        observed_base_ref,
        workspace_key,
        None,
    )
}

fn run_sync_once_observed_with_hosted_and_accept_claim(
    hosted: Arc<HostedContext>,
    args: SyncOnceArgs,
    observed_base_ref: Option<WorkspaceRef>,
    workspace_key: LocalWorkspaceKey,
    work_view_accept_claim: Option<bowline_local::metadata::WorkViewAcceptClaimHandle>,
) -> Result<SyncOnceSummary, SyncOnceError> {
    let workspace_id = WorkspaceId::new(args.workspace_id.clone());
    let device_id = DeviceId::new(args.device_id.clone());
    let base_ref = match observed_base_ref {
        Some(workspace_ref) => workspace_ref,
        None => match hosted.client.get_workspace_ref(&workspace_id)? {
            Some(workspace_ref) => workspace_ref,
            None => hosted.client.create_workspace_ref(&workspace_id)?,
        },
    };
    let byte_store = SignedUrlByteStore::with_http_client(
        hosted.client.as_ref(),
        workspace_id.as_str(),
        hosted.http.clone(),
    );
    run_sync_once_with_accept_claim(RunSyncOnceRequest {
        args,
        control_plane: hosted.client.as_ref(),
        byte_store: &byte_store,
        base_ref,
        workspace_id,
        device_id,
        workspace_key,
        work_view_accept_claim,
    })
}

pub(in crate::daemon) fn hosted_dispatch_claimer_with_context(
    resolver: HostedContextResolver,
) -> DispatchClaimer {
    hosted_dispatch_claimer_with_operations(
        resolver,
        Arc::new(claim_pending_dispatched_lease_with_hosted),
    )
}

type HostedDispatchOperation = Arc<
    dyn Fn(Arc<HostedContext>, SyncOnceArgs) -> Result<Option<Lease>, Box<dyn std::error::Error>>
        + Send
        + Sync,
>;

fn hosted_dispatch_claimer_with_operations(
    resolver: HostedContextResolver,
    operation: HostedDispatchOperation,
) -> DispatchClaimer {
    Box::new(move |args| operation(resolver(&args)?, args))
}

#[cfg(test)]
pub(in crate::daemon) fn noop_dispatch_claimer() -> DispatchClaimer {
    Box::new(|_| Ok(None))
}

fn claim_pending_dispatched_lease_with_hosted(
    hosted: Arc<HostedContext>,
    args: SyncOnceArgs,
) -> Result<Option<Lease>, Box<dyn std::error::Error>> {
    let workspace_id = WorkspaceId::new(args.workspace_id.clone());
    let device_id = DeviceId::new(args.device_id.clone());
    let workspace_content_key = require_local_workspace_key(&args)?.bytes;
    claim_pending_dispatched_lease_with(
        hosted.client.as_ref(),
        args,
        &workspace_id,
        &device_id,
        workspace_content_key,
    )
}

/// Materialization acknowledgement returned through lease and readiness surfaces.
const HANDOFF_MATERIALIZED_STATUS: &str = "handoff-materialized";
const HANDOFF_COMPLETED_STATUS: &str = "completed";

/// Materialize-only handoff pass. Bowline makes the correct workspace/work-view
/// appear on the trusted target host and records a read-only acknowledgement; it
/// no longer claims, runs, or publishes results (that is the agent runtime's job).
pub(in crate::daemon) fn claim_pending_dispatched_lease_with(
    control_plane: &dyn LeaseControlPlaneClient,
    args: SyncOnceArgs,
    workspace_id: &WorkspaceId,
    device_id: &DeviceId,
    workspace_content_key: [u8; 32],
) -> Result<Option<Lease>, Box<dyn std::error::Error>> {
    // A handoff lease is one whose target is this device and that carries an
    // origin device ref; there is no separate dispatch supervisor state.
    let mut handoff = control_plane
        .list_leases(workspace_id)?
        .into_iter()
        .filter(|lease| {
            lease.target_device_ref.as_deref() == Some(device_id.as_str())
                && lease.origin_device_ref.is_some()
        })
        .collect::<Vec<_>>();
    handoff.sort_by(|left, right| {
        left.created_at
            .tick
            .cmp(&right.created_at.tick)
            .then_with(|| left.lease_id.cmp(&right.lease_id))
    });
    let db_path = args.state_root.join(DEFAULT_DATABASE_FILE);
    let store = MetadataStore::open(&db_path)?;
    let mut last_not_ready = None;
    for lease in handoff {
        let local_lease = store.agent_lease_by_id(&LeaseId::new(lease.lease_id.clone()))?;
        let already_materialized = local_lease.is_some();
        if !already_materialized {
            if let Err(error) = validate_dispatch_materialization_ready(&args, &lease) {
                last_not_ready = Some(error);
                continue;
            }
            materialize_claimed_dispatch_lease(&args, &lease, workspace_content_key)?;
        }
        if local_lease
            .as_ref()
            .is_some_and(|local| local.session_state == AgentSessionState::Completed)
            && lease.session_state != LeaseSessionState::Completed
        {
            let completed =
                acknowledge_handoff_completed(control_plane, workspace_id, device_id, &lease)?;
            return Ok(Some(completed));
        }
        // Emit the read-only materialized acknowledgement once. Already-acked
        // leases fall through so the daemon keeps scanning for fresh handoffs.
        if lease.status_code != HANDOFF_MATERIALIZED_STATUS {
            let acknowledged =
                acknowledge_handoff_materialized(control_plane, workspace_id, device_id, &lease)?;
            return Ok(Some(acknowledged));
        }
    }
    if let Some(error) = last_not_ready {
        return Err(error);
    }
    Ok(None)
}

fn acknowledge_handoff_completed(
    control_plane: &dyn LeaseControlPlaneClient,
    workspace_id: &WorkspaceId,
    device_id: &DeviceId,
    lease: &Lease,
) -> Result<Lease, Box<dyn std::error::Error>> {
    control_plane
        .update_lease(LeaseUpdate {
            workspace_id: workspace_id.clone(),
            lease_id: lease.lease_id.clone(),
            expected_version: lease.version,
            updated_by_device_id: device_id.clone(),
            session_state: Some(LeaseSessionState::Completed),
            status_code: Some(HANDOFF_COMPLETED_STATUS.to_string()),
            event_kind: Some(CompactEventKind::LeaseCompleted),
        })
        .map_err(|error| Box::new(error) as Box<dyn std::error::Error>)
}

fn acknowledge_handoff_materialized(
    control_plane: &dyn LeaseControlPlaneClient,
    workspace_id: &WorkspaceId,
    device_id: &DeviceId,
    lease: &Lease,
) -> Result<Lease, Box<dyn std::error::Error>> {
    control_plane
        .update_lease(LeaseUpdate {
            workspace_id: workspace_id.clone(),
            lease_id: lease.lease_id.clone(),
            expected_version: lease.version,
            updated_by_device_id: device_id.clone(),
            session_state: Some(LeaseSessionState::Open),
            status_code: Some(HANDOFF_MATERIALIZED_STATUS.to_string()),
            event_kind: Some(CompactEventKind::LeaseUpdated),
        })
        .map_err(|error| Box::new(error) as Box<dyn std::error::Error>)
}

fn validate_dispatch_materialization_ready(
    args: &SyncOnceArgs,
    lease: &Lease,
) -> Result<(), Box<dyn std::error::Error>> {
    let workspace_id = WorkspaceId::new(args.workspace_id.clone());
    let project_id = ProjectId::new(lease.project_id.clone());
    let db_path = args.state_root.join(DEFAULT_DATABASE_FILE);
    let store = MetadataStore::open(&db_path)?;
    let project = store
        .project_by_id(&workspace_id, &project_id)?
        .ok_or_else(|| {
            runtime_error(format!(
                "dispatched lease `{}` is waiting for project `{}` to sync locally",
                lease.lease_id, lease.project_id
            ))
        })?;
    let base_snapshot_id = SnapshotId::new(lease.base_snapshot_id.clone());
    let snapshot = store.snapshot(&workspace_id, &base_snapshot_id)?;
    if !snapshot.as_ref().is_some_and(|snapshot| {
        snapshot
            .project_id
            .as_ref()
            .is_none_or(|project_id| project_id == &project.id)
    }) {
        return Err(runtime_error(format!(
            "dispatched lease `{}` is waiting for base snapshot `{}` to sync locally",
            lease.lease_id, lease.base_snapshot_id
        )));
    }
    Ok(())
}

pub(in crate::daemon) fn materialize_claimed_dispatch_lease(
    args: &SyncOnceArgs,
    lease: &Lease,
    workspace_content_key: [u8; 32],
) -> Result<(), Box<dyn std::error::Error>> {
    let workspace_id = WorkspaceId::new(args.workspace_id.clone());
    let project_id = ProjectId::new(lease.project_id.clone());
    let lease_id = LeaseId::new(lease.lease_id.clone());
    let db_path = args.state_root.join(DEFAULT_DATABASE_FILE);
    let store = MetadataStore::open(&db_path)?;
    if store.agent_lease_by_id(&lease_id)?.is_some() {
        return Ok(());
    }
    let project = store
        .project_by_id(&workspace_id, &project_id)?
        .ok_or_else(|| {
            runtime_error(format!(
                "claimed dispatched lease `{}` before project `{}` was materialized locally",
                lease.lease_id, lease.project_id
            ))
        })?;
    let Some(target_device_ref) = lease.target_device_ref.clone() else {
        return Err(runtime_error(format!(
            "claimed dispatched lease `{}` is missing target device",
            lease.lease_id
        )));
    };
    let Some(origin_device_ref) = lease.origin_device_ref.clone() else {
        return Err(runtime_error(format!(
            "claimed dispatched lease `{}` is missing origin device",
            lease.lease_id
        )));
    };
    let Some(task_label) = lease.task_label.clone() else {
        return Err(runtime_error(format!(
            "claimed dispatched lease `{}` is missing task label",
            lease.lease_id
        )));
    };
    let project_path = args.root.join(&project.path);
    create_dispatched_agent_lease(DispatchedAgentLeaseCreateOptions {
        lease: AgentLeaseCreateOptions {
            db_path: Some(db_path),
            project_path: project_path.display().to_string(),
            task: task_label,
            base: AgentLeaseBase::LatestWorkspace,
            work_view: lease.write_target_mode == LeaseWriteTargetMode::WorkView,
            force_stale: false,
            device_id: DeviceId::new(args.device_id.clone()),
            generated_at: current_timestamp(),
        },
        identity: DispatchedAgentLeaseIdentity {
            lease_id,
            base_snapshot_id: SnapshotId::new(lease.base_snapshot_id.clone()),
            work_view_id: lease.work_view_id.clone().map(WorkViewId::new),
            target_device_ref: DeviceId::new(target_device_ref),
            origin_device_ref: DeviceId::new(origin_device_ref),
            expires_at: control_plane_timestamp_rfc3339(lease.expires_at)?,
        },
        workspace_content_key,
    })?;
    Ok(())
}

fn control_plane_timestamp_rfc3339(
    timestamp: ControlPlaneTimestamp,
) -> Result<String, Box<dyn std::error::Error>> {
    let nanos = i128::from(timestamp.tick)
        .checked_mul(1_000_000)
        .ok_or_else(|| runtime_error("control-plane timestamp is out of range"))?;
    let expires_at = OffsetDateTime::from_unix_timestamp_nanos(nanos)
        .map_err(|_| runtime_error("control-plane timestamp is out of range"))?;
    expires_at
        .format(&Rfc3339)
        .map_err(|error| runtime_error(format!("control-plane timestamp format failed: {error}")))
}

pub(in crate::daemon) fn run_sync_once_with(
    args: SyncOnceArgs,
    control_plane: &dyn ControlPlaneClient,
    byte_store: &dyn ByteStore,
    base_ref: bowline_control_plane::WorkspaceRef,
    workspace_id: WorkspaceId,
    device_id: DeviceId,
    workspace_key: LocalWorkspaceKey,
) -> Result<SyncOnceSummary, SyncOnceError> {
    run_sync_once_with_accept_claim(RunSyncOnceRequest {
        args,
        control_plane,
        byte_store,
        base_ref,
        workspace_id,
        device_id,
        workspace_key,
        work_view_accept_claim: None,
    })
}

struct RunSyncOnceRequest<'a> {
    args: SyncOnceArgs,
    control_plane: &'a dyn ControlPlaneClient,
    byte_store: &'a dyn ByteStore,
    base_ref: bowline_control_plane::WorkspaceRef,
    workspace_id: WorkspaceId,
    device_id: DeviceId,
    workspace_key: LocalWorkspaceKey,
    work_view_accept_claim: Option<bowline_local::metadata::WorkViewAcceptClaimHandle>,
}

fn run_sync_once_with_accept_claim(
    request: RunSyncOnceRequest<'_>,
) -> Result<SyncOnceSummary, SyncOnceError> {
    let RunSyncOnceRequest {
        args,
        control_plane,
        byte_store,
        base_ref,
        workspace_id,
        device_id,
        workspace_key,
        work_view_accept_claim,
    } = request;
    let mut runner = SyncRunner::new_with_base_ref(
        control_plane,
        byte_store,
        SyncRunnerOptions {
            root: args.root,
            state_root: args.state_root,
            workspace_id,
            device_id,
            workspace_content_key: workspace_key.bytes,
            storage_key: StorageKey::from_bytes(workspace_key.bytes),
            key_epoch: workspace_key.key_epoch,
            generated_at: current_timestamp(),
            sync_claim: args.sync_claim.clone(),
            scan_scope: args.scan_scope.clone(),
        },
        base_ref.clone(),
    );
    if let Some(claim) = work_view_accept_claim {
        runner = runner.with_work_view_accept_claim(claim);
    }
    let outcome = runner.tick()?;
    let cancelled_late = runner.cancellation_requested_after_commit();
    let scan =
        SyncScanSummary::from_scope_and_stats(&runner.last_scan_scope(), &runner.last_scan_stats());
    match outcome {
        SyncTickOutcome::NoWorkspaceRef => Ok(SyncOnceSummary {
            workspace_id: base_ref.workspace_id.into(),
            snapshot_id: base_ref.snapshot_id.into(),
            version: base_ref.version,
            outcome: SyncSummaryOutcome::NoWorkspaceRef,
            snapshot_root_manifest_id: None,
            manifest_object_key: None,
            namespace_root_id: None,
            conflict_count: 0,
            conflicts: Vec::new(),
            scan,
            cancelled_late,
        }),
        SyncTickOutcome::NoChanges => Ok(SyncOnceSummary {
            workspace_id: base_ref.workspace_id.into(),
            snapshot_id: base_ref.snapshot_id.into(),
            version: base_ref.version,
            outcome: SyncSummaryOutcome::NoChanges,
            snapshot_root_manifest_id: None,
            manifest_object_key: None,
            namespace_root_id: None,
            conflict_count: 0,
            conflicts: Vec::new(),
            scan,
            cancelled_late,
        }),
        SyncTickOutcome::Imported(workspace_ref) => Ok(SyncOnceSummary {
            workspace_id: workspace_ref.workspace_id.into(),
            snapshot_id: workspace_ref.snapshot_id.into(),
            version: workspace_ref.version,
            outcome: SyncSummaryOutcome::Imported,
            snapshot_root_manifest_id: None,
            manifest_object_key: None,
            namespace_root_id: None,
            conflict_count: 0,
            conflicts: Vec::new(),
            scan,
            cancelled_late,
        }),
        SyncTickOutcome::Uploaded(outcome) => match *outcome {
            UploadOutcome::Advanced {
                workspace_ref,
                snapshot_root,
                ..
            } => Ok(summary_from_uploaded(
                workspace_ref,
                snapshot_root,
                SyncSummaryOutcome::Uploaded { stale: false },
                scan.clone(),
                cancelled_late,
            )),
            UploadOutcome::Stale {
                stale,
                snapshot_root,
                ..
            } => Ok(summary_from_uploaded(
                stale.current,
                snapshot_root,
                SyncSummaryOutcome::Uploaded { stale: true },
                scan.clone(),
                cancelled_late,
            )),
        },
        SyncTickOutcome::Merged(outcome) => match *outcome {
            UploadOutcome::Advanced {
                workspace_ref,
                snapshot_root,
                ..
            } => Ok(summary_from_uploaded(
                workspace_ref,
                snapshot_root,
                SyncSummaryOutcome::Merged { stale: false },
                scan.clone(),
                cancelled_late,
            )),
            UploadOutcome::Stale {
                stale,
                snapshot_root,
                ..
            } => Ok(summary_from_uploaded(
                stale.current,
                snapshot_root,
                SyncSummaryOutcome::Merged { stale: true },
                scan.clone(),
                cancelled_late,
            )),
        },
        SyncTickOutcome::Conflicted(conflicts) => Ok(SyncOnceSummary {
            workspace_id: base_ref.workspace_id.into(),
            snapshot_id: base_ref.snapshot_id.into(),
            version: base_ref.version,
            outcome: SyncSummaryOutcome::Conflicted,
            snapshot_root_manifest_id: None,
            manifest_object_key: None,
            namespace_root_id: None,
            conflict_count: conflicts.len(),
            conflicts: conflicts
                .into_iter()
                .map(|conflict| ConflictSummary {
                    id: conflict.id,
                    paths: conflict.paths,
                })
                .collect(),
            scan,
            cancelled_late,
        }),
    }
}

pub(in crate::daemon) fn summary_from_uploaded(
    workspace_ref: bowline_control_plane::WorkspaceRef,
    snapshot_root: bowline_control_plane::SnapshotRootRecord,
    outcome: SyncSummaryOutcome,
    scan: SyncScanSummary,
    cancelled_late: bool,
) -> SyncOnceSummary {
    SyncOnceSummary {
        workspace_id: workspace_ref.workspace_id.into(),
        snapshot_id: workspace_ref.snapshot_id.into(),
        version: workspace_ref.version,
        outcome,
        snapshot_root_manifest_id: Some(snapshot_root.manifest_id.into()),
        manifest_object_key: Some(snapshot_root.manifest_object.object_key),
        namespace_root_id: Some(snapshot_root.namespace_root_id),
        conflict_count: 0,
        conflicts: Vec::new(),
        scan,
        cancelled_late,
    }
}

#[cfg(test)]
mod hosted_observer_tests;
