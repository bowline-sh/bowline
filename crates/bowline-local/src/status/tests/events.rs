use super::*;

#[test]
fn unmapped_non_info_events_emit_canonical_fallback_facts() {
    let workspace_id = WorkspaceId::new("ws_code");
    let attention_event = WorkspaceEvent::new(
        EventId::new("evt_unmapped_attention"),
        EventName::Unknown("namespace.attention".to_string()),
        "2026-07-12T00:00:00Z",
        EventSeverity::Attention,
        "Namespace needs attention.",
        workspace_id.clone(),
    );
    let limited_event = WorkspaceEvent::new(
        EventId::new("evt_unmapped_limited"),
        EventName::Unknown("namespace.limited".to_string()),
        "2026-07-12T00:00:00Z",
        EventSeverity::Limited,
        "Namespace capability is limited.",
        workspace_id,
    );
    let mut acc = StatusAccumulator::new("2026-07-12T00:00:01Z");

    apply_event_status(&attention_event, &mut acc);
    apply_event_status(&limited_event, &mut acc);
    let summary = reduce_status_facts(acc.facts, 7, "2026-07-12T00:00:01Z");

    assert_eq!(summary.availability, StatusAvailability::Degraded);
    assert_eq!(summary.attention, StatusAttention::Required);
    assert_eq!(summary.facts.len(), 2);
}

#[test]
fn component_event_workspace_fact_names_the_event_workspace() {
    let workspace_id = WorkspaceId::new("ws_component_status");
    let mut event = WorkspaceEvent::new(
        EventId::new("evt_sync_degraded"),
        EventName::SyncDegraded,
        "2026-07-12T00:00:00Z",
        EventSeverity::Attention,
        "Continuous sync is degraded.",
        workspace_id.clone(),
    );
    event.subject = Some(EventSubject {
        kind: EventSubjectKind::Component,
        id: "sync".to_string(),
        path: None,
    });
    let mut acc = StatusAccumulator::new("2026-07-12T00:00:01Z");

    apply_event_status(&event, &mut acc);

    assert_eq!(acc.facts.len(), 1);
    assert_eq!(acc.facts[0].scope, StatusFactScope::Workspace);
    assert_eq!(
        acc.facts[0].scope_id.as_deref(),
        Some(workspace_id.as_str())
    );
}

#[test]
fn corrupt_metadata_event_requires_attention_and_events_command_lists_it() {
    let temp = TempWorkspace::new("status-events").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_code");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-06-23T12:00:00Z")
        .expect("workspace insert");
    store
        .insert_root("root_code", &workspace_id, "~/Code", "2026-06-23T12:00:00Z")
        .expect("root insert");
    store
        .append_event(WorkspaceEvent::new(
            EventId::new("evt_status_001"),
            EventName::MetadataCorrupt,
            "2026-06-23T12:00:00Z",
            EventSeverity::Limited,
            "Local metadata needs inspection.",
            workspace_id,
        ))
        .expect("event append");

    let output = compose_status(StatusOptions {
        db_path: Some(db_path.clone()),
        requested_path: None,
        workspace_scope: true,
        generated_at: "2026-06-23T12:00:00Z".to_string(),
    })
    .expect("status composes");
    assert_eq!(output.status.level, StatusLevel::Attention);
    assert_eq!(
        output.event_watermarks.last_event_id.unwrap().as_str(),
        "evt_status_001"
    );

    let events = compose_events(EventsOptions {
        db_path: Some(db_path),
        requested_path: None,
        workspace_scope: true,
        generated_at: "2026-06-23T12:00:00Z".to_string(),
        limit: 10,
    })
    .expect("events compose");
    assert_eq!(events.events.len(), 1);
}

#[test]
fn human_events_render_serialized_event_names() {
    let temp = TempWorkspace::new("events-human-label").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_code");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    seed_workspace_root(&store, &workspace_id);
    store
        .append_event(WorkspaceEvent::new(
            EventId::new("evt_source_stale"),
            EventName::SourceStale,
            "2026-06-23T12:00:00Z",
            EventSeverity::Attention,
            "Source is stale.",
            workspace_id,
        ))
        .expect("event append");

    let events = compose_events(EventsOptions {
        db_path: Some(db_path),
        requested_path: None,
        workspace_scope: true,
        generated_at: "2026-06-23T12:00:00Z".to_string(),
        limit: 10,
    })
    .expect("events compose");
    let rendered = super::render_events_human(&events);

    assert!(rendered.contains("source.stale"));
    assert!(!rendered.contains(" event Source is stale."));
}

#[test]
fn explicit_unknown_root_returns_no_events() {
    let temp = TempWorkspace::new("events-explicit-root-miss").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_code");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    seed_workspace_root(&store, &workspace_id);
    store
        .append_event(WorkspaceEvent::new(
            EventId::new("evt_other_workspace"),
            EventName::SourceStale,
            "2026-06-23T12:00:00Z",
            EventSeverity::Attention,
            "An unrelated workspace event.",
            workspace_id,
        ))
        .expect("event append");
    drop(store);

    let requested = temp.root().join("unknown-root").display().to_string();
    let output = compose_events(EventsOptions {
        db_path: Some(db_path),
        requested_path: Some(requested.clone()),
        workspace_scope: true,
        generated_at: "2026-06-23T12:00:00Z".to_string(),
        limit: 10,
    })
    .expect("events compose");

    assert_eq!(output.workspace_id, None);
    assert_eq!(output.project_id, None);
    assert_eq!(output.requested_path.as_deref(), Some(requested.as_str()));
    assert!(output.events.is_empty());
    assert_eq!(output.event_watermarks, empty_watermarks());
}

#[test]
fn project_events_are_scoped_unless_workspace_is_requested() {
    let temp = TempWorkspace::new("status-events-scoped").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_a = ProjectId::new("proj_a");
    let project_b = ProjectId::new("proj_b");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    seed_workspace_root(&store, &workspace_id);
    seed_project(&store, &project_a, &workspace_id, "root_code", "apps/web");
    seed_project(
        &store,
        &project_b,
        &workspace_id,
        "root_code",
        "apps/backend",
    );
    store
        .append_event(project_event(
            "evt_a",
            &workspace_id,
            &project_a,
            "apps/web/src/index.ts",
            EventSeverity::Attention,
            "Web needs attention.",
        ))
        .expect("event append");
    let mut path_only_event = WorkspaceEvent::new(
        EventId::new("evt_a_path_only"),
        EventName::SourceStale,
        "2026-06-23T12:00:01Z",
        EventSeverity::Attention,
        "Web path-only event.",
        workspace_id.clone(),
    );
    path_only_event.path = Some("apps/web/src/button.ts".to_string());
    store
        .append_event(path_only_event)
        .expect("path-only event append");
    store
        .append_event(project_event(
            "evt_b",
            &workspace_id,
            &project_b,
            "apps/backend/src/main.rs",
            EventSeverity::Attention,
            "Backend needs attention.",
        ))
        .expect("event append");

    let project_events = compose_events(EventsOptions {
        db_path: Some(db_path.clone()),
        requested_path: Some("apps/web/src/index.ts".to_string()),
        workspace_scope: false,
        generated_at: "2026-06-23T12:00:00Z".to_string(),
        limit: 10,
    })
    .expect("events compose");
    assert_eq!(project_events.project_id, Some(project_a));
    assert_eq!(project_events.events.len(), 2);
    assert_eq!(project_events.events[0].id.as_str(), "evt_a_path_only");
    assert_eq!(project_events.events[1].id.as_str(), "evt_a");

    let workspace_events = compose_events(EventsOptions {
        db_path: Some(db_path),
        requested_path: None,
        workspace_scope: true,
        generated_at: "2026-06-23T12:00:00Z".to_string(),
        limit: 10,
    })
    .expect("events compose");
    assert_eq!(workspace_events.events.len(), 3);
}

#[test]
fn project_path_prefixes_are_matched_as_literals() {
    let temp = TempWorkspace::new("status-events-literal-prefix").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_web_app = ProjectId::new("proj_web_app");
    let project_webxapp = ProjectId::new("proj_webxapp");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    seed_workspace_root(&store, &workspace_id);
    seed_project(
        &store,
        &project_web_app,
        &workspace_id,
        "root_code",
        "apps/web_app",
    );
    seed_project(
        &store,
        &project_webxapp,
        &workspace_id,
        "root_code",
        "apps/webXapp",
    );

    let mut web_app_event = WorkspaceEvent::new(
        EventId::new("evt_web_app_path"),
        EventName::SourceStale,
        "2026-06-23T12:00:00Z",
        EventSeverity::Attention,
        "Web app path-only event.",
        workspace_id.clone(),
    );
    web_app_event.path = Some("apps/web_app/src/index.ts".to_string());
    store
        .append_event(web_app_event)
        .expect("web app event append");

    let mut webxapp_event = WorkspaceEvent::new(
        EventId::new("evt_webxapp_path"),
        EventName::SourceStale,
        "2026-06-23T12:00:01Z",
        EventSeverity::Attention,
        "Sibling path-only event.",
        workspace_id,
    );
    webxapp_event.path = Some("apps/webXapp/src/index.ts".to_string());
    store
        .append_event(webxapp_event)
        .expect("webXapp event append");

    let project_events = compose_events(EventsOptions {
        db_path: Some(db_path),
        requested_path: Some("apps/web_app/src/main.ts".to_string()),
        workspace_scope: false,
        generated_at: "2026-06-23T12:00:00Z".to_string(),
        limit: 10,
    })
    .expect("events compose");

    assert_eq!(project_events.project_id, Some(project_web_app));
    assert_eq!(project_events.events.len(), 1);
    assert_eq!(project_events.events[0].id.as_str(), "evt_web_app_path");
}

#[test]
fn project_status_summarizes_attention_in_other_projects() {
    let temp = TempWorkspace::new("status-project-summary").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_web = ProjectId::new("proj_web");
    let project_backend = ProjectId::new("proj_backend");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    seed_workspace_root(&store, &workspace_id);
    seed_project(&store, &project_web, &workspace_id, "root_code", "apps/web");
    seed_project(
        &store,
        &project_backend,
        &workspace_id,
        "root_code",
        "apps/backend",
    );
    let mut backend_event = WorkspaceEvent::new(
        EventId::new("evt_backend_attention"),
        EventName::SourceStale,
        "2026-06-23T12:00:00Z",
        EventSeverity::Attention,
        "Backend needs attention.",
        workspace_id.clone(),
    );
    backend_event.path = Some("apps/backend/src/main.rs".to_string());
    store
        .append_event(backend_event)
        .expect("backend event append");

    let output = compose_status(StatusOptions {
        db_path: Some(db_path),
        requested_path: Some("apps/web/src/index.ts".to_string()),
        workspace_scope: false,
        generated_at: "2026-06-23T12:00:00Z".to_string(),
    })
    .expect("status composes");

    assert_eq!(output.project_id, Some(project_web));
    assert_eq!(output.status.level, StatusLevel::Attention);
    let summary = output.workspace_summary.expect("workspace summary");
    assert_eq!(summary.projects_needing_attention.len(), 1);
    assert_eq!(
        summary.projects_needing_attention[0].project_id,
        project_backend
    );
    assert_eq!(summary.projects_needing_attention[0].path, "apps/backend");
    assert_eq!(
        summary.projects_needing_attention[0].summary,
        "Backend needs attention."
    );
}

#[test]
fn status_signal_events_by_project_matches_scoped_queries() {
    let temp = TempWorkspace::new("events-by-project").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_code");
    let web_project = ProjectId::new("proj_web");
    let api_project = ProjectId::new("proj_api");

    let store = MetadataStore::open(&db_path).expect("metadata opens");
    seed_workspace_root(&store, &workspace_id);
    seed_project(&store, &web_project, &workspace_id, "root_code", "apps/web");
    seed_project(
        &store,
        &api_project,
        &workspace_id,
        "root_code",
        "apps/web-api",
    );
    append_signal_event(
        &store,
        &workspace_id,
        "evt_project_id",
        Some(web_project.clone()),
        None,
        "2026-06-23T12:03:00Z",
    );
    append_signal_event(
        &store,
        &workspace_id,
        "evt_path_prefix",
        None,
        Some("apps/web/src/main.rs"),
        "2026-06-23T12:02:00Z",
    );
    append_signal_event(
        &store,
        &workspace_id,
        "evt_boundary",
        None,
        Some("apps/web-api/src/main.rs"),
        "2026-06-23T12:01:00Z",
    );

    let projects = store.projects(&workspace_id).expect("projects");
    let batched = store
        .status_signal_events_by_project(&workspace_id, &projects)
        .expect("batched events");

    for project in projects {
        let scoped = store
            .list_status_signal_events_scoped(EventQuery {
                workspace_id: Some(workspace_id.clone()),
                project_id: Some(project.id.clone()),
                path_prefix: Some(project.path.clone()),
                limit: 0,
            })
            .expect("scoped events");
        let batched_ids = batched
            .get(&project.id)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|event| event.id)
            .collect::<Vec<_>>();
        let scoped_ids = scoped.into_iter().map(|event| event.id).collect::<Vec<_>>();
        assert_eq!(batched_ids, scoped_ids, "project {}", project.path);
    }
}

#[test]
fn status_uses_recent_actionable_events_for_attention() {
    let temp = TempWorkspace::new("status-recent-events").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_code");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    seed_workspace_root(&store, &workspace_id);

    store
        .append_event(WorkspaceEvent::new(
            EventId::new("evt_device_approval"),
            EventName::DeviceApprovalRequested,
            "2026-06-23T11:59:00Z",
            EventSeverity::Attention,
            "Device approval requested.",
            workspace_id.clone(),
        ))
        .expect("device approval event append");
    for index in 0..51 {
        store
            .append_event(WorkspaceEvent::new(
                EventId::new(format!("evt_info_{index:03}")),
                EventName::SyncStarted,
                format!("2026-06-23T12:{index:02}:00Z"),
                EventSeverity::Info,
                "Informational event.",
                workspace_id.clone(),
            ))
            .expect("event append");
    }

    let output = compose_status(StatusOptions {
        db_path: Some(db_path),
        requested_path: None,
        workspace_scope: true,
        generated_at: "2026-06-23T12:00:00Z".to_string(),
    })
    .expect("status composes");

    assert_eq!(output.status.level, StatusLevel::Attention);
    assert!(
        output
            .status
            .attention_items
            .iter()
            .any(|item| item == "Device approval requested.")
    );
}

#[test]
fn resolved_actionable_events_do_not_keep_status_unhealthy() {
    let temp = TempWorkspace::new("status-resolved-events").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_code");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    seed_workspace_root(&store, &workspace_id);
    store
        .append_event(WorkspaceEvent::new(
            EventId::new("evt_conflict_created"),
            EventName::ConflictCreated,
            "2026-06-23T12:00:00Z",
            EventSeverity::Attention,
            "Merge conflict detected.",
            workspace_id.clone(),
        ))
        .expect("conflict event append");
    store
        .append_event(WorkspaceEvent::new(
            EventId::new("evt_conflict_resolved"),
            EventName::ConflictResolutionAccepted,
            "2026-06-23T12:01:00Z",
            EventSeverity::Info,
            "Merge conflict resolved.",
            workspace_id,
        ))
        .expect("resolution event append");

    let output = compose_status(StatusOptions {
        db_path: Some(db_path),
        requested_path: None,
        workspace_scope: true,
        generated_at: "2026-06-23T12:00:00Z".to_string(),
    })
    .expect("status composes");

    assert_eq!(output.status.level, StatusLevel::Healthy);
    assert!(
        output
            .status
            .attention_items
            .iter()
            .all(|item| item != "Merge conflict detected.")
    );
}

#[test]
fn rejected_resolution_event_clears_conflict_attention() {
    let temp = TempWorkspace::new("status-rejected-resolution").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_code");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    seed_workspace_root(&store, &workspace_id);
    let mut created = WorkspaceEvent::new(
        EventId::new("evt_conflict_created"),
        EventName::ConflictCreated,
        "2026-06-23T12:00:00Z",
        EventSeverity::Attention,
        "Merge conflict detected.",
        workspace_id.clone(),
    );
    created.subject = Some(EventSubject {
        kind: EventSubjectKind::Conflict,
        id: "conflict-1".to_string(),
        path: None,
    });
    store.append_event(created).expect("conflict event append");
    let mut rejected = WorkspaceEvent::new(
        EventId::new("evt_conflict_rejected"),
        EventName::ConflictResolutionRejected,
        "2026-06-23T12:01:00Z",
        EventSeverity::Info,
        "Merge conflict resolved by remote version.",
        workspace_id,
    );
    rejected.subject = Some(EventSubject {
        kind: EventSubjectKind::Conflict,
        id: "conflict-1".to_string(),
        path: None,
    });
    store
        .append_event(rejected)
        .expect("resolution event append");

    let output = compose_status(StatusOptions {
        db_path: Some(db_path),
        requested_path: None,
        workspace_scope: true,
        generated_at: "2026-06-23T12:00:00Z".to_string(),
    })
    .expect("status composes");

    assert_eq!(output.status.level, StatusLevel::Healthy);
    assert!(
        output
            .status
            .attention_items
            .iter()
            .all(|item| item != "Merge conflict detected.")
    );
}

#[test]
fn recovered_component_events_do_not_keep_status_unhealthy() {
    let temp = TempWorkspace::new("status-recovered-events").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_code");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    seed_workspace_root(&store, &workspace_id);
    store
        .append_event(WorkspaceEvent::new(
            EventId::new("evt_network_offline"),
            EventName::NetworkOffline,
            "2026-06-23T12:00:00Z",
            EventSeverity::Limited,
            "Network went offline.",
            workspace_id.clone(),
        ))
        .expect("offline event append");
    store
        .append_event(WorkspaceEvent::new(
            EventId::new("evt_network_recovered"),
            EventName::NetworkRecovered,
            "2026-06-23T12:01:00Z",
            EventSeverity::Info,
            "Network recovered.",
            workspace_id,
        ))
        .expect("recovered event append");

    let output = compose_status(StatusOptions {
        db_path: Some(db_path),
        requested_path: None,
        workspace_scope: true,
        generated_at: "2026-06-23T12:00:00Z".to_string(),
    })
    .expect("status composes");

    assert_eq!(output.status.level, StatusLevel::Healthy);
    assert!(
        output
            .status
            .attention_items
            .iter()
            .all(|item| item != "Network went offline.")
    );
}

#[test]
fn post_commit_followup_degraded_event_stays_visible_in_status() {
    let temp = TempWorkspace::new("status-post-commit-followup-degraded").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_code");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    seed_workspace_root(&store, &workspace_id);
    store
        .set_component_state(
            PostCommitSyncComponent::WorkViewOverlaySync.as_str(),
            "degraded",
            "2026-07-05T12:31:00Z",
        )
        .expect("sync state");
    store
        .append_event(WorkspaceEvent::new(
            EventId::new("evt_sync_post_commit_degraded"),
            EventName::SyncDegraded,
            "2026-07-05T12:31:00Z",
            EventSeverity::Limited,
            "Work-view overlay sync is behind.",
            workspace_id,
        ))
        .expect("degraded event append");

    let output = compose_status(StatusOptions {
        db_path: Some(db_path),
        requested_path: None,
        workspace_scope: true,
        generated_at: "2026-07-05T12:31:01Z".to_string(),
    })
    .expect("status composes");

    assert_eq!(output.status.level, StatusLevel::Limited);
    assert!(
        output
            .items
            .iter()
            .any(|item| item.summary == "Work-view overlay sync is behind.")
    );
    assert!(
        output
            .status
            .attention_items
            .iter()
            .any(|item| item == "Work-view overlay sync is behind.")
    );
}

#[test]
fn component_degradation_always_reports_limited_capability() {
    let temp = TempWorkspace::new("status-components").expect("temp workspace");
    let db_path = temp.root().join("state").join("local.sqlite3");
    let workspace_id = WorkspaceId::new("ws_code");
    let store = MetadataStore::open(&db_path).expect("metadata opens");
    seed_workspace_root(&store, &workspace_id);
    store
        .set_component_state("sync", "degraded", "2026-06-23T12:00:00Z")
        .expect("sync state");
    store
        .set_component_state("watcher", "unavailable", "2026-06-23T12:00:00Z")
        .expect("watcher state");
    store
        .set_component_state("network", "offline", "2026-06-23T12:00:00Z")
        .expect("network state");

    let output = compose_status(StatusOptions {
        db_path: Some(db_path),
        requested_path: None,
        workspace_scope: true,
        generated_at: "2026-06-23T12:00:00Z".to_string(),
    })
    .expect("status composes");

    assert_eq!(output.status.level, StatusLevel::Limited);
    assert!(!output.limits.is_empty());
    assert!(
        output
            .limits
            .iter()
            .all(|limit| !limit.still_works.is_empty())
    );
}

fn append_signal_event(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    id: &str,
    project_id: Option<ProjectId>,
    path: Option<&str>,
    occurred_at: &str,
) {
    let mut event = WorkspaceEvent::new(
        EventId::new(id),
        EventName::SourceStale,
        occurred_at,
        EventSeverity::Attention,
        "Source needs attention.",
        workspace_id.clone(),
    );
    event.project_id = project_id;
    event.path = path.map(ToString::to_string);
    store.append_event(event).expect("event append");
}
