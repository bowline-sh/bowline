use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) fn build_from_durable_index(
    db_path: &Path,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
    snapshot_id: &SnapshotId,
    project_root: PathBuf,
    requested_path: String,
    scope_prefix: Option<&str>,
    now: &str,
) -> Result<Option<IndexedProject>, IndexedError> {
    let store = MetadataStore::open(db_path)?;
    let work = store.index_work_for_project(workspace_id, project_id)?;
    if work
        .iter()
        .filter(|record| index_work_affects_scope(record.path.as_deref(), scope_prefix))
        .any(|record| record.state != "ready" || record.source_watermark > record.indexed_watermark)
    {
        return Ok(None);
    }
    if !index_work_has_complete_local_scan(&work)
        && scope_has_local_files(&project_root, scope_prefix)
    {
        return Ok(None);
    }
    let rows = store
        .index_documents_for_project(workspace_id, project_id)?
        .into_iter()
        .filter(|row| row.snapshot_id.as_ref() == Some(snapshot_id))
        .collect::<Vec<_>>();
    if rows.is_empty() {
        return Ok(None);
    }

    let policy_root = resolve_project_policy_root(Some(db_path), workspace_id, project_id)?
        .unwrap_or_else(|| project_root.clone());
    let projected_paths =
        projected_index_paths_for_project(&store, workspace_id, project_id, &policy_root)?;
    let durable_paths = rows
        .iter()
        .map(|row| row.path.clone())
        .collect::<BTreeSet<_>>();
    if scope_has_unindexed_local_files(&project_root, &policy_root, scope_prefix, &durable_paths) {
        return Ok(None);
    }
    let mut text_index = TextIndex::new(now.to_string());
    let mut documents = Vec::new();
    let mut indexed_bytes = 0_u64;
    let mut allowed_paths = BTreeSet::new();
    for row in rows {
        if !scope_allows_path(scope_prefix, &row.path) {
            continue;
        }
        if row.hydration_state == HydrationState::Cold
            && !projected_paths.is_empty()
            && !projected_paths.contains(&row.path)
        {
            let _ = store.purge_index_path(workspace_id, project_id, &row.path)?;
            continue;
        }
        if !durable_index_row_is_fresh(&project_root, &row) {
            return Ok(None);
        }
        let policy = UserPolicy::load_for_path(&policy_root, &row.path)
            .unwrap_or_else(|_| UserPolicy::empty());
        let decision = classify_path(
            &PathFacts {
                relative_path: row.path.clone(),
                is_dir: false,
                byte_len: Some(row.body_text.len() as u64),
            },
            &policy,
        );
        if !indexable_for_agents(&decision) {
            let _ = store.purge_index_path(workspace_id, project_id, &row.path)?;
            continue;
        }
        indexed_bytes += row.indexed_bytes;
        let _ = text_index.upsert(TextDocument {
            path: row.path.clone(),
            project_id: project_id.as_str().to_string(),
            snapshot_id: row
                .snapshot_id
                .as_ref()
                .unwrap_or(snapshot_id)
                .as_str()
                .to_string(),
            content_id: row.content_id.as_ref().map(|id| id.as_str().to_string()),
            body: row.body_text.clone(),
            classification: map_path_classification(decision.classification),
            hydration_state: map_hydration_state(row.hydration_state),
            policy_summary: decision.summary.clone(),
            access: access_flags_for_row(&decision.access),
            source_watermark: row.source_watermark,
        });
        allowed_paths.insert(row.path.clone());
        documents.push(IndexedDocument {
            path: row.path.clone(),
            absolute_path: project_root.join(&row.path),
            body: row.body_text,
            classification: decision.classification,
            mode: decision.mode,
            access: decision.access,
            hydration_state: row.hydration_state,
        });
    }
    if documents.is_empty() {
        return Ok(None);
    }

    let mut symbol_index = bowline_index::SymbolIndex::new(now.to_string());
    for record in store.symbol_records_for_project(workspace_id, project_id)? {
        if record.snapshot_id.as_ref() != Some(snapshot_id) {
            continue;
        }
        if !allowed_paths.contains(&record.path) {
            continue;
        }
        if let Some(symbol) = symbol_record_from_store(&record) {
            symbol_index.insert_record(symbol);
        }
    }

    let path_count = documents.len() as u64;
    Ok(Some(IndexedProject {
        workspace_id: workspace_id.clone(),
        project_id: project_id.clone(),
        root: project_root,
        requested_path,
        snapshot_id: snapshot_id.clone(),
        text_index,
        symbol_index,
        documents,
        index_status: IndexStatus {
            state: IndexState::Ready,
            source: IndexSource::Local,
            indexed_at: Some(now.to_string()),
            updated_at: Some(now.to_string()),
            snapshot_id: Some(snapshot_id.clone()),
            index_pack_object_key: None,
            path_count,
            file_count: path_count,
            indexed_bytes,
            pending_path_count: Some(0),
            degraded_reason: None,
            summary: "Local index loaded from durable metadata rows.".to_string(),
            next_action: None,
        },
    }))
}

pub(super) fn persist_project_index(
    db_path: &Path,
    project: &IndexedProject,
    now: &str,
    purge_scope: IndexPurgeScope,
    local_covered_paths: &BTreeSet<String>,
) -> Result<(), IndexedError> {
    let mut store = MetadataStore::open(db_path)?;
    let mut max_watermark = 0_u64;
    let keep_paths = project
        .documents
        .iter()
        .map(|document| normalize_workspace_path(&document.path))
        .collect::<BTreeSet<_>>();
    let policy_root =
        resolve_project_policy_root(Some(db_path), &project.workspace_id, &project.project_id)?
            .unwrap_or_else(|| project.root.clone());
    let projected_keep_paths = projected_index_paths_for_project(
        &store,
        &project.workspace_id,
        &project.project_id,
        &policy_root,
    )?;
    let complete_project_rebuild = matches!(&purge_scope, IndexPurgeScope::All);
    match &purge_scope {
        IndexPurgeScope::All => {
            store.purge_index_paths_except(
                &project.workspace_id,
                &project.project_id,
                &keep_paths,
                &projected_keep_paths,
            )?;
        }
        IndexPurgeScope::Prefix(prefix) => {
            store.purge_index_paths_under_prefix_except(
                &project.workspace_id,
                &project.project_id,
                prefix,
                &keep_paths,
                &projected_keep_paths,
            )?;
        }
        IndexPurgeScope::None => {}
    }
    for (ordinal, document) in project.documents.iter().enumerate() {
        let source_watermark = ordinal as u64 + 1;
        max_watermark = max_watermark.max(source_watermark);
        store.upsert_index_document(&IndexDocumentRecord {
            workspace_id: project.workspace_id.clone(),
            project_id: Some(project.project_id.clone()),
            path: document.path.clone(),
            snapshot_id: Some(project.snapshot_id.clone()),
            content_id: Some(ContentId::new(format!(
                "cid_{}",
                stable_hex(document.path.as_bytes())
            ))),
            classification: document.classification,
            mode: document.mode,
            access: document.access.clone(),
            policy_summary: access_summary(&document.access),
            body_text: document.body.clone(),
            hydration_state: document.hydration_state,
            indexed_bytes: document.body.len() as u64,
            source_watermark,
            indexed_watermark: source_watermark,
            state: "ready".to_string(),
            updated_at: now.to_string(),
        })?;

        let records = project
            .symbol_index
            .records_for_path(&document.path)
            .into_iter()
            .enumerate()
            .map(|(index, record)| symbol_record_to_store(project, document, record, index, now))
            .collect::<Vec<_>>();
        store.replace_symbol_records_for_path(
            &project.workspace_id,
            &project.project_id,
            &document.path,
            &records,
        )?;
    }

    if complete_project_rebuild {
        for kind in ["namespace", "text", "symbols"] {
            store.upsert_index_work(&IndexWorkRecord {
                id: format!(
                    "index_work:{}:{}:{kind}",
                    project.workspace_id.as_str(),
                    project.project_id.as_str()
                ),
                workspace_id: project.workspace_id.clone(),
                project_id: Some(project.project_id.clone()),
                path: None,
                kind: kind.to_string(),
                source_watermark: max_watermark,
                indexed_watermark: max_watermark,
                state: "ready".to_string(),
                reason: None,
                updated_at: now.to_string(),
            })?;
        }
        store.mark_index_work_ready_for_paths(
            &project.workspace_id,
            &project.project_id,
            local_covered_paths,
            now,
        )?;
        store.mark_local_write_index_work_ready_for_scope(
            &project.workspace_id,
            &project.project_id,
            None,
            now,
        )?;
    } else if matches!(&purge_scope, IndexPurgeScope::Prefix(_)) {
        store.mark_index_work_ready_for_paths(
            &project.workspace_id,
            &project.project_id,
            local_covered_paths,
            now,
        )?;
        if let IndexPurgeScope::Prefix(prefix) = &purge_scope {
            store.mark_local_write_index_work_ready_for_scope(
                &project.workspace_id,
                &project.project_id,
                Some(prefix),
                now,
            )?;
        }
    }
    Ok(())
}

pub(super) fn projected_index_paths_for_project(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
    project_root: &Path,
) -> Result<BTreeSet<String>, IndexedError> {
    let mut paths = BTreeSet::new();
    let project_path = store
        .project_by_id(workspace_id, project_id)?
        .map(|project| normalize_workspace_path(&project.path))
        .unwrap_or_default();
    for node in store.projected_nodes_for_workspace(workspace_id)? {
        if node.kind != NamespaceEntryKind::File {
            continue;
        }
        if let Some(node_project_id) = node.project_id.as_ref()
            && node_project_id != project_id
        {
            continue;
        }
        let node_path = Path::new(&node.path);
        let relative = if node_path.is_absolute() {
            node_path
                .strip_prefix(project_root)
                .map(|path| path.to_string_lossy().replace('\\', "/"))
                .ok()
        } else if node.project_id.as_ref() == Some(project_id)
            || project_path.is_empty()
            || normalize_workspace_path(&node.path) == project_path
            || normalize_workspace_path(&node.path).starts_with(&format!("{project_path}/"))
        {
            Some(project_relative_index_path(&node.path, &project_path))
        } else {
            None
        };
        if let Some(relative) = relative {
            let relative = normalize_workspace_path(&relative);
            if !relative.is_empty() {
                paths.insert(relative);
            }
        }
    }
    Ok(paths)
}

pub(super) fn project_relative_index_path(path: &str, project_path: &str) -> String {
    let path = normalize_workspace_path(path);
    let project_path = normalize_workspace_path(project_path);
    if project_path.is_empty() {
        return path;
    }
    path.strip_prefix(&format!("{project_path}/"))
        .map(str::to_string)
        .unwrap_or(path)
}

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct ProjectedCoverage {
    pub(super) file_count: u64,
    pub(super) cold_file_count: u64,
}

pub(super) fn projected_coverage(
    db_path: Option<&Path>,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
) -> Result<Option<ProjectedCoverage>, IndexedError> {
    let Some(db_path) = db_path else {
        return Ok(None);
    };
    let store = MetadataStore::open(db_path)?;
    let mut coverage = ProjectedCoverage::default();
    for node in store.projected_nodes_for_project(workspace_id, project_id)? {
        if node.kind != NamespaceEntryKind::File {
            continue;
        }
        coverage.file_count += 1;
        if node.hydration_state != HydrationState::Local {
            coverage.cold_file_count += 1;
        }
    }
    Ok((coverage.file_count > 0).then_some(coverage))
}

pub(super) fn resolve_project_policy_root(
    db_path: Option<&Path>,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
) -> Result<Option<PathBuf>, IndexedError> {
    let Some(db_path) = db_path else {
        return Ok(None);
    };
    let store = MetadataStore::open(db_path)?;
    let Some(root) = store.current_workspace_root()? else {
        return Ok(None);
    };
    let Some(project) = store.project_by_id(workspace_id, project_id)? else {
        return Ok(None);
    };
    Ok(Some(
        PathBuf::from(root).join(normalize_workspace_path(&project.path)),
    ))
}

pub(super) fn fallback_policy_root(requested_root: &Path) -> PathBuf {
    let mut discovered = None;
    for ancestor in requested_root.ancestors() {
        if ancestor.join(".bowlineignore").is_file() {
            discovered = Some(ancestor.to_path_buf());
        }
    }
    discovered.unwrap_or_else(|| requested_root.to_path_buf())
}

pub(super) fn policy_relative_path(
    policy_root: &Path,
    file: &Path,
    fallback: &str,
    prefix: Option<&str>,
) -> String {
    file.strip_prefix(policy_root)
        .map(|relative| relative.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| match prefix {
            Some(prefix) => format!(
                "{}/{}",
                normalize_workspace_path(prefix).trim_end_matches('/'),
                normalize_workspace_path(fallback).trim_start_matches('/')
            ),
            None => fallback.to_string(),
        })
}

pub(super) fn scope_prefix_for_request(
    policy_root: &Path,
    requested_root: &Path,
    explicit_prefix: Option<&str>,
) -> Option<String> {
    if let Some(prefix) = explicit_prefix {
        let normalized = normalize_workspace_path(prefix);
        if !normalized.is_empty() {
            return Some(normalized);
        }
    }
    let relative = requested_root.strip_prefix(policy_root).ok()?;
    let normalized = normalize_workspace_path(&relative.to_string_lossy());
    (!normalized.is_empty()).then_some(normalized)
}

pub(super) fn scope_allows_path(scope_prefix: Option<&str>, path: &str) -> bool {
    let Some(prefix) = scope_prefix else {
        return true;
    };
    let prefix = normalize_workspace_path(prefix);
    let path = normalize_workspace_path(path);
    prefix.is_empty() || path == prefix || path.starts_with(&format!("{prefix}/"))
}

pub(super) fn index_work_affects_scope(
    work_path: Option<&str>,
    scope_prefix: Option<&str>,
) -> bool {
    let Some(scope_prefix) = scope_prefix else {
        return true;
    };
    match work_path {
        Some(path) => scope_allows_path(Some(scope_prefix), path),
        None => true,
    }
}

pub(super) fn index_work_has_complete_local_scan(work: &[IndexWorkRecord]) -> bool {
    ["namespace", "text", "symbols"].iter().all(|kind| {
        work.iter().any(|record| {
            record.path.is_none()
                && record.kind == *kind
                && record.state == "ready"
                && record.source_watermark == record.indexed_watermark
        })
    })
}

pub(super) fn scope_has_local_files(project_root: &Path, scope_prefix: Option<&str>) -> bool {
    let root = scope_prefix
        .map(|prefix| project_root.join(normalize_workspace_path(prefix)))
        .unwrap_or_else(|| project_root.to_path_buf());
    walk_readable_files(&root, Some(1))
        .map(|walked| !walked.files.is_empty())
        .unwrap_or(false)
}

pub(super) fn scope_has_unindexed_local_files(
    project_root: &Path,
    policy_root: &Path,
    scope_prefix: Option<&str>,
    indexed_paths: &BTreeSet<String>,
) -> bool {
    let root = scope_prefix
        .map(|prefix| project_root.join(normalize_workspace_path(prefix)))
        .unwrap_or_else(|| project_root.to_path_buf());
    let Ok(walked) = walk_readable_files(&root, None) else {
        return false;
    };
    for file in walked.files {
        let relative = normalize_workspace_path(&relative_path(project_root, &file));
        if indexed_paths.contains(&relative) || !scope_allows_path(scope_prefix, &relative) {
            continue;
        }
        let Ok(metadata) = fs::metadata(&file) else {
            continue;
        };
        if metadata.len() > MAX_INDEXED_FILE_BYTES {
            continue;
        }
        let policy = UserPolicy::load_for_path(policy_root, &relative)
            .unwrap_or_else(|_| UserPolicy::empty());
        let decision = classify_path(
            &PathFacts {
                relative_path: relative,
                is_dir: false,
                byte_len: Some(metadata.len()),
            },
            &policy,
        );
        if indexable_for_agents(&decision) {
            return true;
        }
    }
    false
}

pub(super) fn durable_index_row_is_fresh(project_root: &Path, row: &IndexDocumentRecord) -> bool {
    let path = project_root.join(normalize_workspace_path(&row.path));
    match fs::read_to_string(path) {
        Ok(body) => body == row.body_text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            row.hydration_state != HydrationState::Local
        }
        Err(_) => row.hydration_state != HydrationState::Local,
    }
}
