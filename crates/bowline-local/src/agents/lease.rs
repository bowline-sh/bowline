use super::*;

pub fn create_agent_lease(
    options: AgentLeaseCreateOptions,
) -> Result<AgentLeaseCreateCommandOutput, AgentError> {
    let db_path = resolve_db_path(options.db_path)?;
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
    if options.base == AgentLeaseBase::LatestMain {
        return Err(AgentError::InvalidLease {
            reason: "latest:main base is unavailable until read-only Git observer freshness is available"
                .to_string(),
        });
    }
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
    let lease_name = lease_work_view_name(&options.task, &options.generated_at);
    let lease_token = stable_token(&format!(
        "{}:{}:{}:{}",
        workspace.id.as_str(),
        project.id.as_str(),
        lease_name,
        options.generated_at
    ));
    let lease_id = LeaseId::new(format!("lease_{lease_token}"));
    let work_view_id = agent_work_view_id(workspace.id.as_str(), project.id.as_str(), &lease_name);
    let project_target_path = display_path_for_project(&root, &project.path);
    let work_view_path = display_path_for_agent_work_view(&root, &project.path, &lease_name);
    let write_target_mode = if options.work_view {
        AgentWriteTargetMode::WorkView
    } else {
        AgentWriteTargetMode::Direct
    };
    let write_target_path = if options.work_view {
        work_view_path.clone()
    } else {
        project_target_path.clone()
    };
    let event_id = EventId::new(format!(
        "evt_lease_created_{}",
        stable_token(lease_id.as_str())
    ));
    let mut lease = AgentLease {
        id: lease_id,
        workspace_id: workspace.id.clone(),
        project_id: project.id.clone(),
        device_id: options.device_id.clone(),
        write_target_mode,
        write_target_path: write_target_path.clone(),
        work_view_id: work_view_id.clone(),
        work_view_path: write_target_path.clone(),
        task: redacted_task_label(&options.task),
        base: options.base,
        base_snapshot_id,
        execution_state: AgentLeaseExecutionState::Blocked,
        output_state: AgentLeaseOutputState::Empty,
        scopes: default_scopes(&write_target_path),
        hydrate_budget_bytes: options.hydrate_budget_bytes,
        env_profile: default_env_profile(write_target_mode),
        env_restrictions: Vec::new(),
        output_target: AgentOutputTarget {
            kind: if options.work_view {
                AgentOutputTargetKind::WorkView
            } else {
                AgentOutputTargetKind::RealProject
            },
            work_view_id: options.work_view.then_some(work_view_id.clone()),
            path: write_target_path.clone(),
        },
        audit: AgentAuditPointer {
            local_event_id: event_id.clone(),
            local_receipt_id: None,
            encrypted_object_pointer: None,
        },
        cleanup_state: AgentLeaseCleanupState::Current,
        status_summary: "creating".to_string(),
        expires_at: "never".to_string(),
        created_at: options.generated_at.clone(),
        updated_at: options.generated_at.clone(),
    };
    recover_provisional_agent_lease_by_id(&store, &lease.id, &options.generated_at)?;
    if store.agent_lease_by_id(&lease.id)?.is_some() {
        return Err(AgentError::InvalidLease {
            reason: "agent lease already exists".to_string(),
        });
    }
    if options.work_view
        && store
            .work_view_by_id(&workspace.id, &lease.work_view_id)?
            .is_some()
    {
        return Err(AgentError::InvalidLease {
            reason: "agent lease work view already exists".to_string(),
        });
    }
    store.upsert_agent_lease(&lease)?;
    if !options.work_view {
        lease.execution_state = AgentLeaseExecutionState::Active;
        lease.status_summary = "active".to_string();
        if let Err(error) =
            persist_created_agent_lease(&store, &lease, event_id, &options.generated_at)
        {
            rollback_provisional_agent_lease(&store, &lease);
            return Err(error);
        }
        return Ok(AgentLeaseCreateCommandOutput {
            contract_version: CONTRACT_VERSION,
            command: CommandName::AgentStart,
            generated_at: options.generated_at,
            workspace_id: workspace.id,
            project_id: project.id,
            lease,
            status: WorkspaceStatus::healthy(),
            next_actions: vec![SafeAction {
                label: "Open the project".to_string(),
                command: Some(format!("cd {}", shell_word(&write_target_path))),
            }],
        });
    }
    drop(store);

    let work_output = create_work_view(WorkonOptions {
        db_path: Some(db_path.clone()),
        project_path: options.project_path.clone(),
        name: lease_name,
        owner_device_id: Some(options.device_id.clone()),
        generated_at: options.generated_at.clone(),
    });
    let store = MetadataStore::open(&db_path)?;
    let work_output = match work_output {
        Ok(output) => output,
        Err(error) => {
            rollback_provisional_agent_lease(&store, &lease);
            return Err(error.into());
        }
    };
    debug_assert_eq!(work_output.work_view.id, lease.work_view_id);
    debug_assert_eq!(work_output.work_view.visible_path, lease.work_view_path);
    lease.execution_state = AgentLeaseExecutionState::Active;
    lease.status_summary = "active".to_string();
    if let Err(error) = persist_created_agent_lease(&store, &lease, event_id, &options.generated_at)
    {
        rollback_created_agent_work_view(&store, &lease);
        return Err(error);
    }

    Ok(AgentLeaseCreateCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::AgentStart,
        generated_at: options.generated_at,
        workspace_id: workspace.id,
        project_id: project.id,
        lease,
        status: WorkspaceStatus::healthy(),
        next_actions: vec![SafeAction {
            label: "Open the agent work view".to_string(),
            command: Some("cd <lease work view>".to_string()),
        }],
    })
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
