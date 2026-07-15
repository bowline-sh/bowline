use super::*;

pub(super) fn discover_conflicts(path: &Path) -> Vec<ResolveConflict> {
    let mut conflicts = Vec::new();
    if let Some(conflict) = read_bundle(path) {
        conflicts.push(conflict);
        return conflicts;
    }

    for root in conflict_roots_for(path) {
        if let Ok(entries) = fs::read_dir(root) {
            for entry in entries.flatten() {
                if let Some(conflict) = read_bundle(&entry.path()) {
                    conflicts.push(conflict);
                }
            }
        }
    }
    conflicts
}

pub(super) fn conflict_roots_for(path: &Path) -> Vec<PathBuf> {
    let mut roots = vec![path.join(PRIVATE_STATE_ROOT).join("conflicts")];
    if let Some(state_root) = state_root_for_conflicts() {
        roots.push(state_root.join("conflicts"));
    }
    roots
}

pub(super) fn state_root_for_conflicts() -> Option<PathBuf> {
    if let Some(path) = env::var_os(ENV_STATE_ROOT).map(PathBuf::from) {
        return Some(path);
    }
    bowline_local::metadata::default_database_path()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
}

pub(super) fn read_bundle(path: &Path) -> Option<ResolveConflict> {
    let manifest_path = path.join("manifest.json");
    if !manifest_path.is_file()
        || !path.join("base").is_dir()
        || !path.join("local").is_dir()
        || !path.join("remote").is_dir()
        || !path.join("resolution").is_dir()
    {
        return None;
    }

    let manifest = fs::read(&manifest_path).ok()?;
    let record: ConflictRecord = serde_json::from_slice(&manifest).ok()?;
    if record.state != ConflictState::Unresolved {
        return None;
    }

    Some(ResolveConflict {
        id: record.id,
        occurrence_version: record.occurrence_version,
        state: "unresolved".to_string(),
        bundle_path: path.display().to_string(),
        conflict_kind: conflict_kind_name(record.conflict_kind).to_string(),
        workspace_root: record.workspace_root.map(|root| root.display().to_string()),
        reason: record.reason,
        affected_files: record.paths,
        spans: record
            .spans
            .into_iter()
            .map(|span| ResolveConflictSpan {
                path: span.path,
                base_start_line: span.base_start_line,
                base_end_line: span.base_end_line,
                local_start_line: span.local_start_line,
                local_end_line: span.local_end_line,
                remote_start_line: span.remote_start_line,
                remote_end_line: span.remote_end_line,
                base_context_hash: span.base_context_hash,
                local_context_hash: span.local_context_hash,
                remote_context_hash: span.remote_context_hash,
            })
            .collect(),
        active_view: match record.active_view {
            bowline_local::sync::ConflictActiveView::Local => "local",
            bowline_local::sync::ConflictActiveView::Remote => "remote",
        }
        .to_string(),
        has_resolution_overlay: path.join("resolution").is_dir(),
        contains_secrets: record.contains_secrets,
    })
}

fn conflict_kind_name(kind: ConflictKind) -> &'static str {
    match kind {
        ConflictKind::Text => "text",
        ConflictKind::StructuredText => "structured-text",
        ConflictKind::Binary => "binary",
        ConflictKind::OpaqueGit => "opaque-git",
        ConflictKind::DeleteEdit => "delete-edit",
        ConflictKind::PathShape => "path-shape",
        ConflictKind::EnvKey => "env-key",
        ConflictKind::MergePlugin => "merge-plugin",
    }
}
