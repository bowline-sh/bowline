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

mod durable;
mod error;
mod packs;
mod symbols;
#[cfg(test)]
mod tests;

use durable::*;
pub use error::IndexedError;
pub use packs::import_decrypted_index_pack;
use symbols::*;

const MAX_INDEXED_FILE_BYTES: u64 = 1024 * 1024;

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
