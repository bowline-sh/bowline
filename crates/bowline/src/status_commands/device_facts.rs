//! Device-trust status facts: reduce a device-trust snapshot onto the local
//! device's status items, attention text, approval affordances, and repair
//! actions for the human and machine `status` surfaces.

use super::*;
use bowline_core::devices::display_matching_code;

pub(crate) fn apply_device_status(output: &mut StatusCommandOutput, trust: &DeviceTrustSnapshot) {
    let local_device_id = runtime::daemon_device_id(&output.workspace_id);
    apply_device_status_for_local_device(output, trust, &local_device_id);
}

pub(crate) fn apply_device_status_for_local_device(
    output: &mut StatusCommandOutput,
    trust: &DeviceTrustSnapshot,
    local_device_id: &DeviceId,
) {
    let local_id = local_device_id.as_str();
    if let Some(revoked) = trust
        .revoked_devices
        .iter()
        .find(|device| device.device_id == local_id)
    {
        append_status_fact(
            output,
            "device.revoked",
            format!("device-revoked:{local_id}"),
            format!("device-trust:{local_id}"),
            StatusFactScope::Device,
            Some(local_id),
            None,
        );
        output.status.attention_items.push(format!(
            "This device was revoked from workspace {}.",
            output.workspace_id.as_str()
        ));
        let item = device_status_item(
            output,
            StatusSubjectKind::Device,
            revoked.device_id.as_str(),
            Some(DeviceId::new(revoked.device_id.clone())),
            format!(
                "This device is revoked; future sync and trust operations are blocked. Reason: {}",
                revoked.reason
            ),
        );
        output.items.push(item);
        output.next_actions.push(RepairCommand::inspect(
            "Inspect workspace status".to_string(),
            Some(status_command(output, &[])),
        ));
        return;
    }

    if let Some(device) = trust
        .authorized_devices
        .iter()
        .find(|device| device.device_id == local_id)
    {
        let item = device_status_item(
            output,
            StatusSubjectKind::Device,
            device.device_id.as_str(),
            Some(DeviceId::new(device.device_id.clone())),
            trusted_device_summary(device.device_id.as_str(), device.device_name.as_str()),
        );
        output.items.push(item);
    } else if let Some(request) = trust
        .pending_requests
        .iter()
        .find(|request| request.device_id == local_id)
    {
        append_status_fact(
            output,
            "device.untrusted",
            format!("device-pending:{local_id}"),
            format!("device-trust:{local_id}"),
            StatusFactScope::Device,
            Some(local_id),
            None,
        );
        output
            .status
            .attention_items
            .push("This device is waiting for approval before it can sync.".to_string());
        let item = device_status_item(
            output,
            StatusSubjectKind::DeviceApprovalRequest,
            request.request_id.as_str(),
            Some(DeviceId::new(request.device_id.clone())),
            "This device has a pending approval request.".to_string(),
        );
        output.items.push(item);
    } else if !trust.authorized_devices.is_empty() {
        append_status_fact(
            output,
            "device.untrusted",
            format!("device-untrusted:{local_id}"),
            format!("device-trust:{local_id}"),
            StatusFactScope::Device,
            Some(local_id),
            None,
        );
        output
            .status
            .attention_items
            .push("This device is not trusted for the workspace yet.".to_string());
        let setup_command = format!(
            "bowline setup{}",
            io_helpers::root_flag(output.resolved_workspace_root.as_deref())
        );
        let item = device_status_item(
            output,
            StatusSubjectKind::Device,
            local_device_id.as_str(),
            Some(local_device_id.clone()),
            format!("Run `{setup_command}` to request workspace trust."),
        );
        output.items.push(item);
    }

    if !trust.pending_requests.is_empty() {
        for request in &trust.pending_requests {
            append_status_fact(
                output,
                "device.approval_requested",
                format!("device-approval:{}", request.request_id.as_str()),
                format!("device-approval:{}", request.request_id.as_str()),
                StatusFactScope::Device,
                Some(request.device_id.as_str()),
                Some(request.request_id.as_str()),
            );
        }
        output.status.attention_items.push(format!(
            "{} device approval request(s) are waiting.",
            trust.pending_requests.len()
        ));
        // Trusted local surface: the concrete approve affordance (matching code +
        // `bowline device approve --code …`) is local trust material. It rides on
        // `device_approvals`, correlated to its status item by `request_id`, and
        // must never be written to hosted/persisted status payloads.
        let pending_items = trust
            .pending_requests
            .iter()
            .map(|request| {
                let display_code = display_matching_code(&request.matching_code);
                output.device_approvals.push(DeviceApprovalAffordance {
                    request_id: request.request_id.as_str().to_string(),
                    device_name: request.device_name.clone(),
                    code: display_code.clone(),
                    approve_command: approve_command(
                        output,
                        io_helpers::shell_word(display_code.as_str()),
                    ),
                });
                device_status_item(
                    output,
                    StatusSubjectKind::DeviceApprovalRequest,
                    request.request_id.as_str(),
                    Some(DeviceId::new(request.device_id.clone())),
                    format!(
                        "{} is waiting for approval with matching code {}.",
                        request.device_name, display_code
                    ),
                )
            })
            .collect::<Vec<_>>();
        output.items.extend(pending_items);
        output.next_actions.push(RepairCommand::inspect(
            "Review workspace status".to_string(),
            Some(status_command(output, &[])),
        ));
    }
}

pub(crate) fn append_status_fact(
    output: &mut StatusCommandOutput,
    kind: &str,
    id: impl Into<String>,
    dedupe_key: impl Into<String>,
    scope: StatusFactScope,
    scope_id: Option<&str>,
    action_target_id: Option<&str>,
) {
    let policy = status_fact_policy(kind);
    let mut fact = StatusFact::new(
        id,
        kind,
        policy.authority,
        scope,
        output.generated_at.clone(),
        dedupe_key,
    );
    if let Some(scope_id) = scope_id {
        fact = fact.with_scope_id(scope_id);
    }
    if let (Some(action), Some(target_id)) = (fact.action.as_mut(), action_target_id) {
        action.target_id = Some(target_id.to_string());
    }
    let mut facts = std::mem::take(&mut output.status_summary.facts);
    facts.push(fact);
    let summary = reduce_status_facts(facts, 1, output.generated_at.clone());
    output.status.level = summary.presentation_level();
    output.status_summary = summary;
}

fn status_command(output: &StatusCommandOutput, extra: &[&str]) -> String {
    let mut command = format!(
        "bowline status{}",
        io_helpers::root_flag(output.resolved_workspace_root.as_deref())
    );
    for arg in extra {
        command.push(' ');
        command.push_str(arg);
    }
    command
}

fn approve_command(output: &StatusCommandOutput, code: String) -> String {
    format!(
        "bowline device approve{} --code {code}",
        io_helpers::root_flag(output.resolved_workspace_root.as_deref())
    )
}

fn trusted_device_summary(device_id: &str, device_name: &str) -> String {
    if device_name == device_id {
        return format!("This device is trusted as {device_id}.");
    }
    format!("This device is trusted as {device_id} ({device_name}).")
}

pub(crate) fn device_status_item(
    output: &StatusCommandOutput,
    subject_kind: StatusSubjectKind,
    subject_id: impl Into<String>,
    device_id: Option<DeviceId>,
    summary: String,
) -> StatusItem {
    StatusItem {
        kind: StatusItemKind::Device,
        summary,
        subject: Some(StatusSubject {
            kind: subject_kind,
            id: subject_id.into(),
            path: None,
        }),
        path: None,
        classification: None,
        mode: None,
        access: Vec::new(),
        event_id: None,
        event_name: None,
        device_id,
        lease_id: None,
        project_id: output.project_id.clone(),
        snapshot_id: None,
        policy_version: None,
        env_record_id: None,
    }
}
