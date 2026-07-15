use std::{collections::BTreeSet, error::Error, fmt};

use bowline_core::{
    commands::StatusCommandOutput,
    events::EventName,
    status::{StatusItemKind, StatusSubjectKind},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotificationPayload {
    pub title: String,
    pub body: String,
    pub action: Option<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct NotificationDedupe {
    seen: BTreeSet<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct NotificationDispatchReport {
    pub attempted: usize,
    pub sent: usize,
    pub skipped: usize,
    pub failures: Vec<NotificationDispatchFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotificationDispatchFailure {
    pub title: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotificationSendError {
    Unavailable(String),
    Failed(String),
}

pub trait NotificationSender {
    fn send(&self, payload: &NotificationPayload) -> Result<(), NotificationSendError>;
}

pub struct DesktopNotificationSender;

pub fn pending_device_payloads(status: &StatusCommandOutput) -> Vec<NotificationPayload> {
    status
        .items
        .iter()
        .filter(|item| {
            item.kind == StatusItemKind::Device
                && (item.event_name.as_ref() == Some(&EventName::DeviceApprovalRequested)
                    || item.subject.as_ref().is_some_and(|subject| {
                        subject.kind == StatusSubjectKind::DeviceApprovalRequest
                    }))
        })
        .map(|item| NotificationPayload {
            title: "bowline device approval".to_string(),
            body: item.summary.clone(),
            // The concrete approve affordance rides on `device_approvals`,
            // correlated to this device-approval item by `request_id`.
            action: item.subject.as_ref().and_then(|subject| {
                status
                    .device_approvals
                    .iter()
                    .find(|affordance| affordance.request_id == subject.id)
                    .map(|affordance| affordance.approve_command.clone())
            }),
        })
        .collect()
}

pub fn dispatch_new_notifications<S>(
    payloads: &[NotificationPayload],
    dedupe: &mut NotificationDedupe,
    sender: &S,
) -> NotificationDispatchReport
where
    S: NotificationSender,
{
    dispatch_new_notifications_with_checkpoint(payloads, dedupe, sender, || true)
}

pub fn dispatch_new_notifications_with_checkpoint<S>(
    payloads: &[NotificationPayload],
    dedupe: &mut NotificationDedupe,
    sender: &S,
    mut checkpoint: impl FnMut() -> bool,
) -> NotificationDispatchReport
where
    S: NotificationSender,
{
    let mut report = NotificationDispatchReport::default();
    for payload in payloads {
        if !checkpoint() {
            break;
        }
        report.attempted += 1;
        let key = payload_dedupe_key(payload);
        if dedupe.seen.contains(&key) {
            report.skipped += 1;
            continue;
        }
        match sender.send(payload) {
            Ok(()) => {
                dedupe.seen.insert(key);
                report.sent += 1;
            }
            Err(error) => report.failures.push(NotificationDispatchFailure {
                title: payload.title.clone(),
                message: error.to_string(),
            }),
        }
    }
    report
}

fn payload_dedupe_key(payload: &NotificationPayload) -> String {
    payload
        .action
        .clone()
        .unwrap_or_else(|| format!("{}|{}", payload.title, payload.body))
}

impl NotificationSender for DesktopNotificationSender {
    fn send(&self, payload: &NotificationPayload) -> Result<(), NotificationSendError> {
        send_desktop_notification(payload)
    }
}

#[cfg(target_os = "linux")]
fn send_desktop_notification(payload: &NotificationPayload) -> Result<(), NotificationSendError> {
    let mut body = payload.body.clone();
    if let Some(action) = &payload.action {
        body.push('\n');
        body.push_str(action);
    }
    notify_rust::Notification::new()
        .appname("bowline")
        .summary(&payload.title)
        .body(&body)
        .show()
        .map(|_| ())
        .map_err(|error| NotificationSendError::Failed(error.to_string()))
}

#[cfg(not(target_os = "linux"))]
fn send_desktop_notification(_payload: &NotificationPayload) -> Result<(), NotificationSendError> {
    Err(NotificationSendError::Unavailable(
        "desktop notifications are available only on Linux".to_string(),
    ))
}

impl fmt::Display for NotificationSendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(message) | Self::Failed(message) => formatter.write_str(message),
        }
    }
}

impl Error for NotificationSendError {}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use bowline_core::commands::StatusCommandOutput;

    use super::{
        NotificationDedupe, NotificationPayload, NotificationSendError, NotificationSender,
        dispatch_new_notifications, pending_device_payloads,
    };

    struct RecordingSender {
        sent: RefCell<Vec<NotificationPayload>>,
    }

    impl RecordingSender {
        fn new() -> Self {
            Self {
                sent: RefCell::new(Vec::new()),
            }
        }
    }

    impl NotificationSender for RecordingSender {
        fn send(&self, payload: &NotificationPayload) -> Result<(), NotificationSendError> {
            self.sent.borrow_mut().push(payload.clone());
            Ok(())
        }
    }

    struct FailingSender;

    impl NotificationSender for FailingSender {
        fn send(&self, _payload: &NotificationPayload) -> Result<(), NotificationSendError> {
            Err(NotificationSendError::Unavailable(
                "notification server unavailable".to_string(),
            ))
        }
    }

    #[test]
    fn pending_device_notifications_mirror_status_without_secret_values() {
        let status: StatusCommandOutput = serde_json::from_str(include_str!(
            "../../../../tests/contracts/status/pending-device.json"
        ))
        .expect("pending device status parses");

        let payloads = pending_device_payloads(&status);

        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0].title, "bowline device approval");
        assert!(payloads[0].body.contains("Dev-Mac"));
        // The approve command is sourced from the concrete device-approval
        // affordance (`device_approvals`), correlated by request id.
        assert_eq!(
            payloads[0].action.as_deref(),
            Some("bowline device approve --root ~/Code --code '<redacted>'")
        );
        assert!(!format!("{payloads:?}").contains("secret"));
    }

    #[test]
    fn pending_device_notifications_bind_actions_to_matching_request_ids() {
        let mut status: StatusCommandOutput = serde_json::from_str(include_str!(
            "../../../../tests/contracts/status/pending-device.json"
        ))
        .expect("pending device status parses");

        status.items[0].subject = Some(bowline_core::status::StatusSubject {
            kind: bowline_core::status::StatusSubjectKind::DeviceApprovalRequest,
            id: "device-request:ws_code:dev-mac".to_string(),
            path: None,
        });
        let mut second = status.items[0].clone();
        second.summary =
            "Linux-Vivobook requested approval with matching code 89ab-cdef.".to_string();
        second.device_id = Some(bowline_core::ids::DeviceId::new("dev_linux_vivobook"));
        second.subject = Some(bowline_core::status::StatusSubject {
            kind: bowline_core::status::StatusSubjectKind::DeviceApprovalRequest,
            id: "device-request:ws_code:linux-vivobook".to_string(),
            path: None,
        });
        status.items.push(second);
        status
            .device_approvals
            .push(bowline_core::status::DeviceApprovalAffordance {
                request_id: "device-request:ws_code:linux-vivobook".to_string(),
                device_name: "Linux-Vivobook".to_string(),
                code: "<redacted>".to_string(),
                approve_command:
                    "bowline device approve --root ~/Code --code '<redacted-vivobook>'".to_string(),
            });

        let payloads = pending_device_payloads(&status);

        assert_eq!(payloads.len(), 2);
        // dev-mac correlates to the fixture affordance; linux-vivobook to the
        // one just pushed — both by request id.
        assert_eq!(
            payloads[0].action.as_deref(),
            Some("bowline device approve --root ~/Code --code '<redacted>'")
        );
        assert_eq!(
            payloads[1].action.as_deref(),
            Some("bowline device approve --root ~/Code --code '<redacted-vivobook>'")
        );
    }

    #[test]
    fn dispatcher_sends_each_pending_action_once() {
        let payload = NotificationPayload {
            title: "bowline device approval".to_string(),
            body: "Dev-Mac requested approval.".to_string(),
            action: Some("bowline device approve --root ~/Code --code 0123-4567".to_string()),
        };
        let sender = RecordingSender::new();
        let mut dedupe = NotificationDedupe::default();

        let first =
            dispatch_new_notifications(std::slice::from_ref(&payload), &mut dedupe, &sender);
        let second = dispatch_new_notifications(&[payload], &mut dedupe, &sender);

        assert_eq!(first.sent, 1);
        assert_eq!(first.skipped, 0);
        assert_eq!(second.sent, 0);
        assert_eq!(second.skipped, 1);
        assert_eq!(sender.sent.borrow().len(), 1);
    }

    #[test]
    fn dispatcher_retries_failed_delivery_attempts() {
        let payload = NotificationPayload {
            title: "bowline device approval".to_string(),
            body: "Dev-Mac requested approval.".to_string(),
            action: Some("bowline device approve --root ~/Code --code 0123-4567".to_string()),
        };
        let mut dedupe = NotificationDedupe::default();

        let first =
            dispatch_new_notifications(std::slice::from_ref(&payload), &mut dedupe, &FailingSender);
        let second = dispatch_new_notifications(&[payload], &mut dedupe, &FailingSender);

        assert_eq!(first.sent, 0);
        assert_eq!(first.failures.len(), 1);
        assert_eq!(second.failures.len(), 1);
        assert_eq!(second.skipped, 0);
    }
}
