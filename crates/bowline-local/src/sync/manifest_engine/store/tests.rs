use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use bowline_core::ids::ContentId;

use super::*;
use crate::sync::manifest_engine::manifest::{
    BlobKey, EntryKind, FileMode, KeyEpoch, WorkspacePath,
};
use crate::workspace::TempWorkspace;

fn store_path(name: &str) -> (TempWorkspace, PathBuf) {
    let workspace = TempWorkspace::new(name).expect("temp workspace");
    let path = workspace.root().join("manifest_engine.sqlite3");
    (workspace, path)
}

fn file_record(seed: u64) -> FileRecord {
    FileRecord {
        kind: EntryKind::File,
        size: seed,
        mode: FileMode::new(0o644),
        symlink_target: None,
        content_id: Some(ContentId::new(format!("cid_{seed}"))),
        blob_key: Some(BlobKey::new(format!("b_{seed}"))),
        key_epoch: Some(KeyEpoch::new(1)),
        fingerprint: StatFingerprint {
            mtime_ns: seed as i64,
            ctime_ns: (seed as i64) + 1,
            // Exercise the u64->i64 bit-cast round-trip with a high-bit value.
            inode: u64::MAX - seed,
            dev: 42,
        },
        hashed_at: Some(1000 + seed as i64),
        verified_at: None,
    }
}

fn commit_of(records: &[(&str, FileRecord)]) -> AncestorCommit {
    let mut upserts = BTreeMap::new();
    for (path, record) in records {
        upserts.insert(WorkspacePath::new(*path), record.clone());
    }
    AncestorCommit {
        upserts,
        removals: BTreeSet::new(),
    }
}

#[test]
fn create_and_reopen_round_trips_files_and_state() {
    let (_workspace, path) = store_path("store-reopen");
    let commit = commit_of(&[
        ("src/main.rs", file_record(11)),
        ("README.md", file_record(6)),
    ]);
    {
        let mut store = ManifestStore::open(&path).expect("open");
        store
            .commit_push_success(&commit, &ManifestKey::new("m_head"), 7)
            .expect("push");
    }

    let store = ManifestStore::open(&path).expect("reopen");
    assert_eq!(store.all_files().expect("files"), commit.upserts);
    let state = store.engine_state().expect("state");
    assert_eq!(state.applied_manifest_key, Some(ManifestKey::new("m_head")));
    assert_eq!(state.last_ref_version, Some(7));
    // A push is a verified observation of the hosted head: the freshness ratchet
    // advances with it, so a device that only ever pushes is still rollback-safe.
    assert_eq!(state.highest_verified_ref_version, Some(7));
    assert_eq!(
        state.highest_verified_manifest_key,
        Some(ManifestKey::new("m_head"))
    );
}

#[test]
fn engine_state_singleton_is_enforced() {
    let (_workspace, path) = store_path("store-singleton");
    let mut store = ManifestStore::open(&path).expect("open");

    store
        .record_highest_verified(3, &ManifestKey::new("m_verified"))
        .expect("verified");
    store
        .commit_push_success(
            &commit_of(&[("a", file_record(1))]),
            &ManifestKey::new("m_a"),
            4,
        )
        .expect("push");

    // Both writers upserted the SAME singleton row, never a second one.
    let count: i64 = store
        .connection
        .query_row("SELECT COUNT(*) FROM engine_state", [], |row| row.get(0))
        .expect("count");
    assert_eq!(count, 1);

    let state = store.engine_state().expect("state");
    // The push to version 4 is a later verified observation than the seeded
    // version-3 ratchet, so the monotonic-max ratchet advances to 4/m_a.
    assert_eq!(state.highest_verified_ref_version, Some(4));
    assert_eq!(
        state.highest_verified_manifest_key,
        Some(ManifestKey::new("m_a"))
    );
    assert_eq!(state.applied_manifest_key, Some(ManifestKey::new("m_a")));

    // The CHECK constraint refuses any non-singleton row.
    assert!(
        store
            .connection
            .execute("INSERT INTO engine_state (singleton) VALUES (2)", [])
            .is_err()
    );
}

#[test]
fn push_success_is_atomic() {
    let (_workspace, path) = store_path("store-push-atomic");
    let mut store = ManifestStore::open(&path).expect("open");
    let base = commit_of(&[("keep", file_record(1)), ("drop", file_record(2))]);
    store
        .commit_push_success(&base, &ManifestKey::new("m_base"), 1)
        .expect("base push");

    // A transaction that writes then fails must leave the ancestor and state
    // exactly as the base push left them (all-or-nothing).
    let result: Result<(), ManifestStoreError> = store.in_transaction(|connection| {
        apply_ancestor(
            connection,
            &AncestorCommit {
                upserts: BTreeMap::from([(WorkspacePath::new("intruder"), file_record(9))]),
                removals: BTreeSet::from([WorkspacePath::new("keep")]),
            },
        )?;
        set_applied(connection, &ManifestKey::new("m_should_roll_back"), 99)?;
        Err(ManifestStoreError::Corrupt {
            field: "test-fault",
        })
    });
    assert!(result.is_err());

    assert_eq!(store.all_files().expect("files"), base.upserts);
    let state = store.engine_state().expect("state");
    assert_eq!(state.applied_manifest_key, Some(ManifestKey::new("m_base")));
    assert_eq!(state.last_ref_version, Some(1));
}

#[test]
fn pull_outcome_commits_rows_and_intents_atomically() {
    let (_workspace, path) = store_path("store-pull-atomic");
    let mut store = ManifestStore::open(&path).expect("open");

    store
        .open_intent(&intent("changed", IntentOperationKind::Install))
        .expect("intent changed");
    store
        .open_intent(&intent("untouched", IntentOperationKind::Delete))
        .expect("intent untouched");

    let mut commit = commit_of(&[("changed", file_record(5))]);
    commit.removals.insert(WorkspacePath::new("stale"));

    store
        .commit_pull_outcome(
            &commit,
            Some((&ManifestKey::new("m_pulled"), 12)),
            Some((&ManifestKey::new("m_pulled"), 12)),
            &[WorkspacePath::new("changed")],
        )
        .expect("pull outcome");

    // Rows applied, applied ref advanced, ratchet advanced, only the named intent
    // cleared.
    assert!(
        store
            .all_files()
            .expect("files")
            .contains_key(&WorkspacePath::new("changed"))
    );
    let state = store.engine_state().expect("state");
    assert_eq!(
        state.applied_manifest_key,
        Some(ManifestKey::new("m_pulled"))
    );
    assert_eq!(state.last_ref_version, Some(12));
    assert_eq!(state.highest_verified_ref_version, Some(12));
    assert_eq!(
        state.highest_verified_manifest_key,
        Some(ManifestKey::new("m_pulled"))
    );

    let remaining = store.pending_intents().expect("intents");
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].path, WorkspacePath::new("untouched"));

    // Rollback path: a failing pull leaves both rows and intents untouched.
    let before_files = store.all_files().expect("files");
    let before_intents = store.pending_intents().expect("intents");
    let result: Result<(), ManifestStoreError> = store.in_transaction(|connection| {
        apply_ancestor(connection, &commit_of(&[("changed", file_record(77))]))?;
        connection.execute("DELETE FROM intents", [])?;
        Err(ManifestStoreError::Corrupt {
            field: "test-fault",
        })
    });
    assert!(result.is_err());
    assert_eq!(store.all_files().expect("files"), before_files);
    assert_eq!(store.pending_intents().expect("intents"), before_intents);
}

#[test]
fn verified_ratchet_is_monotonic_max() {
    let (_workspace, path) = store_path("store-ratchet-monotonic");
    let mut store = ManifestStore::open(&path).expect("open");

    // Advance to version 5, then attempt to record a LOWER version 3: the ratchet
    // must stay at 5 (monotonic-max), so a rolled-back observation can never lower
    // it and defeat rollback protection.
    store
        .record_highest_verified(5, &ManifestKey::new("m_five"))
        .expect("record 5");
    store
        .record_highest_verified(3, &ManifestKey::new("m_three"))
        .expect("record 3 is a no-op");
    let state = store.engine_state().expect("state");
    assert_eq!(state.highest_verified_ref_version, Some(5));
    assert_eq!(
        state.highest_verified_manifest_key,
        Some(ManifestKey::new("m_five"))
    );

    // A strictly higher version advances it.
    store
        .record_highest_verified(9, &ManifestKey::new("m_nine"))
        .expect("record 9");
    let state = store.engine_state().expect("state");
    assert_eq!(state.highest_verified_ref_version, Some(9));
    assert_eq!(
        state.highest_verified_manifest_key,
        Some(ManifestKey::new("m_nine"))
    );
}

#[test]
fn intent_survives_reopen() {
    let (_workspace, path) = store_path("store-intent-reopen");
    let recorded = Intent {
        path: WorkspacePath::new("app/.env"),
        operation_kind: IntentOperationKind::ConflictAside,
        temp_name: Some(".bowline/tmp/app-env-abc".to_string()),
        expected_preimage: Some("{\"identity\":\"preimage\"}".to_string()),
        target_record: Some("{\"target\":\"record\"}".to_string()),
        preserved_preimage: Some(".bowline/quarantine/app-env".to_string()),
        target_manifest_key: Some(ManifestKey::new("m_target")),
        created_at: 1_700_000_000,
    };
    {
        let mut store = ManifestStore::open(&path).expect("open");
        store.open_intent(&recorded).expect("intent");
    }
    let store = ManifestStore::open(&path).expect("reopen");
    let intents = store.pending_intents().expect("intents");
    assert_eq!(intents, vec![recorded]);
}

fn intent(path: &str, operation_kind: IntentOperationKind) -> Intent {
    Intent {
        path: WorkspacePath::new(path),
        operation_kind,
        temp_name: None,
        expected_preimage: None,
        target_record: None,
        preserved_preimage: None,
        target_manifest_key: None,
        created_at: 1,
    }
}
