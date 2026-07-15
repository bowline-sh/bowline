use std::collections::{BTreeMap, BTreeSet};

use bowline_core::{
    ids::{ManifestDigest, SnapshotId, WorkspaceId},
    namespace_snapshot::{
        EntryVisitor, NamespaceOperationContext, NamespaceReadError, NamespaceSnapshotReader,
        NamespaceVisitControl,
    },
    workspace_graph::{NamespaceEntry, WorkspaceRelativePath},
};

use crate::sync::{hash_entry_part, hash_namespace_entry_identity};

const ROOT_LEVEL_BOUNDARY: &str = "root-level";
const SUBTREE_BUCKETS: u64 = 16;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct SemanticChunkBoundary(String);

impl SemanticChunkBoundary {
    pub(crate) fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SemanticChunkDigest(String);

impl SemanticChunkDigest {
    pub(crate) fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

pub type SemanticManifestDigest = ManifestDigest;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticManifestIdentity {
    digest: SemanticManifestDigest,
    snapshot_id: SnapshotId,
    chunk_count: u64,
    entries_hashed: u64,
}

impl SemanticManifestIdentity {
    pub fn digest(&self) -> &SemanticManifestDigest {
        &self.digest
    }

    pub fn snapshot_id(&self) -> &SnapshotId {
        &self.snapshot_id
    }

    pub fn chunk_count(&self) -> u64 {
        self.chunk_count
    }

    pub fn entries_hashed(&self) -> u64 {
        self.entries_hashed
    }
}

pub(crate) fn semantic_manifest_identity(
    workspace_id: &WorkspaceId,
    entries: &[NamespaceEntry],
) -> SemanticManifestIdentity {
    let chunks = semantic_manifest_chunks(entries);
    let digests = chunks
        .iter()
        .map(|(boundary, entries)| (boundary.clone(), semantic_chunk_digest(boundary, entries)))
        .collect::<BTreeMap<_, _>>();
    let mut identity = semantic_manifest_identity_from_chunks(workspace_id, &digests);
    identity.entries_hashed = entries.len() as u64;
    identity
}

pub fn semantic_manifest_identity_with_context(
    workspace_id: &WorkspaceId,
    entries: &[NamespaceEntry],
    context: &mut NamespaceOperationContext<'_>,
) -> Result<SemanticManifestIdentity, NamespaceReadError> {
    let roots_with_children = validate_order_and_find_roots(entries, context)?;
    let counts = count_chunks(entries, &roots_with_children, context)?;
    let digests = digest_bounded_chunks(entries, &roots_with_children, &counts, context)?;
    let mut identity = semantic_manifest_identity_from_chunks(workspace_id, &digests);
    identity.entries_hashed = entries.len() as u64;
    Ok(identity)
}

pub(crate) fn semantic_manifest_identity_from_reader(
    reader: &dyn NamespaceSnapshotReader,
    context: &mut NamespaceOperationContext<'_>,
) -> Result<SemanticManifestIdentity, NamespaceReadError> {
    let mut previous = None::<String>;
    let mut roots_with_children = BTreeSet::new();
    let mut entry_count = 0_u64;
    visit_reader_entries(reader, context, &mut |entry| {
        if let Some(prior) = previous.as_deref() {
            match prior.cmp(entry.path.as_str()) {
                std::cmp::Ordering::Less => {}
                std::cmp::Ordering::Equal => {
                    return Err(NamespaceReadError::DuplicatePath { field: "path" });
                }
                std::cmp::Ordering::Greater => {
                    return Err(NamespaceReadError::NonCanonicalOrder { field: "path" });
                }
            }
        }
        if let Some((root, _)) = entry.path.split_once('/') {
            roots_with_children.insert(root.to_string());
        }
        previous = Some(entry.path.clone());
        entry_count = entry_count.saturating_add(1);
        Ok(())
    })?;

    let mut counts = BTreeMap::<SemanticChunkBoundary, u64>::new();
    visit_reader_entries(reader, context, &mut |entry| {
        let boundary = boundary_for_path(&entry.path, &roots_with_children);
        let count = counts.entry(boundary).or_default();
        *count = count.saturating_add(1);
        Ok(())
    })?;
    let mut hashers = counts
        .iter()
        .map(|(boundary, count)| (boundary.clone(), chunk_hasher(boundary, *count)))
        .collect::<BTreeMap<_, _>>();
    visit_reader_entries(reader, context, &mut |entry| {
        let boundary = boundary_for_path(&entry.path, &roots_with_children);
        let hasher = hashers
            .get_mut(&boundary)
            .ok_or(NamespaceReadError::CorruptGraph {
                reason: "streamed semantic chunk was not counted",
            })?;
        hash_namespace_entry_identity(hasher, entry);
        Ok(())
    })?;
    let digests = hashers
        .into_iter()
        .map(|(boundary, hasher)| {
            (
                boundary,
                SemanticChunkDigest::new(hasher.finalize().to_hex().to_string()),
            )
        })
        .collect();
    let mut identity =
        semantic_manifest_identity_from_chunks(&reader.metadata().workspace_id, &digests);
    identity.entries_hashed = entry_count;
    Ok(identity)
}

fn visit_reader_entries(
    reader: &dyn NamespaceSnapshotReader,
    context: &mut NamespaceOperationContext<'_>,
    visitor: &mut dyn FnMut(&NamespaceEntry) -> Result<(), NamespaceReadError>,
) -> Result<(), NamespaceReadError> {
    struct Adapter<'a>(&'a mut dyn FnMut(&NamespaceEntry) -> Result<(), NamespaceReadError>);
    impl EntryVisitor for Adapter<'_> {
        fn visit(
            &mut self,
            entry: &NamespaceEntry,
            _context: &mut NamespaceOperationContext<'_>,
        ) -> Result<NamespaceVisitControl, NamespaceReadError> {
            (self.0)(entry)?;
            Ok(NamespaceVisitControl::Continue)
        }
    }
    reader.visit_prefix(
        &WorkspaceRelativePath::new(""),
        &mut Adapter(visitor),
        context,
    )?;
    Ok(())
}

fn validate_order_and_find_roots(
    entries: &[NamespaceEntry],
    context: &mut NamespaceOperationContext<'_>,
) -> Result<BTreeSet<String>, NamespaceReadError> {
    let mut previous: Option<&str> = None;
    let mut roots_with_children = BTreeSet::new();
    for entry in entries {
        context.charge_entries(1)?;
        if let Some(previous) = previous {
            match previous.cmp(entry.path.as_str()) {
                std::cmp::Ordering::Less => {}
                std::cmp::Ordering::Equal => {
                    return Err(NamespaceReadError::DuplicatePath { field: "path" });
                }
                std::cmp::Ordering::Greater => {
                    return Err(NamespaceReadError::NonCanonicalOrder { field: "path" });
                }
            }
        }
        if let Some((root, _)) = entry.path.split_once('/') {
            roots_with_children.insert(root.to_string());
        }
        previous = Some(&entry.path);
    }
    Ok(roots_with_children)
}

fn count_chunks(
    entries: &[NamespaceEntry],
    roots_with_children: &BTreeSet<String>,
    context: &mut NamespaceOperationContext<'_>,
) -> Result<BTreeMap<SemanticChunkBoundary, u64>, NamespaceReadError> {
    let mut counts = BTreeMap::<SemanticChunkBoundary, u64>::new();
    for entry in entries {
        context.charge_entries(1)?;
        let boundary = boundary_for_path(&entry.path, roots_with_children);
        let count = counts.entry(boundary).or_default();
        *count = count.saturating_add(1);
    }
    Ok(counts)
}

fn digest_bounded_chunks(
    entries: &[NamespaceEntry],
    roots_with_children: &BTreeSet<String>,
    counts: &BTreeMap<SemanticChunkBoundary, u64>,
    context: &mut NamespaceOperationContext<'_>,
) -> Result<BTreeMap<SemanticChunkBoundary, SemanticChunkDigest>, NamespaceReadError> {
    let mut hashers = counts
        .iter()
        .map(|(boundary, count)| (boundary.clone(), chunk_hasher(boundary, *count)))
        .collect::<BTreeMap<_, _>>();
    for entry in entries {
        context.charge_entries(1)?;
        let boundary = boundary_for_path(&entry.path, roots_with_children);
        let hasher = hashers
            .get_mut(&boundary)
            .expect("counted chunk has a bounded hasher");
        hash_namespace_entry_identity(hasher, entry);
    }
    Ok(hashers
        .into_iter()
        .map(|(boundary, hasher)| {
            (
                boundary,
                SemanticChunkDigest::new(hasher.finalize().to_hex().to_string()),
            )
        })
        .collect())
}

pub(crate) fn semantic_manifest_chunks(
    entries: &[NamespaceEntry],
) -> BTreeMap<SemanticChunkBoundary, Vec<&NamespaceEntry>> {
    let roots_with_children = roots_with_children(entries);
    let mut chunks = BTreeMap::<SemanticChunkBoundary, Vec<&NamespaceEntry>>::new();
    for entry in entries {
        let boundary = boundary_for_path(&entry.path, &roots_with_children);
        chunks.entry(boundary).or_default().push(entry);
    }
    chunks
}

pub(crate) fn semantic_chunk_digest(
    boundary: &SemanticChunkBoundary,
    entries: &[&NamespaceEntry],
) -> SemanticChunkDigest {
    let mut hasher = chunk_hasher(boundary, entries.len() as u64);
    for entry in entries {
        hash_namespace_entry_identity(&mut hasher, entry);
    }
    SemanticChunkDigest::new(hasher.finalize().to_hex().to_string())
}

fn chunk_hasher(boundary: &SemanticChunkBoundary, entry_count: u64) -> blake3::Hasher {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"bowline manifest chunk v1\0");
    hash_entry_part(&mut hasher, boundary.as_str().as_bytes());
    hash_entry_part(&mut hasher, &entry_count.to_le_bytes());
    hasher
}

pub(crate) fn semantic_manifest_identity_from_chunks(
    workspace_id: &WorkspaceId,
    chunks: &BTreeMap<SemanticChunkBoundary, SemanticChunkDigest>,
) -> SemanticManifestIdentity {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"bowline chunked snapshot identity v2\0");
    hash_entry_part(&mut hasher, workspace_id.as_str().as_bytes());
    hash_entry_part(&mut hasher, &(chunks.len() as u64).to_le_bytes());
    for (boundary, digest) in chunks {
        hash_entry_part(&mut hasher, boundary.as_str().as_bytes());
        hash_entry_part(&mut hasher, digest.as_str().as_bytes());
    }
    let digest = hasher.finalize().to_hex().to_string();
    SemanticManifestIdentity {
        snapshot_id: SnapshotId::new(format!("snap_{}", &digest[..24])),
        digest: ManifestDigest::new(digest),
        chunk_count: chunks.len() as u64,
        entries_hashed: 0,
    }
}

fn roots_with_children(entries: &[NamespaceEntry]) -> BTreeSet<String> {
    entries
        .iter()
        .filter_map(|entry| entry.path.split_once('/').map(|(root, _)| root.to_string()))
        .collect()
}

fn boundary_for_path(path: &str, roots_with_children: &BTreeSet<String>) -> SemanticChunkBoundary {
    let Some((root, _)) = path.split_once('/') else {
        if roots_with_children.contains(path) {
            return subtree_boundary(path, 0);
        }
        return SemanticChunkBoundary::new(ROOT_LEVEL_BOUNDARY);
    };
    subtree_boundary(root, bucket_for_path(path))
}

fn subtree_boundary(root: &str, bucket: u64) -> SemanticChunkBoundary {
    SemanticChunkBoundary::new(format!("subtree:{root}:{bucket:02}"))
}

fn bucket_for_path(path: &str) -> u64 {
    let mut hasher = blake3::Hasher::new();
    hash_entry_part(&mut hasher, path.as_bytes());
    let digest = hasher.finalize();
    u64::from(digest.as_bytes()[0]) % SUBTREE_BUCKETS
}
