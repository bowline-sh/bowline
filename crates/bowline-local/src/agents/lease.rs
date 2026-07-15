use super::*;

pub fn create_agent_lease(
    options: AgentLeaseCreateOptions,
) -> Result<AgentLeaseCreateCommandOutput, AgentError> {
    create_agent_lease_with_identity(options, None, None)
}

pub fn create_dispatched_agent_lease(
    options: DispatchedAgentLeaseCreateOptions,
) -> Result<AgentLeaseCreateCommandOutput, AgentError> {
    if options.lease.work_view && options.identity.work_view_id.is_none() {
        return Err(AgentError::InvalidLease {
            reason: "dispatched work-view leases require workViewId".to_string(),
        });
    }
    create_agent_lease_with_identity(
        options.lease,
        Some(options.identity),
        Some(options.workspace_content_key),
    )
}

fn create_agent_lease_with_identity(
    options: AgentLeaseCreateOptions,
    remote_identity: Option<DispatchedAgentLeaseIdentity>,
    workspace_content_key: Option<[u8; 32]>,
) -> Result<AgentLeaseCreateCommandOutput, AgentError> {
    let start = resolve_agent_lease_start(&options)?;
    validate_agent_lease_base(&options)?;
    recover_provisional_agent_leases(&start.store, &start.workspace.id, &options.generated_at)?;
    validate_agent_lease_freshness(&start, &options)?;
    let mut draft = build_agent_lease_draft(&options, &start)?;
    if let Some(identity) = remote_identity.as_ref() {
        validate_dispatched_agent_lease_base(&start, identity)?;
        apply_dispatched_agent_lease_identity(&mut draft, identity);
    }
    validate_agent_lease_uniqueness(&start.store, &start.workspace.id, &draft.lease, &options)?;
    start.store.upsert_agent_lease(&draft.lease)?;
    if options.work_view {
        activate_work_view_agent_lease(start, draft, options, workspace_content_key)
    } else {
        activate_direct_agent_lease(start, draft, options)
    }
}

#[derive(Debug, Clone)]
pub struct DispatchedAgentLeaseIdentity {
    pub lease_id: LeaseId,
    pub base_snapshot_id: SnapshotId,
    pub work_view_id: Option<WorkViewId>,
    pub target_device_ref: DeviceId,
    pub origin_device_ref: DeviceId,
    pub expires_at: String,
}

struct ResolvedAgentLeaseStart {
    db_path: PathBuf,
    store: MetadataStore,
    workspace: crate::metadata::WorkspaceRecord,
    project: crate::metadata::ProjectRecord,
    root: String,
    base_snapshot_id: bowline_core::ids::SnapshotId,
}

struct AgentLeaseDraft {
    lease: AgentLease,
    event_id: EventId,
    lease_name: String,
    work_view_base_snapshot_selector: Option<String>,
    work_view_id_override: Option<WorkViewId>,
}

fn resolve_agent_lease_start(
    options: &AgentLeaseCreateOptions,
) -> Result<ResolvedAgentLeaseStart, AgentError> {
    let db_path = resolve_db_path(options.db_path.clone())?;
    let store = MetadataStore::open(&db_path)?;
    let workspace = store
        .current_workspace()?
        .ok_or(AgentError::MissingWorkspace)?;
    let project = match store.current_project_by_path(&options.project_path)? {
        Some(project) => project,
        None => adopt_materialized_project(&store, &workspace.id, &options.project_path)?
            .ok_or_else(|| AgentError::MissingProject {
                path: options.project_path.clone(),
            })?,
    };
    let root = store
        .current_workspace_root()?
        .ok_or_else(|| AgentError::InvalidLease {
            reason: "workspace root is missing".to_string(),
        })?;
    let base_snapshot_id = store
        .project_latest_snapshot_id(&workspace.id, &project.id)?
        .ok_or_else(|| AgentError::InvalidLease {
            reason: format!(
                "project `{}` needs a fresh snapshot before an agent lease can start",
                project.path
            ),
        })?;
    Ok(ResolvedAgentLeaseStart {
        db_path,
        store,
        workspace,
        project,
        root,
        base_snapshot_id,
    })
}

fn validate_agent_lease_base(options: &AgentLeaseCreateOptions) -> Result<(), AgentError> {
    if options.base == AgentLeaseBase::LatestMain {
        return Err(AgentError::InvalidLease {
            reason: "latest:main base is unavailable until read-only Git observer freshness is available"
                .to_string(),
        });
    }
    Ok(())
}

fn build_agent_lease_draft(
    options: &AgentLeaseCreateOptions,
    start: &ResolvedAgentLeaseStart,
) -> Result<AgentLeaseDraft, AgentError> {
    let lease_name = lease_work_view_name(&options.task, &options.generated_at);
    let lease_token = stable_token(&format!(
        "{}:{}:{}:{}",
        start.workspace.id.as_str(),
        start.project.id.as_str(),
        lease_name,
        options.generated_at
    ));
    let lease_id = LeaseId::new(format!("lease_{lease_token}"));
    let work_view_id = agent_work_view_id(
        start.workspace.id.as_str(),
        start.project.id.as_str(),
        &lease_name,
    );
    let project_target_path = display_path_for_project(&start.root, &start.project.path);
    let work_view_path =
        display_path_for_agent_work_view(&start.root, &start.project.path, &lease_name);
    let write_target_mode = if options.work_view {
        AgentWriteTargetMode::WorkView
    } else {
        AgentWriteTargetMode::Direct
    };
    let write_target_path = if options.work_view {
        work_view_path
    } else {
        project_target_path
    };
    let event_id = EventId::new(format!(
        "evt_lease_created_{}",
        stable_token(lease_id.as_str())
    ));
    Ok(AgentLeaseDraft {
        lease: AgentLease {
            id: lease_id,
            workspace_id: start.workspace.id.clone(),
            project_id: start.project.id.clone(),
            device_id: options.device_id.clone(),
            dispatch_state: AgentLeaseDispatchState::None,
            target_device_ref: None,
            origin_device_ref: None,
            write_target_mode,
            write_target_path: write_target_path.clone(),
            work_view_id: work_view_id.clone(),
            work_view_path: write_target_path.clone(),
            task: redacted_task_label(&options.task),
            base: options.base,
            base_snapshot_id: start.base_snapshot_id.clone(),
            session_state: AgentSessionState::Provisional,
            status_summary: AGENT_LEASE_STATUS_CREATING.to_string(),
            expires_at: default_agent_lease_expiry(&options.generated_at)?,
            created_at: options.generated_at.clone(),
            updated_at: options.generated_at.clone(),
        },
        event_id,
        lease_name,
        work_view_base_snapshot_selector: None,
        work_view_id_override: None,
    })
}

fn default_agent_lease_expiry(generated_at: &str) -> Result<String, AgentError> {
    let generated_at =
        OffsetDateTime::parse(generated_at, &Rfc3339).map_err(|_| AgentError::InvalidLease {
            reason: "agent lease generatedAt must be an RFC 3339 timestamp".to_string(),
        })?;
    (generated_at + time::Duration::hours(24))
        .format(&Rfc3339)
        .map_err(|_| AgentError::InvalidLease {
            reason: "agent lease expiry could not be formatted".to_string(),
        })
}

fn validate_dispatched_agent_lease_base(
    start: &ResolvedAgentLeaseStart,
    identity: &DispatchedAgentLeaseIdentity,
) -> Result<(), AgentError> {
    let snapshot = start
        .store
        .snapshot(&start.workspace.id, &identity.base_snapshot_id)?;
    if snapshot.as_ref().is_some_and(|snapshot| {
        snapshot
            .project_id
            .as_ref()
            .is_none_or(|project_id| project_id == &start.project.id)
    }) {
        return Ok(());
    }
    Err(AgentError::InvalidLease {
        reason: format!(
            "dispatched lease base `{}` is not available locally for {}",
            identity.base_snapshot_id.as_str(),
            start.project.path
        ),
    })
}

fn apply_dispatched_agent_lease_identity(
    draft: &mut AgentLeaseDraft,
    identity: &DispatchedAgentLeaseIdentity,
) {
    draft.lease.id = identity.lease_id.clone();
    draft.lease.base_snapshot_id = identity.base_snapshot_id.clone();
    if let Some(work_view_id) = identity.work_view_id.as_ref() {
        draft.lease.work_view_id = work_view_id.clone();
        draft.work_view_id_override = Some(work_view_id.clone());
    }
    draft.lease.dispatch_state = AgentLeaseDispatchState::Claimed;
    draft.lease.target_device_ref = Some(identity.target_device_ref.clone());
    draft.lease.origin_device_ref = Some(identity.origin_device_ref.clone());
    draft.lease.expires_at = identity.expires_at.clone();
    draft.work_view_base_snapshot_selector = Some(identity.base_snapshot_id.as_str().to_string());
    draft.event_id = EventId::new(format!(
        "evt_lease_claimed_{}",
        stable_token(identity.lease_id.as_str())
    ));
}

fn validate_agent_lease_freshness(
    start: &ResolvedAgentLeaseStart,
    options: &AgentLeaseCreateOptions,
) -> Result<bool, AgentError> {
    validate_agent_lease_freshness_with_store(&start.store, start, options)
}

fn validate_agent_lease_freshness_with_store(
    store: &MetadataStore,
    start: &ResolvedAgentLeaseStart,
    options: &AgentLeaseCreateOptions,
) -> Result<bool, AgentError> {
    let project_stale_bases =
        crate::status::snapshot_stale_bases(store, &start.workspace.id, Some(&start.project.id))?;
    if let Some(stale_base) = project_stale_bases
        .iter()
        .find(|status| status.verdict.is_stale())
    {
        if options.force_stale {
            return Ok(true);
        }
        return Err(AgentError::StaleBaseHeld {
            summary: stale_base.summary.clone(),
            remedy_command: stale_base
                .remedy_command
                .clone()
                .unwrap_or_else(|| "bowline status --watch".to_string()),
        });
    }

    let latest_snapshot_id =
        store.project_latest_snapshot_id(&start.workspace.id, &start.project.id)?;
    if latest_snapshot_id.as_ref() == Some(&start.base_snapshot_id) {
        return Ok(false);
    }
    if options.force_stale {
        return Ok(true);
    }
    Err(AgentError::StaleBaseHeld {
        summary: format!(
            "Requested base `{}` is no longer the latest snapshot for {}.",
            start.base_snapshot_id.as_str(),
            start.project.path
        ),
        remedy_command: "bowline status --watch".to_string(),
    })
}

fn validate_agent_lease_uniqueness(
    store: &MetadataStore,
    workspace_id: &bowline_core::ids::WorkspaceId,
    lease: &AgentLease,
    options: &AgentLeaseCreateOptions,
) -> Result<(), AgentError> {
    recover_provisional_agent_lease_by_id(store, &lease.id, &options.generated_at)?;
    if store.agent_lease_by_id(&lease.id)?.is_some() {
        return Err(AgentError::InvalidLease {
            reason: "agent lease already exists".to_string(),
        });
    }
    if options.work_view
        && store
            .work_view_by_id(workspace_id, &lease.work_view_id)?
            .is_some()
    {
        return Err(AgentError::InvalidLease {
            reason: "agent lease work view already exists".to_string(),
        });
    }
    Ok(())
}

fn activate_direct_agent_lease(
    start: ResolvedAgentLeaseStart,
    draft: AgentLeaseDraft,
    options: AgentLeaseCreateOptions,
) -> Result<AgentLeaseCreateCommandOutput, AgentError> {
    let mut lease = draft.lease;
    if let Err(error) =
        persist_activated_agent_lease(&start.store, &start, &options, &mut lease, draft.event_id)
    {
        rollback_provisional_agent_lease(&start.store, &lease);
        return Err(error);
    }
    Ok(agent_lease_create_output(
        options.generated_at,
        start.workspace.id,
        start.project.id,
        lease,
    ))
}

fn activate_work_view_agent_lease(
    start: ResolvedAgentLeaseStart,
    draft: AgentLeaseDraft,
    options: AgentLeaseCreateOptions,
    workspace_content_key: Option<[u8; 32]>,
) -> Result<AgentLeaseCreateCommandOutput, AgentError> {
    let work_output = create_work_view_with_id_and_key(
        WorkCreateOptions {
            db_path: Some(start.db_path.clone()),
            project_path: options.project_path.clone(),
            name: draft.lease_name,
            base_snapshot_selector: draft.work_view_base_snapshot_selector.clone(),
            owner_device_id: Some(options.device_id.clone()),
            generated_at: options.generated_at.clone(),
        },
        draft.work_view_id_override.clone(),
        workspace_content_key,
    );
    let store = MetadataStore::open(&start.db_path)?;
    let mut lease = draft.lease;
    let work_output = match work_output {
        Ok(output) => output,
        Err(error) => {
            rollback_provisional_agent_lease(&store, &lease);
            return Err(error.into());
        }
    };
    debug_assert_eq!(work_output.work_view.id, lease.work_view_id);
    debug_assert_eq!(work_output.work_view.visible_path, lease.work_view_path);
    if let Err(error) =
        persist_activated_agent_lease(&store, &start, &options, &mut lease, draft.event_id)
    {
        rollback_created_agent_work_view(&store, &lease);
        return Err(error);
    }
    Ok(agent_lease_create_output(
        options.generated_at,
        start.workspace.id,
        start.project.id,
        lease,
    ))
}

fn active_status_summary(stale_base_override: bool) -> String {
    if stale_base_override {
        "active with stale-base override".to_string()
    } else {
        "active".to_string()
    }
}

fn persist_activated_agent_lease(
    store: &MetadataStore,
    start: &ResolvedAgentLeaseStart,
    options: &AgentLeaseCreateOptions,
    lease: &mut AgentLease,
    event_id: EventId,
) -> Result<(), AgentError> {
    store.in_immediate_transaction(|| {
        let stale_base_override = validate_agent_lease_freshness(start, options)?;
        lease.session_state = AgentSessionState::Open;
        lease.status_summary = active_status_summary(stale_base_override);
        store.upsert_agent_lease(lease)?;
        store.append_event(lease_event(
            lease,
            EventName::LeaseCreated,
            event_id,
            &options.generated_at,
            "Agent lease created.",
        ))?;
        if stale_base_override {
            store.append_event(lease_event(
                lease,
                EventName::LeaseUpdated,
                EventId::new(format!(
                    "evt_lease_stale_override_{}",
                    stable_token(&format!("{}:{}", lease.id.as_str(), options.generated_at))
                )),
                &options.generated_at,
                "Agent lease started with stale-base override.",
            ))?;
        }
        Ok::<(), AgentError>(())
    })
}

fn agent_lease_create_output(
    generated_at: String,
    workspace_id: bowline_core::ids::WorkspaceId,
    project_id: ProjectId,
    lease: AgentLease,
) -> AgentLeaseCreateCommandOutput {
    AgentLeaseCreateCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::AgentStart,
        generated_at,
        workspace_id,
        project_id,
        lease,
        status: WorkspaceStatus::healthy(),
        // Agent-output next_actions are emitted empty (067 handshake).
        next_actions: Vec::new(),
    }
}

pub(super) fn adopt_materialized_project(
    store: &MetadataStore,
    workspace_id: &bowline_core::ids::WorkspaceId,
    requested_path: &str,
) -> Result<Option<crate::metadata::ProjectRecord>, AgentError> {
    let Some(root) = store.current_workspace_root()? else {
        return Ok(None);
    };
    let root_path = expand_display_path(&root);
    let requested = expand_display_path(requested_path);
    let absolute_project = if requested.is_absolute() {
        requested
    } else {
        root_path.join(requested)
    };
    if !absolute_project.is_dir() {
        return Ok(None);
    }
    let Ok(relative) = absolute_project.strip_prefix(&root_path) else {
        return Ok(None);
    };
    let project_path = normalize_workspace_path(&relative.to_string_lossy());
    if project_path.is_empty() {
        return Ok(None);
    }
    let project_id = materialized_project_id_for_path(workspace_id, &project_path);
    let root_id = store
        .accepted_root_id_for_path(workspace_id, &root)?
        .ok_or_else(|| AgentError::InvalidLease {
            reason: "workspace root metadata is missing".to_string(),
        })?;
    store.insert_project(&project_id, workspace_id, &root_id, &project_path, "")?;
    if let Some(head) = store.workspace_sync_head(workspace_id)? {
        store.set_project_latest_snapshot_id(
            workspace_id,
            &project_id,
            &bowline_core::ids::SnapshotId::new(head.workspace_ref.snapshot_id),
        )?;
    }
    store
        .current_project_by_path(&project_path)
        .map_err(Into::into)
}

pub(super) fn materialized_project_id_for_path(
    workspace_id: &bowline_core::ids::WorkspaceId,
    path: &str,
) -> ProjectId {
    let workspace_hash = blake3::hash(workspace_id.as_str().as_bytes()).to_hex()[..12].to_string();
    if path.is_empty() {
        return ProjectId::new(format!("proj_{workspace_hash}_root"));
    }
    let mut id = format!("proj_{workspace_hash}_");
    for character in path.chars() {
        match character {
            character if character.is_ascii_alphanumeric() => {
                id.push(character.to_ascii_lowercase());
            }
            '/' => id.push('_'),
            '-' => id.push_str("_dash_"),
            '_' => id.push_str("_us_"),
            '.' => id.push_str("_dot_"),
            character => id.push_str(&format!("_x{:x}_", character as u32)),
        }
    }
    while id.contains("__") {
        id = id.replace("__", "_");
    }
    ProjectId::new(id.trim_matches('_').to_string())
}

pub fn default_device_id() -> DeviceId {
    DeviceId::new(DEFAULT_DEVICE_ID)
}
