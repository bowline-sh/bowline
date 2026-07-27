//! Fetch a manifest tree from its root node back into the flat map the merge
//! matrix reconciles against.
//!
//! Two things happen on the way. The walk enforces every bound a hostile peer's
//! tree could otherwise blow past — per-node sealed and decoded size, per-node
//! fan-out, total record count, path length, path depth, aggregate declared
//! bytes, aggregate decoded bytes — and it PRUNES: a child node key that already names a subtree this
//! device holds is not downloaded at all, and the local copy is used verbatim.
//! Pruning is safe for exactly one reason: the node key is a collision-resistant
//! function of the whole subtree, so key equality IS content equality.

use std::collections::{BTreeMap, HashMap};

use super::super::counters::EngineCounters;
use super::super::endpoint::NameFolding;
use super::super::manifest::directory_tree::{DirPath, SubtreeHash};
use super::super::manifest::tree::{TreeEntryPayload, TreeNode};
use super::super::manifest::{
    DecodeLimits, DecodedManifest, Manifest, ManifestEntry, ManifestError, ManifestKey,
    PathCollision, WorkspaceCrypto, WorkspacePath, open_tree_node, physical_manifest_key,
    validate_manifest_path,
};
use super::super::push::RemoteObjects;
use super::{TreeError, TreeNodeLookup};

/// What this device already holds locally, so an unchanged subtree costs nothing.
pub struct PruneBasis<'a> {
    /// The local flat entries a pruned subtree is filled from.
    pub entries: &'a BTreeMap<WorkspacePath, ManifestEntry>,
    /// Subtree content hashes of exactly those entries.
    pub hashes: &'a BTreeMap<DirPath, SubtreeHash>,
    /// Maps a subtree's content to the node object key that carries it.
    pub ledger: &'a dyn TreeNodeLookup,
}

pub struct FetchTreeRequest<'a, O: RemoteObjects> {
    pub objects: &'a O,
    pub crypto: &'a WorkspaceCrypto,
    pub counters: &'a EngineCounters,
    pub root: &'a ManifestKey,
    pub limits: &'a DecodeLimits,
    pub names: NameFolding,
    /// `None` where the caller holds no comparable local state — a work-view
    /// read of an arbitrary historical manifest has nothing to prune against.
    pub prune: Option<PruneBasis<'a>>,
}

/// A fetched manifest plus the node key each directory arrived under, so the
/// caller can teach its ledger which subtrees the object store already holds.
pub struct FetchedTree {
    pub decoded: DecodedManifest,
    pub node_keys: BTreeMap<DirPath, ManifestKey>,
}

pub fn fetch_tree<O: RemoteObjects>(
    request: FetchTreeRequest<'_, O>,
) -> Result<FetchedTree, TreeError> {
    let mut walk = TreeWalk {
        objects: request.objects,
        crypto: request.crypto,
        counters: request.counters,
        limits: request.limits,
        names: request.names,
        prune: request.prune,
        entries: BTreeMap::new(),
        folded: HashMap::new(),
        node_keys: BTreeMap::new(),
        records: 0,
        aggregate: 0,
        decoded: 0,
    };
    walk.descend(&DirPath::root(), request.root, 0)?;
    Ok(FetchedTree {
        decoded: DecodedManifest {
            manifest: Manifest::new(request.crypto.key_epoch(), walk.entries),
            collisions: collisions(walk.folded),
        },
        node_keys: walk.node_keys,
    })
}

struct TreeWalk<'a, O: RemoteObjects> {
    objects: &'a O,
    crypto: &'a WorkspaceCrypto,
    counters: &'a EngineCounters,
    limits: &'a DecodeLimits,
    names: NameFolding,
    prune: Option<PruneBasis<'a>>,
    entries: BTreeMap<WorkspacePath, ManifestEntry>,
    folded: HashMap<String, Vec<WorkspacePath>>,
    node_keys: BTreeMap<DirPath, ManifestKey>,
    records: u64,
    aggregate: u64,
    decoded: u64,
}

impl<O: RemoteObjects> TreeWalk<'_, O> {
    fn descend(&mut self, dir: &DirPath, key: &ManifestKey, depth: u64) -> Result<(), TreeError> {
        if depth > self.limits.max_depth {
            return Err(bound("tree-depth"));
        }
        self.node_keys.insert(dir.clone(), key.clone());
        if self.prune_subtree(dir, key)? {
            return Ok(());
        }
        for entry in self.open_node(key)?.entries {
            let child = dir.child(&entry.name);
            self.count_record()?;
            match entry.payload {
                TreeEntryPayload::File {
                    size,
                    mode,
                    content_id,
                    blob_key,
                    key_epoch,
                } => self.emit(
                    &child,
                    ManifestEntry::File {
                        size,
                        mode,
                        content_id,
                        blob_key,
                        key_epoch,
                    },
                )?,
                TreeEntryPayload::Symlink { mode, target } => {
                    self.emit(&child, ManifestEntry::Symlink { mode, target })?;
                }
                TreeEntryPayload::Directory { mode, child: node } => {
                    self.emit(&child, ManifestEntry::Directory { mode })?;
                    self.descend(&child, &node, depth + 1)?;
                }
                TreeEntryPayload::Subtree { child: node } => {
                    // A structural directory owns no manifest entry, but its path
                    // still has to clear the reader's predicate: descending into a
                    // name the writer could never have published is how a hostile
                    // tree would smuggle depth or reserved names past the walk.
                    self.validate(&child)?;
                    self.descend(&child, &node, depth + 1)?;
                }
            }
        }
        Ok(())
    }

    fn open_node(&mut self, key: &ManifestKey) -> Result<TreeNode, TreeError> {
        let sealed = self
            .objects
            .get_manifest(key)
            .map_err(TreeError::Transport)?;
        if &physical_manifest_key(&sealed) != key {
            return Err(TreeError::NodeKeyMismatch);
        }
        let (plaintext, key_epoch) =
            open_tree_node(self.crypto, &sealed, self.limits).map_err(TreeError::Manifest)?;
        // The per-node cap bounds one object; this bounds the whole walk. Without
        // it a hostile peer publishes many individually valid nodes and this
        // device holds all of their entries at once.
        self.decoded = self.decoded.saturating_add(plaintext.len() as u64);
        if self.decoded > self.limits.max_aggregate_decoded_bytes {
            return Err(bound("tree-aggregate-decoded-bytes"));
        }
        self.counters.record_manifest_download(sealed.len() as u64);
        TreeNode::decode(&plaintext, key_epoch, self.limits).map_err(TreeError::Manifest)
    }

    /// Reuse the local copy of this subtree when the remote names the very node
    /// object this device's own content hashes to.
    fn prune_subtree(&mut self, dir: &DirPath, key: &ManifestKey) -> Result<bool, TreeError> {
        let Some(basis) = &self.prune else {
            return Ok(false);
        };
        let Some(hash) = basis.hashes.get(dir) else {
            return Ok(false);
        };
        if basis.ledger.known(hash)?.as_ref() != Some(key) {
            return Ok(false);
        }
        let local: Vec<(WorkspacePath, ManifestEntry)> = descendants(basis.entries, dir)
            .map(|(path, entry)| (path.clone(), entry.clone()))
            .collect();
        for (path, entry) in local {
            self.count_record()?;
            self.emit_path(path, entry)?;
        }
        Ok(true)
    }

    fn count_record(&mut self) -> Result<(), TreeError> {
        self.records += 1;
        if self.records > self.limits.max_records {
            return Err(bound("record-count"));
        }
        Ok(())
    }

    fn validate(&self, dir: &DirPath) -> Result<(), TreeError> {
        validate_manifest_path(dir.as_str(), self.limits).map_err(TreeError::Manifest)
    }

    fn emit(&mut self, dir: &DirPath, entry: ManifestEntry) -> Result<(), TreeError> {
        self.emit_path(dir.to_workspace_path(), entry)
    }

    /// Every path the walk produces clears the reader's predicate, including the
    /// ones a prune lifted from local state: the ancestor was validated under
    /// the limits in force when it was written, and there is no reason to make
    /// this walk's guarantee depend on that.
    fn emit_path(&mut self, path: WorkspacePath, entry: ManifestEntry) -> Result<(), TreeError> {
        validate_manifest_path(path.as_str(), self.limits).map_err(TreeError::Manifest)?;
        if let ManifestEntry::File { size, .. } = &entry {
            self.aggregate = self
                .aggregate
                .checked_add(*size)
                .filter(|total| *total <= self.limits.max_aggregate_declared_bytes)
                .ok_or_else(|| bound("aggregate-declared-size"))?;
        }
        self.folded
            .entry(self.names.fold(path.as_str()))
            .or_default()
            .push(path.clone());
        self.entries.insert(path, entry);
        Ok(())
    }
}

/// The entries strictly below `dir`, in path order.
fn descendants<'a>(
    entries: &'a BTreeMap<WorkspacePath, ManifestEntry>,
    dir: &DirPath,
) -> impl Iterator<Item = (&'a WorkspacePath, &'a ManifestEntry)> {
    let prefix = if dir.is_root() {
        String::new()
    } else {
        format!("{}/", dir.as_str())
    };
    entries
        .range(WorkspacePath::new(prefix.clone())..)
        .take_while(move |(path, _)| path.as_str().starts_with(&prefix))
}

/// Collisions are whatever THIS endpoint cannot distinguish, so the fold is the
/// probed one: a case-sensitive ext4 volume genuinely holds `Foo` and `foo` as
/// two files and must not be told they collide, while APFS folds both case and
/// normalization form and must be.
fn collisions(folded: HashMap<String, Vec<WorkspacePath>>) -> Vec<PathCollision> {
    let mut collisions: Vec<PathCollision> = folded
        .into_iter()
        .filter(|(_, paths)| paths.len() > 1)
        .map(|(folded_key, mut paths)| {
            paths.sort();
            PathCollision {
                folded: folded_key,
                paths,
            }
        })
        .collect();
    collisions.sort_by(|left, right| left.folded.cmp(&right.folded));
    collisions
}

fn bound(bound: &'static str) -> TreeError {
    TreeError::Manifest(ManifestError::BoundExceeded { bound })
}
