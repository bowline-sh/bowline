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

    let manifest = fs::read_to_string(&manifest_path).ok()?;
    let manifest: Value = serde_json::from_str(&manifest).ok()?;
    let id = string_field(&manifest, &["conflictId", "id"])
        .unwrap_or_else(|| fallback_conflict_id(path));
    let affected_files =
        string_array_field(&manifest, &["affectedFiles", "affectedPaths", "paths"]);
    let active_view =
        string_field(&manifest, &["activeView"]).unwrap_or_else(|| "local".to_string());
    let state = string_field(&manifest, &["state"]).unwrap_or_else(|| "unresolved".to_string());
    if state != "unresolved" {
        return None;
    }
    let reason = string_field(&manifest, &["reason"]).unwrap_or_default();
    let conflict_kind =
        string_field(&manifest, &["conflictKind"]).unwrap_or_else(|| infer_conflict_kind(&reason));
    let contains_secrets = bool_field(&manifest, &["containsSecrets", "secretBearing"]);
    let workspace_root = string_field(&manifest, &["workspaceRoot"]);
    let spans = spans_field(&manifest);

    Some(ResolveConflict {
        id,
        state,
        bundle_path: path.display().to_string(),
        conflict_kind,
        workspace_root,
        reason,
        affected_files,
        spans,
        active_view,
        has_resolution_overlay: path.join("resolution").is_dir(),
        contains_secrets,
    })
}

pub(super) fn infer_conflict_kind(reason: &str) -> String {
    match reason {
        "delete-versus-edit conflict" => "delete-edit",
        "path kind conflict" => "path-shape",
        "opaque Git state conflict" => "opaque-git",
        "structured text merge did not validate" => "structured-text",
        _ => "text",
    }
    .to_string()
}

pub(super) fn spans_field(manifest: &Value) -> Vec<ResolveConflictSpan> {
    manifest
        .get("spans")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|span| {
            Some(ResolveConflictSpan {
                path: string_field(span, &["path"])?,
                base_start_line: u32_field(span, &["baseStartLine"])?,
                base_end_line: u32_field(span, &["baseEndLine"])?,
                local_start_line: u32_field(span, &["localStartLine"])?,
                local_end_line: u32_field(span, &["localEndLine"])?,
                remote_start_line: u32_field(span, &["remoteStartLine"])?,
                remote_end_line: u32_field(span, &["remoteEndLine"])?,
                base_context_hash: string_field(span, &["baseContextHash"]),
                local_context_hash: string_field(span, &["localContextHash"]),
                remote_context_hash: string_field(span, &["remoteContextHash"]),
            })
        })
        .collect()
}
