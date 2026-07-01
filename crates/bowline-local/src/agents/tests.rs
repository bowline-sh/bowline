use bowline_core::{
    commands::{AgentToolAuthority, AgentToolInvokeRequest, AgentToolTransport},
    ids::{ContentId, DeviceId, ProjectId, SnapshotId, WorkspaceId},
    work_views::WorkViewLifecycle,
    workspace_graph::{ContentLocator, ContentStorage, HydrationState, NamespaceEntryKind},
};
use serde_json::Map;

use crate::{
    metadata::{MetadataStore, ProjectedNodeRecord, SetupReceiptRecord},
    status::{StatusOptions, compose_status},
    workspace::TempWorkspace,
};

use super::*;

#[test]
fn default_lease_binds_directly_to_real_project_without_work_view() {
    let (temp, db_path) = seeded_store("agent-lease-direct");
    let project_path = temp.root().join("Code/apps/web");

    let output = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        task: "fix auth routing".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        hydrate_budget_bytes: 1024 * 1024,
        work_view: false,
        device_id: DeviceId::new("device-test"),
        generated_at: now(),
    })
    .expect("direct lease created");

    assert_eq!(output.lease.write_target_mode, AgentWriteTargetMode::Direct);
    assert_eq!(
        output.lease.output_target.kind,
        AgentOutputTargetKind::RealProject
    );
    assert_eq!(output.lease.output_target.work_view_id, None);
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
    for work_view_only_tool in [
        AgentToolName::PublishOverlayForReview,
        AgentToolName::ListOverlayChanges,
        AgentToolName::DiffSnapshots,
    ] {
        assert!(
            !context
                .context
                .capabilities
                .iter()
                .any(|capability| capability.name == work_view_only_tool),
            "direct agent context must not advertise {work_view_only_tool:?}"
        );
    }

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
    for work_view_only_tool in [
        AgentToolName::PublishOverlayForReview,
        AgentToolName::ListOverlayChanges,
        AgentToolName::DiffSnapshots,
    ] {
        let serialized_name =
            serde_json::to_value(work_view_only_tool).expect("tool name serializes");
        assert!(
            !listed_capabilities
                .iter()
                .any(|capability| capability.get("name") == Some(&serialized_name)),
            "direct list_capabilities must not advertise {work_view_only_tool:?}"
        );
    }

    let write = invoke_agent_tool(
        Some(db_path.clone()),
        tool_request(
            &output.lease,
            AgentToolName::WriteOverlayFile,
            serde_json::json!({"path": "README.md", "contents": "direct edit\n"}),
        ),
        now(),
    )
    .expect("direct write");
    assert_eq!(write.outcome, AgentToolResultOutcome::Allowed);
    assert_eq!(
        fs::read_to_string(project_path.join("README.md")).expect("direct file"),
        "direct edit\n"
    );

    let complete = invoke_agent_tool(
        Some(db_path),
        tool_request(
            &output.lease,
            AgentToolName::CompleteTask,
            serde_json::json!({}),
        ),
        now(),
    )
    .expect("direct complete");
    assert_eq!(complete.outcome, AgentToolResultOutcome::Allowed);
}

#[test]
fn hydration_budget_grant_unblocks_exhausted_agent_lease() {
    let (temp, db_path) = seeded_store("agent-budget-grant");
    let project_path = temp.root().join("Code/apps/web");
    let lease = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        task: "inspect cold files".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        hydrate_budget_bytes: 1,
        work_view: false,
        device_id: DeviceId::new("device-test"),
        generated_at: now(),
    })
    .expect("lease created")
    .lease;

    let mut store = MetadataStore::open(&db_path).expect("store");
    let denied = reserve_lease_bytes(
        &mut store,
        HydrationBudgetReservationRequest {
            workspace_id: &lease.workspace_id,
            project_id: &lease.project_id,
            lease_id: &lease.id,
            path: "src/cold.ts",
            content_id: Some("cid_cold"),
            cause: "agent-read",
            requested_bytes: 2,
            limit_bytes: lease.hydrate_budget_bytes,
            now: &now(),
        },
    )
    .expect("budget reservation");
    assert!(!denied.accepted);
    assert_eq!(
        denied.status.state,
        bowline_core::commands::HydrationBudgetState::Exhausted
    );
    assert_eq!(
        denied.status.next_action.and_then(|action| action.command),
        Some(format!(
            "bowline agent budget --lease {} --add 64MiB",
            lease.id.as_str()
        ))
    );
    drop(store);

    let grant = grant_agent_hydration_budget(AgentBudgetGrantOptions {
        db_path: Some(db_path.clone()),
        lease_id: lease.id.clone(),
        add_bytes: 4,
        generated_at: "2026-06-25T12:00:01Z".to_string(),
    })
    .expect("budget grant");
    assert_eq!(grant.previous_limit_bytes, 1);
    assert_eq!(grant.budget.limit_bytes, 5);
    assert_eq!(grant.budget.remaining_bytes, 5);

    let mut store = MetadataStore::open(&db_path).expect("store");
    let accepted = reserve_lease_bytes(
        &mut store,
        HydrationBudgetReservationRequest {
            workspace_id: &lease.workspace_id,
            project_id: &lease.project_id,
            lease_id: &lease.id,
            path: "src/cold.ts",
            content_id: Some("cid_cold"),
            cause: "agent-read",
            requested_bytes: 2,
            limit_bytes: grant.budget.limit_bytes,
            now: "2026-06-25T12:00:02Z",
        },
    )
    .expect("budget reservation after grant");
    assert!(accepted.accepted);

    let events = store.list_events(20).expect("events");
    assert!(events.iter().any(|event| {
        event.name == EventName::HydrationBudgetDenied && event.lease_id == Some(lease.id.clone())
    }));
    assert!(events.iter().any(|event| {
        event.name == EventName::HydrationBudgetOverrideGranted
            && event.lease_id == Some(lease.id.clone())
    }));
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
            hydrate_budget_bytes: 1024 * 1024,
            work_view: true,
            device_id: DeviceId::new("device-test"),
            generated_at: now(),
        })
        .expect("lease created");

    assert_eq!(
        output.lease.execution_state,
        AgentLeaseExecutionState::Active
    );
    assert_eq!(output.lease.output_state, AgentLeaseOutputState::Empty);
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
        capability.name == AgentToolName::WriteOverlayFile
            && capability.state == AgentCapabilityState::Available
    }));

    let prompt = agent_prompt(AgentLeaseSelectorOptions {
        db_path: Some(db_path),
        lease_id: output.lease.id.clone(),
        generated_at: now(),
    })
    .expect("prompt");
    assert!(prompt.prompt.text.contains(task_marker));
    assert!(prompt.prompt.text.contains("bowline agent publish --lease"));
    assert!(!prompt.prompt.text.contains("~/.local/bin/bowline"));
    assert!(
        !prompt
            .prompt
            .text
            .contains("sk_live_abcdefghijklmnopqrstuvwxyz")
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
        hydrate_budget_bytes: 1024 * 1024,
        work_view: true,
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
        hydrate_budget_bytes: 1024 * 1024,
        work_view: true,
        device_id: DeviceId::new("device-test"),
        generated_at: now(),
    })
    .expect("lease created")
    .lease;

    lease.execution_state = AgentLeaseExecutionState::Blocked;
    lease.status_summary = "creating".to_string();
    let store = MetadataStore::open(&db_path).expect("store");
    store
        .connection()
        .execute(
            "DELETE FROM events WHERE id = ?1",
            rusqlite::params![lease.audit.local_event_id.as_str()],
        )
        .expect("delete creation event");
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
    assert_eq!(stored.execution_state, AgentLeaseExecutionState::Active);
    assert_eq!(stored.status_summary, "active");
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
        hydrate_budget_bytes: 1024 * 1024,
        work_view: true,
        device_id: DeviceId::new("device-test"),
        generated_at: now(),
    })
    .expect("lease created")
    .lease;
    lease.execution_state = AgentLeaseExecutionState::Blocked;
    lease.status_summary = "creating".to_string();

    let store = MetadataStore::open(&db_path).expect("store");
    store.upsert_agent_lease(&lease).expect("provisional lease");
    store
        .connection()
        .execute(
            "DELETE FROM work_view_base_files WHERE workspace_id = ?1 AND work_view_id = ?2",
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
fn tool_write_publish_and_complete_update_lease_and_status() {
    let (temp, db_path) = seeded_store("agent-lease-tools");
    let project_path = temp.root().join("Code/apps/web");
    let lease = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        task: "add readme".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        hydrate_budget_bytes: 1024 * 1024,
        work_view: true,
        device_id: DeviceId::new("device-test"),
        generated_at: now(),
    })
    .expect("lease created")
    .lease;

    let write = invoke_agent_tool(
        Some(db_path.clone()),
        tool_request(
            &lease,
            AgentToolName::WriteOverlayFile,
            serde_json::json!({"path": "README.md", "contents": "# Hello\n"}),
        ),
        now(),
    )
    .expect("write");
    assert_eq!(write.outcome, AgentToolResultOutcome::Allowed);
    assert!(write.event_id.is_some());
    assert!(Path::new(&lease.work_view_path).join("README.md").is_file());
    let events = MetadataStore::open(&db_path)
        .expect("store")
        .list_events(20)
        .expect("events");
    assert!(events.iter().any(|event| {
        event.name == EventName::OverlayChanged && event.lease_id == Some(lease.id.clone())
    }));
    let writes = MetadataStore::open(&db_path)
        .expect("store")
        .local_write_log(&lease.workspace_id)
        .expect("write log");
    assert!(writes.iter().any(|write| {
        write.project_id.as_ref() == Some(&lease.project_id)
            && write.path.ends_with("README.md")
            && write.operation == "create"
            && write.causation_id.starts_with("req_WriteOverlayFile_")
    }));

    let publish = invoke_agent_tool(
        Some(db_path.clone()),
        tool_request(
            &lease,
            AgentToolName::PublishOverlayForReview,
            serde_json::json!({}),
        ),
        now(),
    )
    .expect("publish");
    assert_eq!(publish.outcome, AgentToolResultOutcome::Allowed);

    let store = MetadataStore::open(&db_path).expect("store");
    let stored = store
        .agent_lease_by_id(&lease.id)
        .expect("lease query")
        .expect("lease stored");
    assert_eq!(stored.output_state, AgentLeaseOutputState::ReviewReady);
    let work_view = store
        .work_view_by_id(&lease.workspace_id, &lease.work_view_id)
        .expect("work view query")
        .expect("work view stored");
    assert_eq!(work_view.lifecycle, WorkViewLifecycle::ReviewReady);
    drop(store);

    let status = compose_status(StatusOptions {
        db_path: Some(db_path.clone()),
        requested_path: Some(project_path.display().to_string()),
        workspace_scope: false,
        generated_at: now(),
    })
    .expect("status");
    assert!(
        status
            .items
            .iter()
            .any(|item| item.lease_id == Some(lease.id.clone()))
    );
    let context = agent_context(AgentLeaseSelectorOptions {
        db_path: Some(db_path.clone()),
        lease_id: lease.id.clone(),
        generated_at: now(),
    })
    .expect("context");
    assert_eq!(context.context.status.level, StatusLevel::Attention);
    assert!(context.context.status.needs_attention());
    assert!(
        context
            .context
            .attention
            .iter()
            .any(|item| item.event_name == Some(EventName::LeaseReviewReady))
    );

    let complete = invoke_agent_tool(
        Some(db_path.clone()),
        tool_request(&lease, AgentToolName::CompleteTask, serde_json::json!({})),
        now(),
    )
    .expect("complete");
    assert_eq!(complete.outcome, AgentToolResultOutcome::Allowed);
}

#[test]
fn agent_journey_uses_setup_prompt_overlay_and_review_without_touching_project() {
    let (temp, db_path) = seeded_store("agent-lease-journey");
    let project_path = temp.root().join("Code/apps/web");
    let project_readme = project_path.join("README.md");
    fs::write(&project_readme, "# Original\n").expect("project readme");

    let lease = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        task: "update README without leaking API_KEY=secret-token".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        hydrate_budget_bytes: 1024 * 1024,
        work_view: true,
        device_id: DeviceId::new("device-test"),
        generated_at: now(),
    })
    .expect("lease created")
    .lease;

    MetadataStore::open(&db_path)
        .expect("store")
        .upsert_setup_receipt(&setup_receipt(&lease, "echo ok", "approved"))
        .expect("approved receipt");

    let context = agent_context(AgentLeaseSelectorOptions {
        db_path: Some(db_path.clone()),
        lease_id: lease.id.clone(),
        generated_at: now(),
    })
    .expect("context");
    assert_eq!(context.context.start_work.cwd, lease.work_view_path);
    assert_eq!(context.context.setup_receipts.len(), 1);
    let setup_signal = context
        .context
        .readiness
        .signals
        .iter()
        .find(|signal| signal.name == "setup-receipts")
        .expect("setup readiness signal");
    assert_eq!(setup_signal.state, AgentReadinessState::Ready);
    assert!(setup_signal.summary.contains("1 setup receipt"));
    let context_json = serde_json::to_string(&context).expect("context serializes");
    assert!(!context_json.contains("secret-token"));

    let prompt = agent_prompt(AgentLeaseSelectorOptions {
        db_path: Some(db_path.clone()),
        lease_id: lease.id.clone(),
        generated_at: now(),
    })
    .expect("prompt");
    assert_eq!(prompt.prompt.redaction, AgentPromptRedaction::Applied);
    assert!(prompt.prompt.text.contains("bowline agent task"));
    assert!(prompt.prompt.text.contains(&lease.work_view_path));
    assert!(
        prompt
            .prompt
            .allowed_tools
            .contains(&AgentToolName::PublishOverlayForReview)
    );

    let setup_run = invoke_agent_tool(
        Some(db_path.clone()),
        tool_request(
            &lease,
            AgentToolName::RunCommandWithReceipt,
            serde_json::json!({"command": "echo ok"}),
        ),
        now(),
    )
    .expect("setup command");
    assert_eq!(setup_run.outcome, AgentToolResultOutcome::Allowed);
    assert!(setup_run.receipt_id.is_some());

    let write = invoke_agent_tool(
        Some(db_path.clone()),
        tool_request(
            &lease,
            AgentToolName::WriteOverlayFile,
            serde_json::json!({"path": "README.md", "contents": "# Agent edit\n"}),
        ),
        now(),
    )
    .expect("write");
    assert_eq!(write.outcome, AgentToolResultOutcome::Allowed);
    assert_eq!(
        fs::read_to_string(&project_readme).expect("main project readme"),
        "# Original\n"
    );
    assert_eq!(
        fs::read_to_string(Path::new(&lease.work_view_path).join("README.md"))
            .expect("work view readme"),
        "# Agent edit\n"
    );

    let publish = invoke_agent_tool(
        Some(db_path.clone()),
        tool_request(
            &lease,
            AgentToolName::PublishOverlayForReview,
            serde_json::json!({}),
        ),
        now(),
    )
    .expect("publish");
    assert_eq!(publish.outcome, AgentToolResultOutcome::Allowed);

    let status = compose_status(StatusOptions {
        db_path: Some(db_path.clone()),
        requested_path: Some(project_path.display().to_string()),
        workspace_scope: false,
        generated_at: now(),
    })
    .expect("status");
    assert!(
        status
            .items
            .iter()
            .any(|item| item.event_name == Some(EventName::LeaseReviewReady))
    );

    let store = MetadataStore::open(&db_path).expect("store");
    let stored = store
        .agent_lease_by_id(&lease.id)
        .expect("lease query")
        .expect("lease stored");
    assert_eq!(stored.output_state, AgentLeaseOutputState::ReviewReady);
    let writes = store
        .local_write_log(&lease.workspace_id)
        .expect("write log");
    assert!(writes.iter().any(|write| {
        write.project_id.as_ref() == Some(&lease.project_id)
            && write.path.ends_with("README.md")
            && matches!(write.operation.as_str(), "create" | "modify" | "update")
            && write.causation_id.starts_with("req_WriteOverlayFile_")
    }));
}

#[test]
fn run_command_requires_completed_or_approved_setup_receipt() {
    let (temp, db_path) = seeded_store("agent-lease-command-receipt-state");
    let project_path = temp.root().join("Code/apps/web");
    let lease = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        task: "run setup command".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        hydrate_budget_bytes: 1024 * 1024,
        work_view: true,
        device_id: DeviceId::new("device-test"),
        generated_at: now(),
    })
    .expect("lease created")
    .lease;
    let store = MetadataStore::open(&db_path).expect("store");
    store
        .upsert_setup_receipt(&setup_receipt(&lease, "echo ok", "failed"))
        .expect("failed receipt");
    drop(store);

    let denied = invoke_agent_tool(
        Some(db_path.clone()),
        tool_request(
            &lease,
            AgentToolName::RunCommandWithReceipt,
            serde_json::json!({"command": "echo ok"}),
        ),
        now(),
    )
    .expect("command denied");
    assert_eq!(denied.denial.expect("denial").code, "command-not-declared");

    MetadataStore::open(&db_path)
        .expect("store")
        .upsert_setup_receipt(&setup_receipt(&lease, "echo ok", "completed"))
        .expect("completed receipt");
    let allowed = invoke_agent_tool(
        Some(db_path),
        tool_request(
            &lease,
            AgentToolName::RunCommandWithReceipt,
            serde_json::json!({"command": "echo ok"}),
        ),
        now(),
    )
    .expect("command allowed");
    assert_eq!(allowed.outcome, AgentToolResultOutcome::Allowed);
}

#[test]
fn write_overlay_rolls_back_file_log_and_lease_when_audit_event_fails() {
    let (temp, db_path) = seeded_store("agent-lease-write-audit-rollback");
    let project_path = temp.root().join("Code/apps/web");
    let lease = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        task: "write rollback".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        hydrate_budget_bytes: 1024 * 1024,
        work_view: true,
        device_id: DeviceId::new("device-test"),
        generated_at: now(),
    })
    .expect("lease created")
    .lease;
    let store = MetadataStore::open(&db_path).expect("store");
    store
        .connection()
        .execute(
            "CREATE TRIGGER fail_overlay_audit
                 BEFORE INSERT ON events
                 WHEN NEW.name = 'lease.tool_invoked' OR NEW.name = 'overlay.changed'
                 BEGIN
                   SELECT RAISE(FAIL, 'forced overlay audit failure');
                 END",
            [],
        )
        .expect("event failure trigger");
    drop(store);

    let error = invoke_agent_tool(
        Some(db_path.clone()),
        tool_request(
            &lease,
            AgentToolName::WriteOverlayFile,
            serde_json::json!({"path": "ROLLBACK.md", "contents": "do not keep"}),
        ),
        now(),
    )
    .expect_err("audit failure should fail the write");
    assert!(matches!(error, AgentError::Event(_)));
    assert!(
        !Path::new(&lease.work_view_path)
            .join("ROLLBACK.md")
            .exists()
    );

    let store = MetadataStore::open(&db_path).expect("store");
    assert!(
        store
            .local_write_log(&lease.workspace_id)
            .expect("write log")
            .iter()
            .all(|write| !write.path.ends_with("ROLLBACK.md"))
    );
    let stored = store
        .agent_lease_by_id(&lease.id)
        .expect("lease query")
        .expect("lease");
    assert_eq!(stored.output_state, AgentLeaseOutputState::Empty);
}

#[test]
fn publish_for_review_rolls_back_when_event_append_fails() {
    let (temp, db_path) = seeded_store("agent-lease-publish-rollback");
    let project_path = temp.root().join("Code/apps/web");
    let lease = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        task: "publish rollback".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        hydrate_budget_bytes: 1024 * 1024,
        work_view: true,
        device_id: DeviceId::new("device-test"),
        generated_at: now(),
    })
    .expect("lease created")
    .lease;
    let store = MetadataStore::open(&db_path).expect("store");
    store
        .connection()
        .execute(
            "CREATE TRIGGER fail_publish_event
                 BEFORE INSERT ON events
                 BEGIN
                   SELECT RAISE(FAIL, 'forced publish event failure');
                 END",
            [],
        )
        .expect("event failure trigger");
    drop(store);

    let error = invoke_agent_tool(
        Some(db_path.clone()),
        tool_request(
            &lease,
            AgentToolName::PublishOverlayForReview,
            serde_json::json!({}),
        ),
        now(),
    )
    .expect_err("publish event failure should be reported");
    assert!(matches!(error, AgentError::Event(_)));

    let store = MetadataStore::open(&db_path).expect("store");
    let stored_lease = store
        .agent_lease_by_id(&lease.id)
        .expect("lease query")
        .expect("lease");
    assert_eq!(stored_lease.output_state, AgentLeaseOutputState::Empty);
    let work_view = store
        .work_view_by_id(&lease.workspace_id, &lease.work_view_id)
        .expect("work view query")
        .expect("work view");
    assert_eq!(work_view.lifecycle, WorkViewLifecycle::Active);
}

#[test]
fn search_workspace_returns_index_backed_payload_for_lease_scope() {
    let (temp, db_path) = seeded_store("agent-lease-degraded");
    let project_path = temp.root().join("Code/apps/web");
    let lease = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        task: "search".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        hydrate_budget_bytes: 1024 * 1024,
        work_view: true,
        device_id: DeviceId::new("device-test"),
        generated_at: now(),
    })
    .expect("lease created")
    .lease;

    let result = invoke_agent_tool(
        Some(db_path.clone()),
        tool_request(
            &lease,
            AgentToolName::SearchWorkspace,
            serde_json::json!({"query": "auth"}),
        ),
        now(),
    )
    .expect("search");
    assert_eq!(result.outcome, AgentToolResultOutcome::Allowed);
    let payload = result.payload.expect("search payload");
    assert_eq!(
        payload.get("command").and_then(serde_json::Value::as_str),
        Some("search")
    );
    assert_eq!(
        payload.get("projectId").and_then(serde_json::Value::as_str),
        Some(lease.project_id.as_str())
    );
    assert_eq!(
        payload
            .get("workspaceId")
            .and_then(serde_json::Value::as_str),
        Some(lease.workspace_id.as_str())
    );
    assert!(payload.get("results").is_some());
}

#[test]
fn search_workspace_subpath_returns_lease_relative_paths() {
    let (temp, db_path) = seeded_store("agent-lease-search-subpath");
    let project_path = temp.root().join("Code/apps/web");
    let lease = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        task: "search subpath".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        hydrate_budget_bytes: 1024 * 1024,
        work_view: true,
        device_id: DeviceId::new("device-test"),
        generated_at: now(),
    })
    .expect("lease created")
    .lease;
    fs::create_dir_all(Path::new(&lease.work_view_path).join("src")).expect("src dir");
    fs::write(
        Path::new(&lease.work_view_path).join("src/auth.ts"),
        "export function authCallback() {}\n",
    )
    .expect("source");

    let result = invoke_agent_tool(
        Some(db_path.clone()),
        tool_request(
            &lease,
            AgentToolName::SearchWorkspace,
            serde_json::json!({"query": "authCallback", "path": "src"}),
        ),
        now(),
    )
    .expect("search");

    let payload = result.payload.expect("search payload");
    assert_eq!(payload["results"][0]["path"].as_str(), Some("src/auth.ts"));
}

#[test]
fn search_workspace_subpath_keeps_project_root_policy_for_work_view_files() {
    let (temp, db_path) = seeded_store("agent-lease-search-subpath-policy");
    let project_path = temp.root().join("Code/apps/web");
    fs::write(project_path.join(".bowlineignore"), b"private/**\n").expect("policy");
    let lease = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        task: "search private subpath".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        hydrate_budget_bytes: 1024 * 1024,
        work_view: true,
        device_id: DeviceId::new("device-test"),
        generated_at: now(),
    })
    .expect("lease created")
    .lease;
    fs::create_dir_all(Path::new(&lease.work_view_path).join("private")).expect("private dir");
    fs::write(
        Path::new(&lease.work_view_path).join("private/token.txt"),
        "hiddenNeedle\n",
    )
    .expect("hidden file");

    let result = invoke_agent_tool(
        Some(db_path.clone()),
        tool_request(
            &lease,
            AgentToolName::SearchWorkspace,
            serde_json::json!({"query": "hiddenNeedle", "path": "private"}),
        ),
        now(),
    )
    .expect("search");

    assert_eq!(result.outcome, AgentToolResultOutcome::Allowed);
    let payload = result.payload.expect("search payload");
    assert_eq!(payload["results"].as_array().expect("results").len(), 0);
}

#[test]
fn search_workspace_respects_lease_file_bound_before_indexing() {
    let (temp, db_path) = seeded_store("agent-lease-search-file-bound");
    let project_path = temp.root().join("Code/apps/web");
    let mut lease = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        task: "bounded search".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        hydrate_budget_bytes: 1024 * 1024,
        work_view: true,
        device_id: DeviceId::new("device-test"),
        generated_at: now(),
    })
    .expect("lease created")
    .lease;
    lease.scopes.read.max_files_per_request = Some(1);
    let store = MetadataStore::open(&db_path).expect("store");
    store.upsert_agent_lease(&lease).expect("lease update");
    drop(store);
    fs::create_dir_all(Path::new(&lease.work_view_path).join("src")).expect("src dir");
    fs::write(
        Path::new(&lease.work_view_path).join("src/a.ts"),
        "export const boundedNeedle = 1;\n",
    )
    .expect("source a");
    fs::write(
        Path::new(&lease.work_view_path).join("src/b.ts"),
        "export const boundedNeedle = 2;\n",
    )
    .expect("source b");

    let result = invoke_agent_tool(
        Some(db_path.clone()),
        tool_request(
            &lease,
            AgentToolName::SearchWorkspace,
            serde_json::json!({"query": "boundedNeedle"}),
        ),
        now(),
    )
    .expect("search");

    assert_eq!(result.outcome, AgentToolResultOutcome::Allowed);
    let payload = result.payload.expect("search payload");
    assert_eq!(payload["results"].as_array().expect("results").len(), 1);
    assert_eq!(payload["index"]["state"].as_str(), Some("stale"));
}

#[test]
fn request_hydration_records_queue_entry_and_budget_reservation() {
    let (temp, db_path) = seeded_store("agent-lease-hydration-queue");
    let project_path = temp.root().join("Code/apps/web");
    let lease = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        task: "hydrate".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        hydrate_budget_bytes: 1024,
        work_view: true,
        device_id: DeviceId::new("device-test"),
        generated_at: now(),
    })
    .expect("lease created")
    .lease;
    let cold_path = Path::new(&lease.work_view_path).join("cold.rs");
    fs::write(&cold_path, "fn cold() {}\n").expect("fixture file");
    let cold_len = fs::metadata(&cold_path).expect("metadata").len();

    let denied = invoke_agent_tool(
        Some(db_path.clone()),
        tool_request(
            &lease,
            AgentToolName::RequestHydration,
            serde_json::json!({"path": "cold.rs", "bytes": 0, "contentId": "cid_cold"}),
        ),
        now(),
    )
    .expect("hydration denial");
    assert_eq!(denied.outcome, AgentToolResultOutcome::Denied);
    assert_eq!(
        denied.denial.as_ref().map(|denial| denial.code.as_str()),
        Some("content-id-unverified")
    );

    let result = invoke_agent_tool(
        Some(db_path.clone()),
        tool_request(
            &lease,
            AgentToolName::RequestHydration,
            serde_json::json!({"path": "cold.rs", "bytes": 0}),
        ),
        now(),
    )
    .expect("hydration request");

    assert_eq!(result.outcome, AgentToolResultOutcome::Allowed);
    assert_eq!(
        result.payload.as_ref().unwrap()["state"].as_str(),
        Some("completed")
    );
    let store = MetadataStore::open(&db_path).expect("store");
    let queue = store.hydration_queue(&lease.workspace_id).expect("queue");
    assert_eq!(queue.len(), 1);
    assert!(queue[0].path.ends_with("/cold.rs"));
    assert_eq!(queue[0].priority, "agent-lease");
    assert_eq!(queue[0].state, "completed");
    assert_eq!(queue[0].content_id, None);
    let budget = crate::hydration_budget::lease_budget_status(
        &store,
        &lease.workspace_id,
        &lease.project_id,
        &lease.id,
        lease.hydrate_budget_bytes,
    )
    .expect("budget");
    assert_eq!(budget.used_bytes, cold_len);
    assert_eq!(budget.reserved_bytes, 0);
    drop(store);

    let duplicate = invoke_agent_tool(
        Some(db_path.clone()),
        tool_request(
            &lease,
            AgentToolName::RequestHydration,
            serde_json::json!({"path": "cold.rs", "bytes": 0}),
        ),
        now(),
    )
    .expect("duplicate hydration request");
    assert_eq!(duplicate.outcome, AgentToolResultOutcome::Allowed);
    assert_eq!(duplicate.summary, "hydration request already completed");
    assert!(duplicate.event_id.is_some());

    let store = MetadataStore::open(&db_path).expect("store");
    let queue = store.hydration_queue(&lease.workspace_id).expect("queue");
    assert_eq!(queue.len(), 1);
    let budget = crate::hydration_budget::lease_budget_status(
        &store,
        &lease.workspace_id,
        &lease.project_id,
        &lease.id,
        lease.hydrate_budget_bytes,
    )
    .expect("budget");
    assert_eq!(budget.used_bytes, cold_len);
    assert_eq!(budget.reserved_bytes, 0);
}

#[test]
fn request_hydration_event_failure_releases_budget_and_fails_queue() {
    let (temp, db_path) = seeded_store("agent-lease-hydration-event-failure");
    let project_path = temp.root().join("Code/apps/web");
    let lease = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        task: "hydrate event failure".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        hydrate_budget_bytes: 1024,
        work_view: true,
        device_id: DeviceId::new("device-test"),
        generated_at: now(),
    })
    .expect("lease created")
    .lease;
    let cold_path = Path::new(&lease.work_view_path).join("cold.rs");
    fs::write(&cold_path, "fn cold() {}\n").expect("fixture file");
    let store = MetadataStore::open(&db_path).expect("store");
    store
        .connection()
        .execute(
            "CREATE TRIGGER fail_hydration_event
                 BEFORE INSERT ON events
                 WHEN NEW.name = 'lease.hydration_requested'
                 BEGIN
                   SELECT RAISE(FAIL, 'forced hydration event failure');
                 END",
            [],
        )
        .expect("event failure trigger");
    drop(store);

    let error = invoke_agent_tool(
        Some(db_path.clone()),
        tool_request(
            &lease,
            AgentToolName::RequestHydration,
            serde_json::json!({"path": "cold.rs", "bytes": 0}),
        ),
        now(),
    )
    .expect_err("hydration event failure should fail the request");
    assert!(matches!(error, AgentError::Event(_)));

    let store = MetadataStore::open(&db_path).expect("store");
    let queue = store.hydration_queue(&lease.workspace_id).expect("queue");
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].state, "failed");
    let budget = crate::hydration_budget::lease_budget_status(
        &store,
        &lease.workspace_id,
        &lease.project_id,
        &lease.id,
        lease.hydrate_budget_bytes,
    )
    .expect("budget");
    assert_eq!(budget.reserved_bytes, 0);
}

#[test]
fn request_hydration_queues_cold_projected_file_without_local_bytes() {
    let (temp, db_path) = seeded_store("agent-lease-cold-hydration");
    let project_path = temp.root().join("Code/apps/web");
    let lease = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        task: "hydrate cold".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        hydrate_budget_bytes: 2048,
        work_view: true,
        device_id: DeviceId::new("device-test"),
        generated_at: now(),
    })
    .expect("lease created")
    .lease;
    let content_id = ContentId::new("cid_cold_remote");
    let projected_path = project_path.join("cold-remote.rs");
    let store = MetadataStore::open(&db_path).expect("store");
    store
        .put_content_locator(
            &lease.workspace_id,
            &ContentLocator {
                content_id: content_id.clone(),
                storage: ContentStorage::Inline,
                raw_size: 777,
                pack_id: None,
                offset: None,
                length: None,
                chunk_ids: Vec::new(),
            },
            &now(),
        )
        .expect("locator");
    store
        .upsert_projected_node(&ProjectedNodeRecord {
            workspace_id: lease.workspace_id.clone(),
            node_id: "node_cold_remote".to_string(),
            project_id: Some(lease.project_id.clone()),
            parent_node_id: None,
            path: projected_path.display().to_string(),
            kind: NamespaceEntryKind::File,
            content_id: Some(content_id.clone()),
            hydration_state: HydrationState::Cold,
            updated_at: now(),
        })
        .expect("projected node");
    drop(store);

    let denied = invoke_agent_tool(
        Some(db_path.clone()),
        tool_request(
            &lease,
            AgentToolName::RequestHydration,
            serde_json::json!({"path": "cold-remote.rs", "bytes": 0, "contentId": "cid_old"}),
        ),
        now(),
    )
    .expect("hydration denial");
    assert_eq!(denied.outcome, AgentToolResultOutcome::Denied);
    assert_eq!(
        denied.denial.as_ref().map(|denial| denial.code.as_str()),
        Some("content-id-mismatch")
    );

    let result = invoke_agent_tool(
        Some(db_path.clone()),
        tool_request(
            &lease,
            AgentToolName::RequestHydration,
            serde_json::json!({
                "path": "cold-remote.rs",
                "bytes": 0,
                "contentId": content_id.as_str()
            }),
        ),
        now(),
    )
    .expect("hydration request");

    assert_eq!(result.outcome, AgentToolResultOutcome::Allowed);
    let store = MetadataStore::open(&db_path).expect("store");
    let queue = store.hydration_queue(&lease.workspace_id).expect("queue");
    assert_eq!(queue.len(), 1);
    assert!(queue[0].path.ends_with("/cold-remote.rs"));
    assert_eq!(queue[0].content_id, Some(content_id));
    let budget = crate::hydration_budget::lease_budget_status(
        &store,
        &lease.workspace_id,
        &lease.project_id,
        &lease.id,
        lease.hydrate_budget_bytes,
    )
    .expect("budget");
    assert_eq!(budget.reserved_bytes, 777);
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
        hydrate_budget_bytes: 1024 * 1024,
        work_view: true,
        device_id: DeviceId::new("device-test"),
        generated_at: now(),
    })
    .expect("lease created")
    .lease;
    let mut request = tool_request(
        &lease,
        AgentToolName::WriteOverlayFile,
        serde_json::json!({"path": "README.md", "contents": "nope"}),
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
    assert!(!Path::new(&lease.work_view_path).join("README.md").exists());
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

    let path = scoped_path(&display_root, "out.txt").expect("scoped path");
    let _ = fs::remove_dir_all(&root);

    assert_eq!(path, root.join("out.txt"));
}

#[cfg(unix)]
#[test]
fn tool_paths_reject_symlink_escapes() {
    use std::os::unix::fs::symlink;

    let (temp, db_path) = seeded_store("agent-lease-symlink");
    let project_path = temp.root().join("Code/apps/web");
    let lease = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        task: "symlink".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        hydrate_budget_bytes: 1024 * 1024,
        work_view: true,
        device_id: DeviceId::new("device-test"),
        generated_at: now(),
    })
    .expect("lease created")
    .lease;
    let outside = temp.root().join("outside");
    fs::create_dir_all(&outside).expect("outside dir");
    fs::write(outside.join("secret.txt"), "secret").expect("outside file");
    symlink(&outside, Path::new(&lease.work_view_path).join("escape")).expect("symlink");

    let read = invoke_agent_tool(
        Some(db_path.clone()),
        tool_request(
            &lease,
            AgentToolName::ReadFileAtSnapshot,
            serde_json::json!({"path": "escape/secret.txt"}),
        ),
        now(),
    )
    .expect("read");
    assert_eq!(read.outcome, AgentToolResultOutcome::Denied);

    let write = invoke_agent_tool(
        Some(db_path.clone()),
        tool_request(
            &lease,
            AgentToolName::WriteOverlayFile,
            serde_json::json!({"path": "escape/pwned.txt", "contents": "nope"}),
        ),
        now(),
    )
    .expect("write");
    assert_eq!(write.outcome, AgentToolResultOutcome::Denied);
    assert!(!outside.join("pwned.txt").exists());

    let nested_write = invoke_agent_tool(
        Some(db_path),
        tool_request(
            &lease,
            AgentToolName::WriteOverlayFile,
            serde_json::json!({"path": "escape/new/pwned.txt", "contents": "nope"}),
        ),
        now(),
    )
    .expect("nested write");
    assert_eq!(nested_write.outcome, AgentToolResultOutcome::Denied);
    assert!(!outside.join("new").exists());
}

#[test]
fn write_tool_respects_persisted_write_scope_roots() {
    let (temp, db_path) = seeded_store("agent-lease-write-scope");
    let project_path = temp.root().join("Code/apps/web");
    let mut lease = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        task: "scope".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        hydrate_budget_bytes: 1024 * 1024,
        work_view: true,
        device_id: DeviceId::new("device-test"),
        generated_at: now(),
    })
    .expect("lease created")
    .lease;
    lease.scopes.write.roots = vec![
        Path::new(&lease.work_view_path)
            .join("src")
            .display()
            .to_string(),
    ];
    MetadataStore::open(&db_path)
        .expect("store")
        .upsert_agent_lease(&lease)
        .expect("lease update");

    let denied = invoke_agent_tool(
        Some(db_path.clone()),
        tool_request(
            &lease,
            AgentToolName::WriteOverlayFile,
            serde_json::json!({"path": "README.md", "contents": "nope"}),
        ),
        now(),
    )
    .expect("denied write");
    assert_eq!(denied.outcome, AgentToolResultOutcome::Denied);

    let allowed = invoke_agent_tool(
        Some(db_path),
        tool_request(
            &lease,
            AgentToolName::WriteOverlayFile,
            serde_json::json!({"path": "src/index.ts", "contents": "ok"}),
        ),
        now(),
    )
    .expect("allowed write");
    assert_eq!(allowed.outcome, AgentToolResultOutcome::Allowed);
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
        hydrate_budget_bytes: 1024 * 1024,
        work_view: true,
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
            AgentToolName::WriteOverlayFile,
            serde_json::json!({"path": "README.md", "contents": "nope"}),
        ),
        "2026-06-25T12:00:00Z".to_string(),
    )
    .expect("tool result");

    assert_eq!(result.outcome, AgentToolResultOutcome::Denied);
    assert_eq!(result.denial.expect("denial").code, "lease-expired");
    assert!(!Path::new(&lease.work_view_path).join("README.md").exists());
}

#[test]
fn write_tool_respects_lease_byte_budget() {
    let (temp, db_path) = seeded_store("agent-lease-write-budget");
    let project_path = temp.root().join("Code/apps/web");
    let lease = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        task: "budget".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        hydrate_budget_bytes: 4,
        work_view: true,
        device_id: DeviceId::new("device-test"),
        generated_at: now(),
    })
    .expect("lease created")
    .lease;

    let result = invoke_agent_tool(
        Some(db_path),
        tool_request(
            &lease,
            AgentToolName::WriteOverlayFile,
            serde_json::json!({"path": "README.md", "contents": "hello"}),
        ),
        now(),
    )
    .expect("tool result");

    assert_eq!(result.outcome, AgentToolResultOutcome::Denied);
    assert_eq!(
        result.denial.expect("denial").code,
        "write-exceeds-lease-bounds"
    );
    assert!(!Path::new(&lease.work_view_path).join("README.md").exists());
}

#[test]
fn blocked_lease_allows_inspection_but_denies_mutation() {
    let (temp, db_path) = seeded_store("agent-lease-blocked");
    let project_path = temp.root().join("Code/apps/web");
    let mut lease = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        task: "blocked".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        hydrate_budget_bytes: 1024 * 1024,
        work_view: true,
        device_id: DeviceId::new("device-test"),
        generated_at: now(),
    })
    .expect("lease created")
    .lease;
    lease.execution_state = AgentLeaseExecutionState::Blocked;
    MetadataStore::open(&db_path)
        .expect("store")
        .upsert_agent_lease(&lease)
        .expect("lease update");

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

    let write = invoke_agent_tool(
        Some(db_path),
        tool_request(
            &lease,
            AgentToolName::WriteOverlayFile,
            serde_json::json!({"path": "README.md", "contents": "nope"}),
        ),
        now(),
    )
    .expect("write result");
    assert_eq!(write.outcome, AgentToolResultOutcome::Denied);
    assert_eq!(write.denial.expect("denial").code, "lease-blocked");
    assert!(!Path::new(&lease.work_view_path).join("README.md").exists());
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
        hydrate_budget_bytes: 1024 * 1024,
        work_view: true,
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

    let error = invoke_agent_tool(
        Some(db_path),
        tool_request(
            &lease,
            AgentToolName::WriteOverlayFile,
            serde_json::json!({"path": "README.md", "contents": "nope"}),
        ),
        "2026-06-25T12:00:00Z".to_string(),
    )
    .expect_err("denial event failure should be reported");

    assert!(matches!(error, AgentError::Event(_)));
    assert!(!Path::new(&lease.work_view_path).join("README.md").exists());
}

#[test]
fn read_tools_respect_persisted_read_scope_bounds() {
    let (temp, db_path) = seeded_store("agent-lease-read-bounds");
    let project_path = temp.root().join("Code/apps/web");
    let mut lease = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        task: "bounds".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        hydrate_budget_bytes: 1024 * 1024,
        work_view: true,
        device_id: DeviceId::new("device-test"),
        generated_at: now(),
    })
    .expect("lease created")
    .lease;
    let work_view_path = Path::new(&lease.work_view_path);
    fs::write(work_view_path.join("README.md"), "hello").expect("file");
    fs::create_dir_all(work_view_path.join("src")).expect("src dir");
    fs::write(work_view_path.join("src/index.ts"), "console.log('ok');").expect("nested file");
    lease.scopes.read.max_bytes_per_read = Some(4);
    lease.scopes.read.max_files_per_request = Some(1);
    lease.scopes.read.max_depth = Some(0);
    MetadataStore::open(&db_path)
        .expect("store")
        .upsert_agent_lease(&lease)
        .expect("lease update");

    let read = invoke_agent_tool(
        Some(db_path.clone()),
        tool_request(
            &lease,
            AgentToolName::ReadFileAtSnapshot,
            serde_json::json!({"path": "README.md"}),
        ),
        now(),
    )
    .expect("read");
    assert_eq!(read.outcome, AgentToolResultOutcome::Degraded);
    assert!(read.payload.is_none());
    let degraded = read.degraded.expect("read bounds");
    assert_eq!(degraded.max_bytes, 4);
    assert_eq!(degraded.max_files, 1);
    assert_eq!(degraded.max_depth, 0);

    let tree = invoke_agent_tool(
        Some(db_path),
        tool_request(
            &lease,
            AgentToolName::ListTreeAtSnapshot,
            serde_json::json!({"path": "."}),
        ),
        now(),
    )
    .expect("tree");
    assert_eq!(tree.outcome, AgentToolResultOutcome::Allowed);
    let payload = tree.payload.expect("tree payload");
    let entries = payload
        .get("entries")
        .and_then(serde_json::Value::as_array)
        .expect("entries");
    assert!(entries.len() <= 1);
    let bounds = payload.get("bounds").expect("bounds");
    assert_eq!(bounds["maxBytes"].as_u64(), Some(4));
    assert_eq!(bounds["maxFiles"].as_u64(), Some(1));
    assert_eq!(bounds["maxDepth"].as_u64(), Some(0));
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
        hydrate_budget_bytes: 1024 * 1024,
        work_view: true,
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

#[test]
fn read_tool_denies_project_env_contents_and_audits_denial() {
    let (temp, db_path) = seeded_store("agent-lease-env-read");
    let project_path = temp.root().join("Code/apps/web");
    let lease = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        task: "env".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        hydrate_budget_bytes: 1024 * 1024,
        work_view: true,
        device_id: DeviceId::new("device-test"),
        generated_at: now(),
    })
    .expect("lease created")
    .lease;
    fs::write(
        Path::new(&lease.work_view_path).join(".env.local"),
        "OPENAI_API_KEY=sk-test\n",
    )
    .expect("env file");

    let result = invoke_agent_tool(
        Some(db_path.clone()),
        tool_request(
            &lease,
            AgentToolName::ReadFileAtSnapshot,
            serde_json::json!({"path": ".env.local"}),
        ),
        now(),
    )
    .expect("read");

    assert_eq!(result.outcome, AgentToolResultOutcome::Denied);
    assert!(result.payload.is_none());
    assert!(result.event_id.is_some());
    let tree = invoke_agent_tool(
        Some(db_path.clone()),
        tool_request(
            &lease,
            AgentToolName::ListTreeAtSnapshot,
            serde_json::json!({"path": "."}),
        ),
        now(),
    )
    .expect("tree");
    assert_eq!(tree.outcome, AgentToolResultOutcome::Allowed);
    assert!(
        !serde_json::to_string(&tree.payload)
            .expect("tree payload")
            .contains(".env.local")
    );

    let write = invoke_agent_tool(
        Some(db_path.clone()),
        tool_request(
            &lease,
            AgentToolName::WriteOverlayFile,
            serde_json::json!({"path": ".env.agent", "contents": "TOKEN=secret"}),
        ),
        now(),
    )
    .expect("write");
    assert_eq!(write.outcome, AgentToolResultOutcome::Denied);
    assert!(!Path::new(&lease.work_view_path).join(".env.agent").exists());

    let events = MetadataStore::open(&db_path)
        .expect("store")
        .list_events(20)
        .expect("events");
    assert!(events.iter().any(|event| {
        event.name == EventName::LeaseToolDenied && event.lease_id == Some(lease.id.clone())
    }));
}

fn tool_request(
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
        },
        arguments: match arguments {
            serde_json::Value::Object(map) => map,
            _ => Map::new(),
        },
    }
}

fn setup_receipt(lease: &AgentLease, command: &str, state: &str) -> SetupReceiptRecord {
    SetupReceiptRecord {
        id: format!(
            "setup_{}_{}",
            state,
            stable_token(&format!("{}:{command}", lease.id.as_str()))
        ),
        workspace_id: lease.workspace_id.clone(),
        project_id: Some(lease.project_id.clone()),
        command: command.to_string(),
        state: state.to_string(),
        recipe_hash: stable_token(command),
        approval_state: "approved".to_string(),
        trigger: "agent-test".to_string(),
        cwd: lease.write_target_path.clone(),
        os: "macos".to_string(),
        arch: "aarch64".to_string(),
        env_profile: "default".to_string(),
        output_path: None,
        redacted_summary: "redacted setup receipt".to_string(),
        receipt_json: "{}".to_string(),
        updated_at: now(),
    }
}

fn seeded_store(name: &str) -> (TempWorkspace, std::path::PathBuf) {
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

fn now() -> String {
    "2026-06-25T12:00:00Z".to_string()
}
