//! Upload durability: what the engine may claim about bytes it has sent.
//!
//! The engine keeps a durable ledger of sealed blobs so a later push can skip
//! re-uploading content that is already stored. That ledger is only safe if a
//! row is written strictly after the bytes it names are durable -- otherwise a
//! transport failure strands the bytes while the claim survives, and the next
//! manifest names a blob no peer can ever fetch.

use std::fs;

use super::engine_test_support::DriverHarness;
use super::{AUDIT_INTERVAL_MAX_MS, EngineEvent, FullScanReason};

// The durable "this blob is stored" ledger is what lets a later push skip an
// upload. Its rows must therefore never be written before the bytes exist. The
// production transport accepts a blob into a queue and drains it later, so
// `put_blob` returning Ok means accepted, not stored -- and when a drain fails
// its jobs are dropped while the rows survive. The retry then trusts the ledger,
// skips the upload, and publishes a manifest naming a blob no peer can fetch.
// Content is end-to-end encrypted, so nothing server-side can ever notice.
#[test]
fn a_failed_upload_drain_never_leaves_the_head_naming_a_blob_that_was_never_stored() {
    let mut harness = DriverHarness::new("safety-upload-settlement", "device-a");
    harness.start();
    for (name, body) in [("a.txt", "alpha"), ("b.txt", "bravo"), ("c.txt", "charlie")] {
        fs::write(harness.root.join(name), body).expect("seed file");
    }

    // Queue blob uploads and fail the first drain, dropping what it held.
    harness.remote.defer_blob_uploads(1);

    harness.event(EngineEvent::FullScanRequired(
        FullScanReason::WatcherOverflow,
    ));
    for _ in 0..12 {
        harness.run_due();
        harness.clock.advance(AUDIT_INTERVAL_MAX_MS);
    }

    // Referential closure of the published head, not a counter: counters advance
    // for too many reasons to discriminate here.
    let crypto = super::engine_test_support::test_crypto();
    let stored = harness.remote.stored_blob_keys();
    let dangling: Vec<String> = harness
        .remote
        .decoded_manifest(&crypto)
        .map(|manifest| {
            manifest
                .entries
                .values()
                .filter_map(|entry| match entry {
                    super::ManifestEntry::File { blob_key, .. } => {
                        Some(blob_key.as_str().to_string())
                    }
                    _ => None,
                })
                .filter(|key| !stored.contains(key))
                .collect()
        })
        .unwrap_or_default();
    assert!(
        dangling.is_empty(),
        "the published head names {} blob(s) that were never stored, so a peer can never \
         materialize them: {dangling:?}",
        dangling.len()
    );
}
