use super::*;

pub(super) fn compose_from_store(
    store: &MetadataStore,
    options: StatusOptions,
    state_root: PathBuf,
) -> Result<StatusCommandOutput, LocalStatusError> {
    let Some(workspace) = store.current_workspace()? else {
        return Ok(missing_metadata_status(&options));
    };
    if store.accepted_root_count(&workspace.id)? == 0 {
        return Ok(missing_metadata_status(&options));
    }

    let resolved = resolve_scope(
        store,
        options.requested_path.as_deref(),
        options.workspace_scope,
    )?;
    let workspace_id = resolved
        .workspace_id
        .clone()
        .unwrap_or_else(|| WorkspaceId::new("ws_local_uninitialized"));
    let project_id = resolved.project_id.clone();
    let scope = if options.workspace_scope {
        StatusScope::Workspace
    } else if project_id.is_some() {
        StatusScope::Project
    } else {
        StatusScope::Workspace
    };
    let query = resolved.event_query(50);
    let watermarks = store.scoped_event_watermarks(query)?;
    let recent_events = store.list_events_scoped(resolved.event_query(20))?;
    let status_events = store.list_status_signal_events_scoped(resolved.event_query(0))?;
    let unresolved_conflict_paths = unresolved_conflict_paths(&state_root)?
        .into_iter()
        .filter(|path| !status_path_is_source_control_metadata(path))
        .collect::<BTreeSet<_>>();
    let mut items = Vec::new();
    let mut limits = Vec::new();
    let mut attention_items = Vec::new();
    let mut next_actions = Vec::new();
    let mut level = StatusLevel::Healthy;

    apply_watermark_status(
        &watermarks,
        &mut items,
        &mut limits,
        &mut attention_items,
        &mut level,
    );
    apply_status_signal_events(
        &status_events,
        &watermarks,
        &unresolved_conflict_paths,
        &mut items,
        &mut attention_items,
        &mut level,
    );
    let sync_counts = sync_operation_counts_for_local_device(store, &workspace_id, &recent_events)?;
    apply_sync_operation_status(
        &workspace_id,
        &sync_counts,
        &mut items,
        &mut limits,
        &mut attention_items,
        &mut level,
    );
    apply_unresolved_conflict_status(
        &unresolved_conflict_paths,
        &workspace_id,
        &mut items,
        &mut limits,
        &mut attention_items,
        &mut next_actions,
        &mut level,
    )?;

    let total_projects = store.project_count(&workspace_id)?;
    let observed = store.observed_summary(&workspace_id)?;
    let projects_needing_attention = project_attention_summaries(
        store,
        &workspace_id,
        project_id.as_ref(),
        &watermarks,
        &unresolved_conflict_paths,
    )?;
    if !projects_needing_attention.is_empty() && level == StatusLevel::Healthy {
        level = StatusLevel::Attention;
        attention_items.push("Other projects need attention.".to_string());
    }
    let resolved_workspace_root = store
        .current_workspace_root()?
        .map(|path| display_root_path(&path))
        .or_else(|| Some("~/Code".to_string()));
    if total_projects == 0 && items.is_empty() {
        let mut item = base_status_item(
            StatusItemKind::Continuity,
            "Accepted workspace metadata is current; no projects have been observed yet.",
        );
        item.subject = Some(StatusSubject {
            kind: StatusSubjectKind::Workspace,
            id: workspace_id.as_str().to_string(),
            path: None,
        });
        items.push(item);
    }
    if let Some(summary) = observed.as_ref() {
        apply_observed_summary(
            &workspace_id,
            summary,
            watermarks.sync_state == Some(ComponentState::Ready),
            &mut items,
            &mut attention_items,
            &mut level,
        );
    }
    apply_env_setup_metadata(
        store,
        &workspace_id,
        project_id.as_ref(),
        &mut items,
        &mut attention_items,
        &mut level,
    )?;
    apply_work_view_metadata(
        store,
        &workspace_id,
        project_id.as_ref(),
        &mut items,
        &mut attention_items,
        &mut level,
    )?;
    apply_agent_lease_metadata(
        store,
        &workspace_id,
        project_id.as_ref(),
        &options.generated_at,
        &mut items,
        &mut attention_items,
        &mut level,
    )?;
    let index = durable_index_status(store, &workspace_id, project_id.as_ref())?;
    apply_index_status(
        index.as_ref(),
        &mut items,
        &mut limits,
        &mut attention_items,
        &mut level,
    );
    let hydration_budget =
        durable_hydration_budget_status(store, &workspace_id, project_id.as_ref())?;
    let hydration_progress = hydration_progress_from_events(&recent_events);
    let sync_queue = sync_queue_status(&sync_counts);

    Ok(StatusCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::Status,
        generated_at: options.generated_at,
        workspace_id,
        project_id,
        scope: Some(scope),
        requested_path: options.requested_path,
        resolved_workspace_root,
        workspace_summary: Some(WorkspaceSummary {
            projects_needing_attention,
            total_projects: Some(total_projects),
            observed,
        }),
        index,
        hydration_budget,
        hydration_progress,
        sync_queue,
        status: WorkspaceStatus {
            level,
            attention_items,
        },
        items,
        limits,
        event_watermarks: watermarks,
        next_actions: if level == StatusLevel::Healthy {
            next_actions
        } else {
            if next_actions.is_empty() {
                next_actions.push(recent_events_action());
            }
            next_actions
        },
    })
}

pub(super) fn conflict_resolution_action() -> SafeAction {
    SafeAction {
        label: "Resolve conflicts".to_string(),
        command: Some("bowline resolve ~/Code".to_string()),
    }
}

pub(super) fn status_path_is_source_control_metadata(path: &str) -> bool {
    path.split('/')
        .any(|component| matches!(component, ".git" | ".jj" | ".hg" | ".svn"))
}

pub(super) fn recent_events_action() -> SafeAction {
    SafeAction {
        label: "Inspect recent events".to_string(),
        command: Some("bowline status --watch".to_string()),
    }
}

pub(super) fn missing_metadata_status(options: &StatusOptions) -> StatusCommandOutput {
    StatusCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::Status,
        generated_at: options.generated_at.clone(),
        workspace_id: WorkspaceId::new("ws_local_uninitialized"),
        project_id: None,
        scope: Some(StatusScope::Workspace),
        requested_path: options.requested_path.clone(),
        resolved_workspace_root: Some("~/Code".to_string()),
        workspace_summary: Some(WorkspaceSummary::empty()),
        index: None,
        hydration_budget: None,
        hydration_progress: Vec::new(),
        sync_queue: None,
        status: WorkspaceStatus {
            level: StatusLevel::Attention,
            attention_items: vec!["bowline has not initialized local metadata yet.".to_string()],
        },
        items: vec![metadata_item(
            "Local metadata is missing; status is observational and did not create files.",
            None,
        )],
        limits: Vec::new(),
        event_watermarks: empty_watermarks(),
        next_actions: vec![SafeAction {
            label: "Initialize ~/Code when ready".to_string(),
            command: None,
        }],
    }
}

pub(super) fn apply_observed_summary(
    workspace_id: &WorkspaceId,
    summary: &ObservedWorkspaceSummary,
    sync_ready: bool,
    items: &mut Vec<StatusItem>,
    attention_items: &mut Vec<String>,
    level: &mut StatusLevel,
) {
    if !sync_ready && *level == StatusLevel::Healthy {
        *level = StatusLevel::Attention;
        attention_items
            .push("Workspace has been observed locally; sync has not started yet.".to_string());
    }

    let mut item = base_status_item(
        StatusItemKind::Continuity,
        if sync_ready {
            format!(
                "Observed {} repos, {} workspace-sync paths, {} env files, {} generated/dependency paths; sync is active.",
                summary.repo_count,
                summary.workspace_sync_path_count,
                summary.env_file_count,
                summary.generated_path_count + summary.dependency_path_count,
            )
        } else {
            format!(
                "Observed {} repos, {} workspace-sync paths, {} env files, {} generated/dependency paths; no bytes have been uploaded.",
            summary.repo_count,
            summary.workspace_sync_path_count,
            summary.env_file_count,
            summary.generated_path_count + summary.dependency_path_count,
            )
        }
        .as_str(),
    );
    item.subject = Some(StatusSubject {
        kind: StatusSubjectKind::Workspace,
        id: workspace_id.as_str().to_string(),
        path: None,
    });
    items.push(item);

    if summary.repo_count > 0 {
        let mut item = base_status_item(
            StatusItemKind::Source,
            &format!(
                "Git observer is advisory for {} repo(s); bowline reads local metadata only and never fetches, commits, or uses Git as sync.",
                summary.repo_count
            ),
        );
        item.subject = Some(StatusSubject {
            kind: StatusSubjectKind::Workspace,
            id: workspace_id.as_str().to_string(),
            path: None,
        });
        items.push(item);
    }

    if summary.no_remote_repo_count > 0 {
        let mut item = base_status_item(
            StatusItemKind::Source,
            &format!(
                "{} repo(s) have no remote; bowline will still treat their files as syncable workspace state.",
                summary.no_remote_repo_count
            ),
        );
        item.subject = Some(StatusSubject {
            kind: StatusSubjectKind::Workspace,
            id: workspace_id.as_str().to_string(),
            path: None,
        });
        items.push(item);
    }

    if summary.stale_remote_tracking_repo_count > 0 {
        let mut item = base_status_item(
            StatusItemKind::Source,
            &format!(
                "{} repo(s) have local branch refs that differ from local remote-tracking refs; this is advisory only and bowline will not fetch or repair Git.",
                summary.stale_remote_tracking_repo_count
            ),
        );
        item.subject = Some(StatusSubject {
            kind: StatusSubjectKind::Workspace,
            id: workspace_id.as_str().to_string(),
            path: None,
        });
        items.push(item);
    }

    if summary.untracked_file_count > 0 {
        let mut item = base_status_item(
            StatusItemKind::Source,
            &format!(
                "{} untracked file(s) were observed as workspace continuity state.",
                summary.untracked_file_count
            ),
        );
        item.subject = Some(StatusSubject {
            kind: StatusSubjectKind::Workspace,
            id: workspace_id.as_str().to_string(),
            path: None,
        });
        items.push(item);
    }
}

pub(super) fn apply_env_setup_metadata(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    project_id: Option<&ProjectId>,
    items: &mut Vec<StatusItem>,
    attention_items: &mut Vec<String>,
    level: &mut StatusLevel,
) -> Result<(), LocalStatusError> {
    let env_records = store.env_records(workspace_id)?;
    let visible_env_records = env_records
        .iter()
        .filter(|record| project_id.is_none() || record.project_id.as_ref() == project_id)
        .collect::<Vec<_>>();
    if !visible_env_records.is_empty() {
        let source_count = visible_env_records
            .iter()
            .map(|record| record.source_path.as_str())
            .collect::<HashSet<_>>()
            .len();
        let stale_count = visible_env_records
            .iter()
            .filter(|record| record.materialization_state == "stale")
            .count();
        let mut item = base_status_item(
            StatusItemKind::Env,
            &format!(
                "{} project env record(s) across {} file(s) are tracked; values are redacted.",
                visible_env_records.len(),
                source_count
            ),
        );
        item.subject = Some(StatusSubject {
            kind: StatusSubjectKind::EnvRecord,
            id: visible_env_records
                .first()
                .map(|record| record.id.as_str().to_string())
                .unwrap_or_else(|| "env-records".to_string()),
            path: visible_env_records
                .first()
                .map(|record| record.source_path.clone()),
        });
        item.path = visible_env_records
            .first()
            .map(|record| record.source_path.clone());
        item.classification = Some(PathClassification::ProjectEnv);
        item.mode = Some(MaterializationMode::ProjectEnv);
        item.access = visible_env_records
            .first()
            .map(|record| record.access.clone())
            .unwrap_or_default();
        item.project_id = visible_env_records
            .first()
            .and_then(|record| record.project_id.clone());
        item.env_record_id = visible_env_records.first().map(|record| record.id.clone());
        items.push(item);

        if stale_count > 0 {
            if *level == StatusLevel::Healthy {
                *level = StatusLevel::Attention;
            }
            attention_items.push(format!(
                "{stale_count} materialized env record(s) are stale; values remain redacted."
            ));
        }
    }

    let setup_receipts = store.setup_receipts(workspace_id)?;
    let visible_receipts = setup_receipts
        .iter()
        .filter(|record| project_id.is_none() || record.project_id.as_ref() == project_id)
        .collect::<Vec<_>>();
    for receipt in &visible_receipts {
        if setup_receipt_needs_current_attention(store, workspace_id, receipt)? {
            if *level == StatusLevel::Healthy {
                *level = StatusLevel::Attention;
            }
            attention_items.push(format!(
                "Setup for {} needs attention: {}.",
                receipt.cwd, receipt.state
            ));
        }
    }
    for receipt in visible_receipts.iter().take(3) {
        let mut item = base_status_item(
            StatusItemKind::Setup,
            &format!(
                "Setup {} via {}; {}",
                receipt.state,
                receipt.trigger,
                if receipt.redacted_summary.is_empty() {
                    "output is redacted.".to_string()
                } else {
                    receipt.redacted_summary.clone()
                }
            ),
        );
        item.subject = Some(StatusSubject {
            kind: StatusSubjectKind::SetupReceipt,
            id: receipt.id.clone(),
            path: Some(receipt.cwd.clone()),
        });
        item.path = Some(receipt.cwd.clone());
        item.project_id = receipt.project_id.clone();
        items.push(item);
    }

    Ok(())
}

pub(super) fn setup_receipt_needs_current_attention(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    receipt: &crate::metadata::SetupReceiptRecord,
) -> Result<bool, LocalStatusError> {
    if !matches!(
        receipt.state.as_str(),
        "blocked" | "failed" | "approval-required"
    ) {
        return Ok(false);
    }
    let Some(project_id) = receipt.project_id.as_ref() else {
        return Ok(true);
    };
    Ok(store
        .project_hot_state(workspace_id, project_id)?
        .is_none_or(|state| state == "setup.blocked"))
}

pub(super) fn project_attention_summaries(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    current_project_id: Option<&ProjectId>,
    watermarks: &EventWatermarks,
    unresolved_conflict_paths: &BTreeSet<String>,
) -> Result<Vec<ProjectAttentionSummary>, LocalStatusError> {
    let mut summaries = Vec::new();

    for project in store.projects(workspace_id)? {
        if current_project_id == Some(&project.id) {
            continue;
        }

        let events = store.list_status_signal_events_scoped(EventQuery {
            workspace_id: Some(workspace_id.clone()),
            project_id: Some(project.id.clone()),
            path_prefix: Some(project.path.clone()),
            limit: 0,
        })?;
        let mut items = Vec::new();
        let mut attention_items = Vec::new();
        let mut level = StatusLevel::Healthy;
        apply_status_signal_events(
            &events,
            watermarks,
            unresolved_conflict_paths,
            &mut items,
            &mut attention_items,
            &mut level,
        );
        if level != StatusLevel::Healthy
            && items
                .iter()
                .all(|item| item.kind == StatusItemKind::Conflict)
            && !unresolved_conflict_paths.iter().any(|path| {
                path == &project.path || path.starts_with(&format!("{}/", project.path))
            })
        {
            continue;
        }

        if level != StatusLevel::Healthy {
            let summary = attention_items
                .first()
                .cloned()
                .or_else(|| items.first().map(|item| item.summary.clone()))
                .unwrap_or_else(|| "Project needs attention.".to_string());
            summaries.push(ProjectAttentionSummary {
                project_id: project.id,
                path: project.path,
                level,
                summary,
            });
        }
    }

    Ok(summaries)
}

pub(super) fn display_root_path(path: &str) -> String {
    if path == "~" || path.starts_with("~/") {
        return path.to_string();
    }

    let path_buf = PathBuf::from(path);
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return path.to_string();
    };
    let Ok(relative) = path_buf.strip_prefix(home) else {
        return path.to_string();
    };

    if relative.as_os_str().is_empty() {
        "~".to_string()
    } else {
        format!("~/{}", relative.display())
    }
}

pub(super) fn limited_metadata_status(
    options: &StatusOptions,
    state: &DatabaseState,
) -> StatusCommandOutput {
    let reason = match state {
        DatabaseState::FutureIncompatible { found, supported } => {
            format!("metadata schema version {found} is newer than supported version {supported}")
        }
        DatabaseState::Corrupt => "metadata database is corrupt".to_string(),
        DatabaseState::UnsupportedSchema => {
            "metadata database uses an unsupported schema".to_string()
        }
        DatabaseState::Locked => "metadata database is locked".to_string(),
        DatabaseState::PermissionDenied => "metadata database cannot be opened".to_string(),
        DatabaseState::Missing | DatabaseState::Empty | DatabaseState::Current => {
            "metadata database is unavailable".to_string()
        }
    };

    StatusCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::Status,
        generated_at: options.generated_at.clone(),
        workspace_id: WorkspaceId::new("ws_local_limited"),
        project_id: None,
        scope: Some(StatusScope::Workspace),
        requested_path: options.requested_path.clone(),
        resolved_workspace_root: Some("~/Code".to_string()),
        workspace_summary: Some(WorkspaceSummary::empty()),
        index: None,
        hydration_budget: None,
        hydration_progress: Vec::new(),
        sync_queue: None,
        status: WorkspaceStatus {
            level: StatusLevel::Limited,
            attention_items: vec![format!("Local metadata is limited: {reason}.")],
        },
        items: vec![metadata_item(
            "Local metadata could not be opened; source files were not modified.",
            Some(EventName::MetadataCorrupt),
        )],
        limits: vec![LimitedCapability {
            capability: "local metadata".to_string(),
            unavailable_because: reason,
            still_works: vec![
                "source files stay readable".to_string(),
                "status can report recovery guidance".to_string(),
            ],
            path: None,
        }],
        event_watermarks: empty_watermarks(),
        next_actions: vec![SafeAction {
            label: "Check local metadata".to_string(),
            command: None,
        }],
    }
}
