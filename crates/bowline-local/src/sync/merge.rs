use std::{collections::BTreeMap, error::Error, fmt};

use bowline_core::{
    ids::ContentId,
    workspace_graph::{NamespaceEntry, NamespaceEntryKind, workspace_content_id},
};

use crate::env::{EnvLineKind, parse_env_text};

use super::{
    CandidateBase, SnapshotContent,
    coalescer::SnapshotCandidate,
    conflicts::{ConflictRecord, ConflictSpan},
    line_merge::{merge_utf8_lines, split_keep_terminator},
    manifest_id_for_snapshot, snapshot_id_from_hash,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    Clean(Box<SnapshotCandidate>),
    Conflicted(Vec<ConflictRecord>),
}

pub fn merge_snapshots(
    base: &SnapshotContent,
    local: &SnapshotCandidate,
    remote: &SnapshotContent,
    remote_base: CandidateBase,
    workspace_content_key: [u8; 32],
    created_at: impl Into<String>,
) -> Result<MergeOutcome, MergeError> {
    let mut paths = BTreeMap::<String, MergePath>::new();
    index_paths(&mut paths, Side::Base, base);
    index_paths(&mut paths, Side::Local, &local.snapshot);
    index_paths(&mut paths, Side::Remote, remote);

    let mut merged_entries = Vec::new();
    let mut merged_files = BTreeMap::<ContentId, Vec<u8>>::new();
    let mut conflicts = Vec::new();

    for (path, sides) in paths {
        match merge_path(
            &path,
            &sides,
            base,
            &local.snapshot,
            remote,
            workspace_content_key,
        )? {
            PathMerge::Entry { entry, bytes } => {
                if let Some(bytes) = bytes {
                    let content_id = entry
                        .content_id
                        .clone()
                        .ok_or(MergeError::MissingContentId)?;
                    merged_files.insert(content_id, bytes);
                }
                merged_entries.push(entry);
            }
            PathMerge::Deleted => {}
            PathMerge::Conflict(conflict) => conflicts.push(conflict),
        }
    }

    if !conflicts.is_empty() {
        return Ok(MergeOutcome::Conflicted(conflicts));
    }

    merged_entries.sort_by(|left, right| left.path.cmp(&right.path));
    let hash_parts = merged_entries.iter().flat_map(entry_hash_parts);
    let snapshot_id = snapshot_id_from_hash("snap_merge", hash_parts);
    let mut manifest = remote.manifest.clone();
    manifest.snapshot_id = snapshot_id.clone();
    manifest.base_snapshot_id = Some(remote.manifest.snapshot_id.clone());
    manifest.entries = merged_entries;
    for reference in &mut manifest.refs {
        if reference.name == "workspace" {
            reference.target_snapshot_id = snapshot_id.clone();
        }
    }

    Ok(MergeOutcome::Clean(Box::new(SnapshotCandidate {
        base: remote_base,
        device_id: local.device_id.clone(),
        manifest_id: manifest_id_for_snapshot(&snapshot_id),
        snapshot: SnapshotContent::new(manifest, merged_files),
        scan_report: local.scan_report.clone(),
        causation_ids: {
            let mut ids = local.causation_ids.clone();
            ids.push(format!("merge:{}", remote.manifest.snapshot_id.as_str()));
            ids
        },
        created_at: created_at.into(),
    })))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Base,
    Local,
    Remote,
}

#[derive(Default)]
struct MergePath {
    base: Option<NamespaceEntry>,
    local: Option<NamespaceEntry>,
    remote: Option<NamespaceEntry>,
}

enum PathMerge {
    Entry {
        entry: NamespaceEntry,
        bytes: Option<Vec<u8>>,
    },
    Deleted,
    Conflict(ConflictRecord),
}

fn entry_hash_parts(entry: &NamespaceEntry) -> Vec<Vec<u8>> {
    vec![
        entry.path.as_bytes().to_vec(),
        format!("{:?}", entry.kind).into_bytes(),
        format!("{:?}", entry.classification).into_bytes(),
        format!("{:?}", entry.mode).into_bytes(),
        format!("{:?}", entry.access).into_bytes(),
        entry
            .content_id
            .as_ref()
            .map(|content_id| content_id.as_str())
            .unwrap_or_default()
            .as_bytes()
            .to_vec(),
        entry
            .symlink_target
            .as_deref()
            .unwrap_or_default()
            .as_bytes()
            .to_vec(),
        format!("{:?}", entry.byte_len).into_bytes(),
        format!("{:?}", entry.hydration_state).into_bytes(),
    ]
}

fn index_paths(paths: &mut BTreeMap<String, MergePath>, side: Side, snapshot: &SnapshotContent) {
    for entry in &snapshot.manifest.entries {
        let slot = paths.entry(entry.path.clone()).or_default();
        match side {
            Side::Base => slot.base = Some(entry.clone()),
            Side::Local => slot.local = Some(entry.clone()),
            Side::Remote => slot.remote = Some(entry.clone()),
        }
    }
}

fn merge_path(
    path: &str,
    sides: &MergePath,
    base: &SnapshotContent,
    local: &SnapshotContent,
    remote: &SnapshotContent,
    workspace_content_key: [u8; 32],
) -> Result<PathMerge, MergeError> {
    if optional_entries_match_for_merge(sides.local.as_ref(), sides.remote.as_ref()) {
        return Ok(sides.local.clone().map_or(PathMerge::Deleted, |entry| {
            let bytes = local.file_bytes_for_path(path).map(Vec::from);
            PathMerge::Entry { entry, bytes }
        }));
    }
    if optional_entries_match_for_merge(sides.base.as_ref(), sides.local.as_ref()) {
        return Ok(sides.remote.clone().map_or(PathMerge::Deleted, |entry| {
            let bytes = remote.file_bytes_for_path(path).map(Vec::from);
            PathMerge::Entry { entry, bytes }
        }));
    }
    if optional_entries_match_for_merge(sides.base.as_ref(), sides.remote.as_ref()) {
        return Ok(sides.local.clone().map_or(PathMerge::Deleted, |entry| {
            let bytes = local.file_bytes_for_path(path).map(Vec::from);
            PathMerge::Entry { entry, bytes }
        }));
    }

    let Some(local_entry) = &sides.local else {
        return Ok(PathMerge::Conflict(ConflictRecord::delete_edit(path)));
    };
    let Some(remote_entry) = &sides.remote else {
        return Ok(PathMerge::Conflict(ConflictRecord::delete_edit(path)));
    };
    if local_entry.kind != remote_entry.kind || local_entry.kind != NamespaceEntryKind::File {
        return Ok(PathMerge::Conflict(ConflictRecord::path_conflict(path)));
    }
    if is_opaque_git_path(path) {
        return Ok(PathMerge::Conflict(ConflictRecord::opaque_git(path)));
    }

    let base_bytes = base.file_bytes_for_path(path).unwrap_or_default();
    let local_bytes = local.file_bytes_for_path(path).unwrap_or_default();
    let remote_bytes = remote.file_bytes_for_path(path).unwrap_or_default();
    if is_conflict_by_default_structured_path(path) {
        return Ok(PathMerge::Conflict(ConflictRecord::structured(path)));
    }
    if is_env_path(path) {
        let Some(merged) = merge_env_bytes(path, base_bytes, local_bytes, remote_bytes) else {
            return Ok(PathMerge::Conflict(ConflictRecord::env_key(path)));
        };
        let content_id = workspace_content_id(workspace_content_key, &merged);
        let mut entry = remote_entry.clone();
        entry.content_id = Some(content_id);
        entry.locator = None;
        entry.byte_len = Some(merged.len() as u64);
        return Ok(PathMerge::Entry {
            entry,
            bytes: Some(merged),
        });
    }

    let Some(merged) = merge_utf8_lines(base_bytes, local_bytes, remote_bytes) else {
        if std::str::from_utf8(local_bytes).is_ok() && std::str::from_utf8(remote_bytes).is_ok() {
            return Ok(PathMerge::Conflict(ConflictRecord::same_path_span(
                path,
                conflict_span(path, base_bytes, local_bytes, remote_bytes),
            )));
        }
        return Ok(PathMerge::Conflict(ConflictRecord::binary(path)));
    };
    if !structured_merge_output_is_valid(path, &merged) {
        return Ok(PathMerge::Conflict(ConflictRecord::structured(path)));
    };

    let content_id = workspace_content_id(workspace_content_key, &merged);
    let mut entry = remote_entry.clone();
    entry.content_id = Some(content_id);
    entry.locator = None;
    entry.byte_len = Some(merged.len() as u64);
    Ok(PathMerge::Entry {
        entry,
        bytes: Some(merged),
    })
}

fn is_opaque_git_path(path: &str) -> bool {
    path.split('/').any(|component| component == ".git")
}

fn structured_merge_output_is_valid(path: &str, bytes: &[u8]) -> bool {
    match structured_format(path) {
        Some(StructuredFormat::Json) => serde_json::from_slice::<serde_json::Value>(bytes).is_ok(),
        Some(StructuredFormat::Toml) => std::str::from_utf8(bytes)
            .ok()
            .and_then(|text| text.parse::<toml::Table>().ok())
            .is_some(),
        Some(StructuredFormat::Yaml) => serde_yml::from_slice::<serde_yml::Value>(bytes).is_ok(),
        Some(StructuredFormat::Xml) => std::str::from_utf8(bytes)
            .ok()
            .and_then(|text| roxmltree::Document::parse(text).ok())
            .is_some(),
        None => true,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StructuredFormat {
    Json,
    Toml,
    Yaml,
    Xml,
}

fn structured_format(path: &str) -> Option<StructuredFormat> {
    let file_name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    if is_conflict_by_default_structured_file_name(&file_name) {
        return None;
    }
    if file_name.ends_with(".json") {
        return Some(StructuredFormat::Json);
    }
    if file_name.ends_with(".toml") {
        return Some(StructuredFormat::Toml);
    }
    if file_name.ends_with(".yaml") || file_name.ends_with(".yml") {
        return Some(StructuredFormat::Yaml);
    }
    if file_name.ends_with(".xml") {
        return Some(StructuredFormat::Xml);
    }
    None
}

fn is_conflict_by_default_structured_path(path: &str) -> bool {
    let file_name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    is_conflict_by_default_structured_file_name(&file_name)
}

fn is_conflict_by_default_structured_file_name(file_name: &str) -> bool {
    matches!(
        file_name,
        "cargo.lock" | "uv.lock" | "pnpm-lock.yaml" | "package-lock.json" | "yarn.lock"
    )
}

fn is_env_path(path: &str) -> bool {
    path.rsplit('/').next().is_some_and(|file_name| {
        file_name == ".env" || file_name.starts_with(".env.") || file_name.ends_with(".env")
    })
}

fn merge_env_bytes(path: &str, base: &[u8], local: &[u8], remote: &[u8]) -> Option<Vec<u8>> {
    env_keys_can_merge(path, base, local, remote).then(|| merge_utf8_lines(base, local, remote))?
}

fn env_keys_can_merge(path: &str, base: &[u8], local: &[u8], remote: &[u8]) -> bool {
    let base = parse_env_text(path, "project", base);
    let local = parse_env_text(path, "project", local);
    let remote = parse_env_text(path, "project", remote);
    let base_values = env_values(&base);
    let local_values = env_values(&local);
    let remote_values = env_values(&remote);
    let all_keys = base_values
        .keys()
        .chain(local_values.keys())
        .chain(remote_values.keys())
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();

    for key in all_keys {
        let base_value = base_values.get(&key);
        let local_value = local_values.get(&key);
        let remote_value = remote_values.get(&key);
        if local_value == remote_value {
            continue;
        }
        if local_value == base_value {
            if remote_value.is_none() {
                return false;
            }
            continue;
        }
        if remote_value == base_value {
            if local_value.is_none() {
                return false;
            }
            continue;
        }
        return false;
    }

    true
}

fn env_values(parsed: &crate::env::ParsedEnvFile) -> BTreeMap<(String, usize), Vec<u8>> {
    parsed
        .lines
        .iter()
        .filter_map(|line| match &line.kind {
            EnvLineKind::KeyValue(value) => Some((
                (value.key.clone(), value.occurrence_index),
                value.value.as_bytes().to_vec(),
            )),
            EnvLineKind::Blank | EnvLineKind::Comment | EnvLineKind::Opaque(_) => None,
        })
        .collect()
}

fn conflict_span(path: &str, base: &[u8], local: &[u8], remote: &[u8]) -> ConflictSpan {
    let base_lines = line_vec(base);
    let local_lines = line_vec(local);
    let remote_lines = line_vec(remote);
    let max_len = local_lines
        .len()
        .max(remote_lines.len())
        .max(base_lines.len());
    let first = (0..max_len)
        .find(|index| {
            local_lines.get(*index) != remote_lines.get(*index)
                && (local_lines.get(*index) != base_lines.get(*index)
                    || remote_lines.get(*index) != base_lines.get(*index))
        })
        .unwrap_or(0);
    let last = (first..max_len)
        .rfind(|index| {
            local_lines.get(*index) != remote_lines.get(*index)
                && (local_lines.get(*index) != base_lines.get(*index)
                    || remote_lines.get(*index) != base_lines.get(*index))
        })
        .unwrap_or(first);
    let start = (first + 1) as u32;
    let end = (last + 1) as u32;
    ConflictSpan {
        path: path.to_string(),
        base_start_line: start,
        base_end_line: end.min(base_lines.len().max(1) as u32),
        local_start_line: start,
        local_end_line: end.min(local_lines.len().max(1) as u32),
        remote_start_line: start,
        remote_end_line: end.min(remote_lines.len().max(1) as u32),
        base_context_hash: Some(span_context_hash(base, first, last)),
        local_context_hash: Some(span_context_hash(local, first, last)),
        remote_context_hash: Some(span_context_hash(remote, first, last)),
    }
}

fn line_vec(bytes: &[u8]) -> Vec<&str> {
    std::str::from_utf8(bytes)
        .map(split_keep_terminator)
        .unwrap_or_default()
}

fn span_context_hash(bytes: &[u8], first: usize, last: usize) -> String {
    let lines = line_vec(bytes);
    let start = first.saturating_sub(3);
    let end = (last + 4).min(lines.len());
    super::short_hash(lines[start..end].iter().map(|line| line.as_bytes()))
}

fn optional_entries_match_for_merge(
    left: Option<&NamespaceEntry>,
    right: Option<&NamespaceEntry>,
) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => entries_match_for_merge(left, right),
        _ => false,
    }
}

fn entries_match_for_merge(left: &NamespaceEntry, right: &NamespaceEntry) -> bool {
    left.path == right.path
        && left.kind == right.kind
        && left.classification == right.classification
        && left.mode == right.mode
        && left.access == right.access
        && left.content_id == right.content_id
        && left.symlink_target == right.symlink_target
        && left.byte_len == right.byte_len
}

#[derive(Debug)]
pub enum MergeError {
    MissingContentId,
}

impl fmt::Display for MergeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingContentId => {
                formatter.write_str("merged file entry is missing content ID")
            }
        }
    }
}

impl Error for MergeError {}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use bowline_core::{
        ids::{ContentId, DeviceId, ManifestId, SnapshotId, WorkspaceId},
        policy::{AccessFlag, MaterializationMode, PathClassification},
        workspace_graph::{
            HydrationState, NamespaceEntry, NamespaceEntryKind, RefKind, SnapshotKind,
            SnapshotManifest, WorkspaceRef, workspace_content_id,
        },
    };

    use super::*;
    use crate::sync::ConflictKind;

    const KEY: [u8; 32] = [7; 32];

    #[test]
    fn git_index_divergence_creates_opaque_conflict() {
        let base = snapshot("base", ".git/index", b"base-index");
        let local = candidate(&base, "local", ".git/index", b"local-index");
        let remote = snapshot("remote", ".git/index", b"remote-index");

        let merged = merge_snapshots(
            &base,
            &local,
            &remote,
            CandidateBase {
                workspace_id: WorkspaceId::new("ws_code"),
                version: 3,
                snapshot_id: SnapshotId::new("remote"),
            },
            KEY,
            "2026-06-27T12:00:00Z",
        )
        .expect("merge succeeds");

        let MergeOutcome::Conflicted(conflicts) = merged else {
            panic!("git index divergence should create an opaque sync conflict");
        };
        assert_eq!(conflicts[0].conflict_kind, ConflictKind::OpaqueGit);
        assert_eq!(conflicts[0].paths, vec![".git/index".to_string()]);
    }

    #[test]
    fn other_git_state_still_conflicts_when_both_sides_change() {
        let base = snapshot("base", ".git/HEAD", b"ref: refs/heads/main\n");
        let local = candidate(&base, "local", ".git/HEAD", b"ref: refs/heads/local\n");
        let remote = snapshot("remote", ".git/HEAD", b"ref: refs/heads/remote\n");

        let merged = merge_snapshots(
            &base,
            &local,
            &remote,
            CandidateBase {
                workspace_id: WorkspaceId::new("ws_code"),
                version: 3,
                snapshot_id: SnapshotId::new("remote"),
            },
            KEY,
            "2026-06-27T12:00:00Z",
        )
        .expect("merge succeeds");

        let MergeOutcome::Conflicted(conflicts) = merged else {
            panic!("non-index git state should stay opaque");
        };
        assert_eq!(conflicts[0].paths, vec![".git/HEAD".to_string()]);
    }

    #[test]
    fn env_different_key_edits_merge_without_secret_conflict() {
        let base = snapshot("base", ".env.local", b"API_KEY=old\nDATABASE_URL=old\n");
        let local = candidate(
            &base,
            "local",
            ".env.local",
            b"API_KEY=local\nDATABASE_URL=old\n",
        );
        let remote = snapshot(
            "remote",
            ".env.local",
            b"API_KEY=old\nDATABASE_URL=remote\n",
        );

        let merged = merge_snapshots(
            &base,
            &local,
            &remote,
            CandidateBase {
                workspace_id: WorkspaceId::new("ws_code"),
                version: 3,
                snapshot_id: SnapshotId::new("remote"),
            },
            KEY,
            "2026-06-27T12:00:00Z",
        )
        .expect("merge succeeds");

        let MergeOutcome::Clean(merged) = merged else {
            panic!("different env keys should merge");
        };
        assert_eq!(
            merged.snapshot.file_bytes_for_path(".env.local"),
            Some(&b"API_KEY=local\nDATABASE_URL=remote\n"[..])
        );
    }

    #[test]
    fn env_merge_preserves_remote_non_key_edits() {
        let base = snapshot("base", ".env.local", b"API_KEY=old\n# old comment\n");
        let local = candidate(
            &base,
            "local",
            ".env.local",
            b"API_KEY=local\n# old comment\n",
        );
        let remote = snapshot("remote", ".env.local", b"API_KEY=old\n# new comment\n");

        let merged = merge_snapshots(
            &base,
            &local,
            &remote,
            CandidateBase {
                workspace_id: WorkspaceId::new("ws_code"),
                version: 3,
                snapshot_id: SnapshotId::new("remote"),
            },
            KEY,
            "2026-06-27T12:00:00Z",
        )
        .expect("merge succeeds");

        let MergeOutcome::Clean(merged) = merged else {
            panic!("key edit plus remote comment edit should merge");
        };
        assert_eq!(
            merged.snapshot.file_bytes_for_path(".env.local"),
            Some(&b"API_KEY=local\n# new comment\n"[..])
        );
    }

    #[test]
    fn env_same_key_edits_create_secret_bearing_key_conflict() {
        let base = snapshot("base", ".env.local", b"API_KEY=old\n");
        let local = candidate(&base, "local", ".env.local", b"API_KEY=local\n");
        let remote = snapshot("remote", ".env.local", b"API_KEY=remote\n");

        let merged = merge_snapshots(
            &base,
            &local,
            &remote,
            CandidateBase {
                workspace_id: WorkspaceId::new("ws_code"),
                version: 3,
                snapshot_id: SnapshotId::new("remote"),
            },
            KEY,
            "2026-06-27T12:00:00Z",
        )
        .expect("merge succeeds");

        let MergeOutcome::Conflicted(conflicts) = merged else {
            panic!("same env key should conflict");
        };
        assert_eq!(conflicts[0].conflict_kind, ConflictKind::EnvKey);
        assert!(conflicts[0].contains_secrets);
    }

    #[test]
    fn lockfile_edits_conflict_even_when_line_mergeable() {
        let base = snapshot("base", "pnpm-lock.yaml", b"a: 1\nb: 1\n");
        let local = candidate(&base, "local", "pnpm-lock.yaml", b"a: 2\nb: 1\n");
        let remote = snapshot("remote", "pnpm-lock.yaml", b"a: 1\nb: 2\n");

        let merged = merge_snapshots(
            &base,
            &local,
            &remote,
            CandidateBase {
                workspace_id: WorkspaceId::new("ws_code"),
                version: 3,
                snapshot_id: SnapshotId::new("remote"),
            },
            KEY,
            "2026-06-27T12:00:00Z",
        )
        .expect("merge succeeds");

        let MergeOutcome::Conflicted(conflicts) = merged else {
            panic!("lockfiles need semantic validation before automatic merge");
        };
        assert_eq!(conflicts[0].conflict_kind, ConflictKind::StructuredText);
    }

    #[test]
    fn non_utf8_same_path_edits_create_binary_conflict() {
        let base = snapshot("base", "image.bin", &[0, 1, 2, 3]);
        let local = candidate(&base, "local", "image.bin", &[0, 1, 255, 3]);
        let remote = snapshot("remote", "image.bin", &[0, 1, 254, 3]);

        let merged = merge_snapshots(
            &base,
            &local,
            &remote,
            CandidateBase {
                workspace_id: WorkspaceId::new("ws_code"),
                version: 3,
                snapshot_id: SnapshotId::new("remote"),
            },
            KEY,
            "2026-06-27T12:00:00Z",
        )
        .expect("merge succeeds");

        let MergeOutcome::Conflicted(conflicts) = merged else {
            panic!("binary divergence should conflict");
        };
        assert_eq!(conflicts[0].conflict_kind, ConflictKind::Binary);
    }

    fn candidate(
        base: &SnapshotContent,
        snapshot_id: &str,
        path: &str,
        bytes: &[u8],
    ) -> SnapshotCandidate {
        SnapshotCandidate {
            base: CandidateBase {
                workspace_id: WorkspaceId::new("ws_code"),
                version: 1,
                snapshot_id: base.manifest.snapshot_id.clone(),
            },
            device_id: DeviceId::new("device_local"),
            manifest_id: ManifestId::new(format!("manifest_{snapshot_id}")),
            snapshot: snapshot(snapshot_id, path, bytes),
            scan_report: crate::scanner::ScanReport {
                root: std::path::PathBuf::new(),
                projects: Vec::new(),
                paths: Vec::new(),
                summary: bowline_core::status::ObservedWorkspaceSummary::default(),
            },
            causation_ids: Vec::new(),
            created_at: "2026-06-27T12:00:00Z".to_string(),
        }
    }

    fn snapshot(snapshot_id: &str, path: &str, bytes: &[u8]) -> SnapshotContent {
        let content_id = workspace_content_id(KEY, bytes);
        let mut files = BTreeMap::new();
        files.insert(content_id.clone(), bytes.to_vec());
        SnapshotContent::new(
            SnapshotManifest {
                schema_version: 1,
                snapshot_id: SnapshotId::new(snapshot_id),
                workspace_id: WorkspaceId::new("ws_code"),
                project_id: None,
                kind: SnapshotKind::WorkspaceHead,
                base_snapshot_id: None,
                entries: vec![entry(path, content_id, bytes.len())],
                refs: vec![WorkspaceRef {
                    name: "workspace".to_string(),
                    target_snapshot_id: SnapshotId::new(snapshot_id),
                    kind: RefKind::Workspace,
                }],
            },
            files,
        )
    }

    fn entry(path: &str, content_id: ContentId, len: usize) -> NamespaceEntry {
        NamespaceEntry {
            path: path.to_string(),
            kind: NamespaceEntryKind::File,
            classification: PathClassification::WorkspaceSync,
            mode: MaterializationMode::EncryptedSync,
            access: vec![AccessFlag::HumanReadable, AccessFlag::AgentHidden],
            content_id: Some(content_id),
            locator: None,
            symlink_target: None,
            byte_len: Some(len as u64),
            hydration_state: HydrationState::Local,
        }
    }
}
