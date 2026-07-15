use super::*;

use crate::io_helpers::shell_word;

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
) -> Vec<RepairCommand> {
    if conflicts.is_empty() {
        let root = status_root_for_project_or_path(project_or_path);
        return vec![RepairCommand::inspect(
            "Check workspace status".to_string(),
            Some(format!(
                "bowline status --root {} --project {}",
                shell_word(root),
                shell_word(project_or_path)
            )),
        )];
    }

    available_actions(project_or_path, conflicts, available_agents)
        .into_iter()
        .map(resolve_action_to_safe_action)
        .collect()
}

fn resolve_action_to_safe_action(action: ResolveAvailableAction) -> RepairCommand {
    if action.mutates {
        RepairCommand::mutating(action.label, action.command)
    } else {
        RepairCommand::inspect(action.label, action.command)
    }
}

pub(super) fn available_actions(
    project_or_path: &str,
    conflicts: &[ResolveConflict],
    available_agents: &[AvailableAgent],
) -> Vec<ResolveAvailableAction> {
    let mut actions = Vec::new();
    if !conflicts.is_empty() {
        if let Some(shared_root) = homogeneous_conflict_root(project_or_path, conflicts) {
            actions.push(ResolveAvailableAction {
                label: "Print repair prompt".to_string(),
                command: Some(format!(
                    "bowline resolve {} --copy-prompt",
                    shell_word(shared_root)
                )),
                mutates: false,
            });
            for agent in available_agents {
                actions.push(ResolveAvailableAction {
                    label: format!("Prepare {} repair prompt", agent.name.as_str()),
                    command: Some(format!(
                        "bowline resolve {} --agent {}",
                        shell_word(shared_root),
                        agent.name.as_str()
                    )),
                    mutates: false,
                });
            }
        }
        for conflict in conflicts {
            let conflict_root = conflict
                .workspace_root
                .as_deref()
                .unwrap_or(project_or_path);
            actions.push(ResolveAvailableAction {
                label: format!("Open diff {}", conflict.id),
                command: Some(format!(
                    "bowline resolve {} --diff {}",
                    shell_word(conflict_root),
                    shell_word(&conflict.id)
                )),
                mutates: false,
            });
            actions.push(ResolveAvailableAction {
                label: format!("Accept {}", conflict.id),
                command: Some(format!(
                    "bowline resolve {} --accept {}",
                    shell_word(conflict_root),
                    shell_word(&conflict.id)
                )),
                mutates: true,
            });
            actions.push(ResolveAvailableAction {
                label: format!("Reject {}", conflict.id),
                command: Some(format!(
                    "bowline resolve {} --reject {}",
                    shell_word(conflict_root),
                    shell_word(&conflict.id)
                )),
                mutates: true,
            });
        }
    }
    actions
}

fn homogeneous_conflict_root<'a>(
    fallback: &'a str,
    conflicts: &'a [ResolveConflict],
) -> Option<&'a str> {
    let first = conflicts
        .first()
        .and_then(|conflict| conflict.workspace_root.as_deref())
        .unwrap_or(fallback);
    conflicts
        .iter()
        .all(|conflict| conflict.workspace_root.as_deref().unwrap_or(fallback) == first)
        .then_some(first)
}

fn status_root_for_project_or_path(project_or_path: &str) -> &str {
    if project_or_path == "~"
        || project_or_path.starts_with("~/")
        || std::path::Path::new(project_or_path).is_absolute()
    {
        project_or_path
    } else {
        "."
    }
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

#[cfg(test)]
mod tests {
    use super::next_actions;

    #[test]
    fn no_conflict_status_action_uses_requested_path_root() {
        let relative = next_actions("apps/web", &[], &[]);
        assert_eq!(
            relative[0].command.as_deref(),
            Some("bowline status --root . --project apps/web")
        );
        assert!(!relative[0].mutates);

        let home_relative = next_actions("~/Code Projects/apps/web", &[], &[]);
        assert_eq!(
            home_relative[0].command.as_deref(),
            Some(
                "bowline status --root ~/'Code Projects/apps/web' --project ~/'Code Projects/apps/web'"
            )
        );
    }
}
