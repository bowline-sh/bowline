use bowline_core::commands::{ActionsCommandOutput, WatchFrame};
use bowline_core::status::SafeAction;

pub fn render_actions(output: &ActionsCommandOutput) -> String {
    let mut lines = vec![format!("Actions: {:?}", output.status.level)];
    if output.actions.is_empty() {
        lines.extend(output.non_actions.iter().map(|item| format!("  {item}")));
    } else {
        append_actions(&mut lines, &output.actions);
    }
    lines.push(String::new());
    lines.join("\n")
}

pub fn render_watch_frame(frame: &WatchFrame) -> String {
    match frame {
        WatchFrame::Status {
            sequence, status, ..
        } => format!(
            "#{sequence} status {:?}: {}\n",
            status.status.level,
            status
                .status
                .attention_items
                .first()
                .map(String::as_str)
                .unwrap_or("no attention needed")
        ),
        WatchFrame::Event {
            sequence, event, ..
        } => format!("#{sequence} event {:?}: {}\n", event.name, event.summary),
        WatchFrame::Error {
            sequence, error, ..
        } => format!(
            "#{sequence} error {}: {}\n",
            error.error.code, error.error.message
        ),
    }
}

fn append_actions(lines: &mut Vec<String>, actions: &[SafeAction]) {
    for action in actions {
        match &action.command {
            Some(command) => lines.push(format!("  {}: {command}", action.label)),
            None => lines.push(format!("  {}", action.label)),
        }
    }
}
