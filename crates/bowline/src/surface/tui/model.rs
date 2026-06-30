use bowline_core::{commands::ActionsCommandOutput, status::StatusLevel};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TuiAction {
    pub label: String,
    pub command: Option<String>,
    pub mutates: bool,
}

impl TuiAction {
    pub fn is_runnable(&self) -> bool {
        self.command.is_some()
    }

    pub fn effect_label(&self) -> &'static str {
        if !self.is_runnable() {
            "guidance only"
        } else if self.mutates {
            "changes workspace state"
        } else {
            "inspect only"
        }
    }

    pub fn confirmation_label(&self) -> &'static str {
        if !self.is_runnable() {
            "No command attached"
        } else if self.mutates {
            "Enter asks for confirmation"
        } else {
            "Enter runs immediately"
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuiTone {
    Healthy,
    Attention,
    Limited,
}

impl TuiTone {
    pub fn label(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Attention => "attention",
            Self::Limited => "limited",
        }
    }

    pub fn from_status_label(level: &str) -> Self {
        match level {
            "healthy" => Self::Healthy,
            "limited" => Self::Limited,
            _ => Self::Attention,
        }
    }
}

impl From<StatusLevel> for TuiTone {
    fn from(level: StatusLevel) -> Self {
        match level {
            StatusLevel::Healthy => Self::Healthy,
            StatusLevel::Attention => Self::Attention,
            StatusLevel::Limited => Self::Limited,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TuiModel {
    pub title: String,
    pub status: String,
    pub tone: TuiTone,
    pub details: Vec<String>,
    pub actions: Vec<TuiAction>,
    pub selected: usize,
    pub confirming: Option<usize>,
}

impl TuiModel {
    pub fn from_actions(output: &ActionsCommandOutput) -> Self {
        let actions = output
            .actions
            .iter()
            .map(|action| TuiAction {
                label: action.label.clone(),
                command: action.command.clone(),
                mutates: action.effect_category().requires_confirmation(),
            })
            .collect::<Vec<_>>();
        let details = if actions.is_empty() {
            let mut details = output.non_actions.clone();
            if details.is_empty() {
                details.extend(output.status.attention_items.clone());
            }
            details
        } else {
            Vec::new()
        };
        let tone = TuiTone::from(output.status.level);
        Self {
            title: "bowline".to_string(),
            status: tone.label().to_string(),
            tone,
            details,
            actions,
            selected: 0,
            confirming: None,
        }
    }

    pub fn from_resolve(
        summary: String,
        tone: TuiTone,
        actions: Vec<TuiAction>,
        details: Vec<String>,
    ) -> Self {
        Self {
            title: "bowline resolve".to_string(),
            status: summary,
            tone,
            details,
            actions,
            selected: 0,
            confirming: None,
        }
    }

    pub fn selected_action(&self) -> Option<&TuiAction> {
        self.actions.get(self.selected)
    }

    pub fn confirmed_action(&self) -> Option<&TuiAction> {
        self.confirming
            .and_then(|index| self.actions.get(index))
            .or_else(|| self.selected_action())
    }

    pub fn move_down(&mut self) {
        if !self.actions.is_empty() {
            self.selected = (self.selected + 1).min(self.actions.len() - 1);
        }
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn move_first(&mut self) {
        self.selected = 0;
    }

    pub fn move_last(&mut self) {
        if !self.actions.is_empty() {
            self.selected = self.actions.len() - 1;
        }
    }

    pub fn request_confirmation(&mut self) {
        if self
            .selected_action()
            .map(|action| action.is_runnable() && action.mutates)
            .unwrap_or(false)
        {
            self.confirming = Some(self.selected);
        }
    }

    pub fn cancel_confirmation(&mut self) {
        self.confirming = None;
    }
}

#[cfg(test)]
fn mutates(command: Option<&str>) -> bool {
    command
        .map(|command| {
            [
                "bowline approve",
                "bowline revoke",
                "bowline recover create",
                "bowline recover verify",
                "bowline recover rotate",
                "bowline recover revoke",
                "bowline recover use",
                "bowline setup",
                "bowline accept",
                "bowline discard",
                "bowline restore",
                "bowline cleanup --apply",
                "bowline agent publish",
                "bowline agent complete",
                " --accept ",
                " --reject ",
            ]
            .iter()
            .any(|needle| command.contains(needle))
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use bowline_core::{
        commands::{ActionsCommandOutput, CONTRACT_VERSION, CommandName},
        status::{SafeAction, StatusLevel, StatusScope, WorkspaceStatus},
    };

    use super::{TuiModel, TuiTone, mutates};

    fn actions_output(level: StatusLevel) -> ActionsCommandOutput {
        ActionsCommandOutput {
            contract_version: CONTRACT_VERSION,
            command: CommandName::Actions,
            generated_at: "2026-06-28T12:00:00Z".to_string(),
            workspace_id: None,
            project_id: None,
            scope: Some(StatusScope::Project),
            status: WorkspaceStatus {
                level,
                attention_items: Vec::new(),
            },
            actions: vec![SafeAction {
                label: "Inspect status".to_string(),
                command: Some("bowline status".to_string()),
            }],
            non_actions: Vec::new(),
        }
    }

    #[test]
    fn mutating_trust_and_recovery_commands_require_confirmation() {
        for command in [
            "bowline approve req_1",
            "bowline revoke dev_1",
            "bowline recover create",
            "bowline recover verify rk_1",
            "bowline recover rotate",
            "bowline recover revoke rk_1",
            "bowline recover use rk_1",
            "bowline setup",
            "bowline accept auth-fix",
            "bowline discard auth-fix",
            "bowline restore auth-fix",
            "bowline cleanup --apply",
            "bowline agent publish --lease lease_1",
            "bowline agent complete --lease lease_1",
            "bowline resolve ~/Code/app --accept conflict_1",
            "bowline resolve ~/Code/app --reject conflict_1",
        ] {
            assert!(mutates(Some(command)), "{command}");
        }

        assert!(!mutates(Some("bowline status")));
        assert!(!mutates(Some("bowline recover status")));
    }

    #[test]
    fn model_preserves_status_tone_for_rendering() {
        assert_eq!(
            TuiModel::from_actions(&actions_output(StatusLevel::Healthy)).tone,
            TuiTone::Healthy
        );
        assert_eq!(
            TuiModel::from_actions(&actions_output(StatusLevel::Attention)).tone,
            TuiTone::Attention
        );
        assert_eq!(
            TuiModel::from_actions(&actions_output(StatusLevel::Limited)).tone,
            TuiTone::Limited
        );
    }

    #[test]
    fn tone_maps_resolve_status_labels() {
        assert_eq!(TuiTone::from_status_label("healthy"), TuiTone::Healthy);
        assert_eq!(TuiTone::from_status_label("attention"), TuiTone::Attention);
        assert_eq!(TuiTone::from_status_label("limited"), TuiTone::Limited);
        assert_eq!(TuiTone::from_status_label("unknown"), TuiTone::Attention);

        assert_eq!(
            TuiModel::from_resolve(
                "no unresolved conflict bundles found".to_string(),
                TuiTone::Healthy,
                Vec::new(),
                Vec::new(),
            )
            .tone,
            TuiTone::Healthy
        );
    }

    #[test]
    fn action_detail_labels_match_confirmation_behavior() {
        let inspect = super::TuiAction {
            label: "Inspect status".to_string(),
            command: Some("bowline status".to_string()),
            mutates: false,
        };
        let change = super::TuiAction {
            label: "Approve device".to_string(),
            command: Some("bowline approve req_1".to_string()),
            mutates: true,
        };

        assert_eq!(inspect.effect_label(), "inspect only");
        assert_eq!(inspect.confirmation_label(), "Enter runs immediately");
        assert_eq!(change.effect_label(), "changes workspace state");
        assert_eq!(change.confirmation_label(), "Enter asks for confirmation");

        let note = super::TuiAction {
            label: "Review path policy".to_string(),
            command: None,
            mutates: false,
        };

        assert!(!note.is_runnable());
        assert_eq!(note.effect_label(), "guidance only");
        assert_eq!(note.confirmation_label(), "No command attached");
    }
}
