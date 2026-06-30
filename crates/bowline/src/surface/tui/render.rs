use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
};

use super::model::{TuiAction, TuiModel, TuiTone};

pub fn render(frame: &mut Frame<'_>, model: &TuiModel) {
    let area = frame.area();
    let footer_height = if model.confirming.is_some() { 3 } else { 1 };
    let header_height = if area.height <= 10 { 3 } else { 4 };
    let detail_height = if model.actions.is_empty() {
        0
    } else if model.confirming.is_some() && area.height <= 12 {
        2
    } else if area.height >= 14 {
        5
    } else if area.height <= 12 && footer_height == 1 {
        3
    } else {
        4
    };
    let action_height = area
        .height
        .saturating_sub(header_height + detail_height + footer_height)
        .max(3);
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
        let help = if selected_is_note && width < 60 {
            "q quit  j/k move  Home/End jump"
        } else if selected_is_note {
            "q quit  Esc quit  up/down or j/k move  Home/End jump  Enter unavailable"
        } else if width < 60 {
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
        TuiTone::Attention => Color::Yellow,
        TuiTone::Limited => Color::Red,
    };
    Style::default().fg(color)
}

fn status_hint(tone: TuiTone) -> &'static str {
    match tone {
        TuiTone::Healthy => "Nothing is blocking the current workspace.",
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
