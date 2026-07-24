use super::*;

pub(super) fn base_status_item(kind: StatusItemKind, summary: &str) -> StatusItem {
    StatusItem {
        kind,
        summary: summary.to_string(),
        subject: None,
        path: None,
        classification: None,
        mode: None,
        access: Vec::new(),
        event_id: None,
        event_name: None,
        device_id: None,
        lease_id: None,
        project_id: None,
        snapshot_id: None,
        policy_version: None,
        env_record_id: None,
    }
}

pub(crate) fn event_name_label(name: &EventName) -> String {
    serde_json::to_value(name)
        .ok()
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| format!("{name:?}"))
}

pub(super) fn shell_word(value: &str) -> String {
    if value == "~" {
        return "~".to_string();
    }
    if let Some(rest) = value.strip_prefix("~/") {
        if rest.is_empty() {
            return "~/".to_string();
        }
        return format!("~/{}", bowline_core::shell::quote_word(rest));
    }
    bowline_core::shell::quote_word(value)
}
