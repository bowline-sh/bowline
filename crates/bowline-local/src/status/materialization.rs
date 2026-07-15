use super::*;

pub(super) fn apply_materialization_status(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    acc: &mut StatusAccumulator,
) -> Result<(), LocalStatusError> {
    let Some(head) = store.workspace_sync_head(workspace_id)? else {
        return Ok(());
    };
    let tasks =
        store.materialization_tasks_for_snapshot(workspace_id, &head.workspace_ref.snapshot_id)?;
    if tasks.is_empty() {
        return Ok(());
    }
    let total = tasks.len() as u64;
    let ready = tasks
        .iter()
        .filter(|task| task.state == MaterializationTaskState::Ready)
        .count() as u64;
    let total_bytes = tasks.iter().map(|task| task.expected_byte_len).sum::<u64>();
    let ready_bytes = tasks
        .iter()
        .filter(|task| task.state == MaterializationTaskState::Ready)
        .map(|task| task.expected_byte_len)
        .sum::<u64>();
    let blocked_offline = tasks
        .iter()
        .any(|task| task.state == MaterializationTaskState::BlockedOffline);
    let blocked_conflict = tasks
        .iter()
        .any(|task| task.state == MaterializationTaskState::BlockedConflict);
    let blocked_missing = tasks
        .iter()
        .any(|task| task.state == MaterializationTaskState::BlockedMissing);
    let attention = tasks
        .iter()
        .any(|task| task.state == MaterializationTaskState::Attention);

    let summary = if ready == total {
        format!("Workspace files ready: {ready}/{total} paths ({ready_bytes}/{total_bytes} bytes).")
    } else if blocked_offline {
        format!(
            "Materializing files: {ready}/{total} paths ready; remaining remote bytes are waiting for network access."
        )
    } else if blocked_conflict {
        format!(
            "Materializing files: {ready}/{total} paths ready; local changes block at least one queued path."
        )
    } else if blocked_missing {
        format!(
            "Materializing files: {ready}/{total} paths ready; remote content is missing or inconsistent."
        )
    } else {
        format!(
            "Materializing files: {ready}/{total} paths ready ({ready_bytes}/{total_bytes} bytes)."
        )
    };
    let mut item = base_status_item(StatusItemKind::Materialization, &summary);
    item.subject = Some(StatusSubject {
        kind: StatusSubjectKind::Workspace,
        id: workspace_id.as_str().to_string(),
        path: None,
    });
    acc.items.push(item);

    if ready < total {
        acc.observe_fact(
            "project.not_materialized",
            format!("materialization:{workspace_id}"),
            format!("materialization:{workspace_id}"),
            StatusFactScope::Workspace,
            Some(workspace_id.as_str()),
        );
    }
    if blocked_offline {
        acc.limits.push(LimitedCapability {
            capability: "workspace file materialization".to_string(),
            unavailable_because: "network is offline".to_string(),
            still_works: vec!["ready local files".to_string(), "local edits".to_string()],
            path: None,
            support_capability: None,
        });
    }
    if blocked_conflict || blocked_missing || attention {
        acc.observe_fact(
            "project.materialization_blocked",
            format!("materialization-blocked:{workspace_id}"),
            format!("materialization-blocked:{workspace_id}"),
            StatusFactScope::Workspace,
            Some(workspace_id.as_str()),
        );
        acc.attention_items
            .push("Workspace materialization needs attention.".to_string());
    }
    Ok(())
}
