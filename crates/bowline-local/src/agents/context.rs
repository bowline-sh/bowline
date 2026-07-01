use super::*;

pub fn agent_context(
    options: AgentLeaseSelectorOptions,
) -> Result<AgentContextCommandOutput, AgentError> {
    let store = MetadataStore::open(resolve_db_path(options.db_path)?)?;
    let lease = load_lease(&store, &options.lease_id, &options.generated_at)?;
    reconcile_materialized_hydration_queue(&store, &lease.workspace_id, &options.generated_at)?;
    let context = context_for_lease(&store, &lease);
    Ok(AgentContextCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::AgentContext,
        generated_at: options.generated_at,
        workspace_id: lease.workspace_id.clone(),
        project_id: lease.project_id.clone(),
        context,
    })
}

pub fn agent_prompt(
    options: AgentLeaseSelectorOptions,
) -> Result<AgentPromptCommandOutput, AgentError> {
    let store = MetadataStore::open(resolve_db_path(options.db_path)?)?;
    let lease = load_lease(&store, &options.lease_id, &options.generated_at)?;
    reconcile_materialized_hydration_queue(&store, &lease.workspace_id, &options.generated_at)?;
    let context = context_for_lease(&store, &lease);
    let allowed_tools = capabilities_for_lease(&lease)
        .into_iter()
        .filter(|capability| capability.state != AgentCapabilityState::Unavailable)
        .map(|capability| capability.name)
        .collect::<Vec<_>>();
    let target_label = match lease.write_target_mode {
        AgentWriteTargetMode::Direct => "Project",
        AgentWriteTargetMode::WorkView => "Work view",
    };
    let target_path = lease_write_target_path(&lease).to_string();
    let review_instructions = if lease.write_target_mode == AgentWriteTargetMode::WorkView {
        format!(
            "When output is ready, run these from the work view:\n1. `bowline agent publish --lease {}`\n2. `bowline agent complete --lease {}`\n\nPublish for review instead of applying changes to the main workspace yourself.",
            lease.id.as_str(),
            lease.id.as_str()
        )
    } else {
        format!(
            "When output is ready, run `bowline agent complete --lease {}` from the project. Your normal filesystem edits are synced by bowline; do not use Git remotes, commits, branches, staging, or pushes as bowline's sync path.",
            lease.id.as_str()
        )
    };
    let prompt = AgentPrompt {
        recipe_id: "default-agent-lease".to_string(),
        recipe_version: 1,
        redaction: AgentPromptRedaction::Applied,
        text: format!(
            "You are helping inside a bowline agent task.\n\nTask: {}\n{}: {}\n\nWork only inside this lease target. Do not commit, push, branch, stage files, or mutate source-control refs on bowline's behalf.\n\n{}",
            lease.task, target_label, target_path, review_instructions
        ),
        allowed_tools,
        output_target: lease.output_target.clone(),
        adapter_capabilities: Vec::new(),
        instructions: context.instructions.clone(),
    };
    Ok(AgentPromptCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::AgentPrompt,
        generated_at: options.generated_at,
        workspace_id: lease.workspace_id.clone(),
        project_id: lease.project_id.clone(),
        lease,
        prompt,
        status: context.status.clone(),
        next_actions: vec![SafeAction {
            label: "Open the lease target".to_string(),
            command: Some(format!("cd {}", shell_word(&target_path))),
        }],
    })
}

pub(super) fn context_for_lease(store: &MetadataStore, lease: &AgentLease) -> AgentContextV1 {
    let setup_receipts: Vec<String> = store
        .setup_receipts(&lease.workspace_id)
        .unwrap_or_default()
        .into_iter()
        .filter(|receipt| receipt.project_id.as_ref() == Some(&lease.project_id))
        .map(|receipt| receipt.id)
        .collect();
    let attention = attention_for_lease(lease);
    let status = status_for_attention(&attention);
    let readiness = readiness_for_lease(lease, &attention, setup_receipts.len());
    let target_path = lease_write_target_path(lease).to_string();
    let target_label = lease_target_label(lease);
    AgentContextV1 {
        workspace_id: lease.workspace_id.clone(),
        project_id: lease.project_id.clone(),
        lease: lease.clone(),
        policy_version: PolicyVersion::new(DEFAULT_POLICY_VERSION),
        status,
        write_target_path: target_path.clone(),
        work_view_path: target_path.clone(),
        attention,
        capabilities: capabilities_for_lease(lease),
        index: crate::indexed::build_project_index(
            None,
            Some(target_path.clone()),
            &lease.updated_at,
        )
        .ok()
        .map(|project| project.index_status),
        hydration_budget: lease_budget_status(
            store,
            &lease.workspace_id,
            &lease.project_id,
            &lease.id,
            lease.hydrate_budget_bytes,
        )
        .ok(),
        setup_receipts,
        env: lease.env_profile.clone(),
        scopes: lease.scopes.clone(),
        readiness,
        start_work: AgentStartWork {
            cwd: target_path.clone(),
            context_command: format!("bowline agent context --lease {}", lease.id.as_str()),
            prompt_command: format!("bowline agent prompt --lease {}", lease.id.as_str()),
            safe_next_actions: vec![
                SafeAction {
                    label: format!("Open {target_label}"),
                    command: Some(format!("cd {}", shell_word(&target_path))),
                },
                SafeAction {
                    label: "Read agent context".to_string(),
                    command: Some(format!(
                        "bowline agent context --lease {}",
                        lease.id.as_str()
                    )),
                },
                SafeAction {
                    label: "Render agent prompt".to_string(),
                    command: Some(format!(
                        "bowline agent prompt --lease {}",
                        lease.id.as_str()
                    )),
                },
            ],
        },
        adapter_capabilities: Vec::new(),
        instructions: lease_instructions(lease),
    }
}

pub(super) fn readiness_for_lease(
    lease: &AgentLease,
    attention: &[StatusItem],
    setup_receipt_count: usize,
) -> AgentProjectReadiness {
    let lease_state = if lease.execution_state == AgentLeaseExecutionState::Active {
        AgentReadinessState::Ready
    } else {
        AgentReadinessState::Blocked
    };
    let output_state = match lease.output_state {
        AgentLeaseOutputState::Empty | AgentLeaseOutputState::Dirty => AgentReadinessState::Ready,
        AgentLeaseOutputState::ReviewReady | AgentLeaseOutputState::Retained => {
            AgentReadinessState::Attention
        }
        AgentLeaseOutputState::Conflicted => AgentReadinessState::Blocked,
        AgentLeaseOutputState::Accepted | AgentLeaseOutputState::Discarded => {
            AgentReadinessState::Limited
        }
    };
    let state = if attention.is_empty()
        && lease_state == AgentReadinessState::Ready
        && output_state == AgentReadinessState::Ready
    {
        AgentReadinessState::Ready
    } else if lease_state == AgentReadinessState::Blocked
        || output_state == AgentReadinessState::Blocked
    {
        AgentReadinessState::Blocked
    } else {
        AgentReadinessState::Attention
    };

    let target_path = lease_write_target_path(lease);
    let target_name = match lease.write_target_mode {
        AgentWriteTargetMode::Direct => "project",
        AgentWriteTargetMode::WorkView => "work-view",
    };
    let target_summary = match lease.write_target_mode {
        AgentWriteTargetMode::Direct => {
            "Agent writes use the real project directory and normal bowline sync."
        }
        AgentWriteTargetMode::WorkView => "Agent writes are isolated to the lease work view.",
    };
    AgentProjectReadiness {
        state,
        signals: vec![
            AgentReadinessSignal {
                name: "lease".to_string(),
                state: lease_state,
                summary: lease.status_summary.clone(),
                next_action: if lease_state == AgentReadinessState::Ready {
                    None
                } else {
                    Some(SafeAction {
                        label: "Inspect lease context".to_string(),
                        command: Some(format!(
                            "bowline agent context --lease {}",
                            lease.id.as_str()
                        )),
                    })
                },
            },
            AgentReadinessSignal {
                name: target_name.to_string(),
                state: AgentReadinessState::Ready,
                summary: target_summary.to_string(),
                next_action: Some(SafeAction {
                    label: format!("Open {}", lease_target_label(lease)),
                    command: Some(format!("cd {}", shell_word(target_path))),
                }),
            },
            AgentReadinessSignal {
                name: "setup-receipts".to_string(),
                state: AgentReadinessState::Ready,
                summary: if setup_receipt_count == 0 {
                    "No setup receipts are required or recorded for this lease.".to_string()
                } else {
                    format!("{setup_receipt_count} setup receipt(s) are visible to this lease.")
                },
                next_action: Some(SafeAction {
                    label: "Inspect setup receipts".to_string(),
                    command: Some(format!(
                        "bowline agent context --lease {}",
                        lease.id.as_str()
                    )),
                }),
            },
            AgentReadinessSignal {
                name: "output".to_string(),
                state: output_state,
                summary: format!("Agent output state is {:?}.", lease.output_state),
                next_action: if output_state == AgentReadinessState::Ready
                    || lease.write_target_mode == AgentWriteTargetMode::Direct
                {
                    None
                } else {
                    Some(SafeAction {
                        label: "Inspect work view diff".to_string(),
                        command: Some(format!("bowline review {}", lease.work_view_id.as_str())),
                    })
                },
            },
        ],
    }
}

pub(super) fn lease_write_target_path(lease: &AgentLease) -> &str {
    &lease.write_target_path
}

pub(super) fn lease_target_label(lease: &AgentLease) -> &'static str {
    match lease.write_target_mode {
        AgentWriteTargetMode::Direct => "project",
        AgentWriteTargetMode::WorkView => "work view",
    }
}

pub(super) fn lease_instructions(lease: &AgentLease) -> Vec<String> {
    let mut instructions = vec![
        "Work only inside the lease target.".to_string(),
        "Use primitive bowline tools for inspection, bounded reads, writes, review, and completion.".to_string(),
        "Do not commit, push, branch, stage files, or mutate source-control refs on bowline's behalf.".to_string(),
    ];
    match lease.write_target_mode {
        AgentWriteTargetMode::Direct => instructions
            .push("Direct lease edits go through normal bowline real-directory sync.".to_string()),
        AgentWriteTargetMode::WorkView => instructions.push(
            "Publish overlay output for review instead of applying it to the main workspace."
                .to_string(),
        ),
    }
    instructions
}

pub(super) fn capabilities() -> Vec<AgentCapability> {
    let degraded = degraded_bounds();
    [
        (
            AgentToolName::WorkspaceStatus,
            AgentToolCategory::Inspection,
            AgentCapabilityState::Available,
            None,
        ),
        (
            AgentToolName::ListCapabilities,
            AgentToolCategory::Inspection,
            AgentCapabilityState::Available,
            None,
        ),
        (
            AgentToolName::ResolvePath,
            AgentToolCategory::Inspection,
            AgentCapabilityState::Available,
            None,
        ),
        (
            AgentToolName::ExplainPathPolicy,
            AgentToolCategory::Inspection,
            AgentCapabilityState::Available,
            None,
        ),
        (
            AgentToolName::ListAttentionItems,
            AgentToolCategory::Inspection,
            AgentCapabilityState::Available,
            None,
        ),
        (
            AgentToolName::ListTreeAtSnapshot,
            AgentToolCategory::Exploration,
            AgentCapabilityState::Available,
            Some(degraded.clone()),
        ),
        (
            AgentToolName::ReadFileAtSnapshot,
            AgentToolCategory::Exploration,
            AgentCapabilityState::Available,
            Some(degraded.clone()),
        ),
        (
            AgentToolName::SearchWorkspace,
            AgentToolCategory::Exploration,
            AgentCapabilityState::Available,
            None,
        ),
        (
            AgentToolName::SymbolLookup,
            AgentToolCategory::Exploration,
            AgentCapabilityState::Available,
            None,
        ),
        (
            AgentToolName::RequestHydration,
            AgentToolCategory::Hydration,
            AgentCapabilityState::Available,
            None,
        ),
        (
            AgentToolName::GetHydrationStatus,
            AgentToolCategory::Hydration,
            AgentCapabilityState::Available,
            None,
        ),
        (
            AgentToolName::WriteOverlayFile,
            AgentToolCategory::Write,
            AgentCapabilityState::Available,
            None,
        ),
        (
            AgentToolName::ListOverlayChanges,
            AgentToolCategory::Write,
            AgentCapabilityState::Available,
            None,
        ),
        (
            AgentToolName::DiffSnapshots,
            AgentToolCategory::Write,
            AgentCapabilityState::Available,
            None,
        ),
        (
            AgentToolName::RunCommandWithReceipt,
            AgentToolCategory::Execution,
            AgentCapabilityState::Available,
            None,
        ),
        (
            AgentToolName::InspectSetupReceipts,
            AgentToolCategory::Execution,
            AgentCapabilityState::Available,
            None,
        ),
        (
            AgentToolName::ProposePolicyChange,
            AgentToolCategory::Review,
            AgentCapabilityState::Available,
            None,
        ),
        (
            AgentToolName::RequestHumanDecision,
            AgentToolCategory::Review,
            AgentCapabilityState::Available,
            None,
        ),
        (
            AgentToolName::PublishOverlayForReview,
            AgentToolCategory::Review,
            AgentCapabilityState::Available,
            None,
        ),
        (
            AgentToolName::CompleteTask,
            AgentToolCategory::Review,
            AgentCapabilityState::Available,
            None,
        ),
    ]
    .into_iter()
    .map(|(name, category, state, bounds)| AgentCapability {
        name,
        category,
        state,
        bounds,
    })
    .collect()
}

pub(super) fn capabilities_for_lease(lease: &AgentLease) -> Vec<AgentCapability> {
    capabilities()
        .into_iter()
        .filter(|capability| {
            lease.write_target_mode == AgentWriteTargetMode::WorkView
                || !matches!(
                    capability.name,
                    AgentToolName::PublishOverlayForReview
                        | AgentToolName::ListOverlayChanges
                        | AgentToolName::DiffSnapshots
                )
        })
        .collect()
}

pub(super) fn default_scopes(path: &str) -> AgentLeaseScopes {
    let scope = AgentLeaseScope {
        roots: vec![path.to_string()],
        classifications: Vec::new(),
        max_bytes_per_read: Some(MAX_READ_BYTES),
        max_files_per_request: Some(MAX_TREE_FILES),
        max_depth: Some(MAX_TREE_DEPTH),
    };
    AgentLeaseScopes {
        read: scope.clone(),
        write: scope,
    }
}

pub(super) fn default_env_profile(write_target_mode: AgentWriteTargetMode) -> AgentEnvProfile {
    AgentEnvProfile {
        name: "default".to_string(),
        materialization: match write_target_mode {
            AgentWriteTargetMode::Direct => AgentEnvMaterialization::ProjectPath,
            AgentWriteTargetMode::WorkView => AgentEnvMaterialization::LeaseWorkView,
        },
        available_keys: Vec::new(),
        restrictions: Vec::new(),
        grant_ids: Vec::new(),
    }
}

pub(super) fn shell_word(value: &str) -> String {
    if !value.is_empty()
        && value.bytes().all(|byte| {
            matches!(
                byte,
                b'a'..=b'z'
                    | b'A'..=b'Z'
                    | b'0'..=b'9'
                    | b'/'
                    | b'.'
                    | b'_'
                    | b'-'
                    | b':'
                    | b'+'
                    | b'='
                    | b'@'
                    | b'%'
                    | b'~'
            )
        })
    {
        return value.to_string();
    }

    format!("'{}'", value.replace('\'', "'\\''"))
}

pub(super) fn attention_for_lease(lease: &AgentLease) -> Vec<StatusItem> {
    let Some((summary, event_name)) = lease_attention_summary(lease) else {
        return Vec::new();
    };
    vec![StatusItem {
        kind: StatusItemKind::Lease,
        summary: summary.to_string(),
        subject: Some(StatusSubject {
            kind: StatusSubjectKind::Lease,
            id: lease.id.as_str().to_string(),
            path: Some(lease_write_target_path(lease).to_string()),
        }),
        path: Some(lease_write_target_path(lease).to_string()),
        classification: None,
        mode: None,
        access: Vec::new(),
        event_id: None,
        event_name: Some(event_name),
        device_id: Some(lease.device_id.clone()),
        lease_id: Some(lease.id.clone()),
        project_id: Some(lease.project_id.clone()),
        snapshot_id: Some(lease.base_snapshot_id.clone()),
        policy_version: Some(PolicyVersion::new(DEFAULT_POLICY_VERSION)),
        env_record_id: None,
    }]
}

pub(super) fn lease_attention_summary(lease: &AgentLease) -> Option<(&'static str, EventName)> {
    if lease.output_state == AgentLeaseOutputState::ReviewReady {
        return Some((
            "Agent output is ready for review.",
            EventName::LeaseReviewReady,
        ));
    }
    if lease.output_state == AgentLeaseOutputState::Conflicted {
        return Some(("Agent output has conflicts.", EventName::LeaseBlocked));
    }
    if lease.execution_state == AgentLeaseExecutionState::Blocked {
        return Some(("Agent lease is blocked.", EventName::LeaseBlocked));
    }
    None
}

pub(super) fn status_for_attention(attention: &[StatusItem]) -> WorkspaceStatus {
    if attention.is_empty() {
        return WorkspaceStatus::healthy();
    }
    WorkspaceStatus {
        level: StatusLevel::Attention,
        attention_items: attention.iter().map(|item| item.summary.clone()).collect(),
    }
}
