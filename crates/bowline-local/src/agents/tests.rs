use crate::{
    metadata::MetadataStore,
    status::{StatusOptions, compose_status},
    workspace::TempWorkspace,
};
use bowline_core::{
    commands::{AgentToolAuthority, AgentToolInvokeRequest, AgentToolTransport},
    ids::{DeviceId, LeaseId, ProjectId, SnapshotId, WorkspaceId},
    status::FreshnessVerdict,
};

use super::*;

mod mcp;
mod stale_base;

#[test]
fn default_lease_binds_directly_to_real_project_without_work_view() {
    let (temp, db_path) = seeded_store("agent-lease-direct");
    let project_path = temp.root().join("Code/apps/web");

    let output = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        task: "fix auth routing".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        work_view: false,
        force_stale: false,
        device_id: DeviceId::new("device-test"),
        generated_at: now(),
    })
    .expect("direct lease created");

    assert_eq!(output.lease.write_target_mode, AgentWriteTargetMode::Direct);
    assert_eq!(output.lease.expires_at, "2026-06-26T12:00:00Z");
    assert_eq!(
        output.lease.write_target_path,
        project_path.display().to_string()
    );
    assert_eq!(
        output.lease.work_view_path,
        project_path.display().to_string()
    );
    assert!(
        !temp.root().join("Code/.work").exists(),
        "direct lease must not create a work-view tree"
    );

    let context = agent_context(AgentLeaseSelectorOptions {
        db_path: Some(db_path.clone()),
        lease_id: output.lease.id.clone(),
        generated_at: now(),
    })
    .expect("direct context");
    assert_eq!(
        context.context.write_target_path,
        project_path.display().to_string()
    );
    assert_eq!(
        context.context.start_work.cwd,
        project_path.display().to_string()
    );
    let work_view_only_tool = AgentToolName::ListOverlayChanges;
    assert!(
        !context
            .context
            .capabilities
            .iter()
            .any(|capability| capability.name == work_view_only_tool),
        "direct agent context must not advertise {work_view_only_tool:?}"
    );

    let listed_capabilities = invoke_agent_tool(
        Some(db_path.clone()),
        tool_request(
            &output.lease,
            AgentToolName::ListCapabilities,
            serde_json::json!({}),
        ),
        now(),
    )
    .expect("direct list capabilities");
    assert_eq!(listed_capabilities.outcome, AgentToolResultOutcome::Allowed);
    let listed_capabilities = listed_capabilities
        .payload
        .expect("list capabilities payload")
        .remove("capabilities")
        .expect("capabilities field");
    let listed_capabilities = listed_capabilities.as_array().expect("capabilities array");
    let serialized_name = serde_json::to_value(work_view_only_tool).expect("tool name serializes");
    assert!(
        !listed_capabilities
            .iter()
            .any(|capability| capability.get("name") == Some(&serialized_name)),
        "direct list_capabilities must not advertise {work_view_only_tool:?}"
    );
    let token = issue_agent_mcp_token(AgentMcpTokenIssueOptions {
        db_path: Some(db_path.clone()),
        lease_id: output.lease.id.clone(),
        grants: vec![],
        generated_at: now(),
    })
    .expect("token issued");

    let completed = complete_agent_session(AgentLeaseSelectorOptions {
        db_path: Some(db_path.clone()),
        lease_id: output.lease.id.clone(),
        generated_at: "2026-06-25T12:00:01Z".to_string(),
    })
    .expect("direct session completes");
    assert_eq!(completed.command, CommandName::AgentComplete);
    assert_eq!(completed.lease.session_state, AgentSessionState::Completed);
    assert_eq!(completed.lease.status_summary, "completed");
    assert_eq!(completed.status.level, StatusLevel::Healthy);
    assert!(completed.status.attention_items.is_empty());
    assert_eq!(
        MetadataStore::open(&db_path)
            .expect("store")
            .agent_mcp_token_by_file(&token.token_file)
            .expect("token lookup")
            .expect("token record")
            .revoked_at
            .as_deref(),
        Some("2026-06-25T12:00:01Z")
    );
    assert_eq!(
        completed.next_actions[0].command.as_deref(),
        Some(
            format!(
                "bowline status --root {} --project apps/web",
                temp.root().join("Code").display()
            )
            .as_str()
        )
    );

    let repeated = complete_agent_session(AgentLeaseSelectorOptions {
        db_path: Some(db_path),
        lease_id: output.lease.id.clone(),
        generated_at: "2026-06-25T12:00:02Z".to_string(),
    })
    .expect("completion is idempotent");
    assert_eq!(repeated.lease.session_state, AgentSessionState::Completed);
}

#[test]
fn cancelling_agent_session_is_idempotent_and_revokes_tools_and_tokens() {
    let (temp, db_path) = seeded_store("agent-lease-cancel");
    let project_path = temp.root().join("Code/apps/web");
    let lease = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        task: "cancel me".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        work_view: false,
        force_stale: false,
        device_id: DeviceId::new("device-test"),
        generated_at: now(),
    })
    .expect("lease created")
    .lease;
    let token = issue_agent_mcp_token(AgentMcpTokenIssueOptions {
        db_path: Some(db_path.clone()),
        lease_id: lease.id.clone(),
        grants: vec![],
        generated_at: now(),
    })
    .expect("token issued");

    let cancelled = cancel_agent_session(AgentLeaseSelectorOptions {
        db_path: Some(db_path.clone()),
        lease_id: lease.id.clone(),
        generated_at: "2026-06-25T12:01:00Z".to_string(),
    })
    .expect("session cancelled");
    assert_eq!(cancelled.command, CommandName::AgentCancel);
    assert_eq!(cancelled.lease.session_state, AgentSessionState::Cancelled);
    assert_eq!(cancelled.lease.expires_at, "2026-06-25T12:01:00Z");
    let cancelled_context = agent_context(AgentLeaseSelectorOptions {
        db_path: Some(db_path.clone()),
        lease_id: lease.id.clone(),
        generated_at: "2026-06-25T12:01:01Z".to_string(),
    })
    .expect("cancelled context remains inspectable");
    assert!(
        cancelled_context
            .context
            .instructions
            .iter()
            .any(|instruction| instruction.contains("session is cancelled"))
    );
    assert!(
        cancelled_context
            .context
            .instructions
            .iter()
            .all(|instruction| !instruction.contains("agent complete"))
    );

    let repeated = cancel_agent_session(AgentLeaseSelectorOptions {
        db_path: Some(db_path.clone()),
        lease_id: lease.id.clone(),
        generated_at: "2026-06-25T12:02:00Z".to_string(),
    })
    .expect("cancellation is idempotent");
    assert_eq!(repeated.lease.session_state, AgentSessionState::Cancelled);
    assert_eq!(repeated.lease.expires_at, "2026-06-25T12:01:00Z");

    let denied = invoke_agent_tool(
        Some(db_path.clone()),
        tool_request(
            &lease,
            AgentToolName::WorkspaceStatus,
            serde_json::json!({}),
        ),
        "2026-06-25T12:02:00Z".to_string(),
    )
    .expect("tool denial");
    assert_eq!(denied.outcome, AgentToolResultOutcome::Denied);
    assert_eq!(denied.denial.expect("denial").code, "lease-expired");
    let stored_token = MetadataStore::open(&db_path)
        .expect("store")
        .agent_mcp_token_by_file(&token.token_file)
        .expect("token lookup")
        .expect("token record");
    assert_eq!(
        stored_token.revoked_at.as_deref(),
        Some("2026-06-25T12:01:00Z")
    );
    let token_issue = issue_agent_mcp_token(AgentMcpTokenIssueOptions {
        db_path: Some(db_path),
        lease_id: lease.id,
        grants: vec![],
        generated_at: "2026-06-25T12:02:00Z".to_string(),
    });
    assert!(matches!(token_issue, Err(AgentError::ToolDenied { .. })));
}

#[test]
fn extending_agent_session_sets_a_bounded_deadline_without_resurrection() {
    let (temp, db_path) = seeded_store("agent-lease-extend");
    let project_path = temp.root().join("Code/apps/web");
    let lease = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        task: "long task".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        work_view: false,
        force_stale: false,
        device_id: DeviceId::new("device-test"),
        generated_at: now(),
    })
    .expect("lease created")
    .lease;

    let extended = extend_agent_session(AgentLeaseExtendOptions {
        db_path: Some(db_path.clone()),
        lease_id: lease.id.clone(),
        hours: 48,
        generated_at: "2026-06-25T12:01:00Z".to_string(),
    })
    .expect("lease extended");
    assert_eq!(extended.command, CommandName::AgentExtend);
    assert_eq!(extended.lease.expires_at, "2026-06-27T12:01:00Z");
    let repeated = extend_agent_session(AgentLeaseExtendOptions {
        db_path: Some(db_path.clone()),
        lease_id: lease.id.clone(),
        hours: 48,
        generated_at: "2026-06-25T12:01:00Z".to_string(),
    })
    .expect("same extension is idempotent");
    assert_eq!(repeated.lease.expires_at, extended.lease.expires_at);

    for hours in [0, MAX_AGENT_LEASE_EXTENSION_HOURS + 1] {
        let result = extend_agent_session(AgentLeaseExtendOptions {
            db_path: Some(db_path.clone()),
            lease_id: lease.id.clone(),
            hours,
            generated_at: "2026-06-25T12:01:00Z".to_string(),
        });
        assert!(matches!(result, Err(AgentError::InvalidLease { .. })));
    }
    let expired = extend_agent_session(AgentLeaseExtendOptions {
        db_path: Some(db_path),
        lease_id: lease.id,
        hours: 1,
        generated_at: "2026-06-28T12:01:00Z".to_string(),
    });
    assert!(matches!(expired, Err(AgentError::InvalidLease { .. })));
}

#[test]
fn lease_create_binds_to_work_view_and_context_is_secret_free() {
    let (temp, db_path) = seeded_store("agent-lease-create");
    let project_path = temp.root().join("Code/apps/web");
    let task_marker = "task-tail-marker-20260627T090102Z";

    let output = create_agent_lease(AgentLeaseCreateOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            task: format!(
                "fix auth token in the remote agent task prompt without truncating the exact requested work item {task_marker} OPENAI_API_KEY=sk_live_abcdefghijklmnopqrstuvwxyz"
            ),
            base: AgentLeaseBase::LatestWorkspace,
            work_view: true,
            force_stale: false,
        device_id: DeviceId::new("device-test"),
            generated_at: now(),
        })
        .expect("lease created");

    assert_eq!(output.lease.session_state, AgentSessionState::Open);
    assert!(output.lease.task.contains(task_marker));
    assert!(
        !output
            .lease
            .task
            .contains("sk_live_abcdefghijklmnopqrstuvwxyz")
    );
    assert!(
        output
            .lease
            .work_view_path
            .contains(".work/apps/web/agent-fix-auth-token")
    );
    assert!(
        fs::metadata(&output.lease.work_view_path)
            .expect("work view")
            .is_dir()
    );

    let context = agent_context(AgentLeaseSelectorOptions {
        db_path: Some(db_path.clone()),
        lease_id: output.lease.id.clone(),
        generated_at: now(),
    })
    .expect("context");
    let context_json = serde_json::to_string(&context).expect("context serializes");
    assert!(!context_json.contains("nonce"));
    assert!(!context_json.contains("secret"));
    assert!(!context_json.contains("sk_live_abcdefghijklmnopqrstuvwxyz"));
    assert!(context_json.contains("OPENAI_API_KEY=[redacted]"));
    assert!(context_json.contains(task_marker));
    assert_eq!(context.context.work_view_path, output.lease.work_view_path);
    assert!(context.context.capabilities.iter().any(|capability| {
        capability.name == AgentToolName::ListOverlayChanges
            && capability.state == AgentCapabilityState::Available
    }));

    let prompt = agent_prompt(AgentLeaseSelectorOptions {
        db_path: Some(db_path.clone()),
        lease_id: output.lease.id.clone(),
        generated_at: now(),
    })
    .expect("prompt");
    assert!(prompt.prompt.text.contains(task_marker));
    // Completion closes only the session record; it does not revive the removed
    // publish/review gate. Humans still review and accept the work view explicitly.
    assert!(!prompt.prompt.text.contains("agent publish"));
    assert!(prompt.prompt.text.contains("agent complete"));
    assert!(prompt.prompt.text.contains("bowline work review"));
    assert!(!prompt.prompt.text.contains("~/.local/bin/bowline"));
    assert!(
        !prompt
            .prompt
            .text
            .contains("sk_live_abcdefghijklmnopqrstuvwxyz")
    );
    // Agent-output next_actions are emitted empty (067 handshake).
    assert!(prompt.next_actions.is_empty());

    let completed = complete_agent_session(AgentLeaseSelectorOptions {
        db_path: Some(db_path),
        lease_id: output.lease.id.clone(),
        generated_at: "2026-06-25T12:00:01Z".to_string(),
    })
    .expect("isolated session completes");
    assert_eq!(completed.lease.session_state, AgentSessionState::Completed);
    assert_eq!(completed.status.level, StatusLevel::Attention);
    assert!(
        completed
            .status
            .attention_items
            .iter()
            .any(|item| item.contains("review-ready"))
    );
    assert_eq!(completed.next_actions.len(), 1);
    assert_eq!(
        completed.next_actions[0].label,
        "Review the completed work view"
    );
    assert_eq!(
        completed.next_actions[0].command.as_deref(),
        Some(format!("bowline work review {}", output.lease.write_target_path).as_str())
    );
}

#[test]
fn completion_next_actions_quote_recorded_paths() {
    let temp = TempWorkspace::new("agent-complete-command-quoting").expect("temp workspace");
    let code_root = temp.root().join("Code Root");
    let project_relative = "apps/my web";
    let project_path = code_root.join(project_relative);
    fs::create_dir_all(&project_path).expect("project dir");
    let db_path = temp.root().join(".state/local.sqlite3");
    let store = MetadataStore::open(&db_path).expect("metadata");
    let workspace_id = WorkspaceId::new("ws_quoted");
    let project_id = ProjectId::new("proj_quoted");
    store
        .insert_workspace(&workspace_id, "Quoted Code", "2026-06-25T00:00:00Z")
        .expect("workspace");
    store
        .insert_root(
            "root_quoted",
            &workspace_id,
            &code_root.display().to_string(),
            "2026-06-25T00:00:00Z",
        )
        .expect("root");
    store
        .insert_project(
            &project_id,
            &workspace_id,
            "root_quoted",
            project_relative,
            "2026-06-25T00:00:00Z",
        )
        .expect("project");
    store
        .set_project_latest_snapshot_id(&workspace_id, &project_id, &SnapshotId::new("snap_quoted"))
        .expect("snapshot");

    let direct = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        task: "direct quoted command".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        work_view: false,
        force_stale: false,
        device_id: DeviceId::new("device-test"),
        generated_at: now(),
    })
    .expect("direct lease");
    let direct = complete_agent_session(AgentLeaseSelectorOptions {
        db_path: Some(db_path.clone()),
        lease_id: direct.lease.id,
        generated_at: "2026-06-25T12:00:01Z".to_string(),
    })
    .expect("complete direct lease");
    assert_eq!(
        direct.next_actions[0].command.as_deref(),
        Some(
            format!(
                "bowline status --root '{}' --project 'apps/my web'",
                code_root.display()
            )
            .as_str()
        )
    );

    let isolated = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        task: "isolated quoted command".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        work_view: true,
        force_stale: false,
        device_id: DeviceId::new("device-test"),
        generated_at: "2026-06-25T12:00:02Z".to_string(),
    })
    .expect("isolated lease");
    let expected_target = isolated.lease.write_target_path.clone();
    let isolated = complete_agent_session(AgentLeaseSelectorOptions {
        db_path: Some(db_path),
        lease_id: isolated.lease.id,
        generated_at: "2026-06-25T12:00:03Z".to_string(),
    })
    .expect("complete isolated lease");
    assert_eq!(
        isolated.next_actions[0].command.as_deref(),
        Some(format!("bowline work review '{expected_target}'").as_str())
    );
}

#[test]
fn lease_create_rolls_back_work_view_when_creation_event_fails() {
    let (temp, db_path) = seeded_store("agent-lease-create-rollback");
    let project_path = temp.root().join("Code/apps/web");
    let store = MetadataStore::open(&db_path).expect("store");
    store
        .connection()
        .execute(
            "CREATE TRIGGER fail_agent_lease_event
                 BEFORE INSERT ON events
                 BEGIN
                   SELECT RAISE(FAIL, 'forced event failure');
                 END",
            [],
        )
        .expect("event failure trigger");
    drop(store);

    let error = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        task: "rollback".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        work_view: true,
        force_stale: false,
        device_id: DeviceId::new("device-test"),
        generated_at: now(),
    })
    .expect_err("lease creation should fail when creation audit event fails");
    assert!(matches!(error, AgentError::Event(_)));

    let store = MetadataStore::open(&db_path).expect("store");
    assert!(
        store
            .agent_leases(&WorkspaceId::new("ws_code"))
            .expect("leases")
            .is_empty()
    );
    assert!(
        store
            .work_views(&WorkspaceId::new("ws_code"), true, None)
            .expect("work views")
            .is_empty()
    );
    let work_namespace = temp.root().join("Code/.work/apps/web");
    assert!(
        !work_namespace.exists()
            || fs::read_dir(work_namespace)
                .expect("work namespace")
                .next()
                .is_none()
    );
}

#[test]
fn status_recovers_provisional_lease_with_materialized_work_view() {
    let (temp, db_path) = seeded_store("agent-lease-provisional-finalize");
    let project_path = temp.root().join("Code/apps/web");
    let mut lease = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        task: "recover finalized lease".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        work_view: true,
        force_stale: false,
        device_id: DeviceId::new("device-test"),
        generated_at: now(),
    })
    .expect("lease created")
    .lease;

    lease.session_state = AgentSessionState::Provisional;
    lease.status_summary = AGENT_LEASE_STATUS_CREATING.to_string();
    let store = MetadataStore::open(&db_path).expect("store");
    store.upsert_agent_lease(&lease).expect("provisional lease");
    drop(store);

    let status = compose_status(StatusOptions {
        db_path: Some(db_path.clone()),
        requested_path: Some(project_path.display().to_string()),
        workspace_scope: false,
        generated_at: now(),
    })
    .expect("status recovers provisional lease");
    assert!(
        status
            .items
            .iter()
            .any(|item| item.lease_id == Some(lease.id.clone()))
    );

    let stored = MetadataStore::open(&db_path)
        .expect("store")
        .agent_lease_by_id(&lease.id)
        .expect("lease query")
        .expect("lease retained");
    assert_eq!(stored.session_state, AgentSessionState::Open);
    assert_eq!(stored.status_summary, "active");
}

#[test]
fn status_preserves_pending_dispatch_lease() {
    let (temp, db_path) = seeded_store("agent-lease-pending-dispatch");
    let project_path = temp.root().join("Code/apps/web");
    let mut lease = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        task: "dispatch agent work".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        work_view: true,
        force_stale: false,
        device_id: DeviceId::new("device-origin"),
        generated_at: now(),
    })
    .expect("lease created")
    .lease;

    lease.session_state = AgentSessionState::Provisional;
    lease.dispatch_state = AgentLeaseDispatchState::Pending;
    lease.target_device_ref = Some(DeviceId::new("device-target"));
    lease.status_summary = "pending dispatch to device-target".to_string();
    let store = MetadataStore::open(&db_path).expect("store");
    store
        .upsert_agent_lease(&lease)
        .expect("pending dispatch lease");
    drop(store);

    compose_status(StatusOptions {
        db_path: Some(db_path.clone()),
        requested_path: Some(project_path.display().to_string()),
        workspace_scope: false,
        generated_at: now(),
    })
    .expect("status preserves pending dispatch lease");

    let stored = MetadataStore::open(&db_path)
        .expect("store")
        .agent_lease_by_id(&lease.id)
        .expect("lease query")
        .expect("lease retained");
    assert_eq!(stored.session_state, AgentSessionState::Provisional);
    assert_eq!(stored.dispatch_state, AgentLeaseDispatchState::Pending);
    assert_eq!(stored.status_summary, "pending dispatch to device-target");
}

#[test]
fn status_removes_orphaned_provisional_lease_without_work_view() {
    let (temp, db_path) = seeded_store("agent-lease-provisional-orphan");
    let project_path = temp.root().join("Code/apps/web");
    let mut lease = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        task: "recover orphaned lease".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        work_view: true,
        force_stale: false,
        device_id: DeviceId::new("device-test"),
        generated_at: now(),
    })
    .expect("lease created")
    .lease;
    lease.session_state = AgentSessionState::Provisional;
    lease.status_summary = AGENT_LEASE_STATUS_CREATING.to_string();

    let store = MetadataStore::open(&db_path).expect("store");
    store.upsert_agent_lease(&lease).expect("provisional lease");
    store
        .connection()
        .execute(
            "DELETE FROM work_view_base_descriptors WHERE workspace_id = ?1 AND work_view_id = ?2",
            rusqlite::params![lease.workspace_id.as_str(), lease.work_view_id.as_str()],
        )
        .expect("delete base files");
    store
        .connection()
        .execute(
            "DELETE FROM work_views WHERE workspace_id = ?1 AND id = ?2",
            rusqlite::params![lease.workspace_id.as_str(), lease.work_view_id.as_str()],
        )
        .expect("delete work view");
    let work_view_path = lease.work_view_path.clone();
    fs::remove_dir_all(&work_view_path).expect("remove materialization");
    drop(store);

    let status = compose_status(StatusOptions {
        db_path: Some(db_path.clone()),
        requested_path: Some(project_path.display().to_string()),
        workspace_scope: false,
        generated_at: now(),
    })
    .expect("status removes orphaned provisional lease");
    assert!(
        !status
            .items
            .iter()
            .any(|item| item.lease_id == Some(lease.id.clone()))
    );
    assert!(
        MetadataStore::open(&db_path)
            .expect("store")
            .agent_lease_by_id(&lease.id)
            .expect("lease query")
            .is_none()
    );
    assert!(!Path::new(&work_view_path).exists());
}

#[test]
fn truthful_status_reports_attention_items() {
    let (temp, db_path) = seeded_store("agent-lease-truthful-status");
    let project_path = temp.root().join("Code/apps/web");
    let mut lease = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        task: "surface status".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        work_view: true,
        force_stale: false,
        device_id: DeviceId::new("device-test"),
        generated_at: now(),
    })
    .expect("lease created")
    .lease;
    lease.session_state = AgentSessionState::Provisional;
    lease.status_summary = AGENT_LEASE_STATUS_CREATING.to_string();
    MetadataStore::open(&db_path)
        .expect("store")
        .upsert_agent_lease(&lease)
        .expect("lease update");

    let status = invoke_agent_tool(
        Some(db_path),
        tool_request(
            &lease,
            AgentToolName::WorkspaceStatus,
            serde_json::json!({}),
        ),
        now(),
    )
    .expect("status");

    assert_eq!(status.outcome, AgentToolResultOutcome::Allowed);
    let payload = status.payload.expect("status payload");
    let status = payload.get("status").expect("workspace status");
    assert_eq!(
        status.get("level").and_then(serde_json::Value::as_str),
        Some("healthy")
    );
    assert_eq!(
        status
            .get("attentionItems")
            .and_then(serde_json::Value::as_array)
            .map(Vec::len),
        Some(0)
    );
}

#[test]
fn local_daemon_wrapper_ignores_caller_supplied_authority() {
    let (temp, db_path) = seeded_store("agent-lease-mcp-authority");
    let project_path = temp.root().join("Code/apps/web");
    let lease = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        task: "mcp".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        work_view: true,
        force_stale: false,
        device_id: DeviceId::new("device-test"),
        generated_at: now(),
    })
    .expect("lease created")
    .lease;
    let mut request = tool_request(
        &lease,
        AgentToolName::WorkspaceStatus,
        serde_json::json!({}),
    );
    request.authority.transport = AgentToolTransport::LocalDaemon;
    request.authority.peer_credential_checked = true;
    request.authority.nonce_presented = true;

    let result = invoke_agent_tool_from_local_daemon(Some(db_path), request, false, now())
        .expect("tool result");

    assert_eq!(result.outcome, AgentToolResultOutcome::Denied);
    assert_eq!(
        result.denial.expect("denial").code,
        "transport-not-authorized"
    );
}

#[test]
fn scoped_path_expands_home_relative_roots() {
    let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
        return;
    };
    let root = home.join(format!(".bowline-agent-scope-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("root");
    let display_root = format!(
        "~/{}",
        root.strip_prefix(&home).expect("root under home").display()
    );

    let path = super::paths::scoped_path(&display_root, "out.txt").expect("scoped path");
    let _ = fs::remove_dir_all(&root);

    assert_eq!(path, root.join("out.txt"));
}

#[test]
fn expired_lease_denies_tool_execution() {
    let (temp, db_path) = seeded_store("agent-lease-expired");
    let project_path = temp.root().join("Code/apps/web");
    let mut lease = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        task: "expired".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        work_view: true,
        force_stale: false,
        device_id: DeviceId::new("device-test"),
        generated_at: "2026-06-25T10:00:00Z".to_string(),
    })
    .expect("lease created")
    .lease;
    lease.expires_at = "2026-06-25T11:00:00Z".to_string();
    MetadataStore::open(&db_path)
        .expect("store")
        .upsert_agent_lease(&lease)
        .expect("lease update");

    let result = invoke_agent_tool(
        Some(db_path),
        tool_request(
            &lease,
            AgentToolName::WorkspaceStatus,
            serde_json::json!({}),
        ),
        "2026-06-25T12:00:00Z".to_string(),
    )
    .expect("tool result");

    assert_eq!(result.outcome, AgentToolResultOutcome::Denied);
    assert_eq!(result.denial.expect("denial").code, "lease-expired");
}

#[test]
fn blocked_lease_allows_read_only_bridge_tools() {
    let (temp, db_path) = seeded_store("agent-lease-blocked");
    let project_path = temp.root().join("Code/apps/web");
    let mut lease = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        task: "blocked".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        work_view: true,
        force_stale: false,
        device_id: DeviceId::new("device-test"),
        generated_at: now(),
    })
    .expect("lease created")
    .lease;
    lease.session_state = AgentSessionState::Provisional;
    MetadataStore::open(&db_path)
        .expect("store")
        .upsert_agent_lease(&lease)
        .expect("lease update");

    // A provisional session still answers the read-only bridge; every surviving MCP
    // tool is read-only and stays available so an orchestrator can inspect the
    // blocked workspace's status.
    let inspect = invoke_agent_tool(
        Some(db_path.clone()),
        tool_request(
            &lease,
            AgentToolName::ListCapabilities,
            serde_json::json!({}),
        ),
        now(),
    )
    .expect("inspect result");
    assert_eq!(inspect.outcome, AgentToolResultOutcome::Allowed);

    let status = invoke_agent_tool(
        Some(db_path),
        tool_request(
            &lease,
            AgentToolName::WorkspaceStatus,
            serde_json::json!({}),
        ),
        now(),
    )
    .expect("status result");
    assert_eq!(status.outcome, AgentToolResultOutcome::Allowed);
}

#[test]
fn denied_tool_reports_event_append_failure() {
    let (temp, db_path) = seeded_store("agent-lease-denial-event-fail");
    let project_path = temp.root().join("Code/apps/web");
    let mut lease = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        task: "denial".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        work_view: true,
        force_stale: false,
        device_id: DeviceId::new("device-test"),
        generated_at: now(),
    })
    .expect("lease created")
    .lease;
    lease.expires_at = "2026-06-25T11:00:00Z".to_string();
    let store = MetadataStore::open(&db_path).expect("store");
    store.upsert_agent_lease(&lease).expect("lease update");
    store
        .connection()
        .execute(
            "CREATE TRIGGER fail_denial_event
                 BEFORE INSERT ON events
                 BEGIN
                   SELECT RAISE(FAIL, 'forced denial event failure');
                 END",
            [],
        )
        .expect("event failure trigger");
    drop(store);

    let result = invoke_agent_tool(
        Some(db_path),
        tool_request(
            &lease,
            AgentToolName::WorkspaceStatus,
            serde_json::json!({}),
        ),
        "2026-06-25T12:00:00Z".to_string(),
    )
    .expect("denied tool result should not append an event");

    assert_eq!(result.outcome, AgentToolResultOutcome::Denied);
    assert_eq!(
        result.denial.as_ref().map(|denial| denial.code.as_str()),
        Some("lease-expired")
    );
    assert_eq!(result.event_id, None);
}

#[test]
fn latest_main_lease_requires_git_observer_base() {
    let (temp, db_path) = seeded_store("agent-lease-latest-main");
    let project_path = temp.root().join("Code/apps/web");

    let error = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path),
        project_path: project_path.display().to_string(),
        task: "main".to_string(),
        base: AgentLeaseBase::LatestMain,
        work_view: true,
        force_stale: false,
        device_id: DeviceId::new("device-test"),
        generated_at: now(),
    })
    .expect_err("latest:main should fail closed without observer state");

    assert!(
        error
            .to_string()
            .contains("latest:main base is unavailable")
    );
}

pub(super) fn tool_request(
    lease: &AgentLease,
    tool: AgentToolName,
    arguments: serde_json::Value,
) -> AgentToolInvokeRequest {
    let request_suffix =
        stable_token(&serde_json::to_string(&arguments).unwrap_or_else(|_| "{}".to_string()));
    AgentToolInvokeRequest {
        message_type: "agent.tool.invoke".to_string(),
        protocol_version: CONTRACT_VERSION,
        request_id: format!("req_{tool:?}_{request_suffix}"),
        lease_id: lease.id.clone(),
        tool,
        authority: AgentToolAuthority {
            transport: AgentToolTransport::LocalDaemon,
            peer_credential_checked: true,
            nonce_presented: false,
            mcp_token_file: None,
        },
        arguments: match arguments {
            serde_json::Value::Object(map) => map,
            _ => serde_json::Map::new(),
        },
    }
}

fn mcp_tool_request(
    lease: &AgentLease,
    tool: AgentToolName,
    arguments: serde_json::Value,
    token_file: &str,
) -> AgentToolInvokeRequest {
    let mut request = tool_request(lease, tool, arguments);
    request.authority = AgentToolAuthority {
        transport: AgentToolTransport::McpAdapter,
        peer_credential_checked: true,
        nonce_presented: true,
        mcp_token_file: Some(token_file.to_string()),
    };
    request
}

pub(super) fn seeded_store(name: &str) -> (TempWorkspace, std::path::PathBuf) {
    let temp = TempWorkspace::new(name).expect("temp workspace");
    let code_root = temp.root().join("Code");
    fs::create_dir_all(code_root.join("apps/web")).expect("project dir");
    let db_path = temp.root().join(".state/local.sqlite3");
    let store = MetadataStore::open(&db_path).expect("metadata");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_id = ProjectId::new("proj_web");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-06-25T00:00:00Z")
        .expect("workspace");
    store
        .insert_root(
            "root_code",
            &workspace_id,
            &code_root.display().to_string(),
            "2026-06-25T00:00:00Z",
        )
        .expect("root");
    store
        .insert_project(
            &project_id,
            &workspace_id,
            "root_code",
            "apps/web",
            "2026-06-25T00:00:00Z",
        )
        .expect("project");
    store
        .set_project_latest_snapshot_id(
            &workspace_id,
            &project_id,
            &SnapshotId::new("snap_project_base"),
        )
        .expect("snapshot");
    (temp, db_path)
}

pub(super) fn now() -> String {
    "2026-06-25T12:00:00Z".to_string()
}
