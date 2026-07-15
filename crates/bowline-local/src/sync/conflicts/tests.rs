use std::{
    sync::mpsc,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use bowline_control_plane::{ControlPlaneTimestamp, ObjectKind, ObjectPointer};

use super::*;

fn temp_state_root(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("bowline-{name}-{nonce}"));
    fs::create_dir_all(&root).unwrap();
    root
}

#[test]
fn validates_bundle_relative_paths() {
    assert!(validate_bundle_relative_path("src/main.rs").is_ok());
    for path in ["", ".", "../escape", "src/../escape", "/abs"] {
        assert!(matches!(
            validate_bundle_relative_path(path),
            Err(ConflictBundleError::UnsafePath(rejected)) if rejected == path
        ));
    }
}

#[test]
fn conflict_bundle_writes_manifest_sides_and_unresolved_paths() {
    let root = temp_state_root("conflict-bundle");
    let record = ConflictRecord::same_path("src/main.rs");
    let bundle = create_conflict_bundle(
        &root,
        record,
        &[ConflictFile {
            relative_path: "src/main.rs".to_string(),
            base: Some(b"base".to_vec()),
            local: Some(b"local".to_vec()),
            remote: Some(b"remote".to_vec()),
        }],
    )
    .expect("bundle");

    assert_eq!(
        fs::read(bundle.root.join("base/src/main.rs")).unwrap(),
        b"base"
    );
    assert_eq!(
        fs::read(bundle.root.join("local/src/main.rs")).unwrap(),
        b"local"
    );
    assert_eq!(
        fs::read(bundle.root.join("remote/src/main.rs")).unwrap(),
        b"remote"
    );
    assert!(bundle.prompt_path.exists());
    assert_eq!(
        unresolved_conflict_paths(&root).unwrap(),
        BTreeSet::from(["src/main.rs".to_string()])
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn conflict_state_manifest_serialization_stays_lowercase() {
    for (wire, state) in [
        ("unresolved", ConflictState::Unresolved),
        ("accepted", ConflictState::Accepted),
        ("rejected", ConflictState::Rejected),
    ] {
        let json = format!(
            r#"{{"id":"conflict_state","conflictKind":"text","occurrenceVersion":1,"paths":["src/main.rs"],"reason":"test","activeView":"local","bundlePath":null,"containsSecrets":false,"state":"{wire}"}}"#
        );
        let record: ConflictRecord = serde_json::from_str(&json).expect("manifest parses");
        assert_eq!(record.state, state);
        let encoded = serde_json::to_vec(&record).expect("manifest encodes");
        let encoded: serde_json::Value =
            serde_json::from_slice(&encoded).expect("encoded manifest parses");
        assert_eq!(encoded["state"], wire);
    }
}

#[test]
fn conflict_occurrence_version_is_required_and_monotonic() {
    let missing_version = r#"{"id":"conflict_state","conflictKind":"text","paths":["src/main.rs"],"reason":"test","activeView":"local","bundlePath":null,"containsSecrets":false,"state":"unresolved"}"#;
    assert!(serde_json::from_str::<ConflictRecord>(missing_version).is_err());
    let zero_version = r#"{"id":"conflict_state","conflictKind":"text","occurrenceVersion":0,"paths":["src/main.rs"],"reason":"test","activeView":"local","bundlePath":null,"containsSecrets":false,"state":"unresolved"}"#;
    assert!(matches!(
        decode_persisted_conflict_record(zero_version.as_bytes()),
        Err(ConflictBundleError::InvalidOccurrenceVersion { .. })
    ));

    let root = temp_state_root("conflict-occurrence-version");
    let mut first = ConflictRecord::same_path("src/main.rs");
    first.base_snapshot_id = Some("snap_base".to_string());
    first.remote_snapshot_id = Some("snap_remote_1".to_string());
    let first_bundle = create_conflict_bundle(&root, first.clone(), &[]).expect("first");
    let duplicate = create_conflict_bundle(&root, first, &[]).expect("duplicate");
    assert_eq!(first_bundle.record.occurrence_version, 1);
    assert_eq!(duplicate.record.occurrence_version, 1);

    let mut next = ConflictRecord::same_path("src/main.rs");
    next.base_snapshot_id = Some("snap_base".to_string());
    next.remote_snapshot_id = Some("snap_remote_2".to_string());
    let next = create_conflict_bundle(&root, next, &[]).expect("next");
    assert_eq!(next.record.occurrence_version, 2);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn recurring_occurrence_removes_side_bytes_that_are_now_absent() {
    let root = temp_state_root("conflict-recurring-absent-side");
    let mut first = ConflictRecord::same_path("src/main.rs");
    first.base_snapshot_id = Some("snap_base".to_string());
    first.remote_snapshot_id = Some("snap_remote_1".to_string());
    create_conflict_bundle(
        &root,
        first,
        &[ConflictFile {
            relative_path: "src/main.rs".to_string(),
            base: Some(b"base".to_vec()),
            local: Some(b"old local".to_vec()),
            remote: Some(b"old remote".to_vec()),
        }],
    )
    .expect("first occurrence");

    let mut second = ConflictRecord::same_path("src/main.rs");
    second.base_snapshot_id = Some("snap_base".to_string());
    second.remote_snapshot_id = Some("snap_remote_2".to_string());
    let second = create_conflict_bundle(
        &root,
        second,
        &[ConflictFile {
            relative_path: "src/main.rs".to_string(),
            base: Some(b"base".to_vec()),
            local: None,
            remote: Some(b"new remote".to_vec()),
        }],
    )
    .expect("second occurrence");

    assert_eq!(second.record.occurrence_version, 2);
    let recovered = load_conflict_files(&second.record).expect("recover sides");
    assert_eq!(recovered[0].local, None);
    assert_eq!(
        recovered[0].remote.as_deref(),
        Some(b"new remote".as_slice())
    );
    assert!(!second.root.join("local/src/main.rs").exists());
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn marker_and_new_occurrence_serialize_without_overwriting_newer_state() {
    let root = temp_state_root("conflict-occurrence-race");
    let mut first = ConflictRecord::same_path("src/main.rs");
    first.base_snapshot_id = Some("snap_base".to_string());
    first.remote_snapshot_id = Some("snap_remote_1".to_string());
    let first = create_conflict_bundle(&root, first, &[]).expect("first");
    let conflict_id = first.record.id.clone();

    let marker_root = root.clone();
    let marker_id = conflict_id.clone();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let marker = thread::spawn(move || {
        mark_conflict_occurrence_reconciled_inner(
            &marker_root,
            &marker_id,
            1,
            ConflictState::Unresolved,
            "2026-07-13T10:00:00Z",
            || {
                entered_tx.send(()).expect("announce marker lock");
                release_rx.recv().expect("release marker write");
            },
        )
    });
    entered_rx.recv().expect("marker acquired lock");

    let creator_root = root.clone();
    let (created_tx, created_rx) = mpsc::channel();
    let creator = thread::spawn(move || {
        let mut next = ConflictRecord::same_path("src/main.rs");
        next.base_snapshot_id = Some("snap_base".to_string());
        next.remote_snapshot_id = Some("snap_remote_2".to_string());
        let result = create_conflict_bundle(&creator_root, next, &[]);
        created_tx.send(()).expect("announce creator completion");
        result
    });
    assert!(created_rx.recv_timeout(Duration::from_millis(100)).is_err());
    release_tx.send(()).expect("release marker");
    assert!(marker.join().expect("marker thread").expect("marker"));
    created_rx.recv().expect("creator completed");
    assert_eq!(
        creator
            .join()
            .expect("creator thread")
            .expect("creator")
            .record
            .occurrence_version,
        2
    );

    let current = load_conflict_record(&root, &conflict_id)
        .expect("load")
        .expect("current");
    assert_eq!(current.occurrence_version, 2);
    assert_eq!(current.remote_snapshot_id.as_deref(), Some("snap_remote_2"));
    assert_eq!(current.remote_conflict_published_at, None);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn exact_occurrence_guards_pointer_state_and_reconcile_markers() {
    let root = temp_state_root("conflict-exact-occurrence");
    let mut first = ConflictRecord::same_path("src/main.rs");
    first.base_snapshot_id = Some("snap_base".to_string());
    first.remote_snapshot_id = Some("snap_remote_1".to_string());
    let first = create_conflict_bundle(&root, first, &[]).expect("first");
    let mut next = ConflictRecord::same_path("src/main.rs");
    next.base_snapshot_id = Some("snap_base".to_string());
    next.remote_snapshot_id = Some("snap_remote_2".to_string());
    let next = create_conflict_bundle(&root, next, &[]).expect("next");
    let pointer = ObjectPointer {
        object_key: "conflicts_cb_00112233445566778899aabb".to_string(),
        content_id: bowline_core::ids::ContentId::new(first.record.id.clone()),
        byte_len: 128,
        hash: "b3_0000000000000000000000000000000000000000000000000000000000000000".to_string(),
        key_epoch: 1,
        kind: ObjectKind::ConflictBundle,
        created_at: ControlPlaneTimestamp { tick: 42 },
    };
    assert!(!set_conflict_bundle_object(&first.record, pointer).expect("stale pointer"));
    assert!(
        !transition_conflict_occurrence_state(
            &next.root,
            &next.record.id,
            1,
            ConflictState::Accepted,
            "2026-07-13T10:00:00Z",
        )
        .expect("stale transition")
    );
    assert!(
        !mark_conflict_occurrence_reconciled(
            &root,
            &next.record.id,
            1,
            ConflictState::Unresolved,
            "2026-07-13T10:00:00Z",
        )
        .expect("stale marker")
    );
    let current = load_conflict_record(&root, &next.record.id)
        .expect("load")
        .expect("current");
    assert_eq!(current.occurrence_version, 2);
    assert_eq!(current.state, ConflictState::Unresolved);
    assert_eq!(current.bundle_object, None);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn conflict_record_omits_absent_bundle_object_and_round_trips_present_pointer() {
    let mut record = ConflictRecord::same_path("src/main.rs");
    let encoded = serde_json::to_value(&record).expect("record encodes");
    assert!(encoded.get("bundleObject").is_none());

    let pointer = ObjectPointer {
        object_key: "conflicts_cb_00112233445566778899aabb".to_string(),
        content_id: bowline_core::ids::ContentId::new(record.id.clone()),
        byte_len: 128,
        hash: "b3_0000000000000000000000000000000000000000000000000000000000000000".to_string(),
        key_epoch: 1,
        kind: ObjectKind::ConflictBundle,
        created_at: ControlPlaneTimestamp { tick: 42 },
    };
    record.bundle_object = Some(pointer.clone());
    let encoded = serde_json::to_vec(&record).expect("record encodes");
    let decoded: ConflictRecord = serde_json::from_slice(&encoded).expect("record decodes");
    assert_eq!(decoded.bundle_object, Some(pointer));
}

#[test]
fn atomic_private_write_sets_owner_only_permissions() {
    let root = temp_state_root("conflict-private");
    let path = root.join("secret.txt");
    atomic_write_private(&path, b"placeholder").expect("write");

    assert_eq!(fs::read(&path).unwrap(), b"placeholder");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    fs::remove_dir_all(root).unwrap();
}
