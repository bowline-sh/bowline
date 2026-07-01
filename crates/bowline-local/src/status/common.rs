use super::*;

pub(super) fn component_item(
    kind: StatusItemKind,
    summary: &str,
    event_name: EventName,
) -> StatusItem {
    let mut item = base_status_item(kind, summary);
    item.subject = Some(StatusSubject {
        kind: StatusSubjectKind::Component,
        id: format!("{kind:?}").to_ascii_lowercase(),
        path: None,
    });
    item.event_name = Some(event_name);
    item
}

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

pub(super) fn status_level_label(level: StatusLevel) -> &'static str {
    match level {
        StatusLevel::Healthy => "healthy",
        StatusLevel::Attention => "attention",
        StatusLevel::Limited => "limited",
    }
}

pub(super) fn event_name_label(name: EventName) -> String {
    serde_json::to_value(name)
        .ok()
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| format!("{name:?}"))
}
