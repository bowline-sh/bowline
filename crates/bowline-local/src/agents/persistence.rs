use super::*;

pub(super) fn persist_created_agent_lease(
    store: &MetadataStore,
    lease: &AgentLease,
    event_id: EventId,
    generated_at: &str,
) -> Result<(), AgentError> {
    store.persist_agent_lease_with_event(
        lease,
        lease_event(
            lease,
            EventName::LeaseCreated,
            event_id,
            generated_at,
            "Agent lease created.",
        ),
    )?;
    Ok(())
}

pub(super) fn rollback_created_agent_work_view(store: &MetadataStore, lease: &AgentLease) {
    if let Err(error) = store.rollback_created_agent_work_view_metadata(lease) {
        super::log_best_effort_metadata_cleanup("rollback created agent work view", error);
    }
    let work_view_path = expand_display_path(&lease.work_view_path);
    if work_view_path.exists()
        && let Err(error) = fs::remove_dir_all(work_view_path)
    {
        eprintln!("bowline agent filesystem cleanup skipped (remove work view): {error}");
    }
}

pub(super) fn rollback_provisional_agent_lease(store: &MetadataStore, lease: &AgentLease) {
    if let Err(error) = store.delete_agent_lease(&lease.id) {
        super::log_best_effort_metadata_cleanup("rollback provisional agent lease", error);
    }
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

    lease.session_state = AgentSessionState::Open;
    lease.status_summary = "active".to_string();
    lease.updated_at = generated_at.to_string();
    let event_id = EventId::new(format!(
        "evt_lease_recovered_{}",
        stable_token(&format!("{}:{generated_at}", lease.id.as_str()))
    ));
    persist_created_agent_lease(store, &lease, event_id, generated_at)
}

pub(super) fn is_provisional_agent_lease(lease: &AgentLease) -> bool {
    lease.session_state == AgentSessionState::Provisional
        && lease.dispatch_state == AgentLeaseDispatchState::None
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
