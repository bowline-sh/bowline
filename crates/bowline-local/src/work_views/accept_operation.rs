use bowline_core::{
    commands::{CONTRACT_VERSION, CommandName, WorkLifecycleCommandOutput},
    ids::DeviceId,
    status::{RepairCommand, StatusLevel, WorkspaceStatus},
    work_views::{WorkCommandAction, WorkViewLifecycle},
};

use crate::metadata::{
    MetadataStore, WorkViewAcceptCheckpointStep, WorkViewAcceptEnqueueOutcome,
    WorkViewAcceptFailureReason, WorkViewAcceptOperationRecord, WorkViewAcceptOperationState,
    WorkViewAcceptResourceKey, WorkViewBaseState,
};

use super::{
    WorkSelectorOptions, WorkViewError,
    diff::selected_overlay_deltas,
    overlay,
    paths::{open_store, resolve_work_view},
    status_all_command,
};

pub(crate) fn resolve_accept_paths(
    store: &MetadataStore,
    work_view: &bowline_core::work_views::WorkView,
    path_patterns: &[String],
) -> Result<std::collections::BTreeSet<String>, WorkViewError> {
    if path_patterns.is_empty() {
        return Ok(std::collections::BTreeSet::new());
    }
    let mut paths = std::collections::BTreeSet::new();
    for delta in selected_overlay_deltas(store, work_view, path_patterns)? {
        paths.insert(bowline_core::workspace_graph::normalize_workspace_path(
            &delta.path.display().to_string(),
        ));
        if let overlay::OverlayDeltaKind::Rename { from } = delta.kind
            && !from.as_os_str().is_empty()
        {
            paths.insert(bowline_core::workspace_graph::normalize_workspace_path(
                &from.display().to_string(),
            ));
        }
    }
    Ok(paths)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkViewAcceptProgress {
    Pending {
        operation_id: String,
        state: WorkViewAcceptOperationState,
        phase: WorkViewAcceptPhase,
        completed_steps: u8,
        total_steps: u8,
        partial: bool,
    },
    Terminal(Box<WorkLifecycleCommandOutput>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkViewAcceptPhase {
    Queued,
    CandidateBuilt,
    MainFenceRechecked,
    ObjectsUploaded,
    SnapshotStaged,
    MainPublished,
    WorkspaceRefPublished,
    LifecyclePublished,
    WaitingRetry,
}

pub fn enqueue_work_view_accept(
    options: WorkSelectorOptions,
    device_id: DeviceId,
) -> Result<WorkViewAcceptOperationRecord, WorkViewError> {
    let store = open_store(options.db_path.as_deref())?;
    let work_view = resolve_work_view(&store, &options.selector)?;
    if !matches!(
        work_view.lifecycle,
        WorkViewLifecycle::Active | WorkViewLifecycle::ReviewReady
    ) {
        return Err(WorkViewError::InactiveWorkView {
            name: work_view.name,
        });
    }
    match store.work_view_base_state(&work_view.workspace_id, &work_view.id)? {
        WorkViewBaseState::Authoritative { .. } => {}
        WorkViewBaseState::LegacyUnverifiable => {
            return Err(WorkViewError::SnapshotMaterialization {
                snapshot_id: work_view.base_snapshot_id.as_str().to_string(),
                reason: "legacy-base-unverifiable; recreate this work view before accepting"
                    .to_string(),
            });
        }
        WorkViewBaseState::Missing => {
            return Err(WorkViewError::SnapshotMaterialization {
                snapshot_id: work_view.base_snapshot_id.as_str().to_string(),
                reason: "authoritative exposed base is missing; recreate this work view"
                    .to_string(),
            });
        }
    }
    let selected_paths = resolve_accept_paths(&store, &work_view, &options.paths)?;
    if !options.paths.is_empty() && selected_paths.is_empty() {
        return Err(WorkViewError::EmptyPathSelection {
            patterns: options.paths,
        });
    }
    let selected_paths: Option<Vec<String>> =
        (!options.paths.is_empty()).then(|| selected_paths.into_iter().collect());
    let mut identity = blake3::Hasher::new();
    for component in [
        work_view.workspace_id.as_str(),
        work_view.project_id.as_str(),
        work_view.id.as_str(),
        device_id.as_str(),
        &options.generated_at,
    ] {
        identity.update(component.as_bytes());
        identity.update(&[0]);
    }
    for path in selected_paths.iter().flatten() {
        identity.update(path.as_bytes());
        identity.update(&[0]);
    }
    let digest = identity.finalize().to_hex();
    let operation_id = format!("wva_{}", &digest[..24]);
    let record = WorkViewAcceptOperationRecord {
        id: operation_id.clone(),
        workspace_id: work_view.workspace_id.clone(),
        project_id: work_view.project_id.clone(),
        work_view_id: work_view.id.clone(),
        device_id,
        resource_key: WorkViewAcceptResourceKey::new(
            work_view.workspace_id,
            work_view.project_id,
            work_view.id,
        ),
        idempotency_key: operation_id,
        state: WorkViewAcceptOperationState::Queued,
        selected_paths,
        input_json: serde_json::json!({ "selectorCount": options.paths.len() }).to_string(),
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
        created_at: options.generated_at.clone(),
        updated_at: options.generated_at,
    };
    Ok(match store.enqueue_work_view_accept(&record)? {
        WorkViewAcceptEnqueueOutcome::Inserted(record)
        | WorkViewAcceptEnqueueOutcome::Existing(record) => record,
    })
}

pub fn work_view_accept_progress(
    db_path: Option<&std::path::Path>,
    operation_id: &str,
    generated_at: String,
) -> Result<WorkViewAcceptProgress, WorkViewError> {
    let store = open_store(db_path)?;
    let operation = store
        .work_view_accept_operation(operation_id)?
        .ok_or_else(|| WorkViewError::AcceptOperationMissing {
            operation_id: operation_id.to_string(),
        })?;
    match operation.state {
        WorkViewAcceptOperationState::Queued
        | WorkViewAcceptOperationState::Claimed
        | WorkViewAcceptOperationState::WaitingRetry => {
            let checkpoints = store.work_view_accept_checkpoints(&operation.id)?;
            let completed_steps = u8::try_from(checkpoints.len().min(7))
                .expect("bounded checkpoint count fits in u8");
            let phase = if operation.state == WorkViewAcceptOperationState::WaitingRetry {
                WorkViewAcceptPhase::WaitingRetry
            } else {
                checkpoints
                    .last()
                    .map_or(WorkViewAcceptPhase::Queued, |checkpoint| {
                        accept_phase(checkpoint.step)
                    })
            };
            Ok(WorkViewAcceptProgress::Pending {
                operation_id: operation.id,
                state: operation.state,
                phase,
                completed_steps,
                total_steps: 7,
                partial: operation.selected_paths.is_some(),
            })
        }
        WorkViewAcceptOperationState::Failed => Err(WorkViewError::AcceptOperationFailed {
            operation_id: operation.id,
            reason: operation
                .failure_reason
                .map_or("unknown", failure_reason_code),
        }),
        WorkViewAcceptOperationState::Cancelled => Err(WorkViewError::AcceptOperationCancelled {
            operation_id: operation.id,
        }),
        WorkViewAcceptOperationState::Completed | WorkViewAcceptOperationState::ReviewRequired => {
            terminal_output(&store, operation, generated_at)
                .map(Box::new)
                .map(WorkViewAcceptProgress::Terminal)
        }
    }
}

fn accept_phase(step: WorkViewAcceptCheckpointStep) -> WorkViewAcceptPhase {
    match step {
        WorkViewAcceptCheckpointStep::CandidateBuilt => WorkViewAcceptPhase::CandidateBuilt,
        WorkViewAcceptCheckpointStep::MainFenceRechecked => WorkViewAcceptPhase::MainFenceRechecked,
        WorkViewAcceptCheckpointStep::ObjectsUploaded => WorkViewAcceptPhase::ObjectsUploaded,
        WorkViewAcceptCheckpointStep::SnapshotStaged => WorkViewAcceptPhase::SnapshotStaged,
        WorkViewAcceptCheckpointStep::MainPublished => WorkViewAcceptPhase::MainPublished,
        WorkViewAcceptCheckpointStep::WorkspaceRefPublished => {
            WorkViewAcceptPhase::WorkspaceRefPublished
        }
        WorkViewAcceptCheckpointStep::LifecyclePublished => WorkViewAcceptPhase::LifecyclePublished,
    }
}

fn terminal_output(
    store: &MetadataStore,
    operation: WorkViewAcceptOperationRecord,
    generated_at: String,
) -> Result<WorkLifecycleCommandOutput, WorkViewError> {
    let work_view = store
        .work_view_by_id(&operation.workspace_id, &operation.work_view_id)?
        .ok_or_else(|| WorkViewError::AcceptOperationMissing {
            operation_id: operation.id.clone(),
        })?;
    let partial = operation.selected_paths.is_some();
    let paths = operation.selected_paths.unwrap_or_default();
    if operation.state == WorkViewAcceptOperationState::ReviewRequired {
        return Ok(WorkLifecycleCommandOutput {
            contract_version: CONTRACT_VERSION,
            command: CommandName::Accept,
            generated_at,
            action: WorkCommandAction::ReviewReady,
            paths,
            partial,
            work_view,
            status: WorkspaceStatus {
                level: StatusLevel::Attention,
                attention_items: vec![
                    "Accept needs review before touching the main view.".to_string(),
                ],
            },
            next_actions: vec![RepairCommand::inspect(
                "Inspect work-view diff".to_string(),
                Some(format!(
                    "bowline work review {}",
                    operation.work_view_id.as_str()
                )),
            )],
        });
    }
    let status_command = status_all_command(store, &operation.workspace_id)?;
    Ok(WorkLifecycleCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::Accept,
        generated_at,
        action: WorkCommandAction::Accepted,
        paths,
        partial,
        work_view,
        status: WorkspaceStatus::healthy(),
        next_actions: vec![RepairCommand::inspect(
            "Inspect workspace status".to_string(),
            Some(status_command),
        )],
    })
}

fn failure_reason_code(reason: WorkViewAcceptFailureReason) -> &'static str {
    match reason {
        WorkViewAcceptFailureReason::Transient => "transient",
        WorkViewAcceptFailureReason::Permanent => "permanent",
    }
}
