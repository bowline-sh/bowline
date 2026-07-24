use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use bowline_control_plane::{
    ControlPlaneTimestamp, FakeControlPlaneClient, ObjectKind, WorkspaceRef,
    WorkspaceRefStreamConnectionState, WorkspaceRefStreamEvent, workspace_ref_stream_shutdown_pair,
};
use bowline_core::ids::{ContentId, DeviceId, SnapshotId, WorkspaceId};
use bowline_local::sync::manifest_engine::{
    BlobReaderUpload, BlobUpload, CasOutcome, EngineEvent, KeyEpoch, ManifestUpload, RemoteObjects,
    RemoteRef, physical_blob_key, physical_manifest_key,
};
use bowline_storage::{
    ObjectKey, ObjectKind as StorageObjectKind, ObjectMetadata, RetentionState, stable_object_hash,
};

use super::{
    CommittedMetadataExpectation, DrainOutcome, ManifestTransport, ReconnectDelay,
    RefChangeSubscription, RefObserverFailureStage, RefObserverHealthHandle, RefObserverState,
    StreamAttempt, StreamStarter, drain_stream, validate_committed_metadata,
};

const WORKSPACE: &str = "ws_manifest_transport";
const DEVICE: &str = "device_manifest_transport";
const CONTENT_ID: &str = "cid_manifest_transport";

fn transport(
    control_plane: &FakeControlPlaneClient,
) -> ManifestTransport<'_, FakeControlPlaneClient> {
    ManifestTransport::new(
        control_plane,
        WorkspaceId::new(WORKSPACE),
        DeviceId::new(DEVICE),
    )
}

// ---- signed-URL test servers (mirror crates/bowline-control-plane/src/transfer/tests.rs) ----

fn sequenced_put_server(responses: &[(&str, &[u8])]) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("test listener");
    let address = listener.local_addr().expect("listener address");
    let planned: Vec<(String, Vec<u8>)> = responses
        .iter()
        .map(|(status, body)| ((*status).to_string(), (*body).to_vec()))
        .collect();
    thread::spawn(move || {
        for (status, body) in planned {
            let (mut stream, _) = listener.accept().expect("accept request");
            let mut request = [0; 4096];
            let _ = stream.read(&mut request).expect("read request");
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\n\r\n",
                body.len()
            )
            .expect("write headers");
            stream.write_all(&body).expect("write body");
        }
    });
    format!("http://{address}/object")
}

fn owned_signed_url_response(status: &str, body: Arc<Vec<u8>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("test listener");
    let address = listener.local_addr().expect("listener address");
    let status = status.to_string();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept request");
        let mut request = [0; 1024];
        let _ = stream.read(&mut request).expect("read request");
        write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .expect("write headers");
        stream.write_all(&body).expect("write body");
    });
    format!("http://{address}/object")
}

fn ready_workspace() -> FakeControlPlaneClient {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace(WORKSPACE);
    control_plane
}

fn content_id() -> ContentId {
    ContentId::new(CONTENT_ID)
}

// ---- object upload / download ------------------------------------------------

#[test]
fn put_blob_commits_metadata_before_returning() {
    let control_plane = ready_workspace();
    let sealed = b"sealed-blob-payload".to_vec();
    let key = physical_blob_key(&sealed);
    control_plane.set_signed_url_override("upload", sequenced_put_server(&[("200 OK", b"")]));

    let content_id = content_id();
    transport(&control_plane)
        .put_blob(BlobUpload {
            key: &key,
            content_id: &content_id,
            key_epoch: KeyEpoch::new(1),
            sealed: &sealed,
        })
        .expect("put_blob succeeds");

    // Success means the hosted metadata commit landed and read back clean.
    let pointers = control_plane.object_pointers(WORKSPACE);
    let pointer = pointers
        .iter()
        .find(|pointer| pointer.object_key == key.as_str())
        .expect("committed object pointer for the uploaded blob");
    assert_eq!(pointer.byte_len, sealed.len() as u64);
    assert_eq!(pointer.hash, stable_object_hash(&sealed));
    assert_eq!(pointer.key_epoch, 1);
    assert_eq!(pointer.kind, ObjectKind::Blob);
    assert_eq!(pointer.content_id, content_id);
}

#[test]
fn put_then_get_blob_round_trips_sealed_bytes() {
    let control_plane = ready_workspace();
    let sealed = b"sealed-blob-round-trip".to_vec();
    let key = physical_blob_key(&sealed);
    control_plane.set_signed_url_override("upload", sequenced_put_server(&[("200 OK", b"")]));
    control_plane.set_signed_url_override(
        "download",
        owned_signed_url_response("200 OK", Arc::new(sealed.clone())),
    );

    let content_id = content_id();
    let transport = transport(&control_plane);
    transport
        .put_blob(BlobUpload {
            key: &key,
            content_id: &content_id,
            key_epoch: KeyEpoch::new(1),
            sealed: &sealed,
        })
        .expect("put_blob succeeds");

    let fetched = transport.get_blob(&key).expect("get_blob succeeds");
    assert_eq!(fetched, sealed);
}

#[test]
fn put_then_get_manifest_round_trips_sealed_bytes() {
    let control_plane = ready_workspace();
    let sealed = b"sealed-manifest-round-trip".to_vec();
    let key = physical_manifest_key(&sealed);
    control_plane.set_signed_url_override("upload", sequenced_put_server(&[("200 OK", b"")]));
    control_plane.set_signed_url_override(
        "download",
        owned_signed_url_response("200 OK", Arc::new(sealed.clone())),
    );

    let content_id = content_id();
    let transport = transport(&control_plane);
    transport
        .put_manifest(ManifestUpload {
            key: &key,
            content_id: &content_id,
            key_epoch: KeyEpoch::new(1),
            sealed: &sealed,
        })
        .expect("put_manifest succeeds");

    let pointers = control_plane.object_pointers(WORKSPACE);
    assert!(
        pointers
            .iter()
            .any(|pointer| pointer.object_key == key.as_str()
                && pointer.kind == ObjectKind::Manifest)
    );

    let fetched = transport.get_manifest(&key).expect("get_manifest succeeds");
    assert_eq!(fetched, sealed);
}

#[test]
fn put_blob_reader_streams_and_commits() {
    let control_plane = ready_workspace();
    let sealed = b"sealed-streamed-blob-bytes".to_vec();
    let key = physical_blob_key(&sealed);
    let spool = temp_spool(&sealed);
    control_plane.set_signed_url_override("upload", sequenced_put_server(&[("200 OK", b"")]));

    let content_id = content_id();
    let result = transport(&control_plane).put_blob_reader(BlobReaderUpload {
        key: &key,
        content_id: &content_id,
        key_epoch: KeyEpoch::new(1),
        spool_path: &spool,
        byte_len: sealed.len() as u64,
    });
    let _ = std::fs::remove_file(&spool);
    result.expect("put_blob_reader succeeds");

    let pointers = control_plane.object_pointers(WORKSPACE);
    let pointer = pointers
        .iter()
        .find(|pointer| pointer.object_key == key.as_str())
        .expect("committed pointer for the streamed blob");
    assert_eq!(pointer.byte_len, sealed.len() as u64);
    assert_eq!(pointer.hash, stable_object_hash(&sealed));
    assert_eq!(pointer.kind, ObjectKind::Blob);
}

// ---- committed metadata fails closed ----------------------------------------

#[test]
fn committed_metadata_validation_fails_closed_on_mismatch() {
    let sealed = b"sealed-commit-validation".to_vec();
    let key = physical_blob_key(&sealed);
    let hash = stable_object_hash(&sealed);
    let byte_len = sealed.len() as u64;
    let epoch = KeyEpoch::new(1);

    let blob_expectation =
        |committed: &ObjectMetadata, key: &str, expected_byte_len: u64| -> Result<(), String> {
            validate_committed_metadata(CommittedMetadataExpectation {
                key_prefix: ObjectKey::BLOB_PREFIX,
                key,
                expected_hash: &hash,
                expected_byte_len,
                expected_key_epoch: epoch,
                committed,
            })
            .map_err(|error| error.to_string())
        };

    // A faithful commit response passes.
    let good = head_metadata(key.as_str(), &hash, byte_len, 1);
    blob_expectation(&good, key.as_str(), byte_len).expect("matching commit response passes");

    // Every tampered dimension fails closed.
    let wrong_len = head_metadata(key.as_str(), &hash, byte_len + 1, 1);
    assert!(blob_expectation(&wrong_len, key.as_str(), byte_len).is_err());

    let wrong_epoch = head_metadata(key.as_str(), &hash, byte_len, 2);
    assert!(blob_expectation(&wrong_epoch, key.as_str(), byte_len).is_err());

    let other_hash = stable_object_hash(b"other");
    let wrong_hash = head_metadata(key.as_str(), &other_hash, byte_len, 1);
    assert!(blob_expectation(&wrong_hash, key.as_str(), byte_len).is_err());

    let wrong_key = physical_blob_key(b"a-different-object");
    let mismatched_key = head_metadata(wrong_key.as_str(), &hash, byte_len, 1);
    assert!(blob_expectation(&mismatched_key, wrong_key.as_str(), byte_len).is_err());
}

// ---- CAS outcomes ------------------------------------------------------------

#[test]
fn compare_and_swap_advances_from_genesis() {
    let control_plane = ready_workspace();
    let manifest_key = physical_manifest_key(b"genesis-manifest");
    let transport = transport(&control_plane);

    match transport
        .compare_and_swap(None, &manifest_key)
        .expect("cas succeeds")
    {
        CasOutcome::Advanced(observation) => {
            assert_eq!(observation.version, 1);
            assert_eq!(observation.manifest_key, manifest_key);
        }
        other => panic!("expected advance, got {other:?}"),
    }

    // The advanced head now reads back as a real manifest observation.
    let observed = transport.read_ref().expect("read_ref succeeds");
    let observation = observed.expect("a real head after advancing");
    assert_eq!(observation.version, 1);
    assert_eq!(observation.manifest_key, manifest_key);
}

#[test]
fn compare_and_swap_seeds_missing_workspace_ref_then_advances() {
    // Production never seeds the refs row at setup — the first genesis CAS must
    // create the headless ref and then advance it.
    let control_plane = FakeControlPlaneClient::default();
    let manifest_key = physical_manifest_key(b"first-push-manifest");
    let transport = transport(&control_plane);

    match transport
        .compare_and_swap(None, &manifest_key)
        .expect("first push seeds the ref then advances")
    {
        CasOutcome::Advanced(observation) => {
            assert_eq!(observation.version, 1);
            assert_eq!(observation.manifest_key, manifest_key);
        }
        other => panic!("expected advance after seed, got {other:?}"),
    }

    let observed = transport.read_ref().expect("read_ref succeeds");
    let observation = observed.expect("a real head after first push");
    assert_eq!(observation.version, 1);
    assert_eq!(observation.manifest_key, manifest_key);
}

#[test]
fn compare_and_swap_maps_stale_ref_to_lost() {
    let control_plane = ready_workspace();
    let current_key = physical_manifest_key(b"winning-manifest");
    let current = WorkspaceRef {
        workspace_id: WorkspaceId::new(WORKSPACE),
        version: 7,
        snapshot_id: Some(SnapshotId::new(current_key.as_str())),
        updated_at: ControlPlaneTimestamp { tick: 0 },
        updated_by_device_id: None,
    };
    control_plane.make_next_workspace_ref_cas_stale_for_harness(WORKSPACE, current);

    let losing_key = physical_manifest_key(b"losing-manifest");
    match transport(&control_plane)
        .compare_and_swap(Some(0), &losing_key)
        .expect("cas returns a typed outcome")
    {
        CasOutcome::Lost(observation) => {
            assert_eq!(observation.version, 7);
            assert_eq!(observation.manifest_key, current_key);
        }
        other => panic!("expected lost, got {other:?}"),
    }
}

#[test]
fn compare_and_swap_tolerates_a_headless_genesis_lost_ref() {
    // A CAS-lost response should never carry a headless (version-0 genesis) ref
    // under the corrected contract: a genesis loser receives the winner's real
    // head. If the hosted service ever returned one anyway, the transport must
    // fail closed with a typed error rather than panic or fabricate a key.
    let control_plane = ready_workspace();
    let headless_genesis = WorkspaceRef {
        workspace_id: WorkspaceId::new(WORKSPACE),
        version: 0,
        snapshot_id: None,
        updated_at: ControlPlaneTimestamp { tick: 0 },
        updated_by_device_id: None,
    };
    control_plane.make_next_workspace_ref_cas_stale_for_harness(WORKSPACE, headless_genesis);

    let manifest_key = physical_manifest_key(b"candidate-manifest");
    let error = transport(&control_plane)
        .compare_and_swap(None, &manifest_key)
        .expect_err("a headless lost ref fails closed rather than misleading");
    assert!(
        error.to_string().contains("manifest-backed head"),
        "got: {error}"
    );
}

#[test]
fn compare_and_swap_maps_transport_failure_to_ambiguous() {
    let control_plane = ready_workspace();
    control_plane.set_offline(true);
    let manifest_key = physical_manifest_key(b"ambiguous-manifest");

    let outcome = transport(&control_plane)
        .compare_and_swap(Some(0), &manifest_key)
        .expect("a lost ack is a typed ambiguous outcome, not an error");
    assert!(matches!(outcome, CasOutcome::Ambiguous));
}

// ---- read_ref genesis mapping ------------------------------------------------

#[test]
fn read_ref_treats_genesis_as_no_head() {
    let control_plane = ready_workspace();
    // A freshly established workspace seeds a headless version-0 genesis ref; it
    // must read as "no head yet".
    let observed = transport(&control_plane)
        .read_ref()
        .expect("read_ref succeeds");
    assert!(observed.is_none());
}

// ---- ref-change subscription -------------------------------------------------

#[test]
fn ref_subscription_emits_ref_changed_and_reconnects() {
    let (events_tx, events_rx) = mpsc::channel();
    let starter_calls = Arc::new(AtomicUsize::new(0));
    let calls = Arc::clone(&starter_calls);

    let starter: StreamStarter = Box::new(move |stream_tx| {
        calls.fetch_add(1, Ordering::SeqCst);
        let (shutdown, _cancellation) = workspace_ref_stream_shutdown_pair();
        // One wake value per attempt, then the stream ends (sender drops), which
        // the bridge treats as a disconnect and reconnects.
        let worker = thread::Builder::new()
            .name("test-ref-stream".to_string())
            .spawn(move || {
                let _ = stream_tx.send(WorkspaceRefStreamEvent::Ref(Ok(None)));
            })
            .expect("test stream thread");
        Ok(StreamAttempt { shutdown, worker })
    });
    let delay: ReconnectDelay = Arc::new(|_| Duration::from_millis(2));

    let subscription = RefChangeSubscription::spawn_with_starter(starter, events_tx, delay);

    for _ in 0..3 {
        assert!(matches!(
            events_rx.recv_timeout(Duration::from_secs(2)),
            Ok(EngineEvent::RefChanged)
        ));
    }
    drop(subscription);
    assert!(starter_calls.load(Ordering::SeqCst) >= 2);
}

#[test]
fn ref_observer_becomes_live_only_after_initial_value() {
    let (stream_tx, stream_rx) = mpsc::channel();
    let (events_tx, events_rx) = mpsc::channel();
    let shutdown = Arc::new(AtomicBool::new(false));
    let worker_shutdown = Arc::clone(&shutdown);
    let health = RefObserverHealthHandle::new();
    let worker_health = health.clone();
    let worker = thread::spawn(move || {
        drain_stream(
            &stream_rx,
            &events_tx,
            &worker_shutdown,
            &worker_health,
            Duration::from_secs(1),
        )
    });

    assert_eq!(health.current().state, RefObserverState::Connecting);
    stream_tx
        .send(WorkspaceRefStreamEvent::ConnectionState(
            WorkspaceRefStreamConnectionState::Connecting,
        ))
        .expect("initial websocket connection starts");
    stream_tx
        .send(WorkspaceRefStreamEvent::ConnectionState(
            WorkspaceRefStreamConnectionState::Connected,
        ))
        .expect("websocket connected");
    stream_tx
        .send(WorkspaceRefStreamEvent::Ref(Ok(None)))
        .expect("initial value");
    assert!(matches!(
        events_rx.recv_timeout(Duration::from_secs(1)),
        Ok(EngineEvent::RefChanged)
    ));
    assert_eq!(health.current().state, RefObserverState::Live);

    shutdown.store(true, Ordering::SeqCst);
    assert!(matches!(
        worker.join().expect("drain worker"),
        DrainOutcome::DriverGone
    ));
}

#[test]
fn live_ref_observer_carries_a_verified_real_head_after_initial_authority() {
    let (stream_tx, stream_rx) = mpsc::channel();
    let (events_tx, events_rx) = mpsc::channel();
    let shutdown = Arc::new(AtomicBool::new(false));
    let worker_shutdown = Arc::clone(&shutdown);
    let health = RefObserverHealthHandle::new();
    let worker_health = health.clone();
    let manifest_key = physical_manifest_key(b"reactive-head");
    let pushed_ref = WorkspaceRef {
        workspace_id: WorkspaceId::new(WORKSPACE),
        version: 9,
        snapshot_id: Some(SnapshotId::new(manifest_key.as_str())),
        updated_at: ControlPlaneTimestamp { tick: 0 },
        updated_by_device_id: None,
    };
    let worker = thread::spawn(move || {
        drain_stream(
            &stream_rx,
            &events_tx,
            &worker_shutdown,
            &worker_health,
            Duration::from_secs(1),
        )
    });

    stream_tx
        .send(WorkspaceRefStreamEvent::Ref(Ok(None)))
        .expect("initial value");
    assert!(matches!(
        events_rx.recv_timeout(Duration::from_secs(1)),
        Ok(EngineEvent::RefChanged)
    ));
    stream_tx
        .send(WorkspaceRefStreamEvent::Ref(Ok(Some(pushed_ref))))
        .expect("steady-state value");
    assert_eq!(
        events_rx.recv_timeout(Duration::from_secs(1)),
        Ok(EngineEvent::RefObserved(
            bowline_local::sync::manifest_engine::RefObservation {
                version: 9,
                manifest_key,
            }
        ))
    );

    shutdown.store(true, Ordering::SeqCst);
    assert!(matches!(
        worker.join().expect("drain worker"),
        DrainOutcome::DriverGone
    ));
}

#[test]
fn ref_observer_times_out_without_initial_value() {
    let (_stream_tx, stream_rx) = mpsc::channel();
    let (events_tx, _events_rx) = mpsc::channel();
    let shutdown = AtomicBool::new(false);
    let health = RefObserverHealthHandle::new();

    let outcome = drain_stream(
        &stream_rx,
        &events_tx,
        &shutdown,
        &health,
        Duration::from_millis(5),
    );

    assert!(matches!(
        outcome,
        DrainOutcome::Reconnect {
            received_value: false,
            failure: super::RefObserverFailure {
                stage: RefObserverFailureStage::InitialValue,
                ..
            },
        }
    ));
    assert_eq!(health.current().state, RefObserverState::Connecting);
}

#[test]
fn websocket_reconnect_keeps_subscription_and_requires_a_fresh_value() {
    let (stream_tx, stream_rx) = mpsc::channel();
    let (events_tx, events_rx) = mpsc::channel();
    let shutdown = Arc::new(AtomicBool::new(false));
    let worker_shutdown = Arc::clone(&shutdown);
    let health = RefObserverHealthHandle::new();
    let worker_health = health.clone();

    stream_tx
        .send(WorkspaceRefStreamEvent::ConnectionState(
            WorkspaceRefStreamConnectionState::Connected,
        ))
        .expect("initial websocket connection");
    stream_tx
        .send(WorkspaceRefStreamEvent::Ref(Ok(None)))
        .expect("initial subscription value");
    let worker = thread::spawn(move || {
        drain_stream(
            &stream_rx,
            &events_tx,
            &worker_shutdown,
            &worker_health,
            Duration::from_secs(1),
        )
    });
    assert!(matches!(
        events_rx.recv_timeout(Duration::from_secs(1)),
        Ok(EngineEvent::RefChanged)
    ));

    stream_tx
        .send(WorkspaceRefStreamEvent::ConnectionState(
            WorkspaceRefStreamConnectionState::Connecting,
        ))
        .expect("websocket reconnect starts");
    for _ in 0..100 {
        if health.current().state == RefObserverState::Connecting {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(health.current().state, RefObserverState::Connecting);
    assert!(
        events_rx.recv_timeout(Duration::from_millis(20)).is_err(),
        "transport connection alone cannot claim a fresh ref value"
    );

    stream_tx
        .send(WorkspaceRefStreamEvent::ConnectionState(
            WorkspaceRefStreamConnectionState::Connected,
        ))
        .expect("websocket reconnects");
    stream_tx
        .send(WorkspaceRefStreamEvent::Ref(Ok(None)))
        .expect("fresh subscription value");
    assert!(matches!(
        events_rx.recv_timeout(Duration::from_secs(1)),
        Ok(EngineEvent::RefChanged)
    ));
    for _ in 0..100 {
        if health.current().state == RefObserverState::Live {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(health.current().state, RefObserverState::Live);

    shutdown.store(true, Ordering::SeqCst);
    assert!(matches!(
        worker.join().expect("drain worker"),
        DrainOutcome::DriverGone
    ));
}

// ---- helpers -----------------------------------------------------------------

fn head_metadata(key: &str, hash: &str, byte_len: u64, key_epoch: u32) -> ObjectMetadata {
    ObjectMetadata {
        key: ObjectKey::new(key).expect("valid object key"),
        kind: StorageObjectKind::WorkspaceFileV1,
        byte_len,
        hash: hash.to_string(),
        key_epoch,
        created_by_device_id: None,
        created_at_unix_ms: 0,
        retention_state: RetentionState::Current,
        retain_until_unix_ms: None,
    }
}

static SPOOL_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn temp_spool(bytes: &[u8]) -> std::path::PathBuf {
    let mut path = std::env::temp_dir();
    let unique = format!(
        "bowline-manifest-transport-{}-{}",
        std::process::id(),
        SPOOL_COUNTER.fetch_add(1, Ordering::SeqCst)
    );
    path.push(unique);
    std::fs::write(&path, bytes).expect("write spool");
    path
}
