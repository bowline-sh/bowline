use bowline_core::{
    commands::StatusCommandOutput,
    status::{StatusAttention, StatusAvailability, StatusLevel},
};

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
    Preparing,
    Attention,
    Limited,
}

impl TuiTone {
    #[cfg(test)]
    pub fn from_status_label(level: &str) -> Self {
        StatusLevel::from_status_label(level)
            .map(Self::from)
            .unwrap_or_else(|| {
                // Unknown labels should look actionable instead of falsely healthy.
                Self::Attention
            })
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

impl From<crate::surface::style::Verdict> for TuiTone {
    fn from(verdict: crate::surface::style::Verdict) -> Self {
        use crate::surface::style::Verdict;
        match verdict {
            Verdict::Ready => Self::Healthy,
            Verdict::Preparing => Self::Preparing,
            Verdict::NeedsYou => Self::Attention,
            Verdict::Limited => Self::Limited,
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
    canonical_axes: bool,
}

impl TuiModel {
    pub fn from_status(output: &StatusCommandOutput) -> Self {
        let mut actions = output
            .next_actions
            .iter()
            .map(|action| TuiAction {
                label: action.label.clone(),
                command: action.command.clone(),
                // Producer-set; never re-derived from the command string.
                mutates: action.mutates,
            })
            .collect::<Vec<_>>();
        // Pending device-approval affordances are concrete local trust material;
        // the TUI is a trusted local surface, so it renders them as runnable
        // (state-changing) approve actions.
        for approval in &output.device_approvals {
            actions.push(TuiAction {
                label: format!("Approve device (code {})", approval.code),
                command: Some(approval.approve_command.clone()),
                mutates: true,
            });
        }
        let details = if actions.is_empty() {
            let mut details = output.status.attention_items.clone();
            if details.is_empty()
                && output.status_summary.presentation_level() == StatusLevel::Healthy
            {
                details.push("Nothing needs action right now.".to_string());
            }
            details
        } else {
            Vec::new()
        };
        let tone = TuiTone::from(output.status_summary.presentation_level());
        let canonical_status = format!(
            "{} · {}",
            capability_label(output.status_summary.availability),
            action_label(output.status_summary.attention)
        );
        Self {
            title: "bowline".to_string(),
            status: canonical_status,
            tone,
            details,
            actions,
            selected: 0,
            confirming: None,
            canonical_axes: true,
        }
    }

    /// Override the tone/label with a richer verdict (adds the calm Preparing
    /// state that a bare status level cannot express).
    pub fn with_verdict(mut self, verdict: crate::surface::style::Verdict) -> Self {
        self.tone = TuiTone::from(verdict);
        if !self.canonical_axes {
            self.status = verdict.word().to_lowercase();
        }
        self
    }

    #[cfg(test)]
    pub fn from_parts(
        summary: String,
        tone: TuiTone,
        actions: Vec<TuiAction>,
        details: Vec<String>,
    ) -> Self {
        Self {
            title: "bowline".to_string(),
            status: summary,
            tone,
            details,
            actions,
            selected: 0,
            confirming: None,
            canonical_axes: false,
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

fn capability_label(availability: StatusAvailability) -> &'static str {
    match availability {
        StatusAvailability::Ready => "capability ready",
        StatusAvailability::Degraded => "capability degraded",
        StatusAvailability::Unavailable => "capability unavailable",
    }
}

fn action_label(attention: StatusAttention) -> &'static str {
    match attention {
        StatusAttention::None => "no action requested",
        StatusAttention::Recommended => "action recommended",
        StatusAttention::Required => "action required",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnboardingStep {
    AccountLogin,
    RootChoice,
    LocalReadiness,
    ConnectHost,
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnboardingResult {
    pub root: Option<String>,
    pub connect_command: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnboardingModel {
    pub step: OnboardingStep,
    pub root_input: String,
    pub host_input: String,
    default_root: String,
}

impl OnboardingModel {
    pub fn new(default_root: String) -> Self {
        Self {
            step: OnboardingStep::AccountLogin,
            root_input: default_root.clone(),
            host_input: String::new(),
            default_root,
        }
    }

    pub fn title(&self) -> &'static str {
        "bowline setup"
    }

    pub fn active_label(&self) -> &'static str {
        match self.step {
            OnboardingStep::AccountLogin => "Account",
            OnboardingStep::RootChoice => "Root",
            OnboardingStep::LocalReadiness => "Readiness",
            OnboardingStep::ConnectHost => "Host",
            OnboardingStep::Done => "Done",
        }
    }

    pub fn active_value(&self) -> Option<&str> {
        match self.step {
            OnboardingStep::RootChoice => Some(&self.root_input),
            OnboardingStep::ConnectHost => Some(&self.host_input),
            _ => None,
        }
    }

    pub fn push_char(&mut self, ch: char) {
        match self.step {
            OnboardingStep::RootChoice => self.root_input.push(ch),
            OnboardingStep::ConnectHost => self.host_input.push(ch),
            _ => {}
        }
    }

    pub fn pop_char(&mut self) {
        match self.step {
            OnboardingStep::RootChoice => {
                self.root_input.pop();
            }
            OnboardingStep::ConnectHost => {
                self.host_input.pop();
            }
            _ => {}
        }
    }

    pub fn advance(&mut self) {
        self.step = match self.step {
            OnboardingStep::AccountLogin => OnboardingStep::RootChoice,
            OnboardingStep::RootChoice => OnboardingStep::LocalReadiness,
            OnboardingStep::LocalReadiness => OnboardingStep::ConnectHost,
            OnboardingStep::ConnectHost | OnboardingStep::Done => OnboardingStep::Done,
        };
    }

    pub fn result(&self) -> OnboardingResult {
        let root = self.selected_root();
        let host = self.host_input.trim();
        OnboardingResult {
            root: root.clone(),
            connect_command: (!host.is_empty()).then(|| {
                format!(
                    "bowline connect {} --root {}",
                    bowline_core::shell::quote_word(host),
                    crate::io_helpers::shell_word(root.as_deref().unwrap_or("~/Code"))
                )
            }),
        }
    }

    fn trimmed_root(&self) -> Option<String> {
        let root = self.root_input.trim();
        (!root.is_empty()).then(|| root.to_string())
    }

    fn selected_root(&self) -> Option<String> {
        self.trimmed_root().or_else(|| {
            let root = self.default_root.trim();
            (!root.is_empty()).then(|| root.to_string())
        })
    }
}

#[cfg(test)]
mod tests {
    use bowline_core::{
        commands::{CONTRACT_VERSION, CommandName, StatusCommandOutput},
        status::{
            EventWatermarks, FreshnessVerdict, RepairCommand, StatusAttention, StatusFact,
            StatusFactAvailabilityImpact, StatusFactScope, StatusLevel, StatusScope,
            WorkspaceStatus, reduce_status_facts,
        },
    };

    use super::{TuiModel, TuiTone};

    fn status_summary(level: StatusLevel) -> bowline_core::status::StatusSummary {
        let facts = match level {
            StatusLevel::Healthy => Vec::new(),
            StatusLevel::Attention => vec![
                StatusFact::new(
                    "test-attention",
                    "status.aggregate_input",
                    "status-reducer",
                    StatusFactScope::Workspace,
                    "2026-06-28T12:00:00Z",
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
                    "2026-06-28T12:00:00Z",
                    "test limited",
                )
                .with_impacts(
                    StatusFactAvailabilityImpact::Degraded,
                    StatusAttention::None,
                ),
            ],
        };
        reduce_status_facts(facts, 1, "2026-06-28T12:00:00Z")
    }

    fn status_output(level: StatusLevel) -> StatusCommandOutput {
        StatusCommandOutput {
            contract_version: CONTRACT_VERSION,
            command: CommandName::Status,
            generated_at: "2026-06-28T12:00:00Z".to_string(),
            workspace_id: bowline_core::ids::WorkspaceId::new("ws_code"),
            project_id: None,
            scope: Some(StatusScope::Project),
            requested_path: None,
            resolved_workspace_root: Some("~/Code".to_string()),
            resolved_project_root: None,
            workspace_summary: None,
            setup_readiness: None,
            sync_queue: None,
            convergence: None,
            freshness: FreshnessVerdict::Unknown,
            stale_bases: Vec::new(),
            status: WorkspaceStatus {
                level,
                attention_items: Vec::new(),
            },
            status_summary: status_summary(level),
            items: Vec::new(),
            limits: Vec::new(),
            event_watermarks: EventWatermarks {
                last_scan_at: None,
                last_event_id: None,
                event_lag_ms: None,
            },
            next_actions: vec![RepairCommand::inspect(
                "Inspect status".to_string(),
                Some("bowline status --root ~/Code".to_string()),
            )],
            device_approvals: Vec::new(),
            service: None,
            authentication: None,
            sync: None,
        }
    }

    #[test]
    fn model_reads_producer_set_mutation_flag() {
        let mut output = status_output(StatusLevel::Attention);
        output.next_actions = vec![
            RepairCommand::inspect("Inspect status", Some("bowline status".to_string())),
            RepairCommand::mutating(
                "Resolve conflicts",
                Some("bowline resolve ~/Code".to_string()),
            ),
        ];
        let model = TuiModel::from_status(&output);
        assert!(!model.actions[0].mutates);
        assert!(model.actions[1].mutates);
    }

    #[test]
    fn canonical_axes_survive_richer_verdict_tone() {
        let mut output = status_output(StatusLevel::Attention);
        output.status_summary = reduce_status_facts(
            [StatusFact::new(
                "mixed",
                "status.aggregate_input",
                "status-reducer",
                StatusFactScope::Workspace,
                output.generated_at.clone(),
                "mixed",
            )
            .with_impacts(
                StatusFactAvailabilityImpact::Degraded,
                StatusAttention::Required,
            )],
            7,
            output.generated_at.clone(),
        );

        let model =
            TuiModel::from_status(&output).with_verdict(crate::surface::style::Verdict::NeedsYou);

        assert_eq!(model.status, "capability degraded · action required");
    }

    #[test]
    fn model_preserves_status_tone_for_rendering() {
        assert_eq!(
            TuiModel::from_status(&status_output(StatusLevel::Healthy)).tone,
            TuiTone::Healthy
        );
        assert_eq!(
            TuiModel::from_status(&status_output(StatusLevel::Attention)).tone,
            TuiTone::Attention
        );
        assert_eq!(
            TuiModel::from_status(&status_output(StatusLevel::Limited)).tone,
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
            TuiModel::from_parts(
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
            command: Some("bowline status --root ~/Code".to_string()),
            mutates: false,
        };
        let change = super::TuiAction {
            label: "Approve device".to_string(),
            command: Some("bowline device approve --root ~/Code --request req_1".to_string()),
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
