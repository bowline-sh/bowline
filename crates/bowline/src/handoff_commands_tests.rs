use bowline_control_plane::{AuthorizedDeviceRecord, ControlPlaneTimestamp};
use bowline_core::{
    commands::{CONTRACT_VERSION, CommandName, DeviceCommandAction, DevicesCommandOutput},
    devices::{DeviceFingerprint, DevicePlatform, DeviceRecord, DeviceTrustState},
    ids::{DeviceId, WorkspaceId},
};

use crate::handoff_trust::remote_trust_error;

#[test]
fn remote_trust_requires_local_authorized_device_match() {
    let workspace_id = WorkspaceId::new("workspace_1");
    let output = devices_output(remote_device(
        "device_remote",
        "fingerprint_remote",
        &workspace_id,
        true,
        DeviceTrustState::Trusted,
    ));
    let local = vec![authorized_device(
        "device_remote",
        "fingerprint_remote",
        "workspace_1",
        false,
    )];

    assert_eq!(remote_trust_error(&output, &local, &workspace_id), None);
}

#[test]
fn remote_trust_rejects_self_report_without_local_fingerprint_match() {
    let workspace_id = WorkspaceId::new("workspace_1");
    let output = devices_output(remote_device(
        "device_remote",
        "fingerprint_remote",
        &workspace_id,
        true,
        DeviceTrustState::Trusted,
    ));
    let local = vec![authorized_device(
        "device_remote",
        "other_fingerprint",
        "workspace_1",
        false,
    )];

    assert_eq!(
        remote_trust_error(&output, &local, &workspace_id).as_deref(),
        Some("Target is not trusted for handoff.")
    );
}

#[test]
fn remote_trust_rejects_different_workspace() {
    let workspace_id = WorkspaceId::new("workspace_1");
    let output = devices_output(remote_device(
        "device_remote",
        "fingerprint_remote",
        &WorkspaceId::new("workspace_2"),
        true,
        DeviceTrustState::Trusted,
    ));
    let local = vec![authorized_device(
        "device_remote",
        "fingerprint_remote",
        "workspace_1",
        false,
    )];

    assert_eq!(
        remote_trust_error(&output, &local, &workspace_id).as_deref(),
        Some("Target belongs to a different Bowline workspace.")
    );
}

#[test]
fn remote_trust_rejects_revoked_local_record() {
    let workspace_id = WorkspaceId::new("workspace_1");
    let output = devices_output(remote_device(
        "device_remote",
        "fingerprint_remote",
        &workspace_id,
        true,
        DeviceTrustState::Trusted,
    ));
    let local = vec![authorized_device(
        "device_remote",
        "fingerprint_remote",
        "workspace_1",
        true,
    )];

    assert_eq!(
        remote_trust_error(&output, &local, &workspace_id).as_deref(),
        Some("Target is not trusted for handoff.")
    );
}

#[test]
fn remote_trust_rejects_missing_current_device() {
    let workspace_id = WorkspaceId::new("workspace_1");
    let output = devices_output(remote_device(
        "device_remote",
        "fingerprint_remote",
        &workspace_id,
        false,
        DeviceTrustState::Trusted,
    ));
    let local = vec![authorized_device(
        "device_remote",
        "fingerprint_remote",
        "workspace_1",
        false,
    )];

    assert_eq!(
        remote_trust_error(&output, &local, &workspace_id).as_deref(),
        Some("Target did not report a current Bowline device.")
    );
}

fn devices_output(device: DeviceRecord) -> DevicesCommandOutput {
    DevicesCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::Devices,
        generated_at: "2026-07-05T12:00:00Z".to_string(),
        action: DeviceCommandAction::List,
        workspace_id: Some(device.workspace_id.clone()),
        local_device: None,
        devices: vec![device],
        revoked_devices: Vec::new(),
        pending_requests: Vec::new(),
        created_request: None,
        approved_device: None,
        denied_request: None,
        revoked_device: None,
        recovery_key: None,
        next_actions: Vec::new(),
    }
}

fn remote_device(
    device_id: &str,
    fingerprint: &str,
    workspace_id: &WorkspaceId,
    current: bool,
    trust_state: DeviceTrustState,
) -> DeviceRecord {
    DeviceRecord {
        id: DeviceId::new(device_id),
        name: "remote".to_string(),
        workspace_id: workspace_id.clone(),
        platform: DevicePlatform::Linux,
        trust_state,
        device_fingerprint: DeviceFingerprint::new(fingerprint),
        authorized_at: Some("10".to_string()),
        updated_at: "10".to_string(),
        is_current_device: current,
        limitation_reason: None,
    }
}

fn authorized_device(
    device_id: &str,
    fingerprint: &str,
    workspace_id: &str,
    revoked: bool,
) -> AuthorizedDeviceRecord {
    AuthorizedDeviceRecord {
        workspace_id: bowline_core::ids::WorkspaceId::new(workspace_id),
        device_id: bowline_core::ids::DeviceId::new(device_id),
        device_name: "remote".to_string(),
        platform: "linux".to_string(),
        device_fingerprint: fingerprint.to_string(),
        authorized_at: ControlPlaneTimestamp { tick: 10 },
        authorized_by_device_id: Some(bowline_core::ids::DeviceId::new("device_admin")),
        device_authorization_proof_verifier: None,
        revoked_at: revoked.then_some(ControlPlaneTimestamp { tick: 20 }),
    }
}
