use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt, fs, io,
    sync::{Arc, Mutex},
};

use bowline_core::{
    namespace_snapshot::{NamespaceOperationContext, NamespaceReadError},
    workspace_graph::{SNAPSHOT_SCHEMA_VERSION, SnapshotManifest},
};
use bowline_storage::SNAPSHOT_METADATA_PAGE_MAX_CANONICAL_BYTES;

use crate::metadata::{
    MetadataCacheState, MetadataError, MetadataRecordKind as LocalRecordKind, MetadataRecordRef,
    MetadataStore, MetadataVerificationState, SnapshotRecord,
};

use super::{
    SnapshotContent,
    namespace::{
        BuiltPagedNamespaceSnapshot, MetadataIdentityKey, MetadataRecordKind, PageStore,
        PagedRecordSource,
    },
};

pub fn load_cached_snapshot(
    store: &MetadataStore,
    snapshot: &SnapshotRecord,
) -> Result<SnapshotContent, CachedSnapshotError> {
    let completeness = store.snapshot_root_completeness(&snapshot.workspace_id, &snapshot.id)?;
    if !completeness.complete {
        return Err(CachedSnapshotError::IncompleteGraph);
    }
    let database_path = store.database_path()?;
    let identity_key = store
        .metadata_identity_key(&snapshot.workspace_id)?
        .map(MetadataIdentityKey::from_bytes)
        .ok_or(CachedSnapshotError::MissingIdentityKey)?;
    let source = CachedPageSource {
        store: Mutex::new(MetadataStore::open_read_only(database_path)?),
        workspace_id: snapshot.workspace_id.clone(),
        verified: Mutex::new(BTreeMap::new()),
        identity_key,
    };
    let pages = PageStore::from_source(Arc::new(source));
    let manifest = SnapshotManifest {
        schema_version: SNAPSHOT_SCHEMA_VERSION,
        snapshot_id: snapshot.id.clone(),
        workspace_id: snapshot.workspace_id.clone(),
        project_id: snapshot.project_id.clone(),
        kind: snapshot.kind,
        base_snapshot_id: snapshot.base_snapshot_id.clone(),
        namespace_root_id: snapshot.root_id.clone(),
        semantic_manifest_digest: snapshot.semantic_manifest_digest.clone(),
        entry_count: snapshot.entry_count,
        refs: snapshot.refs.clone(),
    };
    let namespace = BuiltPagedNamespaceSnapshot::from_manifest(manifest, pages);
    Ok(SnapshotContent::from_built(namespace, BTreeMap::new()))
}

struct CachedPageSource {
    store: Mutex<MetadataStore>,
    workspace_id: bowline_core::ids::WorkspaceId,
    verified: Mutex<BTreeMap<String, Arc<[u8]>>>,
    identity_key: MetadataIdentityKey,
}

impl PagedRecordSource for CachedPageSource {
    fn metadata_identity_key(&self) -> MetadataIdentityKey {
        self.identity_key
    }

    fn load_record(
        &self,
        kind: MetadataRecordKind,
        logical_id: &str,
        context: &mut NamespaceOperationContext<'_>,
    ) -> Result<Option<Arc<[u8]>>, NamespaceReadError> {
        context.ensure_active()?;
        if let Some(bytes) = self
            .verified
            .lock()
            .map_err(|_| cache_corrupt())?
            .get(logical_id)
            .cloned()
        {
            return Ok(Some(bytes));
        }
        let record = MetadataRecordRef {
            kind: local_record_kind(kind),
            logical_id: crate::metadata::MetadataLogicalId::new(logical_id),
        };
        let store = self.store.lock().map_err(|_| cache_corrupt())?;
        let cache = store
            .metadata_cache_record(&self.workspace_id, &record)
            .map_err(|_| cache_corrupt())?
            .ok_or(NamespaceReadError::MissingRecord {
                record: "cached metadata record",
            })?;
        if store
            .metadata_object_binding(&self.workspace_id, record.kind, &record.logical_id)
            .map_err(|_| cache_corrupt())?
            .is_some_and(|binding| {
                binding.verification_state != MetadataVerificationState::Verified
            })
        {
            return Err(cache_corrupt());
        }
        if cache.state != MetadataCacheState::Present {
            return Ok(None);
        }
        if cache.encoded_bytes > SNAPSHOT_METADATA_PAGE_MAX_CANONICAL_BYTES as u64 {
            return Err(cache_corrupt());
        }
        match kind {
            MetadataRecordKind::NamespacePage => {
                context.ensure_namespace_page_capacity(cache.encoded_bytes)?;
            }
            MetadataRecordKind::ContentLayout => {
                context.ensure_layout_record_capacity(cache.encoded_bytes)?;
            }
            MetadataRecordKind::SegmentPage => {
                context.ensure_segment_page_capacity(cache.encoded_bytes)?;
            }
        }
        context.ensure_active()?;
        let path = cache.cache_path.ok_or(NamespaceReadError::MissingRecord {
            record: "cached metadata plaintext",
        })?;
        let canonical = fs::read(path).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                NamespaceReadError::MissingRecord {
                    record: "cached metadata plaintext",
                }
            } else {
                cache_corrupt()
            }
        })?;
        if canonical.len() as u64 != cache.encoded_bytes {
            return Err(cache_corrupt());
        }
        let mut verified = PageStore::with_identity_key(self.identity_key);
        verified.insert_verified(kind, logical_id, canonical)?;
        let summary =
            verified
                .metadata_record(logical_id)?
                .ok_or(NamespaceReadError::MissingRecord {
                    record: "verified cached metadata summary",
                })?;
        let expected = store
            .metadata_record_children(&self.workspace_id, &record)
            .map_err(|_| cache_corrupt())?
            .into_iter()
            .map(|child| child.logical_id.as_str().to_string())
            .collect::<BTreeSet<_>>();
        if summary
            .child_logical_ids
            .into_iter()
            .collect::<BTreeSet<_>>()
            != expected
        {
            return Err(cache_corrupt());
        }
        let bytes = Arc::<[u8]>::from(
            verified
                .plaintext_record(logical_id)?
                .map(|record| record.plaintext)
                .ok_or(NamespaceReadError::MissingRecord {
                    record: "verified cached metadata plaintext",
                })?,
        );
        self.verified
            .lock()
            .map_err(|_| cache_corrupt())?
            .insert(logical_id.to_string(), Arc::clone(&bytes));
        Ok(Some(bytes))
    }
}

fn local_record_kind(kind: MetadataRecordKind) -> LocalRecordKind {
    match kind {
        MetadataRecordKind::NamespacePage => LocalRecordKind::NamespacePage,
        MetadataRecordKind::ContentLayout => LocalRecordKind::ContentLayout,
        MetadataRecordKind::SegmentPage => LocalRecordKind::SegmentPage,
    }
}

fn cache_corrupt() -> NamespaceReadError {
    NamespaceReadError::CorruptGraph {
        reason: "verified local metadata cache is inconsistent",
    }
}

#[derive(Debug)]
pub enum CachedSnapshotError {
    Metadata(MetadataError),
    Io(io::Error),
    Namespace(NamespaceReadError),
    MissingCache(String),
    CacheLengthMismatch,
    EdgeMismatch,
    IncompleteGraph,
    MissingIdentityKey,
    RecordBudgetExceeded,
    UnsupportedRecordKind,
}

impl fmt::Display for CachedSnapshotError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Metadata(error) => error.fmt(formatter),
            Self::Io(error) => write!(formatter, "cached namespace I/O failed: {error}"),
            Self::Namespace(error) => error.fmt(formatter),
            Self::MissingCache(id) => {
                write!(formatter, "verified metadata cache `{id}` is unavailable")
            }
            Self::CacheLengthMismatch => {
                formatter.write_str("cached metadata length does not match its record")
            }
            Self::EdgeMismatch => {
                formatter.write_str("cached metadata edges do not match canonical plaintext")
            }
            Self::IncompleteGraph => formatter.write_str("cached snapshot graph is incomplete"),
            Self::MissingIdentityKey => {
                formatter.write_str("cached snapshot metadata identity key is unavailable")
            }
            Self::RecordBudgetExceeded => {
                formatter.write_str("cached snapshot graph exceeds its record budget")
            }
            Self::UnsupportedRecordKind => {
                formatter.write_str("snapshot cache contains an unsupported record kind")
            }
        }
    }
}

impl Error for CachedSnapshotError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Metadata(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::Namespace(error) => Some(error),
            Self::MissingCache(_)
            | Self::CacheLengthMismatch
            | Self::EdgeMismatch
            | Self::IncompleteGraph
            | Self::MissingIdentityKey
            | Self::RecordBudgetExceeded
            | Self::UnsupportedRecordKind => None,
        }
    }
}

impl From<MetadataError> for CachedSnapshotError {
    fn from(error: MetadataError) -> Self {
        Self::Metadata(error)
    }
}

impl From<io::Error> for CachedSnapshotError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<NamespaceReadError> for CachedSnapshotError {
    fn from(error: NamespaceReadError) -> Self {
        Self::Namespace(error)
    }
}
