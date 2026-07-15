use super::*;

pub(super) fn apply_work_view_metadata(
    work_views: &[WorkViewRecord],
    project_id: Option<&ProjectId>,
    acc: &mut StatusAccumulator,
) {
    for view in work_views {
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
        if acc.items.iter().any(|item| {
            item.kind == StatusItemKind::WorkView
                && item
                    .subject
                    .as_ref()
                    .is_some_and(|subject| subject.id == view.id.as_str())
        }) {
            continue;
        }
        acc.observe_fact(
            if view.sync_state == WorkViewSyncState::Conflicted {
                "work_view.conflicted"
            } else {
                "work_view.review_ready"
            },
            format!("work-view:{}", view.id.as_str()),
            format!("work-view:{}", view.id.as_str()),
            StatusFactScope::WorkView,
            Some(view.id.as_str()),
        );
        let summary = format!("{} is review-ready; workspace remains usable.", view.name);
        acc.attention_items.push(summary.clone());
        let mut item = base_status_item(StatusItemKind::WorkView, &summary);
        item.subject = Some(StatusSubject {
            kind: StatusSubjectKind::WorkView,
            id: view.id.as_str().to_string(),
            path: Some(view.visible_path.clone()),
        });
        item.path = Some(view.visible_path.clone());
        item.project_id = Some(view.project_id.clone());
        item.event_name = Some(EventName::WorkReviewReady);
        acc.items.push(item);
    }
}

pub(super) fn apply_agent_lease_metadata(
    agent_leases: &[AgentLeaseRecord],
    work_views: &[WorkViewRecord],
    project_id: Option<&ProjectId>,
    acc: &mut StatusAccumulator,
) {
    for lease in agent_leases {
        if let Some(project_id) = project_id
            && &lease.project_id != project_id
        {
            continue;
        }
        let visible = matches!(
            lease.session_state,
            AgentSessionState::Open | AgentSessionState::Provisional | AgentSessionState::Completed
        );
        if !visible {
            continue;
        }
        let completed = matches!(lease.session_state, AgentSessionState::Completed);
        let review_ready = completed
            && matches!(
                lease.write_target_mode,
                bowline_core::commands::AgentWriteTargetMode::WorkView
            )
            && work_views.iter().any(|view| {
                view.id == lease.work_view_id
                    && matches!(
                        view.lifecycle,
                        WorkViewLifecycle::Active | WorkViewLifecycle::ReviewReady
                    )
            });
        let needs_attention = review_ready;
        if needs_attention {
            acc.observe_fact(
                "lease.review_ready",
                format!("lease:{}", lease.id.as_str()),
                format!("lease:{}", lease.id.as_str()),
                StatusFactScope::Lease,
                Some(lease.id.as_str()),
            );
        }
        let summary = if review_ready {
            format!("Agent session {} is review-ready.", lease.id.as_str())
        } else if completed {
            format!(
                "Agent session {} completed; inspect synced project state.",
                lease.id.as_str()
            )
        } else {
            format!("Agent session {} is open.", lease.id.as_str())
        };
        if needs_attention {
            acc.attention_items.push(summary.clone());
        }
        let mut item = base_status_item(StatusItemKind::Lease, &summary);
        item.subject = Some(StatusSubject {
            kind: StatusSubjectKind::Lease,
            id: lease.id.as_str().to_string(),
            path: Some(lease.work_view_path.clone()),
        });
        item.path = Some(lease.work_view_path.clone());
        item.project_id = Some(lease.project_id.clone());
        item.lease_id = Some(lease.id.clone());
        item.event_name = Some(if review_ready {
            EventName::LeaseReviewReady
        } else {
            EventName::LeaseUpdated
        });
        acc.items.push(item);
    }
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
