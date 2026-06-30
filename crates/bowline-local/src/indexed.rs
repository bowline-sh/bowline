use std::{
    collections::BTreeSet,
    error::Error,
    fmt, fs, io,
    path::{Path, PathBuf},
};

use bowline_core::{
    commands::{IndexSource, IndexState, IndexStatus},
    ids::{ContentId, ProjectId, SnapshotId, WorkspaceId},
    policy::{AccessFlag, MaterializationMode, PathClassification},
    workspace_graph::{HydrationState, NamespaceEntryKind, normalize_workspace_path},
};
use bowline_index::{
    AccessFlags, SearchOptions, SymbolLookupOptions, SymbolRecord, TextDocument, TextIndex, redact,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::{
    metadata::{
        DatabaseState, IndexDocumentRecord, IndexPackRecord, IndexWorkRecord, MetadataError,
        MetadataStore, SymbolIndexRecord, default_database_path,
    },
    policy::{PathFacts, PathPolicyDecision, UserPolicy, classify_path},
};

const MAX_INDEXED_FILE_BYTES: u64 = 1024 * 1024;

#[derive(Debug)]
pub enum IndexedError {
    Io(io::Error),
    Json(serde_json::Error),
    Metadata(MetadataError),
    MissingPath(PathBuf),
    NotDirectory(PathBuf),
}

impl fmt::Display for IndexedError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "index filesystem operation failed: {error}"),
            Self::Json(error) => write!(formatter, "index pack payload was invalid: {error}"),
            Self::Metadata(error) => error.fmt(formatter),
            Self::MissingPath(path) => {
                write!(formatter, "indexed path does not exist: {}", path.display())
            }
            Self::NotDirectory(path) => write!(
                formatter,
                "indexed path is not a directory: {}",
                path.display()
            ),
        }
    }
}

impl Error for IndexedError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::Metadata(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for IndexedError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<MetadataError> for IndexedError {
    fn from(error: MetadataError) -> Self {
        Self::Metadata(error)
    }
}

impl From<serde_json::Error> for IndexedError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

#[derive(Debug, Clone)]
pub struct IndexedProject {
    pub workspace_id: WorkspaceId,
    pub project_id: ProjectId,
    pub root: PathBuf,
    pub requested_path: String,
    pub snapshot_id: SnapshotId,
    pub text_index: TextIndex,
    pub symbol_index: bowline_index::SymbolIndex,
    pub documents: Vec<IndexedDocument>,
    pub index_status: IndexStatus,
}

#[derive(Debug, Clone)]
pub struct IndexedDocument {
    pub path: String,
    pub absolute_path: PathBuf,
    pub body: String,
    pub classification: PathClassification,
    pub mode: MaterializationMode,
    pub access: Vec<AccessFlag>,
    pub hydration_state: HydrationState,
}

#[derive(Debug, Clone)]
pub struct IndexedProjectIdentity {
    pub workspace_id: WorkspaceId,
    pub project_id: ProjectId,
    pub snapshot_id: Option<SnapshotId>,
    pub policy_path_prefix: Option<String>,
    pub max_scan_files: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct DecryptedIndexPackImport<'a> {
    pub db_path: &'a Path,
    pub workspace_id: WorkspaceId,
    pub project_id: ProjectId,
    pub snapshot_id: SnapshotId,
    pub object_key: String,
    pub byte_len: u64,
    pub hash: String,
    pub plaintext: &'a [u8],
    pub now: &'a str,
}

#[derive(Debug, Clone)]
enum IndexPurgeScope {
    All,
    Prefix(String),
    None,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IndexPackPayload {
    #[serde(default)]
    full_snapshot: bool,
    #[serde(default)]
    documents: Vec<IndexPackDocument>,
    #[serde(default)]
    symbols: Vec<IndexPackSymbol>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IndexPackDocument {
    path: String,
    body: String,
    #[serde(default)]
    content_id: Option<String>,
    #[serde(default)]
    policy_summary: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IndexPackSymbol {
    path: String,
    name: String,
    kind: String,
    language: String,
    #[serde(default)]
    line_start: u64,
    #[serde(default)]
    line_end: u64,
    #[serde(default)]
    byte_start: u64,
    #[serde(default)]
    byte_end: u64,
}

#[derive(Debug, Clone)]
pub struct LineMatch {
    pub line_start: u64,
    pub line_end: u64,
    pub snippet: String,
}

pub fn build_project_index(
    db_path: Option<PathBuf>,
    requested_path: Option<String>,
    now: &str,
) -> Result<IndexedProject, IndexedError> {
    build_project_index_inner(db_path, requested_path, None, now)
}

pub fn build_project_index_with_identity(
    db_path: Option<PathBuf>,
    requested_path: Option<String>,
    identity: IndexedProjectIdentity,
    now: &str,
) -> Result<IndexedProject, IndexedError> {
    build_project_index_inner(db_path, requested_path, Some(identity), now)
}

pub fn import_decrypted_index_pack(
    options: DecryptedIndexPackImport<'_>,
) -> Result<usize, IndexedError> {
    let payload: IndexPackPayload = serde_json::from_slice(options.plaintext)?;
    let mut store = MetadataStore::open(options.db_path)?;
    let policy_root = resolve_project_policy_root(
        Some(options.db_path),
        &options.workspace_id,
        &options.project_id,
    )?;
    store.upsert_index_pack(&IndexPackRecord {
        workspace_id: options.workspace_id.clone(),
        project_id: Some(options.project_id.clone()),
        snapshot_id: Some(options.snapshot_id.clone()),
        object_key: options.object_key,
        byte_len: options.byte_len,
        hash: options.hash,
        state: "ready".to_string(),
        updated_at: options.now.to_string(),
    })?;

    let mut imported = 0_usize;
    let mut imported_paths = BTreeSet::new();
    let full_snapshot = payload.full_snapshot;
    for (ordinal, document) in payload.documents.into_iter().enumerate() {
        let Some(path) = clean_index_pack_path(&document.path) else {
            continue;
        };
        let policy = policy_root
            .as_ref()
            .map(|root| {
                UserPolicy::load_for_path(root, &path).unwrap_or_else(|_| UserPolicy::empty())
            })
            .unwrap_or_else(UserPolicy::empty);
        let decision = classify_path(
            &PathFacts {
                relative_path: path.clone(),
                is_dir: false,
                byte_len: Some(document.body.len() as u64),
            },
            &policy,
        );
        if !indexable_for_agents(&decision) {
            let _ = store.purge_index_path(&options.workspace_id, &options.project_id, &path)?;
            continue;
        }
        let source_watermark = ordinal as u64 + 1;
        store.upsert_index_document(&IndexDocumentRecord {
            workspace_id: options.workspace_id.clone(),
            project_id: Some(options.project_id.clone()),
            path: path.clone(),
            snapshot_id: Some(options.snapshot_id.clone()),
            content_id: document.content_id.map(ContentId::new),
            classification: decision.classification,
            mode: decision.mode,
            access: decision.access.clone(),
            policy_summary: document
                .policy_summary
                .unwrap_or_else(|| decision.summary.clone()),
            body_text: document.body.clone(),
            hydration_state: HydrationState::Cold,
            indexed_bytes: document.body.len() as u64,
            source_watermark,
            indexed_watermark: source_watermark,
            state: "ready".to_string(),
            updated_at: options.now.to_string(),
        })?;
        imported_paths.insert(path);
        imported += 1;
    }
    if full_snapshot {
        store.purge_index_paths_for_snapshot_except(
            &options.workspace_id,
            &options.project_id,
            &options.snapshot_id,
            &imported_paths,
        )?;
    }
    let mut symbols_by_path = std::collections::BTreeMap::<String, Vec<SymbolIndexRecord>>::new();
    for (ordinal, symbol) in payload.symbols.into_iter().enumerate() {
        let Some(path) = clean_index_pack_path(&symbol.path) else {
            continue;
        };
        if !imported_paths.contains(&path) {
            continue;
        }
        symbols_by_path
            .entry(path.clone())
            .or_default()
            .push(SymbolIndexRecord {
                id: format!(
                    "sym_pack_{}_{}_{}_{}",
                    options.workspace_id.as_str(),
                    options.project_id.as_str(),
                    stable_hex(path.as_bytes()),
                    ordinal
                ),
                workspace_id: options.workspace_id.clone(),
                project_id: Some(options.project_id.clone()),
                path,
                snapshot_id: Some(options.snapshot_id.clone()),
                name: symbol.name,
                kind: symbol.kind,
                language: symbol.language,
                line_start: symbol.line_start,
                line_end: symbol.line_end,
                byte_start: symbol.byte_start,
                byte_end: symbol.byte_end,
                parser_status: "Ready".to_string(),
                access: vec![AccessFlag::HumanReadable, AccessFlag::AgentReadable],
                updated_at: options.now.to_string(),
            });
    }
    for path in imported_paths {
        let records = symbols_by_path.remove(&path).unwrap_or_default();
        store.replace_symbol_records_for_path(
            &options.workspace_id,
            &options.project_id,
            &path,
            records.as_slice(),
        )?;
        store.mark_index_work_ready_under_prefix(
            &options.workspace_id,
            &options.project_id,
            &path,
            options.now,
        )?;
    }
    Ok(imported)
}

fn build_project_index_inner(
    db_path: Option<PathBuf>,
    requested_path: Option<String>,
    identity: Option<IndexedProjectIdentity>,
    now: &str,
) -> Result<IndexedProject, IndexedError> {
    let root = resolve_requested_root(requested_path)?;
    if !root.exists() {
        return Err(IndexedError::MissingPath(root));
    }
    if !root.is_dir() {
        return Err(IndexedError::NotDirectory(root));
    }
    let requested_path = root.display().to_string();
    let policy_path_prefix = identity
        .as_ref()
        .and_then(|identity| identity.policy_path_prefix.clone());
    let max_scan_files = identity
        .as_ref()
        .and_then(|identity| identity.max_scan_files);
    let identity_provided = identity.is_some();
    let db_path = usable_metadata_db_path_for_request(db_path, &requested_path, identity_provided)?;
    let (workspace_id, project_id, snapshot_id) =
        resolve_ids(db_path.clone(), &requested_path, identity)?;
    let persisted_policy_root =
        resolve_project_policy_root(db_path.as_deref(), &workspace_id, &project_id)?;
    let policy_root = persisted_policy_root
        .clone()
        .unwrap_or_else(|| fallback_policy_root(&root));
    let scope_prefix = scope_prefix_for_request(&policy_root, &root, policy_path_prefix.as_deref());
    if let Some(db_path) = db_path.as_deref()
        && let Some(project) = build_from_durable_index(
            db_path,
            &workspace_id,
            &project_id,
            &snapshot_id,
            policy_root.clone(),
            requested_path.clone(),
            scope_prefix.as_deref(),
            now,
        )?
    {
        return Ok(project);
    }

    let mut text_index = TextIndex::new(now.to_string());
    let mut symbol_index = bowline_index::SymbolIndex::new(now.to_string());
    let mut documents = Vec::new();
    let mut local_covered_paths = BTreeSet::new();
    let mut indexed_bytes = 0_u64;
    let mut path_count = 0_u64;

    let walked = walk_readable_files(&root, max_scan_files)?;
    for file in walked.files {
        path_count += 1;
        let metadata = fs::metadata(&file)?;
        let relative = relative_path(&root, &file);
        let policy_relative = policy_relative_path(
            &policy_root,
            &file,
            &relative,
            policy_path_prefix.as_deref(),
        );
        local_covered_paths.insert(normalize_workspace_path(&policy_relative));
        if metadata.len() > MAX_INDEXED_FILE_BYTES {
            continue;
        }
        let policy = UserPolicy::load_for_path(&policy_root, &policy_relative)
            .unwrap_or_else(|_| UserPolicy::empty());
        let decision = classify_path(
            &PathFacts {
                relative_path: policy_relative.clone(),
                is_dir: false,
                byte_len: Some(metadata.len()),
            },
            &policy,
        );
        if !indexable_for_agents(&decision) {
            continue;
        }
        let Ok(bytes) = fs::read(&file) else {
            continue;
        };
        let Ok(body) = String::from_utf8(bytes) else {
            continue;
        };
        let access = decision.access.clone();
        let document = IndexedDocument {
            path: policy_relative.clone(),
            absolute_path: file,
            body: body.clone(),
            classification: decision.classification,
            mode: decision.mode,
            access: access.clone(),
            hydration_state: HydrationState::Local,
        };
        indexed_bytes += body.len() as u64;
        let _ = text_index.upsert(TextDocument {
            path: policy_relative.clone(),
            project_id: project_id.as_str().to_string(),
            snapshot_id: snapshot_id.as_str().to_string(),
            content_id: Some(format!("cid_{}", stable_hex(policy_relative.as_bytes()))),
            body: body.clone(),
            classification: map_path_classification(decision.classification),
            hydration_state: bowline_index::HydrationState::Hydrated,
            policy_summary: decision.summary.clone(),
            access: AccessFlags::readable(),
            source_watermark: path_count,
        });
        if let Some(language) = language_for_path(&policy_relative) {
            let _ = symbol_index.upsert(bowline_index::SymbolDocument {
                path: policy_relative.clone(),
                project_id: project_id.as_str().to_string(),
                snapshot_id: snapshot_id.as_str().to_string(),
                language,
                source: body.clone(),
                access: AccessFlags::readable(),
                source_watermark: path_count,
            });
        }
        for record in manifest_symbol_records(
            &policy_relative,
            &project_id,
            &snapshot_id,
            &body,
            access_flags_for_row(&access),
        ) {
            symbol_index.insert_record(record);
        }
        documents.push(document);
    }

    let projected = projected_coverage(db_path.as_deref(), &workspace_id, &project_id)?;
    let projected_file_count = projected.as_ref().map_or(0, |coverage| coverage.file_count);
    let projected_pending_count = projected.as_ref().map_or(0, |coverage| {
        coverage
            .file_count
            .saturating_sub(documents.len() as u64)
            .max(coverage.cold_file_count)
    });
    let (state, pending_path_count, summary) = if walked.truncated {
        (
            IndexState::Stale,
            None,
            "Local index stopped at the configured exploration file bound.".to_string(),
        )
    } else if projected_pending_count > 0 {
        (
            IndexState::Stale,
            Some(projected_pending_count),
            format!(
                "Local index covers materialized readable files; {projected_pending_count} projected file(s) still need indexed content."
            ),
        )
    } else {
        (
            IndexState::Ready,
            Some(0),
            "Local index covers currently materialized policy-readable files.".to_string(),
        )
    };

    let index_status = IndexStatus {
        state,
        source: IndexSource::Local,
        indexed_at: Some(now.to_string()),
        updated_at: Some(now.to_string()),
        snapshot_id: Some(snapshot_id.clone()),
        index_pack_object_key: None,
        path_count: path_count.max(projected_file_count),
        file_count: documents.len() as u64,
        indexed_bytes,
        pending_path_count,
        degraded_reason: None,
        summary,
        next_action: None,
    };

    let project = IndexedProject {
        workspace_id,
        project_id,
        root,
        requested_path,
        snapshot_id,
        text_index,
        symbol_index,
        documents,
        index_status,
    };
    if let Some(db_path) = db_path.as_deref() {
        let purge_scope = if walked.truncated {
            IndexPurgeScope::None
        } else {
            match scope_prefix.clone() {
                Some(prefix) => IndexPurgeScope::Prefix(prefix),
                None => IndexPurgeScope::All,
            }
        };
        persist_project_index(db_path, &project, now, purge_scope, &local_covered_paths)?;
        if let Some(merged) = build_from_durable_index(
            db_path,
            &project.workspace_id,
            &project.project_id,
            &project.snapshot_id,
            policy_root.clone(),
            project.requested_path.clone(),
            scope_prefix.as_deref(),
            now,
        )? {
            return Ok(merged);
        }
    }
    Ok(project)
}

#[allow(clippy::too_many_arguments)]
fn build_from_durable_index(
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

fn persist_project_index(
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

fn projected_index_paths_for_project(
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

fn project_relative_index_path(path: &str, project_path: &str) -> String {
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
struct ProjectedCoverage {
    file_count: u64,
    cold_file_count: u64,
}

fn projected_coverage(
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

fn resolve_project_policy_root(
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

fn fallback_policy_root(requested_root: &Path) -> PathBuf {
    let mut discovered = None;
    for ancestor in requested_root.ancestors() {
        if ancestor.join(".bowlineignore").is_file() {
            discovered = Some(ancestor.to_path_buf());
        }
    }
    discovered.unwrap_or_else(|| requested_root.to_path_buf())
}

fn policy_relative_path(
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

fn clean_index_pack_path(path: &str) -> Option<String> {
    let raw = path.replace('\\', "/");
    if raw.starts_with('/') || raw.contains(':') {
        return None;
    }
    let normalized = normalize_workspace_path(&raw);
    if normalized.is_empty()
        || normalized
            .split('/')
            .any(|component| component.is_empty() || matches!(component, "." | ".."))
    {
        return None;
    }
    Some(normalized)
}

fn scope_prefix_for_request(
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

fn scope_allows_path(scope_prefix: Option<&str>, path: &str) -> bool {
    let Some(prefix) = scope_prefix else {
        return true;
    };
    let prefix = normalize_workspace_path(prefix);
    let path = normalize_workspace_path(path);
    prefix.is_empty() || path == prefix || path.starts_with(&format!("{prefix}/"))
}

fn index_work_affects_scope(work_path: Option<&str>, scope_prefix: Option<&str>) -> bool {
    let Some(scope_prefix) = scope_prefix else {
        return true;
    };
    match work_path {
        Some(path) => scope_allows_path(Some(scope_prefix), path),
        None => true,
    }
}

fn index_work_has_complete_local_scan(work: &[IndexWorkRecord]) -> bool {
    ["namespace", "text", "symbols"].iter().all(|kind| {
        work.iter().any(|record| {
            record.path.is_none()
                && record.kind == *kind
                && record.state == "ready"
                && record.source_watermark == record.indexed_watermark
        })
    })
}

fn scope_has_local_files(project_root: &Path, scope_prefix: Option<&str>) -> bool {
    let root = scope_prefix
        .map(|prefix| project_root.join(normalize_workspace_path(prefix)))
        .unwrap_or_else(|| project_root.to_path_buf());
    walk_readable_files(&root, Some(1))
        .map(|walked| !walked.files.is_empty())
        .unwrap_or(false)
}

fn scope_has_unindexed_local_files(
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

fn durable_index_row_is_fresh(project_root: &Path, row: &IndexDocumentRecord) -> bool {
    let path = project_root.join(normalize_workspace_path(&row.path));
    match fs::read_to_string(path) {
        Ok(body) => body == row.body_text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            row.hydration_state != HydrationState::Local
        }
        Err(_) => row.hydration_state != HydrationState::Local,
    }
}

pub fn search_options(path_prefix: Option<String>, limit: usize) -> SearchOptions {
    search_options_with_offset(path_prefix, limit, 0)
}

pub fn search_options_with_offset(
    path_prefix: Option<String>,
    limit: usize,
    offset: usize,
) -> SearchOptions {
    SearchOptions {
        path_prefix: path_prefix.map(|path| normalize_query_path(&path)),
        limit,
        offset,
    }
}

pub fn symbol_options(path_prefix: Option<String>, limit: usize) -> SymbolLookupOptions {
    symbol_options_with_offset(path_prefix, limit, 0)
}

pub fn symbol_options_with_offset(
    path_prefix: Option<String>,
    limit: usize,
    offset: usize,
) -> SymbolLookupOptions {
    SymbolLookupOptions {
        path_prefix: path_prefix.map(|path| normalize_query_path(&path)),
        limit,
        offset,
    }
}

pub fn line_match(body: &str, query: &str) -> Option<LineMatch> {
    let terms = query
        .split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>();
    if terms.is_empty() {
        return None;
    }
    for (index, line) in body.lines().enumerate() {
        let lower = line.to_lowercase();
        if terms.iter().all(|term| lower.contains(term))
            || terms.iter().any(|term| lower.contains(term))
        {
            return Some(LineMatch {
                line_start: index as u64 + 1,
                line_end: index as u64 + 1,
                snippet: redact(line.trim()),
            });
        }
    }
    None
}

pub fn document_for_path<'a>(
    project: &'a IndexedProject,
    path: &str,
) -> Option<&'a IndexedDocument> {
    project
        .documents
        .iter()
        .find(|document| document.path == path)
}

fn resolve_requested_root(requested_path: Option<String>) -> io::Result<PathBuf> {
    let path = requested_path
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir()?);
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn resolve_ids(
    db_path: Option<PathBuf>,
    requested_path: &str,
    identity: Option<IndexedProjectIdentity>,
) -> Result<(WorkspaceId, ProjectId, SnapshotId), IndexedError> {
    if let Some(identity) = identity {
        let snapshot_id = match identity.snapshot_id {
            Some(snapshot_id) => snapshot_id,
            None => resolve_project_snapshot_id(
                db_path.as_ref(),
                &identity.workspace_id,
                &identity.project_id,
                requested_path,
            )?,
        };
        return Ok((identity.workspace_id, identity.project_id, snapshot_id));
    }

    let db_path = usable_metadata_db_path(db_path);
    if let Some(db_path) = db_path {
        let store = MetadataStore::open(db_path)?;
        if let Some(workspace) = store.current_workspace()? {
            if let Some(project) = store.current_project_by_path(requested_path)? {
                let snapshot_id = store
                    .project_latest_snapshot_id(&workspace.id, &project.id)?
                    .unwrap_or_else(|| {
                        SnapshotId::new(format!("snap_{}", stable_hex(project.path.as_bytes())))
                    });
                return Ok((workspace.id, project.id, snapshot_id));
            }
            return Ok((
                workspace.id,
                ProjectId::new(format!("proj_{}", stable_hex(requested_path.as_bytes()))),
                SnapshotId::new(format!("snap_{}", stable_hex(requested_path.as_bytes()))),
            ));
        }
    }

    Ok((
        WorkspaceId::new("ws_local"),
        ProjectId::new(format!("proj_{}", stable_hex(requested_path.as_bytes()))),
        SnapshotId::new(format!("snap_{}", stable_hex(requested_path.as_bytes()))),
    ))
}

fn resolve_project_snapshot_id(
    db_path: Option<&PathBuf>,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
    requested_path: &str,
) -> Result<SnapshotId, IndexedError> {
    if let Some(db_path) = db_path {
        let store = MetadataStore::open(db_path)?;
        if let Some(snapshot_id) = store.project_latest_snapshot_id(workspace_id, project_id)? {
            return Ok(snapshot_id);
        }
    }
    Ok(SnapshotId::new(format!(
        "snap_{}",
        stable_hex(requested_path.as_bytes())
    )))
}

fn usable_metadata_db_path(requested: Option<PathBuf>) -> Option<PathBuf> {
    let path = match requested {
        Some(path) => path,
        None => default_database_path().ok()?,
    };
    if !path.exists() {
        return None;
    }
    matches!(MetadataStore::inspect(&path).state, DatabaseState::Current).then_some(path)
}

fn usable_metadata_db_path_for_request(
    requested: Option<PathBuf>,
    requested_path: &str,
    identity_provided: bool,
) -> Result<Option<PathBuf>, IndexedError> {
    let Some(path) = usable_metadata_db_path(requested) else {
        return Ok(None);
    };
    if identity_provided {
        return Ok(Some(path));
    }
    let store = MetadataStore::open(&path)?;
    if store.current_project_by_path(requested_path)?.is_some() {
        Ok(Some(path))
    } else {
        Ok(None)
    }
}

struct WalkedFiles {
    files: Vec<PathBuf>,
    truncated: bool,
}

fn walk_readable_files(root: &Path, max_files: Option<usize>) -> io::Result<WalkedFiles> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    let mut seen = BTreeSet::new();
    let mut truncated = false;
    while let Some(dir) = stack.pop() {
        if max_files.is_some_and(|max_files| files.len() >= max_files) {
            truncated = true;
            break;
        }
        if !seen.insert(dir.clone()) {
            continue;
        }
        let Ok(read_dir) = fs::read_dir(&dir) else {
            continue;
        };
        let mut entries = read_dir.flatten().collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.path());
        for entry in entries {
            if max_files.is_some_and(|max_files| files.len() >= max_files) {
                truncated = true;
                break;
            }
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                if !skip_dir(&path) {
                    stack.push(path);
                }
            } else if file_type.is_file() && !skip_file(&path) {
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(WalkedFiles { files, truncated })
}

fn skip_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            matches!(
                name,
                ".git"
                    | "node_modules"
                    | "target"
                    | ".next"
                    | ".turbo"
                    | "dist"
                    | "build"
                    | "__pycache__"
            )
        })
}

fn skip_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == ".DS_Store" || name.ends_with(".lock"))
}

fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn normalize_query_path(path: &str) -> String {
    path.trim_matches('/').replace('\\', "/")
}

fn indexable_for_agents(decision: &PathPolicyDecision) -> bool {
    decision.access.contains(&AccessFlag::AgentReadable)
        && !decision.access.contains(&AccessFlag::AgentHidden)
        && !matches!(
            decision.classification,
            PathClassification::ProjectEnv
                | PathClassification::SecretLooking
                | PathClassification::LocalOnly
                | PathClassification::Blocked
                | PathClassification::Generated
                | PathClassification::Dependency
                | PathClassification::Cache
        )
}

fn map_path_classification(
    classification: PathClassification,
) -> bowline_index::PathClassification {
    match classification {
        PathClassification::ProjectEnv | PathClassification::SecretLooking => {
            bowline_index::PathClassification::Secret
        }
        PathClassification::Generated
        | PathClassification::Dependency
        | PathClassification::Cache => bowline_index::PathClassification::Generated,
        PathClassification::LargeFile => bowline_index::PathClassification::Binary,
        _ => bowline_index::PathClassification::Source,
    }
}

fn map_hydration_state(state: HydrationState) -> bowline_index::HydrationState {
    match state {
        HydrationState::Local => bowline_index::HydrationState::Hydrated,
        HydrationState::Cold => bowline_index::HydrationState::Cold,
        HydrationState::StructureOnly => bowline_index::HydrationState::Partial,
        HydrationState::Missing => bowline_index::HydrationState::Cold,
    }
}

fn access_flags_for_row(access: &[AccessFlag]) -> AccessFlags {
    AccessFlags {
        policy_readable: access.contains(&AccessFlag::HumanReadable)
            || access.contains(&AccessFlag::AgentReadable),
        lease_readable: access.contains(&AccessFlag::AgentReadable),
        generated: false,
        local_only: false,
    }
}

fn access_summary(access: &[AccessFlag]) -> String {
    if access.is_empty() {
        return "policy:unknown".to_string();
    }
    access
        .iter()
        .map(|flag| format!("{flag:?}"))
        .collect::<Vec<_>>()
        .join(",")
}

fn symbol_record_to_store(
    project: &IndexedProject,
    document: &IndexedDocument,
    record: SymbolRecord,
    index: usize,
    now: &str,
) -> SymbolIndexRecord {
    SymbolIndexRecord {
        id: format!(
            "sym_{}_{}_{}_{}_{}",
            project.workspace_id.as_str(),
            project.project_id.as_str(),
            stable_hex(document.path.as_bytes()),
            stable_hex(record.name.as_bytes()),
            index
        ),
        workspace_id: project.workspace_id.clone(),
        project_id: Some(project.project_id.clone()),
        path: document.path.clone(),
        snapshot_id: Some(project.snapshot_id.clone()),
        name: record.name,
        kind: format!("{:?}", record.kind),
        language: format!("{:?}", record.language),
        line_start: record.line_range.start as u64,
        line_end: record.line_range.end as u64,
        byte_start: record.byte_range.start as u64,
        byte_end: record.byte_range.end as u64,
        parser_status: format!("{:?}", record.parser_status),
        access: document.access.clone(),
        updated_at: now.to_string(),
    }
}

fn symbol_record_from_store(record: &SymbolIndexRecord) -> Option<SymbolRecord> {
    Some(SymbolRecord {
        name: record.name.clone(),
        kind: parse_symbol_kind(&record.kind)?,
        language: parse_language(&record.language)?,
        path: record.path.clone(),
        project_id: record.project_id.as_ref()?.as_str().to_string(),
        snapshot_id: record.snapshot_id.as_ref()?.as_str().to_string(),
        byte_range: record.byte_start as usize..record.byte_end as usize,
        line_range: record.line_start as usize..record.line_end as usize,
        parser_status: parse_index_readiness(&record.parser_status)?,
        access: access_flags_for_row(&record.access),
    })
}

fn parse_symbol_kind(value: &str) -> Option<bowline_index::SymbolKind> {
    match value {
        "Function" => Some(bowline_index::SymbolKind::Function),
        "Class" => Some(bowline_index::SymbolKind::Class),
        "Interface" => Some(bowline_index::SymbolKind::Interface),
        "Variable" => Some(bowline_index::SymbolKind::Variable),
        "Struct" => Some(bowline_index::SymbolKind::Struct),
        "Enum" => Some(bowline_index::SymbolKind::Enum),
        "Trait" => Some(bowline_index::SymbolKind::Trait),
        "Type" => Some(bowline_index::SymbolKind::Type),
        "Import" => Some(bowline_index::SymbolKind::Import),
        "Export" => Some(bowline_index::SymbolKind::Export),
        _ => None,
    }
}

fn parse_language(value: &str) -> Option<bowline_index::Language> {
    match value {
        "TypeScript" => Some(bowline_index::Language::TypeScript),
        "JavaScript" => Some(bowline_index::Language::JavaScript),
        "Python" => Some(bowline_index::Language::Python),
        "Rust" => Some(bowline_index::Language::Rust),
        "Go" => Some(bowline_index::Language::Go),
        _ => None,
    }
}

fn parse_index_readiness(value: &str) -> Option<bowline_index::IndexReadiness> {
    match value {
        "Ready" => Some(bowline_index::IndexReadiness::Ready),
        "Stale" => Some(bowline_index::IndexReadiness::Stale),
        "Rebuilding" => Some(bowline_index::IndexReadiness::Rebuilding),
        "Degraded" => Some(bowline_index::IndexReadiness::Degraded),
        _ => None,
    }
}

pub fn language_for_path(path: &str) -> Option<bowline_index::Language> {
    match Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
    {
        Some("ts" | "tsx") => Some(bowline_index::Language::TypeScript),
        Some("js" | "jsx" | "mjs" | "cjs") => Some(bowline_index::Language::JavaScript),
        Some("py") => Some(bowline_index::Language::Python),
        Some("rs") => Some(bowline_index::Language::Rust),
        Some("go") => Some(bowline_index::Language::Go),
        _ => None,
    }
}

fn manifest_symbol_records(
    path: &str,
    project_id: &ProjectId,
    snapshot_id: &SnapshotId,
    body: &str,
    access: AccessFlags,
) -> Vec<SymbolRecord> {
    let file_name = Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    match file_name {
        "package.json" => package_json_symbols(path, project_id, snapshot_id, body, access),
        "Cargo.toml" => cargo_toml_symbols(path, project_id, snapshot_id, body, access),
        "pyproject.toml" => pyproject_symbols(path, project_id, snapshot_id, body, access),
        "go.mod" => go_mod_symbols(path, project_id, snapshot_id, body, access),
        _ => Vec::new(),
    }
}

fn package_json_symbols(
    path: &str,
    project_id: &ProjectId,
    snapshot_id: &SnapshotId,
    body: &str,
    access: AccessFlags,
) -> Vec<SymbolRecord> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    let mut records = Vec::new();
    if let Some(name) = value.get("name").and_then(|value| value.as_str()) {
        records.push(manifest_symbol(
            path,
            project_id,
            snapshot_id,
            name,
            bowline_index::SymbolKind::Export,
            bowline_index::Language::JavaScript,
            body,
            access,
        ));
    }
    for section in ["dependencies", "devDependencies", "peerDependencies"] {
        if let Some(object) = value.get(section).and_then(|value| value.as_object()) {
            for name in object.keys() {
                records.push(manifest_symbol(
                    path,
                    project_id,
                    snapshot_id,
                    name,
                    bowline_index::SymbolKind::Import,
                    bowline_index::Language::JavaScript,
                    body,
                    access,
                ));
            }
        }
    }
    records
}

fn cargo_toml_symbols(
    path: &str,
    project_id: &ProjectId,
    snapshot_id: &SnapshotId,
    body: &str,
    access: AccessFlags,
) -> Vec<SymbolRecord> {
    let Ok(value) = toml::from_str::<toml::Value>(body) else {
        return Vec::new();
    };
    let mut records = Vec::new();
    if let Some(name) = value
        .get("package")
        .and_then(|package| package.get("name"))
        .and_then(|name| name.as_str())
    {
        records.push(manifest_symbol(
            path,
            project_id,
            snapshot_id,
            name,
            bowline_index::SymbolKind::Export,
            bowline_index::Language::Rust,
            body,
            access,
        ));
    }
    for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(table) = value.get(section).and_then(|value| value.as_table()) {
            for name in table.keys() {
                records.push(manifest_symbol(
                    path,
                    project_id,
                    snapshot_id,
                    name,
                    bowline_index::SymbolKind::Import,
                    bowline_index::Language::Rust,
                    body,
                    access,
                ));
            }
        }
    }
    records
}

fn pyproject_symbols(
    path: &str,
    project_id: &ProjectId,
    snapshot_id: &SnapshotId,
    body: &str,
    access: AccessFlags,
) -> Vec<SymbolRecord> {
    let Ok(value) = toml::from_str::<toml::Value>(body) else {
        return Vec::new();
    };
    let mut records = Vec::new();
    if let Some(name) = value
        .get("project")
        .and_then(|project| project.get("name"))
        .and_then(|name| name.as_str())
    {
        records.push(manifest_symbol(
            path,
            project_id,
            snapshot_id,
            name,
            bowline_index::SymbolKind::Export,
            bowline_index::Language::Python,
            body,
            access,
        ));
    }
    if let Some(dependencies) = value
        .get("project")
        .and_then(|project| project.get("dependencies"))
        .and_then(|dependencies| dependencies.as_array())
    {
        for dependency in dependencies.iter().filter_map(|value| value.as_str()) {
            if let Some(name) = package_name_from_requirement(dependency) {
                records.push(manifest_symbol(
                    path,
                    project_id,
                    snapshot_id,
                    &name,
                    bowline_index::SymbolKind::Import,
                    bowline_index::Language::Python,
                    body,
                    access,
                ));
            }
        }
    }
    records
}

fn go_mod_symbols(
    path: &str,
    project_id: &ProjectId,
    snapshot_id: &SnapshotId,
    body: &str,
    access: AccessFlags,
) -> Vec<SymbolRecord> {
    let mut records = Vec::new();
    let mut in_require_block = false;
    for line in body.lines().map(str::trim) {
        if line.starts_with("//") || line.is_empty() {
            continue;
        }
        if in_require_block {
            if line == ")" {
                in_require_block = false;
                continue;
            }
            if let Some(module) = first_requirement_token(line) {
                records.push(manifest_symbol(
                    path,
                    project_id,
                    snapshot_id,
                    module,
                    bowline_index::SymbolKind::Import,
                    bowline_index::Language::Go,
                    body,
                    access,
                ));
            }
            continue;
        }
        if let Some(module) = line
            .strip_prefix("module ")
            .and_then(first_requirement_token)
        {
            records.push(manifest_symbol(
                path,
                project_id,
                snapshot_id,
                module,
                bowline_index::SymbolKind::Export,
                bowline_index::Language::Go,
                body,
                access,
            ));
        } else if line == "require (" {
            in_require_block = true;
        } else if let Some(module) = line
            .strip_prefix("require ")
            .and_then(first_requirement_token)
        {
            records.push(manifest_symbol(
                path,
                project_id,
                snapshot_id,
                module,
                bowline_index::SymbolKind::Import,
                bowline_index::Language::Go,
                body,
                access,
            ));
        }
    }
    records
}

#[allow(clippy::too_many_arguments)]
fn manifest_symbol(
    path: &str,
    project_id: &ProjectId,
    snapshot_id: &SnapshotId,
    name: impl Into<String>,
    kind: bowline_index::SymbolKind,
    language: bowline_index::Language,
    body: &str,
    access: AccessFlags,
) -> SymbolRecord {
    SymbolRecord {
        name: name.into(),
        kind,
        language,
        path: path.to_string(),
        project_id: project_id.as_str().to_string(),
        snapshot_id: snapshot_id.as_str().to_string(),
        byte_range: 0..body.len(),
        line_range: 1..2,
        parser_status: bowline_index::IndexReadiness::Ready,
        access,
    }
}

fn package_name_from_requirement(requirement: &str) -> Option<String> {
    first_requirement_token(requirement)
        .map(|token| {
            token
                .chars()
                .take_while(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
                .collect()
        })
        .filter(|name: &String| !name.is_empty())
}

fn first_requirement_token(input: &str) -> Option<&str> {
    input
        .split_whitespace()
        .next()
        .filter(|token| !token.is_empty())
}

fn stable_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let hash = hasher.finalize();
    hash.iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bowline_core::commands::{IndexSource, IndexState};
    use bowline_index::{IndexReadiness, Language, SymbolKind};

    #[test]
    fn symbol_record_ids_are_workspace_scoped() {
        let document = IndexedDocument {
            path: "src/lib.ts".to_string(),
            absolute_path: PathBuf::from("/tmp/Code/apps/web/src/lib.ts"),
            body: "export function boot() { return true; }\n".to_string(),
            classification: PathClassification::WorkspaceSync,
            mode: MaterializationMode::WorkspaceSync,
            access: vec![AccessFlag::HumanReadable, AccessFlag::AgentReadable],
            hydration_state: HydrationState::Local,
        };
        let symbol = SymbolRecord {
            name: "boot".to_string(),
            kind: SymbolKind::Function,
            language: Language::TypeScript,
            path: document.path.clone(),
            project_id: "proj_web".to_string(),
            snapshot_id: "snap_web".to_string(),
            byte_range: 16..20,
            line_range: 1..1,
            parser_status: IndexReadiness::Ready,
            access: AccessFlags {
                policy_readable: true,
                lease_readable: true,
                generated: false,
                local_only: false,
            },
        };

        let first = indexed_project_for_symbol_id_test("ws_one");
        let second = indexed_project_for_symbol_id_test("ws_two");

        let first_record = symbol_record_to_store(&first, &document, symbol.clone(), 0, "now");
        let second_record = symbol_record_to_store(&second, &document, symbol, 0, "now");

        assert_ne!(first_record.id, second_record.id);
        assert!(first_record.id.contains("ws_one"));
        assert!(second_record.id.contains("ws_two"));
    }

    fn indexed_project_for_symbol_id_test(workspace_id: &str) -> IndexedProject {
        IndexedProject {
            workspace_id: WorkspaceId::new(workspace_id),
            project_id: ProjectId::new("proj_web"),
            root: PathBuf::from("/tmp/Code/apps/web"),
            requested_path: "/tmp/Code/apps/web".to_string(),
            snapshot_id: SnapshotId::new("snap_web"),
            text_index: TextIndex::new("now"),
            symbol_index: bowline_index::SymbolIndex::new("now"),
            documents: Vec::new(),
            index_status: IndexStatus {
                state: IndexState::Ready,
                source: IndexSource::Local,
                indexed_at: Some("now".to_string()),
                updated_at: Some("now".to_string()),
                snapshot_id: Some(SnapshotId::new("snap_web")),
                index_pack_object_key: None,
                path_count: 0,
                file_count: 0,
                indexed_bytes: 0,
                pending_path_count: None,
                degraded_reason: None,
                summary: "ready".to_string(),
                next_action: None,
            },
        }
    }
}
