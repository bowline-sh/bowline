//! The object-store side of the Merkle manifest: publish a flat manifest as
//! sealed directory nodes, and fetch a root node back into a flat manifest.
//!
//! The whole point of the tree lives in one sentence: a node's physical key is a
//! collision-resistant function of its entire subtree, so a node that has been
//! uploaded once never needs uploading again, and a node whose key a peer
//! already holds never needs downloading again. Both directions are mediated by
//! the [`TreeNodeLedger`] — a cache keyed by the subtree's CONTENT
//! ([`SubtreeHash`]), not by its path, which is what makes it impossible for the
//! cache to be stale in a way that could substitute the wrong bytes.

use std::collections::BTreeMap;

use super::manifest::directory_tree::SubtreeHash;
use super::manifest::{ManifestError, ManifestKey};
use super::push::TransportError;
use super::store::{ManifestStore, ManifestStoreError};

#[path = "tree_transport/fetch.rs"]
mod fetch;
#[path = "tree_transport/publish.rs"]
mod publish;

pub use fetch::{FetchTreeRequest, FetchedTree, PruneBasis, fetch_tree};
pub use publish::{PublishTreeRequest, publish_tree};

/// Reading half of the node ledger. Separated from [`TreeNodeLedger`] so a
/// pull, which only ever consults the ledger, can hold it behind a shared
/// reference while a push, which also writes it, cannot.
pub trait TreeNodeLookup {
    /// The physical key of the node object for a subtree with this content, if
    /// this device has proven that object exists remotely.
    fn known(&self, hash: &SubtreeHash) -> Result<Option<ManifestKey>, TreeError>;
}

/// The device's memory of tree nodes the object store already holds.
///
/// A row may only be written for a node whose upload actually returned (or that
/// was just downloaded), and children are always recorded before their parent —
/// so a hit on a directory proves its WHOLE subtree is present remotely, which
/// is exactly what lets a publish skip it without re-walking it.
pub trait TreeNodeLedger: TreeNodeLookup {
    fn record(&mut self, hash: SubtreeHash, key: ManifestKey);
}

/// A publisher with no durable memory: every node is sealed and PUT.
///
/// Used where there is no engine store to consult — a work-view accept
/// republishes one project-scoped manifest from a daemon RPC and owns no
/// ancestor of its own. Convergent sealing keeps this correct rather than merely
/// wasteful: re-PUTting an object that already exists with identical bytes is a
/// verified no-op, not a conflict.
#[derive(Debug, Default, Clone, Copy)]
pub struct UnledgeredNodes;

impl TreeNodeLookup for UnledgeredNodes {
    fn known(&self, _hash: &SubtreeHash) -> Result<Option<ManifestKey>, TreeError> {
        Ok(None)
    }
}

impl TreeNodeLedger for UnledgeredNodes {
    fn record(&mut self, _hash: SubtreeHash, _key: ManifestKey) {}
}

/// The engine store's node ledger: durable rows plus whatever this operation has
/// established so far, so one publish never seals the same shared subtree twice.
pub struct StoreNodeLedger<'a> {
    store: &'a ManifestStore,
    key_epoch: super::manifest::KeyEpoch,
    fresh: BTreeMap<SubtreeHash, ManifestKey>,
}

impl<'a> StoreNodeLedger<'a> {
    pub fn new(store: &'a ManifestStore, key_epoch: super::manifest::KeyEpoch) -> Self {
        Self {
            store,
            key_epoch,
            fresh: BTreeMap::new(),
        }
    }

    /// The rows this operation earned the right to persist. The caller commits
    /// them; nothing is written for a node whose upload never returned.
    pub fn into_recorded(self) -> BTreeMap<SubtreeHash, ManifestKey> {
        self.fresh
    }
}

impl TreeNodeLookup for StoreNodeLedger<'_> {
    fn known(&self, hash: &SubtreeHash) -> Result<Option<ManifestKey>, TreeError> {
        if let Some(key) = self.fresh.get(hash) {
            return Ok(Some(key.clone()));
        }
        self.store
            .tree_node(hash, self.key_epoch)
            .map_err(TreeError::Store)
    }
}

impl TreeNodeLedger for StoreNodeLedger<'_> {
    fn record(&mut self, hash: SubtreeHash, key: ManifestKey) {
        self.fresh.insert(hash, key);
    }
}

/// What can go wrong moving tree nodes. Push and pull map these onto their own
/// existing variants, so no caller grows a second error vocabulary.
#[derive(Debug)]
pub enum TreeError {
    Manifest(ManifestError),
    Store(ManifestStoreError),
    Transport(TransportError),
    /// A fetched node's bytes do not hash to the key that named it.
    NodeKeyMismatch,
}

impl std::fmt::Display for TreeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Manifest(error) => write!(formatter, "manifest tree: {error}"),
            Self::Store(error) => write!(formatter, "manifest tree store: {error}"),
            Self::Transport(error) => write!(formatter, "manifest tree {error}"),
            Self::NodeKeyMismatch => {
                formatter.write_str("fetched tree node does not match its key")
            }
        }
    }
}

impl std::error::Error for TreeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Manifest(error) => Some(error),
            Self::Store(error) => Some(error),
            Self::Transport(error) => Some(error),
            Self::NodeKeyMismatch => None,
        }
    }
}
