use super::*;

pub(super) fn audit_tool_result(
    store: &MetadataStore,
    lease: &AgentLease,
    mut result: AgentToolResult,
    generated_at: &str,
) -> Result<AgentToolResult, AgentError> {
    if result.outcome == AgentToolResultOutcome::Allowed {
        if let Some((event_name, summary)) = success_event_for_tool(result.tool) {
            let event_id = EventId::new(format!(
                "evt_tool_invoked_{}_{}",
                lease.id.as_str(),
                stable_token(&format!(
                    "{}:{}:{}:{}",
                    result.request_id,
                    serde_json::to_string(&result.tool).unwrap_or_default(),
                    generated_at,
                    result.summary,
                ))
            ));
            append_lease_event(
                store,
                lease,
                event_name,
                event_id.clone(),
                generated_at,
                summary,
            )?;
            result.event_id = Some(event_id);
        }
        return Ok(result);
    }
    if result.outcome != AgentToolResultOutcome::Denied {
        return Ok(result);
    }
    let event_id = EventId::new(format!(
        "evt_tool_denied_{}_{}",
        lease.id.as_str(),
        stable_token(&format!(
            "{}:{}:{}",
            result.request_id,
            serde_json::to_string(&result.tool).unwrap_or_default(),
            generated_at
        ))
    ));
    append_lease_event(
        store,
        lease,
        EventName::LeaseToolDenied,
        event_id.clone(),
        generated_at,
        "Agent tool request was denied.",
    )?;
    result.event_id = Some(event_id);
    Ok(result)
}

pub(super) fn success_event_for_tool(tool: AgentToolName) -> Option<(EventName, &'static str)> {
    match tool {
        AgentToolName::WriteOverlayFile => {
            Some((EventName::OverlayChanged, "Agent overlay changed."))
        }
        AgentToolName::RunCommandWithReceipt => {
            Some((EventName::LeaseToolInvoked, "Agent command tool completed."))
        }
        AgentToolName::RequestHydration => Some((
            EventName::LeaseToolInvoked,
            "Agent hydration tool completed.",
        )),
        AgentToolName::PublishOverlayForReview => {
            Some((EventName::PublishRequested, "Agent publish requested."))
        }
        AgentToolName::CompleteTask => Some((
            EventName::LeaseToolInvoked,
            "Agent completion tool completed.",
        )),
        _ => None,
    }
}

pub(super) fn append_lease_event(
    store: &MetadataStore,
    lease: &AgentLease,
    name: EventName,
    event_id: EventId,
    generated_at: &str,
    summary: &str,
) -> Result<(), AgentError> {
    store.append_event(lease_event(lease, name, event_id, generated_at, summary))?;
    Ok(())
}

pub(super) fn persist_created_agent_lease(
    store: &MetadataStore,
    lease: &AgentLease,
    event_id: EventId,
    generated_at: &str,
) -> Result<(), AgentError> {
    store
        .connection()
        .execute("BEGIN IMMEDIATE", [])
        .map_err(MetadataError::from)?;
    let result = (|| {
        store.upsert_agent_lease(lease)?;
        store.append_event(lease_event(
            lease,
            EventName::LeaseCreated,
            event_id,
            generated_at,
            "Agent lease created.",
        ))?;
        Ok::<(), AgentError>(())
    })();
    match result {
        Ok(()) => {
            store
                .connection()
                .execute("COMMIT", [])
                .map_err(MetadataError::from)?;
            Ok(())
        }
        Err(error) => {
            let _ = store.connection().execute("ROLLBACK", []);
            Err(error)
        }
    }
}

pub(super) fn rollback_created_agent_work_view(store: &MetadataStore, lease: &AgentLease) {
    let _ = store.connection().execute(
        "DELETE FROM leases WHERE id = ?1",
        rusqlite::params![lease.id.as_str()],
    );
    let _ = store.connection().execute(
        "DELETE FROM work_view_base_files WHERE workspace_id = ?1 AND work_view_id = ?2",
        rusqlite::params![lease.workspace_id.as_str(), lease.work_view_id.as_str()],
    );
    let _ = store.connection().execute(
        "DELETE FROM work_views WHERE workspace_id = ?1 AND id = ?2",
        rusqlite::params![lease.workspace_id.as_str(), lease.work_view_id.as_str()],
    );
    let work_view_path = expand_display_path(&lease.work_view_path);
    if work_view_path.exists() {
        let _ = fs::remove_dir_all(work_view_path);
    }
}

pub(super) fn rollback_provisional_agent_lease(store: &MetadataStore, lease: &AgentLease) {
    let _ = store.connection().execute(
        "DELETE FROM leases WHERE id = ?1",
        rusqlite::params![lease.id.as_str()],
    );
}

pub(crate) fn recover_provisional_agent_leases(
    store: &MetadataStore,
    workspace_id: &bowline_core::ids::WorkspaceId,
    generated_at: &str,
) -> Result<(), AgentError> {
    for lease in store.agent_leases(workspace_id)? {
        recover_provisional_agent_lease(store, lease, generated_at)?;
    }
    Ok(())
}

pub(super) fn recover_provisional_agent_lease_by_id(
    store: &MetadataStore,
    lease_id: &LeaseId,
    generated_at: &str,
) -> Result<(), AgentError> {
    if let Some(lease) = store.agent_lease_by_id(lease_id)? {
        recover_provisional_agent_lease(store, lease, generated_at)?;
    }
    Ok(())
}

pub(super) fn recover_provisional_agent_lease(
    store: &MetadataStore,
    mut lease: AgentLease,
    generated_at: &str,
) -> Result<(), AgentError> {
    if !is_provisional_agent_lease(&lease) {
        return Ok(());
    }

    let Some(work_view) = store.work_view_by_id(&lease.workspace_id, &lease.work_view_id)? else {
        rollback_provisional_agent_lease(store, &lease);
        return Ok(());
    };

    if !expand_display_path(&work_view.visible_path).is_dir() {
        rollback_created_agent_work_view(store, &lease);
        return Ok(());
    }

    lease.execution_state = AgentLeaseExecutionState::Active;
    lease.status_summary = "active".to_string();
    lease.updated_at = generated_at.to_string();
    persist_created_agent_lease(
        store,
        &lease,
        lease.audit.local_event_id.clone(),
        generated_at,
    )
}

pub(super) fn is_provisional_agent_lease(lease: &AgentLease) -> bool {
    lease.execution_state == AgentLeaseExecutionState::Blocked && lease.status_summary == "creating"
}

pub(super) fn lease_event(
    lease: &AgentLease,
    name: EventName,
    event_id: EventId,
    generated_at: &str,
    summary: &str,
) -> WorkspaceEvent {
    let mut event = WorkspaceEvent::new(
        event_id,
        name,
        generated_at,
        EventSeverity::Info,
        summary,
        lease.workspace_id.clone(),
    );
    event.project_id = Some(lease.project_id.clone());
    event.lease_id = Some(lease.id.clone());
    event.device_id = Some(lease.device_id.clone());
    event.subject = Some(EventSubject {
        kind: EventSubjectKind::Lease,
        id: lease.id.as_str().to_string(),
        path: Some(lease_write_target_path(lease).to_string()),
    });
    event
}

pub(super) fn load_lease(
    store: &MetadataStore,
    lease_id: &LeaseId,
    generated_at: &str,
) -> Result<AgentLease, AgentError> {
    recover_provisional_agent_lease_by_id(store, lease_id, generated_at)?;
    store
        .agent_lease_by_id(lease_id)?
        .ok_or_else(|| AgentError::MissingLease {
            lease_id: lease_id.clone(),
        })
}
