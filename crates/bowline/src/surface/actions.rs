use bowline_core::{
    commands::{ActionsCommandOutput, CONTRACT_VERSION, CommandName, StatusCommandOutput},
    status::{SafeAction, StatusLevel},
};

pub fn from_status(status: &StatusCommandOutput) -> ActionsCommandOutput {
    let mut actions = status.next_actions.clone();
    for limit in &status.limits {
        if let Some(action) = limit
            .still_works
            .iter()
            .find(|item| item.as_str() == "status")
            .map(|_| SafeAction {
                label: format!("Inspect {}", limit.capability),
                command: Some("bowline status".to_string()),
            })
        {
            push_unique(&mut actions, action);
        }
    }
    if let Some(index) = &status.index
        && let Some(action) = &index.next_action
    {
        push_unique(&mut actions, action.clone());
    }
    if let Some(budget) = &status.hydration_budget
        && let Some(action) = &budget.next_action
    {
        push_unique(&mut actions, action.clone());
    }

    let non_actions = if actions.is_empty() && status.status.level == StatusLevel::Healthy {
        vec!["Nothing needs action right now.".to_string()]
    } else {
        Vec::new()
    };

    ActionsCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::Actions,
        generated_at: status.generated_at.clone(),
        workspace_id: Some(status.workspace_id.clone()),
        project_id: status.project_id.clone(),
        scope: status.scope,
        status: status.status.clone(),
        actions,
        non_actions,
    }
}

fn push_unique(actions: &mut Vec<SafeAction>, action: SafeAction) {
    if actions.iter().any(|existing| {
        if action.command.is_some() {
            existing.command == action.command
        } else {
            existing.label == action.label && existing.command == action.command
        }
    }) {
        return;
    }
    actions.push(action);
}

#[cfg(test)]
mod tests {
    use bowline_core::commands::StatusCommandOutput;

    use super::from_status;

    #[test]
    fn conflict_actions_match_golden_output() {
        let status: StatusCommandOutput = serde_json::from_str(include_str!(
            "../../../../tests/contracts/status/conflict.json"
        ))
        .expect("conflict status fixture should parse");
        let rendered = crate::surface::human::render_actions(&from_status(&status));

        assert_eq!(
            rendered,
            include_str!("../../../../tests/golden/cli/actions-conflict.txt")
        );
    }

    #[test]
    fn status_actions_cover_pending_devices_and_sync_without_placeholders() {
        let pending_device: StatusCommandOutput = serde_json::from_str(include_str!(
            "../../../../tests/contracts/status/pending-device.json"
        ))
        .expect("pending device status fixture should parse");
        let pending_actions = from_status(&pending_device).actions;
        assert!(
            pending_actions
                .iter()
                .any(|action| action.command.as_deref()
                    == Some("bowline approve device-request:ws_code:theo-mac"))
        );
        assert!(
            pending_actions
                .iter()
                .any(|action| action.command.as_deref() == Some("bowline status"))
        );
    }

    #[test]
    fn status_actions_deduplicate_derived_actions() {
        let mut status: StatusCommandOutput = serde_json::from_str(include_str!(
            "../../../../tests/contracts/status/healthy.json"
        ))
        .expect("healthy status fixture should parse");
        status.next_actions.push(bowline_core::status::SafeAction {
            label: "Inspect sync status".to_string(),
            command: Some("bowline status".to_string()),
        });
        status.limits.push(bowline_core::status::LimitedCapability {
            capability: "sync".to_string(),
            unavailable_because: "sync degraded".to_string(),
            still_works: vec!["status".to_string()],
            path: None,
        });

        let actions = from_status(&status).actions;
        assert_eq!(
            actions
                .iter()
                .filter(|action| action.command.as_deref() == Some("bowline status"))
                .count(),
            1
        );
    }
}
