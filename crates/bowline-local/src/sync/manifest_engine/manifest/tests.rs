use std::collections::BTreeMap;

use bowline_core::ids::ContentId;
use bowline_storage::{open, seal};

use super::directory_tree::DirectoryTree;
use super::tree::{TREE_FORMAT_VERSION, TreeEntry, TreeEntryPayload, TreeNode};
use super::*;
use crate::sync::manifest_engine::endpoint::{CaseForm, NameFolding, NormalizationForm};
use crate::sync::manifest_engine::engine_test_support::{
    FakeRemote, fetch_test_tree, publish_test_tree,
};
use crate::sync::manifest_engine::push::{ManifestUpload, RemoteObjects};
use crate::sync::manifest_engine::tree_transport::TreeError;

const KEY_BYTES: [u8; 32] = [7; 32];
const ROTATED_KEY_BYTES: [u8; 32] = [9; 32];

/// A case- and normalization-sensitive endpoint (ext4-shaped).
const FOLDING: NameFolding = NameFolding::EXACT;

/// An APFS-shaped endpoint: folds both case and normalization form.
fn apfs_folding() -> NameFolding {
    NameFolding::new(NormalizationForm::Insensitive, CaseForm::Insensitive)
}

fn crypto(workspace_id: &str, epoch: u32) -> WorkspaceCrypto {
    WorkspaceCrypto::new(workspace_id, KEY_BYTES, KeyEpoch::new(epoch))
}

fn file_entry(crypto: &WorkspaceCrypto, plaintext: &[u8]) -> ManifestEntry {
    let content_id = crypto.content_id(plaintext);
    let sealed = seal_file(crypto, &content_id, plaintext).expect("seal file");
    ManifestEntry::File {
        size: plaintext.len() as u64,
        mode: FileMode::new(0o644),
        content_id,
        blob_key: physical_blob_key(sealed.as_bytes()),
        key_epoch: crypto.key_epoch(),
    }
}

fn sample_manifest(crypto: &WorkspaceCrypto) -> Manifest {
    let mut entries = BTreeMap::new();
    entries.insert(
        WorkspacePath::new("README.md"),
        file_entry(crypto, b"# docs"),
    );
    entries.insert(
        WorkspacePath::new("src/main.rs"),
        file_entry(crypto, b"fn main() {}"),
    );
    entries.insert(
        WorkspacePath::new("target"),
        ManifestEntry::Directory {
            mode: FileMode::new(0o755),
        },
    );
    entries.insert(
        WorkspacePath::new("link"),
        ManifestEntry::Symlink {
            mode: FileMode::new(0o777),
            target: "src/main.rs".to_string(),
        },
    );
    Manifest::new(crypto.key_epoch(), entries)
}

/// Publish into a scratch object store and read the root key back.
fn round_trip(crypto: &WorkspaceCrypto, manifest: &Manifest) -> Manifest {
    let objects = FakeRemote::new();
    let root = publish_test_tree(&objects, crypto, manifest);
    fetch_test_tree(&objects, crypto, &root, &DecodeLimits::default(), FOLDING)
        .expect("fetch tree")
        .decoded
        .manifest
}

// ---- canonical form -------------------------------------------------------

#[test]
fn canonical_node_serialization_is_deterministic() {
    let crypto = crypto("ws_code", 1);
    let manifest = sample_manifest(&crypto);

    let objects = FakeRemote::new();
    let first = publish_test_tree(&objects, &crypto, &manifest);

    // Insertion order must not affect the published tree: the root key is the
    // whole snapshot's identity, so an order-dependent encoding would break both
    // dedup and the "unchanged subtree is never re-uploaded" contract.
    let mut reordered_entries: Vec<_> = manifest.entries.clone().into_iter().collect();
    reordered_entries.reverse();
    let reordered = Manifest::new(crypto.key_epoch(), reordered_entries.into_iter().collect());
    let second = publish_test_tree(&FakeRemote::new(), &crypto, &reordered);

    assert_eq!(
        first, second,
        "insertion order must not change the root key"
    );
}

#[test]
fn manifest_round_trips_through_the_tree() {
    let crypto = crypto("ws_code", 3);
    let manifest = sample_manifest(&crypto);
    assert_eq!(round_trip(&crypto, &manifest), manifest);
}

#[test]
fn a_directory_implied_only_by_its_descendants_gains_no_entry() {
    // `src` exists on the wire as a structural node so `src/main.rs` can hang off
    // it, but the writer published no entry for it — and a round trip must not
    // invent one, or every peer would gain a row nobody wrote.
    let crypto = crypto("ws_code", 1);
    let mut entries = BTreeMap::new();
    entries.insert(
        WorkspacePath::new("src/main.rs"),
        file_entry(&crypto, b"fn main() {}"),
    );
    let manifest = Manifest::new(crypto.key_epoch(), entries);

    let decoded = round_trip(&crypto, &manifest);
    assert_eq!(decoded.entries.len(), 1);
    assert!(!decoded.entries.contains_key(&WorkspacePath::new("src")));
}

#[test]
fn an_empty_directory_entry_survives_the_round_trip() {
    let crypto = crypto("ws_code", 1);
    let mut entries = BTreeMap::new();
    entries.insert(
        WorkspacePath::new("target"),
        ManifestEntry::Directory {
            mode: FileMode::new(0o700),
        },
    );
    let manifest = Manifest::new(crypto.key_epoch(), entries);
    assert_eq!(round_trip(&crypto, &manifest), manifest);
}

#[test]
fn an_empty_manifest_still_publishes_a_root_node() {
    let crypto = crypto("ws_code", 1);
    let manifest = Manifest::new(crypto.key_epoch(), BTreeMap::new());
    assert!(round_trip(&crypto, &manifest).entries.is_empty());
}

// ---- the shared-subtree property ------------------------------------------

#[test]
fn an_unchanged_subtree_is_shared_by_key_across_snapshots() {
    let crypto = crypto("ws_code", 1);
    let mut entries = BTreeMap::new();
    entries.insert(
        WorkspacePath::new("apps/web/index.ts"),
        file_entry(&crypto, b"web"),
    );
    entries.insert(
        WorkspacePath::new("crates/core/lib.rs"),
        file_entry(&crypto, b"core"),
    );
    let before = Manifest::new(crypto.key_epoch(), entries.clone());

    entries.insert(
        WorkspacePath::new("apps/web/index.ts"),
        file_entry(&crypto, b"web edited"),
    );
    let after = Manifest::new(crypto.key_epoch(), entries);

    let subtree = |manifest: &Manifest, dir: &str| {
        let tree = DirectoryTree::decompose(&manifest.entries).expect("decompose");
        tree.subtree_hashes(crypto.key_epoch())
            .expect("hashes")
            .get(&super::directory_tree::DirPath::root().child(dir))
            .cloned()
            .expect("subtree hash")
    };

    assert_eq!(
        subtree(&before, "crates"),
        subtree(&after, "crates"),
        "an untouched subtree keeps its identity, so it is never re-uploaded"
    );
    assert_ne!(
        subtree(&before, "apps"),
        subtree(&after, "apps"),
        "the edited subtree must change identity"
    );
}

#[test]
fn a_file_and_directory_claiming_one_name_is_refused() {
    let crypto = crypto("ws_code", 1);
    let mut entries = BTreeMap::new();
    entries.insert(WorkspacePath::new("src"), file_entry(&crypto, b"not a dir"));
    entries.insert(
        WorkspacePath::new("src/main.rs"),
        file_entry(&crypto, b"fn main() {}"),
    );
    assert!(matches!(
        DirectoryTree::decompose(&entries),
        Err(ManifestError::InvalidEntry { .. })
    ));
}

// ---- sealing boundary -----------------------------------------------------

#[test]
fn file_round_trips_through_seal_and_open() {
    let crypto = crypto("ws_code", 1);
    let plaintext = b"secret env value";
    let content_id = crypto.content_id(plaintext);
    let sealed = seal_file(&crypto, &content_id, plaintext).expect("seal");
    let opened =
        open_file(&crypto, crypto.key_epoch(), &content_id, sealed.as_bytes()).expect("open");
    assert_eq!(opened, plaintext);
}

#[test]
fn old_epoch_file_opens_from_a_rotated_keyring() {
    let epoch_one = crypto("ws_code", 1);
    let plaintext = b"pre-rotation bytes";
    let content_id = epoch_one.content_id(plaintext);
    let sealed = seal_file(&epoch_one, &content_id, plaintext).expect("seal");
    let rotated = WorkspaceCrypto::new("ws_code", ROTATED_KEY_BYTES, KeyEpoch::new(2))
        .with_key_epoch(KeyEpoch::new(1), KEY_BYTES);

    let opened =
        open_file(&rotated, KeyEpoch::new(1), &content_id, sealed.as_bytes()).expect("open");

    assert_eq!(opened, plaintext);
    assert_eq!(
        rotated.content_id_at(KeyEpoch::new(1), &opened).as_ref(),
        Some(&content_id)
    );
}

#[test]
fn old_epoch_file_without_held_key_returns_unknown_epoch() {
    let epoch_one = crypto("ws_code", 1);
    let plaintext = b"pre-rotation bytes";
    let content_id = epoch_one.content_id(plaintext);
    let sealed = seal_file(&epoch_one, &content_id, plaintext).expect("seal");
    let rotated_only = WorkspaceCrypto::new("ws_code", ROTATED_KEY_BYTES, KeyEpoch::new(2));

    assert!(matches!(
        open_file(
            &rotated_only,
            KeyEpoch::new(1),
            &content_id,
            sealed.as_bytes()
        ),
        Err(ManifestError::UnknownKeyEpoch {
            key_epoch
        }) if key_epoch == KeyEpoch::new(1)
    ));
}

#[test]
fn old_epoch_tree_node_opens_from_a_rotated_keyring_and_reports_epoch() {
    let epoch_one = crypto("ws_code", 1);
    let node = TreeNode::new(epoch_one.key_epoch(), Vec::new());
    let plaintext = node.to_canonical_bytes().expect("encode");
    let sealed = seal_tree_node(&epoch_one, &plaintext).expect("seal");
    let rotated = WorkspaceCrypto::new("ws_code", ROTATED_KEY_BYTES, KeyEpoch::new(2))
        .with_key_epoch(KeyEpoch::new(1), KEY_BYTES);

    let (opened, opened_epoch) =
        open_tree_node(&rotated, sealed.as_bytes(), &DecodeLimits::default()).expect("open");

    assert_eq!(opened, plaintext);
    assert_eq!(opened_epoch, KeyEpoch::new(1));
}

#[test]
fn new_writes_seal_at_the_write_epoch() {
    let rotated = WorkspaceCrypto::new("ws_code", ROTATED_KEY_BYTES, KeyEpoch::new(2))
        .with_key_epoch(KeyEpoch::new(1), KEY_BYTES);
    let plaintext = b"post-rotation bytes";
    let content_id = rotated.content_id(plaintext);
    let sealed = seal_file(&rotated, &content_id, plaintext).expect("seal");

    let opened =
        open_file(&rotated, KeyEpoch::new(2), &content_id, sealed.as_bytes()).expect("open");

    assert_eq!(opened, plaintext);
    assert!(open_file(&rotated, KeyEpoch::new(1), &content_id, sealed.as_bytes()).is_err());
}

#[test]
fn reseal_is_convergent_so_the_physical_key_is_a_real_content_address() {
    let crypto = crypto("ws_code", 1);
    let plaintext = b"stable content";
    let content_id = crypto.content_id(plaintext);

    let first = seal_file(&crypto, &content_id, plaintext).expect("first seal");
    let second = seal_file(&crypto, &content_id, plaintext).expect("second seal");

    assert_eq!(
        first.as_bytes(),
        second.as_bytes(),
        "the envelope nonce is derived from the plaintext, so a reseal reproduces the object"
    );
    assert_eq!(
        physical_blob_key(first.as_bytes()),
        physical_blob_key(second.as_bytes()),
        "the physical key is a function of the plaintext, which is what makes dedup possible"
    );
    assert_eq!(
        crypto.content_id(plaintext),
        content_id,
        "logical identity is stable across reseals"
    );
}

#[test]
fn tree_node_sealing_is_convergent_so_a_shared_subtree_is_one_object() {
    let crypto = crypto("ws_code", 1);
    let node = TreeNode::new(
        crypto.key_epoch(),
        vec![TreeEntry {
            name: "a.txt".to_string(),
            payload: TreeEntryPayload::Symlink {
                mode: FileMode::new(0o777),
                target: "b.txt".to_string(),
            },
        }],
    );
    let plaintext = node.to_canonical_bytes().expect("encode");
    let first = seal_tree_node(&crypto, &plaintext).expect("seal");
    let second = seal_tree_node(&crypto, &plaintext).expect("reseal");
    assert_eq!(
        physical_manifest_key(first.as_bytes()),
        physical_manifest_key(second.as_bytes()),
    );
}

#[test]
fn a_different_workspace_yields_a_different_object_for_the_same_bytes() {
    let plaintext = b"stable content";
    let ours = crypto("ws_code", 1);
    let theirs = crypto("ws_other", 1);

    let sealed_here = seal_file(&ours, &ours.content_id(plaintext), plaintext).expect("ours");
    let sealed_there =
        seal_file(&theirs, &theirs.content_id(plaintext), plaintext).expect("theirs");

    // Convergence is deliberately scoped to one workspace: the server may learn
    // that two objects in the SAME workspace are identical, and nothing more.
    // The workspace identity is bound into the envelope's associated data, which
    // feeds the nonce derivation, so the same bytes in two workspaces seal to two
    // unrelated objects even when the key material happens to coincide.
    assert_ne!(
        physical_blob_key(sealed_here.as_bytes()),
        physical_blob_key(sealed_there.as_bytes()),
        "identical bytes in different workspaces must not collide"
    );
}

#[test]
fn substitution_wrong_workspace_fails_open() {
    let sealer = crypto("ws_code", 1);
    let plaintext = b"file bytes";
    let content_id = sealer.content_id(plaintext);
    let sealed = seal_file(&sealer, &content_id, plaintext).expect("seal");

    let attacker = crypto("ws_other", 1);
    assert!(
        open_file(
            &attacker,
            sealer.key_epoch(),
            &content_id,
            sealed.as_bytes()
        )
        .is_err()
    );
}

#[test]
fn substitution_wrong_purpose_fails_open() {
    let crypto = crypto("ws_code", 1);
    let plaintext = b"file bytes";
    let content_id = crypto.content_id(plaintext);
    let sealed = seal_file(&crypto, &content_id, plaintext).expect("seal");

    // A file blob must never open as a tree node.
    assert!(open_tree_node(&crypto, sealed.as_bytes(), &DecodeLimits::default()).is_err());
}

#[test]
fn substitution_wrong_content_id_fails_open() {
    let crypto = crypto("ws_code", 1);
    let plaintext = b"file bytes";
    let content_id = crypto.content_id(plaintext);
    let sealed = seal_file(&crypto, &content_id, plaintext).expect("seal");

    let wrong = crypto.content_id(b"different content");
    assert!(open_file(&crypto, crypto.key_epoch(), &wrong, sealed.as_bytes()).is_err());
}

#[test]
fn substitution_wrong_epoch_fails_open() {
    let sealer = crypto("ws_code", 1);
    let plaintext = b"file bytes";
    let content_id = sealer.content_id(plaintext);
    let sealed = seal_file(&sealer, &content_id, plaintext).expect("seal");

    let other_epoch = crypto("ws_code", 2);
    assert!(
        open_file(
            &other_epoch,
            sealer.key_epoch(),
            &content_id,
            sealed.as_bytes()
        )
        .is_err()
    );
}

#[test]
fn substitution_wrong_format_fails_open() {
    let crypto = crypto("ws_code", 1);
    let plaintext = b"file bytes";
    let content_id = crypto.content_id(plaintext);

    // Seal under a divergent framing version; the normal opener uses version 1.
    let context = crypto.file_context_for_test(&content_id, 99);
    let sealed = seal(plaintext, crypto.storage_key_for_test(), &context).expect("seal");
    assert!(open_file(&crypto, crypto.key_epoch(), &content_id, sealed.as_bytes()).is_err());

    // Sanity: opening under the matching (99) context still succeeds.
    assert!(open(sealed.as_bytes(), crypto.storage_key_for_test(), &context).is_ok());
}

// ---- bounded decode -------------------------------------------------------

/// Seal a node whose plaintext compresses hard: many identical entries.
fn compressible_node(crypto: &WorkspaceCrypto) -> (Vec<u8>, Vec<u8>) {
    let entries = (0..2_000)
        .map(|index| TreeEntry {
            name: format!("dir-{index:08}"),
            payload: TreeEntryPayload::Symlink {
                mode: FileMode::new(0o777),
                target: "the same target everywhere".to_string(),
            },
        })
        .collect();
    let plaintext = TreeNode::new(crypto.key_epoch(), entries)
        .to_canonical_bytes()
        .expect("encode");
    let sealed = seal_tree_node(crypto, &plaintext)
        .expect("seal")
        .as_bytes()
        .to_vec();
    (plaintext, sealed)
}

#[test]
fn compression_bomb_rejected_by_bounds() {
    let crypto = crypto("ws_code", 1);
    let (plaintext, sealed) = compressible_node(&crypto);
    assert!(
        (sealed.len() as u64) < plaintext.len() as u64,
        "test needs a compressible bomb"
    );

    // Sealed passes, decoded exceeds the bound: rejected after open, before the
    // structured entry list is built.
    let decoded_limit = DecodeLimits {
        max_sealed_bytes: u64::MAX,
        max_decoded_bytes: (plaintext.len() as u64) / 2,
        ..DecodeLimits::default()
    };
    assert!(matches!(
        open_tree_node(&crypto, &sealed, &decoded_limit),
        Err(ManifestError::BoundExceeded {
            bound: "decoded-size"
        })
    ));

    // The pre-decompression guard fires before open even allocates plaintext.
    let sealed_limit = DecodeLimits {
        max_sealed_bytes: (sealed.len() as u64) - 1,
        ..DecodeLimits::default()
    };
    assert!(matches!(
        open_tree_node(&crypto, &sealed, &sealed_limit),
        Err(ManifestError::BoundExceeded {
            bound: "sealed-size"
        })
    ));
}

#[test]
fn node_fanout_is_bounded() {
    let crypto = crypto("ws_code", 1);
    let (plaintext, _) = compressible_node(&crypto);
    let limits = DecodeLimits {
        max_node_entries: 10,
        ..DecodeLimits::default()
    };
    assert!(matches!(
        TreeNode::decode(&plaintext, crypto.key_epoch(), &limits),
        Err(ManifestError::BoundExceeded {
            bound: "node-entry-count"
        })
    ));
}

#[test]
fn total_record_count_is_bounded_across_the_whole_tree() {
    let crypto = crypto("ws_code", 1);
    let mut entries = BTreeMap::new();
    for index in 0..40 {
        entries.insert(
            WorkspacePath::new(format!("d{index:02}/f.txt")),
            file_entry(&crypto, b"x"),
        );
    }
    let manifest = Manifest::new(crypto.key_epoch(), entries);
    let objects = FakeRemote::new();
    let root = publish_test_tree(&objects, &crypto, &manifest);

    let limits = DecodeLimits {
        max_records: 10,
        ..DecodeLimits::default()
    };
    assert!(matches!(
        fetch_test_tree(&objects, &crypto, &root, &limits, FOLDING),
        Err(TreeError::Manifest(ManifestError::BoundExceeded {
            bound: "record-count"
        }))
    ));
}

#[test]
fn the_decoded_bytes_a_whole_tree_costs_are_bounded() {
    // Every node here is individually valid — the per-node decoded cap is at its
    // default. Only the sum across the walk is out of bounds, which is exactly
    // the shape a hostile peer uses to make this device hold a tree it can never
    // afford: many small legal nodes rather than one oversized one.
    let crypto = crypto("ws_code", 1);
    let mut entries = BTreeMap::new();
    for index in 0..40 {
        entries.insert(
            WorkspacePath::new(format!("d{index:02}/f.txt")),
            file_entry(&crypto, b"x"),
        );
    }
    let manifest = Manifest::new(crypto.key_epoch(), entries);
    let objects = FakeRemote::new();
    let root = publish_test_tree(&objects, &crypto, &manifest);

    let limits = DecodeLimits {
        max_aggregate_decoded_bytes: 256,
        ..DecodeLimits::default()
    };
    assert!(matches!(
        fetch_test_tree(&objects, &crypto, &root, &limits, FOLDING),
        Err(TreeError::Manifest(ManifestError::BoundExceeded {
            bound: "tree-aggregate-decoded-bytes"
        }))
    ));

    // The same tree is accepted when the aggregate cap is not the binding
    // constraint, so the bound rejects for its own reason and not incidentally.
    assert!(fetch_test_tree(&objects, &crypto, &root, &DecodeLimits::default(), FOLDING).is_ok());
}

#[test]
fn tree_depth_is_bounded() {
    let crypto = crypto("ws_code", 1);
    let deep: Vec<String> = (0..40).map(|index| format!("d{index}")).collect();
    let mut entries = BTreeMap::new();
    entries.insert(
        WorkspacePath::new(format!("{}/leaf.txt", deep.join("/"))),
        file_entry(&crypto, b"deep"),
    );
    let manifest = Manifest::new(crypto.key_epoch(), entries);
    let objects = FakeRemote::new();
    let root = publish_test_tree(&objects, &crypto, &manifest);

    let limits = DecodeLimits {
        max_depth: 5,
        ..DecodeLimits::default()
    };
    assert!(matches!(
        fetch_test_tree(&objects, &crypto, &root, &limits, FOLDING),
        Err(TreeError::Manifest(ManifestError::BoundExceeded {
            bound: "tree-depth"
        }))
    ));
}

#[test]
fn the_writer_refuses_a_path_deeper_than_the_reader_accepts() {
    let deep: Vec<String> = (0..MAX_WORKSPACE_PATH_DEPTH + 1)
        .map(|index| format!("d{index}"))
        .collect();
    assert_eq!(
        publishable_workspace_path(&deep.join("/"), MAX_WORKSPACE_PATH_LEN, false),
        Err(PathRejection::TooDeep)
    );
}

// ---- decode hygiene -------------------------------------------------------

fn node_plaintext(key_epoch: KeyEpoch, names: &[&str]) -> Vec<u8> {
    let entries = names
        .iter()
        .map(|name| TreeEntry {
            name: (*name).to_string(),
            payload: TreeEntryPayload::Directory {
                mode: FileMode::new(0o755),
                child: ManifestKey::new(format!("m_{:064x}", 1)),
            },
        })
        .collect();
    TreeNode::new(key_epoch, entries)
        .to_canonical_bytes()
        .expect("encode")
}

#[test]
fn decode_detects_duplicate_and_unsorted_entries() {
    let epoch = KeyEpoch::new(1);
    let limits = DecodeLimits::default();

    assert!(matches!(
        TreeNode::decode(&node_plaintext(epoch, &["a", "a"]), epoch, &limits),
        Err(ManifestError::DuplicatePath)
    ));
    assert!(matches!(
        TreeNode::decode(&node_plaintext(epoch, &["b", "a"]), epoch, &limits),
        Err(ManifestError::NotSorted)
    ));
}

#[test]
fn decode_rejects_names_that_are_not_single_components() {
    let epoch = KeyEpoch::new(1);
    let limits = DecodeLimits::default();
    for bad in ["a/b", "..", ".", ""] {
        assert!(
            matches!(
                TreeNode::decode(&node_plaintext(epoch, &[bad]), epoch, &limits),
                Err(ManifestError::InvalidEntry { .. })
            ),
            "name `{bad}` must be rejected"
        );
    }
}

#[test]
fn decode_rejects_epoch_mismatch() {
    let limits = DecodeLimits::default();
    let plaintext = node_plaintext(KeyEpoch::new(9), &["a"]);
    assert!(matches!(
        TreeNode::decode(&plaintext, KeyEpoch::new(1), &limits),
        Err(ManifestError::KeyEpochMismatch)
    ));
}

#[test]
fn decode_rejects_unsupported_format_version() {
    let limits = DecodeLimits::default();
    let mut node = TreeNode::new(KeyEpoch::new(1), Vec::new());
    node.format_version = TREE_FORMAT_VERSION + 1;
    let plaintext = node.to_canonical_bytes().expect("encode");
    assert!(matches!(
        TreeNode::decode(&plaintext, KeyEpoch::new(1), &limits),
        Err(ManifestError::UnsupportedFormatVersion { .. })
    ));
}

#[test]
fn the_walk_rejects_unsafe_paths() {
    let crypto = crypto("ws_code", 1);
    let objects = FakeRemote::new();
    // Reserved and traversing names cannot be published through the flat map, so
    // they are injected straight into a node the walk then has to refuse.
    for bad in ["..", ".bowline"] {
        let child = publish_test_tree(
            &objects,
            &crypto,
            &Manifest::new(crypto.key_epoch(), BTreeMap::new()),
        );
        let node = TreeNode::new(
            crypto.key_epoch(),
            vec![TreeEntry {
                name: bad.to_string(),
                payload: TreeEntryPayload::Directory {
                    mode: FileMode::new(0o755),
                    child,
                },
            }],
        );
        let plaintext = node.to_canonical_bytes().expect("encode");
        let sealed = seal_tree_node(&crypto, &plaintext).expect("seal");
        let root = physical_manifest_key(sealed.as_bytes());
        objects
            .put_manifest(ManifestUpload {
                key: &root,
                content_id: &crypto.tree_node_content_id(&plaintext),
                key_epoch: crypto.key_epoch(),
                sealed: sealed.as_bytes(),
            })
            .expect("store hostile node");

        assert!(
            fetch_test_tree(&objects, &crypto, &root, &DecodeLimits::default(), FOLDING).is_err(),
            "unsafe name `{bad}` must be rejected by the walk"
        );
    }
}

#[test]
fn a_node_whose_bytes_do_not_match_its_key_is_refused() {
    let crypto = crypto("ws_code", 1);
    let objects = FakeRemote::new();
    let manifest = sample_manifest(&crypto);
    let root = publish_test_tree(&objects, &crypto, &manifest);

    // Re-file the root's bytes under a key that does not name them.
    let forged = ManifestKey::new(format!("m_{:064x}", 0xdeadu32));
    let sealed = objects.get_manifest(&root).expect("stored root");
    objects
        .put_manifest(ManifestUpload {
            key: &forged,
            content_id: &crypto.content_id(b"irrelevant"),
            key_epoch: crypto.key_epoch(),
            sealed: &sealed,
        })
        .expect("store forged node");

    assert!(matches!(
        fetch_test_tree(
            &objects,
            &crypto,
            &forged,
            &DecodeLimits::default(),
            FOLDING
        ),
        Err(TreeError::NodeKeyMismatch)
    ));
}

// ---- collisions -----------------------------------------------------------

fn published_pair(crypto: &WorkspaceCrypto, left: &str, right: &str) -> (FakeRemote, ManifestKey) {
    let objects = FakeRemote::new();
    let mut entries = BTreeMap::new();
    entries.insert(WorkspacePath::new(left), file_entry(crypto, b"left"));
    entries.insert(WorkspacePath::new(right), file_entry(crypto, b"right"));
    let manifest = Manifest::new(crypto.key_epoch(), entries);
    let root = publish_test_tree(&objects, crypto, &manifest);
    (objects, root)
}

fn decoded_pair(
    crypto: &WorkspaceCrypto,
    left: &str,
    right: &str,
    names: NameFolding,
) -> DecodedManifest {
    let (objects, root) = published_pair(crypto, left, right);
    fetch_test_tree(&objects, crypto, &root, &DecodeLimits::default(), names)
        .expect("fetch")
        .decoded
}

#[test]
fn case_collision_reported_not_dropped() {
    let crypto = crypto("ws_code", 1);
    let decoded = decoded_pair(&crypto, "README.md", "readme.md", apfs_folding());

    // Both entries survive decode — never silently dropped.
    assert_eq!(decoded.manifest.entries.len(), 2);
    assert!(
        decoded
            .manifest
            .entries
            .contains_key(&WorkspacePath::new("README.md"))
    );
    assert!(
        decoded
            .manifest
            .entries
            .contains_key(&WorkspacePath::new("readme.md"))
    );

    // The collision is reported so the caller can conflict-aside it.
    assert_eq!(decoded.collisions.len(), 1);
    assert_eq!(decoded.collisions[0].folded, "readme.md");
    assert_eq!(
        decoded.collisions[0].paths,
        vec![
            WorkspacePath::new("README.md"),
            WorkspacePath::new("readme.md")
        ]
    );
}

/// `café` precomposed (NFC) and decomposed (NFD). One name to a reader, two
/// byte strings to a `BTreeMap`.
const CAFE_NFC: &str = "notes/caf\u{e9}.md";
const CAFE_NFD: &str = "notes/cafe\u{301}.md";

#[test]
fn normalization_collision_is_reported_on_a_folding_endpoint() {
    let crypto = crypto("ws_code", 1);
    let decoded = decoded_pair(&crypto, CAFE_NFC, CAFE_NFD, apfs_folding());

    // Both spellings survive decode; the report is what lets the caller aside
    // the loser instead of silently overwriting one file with the other.
    assert_eq!(decoded.manifest.entries.len(), 2);
    assert_eq!(decoded.collisions.len(), 1);
    assert_eq!(decoded.collisions[0].folded, CAFE_NFC);
    assert_eq!(
        decoded.collisions[0].paths,
        vec![WorkspacePath::new(CAFE_NFD), WorkspacePath::new(CAFE_NFC),]
    );
}

#[test]
fn a_normalization_sensitive_endpoint_reports_no_collision() {
    let crypto = crypto("ws_code", 1);
    // ext4 genuinely holds these as two files; telling the caller they collide
    // would force a conflict-aside for two unrelated paths.
    let decoded = decoded_pair(&crypto, CAFE_NFC, CAFE_NFD, FOLDING);
    assert_eq!(decoded.manifest.entries.len(), 2);
    assert!(decoded.collisions.is_empty());
}

#[test]
fn a_case_sensitive_endpoint_reports_no_case_collision() {
    let crypto = crypto("ws_code", 1);
    let decoded = decoded_pair(&crypto, "README.md", "readme.md", FOLDING);
    assert_eq!(decoded.manifest.entries.len(), 2);
    assert!(decoded.collisions.is_empty());
}

// ---- scale ----------------------------------------------------------------

#[test]
fn measures_hundred_thousand_entry_manifest() {
    let crypto = crypto("ws_code", 1);
    let mut entries = BTreeMap::new();
    for index in 0..100_000_u32 {
        // Distinct content id/blob key per entry so nothing collapses; sizes
        // are realistic small values.
        entries.insert(
            WorkspacePath::new(format!("dir/file-{index:08}.rs")),
            ManifestEntry::File {
                size: (index as u64) % 4096,
                mode: FileMode::new(0o644),
                content_id: ContentId::new(format!("cid_{index:064x}")),
                blob_key: BlobKey::new(format!("b_{index:064x}")),
                key_epoch: crypto.key_epoch(),
            },
        );
    }
    let manifest = Manifest::new(crypto.key_epoch(), entries);
    let objects = FakeRemote::new();
    let root = publish_test_tree(&objects, &crypto, &manifest);

    let limits = DecodeLimits::default();
    let sealed_root = objects.get_manifest(&root).expect("root node");
    assert!((sealed_root.len() as u64) <= limits.max_sealed_bytes);

    let decoded = fetch_test_tree(&objects, &crypto, &root, &limits, FOLDING)
        .expect("fetch")
        .decoded;
    assert_eq!(decoded.manifest.entries.len(), 100_000);
}

// Plan 110 equivalence check: the engine's physical key syntax and the hosted
// object contract must agree. Rather than a "mirrors X, keep in sync" comment,
// this test fails at build/test time if the engine's `b_`/`m_` keys ever drift
// from the prefixes and 64-hex sealed-hash shape the storage `ObjectKey` parser
// (shared with the hosted key validator) accepts.
#[test]
fn physical_keys_match_hosted_object_key_contract() {
    let crypto = crypto("ws_code", 1);
    let file_sealed = seal_file(&crypto, &crypto.content_id(b"x"), b"x").expect("seal file");
    let blob_key = physical_blob_key(file_sealed.as_bytes());

    let manifest_key = publish_test_tree(
        &FakeRemote::new(),
        &crypto,
        &Manifest::new(crypto.key_epoch(), BTreeMap::new()),
    );

    // Prefixes line up with the storage/hosted constants.
    assert!(
        blob_key
            .as_str()
            .starts_with(bowline_storage::ObjectKey::BLOB_PREFIX)
    );
    assert!(
        manifest_key
            .as_str()
            .starts_with(bowline_storage::ObjectKey::MANIFEST_PREFIX)
    );

    // The exact key strings the engine emits are accepted by the shared parser.
    assert!(bowline_storage::ObjectKey::new(blob_key.as_str()).is_ok());
    assert!(bowline_storage::ObjectKey::new(manifest_key.as_str()).is_ok());
}
