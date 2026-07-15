use super::*;

const MAX_BLOCKED_PATH_ITEMS: usize = 20;

pub(super) struct StatusInputs {
    pub(super) projects: Vec<ProjectRecord>,
    pub(super) work_views: Vec<WorkViewRecord>,
    pub(super) agent_leases: Vec<AgentLeaseRecord>,
}

pub(super) fn compose_from_store(
    store: &MetadataStore,
    options: StatusOptions,
    state_root: PathBuf,
) -> Result<StatusCommandOutput, LocalStatusError> {
    let workspace = workspace_for_requested_path(store, options.requested_path.as_deref())?;
    let Some(workspace) = workspace else {
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
    let workspace_root = store.workspace_root(&workspace_id)?;
    let resolved_workspace_root = workspace_root
        .as_deref()
        .map(display_root_path)
        .or_else(|| Some("~/Code".to_string()));
    let watch_root = resolved_workspace_root
        .as_deref()
        .unwrap_or("~/Code")
        .to_string();
    let project_id = resolved.project_id.clone();
    let scope = if options.workspace_scope || project_id.is_none() {
        StatusScope::Workspace
    } else {
        StatusScope::Project
    };
    let query = resolved.event_query(50);
    let watermarks = store.scoped_event_watermarks(query)?;
    let recent_events = store.list_events_scoped(resolved.event_query(20))?;
    let status_events = store.list_status_signal_events_scoped(resolved.event_query(0))?;
    let unresolved_conflict_paths = unresolved_conflict_paths(&state_root)?
        .into_iter()
        .filter(|path| !status_path_is_source_control_metadata(path))
        .collect::<BTreeSet<_>>();
    recover_provisional_agent_leases(store, &workspace_id, &options.generated_at)
        .map_err(agent_recovery_status_error)?;
    let inputs = StatusInputs {
        projects: store.projects(&workspace_id)?,
        work_views: store.work_views(&workspace_id, true, None)?,
        agent_leases: store.agent_leases(&workspace_id)?,
    };
    let mut acc = StatusAccumulator::new(&options.generated_at);

    let requested_limited_path = options
        .requested_path
        .as_ref()
        .filter(|_| !options.workspace_scope)
        .map(String::as_str);
    apply_watermark_status(&watermarks, requested_limited_path, &mut acc);
    apply_status_signal_events(
        &status_events,
        &watermarks,
        &unresolved_conflict_paths,
        &mut acc,
    );
    let sync_counts = sync_operation_counts_for_local_device(store, &workspace_id, &recent_events)?;
    apply_sync_operation_status(&workspace_id, &sync_counts, &mut acc);
    super::materialization::apply_materialization_status(store, &workspace_id, &mut acc)?;
    apply_unresolved_conflict_status(
        &unresolved_conflict_paths,
        &workspace_id,
        workspace_root.as_deref().unwrap_or("~/Code"),
        &mut acc,
    )?;

    let total_projects = store.project_count(&workspace_id)?;
    let observed = store.observed_summary(&workspace_id)?;
    let projects_needing_attention = project_attention_summaries(
        store,
        &workspace_id,
        &inputs.projects,
        project_id.as_ref(),
        &watermarks,
        &unresolved_conflict_paths,
    )?;
    if !projects_needing_attention.is_empty() {
        acc.observe_fact(
            "snapshot.base_behind",
            "other-projects-attention",
            "other-projects-attention",
            StatusFactScope::Workspace,
            Some(workspace_id.as_str()),
        );
        acc.attention_items
            .push("Other projects need attention.".to_string());
    }
    if total_projects == 0 && acc.items.is_empty() {
        let mut item = base_status_item(
            StatusItemKind::Continuity,
            "Accepted workspace metadata is current; no projects have been observed yet.",
        );
        item.subject = Some(StatusSubject {
            kind: StatusSubjectKind::Workspace,
            id: workspace_id.as_str().to_string(),
            path: None,
        });
        acc.items.push(item);
    }
    if let Some(summary) = observed.as_ref() {
        apply_observed_summary(&workspace_id, summary, &mut acc);
    }
    apply_project_lifecycle_status(&inputs.projects, project_id.as_ref(), &mut acc);
    apply_blocked_and_local_only_paths(store, &workspace_id, &mut acc)?;
    apply_env_setup_metadata(store, &workspace_id, project_id.as_ref(), &mut acc)?;
    apply_work_view_metadata(&inputs.work_views, project_id.as_ref(), &mut acc);
    apply_agent_lease_metadata(
        &inputs.agent_leases,
        &inputs.work_views,
        project_id.as_ref(),
        &mut acc,
    );
    let sync_queue = sync_queue_status(&sync_counts);
    let stale_bases = snapshot_stale_bases_from_inputs(
        store,
        &workspace_id,
        &inputs.projects,
        &inputs.agent_leases,
        &inputs.work_views,
        project_id.as_ref(),
    )?;
    let freshness = freshness_for_stale_bases(&stale_bases);
    apply_stale_base_status(&stale_bases, &mut acc);
    let setup_report = setup::project_setup_readiness(
        store,
        &workspace_id,
        &inputs.projects,
        project_id.as_ref(),
        workspace_root.as_deref(),
    )?;
    setup::apply_project_setup_readiness(setup_report.as_ref(), project_id.as_ref(), &mut acc);
    let setup_readiness = setup_report.map(|report| report.readiness);
    let status_summary = reduce_status_facts(acc.facts, 1, options.generated_at.clone());
    let status_level = status_summary.presentation_level();
    let next_actions = if status_level == StatusLevel::Healthy {
        acc.next_actions
    } else {
        if acc.next_actions.is_empty() {
            acc.next_actions.push(recent_events_action(&watch_root));
        }
        acc.next_actions
    };

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
        setup_readiness,
        sync_queue,
        freshness,
        stale_bases,
        status: WorkspaceStatus {
            level: status_level,
            attention_items: acc.attention_items,
        },
        status_summary,
        items: acc.items,
        limits: acc.limits,
        event_watermarks: watermarks,
        next_actions,
        device_approvals: Vec::new(),
    })
}

pub(super) fn conflict_resolution_action(workspace_root: &str) -> RepairCommand {
    RepairCommand::mutating(
        "Resolve conflicts".to_string(),
        Some(format!("bowline resolve {}", shell_word(workspace_root))),
    )
}

pub(super) fn status_path_is_source_control_metadata(path: &str) -> bool {
    path.split('/')
        .any(|component| matches!(component, ".git" | ".jj" | ".hg" | ".svn"))
}

pub(super) fn recent_events_action(root: &str) -> RepairCommand {
    RepairCommand::inspect(
        "Inspect recent events".to_string(),
        Some(format!(
            "bowline status --root {} --watch",
            shell_word(root)
        )),
    )
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
        resolved_workspace_root: options
            .requested_path
            .as_deref()
            .map(display_root_path)
            .or_else(|| Some("~/Code".to_string())),
        workspace_summary: Some(WorkspaceSummary::empty()),
        setup_readiness: None,
        sync_queue: None,
        freshness: bowline_core::status::FreshnessVerdict::Unknown,
        stale_bases: Vec::new(),
        status: WorkspaceStatus {
            level: StatusLevel::Attention,
            attention_items: vec!["bowline has not initialized local metadata yet.".to_string()],
        },
        status_summary: reduce_status_facts(
            [StatusFact::new(
                "metadata-not-initialized",
                "metadata.not_initialized",
                "local-metadata",
                StatusFactScope::Workspace,
                options.generated_at.clone(),
                "metadata",
            )],
            1,
            options.generated_at.clone(),
        ),
        items: vec![metadata_item(
            "Local metadata is missing; status is observational and did not create files.",
            None,
        )],
        limits: Vec::new(),
        event_watermarks: empty_watermarks(),
        next_actions: vec![RepairCommand::inspect(
            "Initialize ~/Code when ready".to_string(),
            None,
        )],
        device_approvals: Vec::new(),
    }
}

pub(super) fn apply_observed_summary(
    workspace_id: &WorkspaceId,
    summary: &ObservedWorkspaceSummary,
    acc: &mut StatusAccumulator,
) {
    let mut item = base_status_item(
        StatusItemKind::Continuity,
        &format!(
            "Tracking {}, {}, {}.",
            plural_phrase(summary.repo_count, "repo", "repos"),
            plural_phrase(summary.workspace_sync_path_count, "file", "files"),
            plural_phrase(summary.env_file_count, "env file", "env files"),
        ),
    );
    item.subject = Some(observed_subject(workspace_id));
    acc.items.push(item);

    if summary.no_remote_repo_count > 0 {
        let mut item = base_status_item(
            StatusItemKind::Source,
            &format!(
                "{} without a remote; still kept as syncable workspace state.",
                plural_phrase(summary.no_remote_repo_count, "repo", "repos"),
            ),
        );
        item.subject = Some(observed_subject(workspace_id));
        acc.items.push(item);
    }

    if summary.stale_remote_tracking_repo_count > 0 {
        let mut item = base_status_item(
            StatusItemKind::Source,
            &format!(
                "{} with local branches ahead of their tracking refs; advisory only.",
                plural_phrase(summary.stale_remote_tracking_repo_count, "repo", "repos"),
            ),
        );
        item.subject = Some(observed_subject(workspace_id));
        acc.items.push(item);
    }

    if summary.untracked_file_count > 0 {
        let mut item = base_status_item(
            StatusItemKind::Source,
            &format!(
                "{} not tracked by Git; kept as workspace state.",
                plural_phrase(summary.untracked_file_count, "file", "files"),
            ),
        );
        item.subject = Some(observed_subject(workspace_id));
        acc.items.push(item);
    }

    if summary.local_only_path_count > 0 {
        let mut item = base_status_item(
            StatusItemKind::Materialization,
            &format!(
                "{} kept local-only; excluded from workspace sync.",
                plural_phrase(summary.local_only_path_count, "path", "paths"),
            ),
        );
        item.subject = Some(observed_subject(workspace_id));
        acc.items.push(item);
    }

    if summary.blocked_path_count > 0 {
        let mut item = base_status_item(
            StatusItemKind::Policy,
            &format!(
                "{} blocked by policy; excluded from sync.",
                plural_phrase(summary.blocked_path_count, "path", "paths"),
            ),
        );
        item.subject = Some(observed_subject(workspace_id));
        acc.items.push(item);
    }

    if summary.git_partial_project_count > 0 {
        acc.observe_fact(
            "git.observation_partial",
            "git-observation-partial",
            "git-observation-partial",
            StatusFactScope::Workspace,
            Some(workspace_id.as_str()),
        );
        let mut item = base_status_item(
            StatusItemKind::Source,
            &format!(
                "{} with partially read Git state; untracked counts may be unavailable.",
                plural_phrase(summary.git_partial_project_count, "project", "projects"),
            ),
        );
        item.subject = Some(observed_subject(workspace_id));
        acc.items.push(item);
    }

    if summary.git_unavailable_project_count > 0 {
        let mut item = base_status_item(
            StatusItemKind::Source,
            &format!(
                "{} with Git state unavailable; source status is degraded.",
                plural_phrase(summary.git_unavailable_project_count, "project", "projects"),
            ),
        );
        item.subject = Some(observed_subject(workspace_id));
        acc.items.push(item);
        acc.observe_fact(
            "git.observation_unavailable",
            "git-observation-unavailable",
            "git-observation-unavailable",
            StatusFactScope::Workspace,
            Some(workspace_id.as_str()),
        );
        acc.attention_items
            .push("Git state could not be fully read.".to_string());
    }
}

pub(super) fn apply_project_lifecycle_status(
    projects: &[ProjectRecord],
    requested_project_id: Option<&ProjectId>,
    acc: &mut StatusAccumulator,
) {
    for project in projects {
        if requested_project_id.is_some_and(|requested| requested != &project.id) {
            continue;
        }
        let summary = match (
            project.lifecycle_state,
            project.local_materialization_state,
            project.purge_after.as_deref(),
        ) {
            (ProjectLifecycleState::Active, ProjectLocalMaterializationState::Forgotten, _) => {
                Some(format!("{} local: forgotten.", project.path))
            }
            (
                ProjectLifecycleState::Archived,
                ProjectLocalMaterializationState::Materialized,
                _,
            ) => Some(format!("{} archived (local copy retained).", project.path)),
            (ProjectLifecycleState::Archived, ProjectLocalMaterializationState::Forgotten, _) => {
                Some(format!("{} archived (local copy forgotten).", project.path))
            }
            (ProjectLifecycleState::PurgePending, _, Some(purge_after)) => Some(format!(
                "{} purge-pending until {purge_after}.",
                project.path
            )),
            (ProjectLifecycleState::PurgePending, _, None) => {
                Some(format!("{} purge-pending.", project.path))
            }
            (ProjectLifecycleState::Purged, ProjectLocalMaterializationState::Materialized, _) => {
                Some(format!(
                    "{} purged remotely (local copy retained).",
                    project.path
                ))
            }
            (ProjectLifecycleState::Purged, ProjectLocalMaterializationState::Forgotten, _) => {
                Some(format!("{} purged remotely.", project.path))
            }
            (ProjectLifecycleState::Active, ProjectLocalMaterializationState::Materialized, _) => {
                None
            }
        };
        let Some(summary) = summary else {
            continue;
        };
        let mut item = base_status_item(StatusItemKind::Source, &summary);
        item.project_id = Some(project.id.clone());
        item.subject = Some(StatusSubject {
            kind: StatusSubjectKind::Project,
            id: project.id.as_str().to_string(),
            path: Some(project.path.clone()),
        });
        item.event_name = Some(EventName::NamespaceDeletedOrArchived);
        acc.items.push(item);
    }
}

fn apply_blocked_and_local_only_paths(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    acc: &mut StatusAccumulator,
) -> Result<(), LocalStatusError> {
    let paths = store.blocked_and_local_only_observed_paths(workspace_id, None)?;
    let mut emitted = 0_usize;
    for path in &paths {
        if path.classification == PathClassification::Blocked {
            acc.observe_fact(
                "policy.path_blocked",
                format!("blocked-path:{}", path.path),
                format!("blocked-path:{}", path.path),
                StatusFactScope::Path,
                Some(&path.path),
            );
            if !acc
                .attention_items
                .iter()
                .any(|item| item == "One or more paths are blocked by policy.")
            {
                acc.attention_items
                    .push("One or more paths are blocked by policy.".to_string());
            }
        } else {
            acc.observe_fact(
                "policy.local_only",
                format!("local-only-path:{}", path.path),
                format!("local-only-path:{}", path.path),
                StatusFactScope::Path,
                Some(&path.path),
            );
        }
        if emitted >= MAX_BLOCKED_PATH_ITEMS {
            continue;
        }
        acc.items.push(observed_path_status_item(path));
        emitted += 1;
    }

    if paths.len() > MAX_BLOCKED_PATH_ITEMS {
        let additional = paths.len() - MAX_BLOCKED_PATH_ITEMS;
        let mut item = base_status_item(
            StatusItemKind::Policy,
            &format!(
                "{} additional blocked/local-only paths; see observed metadata for details.",
                plural_phrase(additional as u64, "path", "paths"),
            ),
        );
        item.subject = Some(observed_subject(workspace_id));
        acc.items.push(item);
    }

    Ok(())
}

fn observed_path_status_item(path: &ObservedLocalPath) -> StatusItem {
    let blocked = path.classification == PathClassification::Blocked
        || path.mode == MaterializationMode::Blocked;
    let mut item = base_status_item(
        if blocked {
            StatusItemKind::Policy
        } else {
            StatusItemKind::Materialization
        },
        if blocked {
            "Blocked by policy; excluded from sync."
        } else {
            "Kept local-only; excluded from workspace sync."
        },
    );
    item.subject = Some(StatusSubject {
        kind: StatusSubjectKind::Path,
        id: path.path.clone(),
        path: Some(path.path.clone()),
    });
    item.path = Some(path.path.clone());
    item.classification = Some(path.classification);
    item.mode = Some(path.mode);
    item.access = path.access.clone();
    item.project_id = path.project_id.clone();
    item
}

pub(super) fn plural_phrase(count: u64, singular: &str, plural: &str) -> String {
    format!("{count} {}", if count == 1 { singular } else { plural })
}

fn observed_subject(workspace_id: &WorkspaceId) -> StatusSubject {
    StatusSubject {
        kind: StatusSubjectKind::Workspace,
        id: workspace_id.as_str().to_string(),
        path: None,
    }
}

pub(super) fn apply_env_setup_metadata(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    project_id: Option<&ProjectId>,
    acc: &mut StatusAccumulator,
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
                "{} across {} tracked; values are redacted.",
                plural_phrase(
                    visible_env_records.len() as u64,
                    "project env record",
                    "project env records"
                ),
                plural_phrase(source_count as u64, "file", "files"),
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
        acc.items.push(item);

        if stale_count > 0 {
            acc.observe_fact(
                "env.materialization_stale",
                "env-materialization-stale",
                "env-materialization-stale",
                StatusFactScope::Project,
                project_id.map(ProjectId::as_str),
            );
            let subject = if stale_count == 1 {
                "record is"
            } else {
                "records are"
            };
            acc.attention_items.push(format!(
                "{stale_count} materialized env {subject} stale; values remain redacted."
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
            let kind = if receipt.state == "blocked" {
                "setup.blocked"
            } else {
                "setup.failed"
            };
            acc.observe_fact(
                kind,
                format!("setup-receipt:{}", receipt.id),
                format!("setup-receipt:{}", receipt.id),
                StatusFactScope::Project,
                receipt.project_id.as_ref().map(ProjectId::as_str),
            );
            acc.attention_items.push(format!(
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
        acc.items.push(item);
    }

    Ok(())
}

pub(super) fn setup_receipt_needs_current_attention(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    receipt: &crate::metadata::SetupReceiptRecord,
) -> Result<bool, LocalStatusError> {
    let needs_attention = matches!(
        SetupReceiptState::from_wire(&receipt.state),
        Some(SetupReceiptState::Failed | SetupReceiptState::ApprovalRequired)
    ) || receipt.state == "blocked";
    if !needs_attention {
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
    projects: &[ProjectRecord],
    current_project_id: Option<&ProjectId>,
    watermarks: &EventWatermarks,
    unresolved_conflict_paths: &BTreeSet<String>,
) -> Result<Vec<ProjectAttentionSummary>, LocalStatusError> {
    let mut summaries = Vec::new();

    let events_by_project = store.status_signal_events_by_project(workspace_id, projects)?;
    for project in projects {
        if current_project_id == Some(&project.id) {
            continue;
        }

        let events = events_by_project
            .get(&project.id)
            .cloned()
            .unwrap_or_default();
        let mut project_acc = StatusAccumulator::new(
            watermarks
                .last_scan_at
                .as_deref()
                .unwrap_or("1970-01-01T00:00:00Z"),
        );
        apply_status_signal_events(
            &events,
            watermarks,
            unresolved_conflict_paths,
            &mut project_acc,
        );
        let summary = reduce_status_facts(
            project_acc.facts.clone(),
            1,
            watermarks
                .last_scan_at
                .clone()
                .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string()),
        );
        let level = summary.presentation_level();
        if level != StatusLevel::Healthy
            && project_acc
                .items
                .iter()
                .all(|item| item.kind == StatusItemKind::Conflict)
            && !unresolved_conflict_paths.iter().any(|path| {
                path == &project.path || path.starts_with(&format!("{}/", project.path))
            })
        {
            continue;
        }

        if level != StatusLevel::Healthy {
            let summary = project_acc
                .attention_items
                .first()
                .cloned()
                .or_else(|| project_acc.items.first().map(|item| item.summary.clone()))
                .unwrap_or_else(|| "Project needs attention.".to_string());
            summaries.push(ProjectAttentionSummary {
                project_id: project.id.clone(),
                path: project.path.clone(),
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
    let status_summary = reduce_status_facts(
        [StatusFact::new(
            "metadata-unavailable",
            match state {
                DatabaseState::Corrupt => "metadata.corrupt",
                _ => "metadata.unavailable",
            },
            "local-metadata",
            StatusFactScope::Workspace,
            options.generated_at.clone(),
            "metadata",
        )],
        1,
        options.generated_at.clone(),
    );

    StatusCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::Status,
        generated_at: options.generated_at.clone(),
        workspace_id: WorkspaceId::new("ws_local_limited"),
        project_id: None,
        scope: Some(StatusScope::Workspace),
        requested_path: options.requested_path.clone(),
        resolved_workspace_root: options
            .requested_path
            .as_deref()
            .map(display_root_path)
            .or_else(|| Some("~/Code".to_string())),
        workspace_summary: Some(WorkspaceSummary::empty()),
        setup_readiness: None,
        sync_queue: None,
        freshness: bowline_core::status::FreshnessVerdict::Unknown,
        stale_bases: Vec::new(),
        status: WorkspaceStatus {
            level: status_summary.presentation_level(),
            attention_items: vec![format!("Local metadata is limited: {reason}.")],
        },
        status_summary,
        items: vec![metadata_item(
            "Local metadata could not be opened; source files were not modified.",
            Some(EventName::MetadataCorrupt),
        )],
        limits: vec![LimitedCapability {
            capability: "local metadata".to_string(),
            support_capability: None,
            unavailable_because: reason,
            still_works: vec![
                "source files stay readable".to_string(),
                "status can report recovery guidance".to_string(),
            ],
            path: None,
        }],
        event_watermarks: empty_watermarks(),
        next_actions: vec![RepairCommand::inspect(
            "Check local metadata".to_string(),
            None,
        )],
        device_approvals: Vec::new(),
    }
}
