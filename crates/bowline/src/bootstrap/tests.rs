use super::*;

use bowline_control_plane::FakeControlPlaneClient;
use bowline_core::{devices::DeviceTrustState, status::RepairCommand};
use bowline_local::fakes::FakeKeychain;

#[test]
fn remote_sync_ready_requires_healthy_without_attention() {
    assert!(remote_sync_is_ready(&WorkspaceStatus::healthy()));
    assert!(!remote_sync_is_ready(&WorkspaceStatus {
        level: StatusLevel::Attention,
        attention_items: Vec::new(),
    }));
    assert!(!remote_sync_is_ready(&WorkspaceStatus {
        level: StatusLevel::Healthy,
        attention_items: vec!["device trust has not settled".to_string()],
    }));
    assert!(!remote_sync_is_ready(&WorkspaceStatus {
        level: StatusLevel::Limited,
        attention_items: vec!["remote daemon unavailable".to_string()],
    }));
}

#[test]
fn remote_daemon_sync_ready_requires_matching_local_and_remote_heads() {
    let ready = r#"{
          "daemon": {"state": "running"},
          "sync": {
            "state": "idle",
            "lastOutcome": "no-changes",
            "localHead": {"workspaceId": "ws", "snapshotId": "snap", "version": 3},
            "remoteHead": {"workspaceId": "ws", "snapshotId": "snap", "version": 3}
          }
        }"#;
    let stale = r#"{
          "daemon": {"state": "running"},
          "sync": {
            "state": "idle",
            "lastOutcome": "no-changes",
            "localHead": {"workspaceId": "ws", "snapshotId": "snap-new", "version": 4},
            "remoteHead": {"workspaceId": "ws", "snapshotId": "snap-old", "version": 3}
          }
        }"#;
    let just_advanced = r#"{
          "daemon": {"state": "running"},
          "sync": {
            "state": "idle",
            "lastOutcome": "advanced",
            "localHead": {"workspaceId": "ws", "snapshotId": "snap", "version": 3},
            "remoteHead": {"workspaceId": "ws", "snapshotId": "snap", "version": 3}
          }
        }"#;

    assert!(remote_daemon_sync_is_ready(ready));
    assert!(remote_daemon_sync_is_ready(just_advanced));
    assert!(!remote_daemon_sync_is_ready(stale));
    assert!(!remote_daemon_sync_is_ready(
        r#"{"daemon":{"state":"running"}}"#
    ));
}

#[test]
fn bootstrap_root_unexpands_local_home_for_remote_hosts() {
    assert_eq!(
        normalize_remote_root_for_home("/workspace/user/Code", "/workspace/user"),
        "~/Code"
    );
    assert_eq!(
        normalize_remote_root_for_home("/srv/Code", "/workspace/user"),
        "/srv/Code"
    );
    assert_eq!(
        normalize_remote_root_for_home("/srv/Code", ""),
        "/srv/Code",
        "empty HOME must not rewrite absolute roots to ~/…"
    );
}

#[test]
fn bootstrap_output_marks_sync_blocked_when_bootstrap_did_not_complete() {
    let output = bootstrap_output(
        BootstrapOutputBase {
            host: "linux-box".to_string(),
            root: "~/Code".to_string(),
            local_root: Some("~/Code".to_string()),
            generated_at: "2026-06-24T12:00:00Z".to_string(),
            steps: vec![step(
                BootstrapStepName::Install,
                BootstrapStepState::Blocked,
                "install failed",
            )],
            remote_status_items: Vec::new(),
        },
        None,
        None,
        false,
        None,
    );

    assert_eq!(output.sync, BootstrapSyncState::Blocked);
    assert_eq!(output.next_required_phase, None);
    assert!(output.remote_status.needs_attention());
    assert_eq!(
        output.repair_actions,
        vec![RepairCommand::mutating(
            "Retry remote bootstrap",
            Some("bowline connect linux-box --root '~/Code' --json".to_string())
        )]
    );
}

#[test]
fn bootstrap_output_keeps_trust_separate_from_sync_status() {
    let output = bootstrap_output(
        BootstrapOutputBase {
            host: "linux-box".to_string(),
            root: "~/Code".to_string(),
            local_root: Some("~/Code".to_string()),
            generated_at: "2026-06-24T12:00:00Z".to_string(),
            steps: vec![step(
                BootstrapStepName::Sync,
                BootstrapStepState::Blocked,
                "daemon unavailable",
            )],
            remote_status_items: Vec::new(),
        },
        None,
        None,
        true,
        Some(WorkspaceStatus {
            level: StatusLevel::Limited,
            attention_items: vec!["daemon unavailable".to_string()],
        }),
    );

    assert!(output.trusted);
    assert_eq!(output.sync, BootstrapSyncState::Blocked);
    assert_eq!(output.next_required_phase, None);
    assert!(output.repair_actions.iter().any(|action| {
        action.label == "Inspect remote daemon status"
            && action.command.as_deref()
                == Some(ssh_command("linux-box", "bowline daemon status --json").as_str())
    }));
    assert!(output.repair_actions.iter().any(|action| {
        action.label == "Inspect remote status"
            && action.command.as_deref()
                == Some(ssh_command("linux-box", "bowline status --root ~/Code --json").as_str())
    }));
}

#[test]
fn daemon_bootstrap_recovery_installs_the_managed_service() {
    let base = BootstrapOutputBase {
        host: "linux-box".to_string(),
        root: "~/Code".to_string(),
        local_root: Some("~/Code".to_string()),
        generated_at: "2026-07-23T21:00:00Z".to_string(),
        steps: Vec::new(),
        remote_status_items: Vec::new(),
    };

    let actions = blocked_repair_actions(&base, BootstrapStepName::DaemonStart);

    assert!(actions.iter().any(|action| {
        action.label == "Install remote daemon service"
            && action.command.as_deref()
                == Some(ssh_command("linux-box", "bowline daemon install --json").as_str())
    }));
    assert!(
        actions.iter().all(|action| !action
            .command
            .as_deref()
            .is_some_and(|command| { command.contains("bowline daemon start") })),
        "bootstrap recovery must never create an unmanaged remote daemon"
    );
}

#[test]
fn bootstrap_output_ready_surfaces_inspect_without_agent_launch() {
    let output = bootstrap_output(
        BootstrapOutputBase {
            host: "linux-box".to_string(),
            root: "~/Code".to_string(),
            local_root: Some("~/Code".to_string()),
            generated_at: "2026-06-24T12:00:00Z".to_string(),
            steps: vec![step(
                BootstrapStepName::Sync,
                BootstrapStepState::Completed,
                "sync ready",
            )],
            remote_status_items: Vec::new(),
        },
        None,
        None,
        true,
        Some(WorkspaceStatus::healthy()),
    );

    assert_eq!(output.sync, BootstrapSyncState::Ready);
    assert!(output.repair_actions.iter().any(|action| {
        action.label == "Inspect remote status"
            && action.command.as_deref()
                == Some(ssh_command("linux-box", "bowline status --root ~/Code --json").as_str())
    }));
    // Bootstrap no longer emits agent-launch actions; the host materializes the
    // workspace and the agent runtime drives the work.
    assert!(
        !output
            .repair_actions
            .iter()
            .any(|action| action.label.to_lowercase().contains("agent"))
    );
}

#[test]
fn bootstrap_output_returns_local_approval_recovery_action() {
    let output = bootstrap_output(
        BootstrapOutputBase {
            host: "linux box".to_string(),
            root: "/workspace/user/Code Projects".to_string(),
            local_root: Some("~/Code".to_string()),
            generated_at: "2026-06-24T12:00:00Z".to_string(),
            steps: vec![step(
                BootstrapStepName::Approve,
                BootstrapStepState::Blocked,
                "key store locked",
            )],
            remote_status_items: Vec::new(),
        },
        None,
        None,
        false,
        None,
    );

    assert_eq!(output.sync, BootstrapSyncState::Blocked);
    assert!(output.repair_actions.iter().any(|action| {
        action.label == "Inspect local device requests"
            && action.command.as_deref() == Some("bowline status --root ~/Code --json")
    }));
    assert!(output.repair_actions.iter().any(|action| {
        action.label == "Retry remote bootstrap"
            && action.command.as_deref()
                == Some("bowline connect 'linux box' --root '/workspace/user/Code Projects' --json")
    }));
}

#[test]
fn remote_path_arg_preserves_remote_tilde_expansion() {
    assert_eq!(remote_path_arg("~/Code"), "~/Code");
    assert_eq!(remote_path_arg("~/Code Projects"), "~/'Code Projects'");
    assert_eq!(
        remote_path_arg("/workspace/user/Code Projects"),
        "'/workspace/user/Code Projects'"
    );
}

#[test]
fn remote_bootstrap_pins_sanitized_device_id() {
    let env = remote_bootstrap_env("linux-box");

    assert!(env.iter().any(|(key, _)| key == "BOWLINE_DEVICE_NAME"));
    assert!(
        env.iter()
            .any(|(key, value)| key == "BOWLINE_DEVICE_ID" && value == "device_linux_box")
    );
    assert!(env.iter().any(
            |(key, value)| key == "BOWLINE_DEVICE_NAME" && value == "bowline-remote-linux_box"
        ));
}

#[test]
fn remote_rebootstrap_device_id_uses_fresh_suffix() {
    assert_eq!(remote_device_id("mac-mini.local"), "device_mac_mini_local");
    assert_ne!(
        remote_rebootstrap_device_id("mac-mini.local", "first"),
        remote_rebootstrap_device_id("mac-mini.local", "second")
    );
    assert!(
        remote_rebootstrap_device_id("mac-mini.local", "first")
            .starts_with("device_mac_mini_local_")
    );
}

#[test]
fn remote_bootstrap_secrets_require_durable_account_session() {
    let without_any_durable_auth = remote_bootstrap_secret_env_from(None, None);
    assert!(remote_bootstrap_auth_error(&without_any_durable_auth));

    let with_session = remote_bootstrap_secret_env_from(
        Some(runtime::AccountSessionRevocation {
            session_id: "bowline-session".to_string(),
            revocation_token: "bowline-revoke".to_string(),
        }),
        None,
    );
    assert!(!remote_bootstrap_auth_error(&with_session));
    assert!(with_session.contains(&(
        "BOWLINE_ACCOUNT_SESSION_ID".to_string(),
        "bowline-session".to_string()
    )));
    assert!(with_session.contains(&(
        "BOWLINE_ACCOUNT_SESSION_REVOCATION_TOKEN".to_string(),
        "bowline-revoke".to_string()
    )));
    assert!(
        !with_session
            .iter()
            .any(|(key, _)| key == "BOWLINE_WORKOS_ACCESS_TOKEN")
    );

    let with_control = remote_bootstrap_secret_env_from(
        Some(runtime::AccountSessionRevocation {
            session_id: "bowline-session".to_string(),
            revocation_token: "bowline-revoke".to_string(),
        }),
        Some("durable-control".to_string()),
    );

    assert!(with_control.contains(&(
        "BOWLINE_ACCOUNT_SESSION_ID".to_string(),
        "bowline-session".to_string()
    )));
    assert!(with_control.contains(&(
        "BOWLINE_CONTROL_PLANE_TOKEN".to_string(),
        "durable-control".to_string()
    )));
    assert!(
        !with_control
            .iter()
            .any(|(key, _)| key == "BOWLINE_WORKOS_REFRESH_TOKEN")
    );
    assert!(!remote_bootstrap_auth_error(&with_control));
}

#[test]
fn remote_device_trust_requires_exact_authorized_device() {
    let control_plane = FakeControlPlaneClient::default();
    let workspace_id = bowline_core::ids::WorkspaceId::new("ws_bootstrap_trust");
    control_plane.create_workspace(workspace_id.as_str());
    let trusted_keychain = FakeKeychain::default();
    bowline_local::trust::ensure_first_device_trust_root(
        &control_plane,
        &trusted_keychain,
        workspace_id.clone(),
        DeviceId::new("trusted-device"),
        "Trusted Mac",
        bowline_core::devices::DevicePlatform::Macos,
        "2026-06-24T12:00:00Z",
    )
    .expect("first trusted device");

    let remote_keychain = FakeKeychain::default();
    let request = bowline_local::trust::create_device_request(
        &control_plane,
        &remote_keychain,
        bowline_local::trust::DeviceRequestOptions {
            workspace_id: workspace_id.clone(),
            device_id: DeviceId::new("remote-device"),
            device_name: "Linux Server".to_string(),
            platform: bowline_core::devices::DevicePlatform::Linux,
            host: Some("linux-server".to_string()),
            root: Some("~/Code".to_string()),
            runtime: None,
            generated_at: "2026-06-24T12:00:00Z".to_string(),
        },
    )
    .expect("request created");

    let before_accept = verify_remote_device_trust(&control_plane, &request)
        .expect_err("pending request is not trusted yet");
    assert!(before_accept.contains("not authorized"));

    bowline_local::trust::approve_device_request(
        &control_plane,
        &trusted_keychain,
        bowline_local::trust::ApproveDeviceOptions {
            workspace_id: workspace_id.clone(),
            request_id: request.request_id.clone(),
            approver_device_id: DeviceId::new("trusted-device"),
            generated_at: "2026-06-24T12:00:01Z".to_string(),
        },
    )
    .expect("request approved");
    bowline_local::trust::accept_device_grant(
        &control_plane,
        &remote_keychain,
        &workspace_id,
        &request.request_id,
        &request.requester_device_id,
    )
    .expect("grant accepted");

    let verified =
        verify_remote_device_trust(&control_plane, &request).expect("remote device trusted");
    assert_eq!(verified.id.as_str(), "remote-device");
    assert_eq!(verified.trust_state, DeviceTrustState::Trusted);
}
