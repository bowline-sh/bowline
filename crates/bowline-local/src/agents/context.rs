use super::*;

pub fn agent_context(
    options: AgentLeaseSelectorOptions,
) -> Result<AgentContextCommandOutput, AgentError> {
    let store = MetadataStore::open(resolve_db_path(options.db_path)?)?;
    let lease = load_lease(&store, &options.lease_id, &options.generated_at)?;
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
    let freshness_note = prompt_freshness_note(&context);
    let review_instructions = if lease.write_target_mode == AgentWriteTargetMode::WorkView {
        format!(
            "Your edits stay inside this lease's work view and sync automatically. When the task is finished, run `bowline agent complete --lease {}`. A human reviews and accepts the work view with `bowline work review` / `bowline work accept`; do not apply changes to the main workspace yourself.",
            lease.id.as_str()
        )
    } else {
        format!(
            "Your normal filesystem edits are synced by bowline automatically. When the task is finished, run `bowline agent complete --lease {}`. Do not use Git remotes, commits, branches, staging, or pushes as bowline's sync path.",
            lease.id.as_str()
        )
    };
    let prompt = AgentPrompt {
        recipe_id: "default-agent-lease".to_string(),
        recipe_version: 1,
        redaction: AgentPromptRedaction::Applied,
        text: format!(
            "You are helping inside a bowline agent task.\n\nTask: {}\n{}: {}\n{}Work only inside this lease target. Do not commit, push, branch, stage files, or mutate source-control refs on bowline's behalf.\n\n{}",
            lease.task, target_label, target_path, freshness_note, review_instructions
        ),
        allowed_tools,
        adapter_capabilities: Vec::new(),
        instructions: context.instructions.clone(),
    };
    // Agent-output next_actions are emitted empty here; 067 owns converting the
    // surviving readiness/truncation signals and removing these fields.
    Ok(AgentPromptCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::AgentPrompt,
        generated_at: options.generated_at,
        workspace_id: lease.workspace_id.clone(),
        project_id: lease.project_id.clone(),
        lease,
        prompt,
        status: context.status.clone(),
        next_actions: Vec::new(),
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
    let stale_bases =
        crate::status::snapshot_stale_bases(store, &lease.workspace_id, Some(&lease.project_id))
            .unwrap_or_else(|_| {
                vec![StaleBaseStatus::snapshot(
                    FreshnessVerdict::Unknown,
                    "Project freshness could not be read from local metadata.",
                    Some(lease.project_id.clone()),
                    None,
                    Some(lease.base_snapshot_id.clone()),
                    None,
                    Some("bowline status --watch".to_string()),
                )]
            });
    let freshness = crate::status::freshness_for_stale_bases(&stale_bases);
    let mut attention = attention_for_lease(lease);
    attention.extend(freshness_attention_items(lease, &stale_bases));
    let status = status_for_attention(&attention);
    let readiness = readiness_for_lease(
        lease,
        &attention,
        setup_receipts.len(),
        freshness,
        &stale_bases,
    );
    let target_path = lease_write_target_path(lease).to_string();
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
        freshness,
        stale_bases,
        setup_receipts,
        readiness,
        start_work: AgentStartWork {
            cwd: target_path.clone(),
            context_command: format!("bowline agent context --lease {}", lease.id.as_str()),
            prompt_command: format!("bowline agent prompt --lease {}", lease.id.as_str()),
            // Agent-output safe_next_actions are emitted empty (067 handshake); the
            // cwd/context_command/prompt_command above carry the same navigation.
            safe_next_actions: Vec::new(),
        },
        adapter_capabilities: Vec::new(),
        instructions: lease_instructions(lease),
    }
}

pub(super) fn readiness_for_lease(
    lease: &AgentLease,
    attention: &[StatusItem],
    setup_receipt_count: usize,
    freshness: FreshnessVerdict,
    stale_bases: &[StaleBaseStatus],
) -> AgentProjectReadiness {
    let lease_state = if lease.session_state == AgentSessionState::Open {
        AgentReadinessState::Ready
    } else {
        AgentReadinessState::Blocked
    };
    let state = if attention.is_empty() && lease_state == AgentReadinessState::Ready {
        AgentReadinessState::Ready
    } else if lease_state == AgentReadinessState::Blocked {
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
    let mut signals = vec![
        AgentReadinessSignal {
            name: "lease".to_string(),
            state: lease_state,
            summary: lease.status_summary.clone(),
            next_action: if lease_state == AgentReadinessState::Ready {
                None
            } else {
                Some(RepairCommand::inspect(
                    "Inspect lease context".to_string(),
                    Some(format!(
                        "bowline agent context --lease {}",
                        lease.id.as_str()
                    )),
                ))
            },
        },
        AgentReadinessSignal {
            name: target_name.to_string(),
            state: AgentReadinessState::Ready,
            summary: target_summary.to_string(),
            next_action: Some(RepairCommand::inspect(
                format!("Open {}", lease_target_label(lease)),
                Some(format!("cd {}", shell_word(target_path))),
            )),
        },
    ];
    if freshness.needs_attention() {
        signals.push(AgentReadinessSignal {
            name: "freshness".to_string(),
            state: AgentReadinessState::Attention,
            summary: freshness_summary(stale_bases),
            next_action: freshness_next_action(stale_bases),
        });
    }
    signals.extend([AgentReadinessSignal {
        name: "setup-receipts".to_string(),
        state: AgentReadinessState::Ready,
        summary: if setup_receipt_count == 0 {
            "No setup receipts are required or recorded for this lease.".to_string()
        } else {
            format!("{setup_receipt_count} setup receipt(s) are visible to this lease.")
        },
        next_action: Some(RepairCommand::inspect(
            "Inspect setup receipts".to_string(),
            Some(format!(
                "bowline agent context --lease {}",
                lease.id.as_str()
            )),
        )),
    }]);
    AgentProjectReadiness { state, signals }
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
        "Do not commit, push, branch, stage files, or mutate source-control refs on bowline's behalf.".to_string(),
    ];
    match lease.session_state {
        AgentSessionState::Open | AgentSessionState::Provisional => instructions.insert(
            1,
            format!(
                "Mark the session finished with `bowline agent complete --lease {}`.",
                lease.id.as_str()
            ),
        ),
        AgentSessionState::Cancelled => {
            instructions.insert(
                1,
                "This session is cancelled; do not continue editing.".to_string(),
            );
        }
        AgentSessionState::Completed => {
            instructions.insert(
                1,
                "This session is complete; no completion action remains.".to_string(),
            );
        }
    }
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

fn freshness_attention_items(
    lease: &AgentLease,
    stale_bases: &[StaleBaseStatus],
) -> Vec<StatusItem> {
    stale_bases
        .iter()
        .filter(|status| status.verdict.needs_attention())
        .map(|status| StatusItem {
            kind: StatusItemKind::Source,
            summary: status.summary.clone(),
            subject: status.project_id.as_ref().map(|project_id| StatusSubject {
                kind: StatusSubjectKind::Project,
                id: project_id.as_str().to_string(),
                path: status.project_path.clone(),
            }),
            path: status.project_path.clone(),
            classification: None,
            mode: None,
            access: Vec::new(),
            event_id: None,
            event_name: Some(EventName::SourceStale),
            device_id: Some(lease.device_id.clone()),
            lease_id: Some(lease.id.clone()),
            project_id: status
                .project_id
                .clone()
                .or_else(|| Some(lease.project_id.clone())),
            snapshot_id: status
                .base_snapshot_id
                .clone()
                .or_else(|| Some(lease.base_snapshot_id.clone())),
            policy_version: Some(PolicyVersion::new(DEFAULT_POLICY_VERSION)),
            env_record_id: None,
        })
        .collect()
}

fn freshness_summary(stale_bases: &[StaleBaseStatus]) -> String {
    stale_bases
        .iter()
        .find(|status| status.verdict.needs_attention())
        .map(|status| status.summary.clone())
        .unwrap_or_else(|| "Project freshness is current.".to_string())
}

fn freshness_next_action(stale_bases: &[StaleBaseStatus]) -> Option<RepairCommand> {
    stale_bases.iter().find_map(|status| {
        status.remedy_command.as_ref().map(|command| {
            RepairCommand::inspect("Inspect freshness".to_string(), Some(command.clone()))
        })
    })
}

fn prompt_freshness_note(context: &AgentContextV1) -> String {
    if !context.freshness.needs_attention() {
        return "\n".to_string();
    }
    let summary = context
        .stale_bases
        .iter()
        .find(|status| status.verdict.needs_attention())
        .map(|status| status.summary.as_str())
        .unwrap_or("Project freshness needs attention.");
    let remedy = context.stale_bases.iter().find_map(|status| {
        status
            .remedy_command
            .as_ref()
            .map(|command| format!(" Run `{command}` before relying on this base."))
    });
    format!(
        "\nFreshness: {}. {}{}\n\n",
        freshness_label(context.freshness),
        summary,
        remedy.unwrap_or_default()
    )
}

fn freshness_label(freshness: FreshnessVerdict) -> &'static str {
    match freshness {
        FreshnessVerdict::Current => "current",
        FreshnessVerdict::Behind => "behind",
        FreshnessVerdict::Diverged => "diverged",
        FreshnessVerdict::Unknown => "unknown",
    }
}

pub(super) fn capabilities() -> Vec<AgentCapability> {
    [
        (
            AgentToolName::WorkspaceStatus,
            AgentToolCategory::Inspection,
            AgentCapabilityState::Available,
        ),
        (
            AgentToolName::ListCapabilities,
            AgentToolCategory::Inspection,
            AgentCapabilityState::Available,
        ),
        (
            AgentToolName::ResolvePath,
            AgentToolCategory::Inspection,
            AgentCapabilityState::Available,
        ),
        (
            AgentToolName::ListOverlayChanges,
            AgentToolCategory::Inspection,
            AgentCapabilityState::Available,
        ),
    ]
    .into_iter()
    .map(|(name, category, state)| AgentCapability {
        name,
        category,
        state,
    })
    .collect()
}

pub(super) fn capabilities_for_lease(lease: &AgentLease) -> Vec<AgentCapability> {
    capabilities()
        .into_iter()
        .filter(|capability| {
            // The work-view handoff reader is only meaningful for work-view
            // leases; direct-write leases have no overlay to hand off.
            lease.write_target_mode == AgentWriteTargetMode::WorkView
                || !matches!(capability.name, AgentToolName::ListOverlayChanges)
        })
        .collect()
}

pub(super) fn shell_word(value: &str) -> String {
    if value == "~" {
        return "~".to_string();
    }
    if let Some(rest) = value.strip_prefix("~/") {
        if rest.is_empty() {
            return "~/".to_string();
        }
        return format!("~/{}", bowline_core::shell::quote_word(rest));
    }
    bowline_core::shell::quote_word(value)
}

pub(super) fn attention_for_lease(_lease: &AgentLease) -> Vec<StatusItem> {
    Vec::new()
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
