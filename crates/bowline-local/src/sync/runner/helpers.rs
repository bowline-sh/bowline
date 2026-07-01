use super::*;

pub(super) fn materialize_snapshot(
    root: &Path,
    base: Option<&SnapshotContent>,
    target: &SnapshotContent,
) -> Result<(), SyncRunnerError> {
    materialize_snapshot_excluding(root, base, target, &BTreeSet::new())
}

pub(super) fn append_hydration_event(
    store: &MetadataStore,
    name: EventName,
    severity: EventSeverity,
    options: &SyncRunnerOptions,
    remote_ref: &WorkspaceRef,
    manifest: Option<&SnapshotManifest>,
    reason: Option<&str>,
) {
    let (file_count, byte_count) = manifest
        .map(|manifest| materialization_counts(&manifest.entries))
        .unwrap_or((0, 0));
    let summary = match name {
        EventName::HydrationStarted => format!(
            "Remote snapshot materialization started: {byte_count} byte(s) across {file_count} file(s)."
        ),
        EventName::HydrationCompleted => format!(
            "Remote snapshot materialization completed: {byte_count} byte(s) across {file_count} file(s)."
        ),
        EventName::HydrationBlocked => format!(
            "Remote snapshot materialization blocked: {}",
            reason.unwrap_or("unknown reason")
        ),
        _ => "Remote snapshot materialization updated.".to_string(),
    };
    let mut event = WorkspaceEvent::new(
        hydration_event_id(name, &remote_ref.snapshot_id, &options.generated_at),
        name,
        options.generated_at.clone(),
        severity,
        summary,
        options.workspace_id.clone(),
    );
    event.path = Some(options.root.display().to_string());
    event.device_id = Some(options.device_id.clone());
    event.subject = Some(EventSubject {
        kind: EventSubjectKind::Root,
        id: workspace_scoped_root_id(&options.workspace_id),
        path: Some(options.root.display().to_string()),
    });
    event.payload.insert(
        "cause".to_string(),
        serde_json::Value::String("remote-import".to_string()),
    );
    event.payload.insert(
        "snapshotId".to_string(),
        serde_json::Value::String(remote_ref.snapshot_id.clone()),
    );
    event
        .payload
        .insert("bytes".to_string(), serde_json::Value::from(byte_count));
    event
        .payload
        .insert("fileCount".to_string(), serde_json::Value::from(file_count));
    if let Some(reason) = reason {
        event.payload.insert(
            "reason".to_string(),
            serde_json::Value::String(reason.to_string()),
        );
    }
    let _ = store.append_event(event);
}

pub(super) fn materialization_counts(entries: &[NamespaceEntry]) -> (usize, u64) {
    entries
        .iter()
        .filter(|entry| entry.kind == NamespaceEntryKind::File)
        .fold((0, 0), |(files, bytes), entry| {
            (files + 1, bytes + entry.byte_len.unwrap_or(0))
        })
}

pub(super) fn should_hydrate_imported_entry(
    entry: &NamespaceEntry,
    selection: ImportedHydrationSelection,
) -> bool {
    match selection {
        ImportedHydrationSelection::AllFiles => true,
        ImportedHydrationSelection::EagerFiles => entry.mode != MaterializationMode::Lazy,
    }
}

pub(super) fn hydration_event_id(name: EventName, snapshot_id: &str, now: &str) -> EventId {
    EventId::new(format!(
        "evt_hydration_{}_{}_{}",
        hydration_event_name(name),
        snapshot_id,
        event_id_component(now)
    ))
}

pub(super) fn sync_checkpoint_id(
    operation_id: &str,
    step: &str,
    state: &str,
    payload_json: &str,
) -> String {
    let hash = blake3::hash(format!("{operation_id}:{step}:{state}:{payload_json}").as_bytes());
    format!(
        "sync-checkpoint-{}-{}-{}",
        event_id_component(operation_id),
        event_id_component(step),
        hash.to_hex().chars().take(12).collect::<String>(),
    )
}

pub(super) fn json_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"<invalid>\"".to_string())
}

pub(super) fn hydration_event_name(name: EventName) -> &'static str {
    match name {
        EventName::HydrationStarted => "started",
        EventName::HydrationCompleted => "completed",
        EventName::HydrationBlocked => "blocked",
        _ => "updated",
    }
}

pub(super) fn event_id_component(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '_'
            }
        })
        .collect()
}

pub(super) fn projected_node_for_entry(
    workspace_id: &WorkspaceId,
    entry: &NamespaceEntry,
    updated_at: &str,
) -> ProjectedNodeRecord {
    ProjectedNodeRecord {
        workspace_id: workspace_id.clone(),
        node_id: format!("node:{}", entry.path),
        project_id: None,
        parent_node_id: parent_path(&entry.path).map(|path| format!("node:{path}")),
        path: entry.path.clone(),
        kind: entry.kind,
        content_id: entry.content_id.clone(),
        hydration_state: entry.hydration_state,
        updated_at: updated_at.to_string(),
    }
}

pub(super) fn projected_node_for_observed_entry(
    workspace_id: &WorkspaceId,
    entry: &NamespaceEntry,
    updated_at: &str,
) -> ProjectedNodeRecord {
    ProjectedNodeRecord {
        workspace_id: workspace_id.clone(),
        node_id: format!("node:{}", entry.path),
        project_id: None,
        parent_node_id: parent_path(&entry.path).map(|path| format!("node:{path}")),
        path: entry.path.clone(),
        kind: entry.kind,
        content_id: entry.content_id.clone(),
        hydration_state: entry.hydration_state,
        updated_at: updated_at.to_string(),
    }
}

pub(super) fn parent_path(path: &str) -> Option<&str> {
    path.rsplit_once('/')
        .map(|(parent, _)| parent)
        .filter(|parent| !parent.is_empty())
}

pub(super) fn ancestor_paths(path: &str) -> Vec<String> {
    let mut ancestors = Vec::new();
    let mut current = path;
    while let Some(parent) = parent_path(current) {
        ancestors.push(parent.to_string());
        current = parent;
    }
    ancestors
}

pub(super) fn cold_placeholder_is_absent(path: &Path) -> Result<bool, SyncRunnerError> {
    match path.try_exists() {
        Ok(exists) => Ok(!exists),
        Err(error) if error.kind() == io::ErrorKind::NotADirectory => Ok(false),
        Err(error) => Err(SyncRunnerError::StateIo(error)),
    }
}

pub(super) fn pack_id_from_object_key(
    object_key: &str,
) -> Result<bowline_core::ids::PackId, SyncRunnerError> {
    let pack_id = object_key
        .strip_prefix("packs_")
        .ok_or(SyncRunnerError::MissingPackedLocator("object_key"))?;
    Ok(bowline_core::ids::PackId::new(pack_id))
}

pub(super) fn pack_epochs_by_id(
    pack_objects: &[bowline_control_plane::ObjectPointer],
) -> Result<BTreeMap<String, u32>, SyncRunnerError> {
    pack_objects
        .iter()
        .map(|pointer| {
            let pack_id = pack_id_from_object_key(&pointer.object_key)?;
            Ok((pack_id.as_str().to_string(), pointer.key_epoch))
        })
        .collect()
}

pub(super) fn materialize_snapshot_excluding(
    root: &Path,
    base: Option<&SnapshotContent>,
    target: &SnapshotContent,
    excluded_paths: &BTreeSet<String>,
) -> Result<(), SyncRunnerError> {
    let target_paths = target
        .manifest
        .entries
        .iter()
        .map(|entry| entry.path.as_str())
        .collect::<BTreeSet<_>>();

    if let Some(base) = base {
        let mut removed = base
            .manifest
            .entries
            .iter()
            .filter(|entry| {
                !target_paths.contains(entry.path.as_str())
                    && !is_excluded_materialization_path(&entry.path, excluded_paths)
            })
            .collect::<Vec<_>>();
        removed.sort_by_key(|entry| std::cmp::Reverse(entry.path.len()));
        for entry in removed {
            let absolute = root.join(&entry.path);
            match entry.kind {
                NamespaceEntryKind::File | NamespaceEntryKind::Symlink => {
                    remove_file_if_present(&absolute)?
                }
                NamespaceEntryKind::Directory => remove_empty_dir_if_present(&absolute)?,
                NamespaceEntryKind::Placeholder | NamespaceEntryKind::Tombstone => {}
            }
        }
    }

    let mut dirs = target
        .manifest
        .entries
        .iter()
        .filter(|entry| {
            entry.kind == NamespaceEntryKind::Directory
                && !is_excluded_materialization_path(&entry.path, excluded_paths)
        })
        .collect::<Vec<_>>();
    dirs.sort_by(|left, right| left.path.cmp(&right.path));
    for entry in dirs {
        ensure_directory_without_symlink(root, Path::new(&entry.path))?;
    }

    for entry in &target.manifest.entries {
        if is_excluded_materialization_path(&entry.path, excluded_paths) {
            continue;
        }
        match entry.kind {
            NamespaceEntryKind::File => {
                let Some(bytes) = target.file_bytes_for_path(&entry.path) else {
                    continue;
                };
                let relative_path = Path::new(&entry.path);
                prepare_parent_dirs(root, relative_path)?;
                let absolute = root.join(relative_path);
                write_materialized_file(
                    &absolute,
                    bytes,
                    materialized_file_requires_owner_only(&entry.path, entry.mode),
                )?;
            }
            NamespaceEntryKind::Symlink => {
                let Some(target_path) = &entry.symlink_target else {
                    continue;
                };
                validate_materialized_symlink_target(target_path)?;
                let relative_path = Path::new(&entry.path);
                prepare_parent_dirs(root, relative_path)?;
                let absolute = root.join(relative_path);
                write_materialized_symlink(&absolute, target_path)?;
            }
            NamespaceEntryKind::Directory => {}
            NamespaceEntryKind::Placeholder | NamespaceEntryKind::Tombstone => {}
        }
    }
    Ok(())
}

pub(super) fn write_materialized_symlink(path: &Path, target: &str) -> Result<(), SyncRunnerError> {
    let temp_path = materialization_temp_path(path)?;
    remove_file_if_present(&temp_path)?;
    std::os::unix::fs::symlink(target, &temp_path).map_err(SyncRunnerError::StateIo)?;
    remove_directory_for_file_materialization(path)?;
    fs::rename(&temp_path, path).map_err(SyncRunnerError::StateIo)?;
    Ok(())
}

pub(super) fn is_excluded_materialization_path(
    path: &str,
    excluded_paths: &BTreeSet<String>,
) -> bool {
    excluded_paths.iter().any(|excluded| {
        path == excluded
            || path
                .strip_prefix(excluded)
                .is_some_and(|suffix| suffix.starts_with('/'))
    })
}

pub(super) fn materialized_file_requires_owner_only(path: &str, mode: MaterializationMode) -> bool {
    matches!(
        mode,
        MaterializationMode::ProjectEnv | MaterializationMode::EncryptedSync
    ) || is_secret_bearing_path(path)
}

pub(super) fn is_secret_bearing_path(path: &str) -> bool {
    path.split('/')
        .any(|part| part == ".env" || part.starts_with(".env.") || part.ends_with(".env"))
}

pub(super) fn write_materialized_file(
    path: &Path,
    bytes: &[u8],
    owner_only: bool,
) -> Result<(), SyncRunnerError> {
    let temp_path = materialization_temp_path(path)?;
    remove_file_if_present(&temp_path)?;
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

        let mode = if owner_only { 0o600 } else { 0o644 };
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&temp_path)
            .map_err(SyncRunnerError::StateIo)?;
        file.write_all(bytes).map_err(SyncRunnerError::StateIo)?;
        file.sync_all().map_err(SyncRunnerError::StateIo)?;
    }

    #[cfg(not(unix))]
    {
        let _ = owner_only;
        fs::write(&temp_path, bytes).map_err(SyncRunnerError::StateIo)?;
    }
    remove_directory_for_file_materialization(path)?;
    fs::rename(&temp_path, path).map_err(SyncRunnerError::StateIo)?;
    Ok(())
}

pub(super) fn materialization_temp_path(path: &Path) -> Result<PathBuf, SyncRunnerError> {
    let Some(parent) = path.parent() else {
        return Err(SyncRunnerError::UnsafeMaterializationPath(
            path.display().to_string(),
        ));
    };
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let slug = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    let hash = blake3::hash(path.to_string_lossy().as_bytes());
    let suffix = hash.to_hex().chars().take(12).collect::<String>();
    Ok(parent.join(format!(".bowline-materialize-{slug}-{suffix}.tmp")))
}

pub(super) fn remove_directory_for_file_materialization(
    path: &Path,
) -> Result<(), SyncRunnerError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            remove_empty_dir_if_present(path)
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(SyncRunnerError::StateIo(error)),
    }
}

pub(super) fn validate_materialized_symlink_target(target: &str) -> Result<(), SyncRunnerError> {
    let normalized = normalize_workspace_path(target);
    if Path::new(target).is_absolute()
        || normalized != target
        || normalized.is_empty()
        || normalized == "."
        || normalized.starts_with("../")
        || normalized.contains("/../")
    {
        return Err(SyncRunnerError::UnsafeMaterializationPath(
            target.to_string(),
        ));
    }
    Ok(())
}

pub(super) fn prepare_parent_dirs(
    root: &Path,
    relative_path: &Path,
) -> Result<(), SyncRunnerError> {
    let Some(parent) = relative_path.parent() else {
        return Ok(());
    };
    if parent.as_os_str().is_empty() {
        return Ok(());
    }
    ensure_directory_without_symlink(root, parent)
}

pub(super) fn ensure_directory_without_symlink(
    root: &Path,
    relative_path: &Path,
) -> Result<(), SyncRunnerError> {
    let mut current = root.to_path_buf();
    for component in relative_path.components() {
        let std::path::Component::Normal(segment) = component else {
            return Err(SyncRunnerError::UnsafeMaterializationPath(
                relative_path.display().to_string(),
            ));
        };
        current.push(segment);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                fs::remove_file(&current).map_err(SyncRunnerError::StateIo)?;
                fs::create_dir(&current).map_err(SyncRunnerError::StateIo)?;
            }
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                fs::remove_file(&current).map_err(SyncRunnerError::StateIo)?;
                fs::create_dir(&current).map_err(SyncRunnerError::StateIo)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&current).map_err(SyncRunnerError::StateIo)?;
            }
            Err(error) => return Err(SyncRunnerError::StateIo(error)),
        }
    }
    Ok(())
}

pub(super) fn remove_file_if_present(path: &Path) -> Result<(), SyncRunnerError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(SyncRunnerError::StateIo(error)),
    }
}

pub(super) fn remove_empty_dir_if_present(path: &Path) -> Result<(), SyncRunnerError> {
    match fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(SyncRunnerError::StateIo(error)),
    }
}

pub(super) fn workspace_scoped_scan_report(
    workspace_id: &WorkspaceId,
    report: &crate::scanner::ScanReport,
) -> crate::scanner::ScanReport {
    let mut scoped = report.clone();
    let project_ids = scoped
        .projects
        .iter_mut()
        .map(|project| {
            let original = project.id.clone();
            project.id = workspace_scoped_project_id(workspace_id, &original);
            (original, project.id.clone())
        })
        .collect::<BTreeMap<_, _>>();
    for path in &mut scoped.paths {
        if let Some(project_id) = &path.project_id
            && let Some(scoped_project_id) = project_ids.get(project_id)
        {
            path.project_id = Some(scoped_project_id.clone());
        }
    }
    scoped
}

pub(super) fn workspace_scoped_project_id(
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
) -> ProjectId {
    ProjectId::new(format!(
        "proj_{}_{}",
        id_component(workspace_id.as_str()),
        id_component(project_id.as_str())
    ))
}

pub(super) fn workspace_scoped_root_id(workspace_id: &WorkspaceId) -> String {
    format!("root_{}", id_component(workspace_id.as_str()))
}

pub(super) fn id_component(value: &str) -> String {
    let mut output = String::new();
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            output.push(character.to_ascii_lowercase());
        } else {
            output.push('_');
        }
    }
    while output.contains("__") {
        output = output.replace("__", "_");
    }
    output.trim_matches('_').to_string()
}

pub(super) fn empty_snapshot_content(
    workspace_id: WorkspaceId,
    snapshot_id: SnapshotId,
) -> SnapshotContent {
    SnapshotContent::new(
        SnapshotManifest {
            schema_version: 1,
            snapshot_id: snapshot_id.clone(),
            workspace_id,
            project_id: None,
            kind: SnapshotKind::WorkspaceHead,
            base_snapshot_id: None,
            entries: Vec::new(),
            refs: vec![SnapshotRef {
                name: "workspace".to_string(),
                target_snapshot_id: snapshot_id,
                kind: RefKind::Workspace,
            }],
        },
        BTreeMap::new(),
    )
}

pub(super) fn empty_workspace_ref(workspace_id: WorkspaceId) -> WorkspaceRef {
    WorkspaceRef {
        workspace_id: workspace_id.as_str().to_string(),
        version: 0,
        snapshot_id: "empty".to_string(),
        updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 0 },
        updated_by_device_id: None,
    }
}

pub(super) fn conflict_files(
    record: &ConflictRecord,
    base: &SnapshotContent,
    local: &SnapshotContent,
    remote: &SnapshotContent,
) -> Vec<ConflictFile> {
    record
        .paths
        .iter()
        .map(|path| ConflictFile {
            relative_path: path.clone(),
            base: base.file_bytes_for_path(path).map(Vec::from),
            local: local.file_bytes_for_path(path).map(Vec::from),
            remote: remote.file_bytes_for_path(path).map(Vec::from),
        })
        .collect()
}

pub(super) fn conflict_kind_name(record: &ConflictRecord) -> &'static str {
    match record.conflict_kind {
        crate::sync::ConflictKind::Text => "text",
        crate::sync::ConflictKind::StructuredText => "structured-text",
        crate::sync::ConflictKind::Binary => "binary",
        crate::sync::ConflictKind::OpaqueGit => "opaque-git",
        crate::sync::ConflictKind::DeleteEdit => "delete-edit",
        crate::sync::ConflictKind::PathShape => "path-shape",
        crate::sync::ConflictKind::EnvKey => "env-key",
    }
}

pub(super) fn conflict_resolution_state(state: &str) -> Option<ConflictResolutionState> {
    match state {
        "accepted" => Some(ConflictResolutionState::Accepted),
        "rejected" => Some(ConflictResolutionState::Rejected),
        _ => None,
    }
}
