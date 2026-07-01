use super::*;

pub(super) fn action_for(args: &ResolveArgs) -> ResolveAction {
    match &args.decision {
        Some(ResolveDecision::Accept(_)) => ResolveAction::Accept,
        Some(ResolveDecision::Reject(_)) => ResolveAction::Reject,
        None if args.diff.is_some() => ResolveAction::Diff,
        None if args.agent.is_some() => ResolveAction::Agent,
        None if args.copy_prompt => ResolveAction::CopyPrompt,
        None => ResolveAction::List,
    }
}

pub(super) fn selected_conflict_id(args: &ResolveArgs) -> Option<String> {
    match &args.decision {
        Some(ResolveDecision::Accept(id)) | Some(ResolveDecision::Reject(id)) => Some(id.clone()),
        None if args.diff.is_some() => args.diff.clone(),
        None => None,
    }
}

pub(super) fn status_for(
    args: &ResolveArgs,
    conflicts: &[ResolveConflict],
    available_agents: &[AvailableAgent],
    decision_result: Result<&ResolveDecisionApplied, &ResolveError>,
) -> ResolveStatus {
    if let Err(error) = decision_result {
        return ResolveStatus {
            level: "attention",
            summary: error.to_string(),
        };
    }

    if let Some(id) = &args.diff
        && !conflicts.iter().any(|conflict| conflict.id == *id)
    {
        return ResolveStatus {
            level: "attention",
            summary: format!("conflict `{id}` was not found"),
        };
    }

    if let Some(agent) = args.agent
        && !available_agents
            .iter()
            .any(|available| available.name == agent)
    {
        return ResolveStatus {
            level: "limited",
            summary: format!("{} is not available on PATH.", agent.as_str()),
        };
    }

    if requested_agent_secret_scope_denied(args, conflicts) {
        return ResolveStatus {
            level: "attention",
            summary: "secret-bearing conflict requires explicit agent secret-read scope; use --copy-prompt for a redacted handoff".to_string(),
        };
    }

    if let Ok(applied) = decision_result
        && args.decision.is_some()
    {
        if !conflicts.is_empty() {
            return ResolveStatus {
                level: "attention",
                summary: format!(
                    "{}; {} unresolved conflict{} remain",
                    applied.summary,
                    conflicts.len(),
                    if conflicts.len() == 1 { "" } else { "s" }
                ),
            };
        }
        return ResolveStatus {
            level: "healthy",
            summary: applied.summary.clone(),
        };
    }

    if conflicts.is_empty() {
        ResolveStatus {
            level: "healthy",
            summary: "no unresolved conflict bundles found".to_string(),
        }
    } else {
        ResolveStatus {
            level: "attention",
            summary: format!("{} unresolved conflict bundle(s) found", conflicts.len()),
        }
    }
}

pub(super) fn requested_agent_secret_scope_denied(
    args: &ResolveArgs,
    conflicts: &[ResolveConflict],
) -> bool {
    if !matches!(action_for(args), ResolveAction::Agent) || agent_secret_scope_allowed() {
        return false;
    }
    let selected = selected_conflict_id(args)
        .as_deref()
        .and_then(|id| conflicts.iter().find(|conflict| conflict.id == id))
        .or_else(|| conflicts.first());
    selected.is_some_and(|conflict| conflict.contains_secrets)
}

pub(super) fn agent_secret_scope_allowed() -> bool {
    env::var("BOWLINE_ALLOW_SECRET_CONFLICT_AGENT")
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

pub(super) fn next_actions(
    project_or_path: &str,
    conflicts: &[ResolveConflict],
    available_agents: &[AvailableAgent],
) -> Vec<ResolveAvailableAction> {
    if conflicts.is_empty() {
        return vec![ResolveAvailableAction {
            label: "Check workspace status".to_string(),
            command: Some(format!("bowline status {}", shell_word(project_or_path))),
        }];
    }

    available_actions(project_or_path, conflicts, available_agents)
}

pub(super) fn available_actions(
    project_or_path: &str,
    conflicts: &[ResolveConflict],
    available_agents: &[AvailableAgent],
) -> Vec<ResolveAvailableAction> {
    let mut actions = Vec::new();
    if !conflicts.is_empty() {
        actions.push(ResolveAvailableAction {
            label: "Print repair prompt".to_string(),
            command: Some(format!(
                "bowline resolve {} --copy-prompt",
                shell_word(project_or_path)
            )),
        });
        for agent in available_agents {
            actions.push(ResolveAvailableAction {
                label: format!("Prepare {} repair prompt", agent.name.as_str()),
                command: Some(format!(
                    "bowline resolve {} --agent {}",
                    shell_word(project_or_path),
                    agent.name.as_str()
                )),
            });
        }
        for conflict in conflicts {
            actions.push(ResolveAvailableAction {
                label: format!("Open diff {}", conflict.id),
                command: Some(format!(
                    "bowline resolve {} --diff {}",
                    shell_word(project_or_path),
                    shell_word(&conflict.id)
                )),
            });
            actions.push(ResolveAvailableAction {
                label: format!("Accept {}", conflict.id),
                command: Some(format!(
                    "bowline resolve {} --accept {}",
                    shell_word(project_or_path),
                    shell_word(&conflict.id)
                )),
            });
            actions.push(ResolveAvailableAction {
                label: format!("Reject {}", conflict.id),
                command: Some(format!(
                    "bowline resolve {} --reject {}",
                    shell_word(project_or_path),
                    shell_word(&conflict.id)
                )),
            });
        }
    }
    actions
}

pub(super) fn shell_word(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| matches!(byte, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'/' | b'.' | b'_' | b'-' | b':' | b'+' | b'=' | b'@' | b'%'))
    {
        return value.to_string();
    }

    format!("'{}'", value.replace('\'', "'\\''"))
}

pub(super) fn detect_agents() -> Vec<AvailableAgent> {
    crate::agent_adapters::detect_agent_cli_capabilities()
        .into_iter()
        .filter_map(|capability| {
            if !capability.available {
                return None;
            }
            let name = resolve_agent_for_cli_name(capability.name)?;
            let command = capability.command.clone()?;
            Some(AvailableAgent {
                name,
                command,
                capability,
            })
        })
        .collect()
}

pub(super) fn resolve_agent_for_cli_name(name: AgentCliName) -> Option<ResolveAgent> {
    match name {
        AgentCliName::Codex => Some(ResolveAgent::Codex),
        AgentCliName::Claude => Some(ResolveAgent::Claude),
        AgentCliName::Cursor => Some(ResolveAgent::Cursor),
    }
}

pub fn parse_agent(value: &str) -> Option<ResolveAgent> {
    crate::agent_adapters::parse_cli_name(value).and_then(resolve_agent_for_cli_name)
}
