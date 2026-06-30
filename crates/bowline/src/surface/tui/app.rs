use std::io;
use std::time::Duration;

use crossterm::event::{self, Event};
#[cfg(test)]
use ratatui::{Terminal, backend::TestBackend};

use super::{
    input::{InputOutcome, apply_key},
    model::TuiModel,
    render,
    terminal::TerminalSession,
};

pub fn run_app(mut model: TuiModel) -> io::Result<Option<String>> {
    let mut session = TerminalSession::enter()?;
    loop {
        session
            .terminal_mut()
            .draw(|frame| render::render(frame, &model))?;
        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        match apply_key(&mut model, key) {
            InputOutcome::Continue => {}
            InputOutcome::Quit => return Ok(None),
            InputOutcome::Confirmed => {
                return Ok(model
                    .confirmed_action()
                    .and_then(|action| action.command.clone()));
            }
        }
    }
}

#[cfg(test)]
pub fn render_snapshot(model: &TuiModel, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal should initialize");
    terminal
        .draw(|frame| render::render(frame, model))
        .expect("test terminal should draw");
    terminal.backend().to_string()
}

#[cfg(test)]
mod tests {
    use bowline_core::{
        commands::{ActionsCommandOutput, CONTRACT_VERSION, CommandName},
        status::{SafeAction, StatusLevel, StatusScope, WorkspaceStatus},
    };

    use super::{TuiModel, render_snapshot};

    #[test]
    fn snapshot_renders_actions_in_small_terminal() {
        let output = ActionsCommandOutput {
            contract_version: CONTRACT_VERSION,
            command: CommandName::Actions,
            generated_at: "2026-06-25T12:00:00Z".to_string(),
            workspace_id: None,
            project_id: None,
            scope: Some(StatusScope::Project),
            status: WorkspaceStatus {
                level: StatusLevel::Attention,
                attention_items: vec!["Conflict needs resolution.".to_string()],
            },
            actions: vec![SafeAction {
                label: "Resolve conflict".to_string(),
                command: Some("bowline resolve ~/Code/app".to_string()),
            }],
            non_actions: Vec::new(),
        };
        let snapshot = render_snapshot(&TuiModel::from_actions(&output), 48, 10);

        assert!(snapshot.contains("bowline"));
        assert!(snapshot.contains("Resolve conflict"));
        assert!(snapshot.contains("Selected"));
        assert!(snapshot.contains("Command: bowline resolve ~/Code/app"));
        assert!(snapshot.contains("Home/End jump"));
    }

    #[test]
    fn snapshot_renders_pending_device_approval() {
        let output = ActionsCommandOutput {
            contract_version: CONTRACT_VERSION,
            command: CommandName::Actions,
            generated_at: "2026-06-25T12:00:00Z".to_string(),
            workspace_id: None,
            project_id: None,
            scope: Some(StatusScope::Workspace),
            status: WorkspaceStatus {
                level: StatusLevel::Attention,
                attention_items: vec!["Dev-Mac is waiting for approval.".to_string()],
            },
            actions: vec![
                SafeAction {
                    label: "Approve Dev-Mac".to_string(),
                    command: Some("bowline approve device-request:dev-mac".to_string()),
                },
                SafeAction {
                    label: "Inspect status".to_string(),
                    command: Some("bowline status".to_string()),
                },
            ],
            non_actions: Vec::new(),
        };
        let snapshot = render_snapshot(&TuiModel::from_actions(&output), 72, 14);

        assert!(snapshot.contains("Approve Dev-Mac"));
        assert!(snapshot.contains("Inspect status"));
        assert!(snapshot.contains("[changes]"));
        assert!(snapshot.contains("A decision or repair path needs attention."));
        assert!(
            snapshot.contains("Enter asks for confirmation"),
            "\n{snapshot}"
        );
    }

    #[test]
    fn snapshot_renders_degraded_and_recovery_actions() {
        let output = ActionsCommandOutput {
            contract_version: CONTRACT_VERSION,
            command: CommandName::Actions,
            generated_at: "2026-06-25T12:00:00Z".to_string(),
            workspace_id: None,
            project_id: None,
            scope: Some(StatusScope::Project),
            status: WorkspaceStatus {
                level: StatusLevel::Limited,
                attention_items: vec![
                    "Sync is degraded.".to_string(),
                    "Recovery Key needs verification.".to_string(),
                ],
            },
            actions: vec![
                SafeAction {
                    label: "Inspect sync".to_string(),
                    command: Some("bowline status".to_string()),
                },
                SafeAction {
                    label: "Verify Recovery Key".to_string(),
                    command: Some("bowline recover verify rk_demo".to_string()),
                },
            ],
            non_actions: Vec::new(),
        };
        let snapshot = render_snapshot(&TuiModel::from_actions(&output), 72, 12);

        assert!(snapshot.contains("Inspect sync"));
        assert!(snapshot.contains("Verify Recovery Key"));
    }

    #[test]
    fn snapshot_renders_no_action_state_without_empty_actions_panel() {
        let output = ActionsCommandOutput {
            contract_version: CONTRACT_VERSION,
            command: CommandName::Actions,
            generated_at: "2026-06-25T12:00:00Z".to_string(),
            workspace_id: None,
            project_id: None,
            scope: Some(StatusScope::Project),
            status: WorkspaceStatus {
                level: StatusLevel::Healthy,
                attention_items: Vec::new(),
            },
            actions: Vec::new(),
            non_actions: vec!["Nothing needs action right now.".to_string()],
        };
        let snapshot = render_snapshot(&TuiModel::from_actions(&output), 72, 10);

        assert!(snapshot.contains("HEALTHY"));
        assert!(snapshot.contains("State"));
        assert!(snapshot.contains("Nothing needs action right now."));
        assert!(!snapshot.contains("Actions"));
    }

    #[test]
    fn snapshot_renders_attention_details_without_actions() {
        let output = ActionsCommandOutput {
            contract_version: CONTRACT_VERSION,
            command: CommandName::Actions,
            generated_at: "2026-06-25T12:00:00Z".to_string(),
            workspace_id: None,
            project_id: None,
            scope: Some(StatusScope::Project),
            status: WorkspaceStatus {
                level: StatusLevel::Attention,
                attention_items: vec!["Sync needs a fresh observation.".to_string()],
            },
            actions: Vec::new(),
            non_actions: Vec::new(),
        };
        let snapshot = render_snapshot(&TuiModel::from_actions(&output), 72, 10);

        assert!(snapshot.contains("ATTENTION"));
        assert!(snapshot.contains("Sync needs a fresh observation."));
        assert!(!snapshot.contains("Nothing needs action right now."));
    }

    #[test]
    fn snapshot_renders_confirmation_footer() {
        let output = ActionsCommandOutput {
            contract_version: CONTRACT_VERSION,
            command: CommandName::Actions,
            generated_at: "2026-06-25T12:00:00Z".to_string(),
            workspace_id: None,
            project_id: None,
            scope: Some(StatusScope::Workspace),
            status: WorkspaceStatus {
                level: StatusLevel::Attention,
                attention_items: vec!["Dev-Mac is waiting for approval.".to_string()],
            },
            actions: vec![SafeAction {
                label: "Approve Dev-Mac".to_string(),
                command: Some("bowline approve device-request:dev-mac".to_string()),
            }],
            non_actions: Vec::new(),
        };
        let mut model = TuiModel::from_actions(&output);
        model.request_confirmation();

        let snapshot = render_snapshot(&model, 72, 12);

        assert!(snapshot.contains("Confirm"));
        assert!(snapshot.contains("Command: bowline approve device-request:dev-mac"));
        assert!(snapshot.contains("Enter runs the selected command."));
        assert!(snapshot.contains("Esc cancels."));
    }

    #[test]
    fn snapshot_keeps_confirmation_detail_bound_to_confirmed_action() {
        let output = ActionsCommandOutput {
            contract_version: CONTRACT_VERSION,
            command: CommandName::Actions,
            generated_at: "2026-06-25T12:00:00Z".to_string(),
            workspace_id: None,
            project_id: None,
            scope: Some(StatusScope::Workspace),
            status: WorkspaceStatus {
                level: StatusLevel::Attention,
                attention_items: vec!["Device trust needs a decision.".to_string()],
            },
            actions: vec![
                SafeAction {
                    label: "Approve Dev-Mac".to_string(),
                    command: Some("bowline approve device-request:dev-mac".to_string()),
                },
                SafeAction {
                    label: "Revoke Dev-Mac".to_string(),
                    command: Some("bowline revoke device-request:dev-mac".to_string()),
                },
            ],
            non_actions: Vec::new(),
        };
        let mut model = TuiModel::from_actions(&output);
        model.request_confirmation();
        model.move_down();

        let snapshot = render_snapshot(&model, 96, 14);

        assert!(snapshot.contains("Command: bowline approve device-request:dev-mac"));
        assert!(!snapshot.contains("Command: bowline revoke device-request:dev-mac"));
    }

    #[test]
    fn snapshot_renders_no_command_action_as_note() {
        let output = ActionsCommandOutput {
            contract_version: CONTRACT_VERSION,
            command: CommandName::Actions,
            generated_at: "2026-06-25T12:00:00Z".to_string(),
            workspace_id: None,
            project_id: None,
            scope: Some(StatusScope::Project),
            status: WorkspaceStatus {
                level: StatusLevel::Attention,
                attention_items: vec!["Path policy needs review.".to_string()],
            },
            actions: vec![SafeAction {
                label: "Review path policy".to_string(),
                command: None,
            }],
            non_actions: Vec::new(),
        };

        let snapshot = render_snapshot(&TuiModel::from_actions(&output), 88, 12);

        assert!(snapshot.contains("[note]"));
        assert!(snapshot.contains("Command: No command attached."));
        assert!(snapshot.contains("Enter unavailable"));
    }
}
