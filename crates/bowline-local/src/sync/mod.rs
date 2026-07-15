use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
    fs::File,
    io::{self, Cursor, Read, Seek, SeekFrom},
};

use crate::metadata::{OwnedStagedPath, PreparationLeaseId, PreparationOwnerMarker};
use bowline_control_plane::WorkspaceRef as RemoteWorkspaceRef;
use bowline_core::{
    ids::{ContentId, ManifestId, SnapshotId, WorkspaceId},
    namespace_snapshot::{
        EntryVisitor, NamespaceBuildError, NamespaceOperationContext, NamespaceReadError,
        NamespaceSnapshotReader, NamespaceVisitControl,
    },
    workspace_graph::{
        ContentLayout, NamespaceEntry, NamespaceEntryKind, SnapshotDraft, SnapshotManifest,
        WorkspaceRelativePath,
    },
};
use bowline_storage::{ContentSourceReader, PackfileError};

mod cached_snapshot;
#[cfg(test)]
mod change_frontier_acceptance_tests;
pub mod change_index;
pub mod coalescer;
mod conflict_operations;
pub mod conflicts;
pub mod download;
pub mod fault;
pub(crate) mod identity;
pub(crate) mod line_merge;
pub(crate) mod manifest_identity;
pub(crate) mod materialization;
pub mod merge;
pub mod merge_plugins;
pub(crate) mod metadata_sidecar;
pub mod namespace;
pub mod observation_scope;
pub(crate) mod paths;
mod prepared_content;
pub mod runner;
pub mod stat_cache;
pub mod upload;
mod upload_error;
mod upload_payloads;
mod work_view_overlay_operations;

pub use cached_snapshot::{CachedSnapshotError, load_cached_snapshot};
pub use coalescer::{CoalesceContext, CoalesceError, SnapshotCandidate, coalesce_workspace_scan};
pub(crate) use conflict_operations::prepare_pending_conflict_occurrence_operations;
pub use conflict_operations::{
    ConflictOccurrenceQueueResult, conflict_occurrence_preparation_required,
    conflict_occurrence_queue_result, decode_conflict_occurrence_operation,
    pending_conflict_occurrence_operations,
};
pub use conflicts::{
    ConflictActiveView, ConflictBundle, ConflictBundleError, ConflictFile, ConflictKind,
    ConflictRecord, ConflictSide, ConflictSpan, ConflictState, conflict_bundle_object_id,
    conflict_occurrence_is_current, create_conflict_bundle, load_conflict_records,
    mark_conflict_occurrence_reconciled, set_conflict_bundle_object,
    transition_conflict_occurrence_state, unresolved_conflict_paths,
};
pub use download::{
    DownloadError, ImportedSnapshot, RemoteNamespaceSnapshot, import_snapshot_by_id,
    import_snapshot_manifest, open_remote_snapshot_by_id,
};
pub(crate) use identity::hash_namespace_entry_identity;
pub(crate) use manifest_identity::build_manifest_identity;
pub use manifest_identity::{ManifestIdentityReport, rebuild_manifest_identity};
pub use merge::{
    MergeContentReader, MergeError, MergeOutcome, MergeTreeInput, MergeTreeOutcome,
    MergedNamespace, merge_snapshots, merge_tree,
};
pub(crate) use merge::{merge_required_content_paths, stale_merge_required_content_paths};
pub use observation_scope::ObservationWriteScope;
pub use runner::{
    LongOperationCancellationPoint, SyncExternalFailureCode, SyncRunner, SyncRunnerError,
    SyncRunnerFailureSource, SyncRunnerOptions, SyncTickOutcome, WorkViewAcceptExecutionInput,
    WorkViewAcceptExecutionOutcome,
};
pub use stat_cache::{RehashReason, ScanStats, StatCacheDeleteScope, StatCacheSession};
pub use upload::{
    UploadOutcome, upload_snapshot_candidate, upload_snapshot_candidate_with_checkpoints,
};
pub use upload_error::{UploadError, UploadFailureSource};
pub use work_view_overlay_operations::{
    WorkViewOverlaySyncInput, WorkViewOverlaySyncResult, decode_work_view_overlay_sync_operation,
    pending_work_view_overlay_sync_operation, work_view_overlay_sync_operation,
    work_view_overlay_sync_result,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanScope {
    Full(FullScanReason),
    // A non-recursive pass over the workspace root's direct children only.
    // Root routing emits it for root-level file changes and maps it to
    // root-level-only delete/observe authority, never a full scan.
    RootShallow,
    // `root_shallow: true` means the scoped subtree pass runs *in addition to* a
    // root-level shallow pass in the same tick, usually from drained root files
    // paired with one or more scoped dirty roots.
    DirtySubtrees {
        roots: BTreeSet<String>,
        root_shallow: bool,
    },
}

impl Default for ScanScope {
    fn default() -> Self {
        Self::Full(FullScanReason::CliRequested)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FullScanReason {
    Startup,
    CliRequested,
    /// A root-level policy input (`.bowlineignore`) changed, so the whole deep
    /// include/exclude classification must be recomputed. Distinct from
    /// `CliRequested` so status does not misreport a policy-driven scan as
    /// user-requested.
    PolicyChanged,
    /// A steady-state reconcile with no specific dirty frontier, or a degrade
    /// path that cannot bound a scoped scan, fell back to a full scan.
    ReconcileFallback,
    WatcherUnavailable,
    WatcherOverflow,
    DirtyCapExceeded,
    HeadManifestUnavailable,
    VerifyDue,
    DivergenceRecovery,
}

impl FullScanReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::CliRequested => "cli-requested",
            Self::PolicyChanged => "policy-changed",
            Self::ReconcileFallback => "reconcile-fallback",
            Self::WatcherUnavailable => "watcher-unavailable",
            Self::WatcherOverflow => "watcher-overflow",
            Self::DirtyCapExceeded => "dirty-cap-exceeded",
            Self::HeadManifestUnavailable => "head-manifest-unavailable",
            Self::VerifyDue => "verify-due",
            Self::DivergenceRecovery => "divergence-recovery",
        }
    }
}

pub(crate) fn content_layout_for_entry(entry: &NamespaceEntry) -> Option<&ContentLayout> {
    if entry.kind != NamespaceEntryKind::File {
        return None;
    }
    let content_id = entry.content_id.as_ref()?;
    let layout = entry.content_layout.as_ref()?;
    if layout.logical_content_id() != content_id {
        return None;
    }
    Some(layout)
}

pub(crate) fn content_layout_map_from_snapshot(
    snapshot: &SnapshotContent,
) -> Result<BTreeMap<ContentId, ContentLayout>, NamespaceReadError> {
    struct LayoutCollector(BTreeMap<ContentId, ContentLayout>);

    impl EntryVisitor for LayoutCollector {
        fn visit(
            &mut self,
            entry: &NamespaceEntry,
            _context: &mut NamespaceOperationContext<'_>,
        ) -> Result<NamespaceVisitControl, NamespaceReadError> {
            if let (Some(content_id), Some(layout)) =
                (entry.content_id.as_ref(), content_layout_for_entry(entry))
            {
                self.0.insert(content_id.clone(), layout.clone());
            }
            Ok(NamespaceVisitControl::Continue)
        }
    }

    let entry_count = snapshot.manifest().entry_count;
    let mut context =
        NamespaceOperationContext::uncancelled(namespace::operation_budget(entry_count, 0, 0));
    let mut collector = LayoutCollector(BTreeMap::new());
    snapshot.namespace_reader().visit_prefix(
        &WorkspaceRelativePath::new(""),
        &mut collector,
        &mut context,
    )?;
    Ok(collector.0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedContent {
    pub content_id: ContentId,
    pub logical_len: u64,
    pub source: PreparedContentSource,
    pub source_fingerprint: Option<PreparedSourceFingerprint>,
    pub cleanup_policy: PreparedContentCleanup,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreparedContentSource {
    StagedFile {
        path: OwnedStagedPath,
        owner_marker: PreparationOwnerMarker,
    },
    Memory(Vec<u8>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreparedSourceFingerprint {
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub inode: u64,
    pub device: u64,
    pub file_mode: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreparedContentCleanup {
    LeaseOwned,
    SnapshotOwned,
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedSnapshotLease {
    pub id: PreparationLeaseId,
    pub owner_marker: PreparationOwnerMarker,
}

impl PreparedContent {
    pub fn memory(content_id: ContentId, bytes: Vec<u8>) -> Self {
        Self {
            logical_len: bytes.len() as u64,
            content_id,
            source: PreparedContentSource::Memory(bytes),
            source_fingerprint: None,
            cleanup_policy: PreparedContentCleanup::None,
        }
    }

    pub fn open(&self) -> io::Result<Box<dyn Read + Send>> {
        match &self.source {
            PreparedContentSource::StagedFile { path, .. } => Ok(Box::new(open_staged_file(path)?)),
            PreparedContentSource::Memory(bytes) => Ok(Box::new(Cursor::new(bytes.clone()))),
        }
    }

    pub fn resident_bytes(&self) -> Option<&[u8]> {
        match &self.source {
            PreparedContentSource::Memory(bytes) => Some(bytes),
            PreparedContentSource::StagedFile { .. } => None,
        }
    }
}

fn open_staged_file(path: &OwnedStagedPath) -> io::Result<File> {
    let descriptor = rustix::fs::open(
        path.as_path(),
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    )
    .map_err(io::Error::from)?;
    Ok(File::from(descriptor))
}

impl ContentSourceReader for PreparedContent {
    fn logical_len(&self) -> u64 {
        self.logical_len
    }

    fn open(&self) -> Result<Box<dyn Read + Send + '_>, PackfileError> {
        PreparedContent::open(self).map_err(Into::into)
    }

    fn open_range(
        &self,
        offset: u64,
        length: u64,
    ) -> Result<Box<dyn Read + Send + '_>, PackfileError> {
        let end = offset
            .checked_add(length)
            .filter(|end| *end <= self.logical_len)
            .ok_or(PackfileError::InvalidRecordRange)?;
        match &self.source {
            PreparedContentSource::StagedFile { path, .. } => {
                let mut file = open_staged_file(path)?;
                file.seek(SeekFrom::Start(offset))?;
                Ok(Box::new(file.take(length)))
            }
            PreparedContentSource::Memory(bytes) => {
                let start =
                    usize::try_from(offset).map_err(|_| PackfileError::InvalidRecordRange)?;
                let end = usize::try_from(end).map_err(|_| PackfileError::InvalidRecordRange)?;
                Ok(Box::new(Cursor::new(&bytes[start..end])))
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotContent {
    manifest: SnapshotManifest,
    namespace: namespace::BuiltPagedNamespaceSnapshot,
    content: BTreeMap<ContentId, PreparedContent>,
    preparation_lease: Option<PreparedSnapshotLease>,
    preparation_owner_marker: Option<PreparationOwnerMarker>,
}

impl SnapshotContent {
    pub fn new(
        draft: SnapshotDraft,
        files: BTreeMap<ContentId, Vec<u8>>,
        workspace_content_key: [u8; 32],
    ) -> Result<Self, NamespaceBuildError> {
        let content = files
            .into_iter()
            .map(|(content_id, bytes)| {
                let prepared = PreparedContent::memory(content_id.clone(), bytes);
                (content_id, prepared)
            })
            .collect();
        Self::from_prepared(draft, content, workspace_content_key)
    }

    pub fn from_prepared(
        draft: SnapshotDraft,
        content: BTreeMap<ContentId, PreparedContent>,
        workspace_content_key: [u8; 32],
    ) -> Result<Self, NamespaceBuildError> {
        let identity_key =
            namespace::MetadataIdentityKey::derive(&draft.workspace_id, workspace_content_key);
        Self::from_prepared_with_identity_key(draft, content, identity_key)
    }

    fn from_prepared_with_identity_key(
        draft: SnapshotDraft,
        content: BTreeMap<ContentId, PreparedContent>,
        identity_key: namespace::MetadataIdentityKey,
    ) -> Result<Self, NamespaceBuildError> {
        let entry_count = draft.entries.len() as u64;
        // Draft validation and canonical page construction each make the three bounded
        // semantic-identity passes over the entries.
        let build_entry_reads = entry_count.saturating_mul(6);
        let mut context = NamespaceOperationContext::uncancelled(namespace::operation_budget(
            build_entry_reads,
            entry_count,
            entry_count,
        ));
        let namespace =
            namespace::PageNamespaceBuilder::from_draft(draft, identity_key, &mut context)?;
        Ok(Self::from_built(namespace, content))
    }

    pub fn from_built(
        namespace: namespace::BuiltPagedNamespaceSnapshot,
        content: BTreeMap<ContentId, PreparedContent>,
    ) -> Self {
        let manifest = namespace.manifest();
        Self {
            manifest,
            namespace,
            content,
            preparation_lease: None,
            preparation_owner_marker: None,
        }
    }

    #[cfg(test)]
    pub fn file_bytes_for_path(&self, path: &str) -> Option<&[u8]> {
        let entry = self
            .entry_for_path(path)
            .expect("test snapshot page graph must be readable")?;
        self.file_bytes_for_entry(&entry)
    }

    pub fn manifest(&self) -> &SnapshotManifest {
        &self.manifest
    }

    pub fn namespace_reader(&self) -> namespace::PageNamespaceReader<'_> {
        namespace::PageNamespaceReader::new(&self.namespace)
    }

    pub fn namespace_snapshot(&self) -> &namespace::BuiltPagedNamespaceSnapshot {
        &self.namespace
    }

    pub fn namespace_store(&self) -> &namespace::PageStore {
        &self.namespace.store
    }

    pub fn visit_entries(
        &self,
        context: &mut NamespaceOperationContext<'_>,
        visitor: &mut dyn EntryVisitor,
    ) -> Result<(), NamespaceReadError> {
        self.namespace_reader()
            .visit_prefix(&WorkspaceRelativePath::new(""), visitor, context)?;
        Ok(())
    }

    pub fn visit_prefix(
        &self,
        prefix: &WorkspaceRelativePath,
        context: &mut NamespaceOperationContext<'_>,
        visitor: &mut dyn EntryVisitor,
    ) -> Result<(), NamespaceReadError> {
        self.namespace_reader()
            .visit_prefix(prefix, visitor, context)?;
        Ok(())
    }

    pub fn prepared_content(&self) -> &BTreeMap<ContentId, PreparedContent> {
        &self.content
    }

    pub fn prepared_content_mut(&mut self) -> &mut BTreeMap<ContentId, PreparedContent> {
        &mut self.content
    }

    pub fn preparation_lease(&self) -> Option<&PreparedSnapshotLease> {
        self.preparation_lease.as_ref()
    }

    pub fn attach_preparation_lease(&mut self, lease: PreparedSnapshotLease) {
        self.preparation_owner_marker = Some(lease.owner_marker.clone());
        self.preparation_lease = Some(lease);
    }

    pub fn preparation_owner_marker(&self) -> Option<&PreparationOwnerMarker> {
        self.preparation_owner_marker.as_ref()
    }

    pub fn attach_preparation_owner_marker(&mut self, owner: PreparationOwnerMarker) {
        self.preparation_owner_marker = Some(owner);
    }

    pub fn prepared_content_for_path(
        &self,
        path: &str,
    ) -> Result<Option<&PreparedContent>, NamespaceReadError> {
        let Some(entry) = self.entry_for_path(path)? else {
            return Ok(None);
        };
        let Some(content_id) = entry.content_id.as_ref() else {
            return Ok(None);
        };
        Ok(self.content.get(content_id))
    }

    pub fn read_file_for_path(&self, path: &str) -> io::Result<Option<Vec<u8>>> {
        let Some(content) = self
            .prepared_content_for_path(path)
            .map_err(io::Error::other)?
        else {
            return Ok(None);
        };
        let mut reader = content.open()?;
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes)?;
        Ok(Some(bytes))
    }

    pub fn remove_lease_owned_files(&self) -> io::Result<()> {
        for content in self.content.values() {
            if content.cleanup_policy != PreparedContentCleanup::LeaseOwned {
                continue;
            }
            let PreparedContentSource::StagedFile { path, .. } = &content.source else {
                continue;
            };
            match std::fs::remove_file(path.as_path()) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    #[cfg(test)]
    fn file_bytes_for_entry(&self, entry: &NamespaceEntry) -> Option<&[u8]> {
        let content_id = entry.content_id.as_ref()?;
        self.content.get(content_id)?.resident_bytes()
    }

    pub fn entry_for_path(&self, path: &str) -> Result<Option<NamespaceEntry>, NamespaceReadError> {
        let mut context =
            NamespaceOperationContext::uncancelled(namespace::operation_budget(1, 0, 0));
        self.namespace_reader()
            .get(&WorkspaceRelativePath::new(path), &mut context)
    }

    #[cfg(test)]
    pub(crate) fn entries_for_test(&self) -> Vec<NamespaceEntry> {
        struct Collector(Vec<NamespaceEntry>);

        impl EntryVisitor for Collector {
            fn visit(
                &mut self,
                entry: &NamespaceEntry,
                _context: &mut NamespaceOperationContext<'_>,
            ) -> Result<NamespaceVisitControl, NamespaceReadError> {
                self.0.push(entry.clone());
                Ok(NamespaceVisitControl::Continue)
            }
        }

        let mut context = NamespaceOperationContext::uncancelled(namespace::operation_budget(
            self.manifest.entry_count,
            0,
            0,
        ));
        let mut collector = Collector(Vec::new());
        self.visit_entries(&mut context, &mut collector)
            .expect("test snapshot page graph must be readable");
        collector.0
    }

    #[cfg(test)]
    pub(crate) fn mutate_entries_for_test(
        &mut self,
        mutate: impl FnOnce(&mut Vec<NamespaceEntry>),
    ) {
        let mut entries = self.entries_for_test();
        mutate(&mut entries);
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        let previous_snapshot_id = self.manifest.snapshot_id.clone();
        let identity = rebuild_manifest_identity(&self.manifest.workspace_id, &entries, "test");
        let snapshot_id = identity.snapshot_id;
        let refs = self
            .manifest
            .refs
            .iter()
            .cloned()
            .map(|mut reference| {
                if reference.target_snapshot_id == previous_snapshot_id {
                    reference.target_snapshot_id = snapshot_id.clone();
                }
                reference
            })
            .collect();
        let mut rebuilt = Self::from_prepared_with_identity_key(
            SnapshotDraft {
                schema_version: self.manifest.schema_version,
                snapshot_id,
                workspace_id: self.manifest.workspace_id.clone(),
                project_id: self.manifest.project_id.clone(),
                kind: self.manifest.kind,
                base_snapshot_id: self.manifest.base_snapshot_id.clone(),
                entries,
                refs,
            },
            self.content.clone(),
            self.namespace.store.identity_key(),
        )
        .expect("mutated test snapshot must remain canonical");
        rebuilt.preparation_lease = self.preparation_lease.clone();
        rebuilt.preparation_owner_marker = self.preparation_owner_marker.clone();
        *self = rebuilt;
    }
}

#[cfg(test)]
mod tests {
    use bowline_core::{
        policy::{AccessFlag, MaterializationMode, PathClassification},
        workspace_graph::{
            HydrationState, NamespaceEntryKind, RefKind, SnapshotDraft, SnapshotKind, WorkspaceRef,
        },
    };

    use super::*;

    #[test]
    fn indexed_lookup_matches_linear_manifest_scan() {
        let first_id = ContentId::new("cid_first");
        let second_id = ContentId::new("cid_second");
        let entries = vec![
            file_entry("src/main.rs", first_id.clone(), 11),
            directory_entry("target"),
            file_entry("README.md", second_id.clone(), 6),
        ];
        let workspace_id = WorkspaceId::new("ws_code");
        let snapshot_id = rebuild_manifest_identity(&workspace_id, &entries, "test").snapshot_id;
        let draft = SnapshotDraft {
            schema_version: bowline_core::workspace_graph::SNAPSHOT_SCHEMA_VERSION,
            snapshot_id: snapshot_id.clone(),
            workspace_id,
            project_id: None,
            kind: SnapshotKind::WorkspaceHead,
            base_snapshot_id: None,
            entries,
            refs: vec![WorkspaceRef {
                name: "workspace".to_string(),
                target_snapshot_id: snapshot_id,
                kind: RefKind::Workspace,
            }],
        };
        let mut files = BTreeMap::new();
        files.insert(first_id, b"fn main() {}".to_vec());
        files.insert(second_id, b"# docs".to_vec());
        let snapshot = SnapshotContent::new(draft, files, [7; 32]).expect("page-backed snapshot");
        for path in ["src/main.rs", "target", "README.md", "missing.rs"] {
            let expected = snapshot
                .entries_for_test()
                .iter()
                .find(|entry| entry.path == path)
                .and_then(|entry| entry.content_id.as_ref())
                .and_then(|content_id| snapshot.prepared_content().get(content_id))
                .and_then(PreparedContent::resident_bytes);
            assert_eq!(snapshot.file_bytes_for_path(path), expected);
        }
    }

    #[test]
    fn indexed_lookup_rejects_duplicate_canonical_paths() {
        let first_id = ContentId::new("cid_first");
        let second_id = ContentId::new("cid_second");
        let workspace_id = WorkspaceId::new("ws_code");
        let entries = vec![
            file_entry("src/main.rs", first_id.clone(), 11),
            file_entry("src/main.rs", second_id.clone(), 12),
        ];
        let snapshot_id = rebuild_manifest_identity(&workspace_id, &entries, "test").snapshot_id;
        let draft = SnapshotDraft {
            schema_version: bowline_core::workspace_graph::SNAPSHOT_SCHEMA_VERSION,
            snapshot_id: snapshot_id.clone(),
            workspace_id,
            project_id: None,
            kind: SnapshotKind::WorkspaceHead,
            base_snapshot_id: None,
            entries,
            refs: vec![WorkspaceRef {
                name: "workspace".to_string(),
                target_snapshot_id: snapshot_id,
                kind: RefKind::Workspace,
            }],
        };
        let mut files = BTreeMap::new();
        files.insert(first_id, b"first".to_vec());
        files.insert(second_id, b"second".to_vec());

        let error =
            SnapshotContent::new(draft, files, [7; 32]).expect_err("duplicate path must fail");

        assert!(matches!(
            error,
            NamespaceBuildError::Read(NamespaceReadError::DuplicatePath { field: "path" })
        ));
    }

    fn file_entry(path: &str, content_id: ContentId, len: usize) -> NamespaceEntry {
        NamespaceEntry {
            path: path.to_string(),
            kind: NamespaceEntryKind::File,
            classification: PathClassification::WorkspaceSync,
            mode: MaterializationMode::EncryptedSync,
            access: vec![AccessFlag::HumanReadable],
            content_id: Some(content_id),
            content_layout: None,
            symlink_target: None,
            byte_len: Some(len as u64),
            executability: bowline_core::workspace_graph::FileExecutability::Regular,
            hydration_state: HydrationState::Local,
        }
    }

    fn directory_entry(path: &str) -> NamespaceEntry {
        NamespaceEntry {
            path: path.to_string(),
            kind: NamespaceEntryKind::Directory,
            classification: PathClassification::WorkspaceSync,
            mode: MaterializationMode::EncryptedSync,
            access: vec![AccessFlag::HumanReadable],
            content_id: None,
            content_layout: None,
            symlink_target: None,
            byte_len: None,
            executability: bowline_core::workspace_graph::FileExecutability::Regular,
            hydration_state: HydrationState::Local,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateBase {
    pub workspace_id: WorkspaceId,
    pub version: u64,
    pub snapshot_id: SnapshotId,
}

impl CandidateBase {
    pub fn from_remote(remote: &RemoteWorkspaceRef) -> Self {
        Self {
            workspace_id: WorkspaceId::new(remote.workspace_id.clone()),
            version: remote.version,
            snapshot_id: SnapshotId::new(remote.snapshot_id.clone()),
        }
    }
}

#[cfg(test)]
pub(crate) fn snapshot_id_from_hasher(prefix: &str, hasher: blake3::Hasher) -> SnapshotId {
    let hash = hasher.finalize().to_hex();
    SnapshotId::new(format!("{prefix}_{}", &hash[..24]))
}

pub fn manifest_id_for_snapshot(snapshot_id: &SnapshotId) -> ManifestId {
    ManifestId::new(format!(
        "mf_{}",
        short_hash([snapshot_id.as_str().as_bytes()])
    ))
}

pub(crate) fn hash_entry_part(hasher: &mut blake3::Hasher, part: &[u8]) {
    hasher.update(&(part.len() as u64).to_le_bytes());
    hasher.update(part);
}

pub(crate) fn short_hash(parts: impl IntoIterator<Item = impl AsRef<[u8]>>) -> String {
    let mut hasher = blake3::Hasher::new();
    for part in parts {
        hash_entry_part(&mut hasher, part.as_ref());
    }
    hasher.finalize().to_hex()[..24].to_string()
}

#[derive(Debug)]
pub enum SyncError {
    Coalesce(CoalesceError),
    Upload(UploadError),
    Download(DownloadError),
    Merge(MergeError),
    ConflictBundle(ConflictBundleError),
}

impl fmt::Display for SyncError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Coalesce(error) => error.fmt(formatter),
            Self::Upload(error) => error.fmt(formatter),
            Self::Download(error) => error.fmt(formatter),
            Self::Merge(error) => error.fmt(formatter),
            Self::ConflictBundle(error) => error.fmt(formatter),
        }
    }
}

impl Error for SyncError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Coalesce(error) => Some(error),
            Self::Upload(error) => Some(error),
            Self::Download(error) => Some(error),
            Self::Merge(error) => Some(error),
            Self::ConflictBundle(error) => Some(error),
        }
    }
}
