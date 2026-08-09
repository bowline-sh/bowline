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
            .commit_push_success(&commit, &ManifestKey::new("m_head"), 7, &BTreeSet::new())
            .expect("push");
    }

    let store = ManifestStore::open(&path).expect("reopen");
    assert_eq!(store.all_files().expect("files"), commit.upserts);
    let state = store.engine_state().expect("state");
    assert_eq!(state.applied_manifest_key, Some(ManifestKey::new("m_head")));
    assert_eq!(state.last_ref_version, Some(7));
    assert_eq!(state.materialization_revision.get(), 1);
    // A push is a verified observation of the hosted head: the freshness ratchet
    // advances with it, so a device that only ever pushes is still rollback-safe.
    assert_eq!(state.highest_verified_ref_version, Some(7));
    assert_eq!(
        state.highest_verified_manifest_key,
        Some(ManifestKey::new("m_head"))
    );
}

#[test]
fn scoped_file_reads_include_exact_paths_and_descendants_only() {
    let (_workspace, path) = store_path("store-scoped-files");
    let commit = commit_of(&[
        ("a", file_record(1)),
        ("a/child", file_record(2)),
        ("a0/not-a-child", file_record(3)),
        ("ab/not-a-child", file_record(4)),
        ("other", file_record(5)),
    ]);
    let mut store = ManifestStore::open(&path).expect("open");
    store
        .commit_push_success(&commit, &ManifestKey::new("m_head"), 1, &BTreeSet::new())
        .expect("push");

    let files = store
        .files_in_scopes(&BTreeSet::from([WorkspacePath::new("a")]))
        .expect("scoped files");

    assert_eq!(
        files.keys().cloned().collect::<Vec<_>>(),
        vec![WorkspacePath::new("a"), WorkspacePath::new("a/child")]
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
            &BTreeSet::new(),
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
    assert_eq!(state.materialization_revision.get(), 1);

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
        .commit_push_success(&base, &ManifestKey::new("m_base"), 1, &BTreeSet::new())
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
    assert_eq!(state.materialization_revision.get(), 1);
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
            &BTreeSet::new(),
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
    assert_eq!(state.materialization_revision.get(), 1);
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
fn applied_ref_pair_and_materialization_revision_are_structural() {
    let (_workspace, path) = store_path("store-applied-frontier");
    let mut store = ManifestStore::open(&path).expect("open");

    assert_eq!(
        store
            .engine_state()
            .expect("genesis")
            .applied_ref()
            .expect("pair"),
        super::super::EngineRef::Genesis
    );
    assert!(
        store
            .connection
            .execute(
                "INSERT INTO engine_state (singleton, applied_manifest_key) VALUES (1, 'partial')",
                [],
            )
            .is_err(),
        "a key without a version is not a representable durable frontier"
    );

    store
        .commit_push_success(
            &commit_of(&[("one", file_record(1))]),
            &ManifestKey::new("m_one"),
            1,
            &BTreeSet::new(),
        )
        .expect("first frontier");
    store
        .record_ref_advance(&ManifestKey::new("m_one"), 2)
        .expect("ABA version frontier");
    let state = store.engine_state().expect("state");
    assert_eq!(state.materialization_revision.get(), 2);
    assert_eq!(
        state.applied_ref().expect("exact pair"),
        super::super::EngineRef::Head(super::super::RefObservation {
            version: 2,
            manifest_key: ManifestKey::new("m_one"),
        })
    );
}

#[test]
fn materialization_revision_overflow_rolls_back_the_whole_frontier() {
    let (_workspace, path) = store_path("store-materialization-overflow");
    let mut store = ManifestStore::open(&path).expect("open");
    store
        .commit_push_success(
            &commit_of(&[("base", file_record(1))]),
            &ManifestKey::new("m_base"),
            1,
            &BTreeSet::new(),
        )
        .expect("base frontier");
    store
        .connection
        .execute(
            "UPDATE engine_state SET materialization_revision = ?1 WHERE singleton = 1",
            [i64::MAX],
        )
        .expect("seed finite domain edge");
    let before_files = store.all_files().expect("files");

    let error = store
        .commit_push_success(
            &commit_of(&[("new", file_record(2))]),
            &ManifestKey::new("m_new"),
            2,
            &BTreeSet::new(),
        )
        .expect_err("overflow is terminal");
    assert!(matches!(
        error,
        ManifestStoreError::ValueOutOfRange {
            field: "materialization_revision"
        }
    ));
    assert_eq!(store.all_files().expect("files"), before_files);
    let state = store.engine_state().expect("state");
    assert_eq!(state.applied_manifest_key, Some(ManifestKey::new("m_base")));
    assert_eq!(state.last_ref_version, Some(1));
    assert_eq!(state.materialization_revision.get(), i64::MAX as u64);
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

#[test]
fn opening_the_store_raises_the_ratchet_floor_to_the_applied_head() {
    let (_workspace, path) = store_path("store-ratchet-floor");
    let applied = ManifestKey::new("m_applied");
    {
        let store = ManifestStore::open(&path).expect("open");
        // The shape a lost `synchronous=NORMAL` transaction can leave behind: the
        // applied head is above the ratchet, so a hosted rollback to a version
        // this device already applied would pass `enforce_freshness`.
        store
            .in_transaction(|connection| set_applied(connection, &applied, 7))
            .expect("seed applied head");
        let state = store.engine_state().expect("state");
        assert_eq!(state.highest_verified_ref_version, None);
    }

    let store = ManifestStore::open(&path).expect("reopen");
    let state = store.engine_state().expect("state");
    assert_eq!(state.highest_verified_ref_version, Some(7));
    assert_eq!(state.highest_verified_manifest_key, Some(applied));
}

#[test]
fn opening_the_store_never_lowers_the_ratchet() {
    let (_workspace, path) = store_path("store-ratchet-no-lower");
    let high = ManifestKey::new("m_high");
    {
        let mut store = ManifestStore::open(&path).expect("open");
        store.record_highest_verified(9, &high).expect("ratchet");
        store
            .in_transaction(|connection| set_applied(connection, &ManifestKey::new("m_low"), 4))
            .expect("seed a lower applied head");
    }

    let store = ManifestStore::open(&path).expect("reopen");
    let state = store.engine_state().expect("state");
    assert_eq!(state.highest_verified_ref_version, Some(9));
    assert_eq!(state.highest_verified_manifest_key, Some(high));
}

#[test]
fn the_blob_ledger_is_scoped_to_the_current_key_epoch() {
    let (_workspace, path) = store_path("store-blob-ledger");
    let mut store = ManifestStore::open(&path).expect("open");
    let content = ContentId::new("cid_ledger");
    let blob = SealedBlob {
        blob_key: BlobKey::new("b_ledger"),
        key_epoch: KeyEpoch::new(1),
        byte_len: 12,
    };
    store
        .record_sealed_blobs(&BTreeMap::from([(content.clone(), blob.clone())]))
        .expect("record");

    assert_eq!(
        store.sealed_blob(&content, KeyEpoch::new(1)).expect("read"),
        Some(blob)
    );
    assert_eq!(
        store.sealed_blob(&content, KeyEpoch::new(2)).expect("read"),
        None,
        "a rotated key must re-seal rather than reuse an object from the old epoch"
    );
}

/// Manual release-mode scale matrix for the SQLite half of invariant C2.
///
/// Run with:
/// `cargo test -p bowline-local --release --lib scoped_ancestor_scale_matrix -- --ignored --nocapture`
///
/// It deliberately seeds rows directly: materializing one million files would
/// measure filesystem fixture creation rather than the range query under test.
#[test]
#[ignore = "manual 10k/100k/1m scale benchmark"]
fn scoped_ancestor_scale_matrix() {
    use std::process::Command;
    use std::time::Instant;

    const SAMPLES: usize = 20;
    const WORKSPACE_SIZES: [usize; 3] = [10_000, 100_000, 1_000_000];
    const CHANGE_COUNTS: [usize; 3] = [1, 10, 100];
    let (_workspace, path) = store_path("store-scope-scale");
    let store = ManifestStore::open(&path).expect("open");

    let mut seeded = 0;
    for workspace_size in WORKSPACE_SIZES {
        store
            .connection
            .execute_batch("BEGIN IMMEDIATE")
            .expect("begin");
        {
            let mut insert = store
                .connection
                .prepare(
                    "INSERT INTO files(path, kind, size, mode, mtime_ns, ctime_ns, inode, dev) \
                     VALUES (?1, 0, 7, 420, 1, 1, ?2, 1)",
                )
                .expect("prepare");
            for index in seeded..workspace_size {
                insert
                    .execute(params![format!("f{index:07}.dat"), index as i64])
                    .expect("insert");
            }
        }
        store.connection.execute_batch("COMMIT").expect("commit");
        seeded = workspace_size;
        for changes in CHANGE_COUNTS {
            let scopes = (0..changes)
                .map(|index| {
                    let row = index * workspace_size / changes;
                    WorkspacePath::new(format!("f{row:07}.dat"))
                })
                .collect::<BTreeSet<_>>();
            let mut micros = Vec::with_capacity(SAMPLES);
            let mut rows_read = 0;
            let mut peak_rss_kib = 0;
            for _ in 0..SAMPLES {
                let started = Instant::now();
                let rows = store.files_in_scopes(&scopes).expect("scoped rows");
                micros.push(started.elapsed().as_micros() as u64);
                rows_read = rows.len();
                peak_rss_kib = peak_rss_kib.max(current_rss_kib());
            }
            micros.sort_unstable();
            let p95_micros = micros[(SAMPLES * 95).div_ceil(100) - 1];
            println!(
                "{{\"workspaceEntries\":{workspace_size},\"changedPaths\":{changes},\
                 \"p95Micros\":{p95_micros},\"rowsRead\":{rows_read},\
                 \"contentOpens\":0,\"peakObservedRssKiB\":{peak_rss_kib}}}"
            );
            assert_eq!(rows_read, changes);
            assert!(p95_micros < 100_000, "scoped ancestor p95 exceeded 100 ms");
            assert!(peak_rss_kib < 1_048_576, "benchmark RSS exceeded 1 GiB");
        }
    }

    fn current_rss_kib() -> u64 {
        let output = Command::new("ps")
            .args(["-o", "rss=", "-p", &std::process::id().to_string()])
            .output()
            .expect("ps");
        String::from_utf8(output.stdout)
            .expect("utf8")
            .trim()
            .parse()
            .expect("rss")
    }
}
