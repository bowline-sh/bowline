use std::io;
use std::time::Duration;

use crossterm::event::{self, Event};
#[cfg(test)]
use ratatui::{Terminal, backend::TestBackend};

use super::{
    input::{InputOutcome, OnboardingInputOutcome, apply_key, apply_onboarding_key},
    model::{OnboardingModel, OnboardingResult, TuiModel},
    render,
    terminal::TerminalSession,
};

const POLL_INTERVAL: Duration = Duration::from_millis(250);

pub fn run_app(mut model: TuiModel) -> io::Result<Option<String>> {
    let mut session = TerminalSession::enter()?;
    loop {
        session
            .terminal_mut()
            .draw(|frame| render::render(frame, &model))?;
        if !event::poll(POLL_INTERVAL)? {
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

pub fn run_onboarding_app(mut model: OnboardingModel) -> io::Result<Option<OnboardingResult>> {
    let mut session = TerminalSession::enter()?;
    loop {
        session
            .terminal_mut()
            .draw(|frame| render::render_onboarding(frame, &model))?;
        if !event::poll(POLL_INTERVAL)? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        match apply_onboarding_key(&mut model, key) {
            OnboardingInputOutcome::Continue => {}
            OnboardingInputOutcome::Quit => return Ok(None),
            OnboardingInputOutcome::Done => return Ok(Some(model.result())),
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
pub fn render_onboarding_snapshot(model: &OnboardingModel, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal should initialize");
    terminal
        .draw(|frame| render::render_onboarding(frame, model))
        .expect("test terminal should draw");
    terminal.backend().to_string()
}

#[cfg(test)]
mod tests {
    use bowline_core::{
        commands::{CONTRACT_VERSION, CommandName, StatusCommandOutput},
        ids::WorkspaceId,
        status::{
            DeviceApprovalAffordance, EventWatermarks, FreshnessVerdict, RepairCommand,
            StatusAttention, StatusFact, StatusFactAvailabilityImpact, StatusFactScope,
            StatusLevel, StatusScope, WorkspaceStatus, reduce_status_facts,
        },
    };

    use super::{OnboardingModel, TuiModel, render_onboarding_snapshot, render_snapshot};

    fn status_summary(level: StatusLevel) -> bowline_core::status::StatusSummary {
        let facts = match level {
            StatusLevel::Healthy => Vec::new(),
            StatusLevel::Attention => vec![
                StatusFact::new(
                    "test-attention",
                    "status.aggregate_input",
                    "status-reducer",
                    StatusFactScope::Workspace,
                    "2026-06-25T12:00:00Z",
                    "test attention",
                )
                .with_impacts(
                    StatusFactAvailabilityImpact::None,
                    StatusAttention::Required,
                ),
            ],
            StatusLevel::Limited => vec![
                StatusFact::new(
                    "test-limited",
                    "status.aggregate_input",
                    "status-reducer",
                    StatusFactScope::Workspace,
                    "2026-06-25T12:00:00Z",
                    "test limited",
                )
                .with_impacts(
                    StatusFactAvailabilityImpact::Degraded,
                    StatusAttention::None,
                ),
            ],
        };
        reduce_status_facts(facts, 1, "2026-06-25T12:00:00Z")
    }

    fn status_output(
        level: StatusLevel,
        attention_items: Vec<String>,
        next_actions: Vec<RepairCommand>,
        device_approvals: Vec<DeviceApprovalAffordance>,
    ) -> StatusCommandOutput {
        StatusCommandOutput {
            contract_version: CONTRACT_VERSION,
            command: CommandName::Status,
            generated_at: "2026-06-25T12:00:00Z".to_string(),
            workspace_id: WorkspaceId::new("ws_code"),
            project_id: None,
            scope: Some(StatusScope::Project),
            requested_path: None,
            resolved_workspace_root: Some("~/Code".to_string()),
            workspace_summary: None,
            setup_readiness: None,
            sync_queue: None,
            freshness: FreshnessVerdict::Unknown,
            stale_bases: Vec::new(),
            status: WorkspaceStatus {
                level,
                attention_items,
            },
            status_summary: status_summary(level),
            items: Vec::new(),
            limits: Vec::new(),
            event_watermarks: EventWatermarks {
                last_scan_at: None,
                last_event_id: None,
                event_lag_ms: None,
                sync_state: None,
                watcher_state: None,
                network_state: None,
            },
            next_actions,
            device_approvals,
        }
    }

    #[test]
    fn snapshot_renders_actions_in_small_terminal() {
        let output = status_output(
            StatusLevel::Attention,
            vec!["Conflict needs resolution.".to_string()],
            vec![RepairCommand::mutating(
                "Resolve conflict".to_string(),
                Some("bowline resolve ~/Code/app".to_string()),
            )],
            Vec::new(),
        );
        let snapshot = render_snapshot(&TuiModel::from_status(&output), 48, 10);

        assert!(snapshot.contains("bowline"));
        assert!(snapshot.contains("Resolve conflict"));
        assert!(snapshot.contains("Selected"));
        assert!(snapshot.contains("Command: bowline resolve ~/Code/app"));
        assert!(snapshot.contains("Home/End jump"));
    }

    #[test]
    fn snapshot_renders_pending_device_approval() {
        let output = status_output(
            StatusLevel::Attention,
            vec!["Dev-Mac is waiting for approval.".to_string()],
            vec![RepairCommand::inspect(
                "Inspect status".to_string(),
                Some("bowline status --root ~/Code".to_string()),
            )],
            vec![DeviceApprovalAffordance {
                request_id: "device-request:dev-mac".to_string(),
                device_name: "Dev-Mac".to_string(),
                code: "42-31".to_string(),
                approve_command: "bowline device approve --root ~/Code --code 42-31".to_string(),
            }],
        );
        // The approve affordance is appended after next_actions; select it so the
        // detail panel shows its concrete command.
        let mut model = TuiModel::from_status(&output);
        model.move_last();
        let snapshot = render_snapshot(&model, 72, 16);

        assert!(snapshot.contains("Approve device (code 42-31)"));
        assert!(snapshot.contains("Inspect status"));
        assert!(snapshot.contains("[changes]"));
        assert!(snapshot.contains("A decision or repair path needs attention."));
        assert!(
            snapshot.contains("Command: bowline device approve --root ~/Code --code 42-31"),
            "\n{snapshot}"
        );
    }

    #[test]
    fn snapshot_renders_degraded_and_recovery_actions() {
        let output = status_output(
            StatusLevel::Limited,
            vec![
                "Sync is degraded.".to_string(),
                "Recovery Key needs verification.".to_string(),
            ],
            vec![
                RepairCommand::inspect(
                    "Inspect sync".to_string(),
                    Some("bowline status --root ~/Code".to_string()),
                ),
                RepairCommand::mutating(
                    "Verify Recovery Key".to_string(),
                    Some("bowline recover verify rk_demo".to_string()),
                ),
            ],
            Vec::new(),
        );
        let snapshot = render_snapshot(&TuiModel::from_status(&output), 72, 12);

        assert!(snapshot.contains("Inspect sync"));
        assert!(snapshot.contains("Verify Recovery Key"));
    }

    #[test]
    fn snapshot_renders_no_action_state_without_empty_actions_panel() {
        let output = status_output(StatusLevel::Healthy, Vec::new(), Vec::new(), Vec::new());
        let snapshot = render_snapshot(&TuiModel::from_status(&output), 72, 10);

        assert!(snapshot.contains("CAPABILITY READY · NO ACTION"));
        assert!(snapshot.contains("State"));
        assert!(snapshot.contains("Nothing needs action right now."));
        assert!(!snapshot.contains("Actions"));
    }

    #[test]
    fn snapshot_renders_attention_details_without_actions() {
        let output = status_output(
            StatusLevel::Attention,
            vec!["Sync needs a fresh observation.".to_string()],
            Vec::new(),
            Vec::new(),
        );
        let snapshot = render_snapshot(&TuiModel::from_status(&output), 72, 10);

        assert!(snapshot.contains("CAPABILITY READY · ACTION REQUIRED"));
        assert!(snapshot.contains("Sync needs a fresh observation."));
        assert!(!snapshot.contains("Nothing needs action right now."));
    }

    #[test]
    fn snapshot_renders_confirmation_footer() {
        let output = status_output(
            StatusLevel::Attention,
            vec!["Dev-Mac is waiting for approval.".to_string()],
            Vec::new(),
            vec![DeviceApprovalAffordance {
                request_id: "device-request:dev-mac".to_string(),
                device_name: "Dev-Mac".to_string(),
                code: "42-31".to_string(),
                approve_command: "bowline device approve --root ~/Code --code 42-31".to_string(),
            }],
        );
        let mut model = TuiModel::from_status(&output);
        model.request_confirmation();

        let snapshot = render_snapshot(&model, 72, 12);

        assert!(snapshot.contains("Confirm"));
        assert!(snapshot.contains("Command: bowline device approve --root ~/Code --code 42-31"));
        assert!(snapshot.contains("Enter runs the selected command."));
        assert!(snapshot.contains("Esc cancels."));
    }

    #[test]
    fn snapshot_keeps_confirmation_detail_bound_to_confirmed_action() {
        let output = status_output(
            StatusLevel::Attention,
            vec!["Device trust needs a decision.".to_string()],
            vec![RepairCommand::mutating(
                "Revoke Dev-Mac".to_string(),
                Some(
                    "bowline device revoke --root ~/Code --device device-request:dev-mac"
                        .to_string(),
                ),
            )],
            vec![DeviceApprovalAffordance {
                request_id: "device-request:dev-mac".to_string(),
                device_name: "Dev-Mac".to_string(),
                code: "42-31".to_string(),
                approve_command: "bowline device approve --root ~/Code --code 42-31".to_string(),
            }],
        );
        let mut model = TuiModel::from_status(&output);
        // The approve affordance is appended after next_actions, so it is the
        // last selectable action; select it, then confirm.
        model.move_last();
        model.request_confirmation();
        model.move_up();

        let snapshot = render_snapshot(&model, 96, 14);

        assert!(snapshot.contains("Command: bowline device approve --root ~/Code --code 42-31"));
        assert!(!snapshot.contains(
            "Command: bowline device revoke --root ~/Code --device device-request:dev-mac"
        ));
    }

    #[test]
    fn snapshot_renders_no_command_action_as_note() {
        let output = status_output(
            StatusLevel::Attention,
            vec!["Path policy needs review.".to_string()],
            vec![RepairCommand::inspect(
                "Review path policy".to_string(),
                None,
            )],
            Vec::new(),
        );

        let snapshot = render_snapshot(&TuiModel::from_status(&output), 88, 12);

        assert!(snapshot.contains("[note]"));
        assert!(snapshot.contains("Command: No command attached."));
        assert!(snapshot.contains("Enter unavailable"));
    }

    #[test]
    fn snapshot_renders_first_onboarding_screen() {
        let model = OnboardingModel::new("~/Code".to_string());
        let snapshot = render_onboarding_snapshot(&model, 72, 14);

        assert!(snapshot.contains("bowline setup"));
        assert!(snapshot.contains("Account login"));
        assert!(snapshot.contains("Root"));
        assert!(snapshot.contains("~/Code"));
    }
}
