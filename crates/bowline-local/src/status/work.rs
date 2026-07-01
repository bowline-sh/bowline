use super::*;

pub(super) fn apply_work_view_metadata(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    project_id: Option<&ProjectId>,
    items: &mut Vec<StatusItem>,
    attention_items: &mut Vec<String>,
    level: &mut StatusLevel,
) -> Result<(), LocalStatusError> {
    for view in store.work_views(workspace_id, true, None)? {
        if let Some(project_id) = project_id
            && &view.project_id != project_id
        {
            continue;
        }
        let needs_attention = matches!(view.lifecycle, WorkViewLifecycle::ReviewReady)
            || matches!(
                view.sync_state,
                WorkViewSyncState::Attention | WorkViewSyncState::Conflicted
            );
        if !needs_attention {
            continue;
        }
        if items.iter().any(|item| {
            item.kind == StatusItemKind::WorkView
                && item
                    .subject
                    .as_ref()
                    .is_some_and(|subject| subject.id == view.id.as_str())
        }) {
            continue;
        }
        if *level == StatusLevel::Healthy {
            *level = StatusLevel::Attention;
        }
        let summary = format!("{} is review-ready; workspace remains usable.", view.name);
        attention_items.push(summary.clone());
        let mut item = base_status_item(StatusItemKind::WorkView, &summary);
        item.subject = Some(StatusSubject {
            kind: StatusSubjectKind::WorkView,
            id: view.id.as_str().to_string(),
            path: Some(view.visible_path.clone()),
        });
        item.path = Some(view.visible_path);
        item.project_id = Some(view.project_id);
        item.event_name = Some(EventName::WorkReviewReady);
        items.push(item);
    }
    Ok(())
}

pub(super) fn apply_agent_lease_metadata(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    project_id: Option<&ProjectId>,
    generated_at: &str,
    items: &mut Vec<StatusItem>,
    attention_items: &mut Vec<String>,
    level: &mut StatusLevel,
) -> Result<(), LocalStatusError> {
    recover_provisional_agent_leases(store, workspace_id, generated_at)
        .map_err(agent_recovery_status_error)?;
    for lease in store.agent_leases(workspace_id)? {
        if let Some(project_id) = project_id
            && &lease.project_id != project_id
        {
            continue;
        }
        let visible = matches!(
            lease.execution_state,
            AgentLeaseExecutionState::Active | AgentLeaseExecutionState::Blocked
        ) || matches!(
            lease.output_state,
            AgentLeaseOutputState::ReviewReady | AgentLeaseOutputState::Conflicted
        );
        if !visible {
            continue;
        }
        let needs_attention =
            matches!(
                lease.output_state,
                AgentLeaseOutputState::ReviewReady | AgentLeaseOutputState::Conflicted
            ) || matches!(lease.execution_state, AgentLeaseExecutionState::Blocked);
        if needs_attention && *level == StatusLevel::Healthy {
            *level = StatusLevel::Attention;
        }
        let summary = match lease.output_state {
            AgentLeaseOutputState::ReviewReady => {
                format!("Agent lease {} is ready for review.", lease.id.as_str())
            }
            AgentLeaseOutputState::Conflicted => {
                format!("Agent lease {} has conflicted output.", lease.id.as_str())
            }
            _ if lease.execution_state == AgentLeaseExecutionState::Blocked => {
                format!("Agent lease {} needs human attention.", lease.id.as_str())
            }
            _ => format!("Agent lease {} is active.", lease.id.as_str()),
        };
        if needs_attention {
            attention_items.push(summary.clone());
        }
        let mut item = base_status_item(StatusItemKind::Lease, &summary);
        item.subject = Some(StatusSubject {
            kind: StatusSubjectKind::Lease,
            id: lease.id.as_str().to_string(),
            path: Some(lease.work_view_path.clone()),
        });
        item.path = Some(lease.work_view_path);
        item.project_id = Some(lease.project_id);
        item.lease_id = Some(lease.id);
        item.event_name = Some(match lease.output_state {
            AgentLeaseOutputState::ReviewReady => EventName::LeaseReviewReady,
            AgentLeaseOutputState::Conflicted => EventName::LeaseBlocked,
            _ if lease.execution_state == AgentLeaseExecutionState::Blocked => {
                EventName::LeaseBlocked
            }
            _ => EventName::LeaseUpdated,
        });
        items.push(item);
    }
    Ok(())
}

pub(super) fn agent_recovery_status_error(error: AgentError) -> LocalStatusError {
    match error {
        AgentError::Metadata(error) => LocalStatusError::Metadata(error),
        AgentError::Event(error) => LocalStatusError::Events(error),
        AgentError::Io(error) => LocalStatusError::Path(error),
        AgentError::WorkView(WorkViewError::Metadata(error)) => LocalStatusError::Metadata(error),
        AgentError::WorkView(WorkViewError::Io(error)) => LocalStatusError::Path(error),
        other => LocalStatusError::Metadata(MetadataError::InvalidStorageMetadata(format!(
            "agent lease recovery failed: {other}"
        ))),
    }
}
