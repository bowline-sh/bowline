use crate::{
    idempotency::{command_has_cwd_relative_target, path_depends_on_cwd},
    runtime,
};

use super::{
    ActionsArgs, Command, DEFAULT_AGENT_HYDRATE_BUDGET_BYTES, DaemonCommand, StatusArgs, TuiArgs,
    agent, approve_no_pending_exit_code, bootstrap::BootstrapSshArgs, devices::DevicesArgs, login,
    parse_args, recovery::RecoveryArgs, redact_setup_text, resolve,
};
use bowline_core::commands::{AgentLeaseBase, CommandName};
use std::path::{Path, PathBuf};

#[test]
fn parses_global_json_anywhere() {
    let cli = parse_args(["status", "--json"]);

    assert!(cli.json);
    assert_eq!(
        cli.command,
        Command::Status(StatusArgs {
            path: None,
            watch: false,
            workspace: false,
        })
    );
}

#[test]
fn json_login_does_not_poll_before_printing_verification_url() {
    let args = super::login_args_for_output(
        login::LoginArgs {
            root: None,
            no_poll: false,
            headless: false,
        },
        true,
    );

    assert!(args.no_poll);
    assert!(!args.headless);
}

#[test]
fn parses_logout() {
    let cli = parse_args(["logout", "--json"]);

    assert!(cli.json);
    assert_eq!(cli.command, Command::Logout);
}

#[test]
fn approve_no_pending_json_uses_usage_exit_code() {
    assert_eq!(approve_no_pending_exit_code(true), super::EXIT_USAGE);
    assert_eq!(approve_no_pending_exit_code(false), 0);
}

#[test]
fn parses_status_watch_workspace() {
    let cli = parse_args(["status", "--watch", "--workspace", "~/Code"]);

    assert_eq!(
        cli.command,
        Command::Status(StatusArgs {
            path: Some("~/Code".to_string()),
            watch: true,
            workspace: true,
        })
    );
}

#[test]
fn parses_agent_lease_create() {
    let cli = parse_args([
        "agent",
        "start",
        "/tmp/project",
        "--task",
        "fix race",
        "--json",
    ]);

    assert!(cli.json);
    assert_eq!(
        cli.command,
        Command::AgentLeaseCreate(agent::AgentLeaseCreateArgs {
            project_path: "/tmp/project".to_string(),
            task: "fix race".to_string(),
            base: AgentLeaseBase::LatestWorkspace,
            hydrate_budget_bytes: DEFAULT_AGENT_HYDRATE_BUDGET_BYTES,
            work_view: false,
        })
    );
}

#[test]
fn parses_agent_lease_create_work_view_opt_in() {
    let cli = parse_args([
        "agent",
        "start",
        "/tmp/project",
        "--task",
        "try router rewrite",
        "--work-view",
    ]);

    assert_eq!(
        cli.command,
        Command::AgentLeaseCreate(agent::AgentLeaseCreateArgs {
            project_path: "/tmp/project".to_string(),
            task: "try router rewrite".to_string(),
            base: AgentLeaseBase::LatestWorkspace,
            hydrate_budget_bytes: DEFAULT_AGENT_HYDRATE_BUDGET_BYTES,
            work_view: true,
        })
    );
}

#[test]
fn rejects_bootstrap_ssh_alias() {
    let cli = parse_args(["bootstrap", "ssh", "linux-server-1", "--root", "/tmp/code"]);

    assert!(matches!(cli.command, Command::UsageError { .. }));
}

#[test]
fn parses_connect_agent_handoff() {
    let cli = parse_args([
        "connect",
        "linux-server-1",
        "--root",
        "~/Code",
        "--project",
        "foo",
        "--task",
        "implement sync",
        "--agent",
        "codex",
    ]);

    assert_eq!(
        cli.command,
        Command::BootstrapSsh(BootstrapSshArgs {
            host: "linux-server-1".to_string(),
            root: "~/Code".to_string(),
            artifact: None,
            project: Some("foo".to_string()),
            task: Some("implement sync".to_string()),
            agent: Some("codex".to_string()),
        })
    );
}

#[test]
fn parses_connect_explicit_root() {
    let cli = parse_args(["connect", "linux-server-1", "--root", "/tmp/code"]);

    assert_eq!(
        cli.command,
        Command::BootstrapSsh(BootstrapSshArgs {
            host: "linux-server-1".to_string(),
            root: "/tmp/code".to_string(),
            artifact: None,
            project: None,
            task: None,
            agent: None,
        })
    );
}

#[test]
fn legacy_diff_usage_message_matches_invoked_command() {
    let cli = parse_args(["diff"]);
    let Command::CommandUsageError(error) = cli.command else {
        panic!("diff without a selector should return a usage error");
    };

    assert_eq!(error.command, CommandName::Diff);
    assert_eq!(
        error.message,
        "bowline diff requires a work-view id or name"
    );
}

#[test]
fn parses_devices_request_default_and_explicit_root() {
    let default_cli = parse_args(["devices", "request"]);
    assert_eq!(
        default_cli.command,
        Command::Devices(DevicesArgs::Request { root: None })
    );

    let explicit_cli = parse_args(["devices", "request", "--root", "/tmp/code"]);
    assert_eq!(
        explicit_cli.command,
        Command::Devices(DevicesArgs::Request {
            root: Some("/tmp/code".to_string()),
        })
    );
}

#[test]
fn parses_resolve_phase_7_shape() {
    let cli = parse_args([
        "resolve",
        "~/Code/app",
        "--copy-prompt",
        "--agent",
        "codex",
        "--json",
    ]);

    assert!(cli.json);
    assert_eq!(
        cli.command,
        Command::Resolve(resolve::ResolveArgs {
            project_or_path: "~/Code/app".to_string(),
            copy_prompt: true,
            tui: false,
            diff: None,
            agent: Some(resolve::ResolveAgent::Codex),
            decision: None,
        })
    );
}

#[test]
fn parses_actions_and_tui_entrypoints() {
    let actions = parse_args(["actions", "~/Code/app", "--workspace"]);
    assert_eq!(
        actions.command,
        Command::Actions(ActionsArgs {
            path: Some("~/Code/app".to_string()),
            workspace: true,
        })
    );

    let tui = parse_args(["tui", "~/Code/app"]);
    assert_eq!(
        tui.command,
        Command::Tui(TuiArgs {
            path: Some("~/Code/app".to_string()),
        })
    );
}

#[test]
fn parses_resolve_tui_flag() {
    let cli = parse_args(["resolve", "~/Code/app", "--tui"]);
    assert_eq!(
        cli.command,
        Command::Resolve(resolve::ResolveArgs {
            project_or_path: "~/Code/app".to_string(),
            copy_prompt: false,
            tui: true,
            diff: None,
            agent: None,
            decision: None,
        })
    );
}

#[test]
fn splits_tui_action_commands_with_shell_quoted_paths() {
    assert_eq!(
        super::split_tui_command_line("bowline resolve '~/Code/my app' --accept conflict-1"),
        Ok(vec![
            "bowline".to_string(),
            "resolve".to_string(),
            "~/Code/my app".to_string(),
            "--accept".to_string(),
            "conflict-1".to_string(),
        ])
    );
    assert_eq!(
        super::split_tui_command_line("bowline status 'repo'\\''s path'"),
        Ok(vec![
            "bowline".to_string(),
            "status".to_string(),
            "repo's path".to_string(),
        ])
    );
    assert_eq!(
        super::split_tui_command_line("bowline status 'unterminated"),
        Err("unterminated quote in TUI action command")
    );
}

#[test]
fn confirmed_tui_child_args_preserve_socket_override() {
    let args = super::confirmed_tui_child_args(
        "bowline resolve '~/Code/my app' --accept conflict-1",
        std::path::Path::new("/tmp/bowline-review.sock"),
    )
    .expect("command should parse");

    assert_eq!(
        args,
        vec![
            std::ffi::OsString::from("--socket"),
            std::ffi::OsString::from("/tmp/bowline-review.sock"),
            std::ffi::OsString::from("resolve"),
            std::ffi::OsString::from("~/Code/my app"),
            std::ffi::OsString::from("--accept"),
            std::ffi::OsString::from("conflict-1"),
        ]
    );
}

#[test]
fn parses_resolve_accept_reject_as_single_action() {
    let accept = parse_args(["resolve", "~/Code/app", "--accept", "conflict-1"]);

    assert_eq!(
        accept.command,
        Command::Resolve(resolve::ResolveArgs {
            project_or_path: "~/Code/app".to_string(),
            copy_prompt: false,
            tui: false,
            diff: None,
            agent: None,
            decision: Some(resolve::ResolveDecision::Accept("conflict-1".to_string())),
        })
    );

    let diff = parse_args(["resolve", "~/Code/app", "--diff", "conflict-1"]);
    assert_eq!(
        diff.command,
        Command::Resolve(resolve::ResolveArgs {
            project_or_path: "~/Code/app".to_string(),
            copy_prompt: false,
            tui: false,
            diff: Some("conflict-1".to_string()),
            agent: None,
            decision: None,
        })
    );

    let reject = parse_args([
        "resolve",
        "~/Code/app",
        "--accept",
        "conflict-1",
        "--reject",
        "conflict-2",
    ]);

    assert!(matches!(reject.command, Command::UsageError { .. }));
}

#[test]
fn parses_recovery_words_from_stdin_shape_only() {
    let cli = parse_args(["recover", "verify", "rk_123"]);

    assert_eq!(
        cli.command,
        Command::Recovery(RecoveryArgs::Verify {
            envelope_id: "rk_123".to_string(),
        })
    );
}

#[test]
fn rejects_recovery_words_in_argv() {
    let cli = parse_args(["recover", "verify", "rk_123", "secret", "words"]);

    assert!(matches!(cli.command, Command::CommandUsageError(_)));
}

#[test]
fn next_exploration_cursor_stops_at_accepted_cap() {
    assert_eq!(
        super::next_exploration_cursor(9_900, 100, true),
        Some("v1:10000".to_string())
    );
    assert_eq!(super::next_exploration_cursor(10_000, 100, true), None);
    assert_eq!(super::next_exploration_cursor(0, 100, false), None);
}

#[test]
fn idempotency_cwd_identity_only_for_relative_paths() {
    assert!(path_depends_on_cwd("apps/web"));
    assert!(path_depends_on_cwd("."));
    assert!(!path_depends_on_cwd("/tmp/project"));
    assert!(!path_depends_on_cwd("~/Code/project"));
}

#[test]
fn connect_relative_targets_require_cwd_identity() {
    let base = BootstrapSshArgs {
        host: "linux-server-1".to_string(),
        root: "~/Code".to_string(),
        artifact: None,
        project: None,
        task: None,
        agent: None,
    };
    assert!(!command_has_cwd_relative_target(&Command::BootstrapSsh(
        base.clone()
    )));

    let mut relative_root = base.clone();
    relative_root.root = "Code".to_string();
    assert!(command_has_cwd_relative_target(&Command::BootstrapSsh(
        relative_root
    )));

    let mut relative_binary = base.clone();
    relative_binary.artifact = Some("target/release/bowline".to_string());
    assert!(command_has_cwd_relative_target(&Command::BootstrapSsh(
        relative_binary
    )));

    let mut relative_project = base;
    relative_project.project = Some("apps/web".to_string());
    assert!(command_has_cwd_relative_target(&Command::BootstrapSsh(
        relative_project
    )));
}

#[test]
fn recovery_json_omits_one_time_generated_words() {
    let output = super::recovery::RecoveryRunOutput {
        output: bowline_core::commands::RecoveryCommandOutput {
            contract_version: bowline_core::commands::CONTRACT_VERSION,
            command: bowline_core::commands::CommandName::Recover,
            generated_at: "2026-06-24T12:00:00Z".to_string(),
            action: bowline_core::commands::RecoveryCommandAction::Create,
            workspace_id: Some(bowline_core::ids::WorkspaceId::new("ws_recovery_json")),
            recovery_key: bowline_core::devices::RecoveryKeyState {
                lifecycle: bowline_core::devices::RecoveryKeyLifecycle::GeneratedUnverified,
                envelope_id: Some(bowline_core::ids::RecoveryEnvelopeId::new("rk_json")),
                fingerprint: Some("rkp_json".to_string()),
                created_at: Some("2026-06-24T12:00:00Z".to_string()),
                verified_at: None,
                rotated_at: None,
                revoked_at: None,
            },
            device_request: None,
            encrypted_grant: None,
            next_actions: Vec::new(),
        },
        generated_words: Some("alpha beta gamma".to_string()),
    };

    let json = serde_json::to_value(&output.output).expect("recovery json output serializes");

    assert!(json.get("generatedWords").is_none());
    assert_eq!(json["action"], "create");
    assert_eq!(json["recoveryKey"]["lifecycle"], "generated-unverified");
}

#[test]
fn devices_list_human_output_includes_pending_matching_code() {
    let workspace_id = bowline_core::ids::WorkspaceId::new("ws_devices");
    let output = bowline_core::commands::DevicesCommandOutput {
        contract_version: bowline_core::commands::CONTRACT_VERSION,
        command: bowline_core::commands::CommandName::Devices,
        generated_at: "2026-06-24T12:00:00Z".to_string(),
        action: bowline_core::commands::DeviceCommandAction::List,
        workspace_id: Some(workspace_id.clone()),
        local_device: None,
        devices: Vec::new(),
        revoked_devices: Vec::new(),
        pending_requests: vec![bowline_core::devices::DeviceApprovalRequest {
            request_id: bowline_core::ids::DeviceApprovalRequestId::new(
                "device-request:ws_devices:linux",
            ),
            workspace_id: workspace_id.clone(),
            requester_device_id: bowline_core::ids::DeviceId::new("device_linux"),
            device_name: "linux-server-1".to_string(),
            platform: bowline_core::devices::DevicePlatform::Linux,
            device_public_key: bowline_core::devices::PublicDeviceKey::new("age1linux"),
            device_fingerprint: bowline_core::devices::DeviceFingerprint::new("fp_linux"),
            matching_code: "842113".to_string(),
            requested_at: "2026-06-24T12:00:00Z".to_string(),
            expires_at: "2026-06-24T12:10:00Z".to_string(),
            state: bowline_core::devices::DeviceApprovalRequestState::Pending,
            host: Some("linux-server-1".to_string()),
            root: Some("~/Code".to_string()),
        }],
        created_request: None,
        approved_device: None,
        denied_request: None,
        revoked_device: None,
        recovery_key: Some(bowline_core::devices::RecoveryKeyState::missing()),
        next_actions: Vec::new(),
    };

    let human = super::render_devices_human(&output);

    assert!(human.contains("code 842113"));
    assert!(human.contains("device-request:ws_devices:linux"));
}

#[test]
fn device_status_item_uses_explicit_subject_and_device_identity() {
    let output = bowline_core::commands::StatusCommandOutput {
        contract_version: bowline_core::commands::CONTRACT_VERSION,
        command: bowline_core::commands::CommandName::Status,
        generated_at: "2026-06-24T12:00:00Z".to_string(),
        workspace_id: bowline_core::ids::WorkspaceId::new("ws_devices"),
        project_id: Some(bowline_core::ids::ProjectId::new("proj_devices")),
        scope: None,
        requested_path: None,
        resolved_workspace_root: None,
        workspace_summary: None,
        index: None,
        hydration_budget: None,
        hydration_progress: Vec::new(),
        sync_queue: None,
        status: bowline_core::status::WorkspaceStatus::healthy(),
        items: Vec::new(),
        limits: Vec::new(),
        event_watermarks: bowline_core::status::EventWatermarks {
            last_scan_at: None,
            last_event_id: None,
            event_lag_ms: Some(0),
            sync_state: None,
            watcher_state: None,
            network_state: None,
        },
        next_actions: Vec::new(),
    };

    let item = super::device_status_item(
        &output,
        bowline_core::status::StatusSubjectKind::DeviceApprovalRequest,
        "device-request:ws_devices:linux",
        Some(bowline_core::ids::DeviceId::new("device_linux")),
        "linux-server-1 is waiting for approval.".to_string(),
    );

    let subject = item.subject.expect("device status has a subject");
    assert_eq!(
        subject.kind,
        bowline_core::status::StatusSubjectKind::DeviceApprovalRequest
    );
    assert_eq!(subject.id, "device-request:ws_devices:linux");
    assert_eq!(item.device_id.expect("device id").as_str(), "device_linux");
    assert_eq!(
        item.project_id.expect("project id").as_str(),
        "proj_devices"
    );
}

#[test]
fn bootstrap_ssh_success_requires_trusted_remote_and_unblocked_steps() {
    let mut output = bowline_core::commands::BootstrapSshCommandOutput {
        contract_version: bowline_core::commands::CONTRACT_VERSION,
        command: bowline_core::commands::CommandName::Connect,
        generated_at: "2026-06-24T12:00:00Z".to_string(),
        workspace_id: Some(bowline_core::ids::WorkspaceId::new("ws_bootstrap")),
        project_id: None,
        host: "linux-server-1".to_string(),
        root: "~/Code".to_string(),
        steps: vec![bowline_core::commands::BootstrapStep {
            name: "trust".to_string(),
            state: bowline_core::commands::BootstrapStepState::Completed,
            summary: "Remote device is trusted.".to_string(),
        }],
        device_request: None,
        authorized_device: None,
        remote_device_fingerprint: None,
        trusted: true,
        secret_store: bowline_core::commands::BootstrapSecretStore::ServerLocal,
        sync: bowline_core::commands::BootstrapSyncState::Ready,
        next_required_phase: None,
        remote_status: bowline_core::status::WorkspaceStatus::healthy(),
        next_actions: Vec::new(),
    };

    assert!(super::bootstrap_ssh_succeeded(&output));

    output.trusted = false;
    assert!(!super::bootstrap_ssh_succeeded(&output));

    output.trusted = true;
    output.steps[0].state = bowline_core::commands::BootstrapStepState::Blocked;
    assert!(!super::bootstrap_ssh_succeeded(&output));

    output.steps[0].name = "sync".to_string();
    assert!(!super::bootstrap_ssh_succeeded(&output));

    output.steps[0].state = bowline_core::commands::BootstrapStepState::Completed;
    assert!(super::bootstrap_ssh_succeeded(&output));
}
