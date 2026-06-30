use crossterm::event::{KeyCode, KeyEvent};

use super::model::TuiModel;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputOutcome {
    Continue,
    Quit,
    Confirmed,
}

pub fn apply_key(model: &mut TuiModel, key: KeyEvent) -> InputOutcome {
    match key.code {
        KeyCode::Char('q') => InputOutcome::Quit,
        KeyCode::Esc => {
            if model.confirming.is_some() {
                model.cancel_confirmation();
                InputOutcome::Continue
            } else {
                InputOutcome::Quit
            }
        }
        KeyCode::Down | KeyCode::Char('j') if model.confirming.is_some() => InputOutcome::Continue,
        KeyCode::Down | KeyCode::Char('j') => {
            model.move_down();
            InputOutcome::Continue
        }
        KeyCode::Up | KeyCode::Char('k') if model.confirming.is_some() => InputOutcome::Continue,
        KeyCode::Up | KeyCode::Char('k') => {
            model.move_up();
            InputOutcome::Continue
        }
        KeyCode::Home if model.confirming.is_some() => InputOutcome::Continue,
        KeyCode::Home => {
            model.move_first();
            InputOutcome::Continue
        }
        KeyCode::End if model.confirming.is_some() => InputOutcome::Continue,
        KeyCode::End => {
            model.move_last();
            InputOutcome::Continue
        }
        KeyCode::Enter if model.confirming.is_some() => InputOutcome::Confirmed,
        KeyCode::Enter => {
            let Some(action) = model.selected_action() else {
                return InputOutcome::Continue;
            };
            if !action.is_runnable() {
                return InputOutcome::Continue;
            }
            if action.mutates {
                model.request_confirmation();
                return InputOutcome::Continue;
            }
            InputOutcome::Confirmed
        }
        _ => InputOutcome::Continue,
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::{InputOutcome, apply_key};
    use crate::surface::tui::{TuiAction, TuiModel, TuiTone};

    fn enter() -> KeyEvent {
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn non_mutating_action_confirms_immediately() {
        let mut model = TuiModel::from_resolve(
            "Attention".to_string(),
            TuiTone::Attention,
            vec![TuiAction {
                label: "Inspect status".to_string(),
                command: Some("bowline status".to_string()),
                mutates: false,
            }],
            Vec::new(),
        );

        assert_eq!(apply_key(&mut model, enter()), InputOutcome::Confirmed);
        assert_eq!(model.confirming, None);
    }

    #[test]
    fn mutating_action_requires_second_enter() {
        let mut model = TuiModel::from_resolve(
            "Attention".to_string(),
            TuiTone::Attention,
            vec![TuiAction {
                label: "Approve device".to_string(),
                command: Some("bowline approve req_1".to_string()),
                mutates: true,
            }],
            Vec::new(),
        );

        assert_eq!(apply_key(&mut model, enter()), InputOutcome::Continue);
        assert_eq!(model.confirming, Some(0));
        assert_eq!(apply_key(&mut model, enter()), InputOutcome::Confirmed);
    }

    #[test]
    fn action_without_command_cannot_be_confirmed() {
        let mut model = TuiModel::from_resolve(
            "Attention".to_string(),
            TuiTone::Attention,
            vec![TuiAction {
                label: "Review path policy".to_string(),
                command: None,
                mutates: false,
            }],
            Vec::new(),
        );

        assert_eq!(apply_key(&mut model, enter()), InputOutcome::Continue);
        assert_eq!(model.confirming, None);
    }

    #[test]
    fn enter_without_actions_stays_in_tui() {
        let mut model = TuiModel::from_resolve(
            "Healthy".to_string(),
            TuiTone::Healthy,
            Vec::new(),
            Vec::new(),
        );

        assert_eq!(apply_key(&mut model, enter()), InputOutcome::Continue);
    }

    #[test]
    fn confirmation_mode_ignores_selection_moves() {
        let mut model = TuiModel::from_resolve(
            "Attention".to_string(),
            TuiTone::Attention,
            vec![
                TuiAction {
                    label: "Approve device".to_string(),
                    command: Some("bowline approve req_1".to_string()),
                    mutates: true,
                },
                TuiAction {
                    label: "Revoke device".to_string(),
                    command: Some("bowline revoke req_1".to_string()),
                    mutates: true,
                },
            ],
            Vec::new(),
        );

        assert_eq!(apply_key(&mut model, enter()), InputOutcome::Continue);
        assert_eq!(
            apply_key(&mut model, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),),
            InputOutcome::Continue
        );
        assert_eq!(model.selected, 0);
        assert_eq!(apply_key(&mut model, enter()), InputOutcome::Confirmed);
        assert_eq!(
            model
                .confirmed_action()
                .and_then(|action| action.command.as_deref()),
            Some("bowline approve req_1")
        );
    }

    #[test]
    fn home_and_end_jump_between_action_boundaries() {
        let mut model = TuiModel::from_resolve(
            "Attention".to_string(),
            TuiTone::Attention,
            vec![
                TuiAction {
                    label: "First".to_string(),
                    command: Some("bowline status".to_string()),
                    mutates: false,
                },
                TuiAction {
                    label: "Second".to_string(),
                    command: Some("bowline explain".to_string()),
                    mutates: false,
                },
                TuiAction {
                    label: "Third".to_string(),
                    command: Some("bowline events".to_string()),
                    mutates: false,
                },
            ],
            Vec::new(),
        );

        assert_eq!(
            apply_key(&mut model, key(KeyCode::End)),
            InputOutcome::Continue
        );
        assert_eq!(model.selected, 2);
        assert_eq!(
            apply_key(&mut model, key(KeyCode::Home)),
            InputOutcome::Continue
        );
        assert_eq!(model.selected, 0);
    }

    #[test]
    fn home_and_end_are_noops_without_actions() {
        let mut model = TuiModel::from_resolve(
            "Healthy".to_string(),
            TuiTone::Healthy,
            Vec::new(),
            Vec::new(),
        );

        assert_eq!(
            apply_key(&mut model, key(KeyCode::End)),
            InputOutcome::Continue
        );
        assert_eq!(model.selected, 0);
        assert_eq!(
            apply_key(&mut model, key(KeyCode::Home)),
            InputOutcome::Continue
        );
        assert_eq!(model.selected, 0);
    }
}
