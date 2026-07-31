//! Copy-on-write publication of a small flat-entry delta over an existing tree.
//!
//! The ordinary publisher accepts a complete flat manifest and is the right
//! bootstrap path. Once a root exists, rebuilding that flat map defeats the
//! Merkle format: one edit should read and rewrite only its ancestor nodes.

use std::collections::BTreeMap;

use super::super::counters::EngineCounters;
use super::super::manifest::tree::{TreeEntry, TreeEntryPayload, TreeNode};
use super::super::manifest::{
    DecodeLimits, ManifestEntry, ManifestError, ManifestKey, WorkspaceCrypto, WorkspacePath,
    open_tree_node, physical_manifest_key, seal_tree_node,
};
use super::super::push::{ManifestUpload, RemoteObjects};
use super::TreeError;

pub struct PatchTreeRequest<'a, O: RemoteObjects> {
    pub objects: &'a O,
    pub crypto: &'a WorkspaceCrypto,
    pub counters: &'a EngineCounters,
    pub root: &'a ManifestKey,
    pub changes: &'a BTreeMap<WorkspacePath, Option<ManifestEntry>>,
}

pub fn patch_tree<O: RemoteObjects>(
    request: PatchTreeRequest<'_, O>,
) -> Result<ManifestKey, TreeError> {
    let patcher = TreePatcher {
        objects: request.objects,
        crypto: request.crypto,
        counters: request.counters,
        limits: DecodeLimits::default(),
    };
    let mut root = request.root.clone();
    let mut removals = request
        .changes
        .iter()
        .filter(|(_, entry)| entry.is_none())
        .collect::<Vec<_>>();
    removals.sort_by(|(left, _), (right, _)| {
        path_depth(right)
            .cmp(&path_depth(left))
            .then_with(|| left.cmp(right))
    });
    for (path, entry) in removals {
        let components = path.as_str().split('/').collect::<Vec<_>>();
        root = patcher.patch_node(&root, &components, entry.as_ref())?.0;
    }
    let mut replacements = request
        .changes
        .iter()
        .filter(|(_, entry)| entry.is_some())
        .collect::<Vec<_>>();
    replacements.sort_by(|(left, _), (right, _)| {
        path_depth(left)
            .cmp(&path_depth(right))
            .then_with(|| left.cmp(right))
    });
    for (path, entry) in replacements {
        let components = path.as_str().split('/').collect::<Vec<_>>();
        root = patcher.patch_node(&root, &components, entry.as_ref())?.0;
    }
    Ok(root)
}

fn path_depth(path: &WorkspacePath) -> usize {
    path.as_str().split('/').count()
}

struct TreePatcher<'a, O: RemoteObjects> {
    objects: &'a O,
    crypto: &'a WorkspaceCrypto,
    counters: &'a EngineCounters,
    limits: DecodeLimits,
}

impl<O: RemoteObjects> TreePatcher<'_, O> {
    fn patch_node(
        &self,
        key: &ManifestKey,
        components: &[&str],
        replacement: Option<&ManifestEntry>,
    ) -> Result<(ManifestKey, bool, bool), TreeError> {
        let mut node = self.open_node(key)?;
        let name = components
            .first()
            .ok_or(TreeError::Manifest(ManifestError::Internal {
                reason: "tree patch received an empty path",
            }))?;
        let changed = if components.len() == 1 {
            self.patch_leaf(&mut node, name, replacement)?
        } else {
            self.patch_descendant(&mut node, name, &components[1..], replacement)?
        };
        let empty = node.entries.is_empty();
        if changed {
            Ok((self.upload_node(node)?, empty, true))
        } else {
            Ok((key.clone(), empty, false))
        }
    }

    fn patch_leaf(
        &self,
        node: &mut TreeNode,
        name: &str,
        replacement: Option<&ManifestEntry>,
    ) -> Result<bool, TreeError> {
        let position = node
            .entries
            .binary_search_by(|entry| entry.name.as_str().cmp(name));
        match replacement {
            None => {
                if let Ok(index) = position {
                    node.entries.remove(index);
                    return Ok(true);
                }
                Ok(false)
            }
            Some(entry) => {
                let existing = position
                    .ok()
                    .and_then(|index| node.entries.get(index))
                    .map(|entry| &entry.payload);
                let payload = self.entry_payload(entry, existing)?;
                if existing == Some(&payload) {
                    return Ok(false);
                }
                let tree_entry = TreeEntry {
                    name: name.to_string(),
                    payload,
                };
                match position {
                    Ok(index) => node.entries[index] = tree_entry,
                    Err(index) => node.entries.insert(index, tree_entry),
                }
                Ok(true)
            }
        }
    }

    fn patch_descendant(
        &self,
        node: &mut TreeNode,
        name: &str,
        rest: &[&str],
        replacement: Option<&ManifestEntry>,
    ) -> Result<bool, TreeError> {
        let position = node
            .entries
            .binary_search_by(|entry| entry.name.as_str().cmp(name));
        let (child_key, directory_mode) = match position {
            Ok(index) => match &node.entries[index].payload {
                TreeEntryPayload::Directory { mode, child } => (child.clone(), Some(*mode)),
                TreeEntryPayload::Subtree { child } => (child.clone(), None),
                TreeEntryPayload::File { .. } | TreeEntryPayload::Symlink { .. } => {
                    if replacement.is_none() {
                        return Ok(false);
                    }
                    return Err(TreeError::Manifest(ManifestError::Internal {
                        reason: "tree patch descends through a leaf",
                    }));
                }
            },
            Err(_) if replacement.is_none() => return Ok(false),
            Err(_) => (
                self.upload_node(TreeNode::new(self.crypto.key_epoch(), Vec::new()))?,
                None,
            ),
        };
        let (patched_child, child_empty, child_changed) =
            self.patch_node(&child_key, rest, replacement)?;
        if !child_changed {
            return Ok(false);
        }
        if child_empty && directory_mode.is_none() {
            if let Ok(index) = position {
                node.entries.remove(index);
            }
            return Ok(true);
        }
        let payload = match directory_mode {
            Some(mode) => TreeEntryPayload::Directory {
                mode,
                child: patched_child,
            },
            None => TreeEntryPayload::Subtree {
                child: patched_child,
            },
        };
        let tree_entry = TreeEntry {
            name: name.to_string(),
            payload,
        };
        match position {
            Ok(index) => node.entries[index] = tree_entry,
            Err(index) => node.entries.insert(index, tree_entry),
        }
        Ok(true)
    }

    fn entry_payload(
        &self,
        entry: &ManifestEntry,
        existing: Option<&TreeEntryPayload>,
    ) -> Result<TreeEntryPayload, TreeError> {
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
            ManifestEntry::Directory { mode } => {
                let child = match existing {
                    Some(TreeEntryPayload::Directory { child, .. })
                    | Some(TreeEntryPayload::Subtree { child }) => child.clone(),
                    Some(TreeEntryPayload::File { .. } | TreeEntryPayload::Symlink { .. })
                    | None => {
                        self.upload_node(TreeNode::new(self.crypto.key_epoch(), Vec::new()))?
                    }
                };
                Ok(TreeEntryPayload::Directory { mode: *mode, child })
            }
        }
    }

    fn open_node(&self, key: &ManifestKey) -> Result<TreeNode, TreeError> {
        let sealed = self
            .objects
            .get_manifest(key)
            .map_err(TreeError::Transport)?;
        if &physical_manifest_key(&sealed) != key {
            return Err(TreeError::NodeKeyMismatch);
        }
        let (plaintext, epoch) =
            open_tree_node(self.crypto, &sealed, &self.limits).map_err(TreeError::Manifest)?;
        self.counters.record_manifest_download(sealed.len() as u64);
        TreeNode::decode(&plaintext, epoch, &self.limits).map_err(TreeError::Manifest)
    }

    fn upload_node(&self, node: TreeNode) -> Result<ManifestKey, TreeError> {
        let plaintext = node.to_canonical_bytes().map_err(TreeError::Manifest)?;
        let content_id = self.crypto.tree_node_content_id(&plaintext);
        let sealed = seal_tree_node(self.crypto, &plaintext).map_err(TreeError::Manifest)?;
        let key = physical_manifest_key(sealed.as_bytes());
        self.objects
            .put_manifest(ManifestUpload {
                key: &key,
                content_id: &content_id,
                key_epoch: self.crypto.key_epoch(),
                sealed: sealed.as_bytes(),
            })
            .map_err(TreeError::Transport)?;
        self.counters
            .record_manifest_upload(sealed.as_bytes().len() as u64);
        Ok(key)
    }
}
