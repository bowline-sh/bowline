use super::*;

pub const MAX_AGENT_LEASE_EXTENSION_HOURS: u16 = 168;

pub fn complete_agent_session(
    options: AgentLeaseSelectorOptions,
) -> Result<AgentCompleteCommandOutput, AgentError> {
    let db_path = resolve_db_path(options.db_path)?;
    let store = MetadataStore::open(&db_path)?;
    let mut lease = load_lease(&store, &options.lease_id, &options.generated_at)?;
    match lease.session_state {
        AgentSessionState::Completed => {}
        AgentSessionState::Cancelled => {
            return Err(AgentError::InvalidLease {
                reason: "cannot complete a cancelled session".to_string(),
            });
        }
        AgentSessionState::Open => {
            lease.session_state = AgentSessionState::Completed;
            lease.status_summary = "completed".to_string();
            lease.updated_at = options.generated_at.clone();
            let event_id = EventId::new(format!(
                "evt_lease_completed_{}",
                stable_token(&format!("{}:{}", lease.id.as_str(), options.generated_at))
            ));
            store.persist_agent_lease_with_event(
                &lease,
                lease_event(
                    &lease,
                    EventName::LeaseCompleted,
                    event_id,
                    &options.generated_at,
                    "Agent session completed.",
                ),
            )?;
        }
        AgentSessionState::Provisional => {
            return Err(AgentError::InvalidLease {
                reason: "cannot complete a session before its workspace is ready".to_string(),
            });
        }
    }
    store.revoke_agent_mcp_tokens_for_lease(&lease.id, &options.generated_at)?;

    let next_actions = match lease.write_target_mode {
        AgentWriteTargetMode::Direct => {
            let root = store.workspace_root(&lease.workspace_id)?.ok_or_else(|| {
                AgentError::InvalidLease {
                    reason: "agent lease workspace root is missing".to_string(),
                }
            })?;
            let project = store
                .project_by_id(&lease.workspace_id, &lease.project_id)?
                .ok_or_else(|| AgentError::InvalidLease {
                    reason: "agent lease project is missing".to_string(),
                })?;
            vec![RepairCommand::inspect(
                "Inspect synced project status".to_string(),
                Some(format!(
                    "bowline status --root {} --project {}",
                    shell_word(&root),
                    shell_word(&project.path)
                )),
            )]
        }
        AgentWriteTargetMode::WorkView => vec![RepairCommand::inspect(
            "Review the completed work view".to_string(),
            Some(format!(
                "bowline work review {}",
                shell_word(&lease.write_target_path)
            )),
        )],
    };
    let project = store
        .project_by_id(&lease.workspace_id, &lease.project_id)?
        .ok_or_else(|| AgentError::MissingProject {
            path: lease.write_target_path.clone(),
        })?;
    let workspace_root = store
        .workspace_root(&lease.workspace_id)?
        .ok_or(AgentError::MissingWorkspace)?;
    let project_path = display_path_for_project(&workspace_root, &project.path);
    drop(store);
    let status = crate::status::compose_status(crate::status::StatusOptions {
        db_path: Some(db_path),
        requested_path: Some(project_path),
        workspace_scope: false,
        generated_at: options.generated_at.clone(),
    })
    .map_err(|error| AgentError::InvalidLease {
        reason: format!("completed session status could not be composed: {error}"),
    })?
    .status;
    Ok(AgentCompleteCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::AgentComplete,
        generated_at: options.generated_at,
        workspace_id: lease.workspace_id.clone(),
        project_id: lease.project_id.clone(),
        lease,
        status,
        next_actions,
    })
}

pub fn cancel_agent_session(
    options: AgentLeaseSelectorOptions,
) -> Result<AgentLeaseUpdateCommandOutput, AgentError> {
    let db_path = resolve_db_path(options.db_path)?;
    let store = MetadataStore::open(&db_path)?;
    let mut lease = load_lease(&store, &options.lease_id, &options.generated_at)?;
    match lease.session_state {
        AgentSessionState::Cancelled => {}
        AgentSessionState::Completed => {
            return Err(AgentError::InvalidLease {
                reason: "cannot cancel a completed session".to_string(),
            });
        }
        AgentSessionState::Open | AgentSessionState::Provisional => {
            lease.session_state = AgentSessionState::Cancelled;
            lease.status_summary = "cancelled".to_string();
            lease.expires_at = options.generated_at.clone();
            lease.updated_at = options.generated_at.clone();
            let event_id = EventId::new(format!(
                "evt_lease_cancelled_{}",
                stable_token(&format!("{}:{}", lease.id.as_str(), options.generated_at))
            ));
            store.persist_agent_lease_with_event(
                &lease,
                lease_event(
                    &lease,
                    EventName::LeaseCancelled,
                    event_id,
                    &options.generated_at,
                    "Agent session cancelled and authority revoked.",
                ),
            )?;
        }
    }
    store.revoke_agent_mcp_tokens_for_lease(&lease.id, &options.generated_at)?;
    lifecycle_output(
        store,
        db_path,
        lease,
        CommandName::AgentCancel,
        options.generated_at,
    )
}

pub fn extend_agent_session(
    options: AgentLeaseExtendOptions,
) -> Result<AgentLeaseUpdateCommandOutput, AgentError> {
    if !(1..=MAX_AGENT_LEASE_EXTENSION_HOURS).contains(&options.hours) {
        return Err(AgentError::InvalidLease {
            reason: format!(
                "lease extension hours must be between 1 and {MAX_AGENT_LEASE_EXTENSION_HOURS}"
            ),
        });
    }
    let db_path = resolve_db_path(options.db_path)?;
    let store = MetadataStore::open(&db_path)?;
    let mut lease = load_lease(&store, &options.lease_id, &options.generated_at)?;
    if lease.session_state != AgentSessionState::Open {
        return Err(AgentError::InvalidLease {
            reason: "only an open session can be extended".to_string(),
        });
    }
    if expiry_elapsed(&lease.expires_at, &options.generated_at) {
        store.revoke_agent_mcp_tokens_for_lease(&lease.id, &options.generated_at)?;
        return Err(AgentError::InvalidLease {
            reason: "an expired session cannot be extended".to_string(),
        });
    }
    let generated_at = OffsetDateTime::parse(&options.generated_at, &Rfc3339).map_err(|_| {
        AgentError::InvalidLease {
            reason: "generated timestamp must be RFC3339".to_string(),
        }
    })?;
    let requested_expiry = generated_at + time::Duration::hours(i64::from(options.hours));
    let current_expiry = OffsetDateTime::parse(&lease.expires_at, &Rfc3339).map_err(|_| {
        AgentError::InvalidLease {
            reason: "lease expiry must be RFC3339".to_string(),
        }
    })?;
    if requested_expiry > current_expiry {
        lease.expires_at =
            requested_expiry
                .format(&Rfc3339)
                .map_err(|_| AgentError::InvalidLease {
                    reason: "extended lease expiry could not be formatted".to_string(),
                })?;
        lease.status_summary = "active".to_string();
        lease.updated_at = options.generated_at.clone();
        let event_id = EventId::new(format!(
            "evt_lease_extended_{}",
            stable_token(&format!(
                "{}:{}:{}",
                lease.id.as_str(),
                options.generated_at,
                options.hours
            ))
        ));
        store.persist_agent_lease_with_event(
            &lease,
            lease_event(
                &lease,
                EventName::LeaseExtended,
                event_id,
                &options.generated_at,
                "Agent session expiry extended.",
            ),
        )?;
    }
    lifecycle_output(
        store,
        db_path,
        lease,
        CommandName::AgentExtend,
        options.generated_at,
    )
}

fn lifecycle_output(
    store: MetadataStore,
    db_path: PathBuf,
    lease: AgentLease,
    command: CommandName,
    generated_at: String,
) -> Result<AgentLeaseUpdateCommandOutput, AgentError> {
    let project = store
        .project_by_id(&lease.workspace_id, &lease.project_id)?
        .ok_or_else(|| AgentError::MissingProject {
            path: lease.write_target_path.clone(),
        })?;
    let workspace_root = store
        .workspace_root(&lease.workspace_id)?
        .ok_or(AgentError::MissingWorkspace)?;
    let project_path = display_path_for_project(&workspace_root, &project.path);
    drop(store);
    let status = crate::status::compose_status(crate::status::StatusOptions {
        db_path: Some(db_path),
        requested_path: Some(project_path),
        workspace_scope: false,
        generated_at: generated_at.clone(),
    })
    .map_err(|error| AgentError::InvalidLease {
        reason: format!("agent session status could not be composed: {error}"),
    })?
    .status;
    Ok(AgentLeaseUpdateCommandOutput {
        contract_version: CONTRACT_VERSION,
        command,
        generated_at,
        workspace_id: lease.workspace_id.clone(),
        project_id: lease.project_id.clone(),
        lease,
        status,
        next_actions: Vec::new(),
    })
}
