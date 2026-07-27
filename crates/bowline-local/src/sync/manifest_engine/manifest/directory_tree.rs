//! The directory decomposition of a flat manifest, and the content hash that
//! identifies each subtree locally.
//!
//! Two jobs live here, and they are the same job seen from both ends of the
//! wire. [`DirectoryTree::decompose`] turns the in-memory flat manifest into the
//! per-directory shape the tree format publishes; [`DirectoryTree::subtree_hashes`]
//! gives every directory a hash of its ENTIRE subtree that costs one BLAKE3 pass
//! and no sealing.
//!
//! That hash is what makes the node ledger safe. A node's physical key is
//! `blake3(seal(plaintext))` and sealing is convergent, so the key is a pure
//! function of the subtree's content — which means a cache keyed by
//! [`SubtreeHash`] is content-addressed and can never go stale. There is no
//! path-keyed index to keep transactionally in step with the ancestor table, and
//! a crash can only lose cached work, never make the cache lie.

use std::collections::BTreeMap;

use super::tree::TREE_FORMAT_VERSION;
use super::{FileMode, KeyEpoch, ManifestEntry, ManifestError, WorkspacePath};

/// Keeps a subtree hash from colliding with any other keyed BLAKE3 in the
/// engine even on identical bytes.
const SUBTREE_DOMAIN: &[u8] = b"bowline/manifest-subtree/v1";

/// A workspace-relative DIRECTORY path. The root is the empty string, which is
/// deliberately not a legal [`WorkspacePath`] — the two namespaces must not be
/// interchangeable at a call boundary.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct DirPath(String);

impl DirPath {
    pub fn root() -> Self {
        Self(String::new())
    }

    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    /// The directory or entry path one component below this one.
    pub fn child(&self, name: &str) -> Self {
        if self.0.is_empty() {
            Self(name.to_string())
        } else {
            Self(format!("{}/{name}", self.0))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// This directory read as a manifest entry path. Never called on the root,
    /// which names no entry.
    pub fn to_workspace_path(&self) -> WorkspacePath {
        WorkspacePath::new(self.0.clone())
    }
}

/// The local content hash of one directory's whole subtree. Never crosses the
/// wire: it is the ledger key that lets a push learn a node's physical key
/// without resealing the node.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SubtreeHash(String);

impl SubtreeHash {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What a directory holds directly under one name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChildSpec {
    /// A file or symlink — a manifest entry that owns no subtree. Directories
    /// never appear here; [`DirectoryTree::decompose`] routes them to
    /// [`ChildSpec::Directory`].
    Leaf(ManifestEntry),
    /// A directory the manifest carries an entry for, with its own mode.
    Directory { mode: FileMode },
    /// A directory implied only by its descendants. It owns no manifest entry,
    /// so flattening the tree must not produce one.
    Subtree,
}

/// Every directory a flat manifest implies, with its direct children.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryTree {
    nodes: BTreeMap<DirPath, BTreeMap<String, ChildSpec>>,
}

impl DirectoryTree {
    /// Decompose a flat manifest. The root always exists, even for an empty
    /// manifest: a workspace with no entries still publishes a root node, which
    /// is what lets "everything was deleted" be a legitimate, verifiable head
    /// rather than an absent ref.
    pub fn decompose(
        entries: &BTreeMap<WorkspacePath, ManifestEntry>,
    ) -> Result<Self, ManifestError> {
        let mut nodes: BTreeMap<DirPath, BTreeMap<String, ChildSpec>> = BTreeMap::new();
        nodes.insert(DirPath::root(), BTreeMap::new());
        for (path, entry) in entries {
            insert_path(&mut nodes, path, entry)?;
        }
        Ok(Self { nodes })
    }

    pub fn children(&self, dir: &DirPath) -> Option<&BTreeMap<String, ChildSpec>> {
        self.nodes.get(dir)
    }

    pub fn directories(&self) -> impl Iterator<Item = &DirPath> {
        self.nodes.keys()
    }

    /// One BLAKE3 pass giving every directory a hash of its entire subtree.
    ///
    /// Children sort strictly after their parent (a parent path is a proper
    /// prefix), so reverse iteration visits every child before its parent and a
    /// single pass suffices.
    pub fn subtree_hashes(
        &self,
        key_epoch: KeyEpoch,
    ) -> Result<BTreeMap<DirPath, SubtreeHash>, ManifestError> {
        let mut hashes: BTreeMap<DirPath, SubtreeHash> = BTreeMap::new();
        for (dir, children) in self.nodes.iter().rev() {
            let hash = hash_directory(dir, children, key_epoch, &hashes)?;
            hashes.insert(dir.clone(), hash);
        }
        Ok(hashes)
    }
}

fn insert_path(
    nodes: &mut BTreeMap<DirPath, BTreeMap<String, ChildSpec>>,
    path: &WorkspacePath,
    entry: &ManifestEntry,
) -> Result<(), ManifestError> {
    let mut parent = DirPath::root();
    let mut components = path.as_str().split('/').peekable();
    while let Some(component) = components.next() {
        if component.is_empty() {
            return Err(ManifestError::InvalidPath {
                reason: "path has an empty component",
            });
        }
        if components.peek().is_some() {
            claim_child(nodes, &parent, component, ChildSpec::Subtree)?;
            parent = parent.child(component);
            nodes.entry(parent.clone()).or_default();
            continue;
        }
        let spec = match entry {
            ManifestEntry::Directory { mode } => ChildSpec::Directory { mode: *mode },
            file_or_symlink => ChildSpec::Leaf(file_or_symlink.clone()),
        };
        let owns_subtree = matches!(spec, ChildSpec::Directory { .. });
        claim_child(nodes, &parent, component, spec)?;
        if owns_subtree {
            nodes.entry(parent.child(component)).or_default();
        }
    }
    Ok(())
}

/// Reconcile a claim on one name against whatever already occupies it.
///
/// A directory entry and the structural subtree its descendants imply are the
/// same node seen twice, so the entry (which carries a real mode) always wins.
/// A name claimed as both a leaf and a directory is a manifest that cannot exist
/// on any filesystem; it is rejected rather than resolved, because either
/// resolution silently discards a published entry.
fn claim_child(
    nodes: &mut BTreeMap<DirPath, BTreeMap<String, ChildSpec>>,
    parent: &DirPath,
    name: &str,
    incoming: ChildSpec,
) -> Result<(), ManifestError> {
    let children = nodes.entry(parent.clone()).or_default();
    match (children.get(name), &incoming) {
        (None, _) => {
            children.insert(name.to_string(), incoming);
        }
        (Some(ChildSpec::Subtree), ChildSpec::Directory { .. }) => {
            children.insert(name.to_string(), incoming);
        }
        (Some(ChildSpec::Directory { .. }), ChildSpec::Subtree)
        | (Some(ChildSpec::Subtree), ChildSpec::Subtree) => {}
        _ => {
            return Err(ManifestError::InvalidEntry {
                reason: "path component is claimed as both a file and a directory",
            });
        }
    }
    Ok(())
}

fn hash_directory(
    dir: &DirPath,
    children: &BTreeMap<String, ChildSpec>,
    key_epoch: KeyEpoch,
    hashes: &BTreeMap<DirPath, SubtreeHash>,
) -> Result<SubtreeHash, ManifestError> {
    let mut hasher = blake3::Hasher::new();
    write_bytes(&mut hasher, SUBTREE_DOMAIN);
    hasher.update(&u64::from(TREE_FORMAT_VERSION).to_le_bytes());
    hasher.update(&u64::from(key_epoch.get()).to_le_bytes());
    hasher.update(&(children.len() as u64).to_le_bytes());
    for (name, spec) in children {
        write_bytes(&mut hasher, name.as_bytes());
        write_child(&mut hasher, dir, name, spec, hashes)?;
    }
    Ok(SubtreeHash::new(format!(
        "st_{}",
        hasher.finalize().to_hex()
    )))
}

fn write_child(
    hasher: &mut blake3::Hasher,
    dir: &DirPath,
    name: &str,
    spec: &ChildSpec,
    hashes: &BTreeMap<DirPath, SubtreeHash>,
) -> Result<(), ManifestError> {
    match spec {
        ChildSpec::Leaf(ManifestEntry::File {
            size,
            mode,
            content_id,
            blob_key,
            key_epoch,
        }) => {
            hasher.update(&[0]);
            hasher.update(&size.to_le_bytes());
            hasher.update(&u64::from(mode.get()).to_le_bytes());
            write_bytes(hasher, content_id.as_str().as_bytes());
            write_bytes(hasher, blob_key.as_str().as_bytes());
            hasher.update(&u64::from(key_epoch.get()).to_le_bytes());
        }
        ChildSpec::Leaf(ManifestEntry::Symlink { mode, target }) => {
            hasher.update(&[1]);
            hasher.update(&u64::from(mode.get()).to_le_bytes());
            write_bytes(hasher, target.as_bytes());
        }
        ChildSpec::Directory { mode } => {
            hasher.update(&[2]);
            hasher.update(&u64::from(mode.get()).to_le_bytes());
            write_bytes(hasher, child_hash(dir, name, hashes)?.as_str().as_bytes());
        }
        ChildSpec::Subtree => {
            hasher.update(&[3]);
            write_bytes(hasher, child_hash(dir, name, hashes)?.as_str().as_bytes());
        }
        // `decompose` routes every directory entry to `ChildSpec::Directory`, so
        // this pairing cannot be constructed. Refuse rather than invent an
        // encoding for it: a wrong subtree hash would make the node ledger lie.
        ChildSpec::Leaf(ManifestEntry::Directory { .. }) => {
            return Err(ManifestError::Internal {
                reason: "directory entry occupies a leaf slot",
            });
        }
    }
    Ok(())
}

fn child_hash<'a>(
    dir: &DirPath,
    name: &str,
    hashes: &'a BTreeMap<DirPath, SubtreeHash>,
) -> Result<&'a SubtreeHash, ManifestError> {
    hashes.get(&dir.child(name)).ok_or(ManifestError::Internal {
        reason: "child subtree hash was not computed before its parent",
    })
}

/// Length-prefix every variable-length field so no content can forge a field
/// boundary and two different subtrees hash alike.
fn write_bytes(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}
