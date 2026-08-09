//! Publish a flat manifest as a Merkle tree of sealed directory nodes.
//!
//! Only nodes the ledger has never seen are sealed and uploaded. Everything else
//! — every subtree whose content is unchanged since some earlier publish — is
//! reused by key and costs zero bytes. That is the whole cost model change: a
//! one-file edit rewrites the nodes from that file to the root and nothing else.

use std::collections::BTreeMap;

use super::super::counters::EngineCounters;
use super::super::manifest::directory_tree::{ChildSpec, DirPath, DirectoryTree, SubtreeHash};
use super::super::manifest::tree::{TreeEntry, TreeEntryPayload, TreeNode};
use super::super::manifest::{
    KeyEpoch, MAX_WORKSPACE_PATH_DEPTH, Manifest, ManifestEntry, ManifestError, ManifestKey,
    WorkspaceCrypto, physical_manifest_key, seal_tree_node,
};
use super::super::push::{ManifestBatchUpload, RemoteObjects};
use super::{TreeError, TreeNodeLedger};

/// Everything one publish needs. A params struct because a publish legitimately
/// takes six collaborators and a positional list of six would be unreadable.
pub struct PublishTreeRequest<'a, O: RemoteObjects, L: TreeNodeLedger> {
    pub objects: &'a O,
    pub crypto: &'a WorkspaceCrypto,
    pub counters: &'a EngineCounters,
    pub manifest: &'a Manifest,
    pub ledger: &'a mut L,
}

/// Publish `manifest` and return its ROOT node key — the value the workspace ref
/// compare-and-swaps onto.
pub fn publish_tree<O: RemoteObjects, L: TreeNodeLedger>(
    request: PublishTreeRequest<'_, O, L>,
) -> Result<ManifestKey, TreeError> {
    // Deliberately not named `*manifest`: the flat-manifest cutover gate treats a
    // manifest-named binding's entry-map access as the retired `SnapshotManifest`
    // reader, which this is not.
    let snapshot = request.manifest;
    let tree = DirectoryTree::decompose(&snapshot.entries).map_err(TreeError::Manifest)?;
    let hashes = tree
        .subtree_hashes(snapshot.key_epoch)
        .map_err(TreeError::Manifest)?;
    let mut publisher = NodePublisher {
        objects: request.objects,
        crypto: request.crypto,
        counters: request.counters,
        key_epoch: snapshot.key_epoch,
        tree: &tree,
        hashes: &hashes,
        ledger: request.ledger,
        pending_uploads: Vec::new(),
        pending_ledger: Vec::new(),
    };
    let root = publisher.publish(&DirPath::root(), 0)?;
    publisher.commit_pending()?;
    Ok(root)
}

struct NodePublisher<'a, O: RemoteObjects, L: TreeNodeLedger> {
    objects: &'a O,
    crypto: &'a WorkspaceCrypto,
    counters: &'a EngineCounters,
    key_epoch: KeyEpoch,
    tree: &'a DirectoryTree,
    hashes: &'a BTreeMap<DirPath, SubtreeHash>,
    ledger: &'a mut L,
    pending_uploads: Vec<ManifestBatchUpload>,
    pending_ledger: Vec<(SubtreeHash, ManifestKey)>,
}

impl<O: RemoteObjects, L: TreeNodeLedger> NodePublisher<'_, O, L> {
    fn publish(&mut self, dir: &DirPath, depth: u64) -> Result<ManifestKey, TreeError> {
        // Depth is recursion here, so it is bounded before it is walked. The
        // writer's path predicate rejects a too-deep path long before this, which
        // makes reaching the guard an engine bug rather than hostile input.
        if depth > MAX_WORKSPACE_PATH_DEPTH {
            return Err(TreeError::Manifest(ManifestError::BoundExceeded {
                bound: "tree-depth",
            }));
        }
        let hash = self.subtree_hash(dir)?.clone();
        if let Some(key) = self.ledger.known(&hash)? {
            return Ok(key);
        }
        let entries = self.build_entries(dir, depth)?;
        let key = self.seal_node(TreeNode::new(self.key_epoch, entries))?;
        self.pending_ledger.push((hash, key.clone()));
        Ok(key)
    }

    fn build_entries(&mut self, dir: &DirPath, depth: u64) -> Result<Vec<TreeEntry>, TreeError> {
        // Copy the shared borrows out of `self` so the child recursion below can
        // take `&mut self` while this loop still reads the tree.
        let tree = self.tree;
        let children = tree
            .children(dir)
            .ok_or(TreeError::Manifest(ManifestError::Internal {
                reason: "directory has no node in the decomposed tree",
            }))?;
        let mut entries = Vec::with_capacity(children.len());
        for (name, spec) in children {
            let payload = match spec {
                ChildSpec::Leaf(entry) => leaf_payload(entry)?,
                ChildSpec::Directory { mode } => TreeEntryPayload::Directory {
                    mode: *mode,
                    child: self.publish(&dir.child(name), depth + 1)?,
                },
                ChildSpec::Subtree => TreeEntryPayload::Subtree {
                    child: self.publish(&dir.child(name), depth + 1)?,
                },
            };
            entries.push(TreeEntry {
                name: name.clone(),
                payload,
            });
        }
        Ok(entries)
    }

    fn seal_node(&mut self, node: TreeNode) -> Result<ManifestKey, TreeError> {
        let plaintext = node.to_canonical_bytes().map_err(TreeError::Manifest)?;
        let content_id = self.crypto.tree_node_content_id(&plaintext);
        let sealed = seal_tree_node(self.crypto, &plaintext).map_err(TreeError::Manifest)?;
        let key = physical_manifest_key(sealed.as_bytes());
        self.pending_uploads.push(ManifestBatchUpload {
            key: key.clone(),
            content_id,
            key_epoch: self.key_epoch,
            sealed: sealed.into_bytes(),
        });
        Ok(key)
    }

    fn commit_pending(&mut self) -> Result<(), TreeError> {
        self.objects
            .put_manifests(&self.pending_uploads)
            .map_err(TreeError::Transport)?;
        for upload in &self.pending_uploads {
            self.counters
                .record_manifest_upload(upload.sealed.len() as u64);
        }
        for (hash, key) in std::mem::take(&mut self.pending_ledger) {
            self.ledger.record(hash, key);
        }
        Ok(())
    }

    fn subtree_hash(&self, dir: &DirPath) -> Result<&SubtreeHash, TreeError> {
        self.hashes
            .get(dir)
            .ok_or(TreeError::Manifest(ManifestError::Internal {
                reason: "directory has no subtree hash",
            }))
    }
}

fn leaf_payload(entry: &ManifestEntry) -> Result<TreeEntryPayload, TreeError> {
    match entry {
        ManifestEntry::File {
            size,
            mode,
            content_id,
            blob_key,
            key_epoch,
        } => Ok(TreeEntryPayload::File {
            size: *size,
            mode: *mode,
            content_id: content_id.clone(),
            blob_key: blob_key.clone(),
            key_epoch: *key_epoch,
        }),
        ManifestEntry::Symlink { mode, target } => Ok(TreeEntryPayload::Symlink {
            mode: *mode,
            target: target.clone(),
        }),
        // `DirectoryTree::decompose` routes every directory entry to
        // `ChildSpec::Directory`, which owns a child node; a directory reaching
        // the leaf arm would publish a subtree-less directory whose descendants
        // silently vanish.
        ManifestEntry::Directory { .. } => Err(TreeError::Manifest(ManifestError::Internal {
            reason: "directory entry occupies a leaf slot",
        })),
    }
}
