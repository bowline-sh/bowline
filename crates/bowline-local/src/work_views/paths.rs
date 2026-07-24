use std::{
    fs,
    path::{Component, Path, PathBuf},
};

use bowline_core::{
    events::{EventName, EventSeverity, EventSubject, EventSubjectKind, WorkspaceEvent},
    ids::{EventId, WorkViewId},
    status::{StatusLevel, WorkspaceStatus},
    work_views::{WorkView, WorkViewLifecycle, WorkViewRetentionState, WorkViewSyncState},
    workspace_graph::normalize_workspace_path,
};

use crate::metadata::{MetadataStore, default_database_path};

use super::WorkViewError;

pub fn resolve_work_view(store: &MetadataStore, selector: &str) -> Result<WorkView, WorkViewError> {
    reconcile_aux_work_views(store)?;
    let workspace = store
        .current_workspace()?
        .ok_or(WorkViewError::MissingWorkspace)?;
    if let Some(work_view) =
        store.work_view_by_id(&workspace.id, &WorkViewId::new(selector.to_string()))?
    {
        return Ok(work_view);
    }

    let matches = store.work_views_by_name(&workspace.id, None, selector)?;
    match matches.as_slice() {
        [work_view] => Ok(work_view.clone()),
        [] => resolve_work_view_by_visible_path(store, &workspace.id, selector)?.ok_or(
            WorkViewError::MissingWorkView {
                selector: selector.to_string(),
            },
        ),
        _ => Err(WorkViewError::AmbiguousSelector {
            selector: selector.to_string(),
            matches: matches
                .iter()
                .map(|view| format!("{} ({})", view.id.as_str(), view.project_path))
                .collect(),
        }),
    }
}

pub fn reconcile_aux_work_views(store: &MetadataStore) -> Result<(), WorkViewError> {
    use crate::sync::manifest_engine::work_view_cli::{read_aux_index_file, wire_view_from_record};

    let workspace = store
        .current_workspace()?
        .ok_or(WorkViewError::MissingWorkspace)?;
    let root = store
        .current_workspace_root()?
        .ok_or(WorkViewError::MissingWorkspaceRoot)?;
    let root_id = store
        .accepted_root_id_for_path(&workspace.id, &root)?
        .ok_or(WorkViewError::MissingWorkspaceRoot)?;
    let expanded_root = expand_display_path(&root);
    let aux = read_aux_index_file(&expanded_root)?;
    for (id, record) in &aux.work_views {
        store.insert_project(
            &record.project_id,
            &workspace.id,
            &root_id,
            &record.project_path,
            &record.created_at,
        )?;
        let mut view = wire_view_from_record(&workspace.id, &expanded_root, id, record);
        if let Some(existing) = store.work_view_by_id(&workspace.id, &view.id)?
            && existing.retention.state == WorkViewRetentionState::DeleteEligible
            && matches!(
                view.lifecycle,
                WorkViewLifecycle::Accepted | WorkViewLifecycle::Discarded
            )
        {
            view.retention = existing.retention;
            view.updated_at = existing.updated_at;
        }
        store.upsert_work_view(&view)?;
    }
    Ok(())
}

pub(super) fn resolve_work_view_by_visible_path(
    store: &MetadataStore,
    workspace_id: &bowline_core::ids::WorkspaceId,
    selector: &str,
) -> Result<Option<WorkView>, WorkViewError> {
    let selector_path = normalize_lexical_path(expand_display_path(selector));
    Ok(store
        .work_views(workspace_id, true, None)?
        .into_iter()
        .filter(|view| {
            path_is_at_or_under(
                &selector_path,
                &normalize_lexical_path(expand_display_path(&view.visible_path)),
            )
        })
        .max_by_key(|view| {
            normalize_lexical_path(expand_display_path(&view.visible_path))
                .components()
                .count()
        }))
}

pub(super) fn path_is_at_or_under(path: &Path, root: &Path) -> bool {
    path == root || path.starts_with(root)
}

pub(super) fn normalize_lexical_path(path: PathBuf) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

pub(super) fn work_namespace_root(
    store: &MetadataStore,
    work_view: &WorkView,
) -> Result<Option<PathBuf>, WorkViewError> {
    let Some(root) = store.current_workspace_root()? else {
        return Ok(None);
    };
    Ok(Some(
        expand_display_path(root)
            .join(".work")
            .join(normalize_workspace_path(&work_view.project_path)),
    ))
}

pub(super) fn ensure_path_inside(
    path: &Path,
    root: &Path,
    reason: &'static str,
) -> Result<(), WorkViewError> {
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(WorkViewError::UnsafeWorkViewPath {
            path: path.display().to_string(),
            reason,
        });
    }
    if path.starts_with(root) {
        return Ok(());
    }
    Err(WorkViewError::UnsafeWorkViewPath {
        path: path.display().to_string(),
        reason,
    })
}

pub(super) fn ensure_existing_path_inside_real(
    path: &Path,
    root: &Path,
    reason: &'static str,
) -> Result<(), WorkViewError> {
    let canonical_path = fs::canonicalize(path)?;
    let canonical_root = fs::canonicalize(root)?;
    if canonical_path.starts_with(&canonical_root) {
        return Ok(());
    }
    Err(WorkViewError::UnsafeWorkViewPath {
        path: path.display().to_string(),
        reason,
    })
}

pub(super) fn ensure_no_symlink_ancestors(
    path: &Path,
    root: &Path,
    reason: &'static str,
) -> Result<(), WorkViewError> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| WorkViewError::UnsafeWorkViewPath {
            path: path.display().to_string(),
            reason,
        })?;
    let mut current = root.to_path_buf();
    for component in relative {
        current.push(component);
        if let Ok(metadata) = fs::symlink_metadata(&current)
            && metadata.file_type().is_symlink()
        {
            return Err(WorkViewError::UnsafeWorkViewPath {
                path: current.display().to_string(),
                reason,
            });
        }
    }
    Ok(())
}

pub(super) fn status_for_work_views(work_views: &[WorkView]) -> WorkspaceStatus {
    let attention_items = work_views
        .iter()
        .filter(|work_view| {
            matches!(work_view.lifecycle, WorkViewLifecycle::ReviewReady)
                || matches!(
                    work_view.sync_state,
                    WorkViewSyncState::Attention | WorkViewSyncState::Conflicted
                )
                || !work_view.attention.is_empty()
        })
        .map(|work_view| {
            format!(
                "{} is {}; review before accepting.",
                work_view.name,
                serde_json::to_value(work_view.lifecycle)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_string))
                    .unwrap_or_else(|| "attention".to_string())
            )
        })
        .collect::<Vec<_>>();
    if attention_items.is_empty() {
        WorkspaceStatus::healthy()
    } else {
        WorkspaceStatus {
            level: StatusLevel::Attention,
            attention_items,
        }
    }
}

pub fn open_store(db_path: Option<&Path>) -> Result<MetadataStore, WorkViewError> {
    let path = match db_path {
        Some(path) => path.to_path_buf(),
        None => default_database_path().map_err(|_| WorkViewError::MissingMetadataDb)?,
    };
    MetadataStore::open(path).map_err(Into::into)
}

pub fn validate_work_view_name(name: &str) -> Result<(), WorkViewError> {
    let invalid = |reason| WorkViewError::InvalidName {
        name: name.to_string(),
        reason,
    };
    if name.is_empty() {
        return Err(invalid("name must not be empty"));
    }
    if name == "." || name == ".." || name == ".work" {
        return Err(invalid("reserved name"));
    }
    if name.starts_with('.') {
        return Err(invalid("hidden names are reserved for bowline metadata"));
    }
    if name.contains('/') || name.contains('\\') {
        return Err(invalid(
            "use a short branch-like name without path separators",
        ));
    }
    if name
        .chars()
        .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err(invalid(
            "use hyphens instead of whitespace or control characters",
        ));
    }
    Ok(())
}

pub fn visible_path(root: &str, project_path: &str, name: &str) -> PathBuf {
    expand_display_path(root)
        .join(".work")
        .join(normalize_workspace_path(project_path))
        .join(name)
}

pub fn display_path(path: &Path) -> String {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return path.display().to_string();
    };
    let Ok(relative) = path.strip_prefix(&home) else {
        return path.display().to_string();
    };
    if relative.as_os_str().is_empty() {
        "~".to_string()
    } else {
        format!("~/{}", relative.display())
    }
}

pub fn expand_display_path(path: impl AsRef<str>) -> PathBuf {
    let path = path.as_ref();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        if path == "~" {
            return home;
        }
        if let Some(relative) = path.strip_prefix("~/") {
            return home.join(relative);
        }
    }
    PathBuf::from(path)
}

pub fn work_view_id(workspace_id: &str, project_id: &str, name: &str) -> WorkViewId {
    let input = format!("{workspace_id}:{project_id}:{name}");
    WorkViewId::new(format!(
        "work_{}",
        &blake3::hash(input.as_bytes()).to_hex()[..16]
    ))
}

pub fn append_work_event(
    store: &MetadataStore,
    name: EventName,
    work_view: &WorkView,
    generated_at: &str,
) {
    append_event_or_log(store, work_event(name, work_view, generated_at));
}

pub(crate) fn work_event(
    name: EventName,
    work_view: &WorkView,
    generated_at: &str,
) -> WorkspaceEvent {
    let mut event = WorkspaceEvent::new(
        event_id(&name, work_view.id.as_str(), generated_at),
        name.clone(),
        generated_at,
        work_event_severity(&name),
        format!("Work view {} {}", work_view.name, event_verb(&name)),
        work_view.workspace_id.clone(),
    );
    event.project_id = Some(work_view.project_id.clone());
    event.path = Some(work_view.visible_path.clone());
    event.subject = Some(EventSubject {
        kind: EventSubjectKind::WorkView,
        id: work_view.id.as_str().to_string(),
        path: Some(work_view.visible_path.clone()),
    });
    event.payload.insert(
        "name".to_string(),
        serde_json::Value::String(work_view.name.clone()),
    );
    event
}

pub(super) fn append_workspace_event(
    store: &MetadataStore,
    name: EventName,
    workspace_id: &bowline_core::ids::WorkspaceId,
    generated_at: &str,
    summary: &str,
) {
    let event = WorkspaceEvent::new(
        event_id(&name, workspace_id.as_str(), generated_at),
        name,
        generated_at,
        EventSeverity::Info,
        summary,
        workspace_id.clone(),
    );
    append_event_or_log(store, event);
}

fn append_event_or_log(store: &MetadataStore, event: WorkspaceEvent) -> bool {
    let event_id = event.id.as_str().to_string();
    let event_name = event.name.clone();
    if let Err(error) = store.append_event(event) {
        eprintln!("bowline work-view event append failed for {event_name:?} {event_id}: {error}");
        return false;
    }
    true
}

pub(super) fn event_id(name: &EventName, subject: &str, generated_at: &str) -> EventId {
    let input = format!("{name:?}:{subject}:{generated_at}");
    EventId::new(format!(
        "evt_work_{}",
        &blake3::hash(input.as_bytes()).to_hex()[..16]
    ))
}

pub(super) fn event_verb(name: &EventName) -> &'static str {
    match name {
        EventName::WorkCreated => "created",
        EventName::WorkAccepted => "accepted",
        EventName::WorkDiscarded => "discarded",
        EventName::WorkRestored => "restored",
        _ => "updated",
    }
}

pub(super) fn work_event_severity(name: &EventName) -> EventSeverity {
    match name {
        EventName::WorkReviewReady => EventSeverity::Attention,
        _ => EventSeverity::Info,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::TempWorkspace;
    use bowline_core::ids::WorkspaceId;

    #[test]
    fn append_workspace_event_logs_duplicate_append_failure_without_panicking() {
        let temp = TempWorkspace::new("work-view-event-append-log").expect("temp workspace");
        let db_path = temp.root().join("state").join("local.sqlite3");
        let workspace_id = WorkspaceId::new("ws_code");
        let store = MetadataStore::open(&db_path).expect("metadata opens");
        store
            .insert_workspace(&workspace_id, "User Code", "2026-07-05T00:00:00Z")
            .expect("workspace insert");

        let event = WorkspaceEvent::new(
            event_id(
                &EventName::WorkCreated,
                workspace_id.as_str(),
                "2026-07-05T00:00:00Z",
            ),
            EventName::WorkCreated,
            "2026-07-05T00:00:00Z",
            EventSeverity::Info,
            "Work view created",
            workspace_id.clone(),
        );
        assert!(append_event_or_log(&store, event.clone()));
        assert!(!append_event_or_log(&store, event));

        let events = store.list_events(10).expect("events list");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].summary, "Work view created");
    }
}
