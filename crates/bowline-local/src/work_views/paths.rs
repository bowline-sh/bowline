use std::{
    fs, io,
    path::{Component, Path, PathBuf},
};

use bowline_core::{
    events::{EventName, EventSeverity, EventSubject, EventSubjectKind, WorkspaceEvent},
    ids::{EventId, ProjectId, WorkViewId},
    policy::{AccessFlag, MaterializationMode, PathClassification},
    status::{StatusLevel, WorkspaceStatus},
    work_views::{
        WorkDiffChangeKind, WorkDiffEntry, WorkView, WorkViewLifecycle, WorkViewSyncState,
    },
    workspace_graph::normalize_workspace_path,
};

use crate::{
    metadata::{MetadataStore, default_database_path},
    policy::{PathFacts, UserPolicy, classify_path},
};

use super::WorkViewError;

pub(super) fn project_has_pending_local_writes(
    store: &MetadataStore,
    workspace_id: &bowline_core::ids::WorkspaceId,
    project_id: &ProjectId,
    project_path: &str,
) -> Result<bool, WorkViewError> {
    let project_path = normalize_workspace_path(project_path);
    let synced_at = store
        .workspace_sync_head(workspace_id)?
        .map(|head| head.observed_at);
    for write in store.local_writes_for_project(
        workspace_id,
        project_id,
        &project_path,
        synced_at.as_deref(),
        None,
    )? {
        let Ok(relative_path) = store.workspace_relative_path(workspace_id, &write.path) else {
            if write.project_id.as_ref() == Some(project_id) {
                return Ok(true);
            }
            continue;
        };
        let relative_path = normalize_workspace_path(&relative_path);
        if relative_path == project_path && write.operation == "modify" {
            continue;
        }
        if relative_path == ".work"
            || relative_path
                .strip_prefix(".work")
                .is_some_and(|suffix| suffix.starts_with('/'))
        {
            continue;
        }
        if write.project_id.as_ref() == Some(project_id) {
            return Ok(true);
        }
        if relative_path == project_path
            || relative_path
                .strip_prefix(&project_path)
                .is_some_and(|suffix| suffix.starts_with('/'))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn resolve_work_view(
    store: &MetadataStore,
    selector: &str,
) -> Result<WorkView, WorkViewError> {
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

pub(super) fn workspace_path_for_project_file(work_view: &WorkView, relative: &Path) -> String {
    normalize_workspace_path(
        &PathBuf::from(normalize_workspace_path(&work_view.project_path))
            .join(relative)
            .display()
            .to_string(),
    )
}

pub(super) fn clean_accept_policy(
    store: &MetadataStore,
    workspace_root: &Path,
    workspace_id: &bowline_core::ids::WorkspaceId,
    workspace_path: &str,
    source: Option<&Path>,
) -> Result<crate::policy::PathPolicyDecision, WorkViewError> {
    if let Some(observed) = store.observed_path(workspace_id, workspace_path)? {
        return Ok(crate::policy::PathPolicyDecision {
            classification: observed.classification,
            mode: observed.mode,
            access: observed.access,
        });
    }
    let policy = UserPolicy::load_for_path(workspace_root, workspace_path)?;
    let byte_len = source
        .map(fs::metadata)
        .transpose()?
        .map(|metadata| metadata.len());
    Ok(classify_path(
        &PathFacts {
            relative_path: workspace_path.to_string(),
            is_dir: false,
            byte_len,
        },
        &policy,
    ))
}

pub(super) fn clean_accept_explicit_include(
    workspace_root: &Path,
    workspace_path: &str,
) -> Result<bool, WorkViewError> {
    Ok(UserPolicy::load_for_path(workspace_root, workspace_path)?
        .explicitly_includes(workspace_path))
}

pub(super) fn is_clean_accept_policy_eligible(
    classification: PathClassification,
    mode: MaterializationMode,
) -> bool {
    matches!(
        (classification, mode),
        (PathClassification::WorkspaceSync, _)
            | (PathClassification::LargeFile, MaterializationMode::Lazy)
            | (
                PathClassification::ProjectEnv,
                MaterializationMode::ProjectEnv | MaterializationMode::EncryptedSync
            )
            | (
                PathClassification::SecretLooking,
                MaterializationMode::EncryptedSync | MaterializationMode::ProjectEnv
            )
    )
}

pub(super) fn is_work_view_materialization_eligible(
    classification: PathClassification,
    mode: MaterializationMode,
    access: &[AccessFlag],
) -> bool {
    if access.contains(&AccessFlag::AgentHidden) {
        return false;
    }
    matches!(
        (classification, mode),
        (PathClassification::WorkspaceSync, _)
            | (PathClassification::LargeFile, MaterializationMode::Lazy)
            | (
                PathClassification::ProjectEnv,
                MaterializationMode::ProjectEnv | MaterializationMode::EncryptedSync
            )
    )
}

pub(super) fn is_owner_only_work_view_policy(
    classification: PathClassification,
    mode: MaterializationMode,
) -> bool {
    matches!(
        (classification, mode),
        (
            PathClassification::ProjectEnv | PathClassification::SecretLooking,
            MaterializationMode::ProjectEnv | MaterializationMode::EncryptedSync
        )
    )
}

#[cfg(unix)]
#[cfg(test)]
pub(super) fn apply_owner_only_work_view_permissions(
    path: &Path,
    owner_only: bool,
) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    if owner_only {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(not(unix))]
#[cfg(test)]
pub(super) fn apply_owner_only_work_view_permissions(
    path: &Path,
    owner_only: bool,
) -> io::Result<()> {
    let _ = (path, owner_only);
    Ok(())
}

pub(super) fn is_ignored_clean_accept_policy(
    classification: PathClassification,
    mode: MaterializationMode,
) -> bool {
    matches!(
        (classification, mode),
        (
            PathClassification::Generated
                | PathClassification::Dependency
                | PathClassification::Cache
                | PathClassification::LocalOnly,
            MaterializationMode::LocalRegenerate
                | MaterializationMode::LocalCache
                | MaterializationMode::Ignore
                | MaterializationMode::LocalOnly
        )
    )
}

pub(super) fn is_bowline_owned_namespace(relative: &Path) -> bool {
    matches!(
        relative.components().next(),
        Some(Component::Normal(name)) if name.to_str() == Some(".work")
    )
}

pub(super) fn is_secret_bearing_work_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(".env"))
}

pub(super) fn is_source_control_metadata_path(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(
            component,
            Component::Normal(name)
                if matches!(name.to_str(), Some(".git" | ".jj" | ".hg" | ".svn"))
        )
    })
}

pub(super) fn main_project_root(
    store: &MetadataStore,
    work_view: &WorkView,
) -> Result<Option<PathBuf>, WorkViewError> {
    let Some(root) = store.current_workspace_root()? else {
        return Ok(None);
    };
    Ok(Some(
        expand_display_path(root).join(normalize_workspace_path(&work_view.project_path)),
    ))
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

pub(super) fn files_under_with_checkpoint(
    root: &Path,
    checkpoint: &mut dyn FnMut() -> bool,
) -> Result<Vec<PathBuf>, WorkViewError> {
    let mut files = Vec::new();
    collect_files_with_checkpoint(root, root, &mut files, checkpoint)?;
    files.sort();
    Ok(files)
}

fn collect_files_with_checkpoint(
    root: &Path,
    path: &Path,
    files: &mut Vec<PathBuf>,
    checkpoint: &mut dyn FnMut() -> bool,
) -> Result<(), WorkViewError> {
    for entry in fs::read_dir(path)? {
        cancellation_checkpoint(checkpoint)?;
        let entry = entry?;
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if is_source_control_metadata_path(relative) {
            continue;
        }
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            return Err(WorkViewError::UnsafeWorkViewPath {
                path: path.display().to_string(),
                reason: "symlinks are not followed in work views",
            });
        }
        if metadata.is_dir() {
            collect_files_with_checkpoint(root, &path, files, checkpoint)?;
        } else if metadata.is_file() {
            files.push(path);
        }
    }
    Ok(())
}

pub(super) fn cancellation_checkpoint(
    checkpoint: &mut dyn FnMut() -> bool,
) -> Result<(), WorkViewError> {
    if checkpoint() {
        Ok(())
    } else {
        Err(WorkViewError::Io(io::Error::new(
            io::ErrorKind::Interrupted,
            "work-view operation cancelled",
        )))
    }
}

pub(super) fn ensure_fresh_materialization_path(path: &Path) -> Result<(), WorkViewError> {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Ok(());
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(WorkViewError::UnsafeWorkViewPath {
            path: path.display().to_string(),
            reason: "work view materialization path already exists",
        });
    }
    if fs::read_dir(path)?.next().is_some() {
        return Err(WorkViewError::UnsafeWorkViewPath {
            path: path.display().to_string(),
            reason: "work view materialization path is not empty",
        });
    }
    Ok(())
}

pub(super) fn remove_materialization_tree(path: &Path) {
    if let Ok(metadata) = fs::symlink_metadata(path)
        && metadata.is_dir()
        && !metadata.file_type().is_symlink()
    {
        let _ = fs::remove_dir_all(path);
    }
}

pub(super) fn status_for_changes(changes: &[WorkDiffEntry]) -> WorkspaceStatus {
    if changes.iter().any(|change| {
        matches!(
            change.kind,
            WorkDiffChangeKind::Conflict | WorkDiffChangeKind::PolicyReview
        )
    }) {
        return WorkspaceStatus {
            level: StatusLevel::Attention,
            attention_items: vec!["Work view has changes that need review.".to_string()],
        };
    }
    WorkspaceStatus::healthy()
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

pub(super) fn open_store(db_path: Option<&Path>) -> Result<MetadataStore, WorkViewError> {
    let path = match db_path {
        Some(path) => path.to_path_buf(),
        None => default_database_path().map_err(|_| WorkViewError::MissingMetadataDb)?,
    };
    MetadataStore::open(path).map_err(Into::into)
}

pub(super) fn validate_work_view_name(name: &str) -> Result<(), WorkViewError> {
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

pub(super) fn visible_path(root: &str, project_path: &str, name: &str) -> PathBuf {
    expand_display_path(root)
        .join(".work")
        .join(normalize_workspace_path(project_path))
        .join(name)
}

pub(super) fn display_path(path: &Path) -> String {
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

pub(crate) fn expand_display_path(path: impl AsRef<str>) -> PathBuf {
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

pub(super) fn work_view_id(workspace_id: &str, project_id: &str, name: &str) -> WorkViewId {
    let input = format!("{workspace_id}:{project_id}:{name}");
    WorkViewId::new(format!(
        "work_{}",
        &blake3::hash(input.as_bytes()).to_hex()[..16]
    ))
}

pub(super) fn append_work_event(
    store: &MetadataStore,
    name: EventName,
    work_view: &WorkView,
    generated_at: &str,
) {
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
    append_event_or_log(store, event);
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
