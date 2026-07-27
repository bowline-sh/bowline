//! Unicode normalization discipline end to end (report R8).
//!
//! APFS is normalization-*insensitive* and normalization-*preserving*: a name
//! created decomposed (NFD) on macOS and precomposed (NFC) on Linux is the same
//! file on disk under two different byte strings. Left alone, the engine
//! observes that one file twice, publishes it as two manifest entries, and lets
//! apply ping-pong between them — manufacturing duplicate conflict-asides for a
//! file nobody touched.
//!
//! Every case here forces [`NameFolding`] rather than trusting the volume the
//! test happens to run on, and creates its fixture under the precomposed
//! spelling, which resolves on a folding and a non-folding filesystem alike. The
//! probe that produces the real verdict has its own tests in `endpoint`.

use std::fs;

use super::endpoint::{CaseForm, NameFolding, NormalizationForm};
use super::engine_test_support::TestEngine;
use super::manifest::WorkspacePath;
use super::push::PushOutcome;

/// `notes/café.md` precomposed (`é` as one scalar) and decomposed (`e` plus a
/// combining acute accent). One name to a reader, two keys to a `BTreeMap`.
const CAFE_NFC: &str = "notes/caf\u{e9}.md";
const CAFE_NFD: &str = "notes/cafe\u{301}.md";

/// An APFS-shaped volume: folds both normalization form and case.
fn apfs() -> NameFolding {
    NameFolding::new(NormalizationForm::Insensitive, CaseForm::Insensitive)
}

fn wp(path: &str) -> WorkspacePath {
    WorkspacePath::new(path)
}

fn notes_entries(engine: &TestEngine) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(engine.root().join("notes"))
        .expect("notes dir")
        .map(|entry| {
            entry
                .expect("dir entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

#[test]
fn a_decomposed_dirty_path_is_the_same_entry_as_its_precomposed_twin() {
    let mut engine = TestEngine::new("nfc-one-entry");
    engine.ctx.names = apfs();
    engine.write(CAFE_NFC, b"beans");
    engine.push(&[CAFE_NFC]);

    // The watcher now reports the SAME file under its decomposed spelling — the
    // ordinary macOS case, where the event carries whatever form created the
    // name. It must settle as unchanged, not publish a second entry.
    let outcome = engine.push(&[CAFE_NFD]);
    assert!(
        matches!(outcome, PushOutcome::NoChange { .. }),
        "a decomposed spelling of a published path is not a change: {outcome:?}"
    );

    let files = engine.files();
    assert_eq!(files.len(), 1, "one file on disk stays one ancestor row");
    assert!(files.contains_key(&wp(CAFE_NFC)));
}

#[test]
fn a_decomposed_dirty_path_publishes_under_its_precomposed_spelling() {
    let mut engine = TestEngine::new("nfc-wire-form");
    engine.ctx.names = apfs();
    engine.write(CAFE_NFC, b"beans");

    // Only the decomposed spelling is dirty, so the wire form is decided purely
    // by the canonicalization — NFC, following Dropbox and Google Drive.
    engine.push(&[CAFE_NFD]);

    let manifest = engine
        .remote
        .decoded_manifest(&engine.ctx.crypto)
        .expect("head manifest");
    assert_eq!(
        manifest.entries.keys().collect::<Vec<_>>(),
        vec![&wp(CAFE_NFC)]
    );
}

#[test]
fn an_already_precomposed_path_is_published_byte_for_byte() {
    let mut engine = TestEngine::new("nfc-untouched");
    engine.ctx.names = apfs();
    engine.write("src/auth.ts", b"export {}");
    engine.write(CAFE_NFC, b"beans");
    engine.push(&["src/auth.ts", CAFE_NFC]);

    let manifest = engine
        .remote
        .decoded_manifest(&engine.ctx.crypto)
        .expect("head manifest");
    assert_eq!(
        manifest.entries.keys().collect::<Vec<_>>(),
        vec![&wp(CAFE_NFC), &wp("src/auth.ts")],
        "canonicalization rewrites nothing that is already NFC"
    );
}

#[test]
fn a_push_pull_round_trip_creates_no_duplicate_aside_for_a_decomposed_name() {
    let mut producer = TestEngine::new("nfc-roundtrip-producer");
    producer.ctx.names = apfs();
    producer.write(CAFE_NFC, b"beans");
    producer.push(&[CAFE_NFC]);

    let mut peer = TestEngine::new("nfc-roundtrip-peer");
    peer.ctx.names = apfs();
    peer.remote = producer.remote.clone_state();

    let installed = peer.pull();
    assert!(installed.conflict_asides.is_empty());
    assert_eq!(peer.read(CAFE_NFC), b"beans");

    // The peer's watcher reports the freshly installed file under the decomposed
    // spelling. Before the fold, this pushed a second entry for one file, and the
    // producer then pulled it as an unknown create — the ping-pong that ends in
    // duplicate asides on both devices.
    let echoed = peer.push(&[CAFE_NFD]);
    assert!(
        matches!(echoed, PushOutcome::NoChange { .. }),
        "the pull echo publishes nothing: {echoed:?}"
    );

    let settled = peer.pull();
    assert!(settled.already_current);
    assert!(settled.conflict_asides.is_empty());
    assert_eq!(
        notes_entries(&peer).len(),
        1,
        "exactly one file, so no aside was materialized alongside it"
    );

    // And the producer, pulling the peer's state back, still sees one entry.
    producer.remote = peer.remote.clone_state();
    let back = producer.pull();
    assert!(back.conflict_asides.is_empty());
    assert_eq!(notes_entries(&producer).len(), 1);
    assert_eq!(producer.files().len(), 1);
}
