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
