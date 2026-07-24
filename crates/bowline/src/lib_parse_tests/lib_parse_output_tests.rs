#[test]
fn recovery_json_omits_one_time_generated_words() {
    let output = super::super::recovery::RecoveryRunOutput {
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
            setup_receipts_digest: None,
        }],
        created_request: None,
        approved_device: None,
        denied_request: None,
        revoked_device: None,
        recovery_key: Some(bowline_core::devices::RecoveryKeyState::missing()),
        next_actions: Vec::new(),
    };

    let human = super::super::render_devices_human(&output);
    let quiet = super::super::render_devices_quiet(&output);

    assert!(human.contains("code 842113"));
    assert!(human.contains("device-request:ws_devices:linux"));
    assert_eq!(quiet, "device-request:ws_devices:linux\n");
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
        resolved_project_root: None,
        workspace_summary: None,
        setup_readiness: None,
        sync_queue: None,
        convergence: None,
        freshness: bowline_core::status::FreshnessVerdict::Unknown,
        stale_bases: Vec::new(),
        status: bowline_core::status::WorkspaceStatus::healthy(),
        status_summary: bowline_core::status::reduce_status_facts(
            Vec::new(),
            1,
            "2026-06-24T12:00:00Z",
        ),
        items: Vec::new(),
        limits: Vec::new(),
        event_watermarks: bowline_core::status::EventWatermarks {
            last_scan_at: None,
            last_event_id: None,
            event_lag_ms: Some(0),
        },
        next_actions: Vec::new(),
        device_approvals: Vec::new(),
        service: None,
        authentication: None,
        sync: None,
    };

    let item = super::super::device_status_item(
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
            name: bowline_core::commands::BootstrapStepName::Trust,
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
        repair_actions: Vec::new(),
    };

    assert!(super::super::bootstrap_ssh_succeeded(&output));

    output.trusted = false;
    assert!(!super::super::bootstrap_ssh_succeeded(&output));

    output.trusted = true;
    output.steps[0].state = bowline_core::commands::BootstrapStepState::Blocked;
    assert!(!super::super::bootstrap_ssh_succeeded(&output));

    output.steps[0].name = bowline_core::commands::BootstrapStepName::Sync;
    assert!(!super::super::bootstrap_ssh_succeeded(&output));

    output.steps[0].state = bowline_core::commands::BootstrapStepState::Completed;
    assert!(super::super::bootstrap_ssh_succeeded(&output));
}

#[test]
fn workspace_selection_preserves_complete_project_paths() {
    assert_eq!(
        super::super::selected_workspace_path(super::super::WorkspaceSelection {
            root: "~/Code".to_string(),
            project: Some(".".to_string()),
        }),
        std::env::current_dir()
            .ok()
            .map(|path| path.display().to_string())
    );
    assert_eq!(
        super::super::selected_workspace_path(super::super::WorkspaceSelection {
            root: "~/Code".to_string(),
            project: Some("~/Code/acme/web".to_string()),
        }),
        Some("~/Code/acme/web".to_string())
    );
    assert_eq!(
        super::super::selected_workspace_path(super::super::WorkspaceSelection {
            root: "~/Code".to_string(),
            project: Some("/tmp/acme/web".to_string()),
        }),
        Some("/tmp/acme/web".to_string())
    );
    assert_eq!(
        super::super::selected_workspace_path(super::super::WorkspaceSelection {
            root: "~/Code".to_string(),
            project: Some("acme/web".to_string()),
        }),
        Some("~/Code/acme/web".to_string())
    );
}
