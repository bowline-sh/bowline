//! The per-directory Merkle manifest: node wire form and canonical codec.
//!
//! A directory node is a canonically sorted list of `(name, payload)`, and the
//! ROOT node's physical key is the manifest key the workspace ref points at.
//! Every payload naming a child directory carries that child's node key, so a
//! node key is a collision-resistant function of its entire subtree: two
//! snapshots sharing a subtree share its node object byte for byte, and that
//! object is uploaded once, ever.
//!
//! Why nodes rather than one flat document: a one-character edit rewrites only
//! the nodes on the path from that file to the root — O(changed + depth) — where
//! the flat form reserialized, resealed, and re-uploaded every entry in the
//! workspace on every publish.
//!
//! The encoding is [postcard], a compact non-self-describing binary format.
//! Determinism comes from the type, not from the encoder's goodwill: a node is a
//! `Vec` in one fixed order with fixed field order, so equal nodes encode to
//! equal bytes. JSON was ~200 bytes per entry; this is roughly a quarter of that
//! and carries no key names at all.

use serde::{Deserialize, Serialize};

use bowline_core::ids::ContentId;

use super::{BlobKey, DecodeLimits, FileMode, KeyEpoch, ManifestError, ManifestKey};

/// The canonical tree-node format version, carried inside the sealed plaintext.
/// Distinct from the envelope framing version, which binds the wire framing as
/// associated data.
pub const TREE_FORMAT_VERSION: u32 = 1;

/// What a directory entry points at. The `Directory`/`Subtree` split is load
/// bearing: `Directory` is a manifest entry with its own mode that must be
/// materialized even when empty, while `Subtree` exists only because something
/// below it does. Flattening must not invent an entry for a `Subtree`, or a
/// round trip through the tree would add rows no writer ever published.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TreeEntryPayload {
    File {
        size: u64,
        mode: FileMode,
        content_id: ContentId,
        blob_key: BlobKey,
        key_epoch: KeyEpoch,
    },
    Directory {
        mode: FileMode,
        child: ManifestKey,
    },
    Subtree {
        child: ManifestKey,
    },
    Symlink {
        mode: FileMode,
        target: String,
    },
}

impl TreeEntryPayload {
    /// The child node key this payload descends into, if any.
    pub fn child(&self) -> Option<&ManifestKey> {
        match self {
            Self::Directory { child, .. } | Self::Subtree { child } => Some(child),
            Self::File { .. } | Self::Symlink { .. } => None,
        }
    }
}

/// One directory entry: a single path COMPONENT (never a path) plus its payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeEntry {
    pub name: String,
    pub payload: TreeEntryPayload,
}

/// One sealed directory object.
///
/// Entries serialize as a sorted `Vec` rather than a map so decode can bound the
/// entry count and reject a reordered or duplicated name before building
/// anything — neither of which a map decode into a `BTreeMap` (which silently
/// dedups) could do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeNode {
    pub format_version: u32,
    pub key_epoch: KeyEpoch,
    pub entries: Vec<TreeEntry>,
}

impl TreeNode {
    /// Build a node from entries the caller has already sorted by name.
    pub fn new(key_epoch: KeyEpoch, entries: Vec<TreeEntry>) -> Self {
        Self {
            format_version: TREE_FORMAT_VERSION,
            key_epoch,
            entries,
        }
    }

    /// Deterministic canonical plaintext — the pre-seal identity input. Sealing
    /// is convergent, so `blake3(seal(this))` is a stable function of the node's
    /// content and therefore of its whole subtree.
    pub fn to_canonical_bytes(&self) -> Result<Vec<u8>, ManifestError> {
        postcard::to_allocvec(self)
            .map_err(|_| ManifestError::Serialization("tree node serialization failed"))
    }

    /// Decode node plaintext with full hygiene: format, epoch, entry-count
    /// bound, strict name ordering, and per-entry validity.
    pub fn decode(
        plaintext: &[u8],
        expected_epoch: KeyEpoch,
        limits: &DecodeLimits,
    ) -> Result<Self, ManifestError> {
        // `from_bytes` rejects trailing input, so a node cannot smuggle bytes the
        // decoder ignores but the physical key covers.
        let node: Self = postcard::from_bytes(plaintext)
            .map_err(|_| ManifestError::Serialization("tree node decode failed"))?;
        // An authenticated node from a future writer must fail closed rather than
        // be applied with v1 semantics just because its fields happen to fit.
        if node.format_version != TREE_FORMAT_VERSION {
            return Err(ManifestError::UnsupportedFormatVersion {
                found: node.format_version,
            });
        }
        if node.key_epoch != expected_epoch {
            return Err(ManifestError::KeyEpochMismatch);
        }
        if node.entries.len() as u64 > limits.max_node_entries {
            return Err(ManifestError::BoundExceeded {
                bound: "node-entry-count",
            });
        }
        let mut previous: Option<&str> = None;
        for entry in &node.entries {
            validate_name(&entry.name, limits)?;
            // The vector is the canonical sorted form; a decode that is not
            // strictly increasing is either reordered or duplicated.
            if let Some(prior) = previous
                && prior >= entry.name.as_str()
            {
                return Err(if prior == entry.name.as_str() {
                    ManifestError::DuplicatePath
                } else {
                    ManifestError::NotSorted
                });
            }
            previous = Some(&entry.name);
            validate_payload(&entry.payload, limits)?;
        }
        Ok(node)
    }
}

/// The longest single path component a node may carry. POSIX `NAME_MAX` is 255
/// on every filesystem Bowline materializes onto, so a longer name could never
/// be written anyway.
pub const MAX_NAME_LEN: u64 = 255;

fn validate_name(name: &str, limits: &DecodeLimits) -> Result<(), ManifestError> {
    if name.is_empty() {
        return Err(ManifestError::InvalidEntry {
            reason: "tree entry name is empty",
        });
    }
    if name.len() as u64 > limits.max_name_len {
        return Err(ManifestError::BoundExceeded {
            bound: "name-length",
        });
    }
    if name.contains('/') {
        return Err(ManifestError::InvalidEntry {
            reason: "tree entry name contains a separator",
        });
    }
    if name == "." || name == ".." {
        return Err(ManifestError::InvalidEntry {
            reason: "tree entry name traverses",
        });
    }
    Ok(())
}

fn validate_payload(
    payload: &TreeEntryPayload,
    limits: &DecodeLimits,
) -> Result<(), ManifestError> {
    if let TreeEntryPayload::Symlink { target, .. } = payload {
        if target.is_empty() {
            return Err(ManifestError::InvalidEntry {
                reason: "symlink target is empty",
            });
        }
        if target.len() as u64 > limits.max_path_len {
            return Err(ManifestError::BoundExceeded {
                bound: "symlink-target-length",
            });
        }
    }
    Ok(())
}
