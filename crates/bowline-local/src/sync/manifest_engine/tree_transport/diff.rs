//! Change-proportional comparison of two Merkle roots.
//!
//! Equal node keys prune whole subtrees without consulting SQLite. Differing
//! nodes are opened and compared directly, yielding only paths whose remote
//! value changed plus explicitly requested locally-dirty paths.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use super::super::counters::EngineCounters;
use super::super::endpoint::NameFolding;
use super::super::manifest::directory_tree::DirPath;
use super::super::manifest::tree::{TreeEntry, TreeEntryPayload, TreeNode};
use super::super::manifest::{
    DecodeLimits, ManifestEntry, ManifestKey, PathCollision, WorkspaceCrypto, WorkspacePath,
    open_tree_node, physical_manifest_key, validate_manifest_path,
};
use super::super::push::RemoteObjects;
use super::TreeError;

pub struct DiffTreeRequest<'a, O: RemoteObjects> {
    pub objects: &'a O,
    pub crypto: &'a WorkspaceCrypto,
    pub counters: &'a EngineCounters,
    pub old_root: &'a ManifestKey,
    pub new_root: &'a ManifestKey,
    pub dirty: &'a BTreeSet<WorkspacePath>,
    pub names: NameFolding,
    pub limits: &'a DecodeLimits,
}

pub struct TreeDelta {
    pub entries: BTreeMap<WorkspacePath, ManifestEntry>,
    pub touched: BTreeSet<WorkspacePath>,
    pub collisions: Vec<PathCollision>,
}

pub fn diff_tree<O: RemoteObjects>(
    request: DiffTreeRequest<'_, O>,
) -> Result<TreeDelta, TreeError> {
    let mut differ = TreeDiffer {
        objects: request.objects,
        crypto: request.crypto,
        counters: request.counters,
        names: request.names,
        limits: request.limits,
        entries: BTreeMap::new(),
        touched: BTreeSet::new(),
        folded: HashMap::new(),
        collision_entries: BTreeMap::new(),
        opened: HashMap::new(),
        records: 0,
        aggregate: 0,
        decoded: DecodedBudgets::default(),
    };
    differ.compare(&DirPath::root(), request.old_root, request.new_root, 0)?;
    for path in request.dirty {
        differ.resolve(request.new_root, path)?;
        differ.touched.insert(path.clone());
    }
    let collisions = collisions(differ.folded);
    for collision in &collisions {
        for path in &collision.paths {
            differ.touched.insert(path.clone());
            if let Some(entry) = differ.collision_entries.get(path) {
                differ.entries.insert(path.clone(), entry.clone());
            }
        }
    }
    Ok(TreeDelta {
        entries: differ.entries,
        touched: differ.touched,
        collisions,
    })
}

struct TreeDiffer<'a, O: RemoteObjects> {
    objects: &'a O,
    crypto: &'a WorkspaceCrypto,
    counters: &'a EngineCounters,
    names: NameFolding,
    limits: &'a DecodeLimits,
    entries: BTreeMap<WorkspacePath, ManifestEntry>,
    touched: BTreeSet<WorkspacePath>,
    folded: HashMap<String, Vec<WorkspacePath>>,
    collision_entries: BTreeMap<WorkspacePath, ManifestEntry>,
    opened: HashMap<ManifestKey, OpenedTreeNode>,
    records: u64,
    aggregate: u64,
    decoded: DecodedBudgets,
}

#[derive(Clone, Copy)]
enum TreeSide {
    Old,
    New,
}

#[derive(Clone)]
struct OpenedTreeNode {
    node: TreeNode,
    decoded_bytes: u64,
}

#[derive(Default)]
struct DecodedBudgets {
    old: u64,
    new: u64,
}

impl DecodedBudgets {
    fn charge(&mut self, side: TreeSide, bytes: u64, limit: u64) -> Result<(), TreeError> {
        let decoded = match side {
            TreeSide::Old => &mut self.old,
            TreeSide::New => &mut self.new,
        };
        *decoded = decoded.saturating_add(bytes);
        if *decoded > limit {
            return Err(bound("tree-aggregate-decoded-bytes"));
        }
        Ok(())
    }
}

impl<O: RemoteObjects> TreeDiffer<'_, O> {
    fn compare(
        &mut self,
        dir: &DirPath,
        old_key: &ManifestKey,
        new_key: &ManifestKey,
        depth: u64,
    ) -> Result<(), TreeError> {
        if old_key == new_key {
            return Ok(());
        }
        self.check_depth(depth)?;
        let old_node = self.open_node(old_key, TreeSide::Old, true)?;
        self.count_node(&old_node, false)?;
        let new_node = self.open_node(new_key, TreeSide::New, true)?;
        self.count_node(&new_node, true)?;
        let old = entries_by_name(old_node);
        let new = entries_by_name(new_node);
        self.record_collisions(dir, new.values())?;
        let names = old
            .keys()
            .chain(new.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        for name in names {
            let child = dir.child(&name);
            match (old.get(&name), new.get(&name)) {
                (Some(before), Some(after)) if before.payload == after.payload => {}
                (Some(before), Some(after)) => {
                    self.compare_entry(&child, before, after, depth + 1)?;
                }
                (Some(before), None) => self.remove_entry(&child, before, depth + 1)?,
                (None, Some(after)) => self.add_entry(&child, after, depth + 1)?,
                (None, None) => {}
            }
        }
        Ok(())
    }

    fn compare_entry(
        &mut self,
        path: &DirPath,
        before: &TreeEntry,
        after: &TreeEntry,
        depth: u64,
    ) -> Result<(), TreeError> {
        match (&before.payload, &after.payload) {
            (
                TreeEntryPayload::Directory {
                    mode: before_mode,
                    child: before_child,
                },
                TreeEntryPayload::Directory {
                    mode: after_mode,
                    child: after_child,
                },
            ) => {
                if before_mode != after_mode {
                    self.emit(path, ManifestEntry::Directory { mode: *after_mode })?;
                }
                self.compare(path, before_child, after_child, depth)
            }
            (
                TreeEntryPayload::Subtree {
                    child: before_child,
                },
                TreeEntryPayload::Subtree { child: after_child },
            ) => self.compare(path, before_child, after_child, depth),
            (
                TreeEntryPayload::Directory {
                    child: before_child,
                    ..
                },
                TreeEntryPayload::Subtree { child: after_child },
            ) => {
                self.touched.insert(path.to_workspace_path());
                self.compare(path, before_child, after_child, depth)
            }
            (
                TreeEntryPayload::Subtree {
                    child: before_child,
                },
                TreeEntryPayload::Directory {
                    mode,
                    child: after_child,
                },
            ) => {
                self.emit(path, ManifestEntry::Directory { mode: *mode })?;
                self.compare(path, before_child, after_child, depth)
            }
            _ => {
                self.remove_entry(path, before, depth)?;
                self.add_entry(path, after, depth)
            }
        }
    }

    fn add_entry(
        &mut self,
        path: &DirPath,
        entry: &TreeEntry,
        depth: u64,
    ) -> Result<(), TreeError> {
        if let Some(visible) = visible_entry(&entry.payload) {
            self.emit(path, visible)?;
        }
        if let Some(child) = entry.payload.child() {
            self.enumerate(path, child, depth, true)?;
        }
        Ok(())
    }

    fn remove_entry(
        &mut self,
        path: &DirPath,
        entry: &TreeEntry,
        depth: u64,
    ) -> Result<(), TreeError> {
        if visible_entry(&entry.payload).is_some() {
            self.touched.insert(path.to_workspace_path());
        }
        if let Some(child) = entry.payload.child() {
            self.enumerate(path, child, depth, false)?;
        }
        Ok(())
    }

    fn enumerate(
        &mut self,
        dir: &DirPath,
        key: &ManifestKey,
        depth: u64,
        adding: bool,
    ) -> Result<(), TreeError> {
        self.check_depth(depth)?;
        let side = if adding { TreeSide::New } else { TreeSide::Old };
        let node = self.open_node(key, side, true)?;
        self.count_node(&node, adding)?;
        if adding {
            self.record_collisions(dir, node.entries.iter())?;
        }
        for entry in node.entries {
            let child = dir.child(&entry.name);
            if adding {
                if let Some(visible) = visible_entry(&entry.payload) {
                    self.emit(&child, visible)?;
                }
            } else if visible_entry(&entry.payload).is_some() {
                self.touched.insert(child.to_workspace_path());
            }
            if let Some(node) = entry.payload.child() {
                self.enumerate(&child, node, depth + 1, adding)?;
            }
        }
        Ok(())
    }

    fn resolve(&mut self, root: &ManifestKey, path: &WorkspacePath) -> Result<(), TreeError> {
        let mut key = root.clone();
        let components = path.as_str().split('/').collect::<Vec<_>>();
        for (index, component) in components.iter().enumerate() {
            let node = self.open_node(&key, TreeSide::New, false)?;
            let Ok(position) = node
                .entries
                .binary_search_by(|entry| entry.name.as_str().cmp(component))
            else {
                return Ok(());
            };
            let entry = &node.entries[position];
            if index + 1 == components.len() {
                if let Some(visible) = visible_entry(&entry.payload) {
                    self.entries.insert(path.clone(), visible);
                }
                return Ok(());
            }
            let Some(child) = entry.payload.child() else {
                return Ok(());
            };
            key = child.clone();
        }
        Ok(())
    }

    fn emit(&mut self, path: &DirPath, entry: ManifestEntry) -> Result<(), TreeError> {
        validate_manifest_path(path.as_str(), self.limits).map_err(TreeError::Manifest)?;
        let path = path.to_workspace_path();
        self.touched.insert(path.clone());
        self.entries.insert(path, entry);
        Ok(())
    }

    fn record_collisions<'a>(
        &mut self,
        dir: &DirPath,
        entries: impl Iterator<Item = &'a TreeEntry>,
    ) -> Result<(), TreeError> {
        for entry in entries {
            let path = dir.child(&entry.name);
            validate_manifest_path(path.as_str(), self.limits).map_err(TreeError::Manifest)?;
            let path = path.to_workspace_path();
            self.folded
                .entry(self.names.fold(path.as_str()))
                .or_default()
                .push(path.clone());
            if let Some(visible) = visible_entry(&entry.payload) {
                self.collision_entries.entry(path).or_insert(visible);
            }
        }
        Ok(())
    }

    fn open_node(
        &mut self,
        key: &ManifestKey,
        side: TreeSide,
        charge_cached: bool,
    ) -> Result<TreeNode, TreeError> {
        if let Some(opened) = self.opened.get(key).cloned() {
            if charge_cached {
                self.decoded.charge(
                    side,
                    opened.decoded_bytes,
                    self.limits.max_aggregate_decoded_bytes,
                )?;
            }
            return Ok(opened.node);
        }
        let sealed = self
            .objects
            .get_manifest(key)
            .map_err(TreeError::Transport)?;
        if &physical_manifest_key(&sealed) != key {
            return Err(TreeError::NodeKeyMismatch);
        }
        let (plaintext, epoch) =
            open_tree_node(self.crypto, &sealed, self.limits).map_err(TreeError::Manifest)?;
        self.decoded.charge(
            side,
            plaintext.len() as u64,
            self.limits.max_aggregate_decoded_bytes,
        )?;
        self.counters.record_manifest_download(sealed.len() as u64);
        let node = TreeNode::decode(&plaintext, epoch, self.limits).map_err(TreeError::Manifest)?;
        self.opened.insert(
            key.clone(),
            OpenedTreeNode {
                node: node.clone(),
                decoded_bytes: plaintext.len() as u64,
            },
        );
        Ok(node)
    }

    fn count_node(&mut self, node: &TreeNode, new_tree: bool) -> Result<(), TreeError> {
        self.records = self.records.saturating_add(node.entries.len() as u64);
        // A diff may legitimately visit one complete valid old tree and one
        // complete valid new tree. Bound both sides without rejecting that pair.
        if self.records > self.limits.max_records.saturating_mul(2) {
            return Err(bound("record-count"));
        }
        if new_tree {
            for entry in &node.entries {
                let TreeEntryPayload::File { size, .. } = &entry.payload else {
                    continue;
                };
                self.aggregate = self
                    .aggregate
                    .checked_add(*size)
                    .filter(|total| *total <= self.limits.max_aggregate_declared_bytes)
                    .ok_or_else(|| bound("aggregate-declared-size"))?;
            }
        }
        Ok(())
    }

    fn check_depth(&self, depth: u64) -> Result<(), TreeError> {
        if depth > self.limits.max_depth {
            return Err(TreeError::Manifest(
                super::super::manifest::ManifestError::BoundExceeded {
                    bound: "tree-depth",
                },
            ));
        }
        Ok(())
    }
}

fn bound(bound: &'static str) -> TreeError {
    TreeError::Manifest(super::super::manifest::ManifestError::BoundExceeded { bound })
}

fn entries_by_name(node: TreeNode) -> BTreeMap<String, TreeEntry> {
    node.entries
        .into_iter()
        .map(|entry| (entry.name.clone(), entry))
        .collect()
}

fn visible_entry(payload: &TreeEntryPayload) -> Option<ManifestEntry> {
    match payload {
        TreeEntryPayload::File {
            size,
            mode,
            content_id,
            blob_key,
            key_epoch,
        } => Some(ManifestEntry::File {
            size: *size,
            mode: *mode,
            content_id: content_id.clone(),
            blob_key: blob_key.clone(),
            key_epoch: *key_epoch,
        }),
        TreeEntryPayload::Directory { mode, .. } => Some(ManifestEntry::Directory { mode: *mode }),
        TreeEntryPayload::Symlink { mode, target } => Some(ManifestEntry::Symlink {
            mode: *mode,
            target: target.clone(),
        }),
        TreeEntryPayload::Subtree { .. } => None,
    }
}

fn collisions(folded: HashMap<String, Vec<WorkspacePath>>) -> Vec<PathCollision> {
    let mut collisions = folded
        .into_iter()
        .filter(|(_, paths)| paths.len() > 1)
        .map(|(folded, mut paths)| {
            paths.sort();
            paths.dedup();
            PathCollision { folded, paths }
        })
        .filter(|collision| collision.paths.len() > 1)
        .collect::<Vec<_>>();
    collisions.sort_by(|left, right| left.folded.cmp(&right.folded));
    collisions
}

#[cfg(test)]
mod tests {
    use super::{DecodedBudgets, TreeSide};

    #[test]
    fn decoded_budget_is_enforced_independently_per_diff_tree() {
        let mut budgets = DecodedBudgets::default();

        budgets.charge(TreeSide::Old, 256, 256).unwrap();
        budgets.charge(TreeSide::New, 256, 256).unwrap();
        assert_eq!(budgets.old, 256);
        assert_eq!(budgets.new, 256);

        assert!(budgets.charge(TreeSide::New, 1, 256).is_err());
    }

    #[test]
    fn decoded_budget_charges_repeated_subtree_occurrences() {
        let mut budgets = DecodedBudgets::default();

        budgets.charge(TreeSide::New, 128, 256).unwrap();
        budgets.charge(TreeSide::New, 128, 256).unwrap();
        assert!(budgets.charge(TreeSide::New, 128, 256).is_err());
    }
}
