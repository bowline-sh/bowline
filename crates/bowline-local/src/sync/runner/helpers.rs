use super::permissions::MaterializedFilePermissions;
use super::worktree_registration::preserves_local_out_of_root_worktree_registration;
use super::*;
use bowline_core::{
    git_paths::is_git_derivable_volatile_path,
    git_worktree_link::{WorktreeLinkFile, worktree_link_file},
};
use std::io::Write as _;

// Single owner for the empty-snapshot sentinel lives in bowline-core; re-export
// it so the in-crate `helpers::EMPTY_SNAPSHOT_ID` call sites keep resolving.
pub(super) use bowline_core::hosted::EMPTY_SNAPSHOT_ID;

pub(super) fn checkpoint_payload<T: serde::Serialize>(
    payload: &T,
) -> Result<String, SyncRunnerError> {
    serde_json::to_string(payload).map_err(Into::into)
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SnapshotVersionPayload<'a> {
    pub(super) snapshot_id: &'a str,
    pub(super) version: u64,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SnapshotFileCountPayload<'a> {
    pub(super) snapshot_id: &'a str,
    pub(super) file_count: usize,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct NamespaceBuildPayload<'a> {
    pub(super) snapshot_id: &'a str,
    pub(super) namespace_root_id: &'a str,
    pub(super) namespace_pages_created: u64,
    pub(super) namespace_pages_reused: u64,
    pub(super) content_layouts_created: u64,
    pub(super) content_layouts_reused: u64,
    pub(super) semantic_entries_hashed: u64,
    pub(super) namespace_pages_loaded_during_build: u64,
    pub(super) namespace_pages_encoded: u64,
    pub(super) content_layouts_encoded: u64,
    pub(super) segment_pages_encoded: u64,
}

#[derive(serde::Serialize)]
pub(super) struct ReasonPayload<'a> {
    pub(super) reason: &'a str,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PackReuseUnavailablePayload<'a> {
    pub(super) reason: &'a str,
    pub(super) pack_id: &'a str,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SnapshotEntryCountPayload<'a> {
    pub(super) snapshot_id: &'a str,
    pub(super) entry_count: usize,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SnapshotPackLocatorCountPayload<'a> {
    pub(super) snapshot_id: &'a str,
    pub(super) pack_count: usize,
    pub(super) locator_count: usize,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SnapshotReasonPayload<'a> {
    pub(super) snapshot_id: &'a str,
    pub(super) reason: &'a str,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RemoteRefHistoryPayload<'a> {
    pub(super) source: &'a str,
    pub(super) caused_by_event_id: Option<&'a str>,
    pub(super) project_id: Option<&'a str>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct MergePluginApprovalPayload<'a> {
    policy_kind: &'a str,
    plugin_id: &'a str,
    plugin_version: &'a str,
    digest: &'a str,
    matcher_version: &'a str,
    validator_version: &'a str,
    module: &'a str,
    patterns: &'a [String],
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct MergePluginAppliedPayload<'a> {
    plugin_id: &'a str,
    plugin_version: &'a str,
    digest: &'a str,
    output_digest: &'a str,
    remote_snapshot_id: &'a str,
}

pub(super) fn merge_plugin_approval_payload(
    request: &crate::sync::merge_plugins::MergePluginApprovalRequest,
) -> serde_json::Map<String, serde_json::Value> {
    event_payload_object(&MergePluginApprovalPayload {
        policy_kind: "merge-plugin",
        plugin_id: &request.plugin.id,
        plugin_version: &request.plugin.version,
        digest: &request.plugin.digest,
        matcher_version: &request.plugin.matcher_version,
        validator_version: &request.plugin.validator_version,
        module: &request.module,
        patterns: &request.patterns,
    })
}

pub(super) fn merge_plugin_applied_payload(
    record: &MergePluginAuditRecord,
    remote_ref: &WorkspaceRef,
) -> serde_json::Map<String, serde_json::Value> {
    event_payload_object(&MergePluginAppliedPayload {
        plugin_id: &record.plugin.id,
        plugin_version: &record.plugin.version,
        digest: &record.plugin.digest,
        output_digest: &record.output_digest,
        remote_snapshot_id: &remote_ref.snapshot_id,
    })
}

fn event_payload_object<T: serde::Serialize>(
    payload: &T,
) -> serde_json::Map<String, serde_json::Value> {
    match serde_json::to_value(payload) {
        Ok(serde_json::Value::Object(payload)) => payload,
        Ok(_) => serde_json::Map::new(),
        Err(error) => {
            report_event_append_failure("serialize event payload", &error);
            serde_json::Map::new()
        }
    }
}

pub(super) fn report_event_append_failure(action: &str, error: &impl std::fmt::Display) {
    // Events are the audit trail; sync keeps moving, but every lost append path is surfaced through this one policy owner.
    eprintln!("bowline-sync event append failed during {action}: {error}");
}

pub(super) fn append_hydration_event(
    store: &MetadataStore,
    name: EventName,
    severity: EventSeverity,
    options: &SyncRunnerOptions,
    remote_ref: &WorkspaceRef,
    snapshot: Option<&SnapshotContent>,
    reason: Option<&str>,
) {
    let (file_count, byte_count) = snapshot
        .and_then(|snapshot| streamed_materialization_counts(snapshot).ok())
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
        hydration_event_id(&name, &remote_ref.snapshot_id, &options.generated_at),
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
        serde_json::Value::String(remote_ref.snapshot_id.as_str().to_string()),
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
    if let Err(error) = store.append_event(event) {
        report_event_append_failure("remote hydration event append", &error);
    }
}

fn streamed_materialization_counts(
    snapshot: &SnapshotContent,
) -> Result<(usize, u64), SyncRunnerError> {
    let mut counts = (0_usize, 0_u64);
    visit_snapshot_entries(snapshot, &mut |entry| {
        if entry.kind == NamespaceEntryKind::File {
            counts.0 = counts.0.saturating_add(1);
            counts.1 = counts.1.saturating_add(entry.byte_len.unwrap_or(0));
        }
        Ok(true)
    })?;
    Ok(counts)
}

pub(super) fn visit_snapshot_entries(
    snapshot: &SnapshotContent,
    visitor: &mut dyn FnMut(&NamespaceEntry) -> Result<bool, SyncRunnerError>,
) -> Result<(), SyncRunnerError> {
    visit_snapshot_prefix_entries(snapshot, &WorkspaceRelativePath::new(""), visitor)
}

pub(super) fn visit_snapshot_prefix_entries(
    snapshot: &SnapshotContent,
    prefix: &WorkspaceRelativePath,
    visitor: &mut dyn FnMut(&NamespaceEntry) -> Result<bool, SyncRunnerError>,
) -> Result<(), SyncRunnerError> {
    struct Adapter<'a> {
        visitor: &'a mut dyn FnMut(&NamespaceEntry) -> Result<bool, SyncRunnerError>,
        error: Option<SyncRunnerError>,
    }

    impl EntryVisitor for Adapter<'_> {
        fn visit(
            &mut self,
            entry: &NamespaceEntry,
            _context: &mut NamespaceOperationContext<'_>,
        ) -> Result<NamespaceVisitControl, NamespaceReadError> {
            match (self.visitor)(entry) {
                Ok(true) => Ok(NamespaceVisitControl::Continue),
                Ok(false) => Ok(NamespaceVisitControl::Stop),
                Err(error) => {
                    self.error = Some(error);
                    Ok(NamespaceVisitControl::Stop)
                }
            }
        }
    }

    let mut context = NamespaceOperationContext::uncancelled(
        crate::sync::namespace::operation_budget(snapshot.manifest().entry_count, 0, 0),
    );
    let mut adapter = Adapter {
        visitor,
        error: None,
    };
    snapshot.visit_prefix(prefix, &mut context, &mut adapter)?;
    adapter.error.map_or(Ok(()), Err)
}

pub(super) fn visit_snapshot_prefix_descriptors(
    snapshot: &SnapshotContent,
    prefix: &WorkspaceRelativePath,
    visitor: &mut dyn FnMut(
        &crate::sync::namespace::NamespaceEntryDescriptor,
    ) -> Result<bool, SyncRunnerError>,
) -> Result<(), SyncRunnerError> {
    let mut context = NamespaceOperationContext::uncancelled(
        crate::sync::namespace::operation_budget(snapshot.manifest().entry_count, 0, 0),
    );
    let mut visitor_error = None;
    snapshot.namespace_reader().visit_prefix_descriptors(
        prefix,
        &mut context,
        &mut |descriptor| match visitor(&descriptor) {
            Ok(true) => Ok(NamespaceVisitControl::Continue),
            Ok(false) => Ok(NamespaceVisitControl::Stop),
            Err(error) => {
                visitor_error = Some(error);
                Ok(NamespaceVisitControl::Stop)
            }
        },
    )?;
    visitor_error.map_or(Ok(()), Err)
}

pub(super) fn portable_git_worktree_link_entry(entry: &NamespaceEntry) -> Option<WorktreeLinkFile> {
    worktree_link_file(&entry.path, entry.kind)
}

pub(super) fn is_nonportable_derivable_git_entry(entry: &NamespaceEntry) -> bool {
    is_git_derivable_volatile_path(&entry.path) && portable_git_worktree_link_entry(entry).is_none()
}

pub(super) fn should_hydrate_imported_entry(
    entry: &NamespaceEntry,
    selection: &ImportedHydrationSelection,
) -> bool {
    if is_nonportable_derivable_git_entry(entry) {
        return false;
    }
    match selection {
        ImportedHydrationSelection::AllFiles => true,
        #[cfg(test)]
        ImportedHydrationSelection::RequiredFiles => {
            crate::sync::materialization::required_in_ordinary_directory(entry)
        }
        ImportedHydrationSelection::Paths(paths) => paths.contains(&entry.path),
    }
}

pub(super) fn hydration_event_id(name: &EventName, snapshot_id: &str, now: &str) -> EventId {
    EventId::new(format!(
        "evt_hydration_{}_{}_{}",
        hydration_event_name(name),
        snapshot_id,
        event_id_component(now)
    ))
}

pub(super) fn merge_plugin_event_id(name: EventName, stable_key: &str, now: &str) -> EventId {
    let name = format!("{name:?}");
    EventId::new(format!(
        "evt_merge_plugin_{}_{}",
        super::super::short_hash([name.as_bytes(), stable_key.as_bytes(), now.as_bytes()]),
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

pub(super) fn hydration_event_name(name: &EventName) -> &'static str {
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
    pack_pointers: &[bowline_control_plane::ObjectPointer],
) -> Result<BTreeMap<String, u32>, SyncRunnerError> {
    pack_pointers
        .iter()
        .map(|pointer| {
            let pack_id = pack_id_from_object_key(&pointer.object_key)?;
            Ok((pack_id.as_str().to_string(), pointer.key_epoch))
        })
        .collect()
}

pub(super) fn remove_materialized_entry(
    root: &Path,
    entry: &NamespaceEntry,
) -> Result<(), SyncRunnerError> {
    if preserves_local_out_of_root_worktree_registration(root, &entry.path)? {
        return Ok(());
    }
    let absolute = root.join(&entry.path);
    match entry.kind {
        NamespaceEntryKind::File | NamespaceEntryKind::Symlink => remove_file_if_present(&absolute),
        NamespaceEntryKind::Directory => {
            // Keep non-empty local directories to protect unsynced local children.
            let _ = remove_empty_dir_if_present(&absolute)?;
            Ok(())
        }
        NamespaceEntryKind::Placeholder | NamespaceEntryKind::Tombstone => Ok(()),
    }
}

pub(super) fn write_materialized_symlink(path: &Path, target: &str) -> Result<(), SyncRunnerError> {
    let temp_path = materialization_temp_path(path)?;
    remove_file_if_present(&temp_path)?;
    std::os::unix::fs::symlink(target, &temp_path).map_err(SyncRunnerError::StateIo)?;
    if let Err(error) = remove_directory_for_file_materialization(path) {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }
    if let Err(error) = fs::rename(&temp_path, path).map_err(SyncRunnerError::StateIo) {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }
    #[cfg(feature = "fault-injection")]
    crate::sync::fault::trip(crate::sync::fault::FaultPoint::AfterMaterializationRename)?;
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

pub(super) fn write_materialized_file(
    path: &Path,
    bytes: &[u8],
    permissions: MaterializedFilePermissions,
) -> Result<(), SyncRunnerError> {
    let temp_path = materialization_temp_path(path)?;
    remove_file_if_present(&temp_path)?;
    write_materialization_temp_file(&temp_path, bytes, permissions)?;
    if let Err(error) = remove_directory_for_file_materialization(path) {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }
    if let Err(error) = fs::rename(&temp_path, path).map_err(SyncRunnerError::StateIo) {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }
    #[cfg(feature = "fault-injection")]
    crate::sync::fault::trip(crate::sync::fault::FaultPoint::AfterMaterializationRename)?;
    Ok(())
}

fn write_materialization_temp_file(
    temp_path: &Path,
    bytes: &[u8],
    permissions: MaterializedFilePermissions,
) -> Result<(), SyncRunnerError> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mode = permissions.unix_mode();
        options.mode(mode);
        let mut file = options.open(temp_path).map_err(SyncRunnerError::StateIo)?;
        file.set_permissions(fs::Permissions::from_mode(mode))
            .map_err(SyncRunnerError::StateIo)?;
        file.write_all(bytes).map_err(SyncRunnerError::StateIo)?;
        file.sync_all().map_err(SyncRunnerError::StateIo)?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let mut file = options.open(temp_path).map_err(SyncRunnerError::StateIo)?;
        file.write_all(bytes).map_err(SyncRunnerError::StateIo)?;
        file.sync_all().map_err(SyncRunnerError::StateIo)?;
        Ok(())
    }
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
            match remove_empty_dir_if_present(path)? {
                DirRemoval::Removed | DirRemoval::NotPresent => Ok(()),
                DirRemoval::NotEmpty => Err(SyncRunnerError::MaterializationBlockedByDirectory(
                    path.display().to_string(),
                )),
            }
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(SyncRunnerError::StateIo(error)),
    }
}

pub(super) fn validate_materialized_symlink_target(target: &str) -> Result<(), SyncRunnerError> {
    if !bowline_core::workspace_graph::is_safe_workspace_symlink_target(target) {
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

pub(super) enum DirRemoval {
    Removed,
    NotPresent,
    NotEmpty,
}

pub(super) fn remove_empty_dir_if_present(path: &Path) -> Result<DirRemoval, SyncRunnerError> {
    match fs::remove_dir(path) {
        Ok(()) => Ok(DirRemoval::Removed),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(DirRemoval::NotPresent),
        Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => Ok(DirRemoval::NotEmpty),
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
    _snapshot_id: SnapshotId,
    workspace_content_key: [u8; 32],
) -> Result<SnapshotContent, bowline_core::namespace_snapshot::NamespaceBuildError> {
    let identity = crate::sync::rebuild_manifest_identity(&workspace_id, &[], "empty");
    let snapshot_id = identity.snapshot_id;
    SnapshotContent::new(
        bowline_core::workspace_graph::SnapshotDraft {
            schema_version: bowline_core::workspace_graph::SNAPSHOT_SCHEMA_VERSION,
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
        workspace_content_key,
    )
}

pub(super) fn empty_workspace_ref(workspace_id: WorkspaceId) -> WorkspaceRef {
    WorkspaceRef {
        workspace_id,
        version: 0,
        snapshot_id: SnapshotId::new(EMPTY_SNAPSHOT_ID),
        updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 0 },
        updated_by_device_id: None,
    }
}

pub(super) fn conflict_files(
    record: &ConflictRecord,
    base: &SnapshotContent,
    local: &SnapshotContent,
    remote: &SnapshotContent,
) -> Result<Vec<ConflictFile>, SyncRunnerError> {
    record
        .paths
        .iter()
        .map(|path| {
            Ok(ConflictFile {
                relative_path: path.clone(),
                base: base
                    .read_file_for_path(path)
                    .map_err(SyncRunnerError::StateIo)?,
                local: local
                    .read_file_for_path(path)
                    .map_err(SyncRunnerError::StateIo)?,
                remote: remote
                    .read_file_for_path(path)
                    .map_err(SyncRunnerError::StateIo)?,
            })
        })
        .collect()
}
