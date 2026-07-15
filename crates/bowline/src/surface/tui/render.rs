use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
};

use super::model::{OnboardingModel, OnboardingStep, TuiAction, TuiModel, TuiTone};

const CONFIRMING_FOOTER_HEIGHT: u16 = 3;
const STANDARD_FOOTER_HEIGHT: u16 = 1;
const COMPACT_HEIGHT: u16 = 10;
const TIGHT_HEIGHT: u16 = 12;
const ROOMY_HEIGHT: u16 = 14;
const COMPACT_HEADER_HEIGHT: u16 = 3;
const STANDARD_HEADER_HEIGHT: u16 = 4;
const NO_DETAIL_HEIGHT: u16 = 0;
const CONFIRMING_TIGHT_DETAIL_HEIGHT: u16 = 2;
const STANDARD_TIGHT_DETAIL_HEIGHT: u16 = 3;
const STANDARD_DETAIL_HEIGHT: u16 = 4;
const ROOMY_DETAIL_HEIGHT: u16 = 5;
const MIN_ACTION_HEIGHT: u16 = 3;
const NARROW_WIDTH: u16 = 60;

pub fn render(frame: &mut Frame<'_>, model: &TuiModel) {
    let area = frame.area();
    let footer_height = footer_height(model);
    let header_height = header_height(area.height);
    let detail_height = detail_height(model, area.height);
    let action_height = area
        .height
        .saturating_sub(header_height + detail_height + footer_height)
        .max(MIN_ACTION_HEIGHT);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_height),
            Constraint::Length(action_height),
            Constraint::Length(detail_height),
            Constraint::Length(footer_height),
        ])
        .split(area);

    let tone_style = tone_style(model.tone);
    frame.render_widget(
        Paragraph::new(header_text(model, header_height > 3, tone_style))
            .wrap(Wrap { trim: true })
            .block(
                Block::default()
                    .title(model.title.as_str())
                    .borders(Borders::ALL)
                    .border_style(tone_style),
            ),
        chunks[0],
    );

    let items = list_items(model);
    frame.render_widget(
        List::new(items).block(
            Block::default()
                .title(list_title(model))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        ),
        chunks[1],
    );

    if detail_height > 0 {
        frame.render_widget(
            Paragraph::new(action_detail(model, detail_height < 5))
                .wrap(Wrap { trim: true })
                .block(
                    Block::default()
                        .title("Selected")
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::DarkGray)),
                ),
            chunks[2],
        );
    }

    let footer = footer_text(model, area.width);
    frame.render_widget(
        Paragraph::new(footer).style(Style::default().fg(Color::Gray)),
        chunks[3],
    );
}

fn footer_height(model: &TuiModel) -> u16 {
    if model.confirming.is_some() {
        CONFIRMING_FOOTER_HEIGHT
    } else {
        STANDARD_FOOTER_HEIGHT
    }
}

fn header_height(height: u16) -> u16 {
    if height <= COMPACT_HEIGHT {
        COMPACT_HEADER_HEIGHT
    } else {
        STANDARD_HEADER_HEIGHT
    }
}

fn detail_height(model: &TuiModel, height: u16) -> u16 {
    if model.actions.is_empty() {
        return NO_DETAIL_HEIGHT;
    }
    // 10-line terminals lose the header hint, 12-line terminals compact
    // selected-action detail, and 14-line terminals can show the full detail box.
    match (model.confirming.is_some(), height) {
        (true, ..=TIGHT_HEIGHT) => CONFIRMING_TIGHT_DETAIL_HEIGHT,
        (_, ROOMY_HEIGHT..) => ROOMY_DETAIL_HEIGHT,
        (false, ..=TIGHT_HEIGHT) => STANDARD_TIGHT_DETAIL_HEIGHT,
        _ => STANDARD_DETAIL_HEIGHT,
    }
}

pub fn render_onboarding(frame: &mut Frame<'_>, model: &OnboardingModel) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Min(7),
            Constraint::Length(3),
        ])
        .split(area);

    frame.render_widget(
        Paragraph::new(onboarding_header(model))
            .wrap(Wrap { trim: true })
            .block(
                Block::default()
                    .title(model.title())
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan)),
            ),
        chunks[0],
    );

    frame.render_widget(
        List::new(onboarding_items(model)).block(
            Block::default()
                .title("Steps")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        ),
        chunks[1],
    );

    frame.render_widget(
        Paragraph::new(onboarding_footer(model)).style(Style::default().fg(Color::Gray)),
        chunks[2],
    );
}

fn onboarding_header(model: &OnboardingModel) -> Text<'_> {
    Text::from(vec![
        Line::from(vec![
            Span::raw("Step "),
            Span::styled(
                model.active_label(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(onboarding_hint(model.step)),
    ])
}

fn onboarding_hint(step: OnboardingStep) -> &'static str {
    match step {
        OnboardingStep::AccountLogin => {
            "Bowline will start account login, then prepare the workspace root."
        }
        OnboardingStep::RootChoice => "Choose the local root for synced code.",
        OnboardingStep::LocalReadiness => {
            "Bowline will inspect local readiness and safe next actions."
        }
        OnboardingStep::ConnectHost => {
            "Optionally enter another machine host to connect after setup."
        }
        OnboardingStep::Done => "Press Enter to run setup.",
    }
}

fn onboarding_items(model: &OnboardingModel) -> Vec<ListItem<'_>> {
    [
        (
            OnboardingStep::AccountLogin,
            "Account login",
            "existing browser/device flow",
        ),
        (
            OnboardingStep::RootChoice,
            "Root",
            model.root_input.as_str(),
        ),
        (
            OnboardingStep::LocalReadiness,
            "Local readiness",
            "status and next actions",
        ),
        (
            OnboardingStep::ConnectHost,
            "Connect host",
            if model.host_input.is_empty() {
                "skip"
            } else {
                model.host_input.as_str()
            },
        ),
        (OnboardingStep::Done, "Done", "run setup"),
    ]
    .into_iter()
    .map(|(step, label, value)| {
        let marker = if step == model.step { "> " } else { "  " };
        let mut item = ListItem::new(Line::from(vec![
            Span::raw(marker),
            Span::styled(label, Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("  "),
            Span::raw(value),
        ]));
        if step == model.step {
            item = item.style(selected_row_style());
        }
        item
    })
    .collect()
}

fn onboarding_footer(model: &OnboardingModel) -> Text<'_> {
    if let Some(value) = model.active_value() {
        return Text::from(vec![
            Line::from(vec![
                Span::styled("Editing: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(model.active_label()),
                Span::raw(" = "),
                Span::raw(value),
            ]),
            Line::from("Enter next  Backspace delete  Esc quit"),
        ]);
    }
    Text::from(Line::from("Enter next  Esc quit"))
}

fn header_text(model: &TuiModel, include_hint: bool, tone_style: Style) -> Text<'_> {
    let status = Line::from(vec![
        Span::raw("State "),
        Span::styled(
            model.status.to_uppercase(),
            tone_style.add_modifier(Modifier::BOLD),
        ),
    ]);
    if include_hint {
        Text::from(vec![status, Line::from(status_hint(model.tone))])
    } else {
        Text::from(status)
    }
}

fn list_title(model: &TuiModel) -> &'static str {
    if model.actions.is_empty() {
        "State"
    } else {
        "Actions"
    }
}

fn list_items(model: &TuiModel) -> Vec<ListItem<'_>> {
    if model.actions.is_empty() {
        return state_items(model);
    }
    model
        .actions
        .iter()
        .enumerate()
        .map(|(index, action)| action_item(action, index == model.selected))
        .collect()
}

fn state_items(model: &TuiModel) -> Vec<ListItem<'_>> {
    if model.details.is_empty() {
        return vec![ListItem::new(Line::from(empty_state_text(model.tone)))];
    }
    model
        .details
        .iter()
        .map(|detail| ListItem::new(Line::from(detail.as_str())))
        .collect()
}

fn empty_state_text(tone: TuiTone) -> &'static str {
    match tone {
        TuiTone::Healthy => "Nothing needs action right now.",
        TuiTone::Preparing => "Getting set up; nothing needs you yet.",
        TuiTone::Attention => "No safe action is available yet; inspect status for details.",
        TuiTone::Limited => "Some capabilities are unavailable; inspect status for details.",
    }
}

fn action_item(action: &TuiAction, selected: bool) -> ListItem<'_> {
    let marker = if selected { "> " } else { "  " };
    let (effect, effect_style) = action_badge(action);
    let mut item = ListItem::new(Line::from(vec![
        Span::raw(marker),
        Span::styled(effect, effect_style.add_modifier(Modifier::BOLD)),
        Span::raw(" "),
        Span::raw(action.label.as_str()),
    ]));
    if selected {
        item = item.style(selected_row_style());
    }
    item
}

fn action_badge(action: &TuiAction) -> (&'static str, Style) {
    if !action.is_runnable() {
        ("[note]", Style::default().fg(Color::Gray))
    } else if action.mutates {
        ("[changes]", Style::default().fg(Color::Yellow))
    } else {
        ("[view]", Style::default().fg(Color::Cyan))
    }
}

fn selected_row_style() -> Style {
    Style::default()
        .fg(Color::White)
        .bg(Color::DarkGray)
        .add_modifier(Modifier::BOLD)
}

fn footer_text(model: &TuiModel, width: u16) -> Text<'_> {
    if let Some(index) = model.confirming {
        let action = model.actions.get(index);
        let label = action
            .map(|action| action.label.as_str())
            .unwrap_or("action");
        let command = action
            .and_then(|action| action.command.as_deref())
            .unwrap_or("No command attached.");
        Text::from(vec![
            Line::from(vec![
                Span::styled("Confirm ", Style::default().fg(Color::Yellow)),
                Span::styled(
                    label.to_string(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                Span::styled("Command: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(command),
            ]),
            Line::from("Enter runs the selected command. Esc cancels."),
        ])
    } else {
        let selected_is_note = model
            .selected_action()
            .is_some_and(|action| !action.is_runnable());
        let help = if selected_is_note && width < NARROW_WIDTH {
            "q quit  j/k move  Home/End jump"
        } else if selected_is_note {
            "q quit  Esc quit  up/down or j/k move  Home/End jump  Enter unavailable"
        } else if width < NARROW_WIDTH {
            "q quit  j/k move  Home/End jump  Enter"
        } else {
            "q quit  Esc quit  up/down or j/k move  Home/End jump  Enter select"
        };
        Text::from(Line::from(help))
    }
}

fn tone_style(tone: TuiTone) -> Style {
    let color = match tone {
        TuiTone::Healthy => Color::Green,
        TuiTone::Preparing => Color::Cyan,
        TuiTone::Attention => Color::Yellow,
        TuiTone::Limited => Color::Red,
    };
    Style::default().fg(color)
}

fn status_hint(tone: TuiTone) -> &'static str {
    match tone {
        TuiTone::Healthy => "Nothing is blocking the current workspace.",
        TuiTone::Preparing => "Getting set up; nothing needs you.",
        TuiTone::Attention => "A decision or repair path needs attention.",
        TuiTone::Limited => "Some capabilities are unavailable; inspect the safe actions.",
    }
}

fn action_detail(model: &TuiModel, compact: bool) -> Text<'_> {
    let Some(action) = model.confirmed_action() else {
        return Text::from("No action selected.");
    };
    let command = action.command.as_deref().unwrap_or("No command attached.");
    let confirm = if model.confirming.is_some() {
        "Confirming this action."
    } else {
        action.confirmation_label()
    };
    if compact {
        return Text::from(Line::from(vec![
            Span::styled("Command: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(command),
            Span::raw(" | "),
            Span::styled("Effect: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(action.effect_label()),
            Span::raw(" - "),
            Span::raw(confirm),
        ]));
    }
    Text::from(vec![
        Line::from(vec![
            Span::styled("Action: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(action.label.as_str()),
        ]),
        Line::from(vec![
            Span::styled("Command: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(command),
        ]),
        Line::from(vec![
            Span::styled("Effect: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(action.effect_label()),
            Span::raw(" - "),
            Span::raw(confirm),
        ]),
    ])
}
