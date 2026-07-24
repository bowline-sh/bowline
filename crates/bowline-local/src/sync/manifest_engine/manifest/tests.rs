use std::collections::BTreeMap;

use bowline_core::ids::ContentId;
use bowline_storage::{open, seal};

use super::*;

const KEY_BYTES: [u8; 32] = [7; 32];

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

#[test]
fn canonical_serialization_is_deterministic() {
    let crypto = crypto("ws_code", 1);
    let manifest = sample_manifest(&crypto);

    let first = manifest.to_canonical_bytes().expect("serialize");
    let second = manifest.to_canonical_bytes().expect("serialize");
    assert_eq!(first, second, "same manifest must serialize identically");

    // Insertion order must not affect canonical bytes.
    let mut reordered_entries: Vec<_> = manifest.entries.clone().into_iter().collect();
    reordered_entries.reverse();
    let reordered = Manifest::new(crypto.key_epoch(), reordered_entries.into_iter().collect());
    assert_eq!(
        reordered.to_canonical_bytes().expect("serialize"),
        first,
        "insertion order must not change canonical bytes"
    );
}

#[test]
fn manifest_round_trips_through_seal_and_open() {
    let crypto = crypto("ws_code", 3);
    let manifest = sample_manifest(&crypto);
    let plaintext = manifest.to_canonical_bytes().expect("serialize");
    let sealed = seal_manifest(&crypto, &plaintext).expect("seal manifest");

    let decoded =
        open_manifest(&crypto, sealed.as_bytes(), &DecodeLimits::default()).expect("open manifest");
    assert_eq!(decoded.manifest, manifest);
    assert!(decoded.collisions.is_empty());
}

#[test]
fn file_round_trips_through_seal_and_open() {
    let crypto = crypto("ws_code", 1);
    let plaintext = b"secret env value";
    let content_id = crypto.content_id(plaintext);
    let sealed = seal_file(&crypto, &content_id, plaintext).expect("seal");
    let opened = open_file(&crypto, &content_id, sealed.as_bytes()).expect("open");
    assert_eq!(opened, plaintext);
}

#[test]
fn reseal_changes_physical_key_but_not_content_id() {
    let crypto = crypto("ws_code", 1);
    let plaintext = b"stable content";
    let content_id = crypto.content_id(plaintext);

    let first = seal_file(&crypto, &content_id, plaintext).expect("first seal");
    let second = seal_file(&crypto, &content_id, plaintext).expect("second seal");

    assert_ne!(first.as_bytes(), second.as_bytes(), "random nonce differs");
    assert_ne!(
        physical_blob_key(first.as_bytes()),
        physical_blob_key(second.as_bytes()),
        "physical key follows sealed bytes"
    );
    assert_eq!(
        crypto.content_id(plaintext),
        content_id,
        "logical identity is stable across reseals"
    );
}

#[test]
fn substitution_wrong_workspace_fails_open() {
    let sealer = crypto("ws_code", 1);
    let plaintext = b"file bytes";
    let content_id = sealer.content_id(plaintext);
    let sealed = seal_file(&sealer, &content_id, plaintext).expect("seal");

    let attacker = crypto("ws_other", 1);
    assert!(open_file(&attacker, &content_id, sealed.as_bytes()).is_err());
}

#[test]
fn substitution_wrong_purpose_fails_open() {
    let crypto = crypto("ws_code", 1);
    let plaintext = b"file bytes";
    let content_id = crypto.content_id(plaintext);
    let sealed = seal_file(&crypto, &content_id, plaintext).expect("seal");

    // A file blob must never open as a manifest.
    assert!(open_manifest(&crypto, sealed.as_bytes(), &DecodeLimits::default()).is_err());
}

#[test]
fn substitution_wrong_content_id_fails_open() {
    let crypto = crypto("ws_code", 1);
    let plaintext = b"file bytes";
    let content_id = crypto.content_id(plaintext);
    let sealed = seal_file(&crypto, &content_id, plaintext).expect("seal");

    let wrong = crypto.content_id(b"different content");
    assert!(open_file(&crypto, &wrong, sealed.as_bytes()).is_err());
}

#[test]
fn substitution_wrong_epoch_fails_open() {
    let sealer = crypto("ws_code", 1);
    let plaintext = b"file bytes";
    let content_id = sealer.content_id(plaintext);
    let sealed = seal_file(&sealer, &content_id, plaintext).expect("seal");

    let other_epoch = crypto("ws_code", 2);
    assert!(open_file(&other_epoch, &content_id, sealed.as_bytes()).is_err());
}

#[test]
fn substitution_wrong_format_fails_open() {
    let crypto = crypto("ws_code", 1);
    let plaintext = b"file bytes";
    let content_id = crypto.content_id(plaintext);

    // Seal under a divergent framing version; the normal opener uses version 1.
    let context = crypto.file_context_for_test(&content_id, 99);
    let sealed = seal(plaintext, crypto.storage_key_for_test(), &context).expect("seal");
    assert!(open_file(&crypto, &content_id, sealed.as_bytes()).is_err());

    // Sanity: opening under the matching (99) context still succeeds.
    assert!(open(sealed.as_bytes(), crypto.storage_key_for_test(), &context).is_ok());
}

#[test]
fn compression_bomb_rejected_by_bounds() {
    let crypto = crypto("ws_code", 1);
    // Highly compressible plaintext: many identical directory entries seal to a
    // small blob whose decoded size dwarfs it — the classic bomb shape.
    let mut entries = BTreeMap::new();
    for index in 0..2_000 {
        entries.insert(
            WorkspacePath::new(format!("dir-{index:08}")),
            ManifestEntry::Directory {
                mode: FileMode::new(0o755),
            },
        );
    }
    let manifest = Manifest::new(crypto.key_epoch(), entries);
    let plaintext = manifest.to_canonical_bytes().expect("serialize");
    let sealed = seal_manifest(&crypto, &plaintext).expect("seal");

    assert!(
        (sealed.as_bytes().len() as u64) < plaintext.len() as u64,
        "test needs a compressible bomb"
    );

    // Sealed passes, decoded exceeds the bound: rejected after open, before the
    // structured entry map is built.
    let limits = DecodeLimits {
        max_sealed_bytes: u64::MAX,
        max_decoded_bytes: (plaintext.len() as u64) / 2,
        ..DecodeLimits::default()
    };
    assert!(matches!(
        open_manifest(&crypto, sealed.as_bytes(), &limits),
        Err(ManifestError::BoundExceeded {
            bound: "decoded-size"
        })
    ));

    // The pre-decompression guard fires before open even allocates plaintext.
    let sealed_limit = DecodeLimits {
        max_sealed_bytes: (sealed.as_bytes().len() as u64) - 1,
        ..DecodeLimits::default()
    };
    assert!(matches!(
        open_manifest(&crypto, sealed.as_bytes(), &sealed_limit),
        Err(ManifestError::BoundExceeded {
            bound: "sealed-size"
        })
    ));

    // Record-count guard.
    let record_limit = DecodeLimits {
        max_records: 10,
        ..DecodeLimits::default()
    };
    assert!(matches!(
        open_manifest(&crypto, sealed.as_bytes(), &record_limit),
        Err(ManifestError::BoundExceeded {
            bound: "record-count"
        })
    ));
}

#[test]
fn case_collision_reported_not_dropped() {
    let crypto = crypto("ws_code", 1);
    let mut entries = BTreeMap::new();
    entries.insert(
        WorkspacePath::new("README.md"),
        file_entry(&crypto, b"upper"),
    );
    entries.insert(
        WorkspacePath::new("readme.md"),
        file_entry(&crypto, b"lower"),
    );
    let manifest = Manifest::new(crypto.key_epoch(), entries);
    let plaintext = manifest.to_canonical_bytes().expect("serialize");
    let sealed = seal_manifest(&crypto, &plaintext).expect("seal");

    let decoded =
        open_manifest(&crypto, sealed.as_bytes(), &DecodeLimits::default()).expect("open");

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

#[test]
fn decode_rejects_unsafe_and_disordered_paths() {
    let crypto = crypto("ws_code", 1);
    let limits = DecodeLimits::default();

    for bad in [
        "/abs/path",
        "../escape",
        "a/../b",
        ".bowline/local.sqlite3",
        "trailing/",
    ] {
        let mut entries = BTreeMap::new();
        // Insert directly as a raw string to bypass any normalization.
        entries.insert(
            WorkspacePath::new(bad),
            ManifestEntry::Directory {
                mode: FileMode::new(0o755),
            },
        );
        let manifest = Manifest::new(crypto.key_epoch(), entries);
        let plaintext = manifest.to_canonical_bytes().expect("serialize");
        let sealed = seal_manifest(&crypto, &plaintext).expect("seal");
        assert!(
            open_manifest(&crypto, sealed.as_bytes(), &limits).is_err(),
            "unsafe path `{bad}` must be rejected"
        );
    }
}

#[test]
fn decode_detects_duplicate_and_unsorted_entries() {
    let crypto = crypto("ws_code", 1);
    let limits = DecodeLimits::default();

    // Craft raw wire plaintext with a duplicate path (the BTreeMap in-memory
    // form cannot express it, so hand-build the JSON array via serde).
    let duplicate = br#"{"formatVersion":1,"keyEpoch":1,"entries":[{"path":"a","kind":"directory","mode":493},{"path":"a","kind":"directory","mode":493}]}"#;
    assert!(matches!(
        decode_manifest_plaintext(duplicate, crypto.key_epoch(), &limits),
        Err(ManifestError::DuplicatePath)
    ));

    let unsorted = br#"{"formatVersion":1,"keyEpoch":1,"entries":[{"path":"b","kind":"directory","mode":493},{"path":"a","kind":"directory","mode":493}]}"#;
    assert!(matches!(
        decode_manifest_plaintext(unsorted, crypto.key_epoch(), &limits),
        Err(ManifestError::NotSorted)
    ));
}

#[test]
fn decode_rejects_epoch_mismatch() {
    let limits = DecodeLimits::default();
    let plaintext = br#"{"formatVersion":1,"keyEpoch":9,"entries":[]}"#;
    assert!(matches!(
        decode_manifest_plaintext(plaintext, KeyEpoch::new(1), &limits),
        Err(ManifestError::KeyEpochMismatch)
    ));
}

#[test]
fn decode_rejects_unsupported_format_version() {
    let limits = DecodeLimits::default();
    let plaintext = br#"{"formatVersion":2,"keyEpoch":1,"entries":[]}"#;
    assert!(matches!(
        decode_manifest_plaintext(plaintext, KeyEpoch::new(1), &limits),
        Err(ManifestError::UnsupportedFormatVersion { found: 2 })
    ));
}

#[test]
fn measures_hundred_thousand_entry_manifest() {
    let crypto = crypto("ws_code", 1);
    let mut entries = BTreeMap::new();
    for index in 0..100_000_u32 {
        // Distinct content id/blob key per entry so nothing collapses; sizes
        // are realistic small values.
        let content_id = ContentId::new(format!("cid_{index:064x}"));
        let blob_key = BlobKey::new(format!("b_{index:064x}"));
        entries.insert(
            WorkspacePath::new(format!("dir/file-{index:08}.rs")),
            ManifestEntry::File {
                size: (index as u64) % 4096,
                mode: FileMode::new(0o644),
                content_id,
                blob_key,
                key_epoch: crypto.key_epoch(),
            },
        );
    }
    let manifest = Manifest::new(crypto.key_epoch(), entries);
    let plaintext = manifest.to_canonical_bytes().expect("serialize");
    let sealed = seal_manifest(&crypto, &plaintext).expect("seal");

    let limits = DecodeLimits::default();
    assert!((plaintext.len() as u64) <= limits.max_decoded_bytes);
    assert!((sealed.as_bytes().len() as u64) <= limits.max_sealed_bytes);

    let decoded = open_manifest(&crypto, sealed.as_bytes(), &limits).expect("open");
    assert_eq!(decoded.manifest.entries.len(), 100_000);

    println!(
        "manifest_engine 100k-entry manifest: canonical={} bytes, sealed={} bytes",
        plaintext.len(),
        sealed.as_bytes().len()
    );
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

    let manifest = Manifest::new(crypto.key_epoch(), BTreeMap::new());
    let manifest_sealed =
        seal_manifest(&crypto, &manifest.to_canonical_bytes().expect("serialize")).expect("seal");
    let manifest_key = physical_manifest_key(manifest_sealed.as_bytes());

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
