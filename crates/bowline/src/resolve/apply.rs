use super::*;

#[derive(Debug)]
pub(super) struct ResolveDecisionApplied {
    pub(super) summary: String,
}

#[derive(Debug)]
pub(super) enum ResolveError {
    ConflictNotFound(String),
    MissingResolution(String),
    UnsafePath(String),
    Io(io::Error),
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConflictNotFound(id) => write!(formatter, "conflict `{id}` was not found"),
            Self::MissingResolution(path) => {
                write!(formatter, "resolution overlay is missing `{path}`")
            }
            Self::UnsafePath(path) => write!(formatter, "resolution path `{path}` is unsafe"),
            Self::Io(error) => write!(formatter, "resolve action failed: {error}"),
        }
    }
}

impl From<io::Error> for ResolveError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub(super) fn apply_decision(
    requested_path: &Path,
    conflicts: &[ResolveConflict],
    decision: Option<&ResolveDecision>,
    generated_at: &str,
) -> Result<ResolveDecisionApplied, ResolveError> {
    let Some(decision) = decision else {
        return Ok(ResolveDecisionApplied {
            summary: String::new(),
        });
    };
    let (target_id, accepting) = match decision {
        ResolveDecision::Accept(id) => (id, true),
        ResolveDecision::Reject(id) => (id, false),
    };
    let conflict = conflicts
        .iter()
        .find(|conflict| conflict.id == *target_id)
        .ok_or_else(|| ResolveError::ConflictNotFound(target_id.clone()))?;
    let bundle = PathBuf::from(&conflict.bundle_path);
    if accepting {
        let project_root = project_root_for_bundle(requested_path, &bundle, conflict)?;
        let resolution_root = bundle.join("resolution");
        let mut staged = Vec::with_capacity(conflict.affected_files.len());
        for affected in &conflict.affected_files {
            validate_relative_path(affected)?;
            let source = resolution_root.join(affected);
            let destination = project_root.join(affected);
            staged.push(stage_resolution_file(
                &resolution_root,
                &project_root,
                conflict,
                affected,
                &source,
                &destination,
            )?);
        }
        apply_staged_resolutions(&project_root, staged)?;
        mark_bundle_state(&bundle, "accepted", generated_at)?;
        enqueue_resolution_sync(&project_root, conflict, "accept", generated_at);
        return Ok(ResolveDecisionApplied {
            summary: format!("accepted resolution for conflict `{}`", conflict.id),
        });
    }
    let project_root = project_root_for_bundle(requested_path, &bundle, conflict)?;
    let remote_root = bundle.join("remote");
    let mut staged = Vec::with_capacity(conflict.affected_files.len());
    for affected in &conflict.affected_files {
        validate_relative_path(affected)?;
        let source = remote_root.join(affected);
        let destination = project_root.join(affected);
        let missing_policy = if conflict.reason == "delete-versus-edit conflict" {
            MissingSidePolicy::DeleteDestination
        } else {
            MissingSidePolicy::Error
        };
        staged.push(stage_bundle_side_file(
            &remote_root,
            &project_root,
            affected,
            &source,
            &destination,
            missing_policy,
        )?);
    }
    apply_staged_resolutions(&project_root, staged)?;
    mark_bundle_state(&bundle, "rejected", generated_at)?;
    enqueue_resolution_sync(&project_root, conflict, "reject", generated_at);
    Ok(ResolveDecisionApplied {
        summary: format!("rejected resolution for conflict `{}`", conflict.id),
    })
}

pub(super) fn enqueue_resolution_sync(
    project_root: &Path,
    conflict: &ResolveConflict,
    decision: &str,
    generated_at: &str,
) {
    let Ok(db_path) = bowline_local::metadata::default_database_path() else {
        return;
    };
    if !db_path.exists() {
        return;
    }
    let Ok(store) = MetadataStore::open(&db_path) else {
        return;
    };
    let Ok(Some(workspace)) = store.current_workspace() else {
        return;
    };
    let Ok(roots) = store.accepted_roots(&workspace.id) else {
        return;
    };
    if !roots
        .iter()
        .any(|root| path_starts_with(project_root, Path::new(root)))
    {
        return;
    }
    let Ok(base) = store.workspace_sync_head(&workspace.id) else {
        return;
    };
    let base = base.map(|head| head.workspace_ref);
    let payload = serde_json::json!({
        "source": "resolve",
        "decision": decision,
        "conflictId": conflict.id,
        "affectedFiles": conflict.affected_files,
    });
    append_resolution_event(
        &store,
        &workspace.id,
        project_root,
        conflict,
        decision,
        generated_at,
    );
    let _ = store.enqueue_sync_operation(&SyncOperationRecord {
        id: format!(
            "resolve:{}:{}:{}",
            conflict.id,
            decision,
            stable_suffix(generated_at)
        ),
        workspace_id: workspace.id.clone(),
        kind: "upload".to_string(),
        state: "queued".to_string(),
        idempotency_key: format!(
            "resolve:{}:{}:{}",
            conflict.id,
            decision,
            base.as_ref()
                .map(|workspace_ref| workspace_ref.version.to_string())
                .unwrap_or_else(|| "no-head".to_string())
        ),
        base_version: base.as_ref().map(|workspace_ref| workspace_ref.version),
        base_snapshot_id: base
            .as_ref()
            .map(|workspace_ref| workspace_ref.snapshot_id.clone()),
        target_snapshot_id: None,
        device_id: None,
        payload_json: payload.to_string(),
        attempt_count: 0,
        claimed_by: None,
        heartbeat_at: None,
        next_attempt_at: None,
        last_error: None,
        created_at: generated_at.to_string(),
        updated_at: generated_at.to_string(),
    });
}

pub(super) fn append_resolution_event(
    store: &MetadataStore,
    workspace_id: &bowline_core::ids::WorkspaceId,
    project_root: &Path,
    conflict: &ResolveConflict,
    decision: &str,
    generated_at: &str,
) {
    let (name, summary) = match decision {
        "accept" => (
            EventName::ConflictResolutionAccepted,
            format!("Accepted resolution for conflict `{}`.", conflict.id),
        ),
        "reject" => (
            EventName::ConflictResolutionRejected,
            format!("Rejected resolution for conflict `{}`.", conflict.id),
        ),
        _ => return,
    };
    let mut event = WorkspaceEvent::new(
        resolution_event_id(name, &conflict.id, generated_at),
        name,
        generated_at,
        EventSeverity::Info,
        summary,
        workspace_id.clone(),
    );
    let affected_path = conflict
        .affected_files
        .first()
        .cloned()
        .unwrap_or_else(|| project_root.display().to_string());
    event.project_id = store
        .current_project_by_path(&affected_path)
        .ok()
        .flatten()
        .map(|project| project.id);
    event.path = Some(affected_path.clone());
    event.subject = Some(EventSubject {
        kind: EventSubjectKind::Conflict,
        id: conflict.id.clone(),
        path: Some(affected_path),
    });
    event.payload.insert(
        "decision".to_string(),
        serde_json::Value::String(decision.to_string()),
    );
    event.payload.insert(
        "conflictId".to_string(),
        serde_json::Value::String(conflict.id.clone()),
    );
    event.payload.insert(
        "affectedFiles".to_string(),
        serde_json::Value::Array(
            conflict
                .affected_files
                .iter()
                .map(|path| serde_json::Value::String(path.clone()))
                .collect(),
        ),
    );
    event.redaction = EventRedaction::applied(["secret-values-not-included"]);
    let _ = store.append_event(event);
}

pub(super) fn resolution_event_id(
    name: EventName,
    conflict_id: &str,
    generated_at: &str,
) -> EventId {
    let input = format!("{name:?}:{conflict_id}:{generated_at}");
    EventId::new(format!(
        "evt_resolve_{}",
        &blake3::hash(input.as_bytes()).to_hex()[..16]
    ))
}

pub(super) fn stable_suffix(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' => byte as char,
            _ => '_',
        })
        .collect()
}

#[derive(Debug)]
pub(super) struct StagedResolution {
    affected: String,
    destination: PathBuf,
    action: StagedResolutionAction,
}

#[derive(Debug)]
pub(super) enum StagedResolutionAction {
    Write(Vec<u8>),
    Delete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MissingSidePolicy {
    Error,
    DeleteDestination,
}

pub(super) fn stage_resolution_file(
    resolution_root: &Path,
    project_root: &Path,
    conflict: &ResolveConflict,
    affected: &str,
    source: &Path,
    destination: &Path,
) -> Result<StagedResolution, ResolveError> {
    let missing_policy = if conflict.reason == "delete-versus-edit conflict" {
        MissingSidePolicy::DeleteDestination
    } else {
        MissingSidePolicy::Error
    };
    stage_bundle_side_file(
        resolution_root,
        project_root,
        affected,
        source,
        destination,
        missing_policy,
    )
}

pub(super) fn stage_bundle_side_file(
    source_root: &Path,
    project_root: &Path,
    affected: &str,
    source: &Path,
    destination: &Path,
    missing_policy: MissingSidePolicy,
) -> Result<StagedResolution, ResolveError> {
    reject_existing_symlink_components(source_root, affected)?;
    let metadata = match fs::symlink_metadata(source) {
        Ok(metadata) => metadata,
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                && missing_policy == MissingSidePolicy::DeleteDestination =>
        {
            reject_existing_symlink_components(project_root, affected)?;
            return Ok(StagedResolution {
                affected: affected.to_string(),
                destination: destination.to_path_buf(),
                action: StagedResolutionAction::Delete,
            });
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(ResolveError::MissingResolution(affected.to_string()));
        }
        Err(error) => return Err(ResolveError::Io(error)),
    };
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        return Err(ResolveError::UnsafePath(affected.to_string()));
    }
    if !file_type.is_file() {
        return Err(ResolveError::MissingResolution(affected.to_string()));
    }

    reject_existing_symlink_components(project_root, affected)?;
    let bytes = fs::read(source)?;
    Ok(StagedResolution {
        affected: affected.to_string(),
        destination: destination.to_path_buf(),
        action: StagedResolutionAction::Write(bytes),
    })
}

pub(super) fn apply_staged_resolutions(
    project_root: &Path,
    staged: Vec<StagedResolution>,
) -> Result<(), ResolveError> {
    for staged_file in &staged {
        preflight_destination(project_root, staged_file)?;
    }

    let mut temp_paths = Vec::with_capacity(staged.len());
    for (index, staged_file) in staged.iter().enumerate() {
        let StagedResolutionAction::Write(bytes) = &staged_file.action else {
            temp_paths.push(None);
            continue;
        };
        let parent = staged_file
            .destination
            .parent()
            .ok_or_else(|| ResolveError::UnsafePath(staged_file.affected.clone()))?;
        let temp_path = parent.join(format!(
            ".bowline-resolve-{}-{index}.tmp",
            std::process::id()
        ));
        if let Err(error) = write_private_temp_file(&temp_path, bytes) {
            cleanup_temp_paths(temp_paths.iter().filter_map(Option::as_ref));
            return Err(error.into());
        }
        temp_paths.push(Some(temp_path));
    }

    for (staged_file, temp_path) in staged.iter().zip(&temp_paths) {
        reject_existing_symlink_components(project_root, &staged_file.affected)?;
        match (&staged_file.action, temp_path) {
            (StagedResolutionAction::Write(_), Some(temp_path)) => {
                if let Err(error) = fs::rename(temp_path, &staged_file.destination) {
                    cleanup_temp_paths(temp_paths.iter().filter_map(Option::as_ref));
                    return Err(error.into());
                }
            }
            (StagedResolutionAction::Delete, None) => {
                if let Err(error) = remove_destination_file(&staged_file.destination) {
                    cleanup_temp_paths(temp_paths.iter().filter_map(Option::as_ref));
                    return Err(error.into());
                }
            }
            _ => return Err(ResolveError::UnsafePath(staged_file.affected.clone())),
        }
    }
    Ok(())
}

pub(super) fn preflight_destination(
    project_root: &Path,
    staged_file: &StagedResolution,
) -> Result<(), ResolveError> {
    reject_existing_symlink_components(project_root, &staged_file.affected)?;
    let parent = staged_file
        .destination
        .parent()
        .ok_or_else(|| ResolveError::UnsafePath(staged_file.affected.clone()))?;
    fs::create_dir_all(parent)?;
    reject_existing_symlink_components(project_root, &staged_file.affected)?;
    match fs::symlink_metadata(&staged_file.destination) {
        Ok(metadata)
            if metadata.file_type().is_file()
                && matches!(staged_file.action, StagedResolutionAction::Write(_)) =>
        {
            Ok(())
        }
        Ok(metadata)
            if metadata.file_type().is_file()
                && matches!(staged_file.action, StagedResolutionAction::Delete) =>
        {
            Ok(())
        }
        Ok(_) => Err(ResolveError::UnsafePath(staged_file.affected.clone())),
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                && matches!(staged_file.action, StagedResolutionAction::Write(_)) =>
        {
            Ok(())
        }
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                && matches!(staged_file.action, StagedResolutionAction::Delete) =>
        {
            Ok(())
        }
        Err(error) => Err(ResolveError::Io(error)),
    }
}

pub(super) fn write_private_temp_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)
}

pub(super) fn cleanup_temp_paths<'a>(paths: impl IntoIterator<Item = &'a PathBuf>) {
    for path in paths {
        let _ = fs::remove_file(path);
    }
}

pub(super) fn remove_destination_file(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

pub(super) fn reject_existing_symlink_components(
    root: &Path,
    relative_path: &str,
) -> Result<(), ResolveError> {
    let mut current = root.to_path_buf();
    for component in Path::new(relative_path).components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(ResolveError::UnsafePath(relative_path.to_string()));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(ResolveError::Io(error)),
        }
    }
    Ok(())
}

pub(super) fn project_root_for_bundle(
    requested_path: &Path,
    bundle: &Path,
    conflict: &ResolveConflict,
) -> Result<PathBuf, ResolveError> {
    if !is_bundle_path(requested_path) {
        if bundle_is_under_project_conflicts(requested_path, bundle) {
            return Ok(requested_path.to_path_buf());
        }
        if let Some(workspace_root) = conflict.workspace_root.as_deref() {
            let workspace_root = PathBuf::from(workspace_root);
            if same_path(&workspace_root, requested_path)
                || request_covers_all_affected_paths(requested_path, &workspace_root, conflict)?
            {
                return Ok(workspace_root);
            }
            return Err(ResolveError::UnsafePath(format!(
                "state-root conflict bundle belongs to `{}`; requested `{}`",
                workspace_root.display(),
                requested_path.display()
            )));
        }
        if is_state_root_bundle(bundle) {
            return Err(ResolveError::UnsafePath(
                "state-root conflict bundle is missing trusted workspace root metadata".to_string(),
            ));
        }
        return Ok(requested_path.to_path_buf());
    }
    let components = bundle
        .components()
        .map(|component| component.as_os_str().to_string_lossy().to_string())
        .collect::<Vec<_>>();
    if let Some(index) = components
        .iter()
        .position(|component| component == PRIVATE_STATE_ROOT)
    {
        let mut root = PathBuf::new();
        for component in &components[..index] {
            root.push(component);
        }
        return Ok(root);
    }
    Err(ResolveError::UnsafePath(
        "state-root conflict bundles must be accepted from the workspace root, not by direct bundle path"
            .to_string(),
    ))
}

pub(super) fn request_covers_all_affected_paths(
    requested_path: &Path,
    workspace_root: &Path,
    conflict: &ResolveConflict,
) -> Result<bool, ResolveError> {
    let requested_path =
        fs::canonicalize(requested_path).unwrap_or_else(|_| requested_path.to_path_buf());
    let workspace_root =
        fs::canonicalize(workspace_root).unwrap_or_else(|_| workspace_root.to_path_buf());
    if !requested_path.starts_with(&workspace_root) {
        return Ok(false);
    }
    for affected in &conflict.affected_files {
        validate_relative_path(affected)?;
        let affected_path = workspace_root.join(affected);
        if !affected_path.starts_with(&requested_path) {
            return Ok(false);
        }
    }
    Ok(!conflict.affected_files.is_empty())
}

pub(super) fn bundle_is_under_project_conflicts(project_root: &Path, bundle: &Path) -> bool {
    path_starts_with(
        bundle,
        &project_root.join(PRIVATE_STATE_ROOT).join("conflicts"),
    )
}

pub(super) fn is_state_root_bundle(bundle: &Path) -> bool {
    state_root_for_conflicts()
        .map(|state_root| path_starts_with(bundle, &state_root.join("conflicts")))
        .unwrap_or(false)
}

pub(super) fn path_starts_with(path: &Path, root: &Path) -> bool {
    let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    path.starts_with(root)
}

pub(super) fn same_path(left: &Path, right: &Path) -> bool {
    let left = fs::canonicalize(left).unwrap_or_else(|_| left.to_path_buf());
    let right = fs::canonicalize(right).unwrap_or_else(|_| right.to_path_buf());
    left == right
}

pub(super) fn is_bundle_path(path: &Path) -> bool {
    path.join("manifest.json").is_file()
}

pub(super) fn validate_relative_path(path: &str) -> Result<(), ResolveError> {
    let normalized = bowline_core::workspace_graph::normalize_workspace_path(path);
    if normalized != path
        || normalized.is_empty()
        || normalized.starts_with("../")
        || normalized.contains("/../")
        || normalized == PRIVATE_STATE_ROOT
        || normalized.starts_with(&format!("{PRIVATE_STATE_ROOT}/"))
    {
        return Err(ResolveError::UnsafePath(path.to_string()));
    }
    Ok(())
}
