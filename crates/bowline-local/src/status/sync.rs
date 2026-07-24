use super::*;

pub(super) fn empty_watermarks() -> EventWatermarks {
    EventWatermarks {
        last_scan_at: None,
        last_event_id: None,
        event_lag_ms: Some(0),
    }
}

pub(super) fn metadata_item(summary: &str, event_name: Option<EventName>) -> StatusItem {
    let mut item = base_status_item(StatusItemKind::Metadata, summary);
    item.subject = Some(StatusSubject {
        kind: StatusSubjectKind::Metadata,
        id: "metadata-local".to_string(),
        path: None,
    });
    item.event_name = event_name;
    item
}

const STALE_BASE_REMEDY_COMMAND: &str = "bowline status --watch";

pub(crate) fn snapshot_stale_bases_from_inputs(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    projects: &[ProjectRecord],
    active_work_views: &[WorkViewRecord],
    project_id: Option<&ProjectId>,
) -> Result<Vec<StaleBaseStatus>, MetadataError> {
    let latest_snapshot_ids = store.project_latest_snapshot_ids(workspace_id)?;
    let mut stale_bases = Vec::new();
    for project in projects {
        if let Some(project_id) = project_id
            && &project.id != project_id
        {
            continue;
        }
        let latest_snapshot_id = latest_snapshot_ids.get(&project.id).cloned();
        let Some(latest_snapshot_id) = latest_snapshot_id else {
            if project_id.is_some() {
                stale_bases.push(StaleBaseStatus::snapshot(
                    FreshnessVerdict::Unknown,
                    format!(
                        "{} has no local project snapshot yet; freshness cannot be proven.",
                        project.path
                    ),
                    Some(project.id.clone()),
                    Some(project.path.clone()),
                    None,
                    None,
                    Some(STALE_BASE_REMEDY_COMMAND.to_string()),
                ));
            }
            continue;
        };

        let project_stale_count = stale_bases.len();
        for view in active_work_views {
            if view.project_id != project.id
                || !matches!(
                    view.lifecycle,
                    WorkViewLifecycle::Active | WorkViewLifecycle::ReviewReady
                )
                || view.base_snapshot_id == latest_snapshot_id
            {
                continue;
            }
            stale_bases.push(StaleBaseStatus::snapshot(
                FreshnessVerdict::Behind,
                format!(
                    "Work view {} is based on an older snapshot for {}.",
                    view.name, project.path
                ),
                Some(project.id.clone()),
                Some(project.path.clone()),
                Some(view.base_snapshot_id.clone()),
                Some(latest_snapshot_id.clone()),
                Some(STALE_BASE_REMEDY_COMMAND.to_string()),
            ));
        }

        if project_id.is_some() && stale_bases.len() == project_stale_count {
            let (verdict, summary, remedy) = match project.git_observer_state {
                GitObserverState::Ok => (
                    FreshnessVerdict::Current,
                    format!("Git observation for {} is current.", project.path),
                    None,
                ),
                GitObserverState::Partial => (
                    FreshnessVerdict::Unknown,
                    format!(
                        "Git observation for {} is partial; freshness cannot be proven.",
                        project.path
                    ),
                    Some(STALE_BASE_REMEDY_COMMAND.to_string()),
                ),
                GitObserverState::Unavailable => (
                    FreshnessVerdict::Unknown,
                    format!(
                        "Git observation for {} is unavailable; freshness cannot be proven.",
                        project.path
                    ),
                    Some(STALE_BASE_REMEDY_COMMAND.to_string()),
                ),
            };
            stale_bases.push(StaleBaseStatus::git(
                verdict,
                summary,
                Some(project.id.clone()),
                Some(project.path.clone()),
                remedy,
            ));
        }
    }
    Ok(stale_bases)
}

pub(crate) fn freshness_for_stale_bases(stale_bases: &[StaleBaseStatus]) -> FreshnessVerdict {
    bowline_core::status::freshness_verdict_for(stale_bases)
}

pub(super) fn apply_stale_base_status(
    stale_bases: &[StaleBaseStatus],
    acc: &mut StatusAccumulator,
) {
    for stale_base in stale_bases {
        if !stale_base.verdict.needs_attention() {
            continue;
        }
        let kind = if stale_base.verdict == FreshnessVerdict::Unknown {
            "observation.freshness_unknown"
        } else if stale_base.verdict == FreshnessVerdict::Diverged {
            "snapshot.base_diverged"
        } else {
            "snapshot.base_behind"
        };
        acc.observe_fact(
            kind,
            format!(
                "stale-base:{}",
                stale_base
                    .project_id
                    .as_ref()
                    .map_or("workspace", ProjectId::as_str)
            ),
            format!(
                "stale-base:{}",
                stale_base
                    .project_id
                    .as_ref()
                    .map_or("workspace", ProjectId::as_str)
            ),
            StatusFactScope::Project,
            stale_base.project_id.as_ref().map(ProjectId::as_str),
        );
        acc.attention_items.push(stale_base.summary.clone());
        let mut item = base_status_item(StatusItemKind::Source, &stale_base.summary);
        item.subject = stale_base
            .project_id
            .as_ref()
            .map(|project_id| StatusSubject {
                kind: StatusSubjectKind::Project,
                id: project_id.as_str().to_string(),
                path: stale_base.project_path.clone(),
            });
        item.path = stale_base.project_path.clone();
        item.project_id = stale_base.project_id.clone();
        item.snapshot_id = stale_base.base_snapshot_id.clone();
        item.event_name = Some(EventName::SourceStale);
        acc.items.push(item);
        if let Some(command) = stale_base.remedy_command.as_ref()
            && !acc
                .next_actions
                .iter()
                .any(|action| action.command.as_deref() == Some(command.as_str()))
        {
            acc.next_actions.push(RepairCommand::inspect(
                "Inspect freshness".to_string(),
                Some(command.clone()),
            ));
        }
    }
}
