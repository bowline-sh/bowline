use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct CacheDeleteReport {
    pub(super) file_deleted: bool,
    pub(super) bytes_deleted: u64,
}

#[derive(Debug)]
pub(super) struct CacheDeletePlan {
    path: PathBuf,
    bytes: u64,
}

pub(super) fn gc_cache_record(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    record: &MetadataRecordRef,
) -> Result<Option<MetadataCacheRecord>, MetadataError> {
    if record.kind == MetadataRecordKind::SnapshotRoot {
        return Ok(None);
    }
    store.metadata_cache_record(workspace_id, record)
}

pub(super) fn begin_candidate_cache_delete(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    candidate: &MetadataGcCandidate,
) -> Result<Option<CacheDeletePlan>, MetadataError> {
    begin_cache_delete(
        store,
        workspace_id,
        &candidate.record,
        candidate.cache_path.as_deref(),
        candidate.cache_bytes,
    )
}

pub(super) fn finish_candidate_cache_delete(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    candidate: &MetadataGcCandidate,
) -> Result<(), MetadataError> {
    let eligible = store.connection.query_row(
        "SELECT EXISTS(
           SELECT 1 FROM metadata_gc_queue AS queue
           JOIN metadata_object_bindings AS bindings
             ON bindings.workspace_id = queue.workspace_id
            AND bindings.record_kind = queue.record_kind
            AND bindings.logical_id = queue.logical_id
           WHERE queue.workspace_id = ?1 AND queue.generation = ?2
             AND queue.record_kind = ?3 AND queue.logical_id = ?4
             AND queue.state = 'delete-eligible' AND bindings.object_key = ?5
         )",
        params![
            workspace_id.as_str(),
            candidate.generation,
            candidate.record.kind.as_str(),
            candidate.record.logical_id.as_str(),
            candidate.object_key.as_str(),
        ],
        |row| row.get::<_, bool>(0),
    )?;
    if !eligible {
        return Err(MetadataError::InvalidStorageMetadata(
            "metadata GC candidate stopped being delete-eligible".to_string(),
        ));
    }
    finish_cache_delete(
        store,
        workspace_id,
        &candidate.record,
        candidate.cache_path.as_deref(),
        candidate.cache_bytes,
    )
}

pub(super) fn begin_cache_delete(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    record: &MetadataRecordRef,
    expected_path: Option<&str>,
    expected_bytes: u64,
) -> Result<Option<CacheDeletePlan>, MetadataError> {
    let current =
        matching_cache_record(store, workspace_id, record, expected_path, expected_bytes)?;
    let Some(cache_path) = expected_path else {
        return Ok(None);
    };
    let current = current.ok_or_else(|| {
        MetadataError::InvalidStorageMetadata(
            "metadata cache candidate is missing during GC finalization".to_string(),
        )
    })?;
    if !matches!(
        current.state,
        MetadataCacheState::Present | MetadataCacheState::Deleting
    ) {
        return Err(MetadataError::InvalidStorageMetadata(
            "metadata cache candidate is not available for deletion".to_string(),
        ));
    }
    let path = validated_cache_path(store, cache_path)?;
    let table = cache_table(record.kind)?;
    let updated = store.connection.execute(
        &format!(
            "UPDATE {table} SET cache_state = 'deleting'
             WHERE workspace_id = ?1 AND logical_id = ?2 AND cache_path = ?3
               AND encoded_bytes = ?4 AND cache_state IN ('present', 'deleting')"
        ),
        params![
            workspace_id.as_str(),
            record.logical_id.as_str(),
            cache_path,
            expected_bytes,
        ],
    )?;
    if updated != 1 {
        return Err(MetadataError::InvalidStorageMetadata(
            "metadata cache candidate changed while entering deletion".to_string(),
        ));
    }
    Ok(Some(CacheDeletePlan {
        path,
        bytes: expected_bytes,
    }))
}

pub(super) fn finish_cache_delete(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    record: &MetadataRecordRef,
    expected_path: Option<&str>,
    expected_bytes: u64,
) -> Result<(), MetadataError> {
    let current =
        matching_cache_record(store, workspace_id, record, expected_path, expected_bytes)?;
    if expected_path.is_some()
        && !current.is_some_and(|cache| cache.state == MetadataCacheState::Deleting)
    {
        return Err(MetadataError::InvalidStorageMetadata(
            "metadata cache deletion lost its durable deleting state".to_string(),
        ));
    }
    Ok(())
}

pub(super) fn execute_cache_delete(
    plan: Option<CacheDeletePlan>,
) -> Result<CacheDeleteReport, MetadataError> {
    let Some(plan) = plan else {
        return Ok(CacheDeleteReport::default());
    };
    #[cfg(test)]
    if delete_faults()
        .lock()
        .expect("metadata cache delete fault lock")
        .contains(&plan.path)
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "injected metadata cache delete failure",
        )
        .into());
    }
    match fs::remove_file(&plan.path) {
        Ok(()) => Ok(CacheDeleteReport {
            file_deleted: true,
            bytes_deleted: plan.bytes,
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(CacheDeleteReport::default()),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn advance_sweep_cursor(
    store: &MetadataStore,
    checkpoint: &MetadataGcCheckpoint,
    record: &MetadataRecordRef,
    now: &str,
) -> Result<(), MetadataError> {
    let updated = store.connection.execute(
        "UPDATE metadata_gc_checkpoints
         SET sweep_cursor_kind = ?3, sweep_cursor_id = ?4, updated_at = ?5
         WHERE workspace_id = ?1 AND generation = ?2 AND phase = 'sweep'",
        params![
            checkpoint.workspace_id.as_str(),
            checkpoint.generation,
            record.kind.as_str(),
            record.logical_id.as_str(),
            now,
        ],
    )?;
    if updated != 1 {
        return Err(MetadataError::InvalidStorageMetadata(
            "metadata GC generation changed during sweep finalization".to_string(),
        ));
    }
    Ok(())
}

fn matching_cache_record(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    record: &MetadataRecordRef,
    expected_path: Option<&str>,
    expected_bytes: u64,
) -> Result<Option<MetadataCacheRecord>, MetadataError> {
    let current = gc_cache_record(store, workspace_id, record)?;
    let current_path = current
        .as_ref()
        .and_then(|cache| cache.cache_path.as_deref());
    let current_bytes = current.as_ref().map_or(0, |cache| cache.encoded_bytes);
    if current_path != expected_path || current_bytes != expected_bytes {
        return Err(MetadataError::InvalidStorageMetadata(
            "metadata cache candidate changed during GC finalization".to_string(),
        ));
    }
    Ok(current)
}

fn validated_cache_path(store: &MetadataStore, cache_path: &str) -> Result<PathBuf, MetadataError> {
    let database_path = store.connection.path().map(PathBuf::from).ok_or_else(|| {
        MetadataError::InvalidStorageMetadata(
            "metadata database has no filesystem path for cache GC".to_string(),
        )
    })?;
    let cache_root = database_path
        .parent()
        .ok_or_else(|| {
            MetadataError::InvalidStorageMetadata(
                "metadata database path has no state root for cache GC".to_string(),
            )
        })?
        .join("metadata-pages");
    let cache_path = PathBuf::from(cache_path);
    if !cache_path.is_absolute()
        || !matches!(cache_path.file_name(), Some(name) if !name.is_empty())
    {
        return Err(MetadataError::InvalidStorageMetadata(
            "metadata cache candidate must be an absolute file path".to_string(),
        ));
    }
    let canonical_root = fs::canonicalize(&cache_root)?;
    let canonical_parent = fs::canonicalize(cache_path.parent().ok_or_else(|| {
        MetadataError::InvalidStorageMetadata(
            "metadata cache candidate has no parent directory".to_string(),
        )
    })?)?;
    if !canonical_parent.starts_with(&canonical_root) {
        return Err(MetadataError::InvalidStorageMetadata(
            "metadata cache candidate escapes the cache root".to_string(),
        ));
    }
    match fs::symlink_metadata(&cache_path) {
        Ok(metadata) if metadata.file_type().is_dir() => {
            Err(MetadataError::InvalidStorageMetadata(
                "metadata cache candidate is not a file".to_string(),
            ))
        }
        Ok(_) => Ok(cache_path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(cache_path),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
fn delete_faults() -> &'static std::sync::Mutex<BTreeSet<PathBuf>> {
    static FAULTS: std::sync::OnceLock<std::sync::Mutex<BTreeSet<PathBuf>>> =
        std::sync::OnceLock::new();
    FAULTS.get_or_init(|| std::sync::Mutex::new(BTreeSet::new()))
}

#[cfg(test)]
pub(super) fn set_cache_delete_fault(path: &PathBuf, enabled: bool) {
    let mut faults = delete_faults()
        .lock()
        .expect("metadata cache delete fault lock");
    if enabled {
        faults.insert(path.clone());
    } else {
        faults.remove(path);
    }
}
