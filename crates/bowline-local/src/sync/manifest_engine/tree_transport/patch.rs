//! Copy-on-write publication of a grouped flat-entry delta over an existing tree.
//!
//! All affected nodes are opened at most once, mutations are applied in the
//! established removal-then-replacement order, and final nodes are emitted
//! bottom-up once. This keeps remote work proportional to affected tree nodes
//! and ancestors rather than changed paths multiplied by tree depth.

use std::cell::RefCell;
use std::collections::BTreeMap;

use bowline_core::ids::ContentId;

use super::super::counters::EngineCounters;
use super::super::manifest::tree::{TreeEntry, TreeEntryPayload, TreeNode};
use super::super::manifest::{
    BlobKey, DecodeLimits, FileMode, KeyEpoch, ManifestEntry, ManifestError, ManifestKey,
    WorkspaceCrypto, WorkspacePath, open_tree_node, physical_manifest_key, seal_tree_node,
};
use super::super::push::{ManifestBatchUpload, RemoteObjects};
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
        pending_uploads: RefCell::new(Vec::new()),
    };
    let mut root = EditableNode::open(request.root, &patcher)?;
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
    for (path, _) in removals {
        root.apply_path(path_components(path), None, &patcher)?;
    }
    let mut replacements = request
        .changes
        .iter()
        .filter_map(|(path, entry)| entry.as_ref().map(|entry| (path, entry)))
        .collect::<Vec<_>>();
    replacements.sort_by(|(left, _), (right, _)| {
        path_depth(left)
            .cmp(&path_depth(right))
            .then_with(|| left.cmp(right))
    });
    for (path, entry) in replacements {
        root.apply_path(path_components(path), Some(entry), &patcher)?;
    }
    let root_key = root.persist(&patcher)?;
    patcher.commit_pending()?;
    Ok(root_key)
}

fn path_components(path: &WorkspacePath) -> Vec<&str> {
    path.as_str().split('/').collect()
}

fn path_depth(path: &WorkspacePath) -> usize {
    path.as_str().split('/').count()
}

struct TreePatcher<'a, O: RemoteObjects> {
    objects: &'a O,
    crypto: &'a WorkspaceCrypto,
    counters: &'a EngineCounters,
    limits: DecodeLimits,
    pending_uploads: RefCell<Vec<ManifestBatchUpload>>,
}

struct EditableNode {
    original_key: Option<ManifestKey>,
    format_version: u32,
    key_epoch: KeyEpoch,
    entries: BTreeMap<String, EditablePayload>,
    dirty: bool,
}

enum EditablePayload {
    File {
        size: u64,
        mode: FileMode,
        content_id: ContentId,
        blob_key: BlobKey,
        key_epoch: KeyEpoch,
    },
    Directory {
        mode: FileMode,
        child: EditableChild,
    },
    Subtree {
        child: EditableChild,
    },
    Symlink {
        mode: FileMode,
        target: String,
    },
}

enum EditableChild {
    Stored(ManifestKey),
    Loaded(Box<EditableNode>),
}

impl EditableNode {
    fn open<O: RemoteObjects>(
        key: &ManifestKey,
        patcher: &TreePatcher<'_, O>,
    ) -> Result<Self, TreeError> {
        let node = patcher.open_node(key)?;
        Ok(Self::from_tree_node(node, Some(key.clone())))
    }

    fn empty(crypto: &WorkspaceCrypto) -> Self {
        Self::from_tree_node(TreeNode::new(crypto.key_epoch(), Vec::new()), None)
    }

    fn from_tree_node(node: TreeNode, original_key: Option<ManifestKey>) -> Self {
        let entries = node
            .entries
            .into_iter()
            .map(|entry| (entry.name, EditablePayload::from_tree(entry.payload)))
            .collect();
        Self {
            original_key,
            format_version: node.format_version,
            key_epoch: node.key_epoch,
            entries,
            dirty: false,
        }
    }

    fn apply_path<O: RemoteObjects>(
        &mut self,
        components: Vec<&str>,
        replacement: Option<&ManifestEntry>,
        patcher: &TreePatcher<'_, O>,
    ) -> Result<bool, TreeError> {
        let (name, rest) =
            components
                .split_first()
                .ok_or(TreeError::Manifest(ManifestError::Internal {
                    reason: "tree patch received an empty path",
                }))?;
        if rest.is_empty() {
            return Ok(self.apply_leaf(name, replacement, patcher));
        }
        self.apply_descendant(name, rest, replacement, patcher)
    }

    fn apply_leaf<O: RemoteObjects>(
        &mut self,
        name: &str,
        replacement: Option<&ManifestEntry>,
        patcher: &TreePatcher<'_, O>,
    ) -> bool {
        match replacement {
            None => {
                let changed = self.entries.remove(name).is_some();
                self.dirty |= changed;
                changed
            }
            Some(entry) => {
                let existing = self.entries.remove(name);
                let (payload, changed) = EditablePayload::replacement(entry, existing, patcher);
                self.entries.insert(name.to_string(), payload);
                self.dirty |= changed;
                changed
            }
        }
    }

    fn apply_descendant<O: RemoteObjects>(
        &mut self,
        name: &str,
        rest: &[&str],
        replacement: Option<&ManifestEntry>,
        patcher: &TreePatcher<'_, O>,
    ) -> Result<bool, TreeError> {
        if !self.entries.contains_key(name) {
            if replacement.is_none() {
                return Ok(false);
            }
            self.entries.insert(
                name.to_string(),
                EditablePayload::Subtree {
                    child: EditableChild::Loaded(Box::new(Self::empty(patcher.crypto))),
                },
            );
            self.dirty = true;
        }
        let payload =
            self.entries
                .get_mut(name)
                .ok_or(TreeError::Manifest(ManifestError::Internal {
                    reason: "tree patch lost an inserted ancestor",
                }))?;
        let is_implicit = matches!(payload, EditablePayload::Subtree { .. });
        let child = match payload.child_mut(patcher)? {
            Some(child) => child,
            None if replacement.is_none() => return Ok(false),
            None => {
                return Err(TreeError::Manifest(ManifestError::Internal {
                    reason: "tree patch descends through a leaf",
                }));
            }
        };
        let changed = child.apply_path(rest.to_vec(), replacement, patcher)?;
        let prune = changed && is_implicit && child.entries.is_empty();
        if prune {
            self.entries.remove(name);
        }
        self.dirty |= changed;
        Ok(changed)
    }

    fn persist<O: RemoteObjects>(
        &mut self,
        patcher: &TreePatcher<'_, O>,
    ) -> Result<ManifestKey, TreeError> {
        let mut child_changed = false;
        let mut entries = Vec::with_capacity(self.entries.len());
        for (name, payload) in &mut self.entries {
            let (payload, changed) = payload.persist(patcher)?;
            child_changed |= changed;
            entries.push(TreeEntry {
                name: name.clone(),
                payload,
            });
        }
        if !self.dirty
            && !child_changed
            && let Some(original_key) = &self.original_key
        {
            return Ok(original_key.clone());
        }
        patcher.upload_node(TreeNode {
            format_version: self.format_version,
            key_epoch: self.key_epoch,
            entries,
        })
    }
}

impl EditablePayload {
    fn from_tree(payload: TreeEntryPayload) -> Self {
        match payload {
            TreeEntryPayload::File {
                size,
                mode,
                content_id,
                blob_key,
                key_epoch,
            } => Self::File {
                size,
                mode,
                content_id,
                blob_key,
                key_epoch,
            },
            TreeEntryPayload::Directory { mode, child } => Self::Directory {
                mode,
                child: EditableChild::Stored(child),
            },
            TreeEntryPayload::Subtree { child } => Self::Subtree {
                child: EditableChild::Stored(child),
            },
            TreeEntryPayload::Symlink { mode, target } => Self::Symlink { mode, target },
        }
    }

    fn replacement<O: RemoteObjects>(
        entry: &ManifestEntry,
        existing: Option<Self>,
        patcher: &TreePatcher<'_, O>,
    ) -> (Self, bool) {
        match entry {
            ManifestEntry::File {
                size,
                mode,
                content_id,
                blob_key,
                key_epoch,
            } => {
                let unchanged = matches!(
                    &existing,
                    Some(Self::File {
                        size: old_size,
                        mode: old_mode,
                        content_id: old_content_id,
                        blob_key: old_blob_key,
                        key_epoch: old_key_epoch,
                    }) if old_size == size
                        && old_mode == mode
                        && old_content_id == content_id
                        && old_blob_key == blob_key
                        && old_key_epoch == key_epoch
                );
                (
                    Self::File {
                        size: *size,
                        mode: *mode,
                        content_id: content_id.clone(),
                        blob_key: blob_key.clone(),
                        key_epoch: *key_epoch,
                    },
                    !unchanged,
                )
            }
            ManifestEntry::Symlink { mode, target } => {
                let unchanged = matches!(
                    &existing,
                    Some(Self::Symlink { mode: old_mode, target: old_target })
                        if old_mode == mode && old_target == target
                );
                (
                    Self::Symlink {
                        mode: *mode,
                        target: target.clone(),
                    },
                    !unchanged,
                )
            }
            ManifestEntry::Directory { mode } => match existing {
                Some(Self::Directory {
                    mode: old_mode,
                    child,
                }) => (Self::Directory { mode: *mode, child }, old_mode != *mode),
                Some(Self::Subtree { child }) => (Self::Directory { mode: *mode, child }, true),
                Some(Self::File { .. } | Self::Symlink { .. }) | None => (
                    Self::Directory {
                        mode: *mode,
                        child: EditableChild::Loaded(Box::new(EditableNode::empty(patcher.crypto))),
                    },
                    true,
                ),
            },
        }
    }

    fn child_mut<O: RemoteObjects>(
        &mut self,
        patcher: &TreePatcher<'_, O>,
    ) -> Result<Option<&mut EditableNode>, TreeError> {
        let child = match self {
            Self::Directory { child, .. } | Self::Subtree { child } => child,
            Self::File { .. } | Self::Symlink { .. } => return Ok(None),
        };
        child.load(patcher).map(Some)
    }

    fn persist<O: RemoteObjects>(
        &mut self,
        patcher: &TreePatcher<'_, O>,
    ) -> Result<(TreeEntryPayload, bool), TreeError> {
        match self {
            Self::File {
                size,
                mode,
                content_id,
                blob_key,
                key_epoch,
            } => Ok((
                TreeEntryPayload::File {
                    size: *size,
                    mode: *mode,
                    content_id: content_id.clone(),
                    blob_key: blob_key.clone(),
                    key_epoch: *key_epoch,
                },
                false,
            )),
            Self::Directory { mode, child } => {
                let (child, changed) = child.persist(patcher)?;
                Ok((TreeEntryPayload::Directory { mode: *mode, child }, changed))
            }
            Self::Subtree { child } => {
                let (child, changed) = child.persist(patcher)?;
                Ok((TreeEntryPayload::Subtree { child }, changed))
            }
            Self::Symlink { mode, target } => Ok((
                TreeEntryPayload::Symlink {
                    mode: *mode,
                    target: target.clone(),
                },
                false,
            )),
        }
    }
}

impl EditableChild {
    fn load<O: RemoteObjects>(
        &mut self,
        patcher: &TreePatcher<'_, O>,
    ) -> Result<&mut EditableNode, TreeError> {
        if let Self::Stored(key) = self {
            *self = Self::Loaded(Box::new(EditableNode::open(key, patcher)?));
        }
        match self {
            Self::Loaded(node) => Ok(node),
            Self::Stored(_) => Err(TreeError::Manifest(ManifestError::Internal {
                reason: "tree patch failed to load a stored child",
            })),
        }
    }

    fn persist<O: RemoteObjects>(
        &mut self,
        patcher: &TreePatcher<'_, O>,
    ) -> Result<(ManifestKey, bool), TreeError> {
        match self {
            Self::Stored(key) => Ok((key.clone(), false)),
            Self::Loaded(node) => {
                let original = node.original_key.clone();
                let key = node.persist(patcher)?;
                Ok((key.clone(), original.as_ref() != Some(&key)))
            }
        }
    }
}

impl<O: RemoteObjects> TreePatcher<'_, O> {
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
        self.pending_uploads.borrow_mut().push(ManifestBatchUpload {
            key: key.clone(),
            content_id,
            key_epoch: self.crypto.key_epoch(),
            sealed: sealed.into_bytes(),
        });
        Ok(key)
    }

    fn commit_pending(&self) -> Result<(), TreeError> {
        let uploads = self.pending_uploads.borrow();
        self.objects
            .put_manifests(&uploads)
            .map_err(TreeError::Transport)?;
        for upload in uploads.iter() {
            self.counters
                .record_manifest_upload(upload.sealed.len() as u64);
        }
        Ok(())
    }
}
