use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
    sync::atomic::{AtomicU64, Ordering as AtomicOrdering},
};

use bowline_core::{
    git_worktree_link::normalize_worktree_link_entry_bytes,
    ids::{ContentId, WorkspaceId},
    workspace_graph::NamespaceEntryKind,
};

use crate::metadata::{OwnedStagedPath, PreparationOwnerMarker};

use super::{
    PreparedContent, PreparedContentCleanup, PreparedContentSource, PreparedSourceFingerprint,
    coalescer::CoalesceError, short_hash, stat_cache::StatFingerprint,
};

const PREPARATION_BUFFER_BYTES: usize = 64 * 1024;
const MAX_INLINE_OVERRIDE_BYTES: usize = 1024 * 1024;
const MAX_WORKTREE_LINK_BYTES: u64 = 64 * 1024;
static NEXT_PREPARATION_OWNER: AtomicU64 = AtomicU64::new(1);

pub(super) struct PrepareContentRequest<'a> {
    pub(super) workspace_id: &'a WorkspaceId,
    pub(super) workspace_content_key: [u8; 32],
    pub(super) workspace_root: &'a Path,
    pub(super) preparation_root: Option<&'a Path>,
    pub(super) relative_path: &'a str,
    pub(super) absolute_path: &'a Path,
    pub(super) scan_fingerprint: Option<StatFingerprint>,
    pub(super) override_bytes: Option<&'a Vec<u8>>,
    pub(super) portable_git_worktree_link: bool,
    pub(super) created_at: &'a str,
    pub(super) owner_marker: Option<&'a PreparationOwnerMarker>,
}

pub(crate) struct PrepareSnapshotPathRequest<'a> {
    pub(crate) workspace_id: &'a WorkspaceId,
    pub(crate) workspace_content_key: [u8; 32],
    pub(crate) workspace_root: &'a Path,
    pub(crate) preparation_root: &'a Path,
    pub(crate) relative_path: &'a str,
    pub(crate) created_at: &'a str,
    pub(crate) portable_git_worktree_link: bool,
    pub(crate) owner_marker: Option<&'a PreparationOwnerMarker>,
}

pub(crate) struct PrepareSnapshotReaderRequest<'a> {
    pub(crate) workspace_id: &'a WorkspaceId,
    pub(crate) workspace_content_key: [u8; 32],
    pub(crate) workspace_root: &'a Path,
    pub(crate) preparation_root: &'a Path,
    pub(crate) relative_path: &'a str,
    pub(crate) created_at: &'a str,
}

pub(crate) fn prepare_snapshot_path(
    request: PrepareSnapshotPathRequest<'_>,
) -> Result<PreparedContent, CoalesceError> {
    prepare_content(PrepareContentRequest {
        workspace_id: request.workspace_id,
        workspace_content_key: request.workspace_content_key,
        workspace_root: request.workspace_root,
        preparation_root: Some(request.preparation_root),
        relative_path: request.relative_path,
        absolute_path: &request.workspace_root.join(request.relative_path),
        scan_fingerprint: None,
        override_bytes: None,
        portable_git_worktree_link: request.portable_git_worktree_link,
        created_at: request.created_at,
        owner_marker: request.owner_marker,
    })
}

pub(crate) fn prepare_snapshot_reader(
    request: PrepareSnapshotReaderRequest<'_>,
    reader: &mut dyn Read,
) -> Result<PreparedContent, CoalesceError> {
    prepare_reader(
        &PrepareContentRequest {
            workspace_id: request.workspace_id,
            workspace_content_key: request.workspace_content_key,
            workspace_root: request.workspace_root,
            preparation_root: Some(request.preparation_root),
            relative_path: request.relative_path,
            absolute_path: request.workspace_root,
            scan_fingerprint: None,
            override_bytes: None,
            portable_git_worktree_link: false,
            created_at: request.created_at,
            owner_marker: None,
        },
        reader,
        None,
    )
}

pub(super) fn prepare_content(
    request: PrepareContentRequest<'_>,
) -> Result<PreparedContent, CoalesceError> {
    if let Some(bytes) = request.override_bytes {
        return prepare_override_content(&request, bytes);
    }

    let descriptor = rustix::fs::open(
        request.absolute_path,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    )
    .map_err(|source| CoalesceError::PrepareContent {
        path: request.relative_path.to_string(),
        field: "source_open",
        source: std::io::Error::from(source),
    })?;
    let mut source = File::from(descriptor);
    let before = prepared_source_fingerprint(&source.metadata().map_err(|source| {
        CoalesceError::PrepareContent {
            path: request.relative_path.to_string(),
            field: "source_metadata_before",
            source,
        }
    })?);
    if request
        .scan_fingerprint
        .is_some_and(|scanned| !fingerprints_match(scanned, before))
    {
        return Err(CoalesceError::SourceChangedDuringPreparation {
            path: request.relative_path.to_string(),
        });
    }

    let prepared = if request.portable_git_worktree_link {
        if before.size > MAX_WORKTREE_LINK_BYTES {
            return Err(CoalesceError::PreparationLimitExceeded {
                path: request.relative_path.to_string(),
                limit_bytes: MAX_WORKTREE_LINK_BYTES,
            });
        }
        let mut bytes = Vec::with_capacity(before.size as usize);
        source
            .read_to_end(&mut bytes)
            .map_err(|source| CoalesceError::PrepareContent {
                path: request.relative_path.to_string(),
                field: "worktree_link_read",
                source,
            })?;
        let normalized = normalize_worktree_link_entry_bytes(
            request.relative_path,
            NamespaceEntryKind::File,
            &bytes,
            request.workspace_root,
        );
        prepare_owned_bytes(&request, normalized, Some(before))?
    } else {
        prepare_reader(&request, &mut source, Some(before))?
    };

    let after = prepared_source_fingerprint(&source.metadata().map_err(|source| {
        CoalesceError::PrepareContent {
            path: request.relative_path.to_string(),
            field: "source_metadata_after",
            source,
        }
    })?);
    if before != after {
        discard_prepared_content(&prepared);
        return Err(CoalesceError::SourceChangedDuringPreparation {
            path: request.relative_path.to_string(),
        });
    }
    Ok(prepared)
}

fn prepare_override_content(
    request: &PrepareContentRequest<'_>,
    bytes: &[u8],
) -> Result<PreparedContent, CoalesceError> {
    if bytes.len() <= MAX_INLINE_OVERRIDE_BYTES && request.preparation_root.is_none() {
        return Ok(PreparedContent::memory(
            content_id_for_bytes(request.workspace_content_key, bytes),
            bytes.to_vec(),
        ));
    }
    prepare_owned_bytes(request, bytes.to_vec(), None)
}

fn prepare_owned_bytes(
    request: &PrepareContentRequest<'_>,
    bytes: Vec<u8>,
    source_fingerprint: Option<PreparedSourceFingerprint>,
) -> Result<PreparedContent, CoalesceError> {
    if request.preparation_root.is_none() {
        let content_id = content_id_for_bytes(request.workspace_content_key, &bytes);
        return Ok(PreparedContent {
            logical_len: bytes.len() as u64,
            content_id,
            source: PreparedContentSource::Memory(bytes),
            source_fingerprint,
            cleanup_policy: PreparedContentCleanup::None,
        });
    }
    prepare_reader(request, &mut bytes.as_slice(), source_fingerprint)
}

fn prepare_reader(
    request: &PrepareContentRequest<'_>,
    reader: &mut dyn Read,
    source_fingerprint: Option<PreparedSourceFingerprint>,
) -> Result<PreparedContent, CoalesceError> {
    let Some(preparation_root) = request.preparation_root else {
        let mut bytes = Vec::new();
        reader
            .read_to_end(&mut bytes)
            .map_err(|source| CoalesceError::PrepareContent {
                path: request.relative_path.to_string(),
                field: "source_read",
                source,
            })?;
        let content_id = content_id_for_bytes(request.workspace_content_key, &bytes);
        return Ok(PreparedContent {
            logical_len: bytes.len() as u64,
            content_id,
            source: PreparedContentSource::Memory(bytes),
            source_fingerprint,
            cleanup_policy: PreparedContentCleanup::None,
        });
    };

    fs::create_dir_all(preparation_root).map_err(|source| CoalesceError::PrepareContent {
        path: request.relative_path.to_string(),
        field: "staging_root",
        source,
    })?;
    let owner_marker = request
        .owner_marker
        .cloned()
        .unwrap_or_else(|| next_preparation_owner_marker(request.workspace_id, request.created_at));
    let temp_path = preparation_root.join(format!("{}.tmp", owner_marker.as_str()));
    let mut staged = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp_path)
        .or_else(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                fs::remove_file(&temp_path)?;
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&temp_path)
            } else {
                Err(error)
            }
        })
        .map_err(|source| CoalesceError::PrepareContent {
            path: request.relative_path.to_string(),
            field: "staging_create",
            source,
        })?;
    let mut hasher = blake3::Hasher::new_keyed(&request.workspace_content_key);
    let mut logical_len = 0_u64;
    let mut buffer = vec![0_u8; PREPARATION_BUFFER_BYTES];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|source| CoalesceError::PrepareContent {
                path: request.relative_path.to_string(),
                field: "source_read",
                source,
            })?;
        if read == 0 {
            break;
        }
        staged
            .write_all(&buffer[..read])
            .map_err(|source| CoalesceError::PrepareContent {
                path: request.relative_path.to_string(),
                field: "staging_write",
                source,
            })?;
        hasher.update(&buffer[..read]);
        logical_len = logical_len.saturating_add(read as u64);
    }
    staged
        .sync_all()
        .map_err(|source| CoalesceError::PrepareContent {
            path: request.relative_path.to_string(),
            field: "staging_sync",
            source,
        })?;
    drop(staged);
    let content_id = ContentId::new(format!("cid_{}", hasher.finalize().to_hex()));
    let final_path = preparation_root.join(format!(
        "{}-{}.prepared",
        owner_marker.as_str(),
        content_id.as_str()
    ));
    // The content-derived name is an index, not proof that an existing file is
    // still trustworthy. Replace it atomically with the bytes just hashed.
    fs::rename(&temp_path, &final_path).map_err(|source| CoalesceError::PrepareContent {
        path: request.relative_path.to_string(),
        field: "staging_commit",
        source,
    })?;
    fs::set_permissions(&final_path, fs::Permissions::from_mode(0o400)).map_err(|source| {
        CoalesceError::PrepareContent {
            path: request.relative_path.to_string(),
            field: "staging_permissions",
            source,
        }
    })?;
    Ok(PreparedContent {
        content_id,
        logical_len,
        source: PreparedContentSource::StagedFile {
            path: OwnedStagedPath::new(final_path),
            owner_marker,
        },
        source_fingerprint,
        cleanup_policy: PreparedContentCleanup::LeaseOwned,
    })
}

pub(super) fn retain_one_prepared_source(
    content: &mut BTreeMap<ContentId, PreparedContent>,
    content_id: ContentId,
    prepared: PreparedContent,
) -> Result<(), CoalesceError> {
    if content.contains_key(&content_id) {
        let existing_path = content
            .get(&content_id)
            .and_then(|existing| match &existing.source {
                PreparedContentSource::StagedFile { path, .. } => Some(path.as_path()),
                PreparedContentSource::Memory(_) => None,
            });
        let prepared_path = match &prepared.source {
            PreparedContentSource::StagedFile { path, .. } => Some(path.as_path()),
            PreparedContentSource::Memory(_) => None,
        };
        if existing_path != prepared_path {
            discard_prepared_content(&prepared);
        }
        return Ok(());
    }
    content.insert(content_id, prepared);
    Ok(())
}

pub(crate) fn staged_content_matches(
    path: &OwnedStagedPath,
    expected_content_id: &ContentId,
    expected_logical_len: u64,
    workspace_content_key: [u8; 32],
) -> std::io::Result<bool> {
    let descriptor = rustix::fs::open(
        path.as_path(),
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    )
    .map_err(std::io::Error::from)?;
    let mut source = File::from(descriptor);
    if source.metadata()?.len() != expected_logical_len {
        return Ok(false);
    }
    let mut hasher = blake3::Hasher::new_keyed(&workspace_content_key);
    let mut logical_len = 0_u64;
    let mut buffer = [0_u8; PREPARATION_BUFFER_BYTES];
    loop {
        let read = source.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        logical_len = logical_len.saturating_add(read as u64);
    }
    let actual_content_id = ContentId::new(format!("cid_{}", hasher.finalize().to_hex()));
    Ok(logical_len == expected_logical_len && actual_content_id == *expected_content_id)
}

fn discard_prepared_content(content: &PreparedContent) {
    if content.cleanup_policy == PreparedContentCleanup::LeaseOwned
        && let PreparedContentSource::StagedFile { path, .. } = &content.source
    {
        let _ = fs::remove_file(path.as_path());
    }
}

fn content_id_for_bytes(workspace_content_key: [u8; 32], bytes: &[u8]) -> ContentId {
    let mut hasher = blake3::Hasher::new_keyed(&workspace_content_key);
    hasher.update(bytes);
    ContentId::new(format!("cid_{}", hasher.finalize().to_hex()))
}

fn fingerprints_match(scanned: StatFingerprint, prepared: PreparedSourceFingerprint) -> bool {
    scanned.size == prepared.size
        && scanned.mtime_ns.as_i64() == prepared.mtime_ns
        && scanned.ctime_ns.as_i64() == prepared.ctime_ns
        && scanned.inode == prepared.inode
        && scanned.dev == prepared.device
        && scanned.file_mode == prepared.file_mode
}

fn prepared_source_fingerprint(metadata: &fs::Metadata) -> PreparedSourceFingerprint {
    PreparedSourceFingerprint {
        size: metadata.len(),
        mtime_ns: timestamp_nanos(metadata.mtime(), metadata.mtime_nsec()),
        ctime_ns: timestamp_nanos(metadata.ctime(), metadata.ctime_nsec()),
        inode: metadata.ino(),
        device: metadata.dev(),
        file_mode: metadata.mode(),
    }
}

fn timestamp_nanos(seconds: i64, nanos: i64) -> i64 {
    seconds.saturating_mul(1_000_000_000).saturating_add(nanos)
}

pub(super) fn next_preparation_owner_marker(
    workspace_id: &WorkspaceId,
    created_at: &str,
) -> PreparationOwnerMarker {
    let sequence = NEXT_PREPARATION_OWNER.fetch_add(1, AtomicOrdering::Relaxed);
    PreparationOwnerMarker::new(format!(
        "prep_{}_{}_{}",
        short_hash([workspace_id.as_str().as_bytes(), created_at.as_bytes(),]),
        std::process::id(),
        sequence
    ))
}

#[cfg(test)]
mod tests {
    use std::io::Read as _;

    use super::*;
    use crate::workspace::TempWorkspace;

    #[test]
    fn duplicate_content_keeps_one_staged_source() {
        let workspace = TempWorkspace::new("prepared-duplicate-workspace").expect("workspace");
        let state = TempWorkspace::new("prepared-duplicate-state").expect("state");
        let workspace_id = WorkspaceId::new("ws_prepared");
        let owner = next_preparation_owner_marker(&workspace_id, "2026-07-12T12:00:00Z");
        let first_request = PrepareContentRequest {
            workspace_id: &workspace_id,
            workspace_content_key: [8_u8; 32],
            workspace_root: workspace.root(),
            preparation_root: Some(state.root()),
            relative_path: "a.bin",
            absolute_path: workspace.root(),
            scan_fingerprint: None,
            override_bytes: None,
            portable_git_worktree_link: false,
            created_at: "2026-07-12T12:00:00Z",
            owner_marker: Some(&owner),
        };
        let first =
            prepare_owned_bytes(&first_request, b"same".to_vec(), None).expect("prepare first");
        let second_request = PrepareContentRequest {
            relative_path: "b.bin",
            ..first_request
        };
        let second = prepare_owned_bytes(&second_request, b"same".to_vec(), None)
            .expect("prepare duplicate");
        let content_id = first.content_id.clone();
        let mut content = BTreeMap::new();

        retain_one_prepared_source(&mut content, content_id.clone(), first).expect("retain first");
        retain_one_prepared_source(&mut content, content_id, second).expect("deduplicate second");

        assert_eq!(content.len(), 1);
        assert_eq!(
            fs::read_dir(state.root())
                .expect("staging directory")
                .count(),
            1
        );
    }

    #[test]
    fn duplicate_preparation_replaces_corrupted_staged_bytes() {
        let workspace = TempWorkspace::new("prepared-repair-workspace").expect("workspace");
        let state = TempWorkspace::new("prepared-repair-state").expect("state");
        let workspace_id = WorkspaceId::new("ws_prepared");
        let owner = next_preparation_owner_marker(&workspace_id, "2026-07-12T12:00:00Z");
        let request = PrepareContentRequest {
            workspace_id: &workspace_id,
            workspace_content_key: [9_u8; 32],
            workspace_root: workspace.root(),
            preparation_root: Some(state.root()),
            relative_path: "same.bin",
            absolute_path: workspace.root(),
            scan_fingerprint: None,
            override_bytes: None,
            portable_git_worktree_link: false,
            created_at: "2026-07-12T12:00:00Z",
            owner_marker: Some(&owner),
        };
        let first = prepare_owned_bytes(&request, b"trusted".to_vec(), None)
            .expect("prepare original bytes");
        let PreparedContentSource::StagedFile { path, .. } = &first.source else {
            panic!("prepared content must be staged");
        };
        fs::set_permissions(path.as_path(), fs::Permissions::from_mode(0o600))
            .expect("make staged source writable for corruption simulation");
        fs::write(path.as_path(), b"corrupt").expect("corrupt staged source");

        let repaired = prepare_owned_bytes(&request, b"trusted".to_vec(), None)
            .expect("prepare duplicate bytes");

        assert_eq!(repaired.content_id, first.content_id);
        let mut reopened = Vec::new();
        repaired
            .open()
            .expect("open repaired source")
            .read_to_end(&mut reopened)
            .expect("read repaired source");
        assert_eq!(reopened, b"trusted");
    }
}
