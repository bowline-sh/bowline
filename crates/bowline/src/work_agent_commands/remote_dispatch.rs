use std::{error::Error, fmt};

#[derive(Debug)]
pub(super) enum DispatchTargetError {
    Runtime(String),
    AmbiguousDeviceName {
        target: String,
        device_ids: Vec<String>,
    },
}

impl DispatchTargetError {
    pub(super) fn requires_user_action(&self) -> bool {
        matches!(self, Self::AmbiguousDeviceName { .. })
    }
}

impl fmt::Display for DispatchTargetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Runtime(message) => formatter.write_str(message),
            Self::AmbiguousDeviceName { target, device_ids } => write!(
                formatter,
                "authorized device name `{target}` is ambiguous; use one of these device ids: {}",
                device_ids.join(", ")
            ),
        }
    }
}

impl Error for DispatchTargetError {}

impl From<String> for DispatchTargetError {
    fn from(message: String) -> Self {
        Self::Runtime(message)
    }
}

pub(super) fn compact_task_label(task: &str) -> String {
    let label = task.split_whitespace().collect::<Vec<_>>().join(" ");
    if label.is_empty() {
        "agent task".to_string()
    } else if label.len() <= 512 {
        label
    } else {
        label
            .char_indices()
            .take_while(|(index, char)| index + char.len_utf8() <= 512)
            .map(|(_, char)| char)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_task_label_collapses_multiline_tasks() {
        assert_eq!(
            compact_task_label("fix auth\nthen run tests\tcarefully"),
            "fix auth then run tests carefully"
        );
        assert_eq!(compact_task_label("\n\t "), "agent task");
        let emoji_label = compact_task_label(&"🚀".repeat(512));
        assert_eq!(emoji_label.len(), 512);
        assert_eq!(emoji_label.chars().count(), 128);
    }
}
