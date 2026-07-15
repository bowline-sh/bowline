use super::*;
use std::{cell::RefCell, rc::Rc};

use bowline_control_plane::{DeviceControlPlaneClient, FakeControlPlaneClient};
use bowline_core::{
    commands::{CONTRACT_VERSION, DeviceCommandAction, DevicesCommandOutput},
    devices::{DevicePlatform, DeviceTrustState, RecoveryKeyState},
    ids::{DeviceApprovalRequestId, WorkspaceId},
    status::RepairCommand,
};
use bowline_local::{
    bootstrap::{
        install::{RemoteBowlineInstall, RemotePlatform},
        process::{ProcessError, ProcessOutput, ProcessRunner},
    },
    fakes::FakeKeychain,
};

#[derive(Clone)]
struct FakeBootstrapRunner {
    control_plane: FakeControlPlaneClient,
    remote_keychain: FakeKeychain,
    workspace_id: WorkspaceId,
    request_id: Rc<RefCell<Option<DeviceApprovalRequestId>>>,
    remote_daemon_running: Rc<RefCell<bool>>,
}

impl ProcessRunner for FakeBootstrapRunner {
    fn run(&self, _program: &str, args: &[String]) -> Result<ProcessOutput, ProcessError> {
        self.run_with_stdin(_program, args, "")
    }

    fn run_with_stdin(
        &self,
        _program: &str,
        args: &[String],
        _stdin: &str,
    ) -> Result<ProcessOutput, ProcessError> {
        let command = args.last().cloned().unwrap_or_default();
        if command.contains("device request") && command.contains("--json") {
            let request = bowline_local::trust::create_device_request(
                &self.control_plane,
                &self.remote_keychain,
                bowline_local::trust::DeviceRequestOptions {
                    workspace_id: self.workspace_id.clone(),
                    device_id: DeviceId::new("remote-linux"),
                    device_name: "Remote Linux".to_string(),
                    platform: DevicePlatform::Linux,
                    host: Some("linux-box".to_string()),
                    lease_id: None,
                    root: Some("~/Code".to_string()),
                    runtime: None,
                    generated_at: "2026-06-26T12:00:00Z".to_string(),
                },
            )
            .expect("remote request");
            *self.request_id.borrow_mut() = Some(request.request_id.clone());
            return Ok(json_output(&DevicesCommandOutput {
                contract_version: CONTRACT_VERSION,
                command: bowline_core::commands::CommandName::Devices,
                generated_at: "2026-06-26T12:00:00Z".to_string(),
                action: DeviceCommandAction::Request,
                workspace_id: Some(self.workspace_id.clone()),
                local_device: None,
                devices: Vec::new(),
                revoked_devices: Vec::new(),
                pending_requests: vec![request.clone()],
                created_request: Some(request),
                approved_device: None,
                denied_request: None,
                revoked_device: None,
                recovery_key: Some(RecoveryKeyState::missing()),
                next_actions: Vec::new(),
            }));
        }
        if command.contains("device accept") {
            let request_id = self
                .request_id
                .borrow()
                .clone()
                .expect("request id exists before accept");
            bowline_local::trust::accept_device_grant(
                &self.control_plane,
                &self.remote_keychain,
                &self.workspace_id,
                &request_id,
                &DeviceId::new("remote-linux"),
            )
            .expect("remote accepts grant");
            return Ok(json_output(&serde_json::json!({"ok": true})));
        }
        if command.contains("daemon stop --json") {
            *self.remote_daemon_running.borrow_mut() = false;
            return Ok(json_output(
                &serde_json::json!({"daemon": {"state": "stopped"}}),
            ));
        }
        if command.contains("daemon start --json") {
            *self.remote_daemon_running.borrow_mut() = true;
            return Ok(json_output(
                &serde_json::json!({"daemon": {"state": "starting"}}),
            ));
        }
        if command.contains("daemon status --json") {
            let running = *self.remote_daemon_running.borrow();
            return Ok(json_output(&serde_json::json!({
                "daemon": {"state": if running { "running" } else { "stopped" }},
                "sync": {
                    "state": "idle",
                    "lastOutcome": "no-changes",
                    "localHead": {
                        "workspaceId": self.workspace_id.as_str(),
                        "snapshotId": "snap-ready",
                        "version": 1
                    },
                    "remoteHead": {
                        "workspaceId": self.workspace_id.as_str(),
                        "snapshotId": "snap-ready",
                        "version": 1
                    }
                }
            })));
        }
        if command.contains("setup --root")
            || command.contains("ln -sfn")
            || command.contains("daemon.env")
        {
            return Ok(json_output(&serde_json::json!({"ok": true})));
        }
        if command.contains("agent start") {
            return Ok(json_output(&serde_json::json!({
                "lease": {
                    "id": "lease-remote-codex",
                    "writeTargetMode": "direct",
                    "writeTargetPath": "~/Code/foo",
                    "outputTarget": {
                        "kind": "real-project",
                        "path": "~/Code/foo"
                    }
                }
            })));
        }
        if command.contains("codex exec") {
            return Ok(ProcessOutput {
                status_code: 0,
                stdout: "codex completed\n".to_string(),
                stderr: String::new(),
            });
        }
        if command.contains("agent complete --lease")
            && command.contains("lease-remote-codex")
            && command.contains("--json")
        {
            return Ok(json_output(&serde_json::json!({
                "requestId": "tool-complete",
                "leaseId": "lease-remote-codex",
                "tool": "complete-task",
                "outcome": "allowed",
                "summary": "task completed"
            })));
        }
        if command.contains("accept")
            && command.contains("work_view_remote_codex")
            && command.contains("--json")
        {
            return Ok(json_output(&serde_json::json!({
                "action": "accepted",
                "workView": {"id": "work_view_remote_codex"},
                "status": {"level": "healthy", "attentionItems": []}
            })));
        }
        Ok(json_output(&serde_json::json!({})))
    }
}

fn json_output<T: serde::Serialize>(value: &T) -> ProcessOutput {
    ProcessOutput {
        status_code: 0,
        stdout: serde_json::to_string(value).expect("json") + "\n",
        stderr: String::new(),
    }
}

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
fn blocked_remote_agent_handoff_points_at_conflict_resolution() {
    let output = bootstrap_output(
        BootstrapOutputBase {
            host: "linux-box".to_string(),
            root: "~/Code".to_string(),
            local_root: Some("~/Code".to_string()),
            generated_at: "2026-06-24T12:00:00Z".to_string(),
            steps: vec![step(
                BootstrapStepName::AgentLease,
                BootstrapStepState::Blocked,
                "Remote agent start failed: conflicts need attention",
            )],
            remote_status_items: vec![StatusItem {
                kind: StatusItemKind::Conflict,
                summary: "1 unresolved conflict needs attention".to_string(),
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
            }],
        },
        None,
        None,
        true,
        Some(WorkspaceStatus {
            level: StatusLevel::Attention,
            attention_items: vec!["1 unresolved conflict needs attention".to_string()],
        }),
    );

    assert_eq!(output.sync, BootstrapSyncState::Blocked);
    assert!(output.repair_actions.iter().any(|action| {
        action.label == "Resolve remote conflicts"
            && action.command.as_deref()
                == Some(ssh_command("linux-box", "bowline resolve ~/Code --json").as_str())
    }));
    assert!(
        output
            .repair_actions
            .iter()
            .any(|action| action.label == "Retry remote bootstrap")
    );
    // A blocked handoff is repaired by resolving conflicts / retrying, never by an
    // agent-launch action.
    assert!(
        !output
            .repair_actions
            .iter()
            .any(|action| action.label == "Start remote agent work")
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
fn fake_ssh_bootstrap_completes_device_trust_and_prepares_agent_lease() {
    let control_plane = FakeControlPlaneClient::default();
    let workspace_id = WorkspaceId::new("ws_agent_native_fake_bootstrap");
    control_plane.create_workspace(workspace_id.as_str());
    let local_keychain = FakeKeychain::default();
    bowline_local::trust::ensure_first_device_trust_root(
        &control_plane,
        &local_keychain,
        workspace_id.clone(),
        DeviceId::new("local-codex"),
        "Local Codex".to_string(),
        DevicePlatform::Macos,
        "2026-06-26T12:00:00Z",
    )
    .expect("local device trusted");
    let runner = FakeBootstrapRunner {
        control_plane: control_plane.clone(),
        remote_keychain: FakeKeychain::default(),
        workspace_id: workspace_id.clone(),
        request_id: Rc::new(RefCell::new(None)),
        remote_daemon_running: Rc::new(RefCell::new(false)),
    };
    let output = run_after_install(AfterInstallInput {
        runner: &runner,
        args: BootstrapSshArgs {
            host: "linux-box".to_string(),
            root: "~/Code".to_string(),
            artifact: None,
            project: Some("foo".to_string()),
            task: Some("implement the thing".to_string()),
            agent: Some("codex".to_string()),
        },
        generated_at: "2026-06-26T12:00:00Z".to_string(),
        steps: vec![step(
            BootstrapStepName::Install,
            BootstrapStepState::Completed,
            "Installed fake bowline artifacts.",
        )],
        install: RemoteBowlineInstall {
            platform: RemotePlatform {
                os: "linux".to_string(),
                arch: "x86_64".to_string(),
            },
            remote_binary: "~/.local/bin/bowline".to_string(),
            remote_daemon_binary: "~/.local/bin/bowline-daemon".to_string(),
            artifact_sha256: "0123456789abcdef".repeat(4),
            daemon_artifact_sha256: "fedcba9876543210".repeat(4),
        },
        control_plane: &control_plane,
        key_store: &local_keychain,
        workspace_id: workspace_id.clone(),
        device_id: DeviceId::new("local-codex"),
        remote_secret_env: Vec::new(),
    });

    assert!(output.trusted);
    assert_eq!(output.sync, BootstrapSyncState::Ready);
    assert!(
        output
            .steps
            .iter()
            .all(|step| step.state == BootstrapStepState::Completed)
    );
    assert_eq!(
        output
            .authorized_device
            .as_ref()
            .expect("authorized remote")
            .id
            .as_str(),
        "remote-linux"
    );
    assert!(output.steps.iter().any(|step| {
        step.name == BootstrapStepName::AgentLease
            && step.state == BootstrapStepState::Completed
            && step.summary.contains("lease-remote-codex")
            && step.summary.contains("Prepared remote agent work")
    }));
    // Bootstrap prepares the handoff lease and stops; it never launches, completes,
    // or accepts the agent, so no agent-launch repair actions are emitted.
    assert!(
        !output
            .repair_actions
            .iter()
            .any(|action| action.label.contains("Launch Codex"))
    );
    assert!(
        !output
            .repair_actions
            .iter()
            .any(|action| action.label.contains("Copy prompt"))
    );

    let trust = control_plane
        .list_device_trust(&workspace_id)
        .expect("trust list");
    assert!(trust.pending_requests.is_empty());
    assert!(trust.authorized_devices.iter().any(|device| {
        device.device_id == "remote-linux" && device.device_name == "Remote Linux"
    }));
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
            lease_id: None,
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
