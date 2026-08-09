use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use bowline_control_plane::{
    ControlPlaneError, ControlPlaneTimestamp, DependencyFailureClass, FakeControlPlaneClient,
    ObjectKind, RejectionCode, WorkspaceRef, WorkspaceRefStreamConnectionState,
    WorkspaceRefStreamEvent, workspace_ref_stream_shutdown_pair,
};
use bowline_core::ids::{ContentId, DeviceId, SnapshotId, WorkspaceId};
use bowline_local::sync::manifest_engine::{
    BlobPrefetchRequest, BlobReaderUpload, BlobUpload, CasOutcome, EngineEvent, KeyEpoch,
    ManifestBatchUpload, ManifestUpload, RefVersionLookup, RemoteObjects, RemoteRef,
    TransportFailureClass, physical_blob_key, physical_manifest_key,
};
use bowline_storage::{
    ByteStoreError, ObjectKey, ObjectKind as StorageObjectKind, ObjectMetadata, RetentionState,
    stable_object_hash,
};

use super::object_uploader::{CommittedMetadataExpectation, validate_committed_metadata};
use super::ref_observer::{
    AttemptHistory, DrainOutcome, StreamAttempt, StreamStarter, drain_stream,
    should_log_observer_failure,
};
use super::{
    ManifestTransport, ReconnectAttempt, ReconnectDelay, RefChangeSubscription, RefObserverFailure,
    RefObserverFailureCode, RefObserverFailureStage, RefObserverHealthHandle, RefObserverReadiness,
    RefObserverRemediationKind, RefObserverState, SignerTrustRefresh,
};
use crate::device_trust::TrustRefreshOutcome;

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

#[test]
fn immutable_object_violation_remains_integrity_across_the_daemon_adapter() {
    let key = ObjectKey::new(format!("b_{}", "ea".repeat(32))).expect("object key");
    let error = super::helpers::byte_store_error(
        "put-blob",
        ByteStoreError::IntegrityViolation {
            key,
            reason: "existing bytes differ",
        },
    );

    assert_eq!(error.failure_class(), TransportFailureClass::Integrity);
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

/// Like [`sequenced_put_server`] but handles overlapping connections. The upload
/// pipeline PUTs several objects at once, so a server that only ever serves one
/// connection at a time would measure the test harness rather than the code.
///
/// It serves until the listener is dropped rather than a fixed number of
/// connections, because how many TCP connections a client opens for N requests
/// is the client's own pooling decision and not something a test may pin. A
/// fixed count left a reconnect with nothing listening, which surfaced under CI
/// load as a flaky `error sending request` on the client and a `BrokenPipe` on
/// the server — the harness, never the transport.
fn concurrent_put_server(status: &str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("test listener");
    let address = listener.local_addr().expect("listener address");
    let status = status.to_string();
    thread::spawn(move || {
        while let Ok((mut stream, _)) = listener.accept() {
            let status = status.clone();
            thread::spawn(move || {
                let mut request = [0; 4096];
                let _ = stream.read(&mut request).expect("read request");
                // `Connection: close` because that is what this handler does —
                // it answers once and drops the stream. Without it the reply is
                // HTTP/1.1 keep-alive by default, so the client pools a socket
                // the server has already closed and the next request fails on a
                // connection that was never reusable.
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .expect("write headers");
            });
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

/// R6's cost-model assertion, stated as round trips rather than wall clock.
///
/// Under the old transport `put_blob` cost three serialized round trips —
/// upload intent, PUT, metadata commit — before it returned, so a `git checkout`
/// touching 5,000 files paid 15,000 of them in sequence. This asserts the new
/// contract directly: `put_blob` contacts nothing, and the commits land in one
/// parallel drain at the publishing barrier. It fails on any regression to
/// inline per-object I/O.
#[test]
fn put_blob_defers_every_round_trip_to_the_publishing_barrier() {
    let control_plane = ready_workspace();
    let blobs: Vec<Vec<u8>> = (0..4_u8)
        .map(|index| format!("sealed-blob-payload-{index}").into_bytes())
        .collect();
    let manifest = b"sealed-manifest-payload".to_vec();
    let manifest_key = physical_manifest_key(&manifest);
    control_plane.set_signed_url_override("upload", concurrent_put_server("200 OK"));

    let content_id = content_id();
    let transport = transport(&control_plane);
    let blob_keys: Vec<_> = blobs
        .iter()
        .map(|sealed| physical_blob_key(sealed))
        .collect();
    for (sealed, key) in blobs.iter().zip(&blob_keys) {
        transport
            .put_blob(BlobUpload {
                key,
                content_id: &content_id,
                key_epoch: KeyEpoch::new(1),
                sealed,
            })
            .expect("put_blob succeeds");
    }

    assert!(
        control_plane.object_pointers(WORKSPACE).is_empty(),
        "put_blob must not contact the control plane inline"
    );

    transport
        .put_manifest(ManifestUpload {
            key: &manifest_key,
            content_id: &content_id,
            key_epoch: KeyEpoch::new(1),
            sealed: &manifest,
        })
        .expect("put_manifest succeeds");

    // Every blob's metadata row must exist by the time the manifest's does: the
    // manifest is the only thing that can name a blob.
    let pointers = control_plane.object_pointers(WORKSPACE);
    for (sealed, key) in blobs.iter().zip(&blob_keys) {
        let pointer = pointers
            .iter()
            .find(|pointer| pointer.object_key == key.as_str())
            .expect("committed object pointer for the uploaded blob");
        assert_eq!(pointer.byte_len, sealed.len() as u64);
        assert_eq!(pointer.hash, stable_object_hash(sealed));
        assert_eq!(pointer.key_epoch, 1);
        assert_eq!(pointer.kind, ObjectKind::Blob);
        assert_eq!(pointer.content_id, content_id);
    }
    assert!(
        pointers
            .iter()
            .any(|pointer| pointer.object_key == manifest_key.as_str())
    );
}

#[test]
fn two_hundred_fifty_six_blobs_use_four_ordered_metadata_batches() {
    let control_plane = ready_workspace();
    control_plane.set_signed_url_override("upload", concurrent_put_server("200 OK"));
    let content_id = content_id();
    let transport = transport(&control_plane);

    for index in 0..256_u16 {
        let sealed = format!("sealed-batch-object-{index:03}").into_bytes();
        let key = physical_blob_key(&sealed);
        transport
            .put_blob(BlobUpload {
                key: &key,
                content_id: &content_id,
                key_epoch: KeyEpoch::new(1),
                sealed: &sealed,
            })
            .expect("queued blob upload succeeds");
    }

    assert_eq!(
        control_plane.upload_reservation_batch_sizes(),
        vec![64, 64, 64, 64]
    );
    assert_eq!(
        control_plane.metadata_commit_batch_sizes(),
        vec![64, 64, 64, 64]
    );
    assert_eq!(control_plane.object_pointers(WORKSPACE).len(), 256);
}

#[test]
fn peer_prefetch_rejects_one_object_larger_than_the_total_byte_budget() {
    let control_plane = ready_workspace();
    let transport = transport(&control_plane);
    let error = transport
        .prefetch_blobs(&[BlobPrefetchRequest {
            key: physical_blob_key(b"oversized-prefetch"),
            byte_len: super::MAX_PREFETCH_BYTES + 1,
        }])
        .expect_err("an oversized object cannot enter the bounded prefetch pool");

    assert_eq!(error.operation, "prefetch-blob");
    assert!(
        error
            .detail
            .contains("exceeded the bounded prefetch budget")
    );
}

#[test]
fn manifest_nodes_share_the_same_bounded_metadata_batch_transport() {
    let control_plane = ready_workspace();
    control_plane.set_signed_url_override("upload", concurrent_put_server("200 OK"));
    let content_id = content_id();
    let uploads = (0..65_u8)
        .map(|index| {
            let sealed = format!("sealed-manifest-node-{index:02}").into_bytes();
            ManifestBatchUpload {
                key: physical_manifest_key(&sealed),
                content_id: content_id.clone(),
                key_epoch: KeyEpoch::new(1),
                sealed,
            }
        })
        .collect::<Vec<_>>();

    transport(&control_plane)
        .put_manifests(&uploads)
        .expect("manifest node batch succeeds");

    assert_eq!(control_plane.upload_reservation_batch_sizes(), vec![64, 1]);
    assert_eq!(control_plane.metadata_commit_batch_sizes(), vec![64, 1]);
    assert_eq!(control_plane.object_pointers(WORKSPACE).len(), 65);
}

/// A queued blob must never survive a ref move: the ref is what makes a manifest
/// — and through it every blob — reachable to another device.
#[test]
fn compare_and_swap_drains_queued_uploads_first() {
    let control_plane = ready_workspace();
    let sealed = b"sealed-blob-before-cas".to_vec();
    let key = physical_blob_key(&sealed);
    let manifest_key = physical_manifest_key(b"sealed-manifest-before-cas");
    control_plane.set_signed_url_override("upload", sequenced_put_server(&[("200 OK", b"")]));

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

    transport
        .compare_and_swap(None, &manifest_key)
        .expect("compare-and-swap succeeds");

    assert!(
        control_plane
            .object_pointers(WORKSPACE)
            .iter()
            .any(|pointer| pointer.object_key == key.as_str()),
        "the ref advanced with a blob still queued"
    );
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
fn blob_download_streams_into_the_callers_writer() {
    let control_plane = ready_workspace();
    let sealed = (0..2 * 1024)
        .map(|index| (index % 251) as u8)
        .collect::<Vec<_>>();
    let key = physical_blob_key(&sealed);
    control_plane.set_signed_url_override("upload", sequenced_put_server(&[("200 OK", b"")]));
    control_plane.set_signed_url_override(
        "download",
        owned_signed_url_response("200 OK", Arc::new(sealed.clone())),
    );

    let transport = transport(&control_plane);
    let content_id = content_id();
    transport
        .put_blob(BlobUpload {
            key: &key,
            content_id: &content_id,
            key_epoch: KeyEpoch::new(1),
            sealed: &sealed,
        })
        .expect("put blob");
    let mut fetched = Vec::new();
    let copied = transport
        .get_blob_to_writer(&key, &mut fetched)
        .expect("streaming blob download succeeds");

    assert_eq!(copied, sealed.len() as u64);
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

/// Convergent sealing makes a re-upload of identical content routine, so the
/// transport must treat "the server already has it" as done rather than as a
/// failed PUT. The one-shot PUT server proves it: a second attempt that tried to
/// transfer anything would hang or fail on a closed listener.
#[test]
fn put_blob_accepts_an_object_the_server_already_holds() {
    let control_plane = ready_workspace();
    let sealed = b"sealed-blob-already-committed".to_vec();
    let key = physical_blob_key(&sealed);
    let manifest_key = physical_manifest_key(b"sealed-manifest-already-committed");
    control_plane.set_signed_url_override("upload", sequenced_put_server(&[("200 OK", b"")]));

    let content_id = content_id();
    let blob = || BlobUpload {
        key: &key,
        content_id: &content_id,
        key_epoch: KeyEpoch::new(1),
        sealed: &sealed,
    };
    let transport = transport(&control_plane);
    transport.put_blob(blob()).expect("put_blob succeeds");
    // Draining is what actually contacts the control plane, so publish once to
    // commit the blob before re-presenting it.
    transport
        .compare_and_swap(None, &manifest_key)
        .expect("compare-and-swap drains the first upload");

    transport
        .put_blob(blob())
        .expect("re-presenting committed bytes queues");
    transport
        .compare_and_swap(None, &manifest_key)
        .expect("an already-committed blob does not fail the drain");

    let pointers = control_plane.object_pointers(WORKSPACE);
    assert_eq!(
        pointers
            .iter()
            .filter(|pointer| pointer.object_key == key.as_str())
            .count(),
        1,
        "an already-committed blob must not be committed twice"
    );
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
fn ref_version_lookup_proves_a_superseded_manifest_from_history() {
    let control_plane = ready_workspace();
    let transport = transport(&control_plane);
    let first = physical_manifest_key(b"first-manifest");
    let second = physical_manifest_key(b"second-manifest");
    let third = physical_manifest_key(b"third-manifest");

    transport
        .compare_and_swap(None, &first)
        .expect("first CAS succeeds");
    transport
        .compare_and_swap(Some(1), &second)
        .expect("second CAS succeeds");
    transport
        .compare_and_swap(Some(2), &third)
        .expect("third CAS succeeds");

    assert_eq!(
        transport.lookup_ref_version(1).expect("history lookup"),
        RefVersionLookup::Found(first)
    );
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
fn compare_and_swap_does_not_launder_a_decisive_rejection_into_ambiguous() {
    // A swap that never left the client has a KNOWN outcome. Reporting it as
    // ambiguous made the engine re-read the ref and retry forever on the normal
    // backoff, so a revoked device or an expired session looked like a sync that
    // was merely catching up.
    let control_plane = ready_workspace();
    control_plane.set_offline(true);
    let manifest_key = physical_manifest_key(b"rejected-manifest");

    let error = transport(&control_plane)
        .compare_and_swap(Some(0), &manifest_key)
        .expect_err("a decisive rejection is an error, not an ambiguous outcome");
    assert!(
        error.to_string().contains("compare-and-swap"),
        "got: {error}"
    );
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
    let (events_tx, events_rx) = crossbeam_channel::bounded(64);
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

    let subscription = RefChangeSubscription::spawn_with_starter(
        starter,
        events_tx,
        delay,
        never_refreshed_trust(),
    );

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
    let (events_tx, events_rx) = crossbeam_channel::bounded(64);
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
    let (events_tx, events_rx) = crossbeam_channel::bounded(64);
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

/// The observer's own classification of what ended an attempt. A refused
/// credential must not hide inside the transport stage: the reconnect schedule
/// and the status projection both key off it, and a daemon that silently stops
/// receiving remote heads is the failure this exists to prevent.
#[test]
fn a_refused_credential_is_classified_apart_from_a_transport_drop() {
    let refusal = drain_one_stream_error(ControlPlaneError::Rejected {
        code: RejectionCode::Unauthorized,
        message: "account session expired".to_string(),
    });
    let drop = drain_one_stream_error(ControlPlaneError::Transport {
        detail: "connection reset".to_string(),
    });

    assert_eq!(refusal.stage, RefObserverFailureStage::Authentication);
    assert_eq!(drop.stage, RefObserverFailureStage::Stream);
}

/// The failure that ended an attempt outlives the wait before the next one, so
/// the condition stays reported instead of flickering back to a neutral
/// "connecting" on every backoff cycle.
#[test]
fn a_refused_credential_reports_action_required_until_remediated() {
    let health = RefObserverHealthHandle::new();
    assert_eq!(health.readiness(), RefObserverReadiness::Retrying);

    health.transition(
        RefObserverState::Blocked,
        1,
        true,
        Some(RefObserverFailure {
            stage: RefObserverFailureStage::Authentication,
            class: DependencyFailureClass::AuthenticationRequired,
            code: RefObserverFailureCode::AuthenticationRequired,
        }),
    );
    assert_eq!(
        health.readiness(),
        RefObserverReadiness::Blocked {
            class: DependencyFailureClass::AuthenticationRequired,
            code: RefObserverFailureCode::AuthenticationRequired,
        }
    );
    let remediation = health
        .remediation_for_current_block(RefObserverRemediationKind::AuthenticationRestored)
        .expect("current block issues exact remediation evidence");
    assert!(health.remediation_completed(remediation));
    assert_eq!(health.readiness(), RefObserverReadiness::Retrying);
}

/// A dropped websocket keeps the ordinary retrying condition; only a refusal
/// escalates. Otherwise every network blip would claim the daemon needs a login.
#[test]
fn a_transport_drop_stays_ordinary_retrying() {
    let health = RefObserverHealthHandle::new();

    health.transition(
        RefObserverState::Retrying,
        1,
        true,
        Some(RefObserverFailure {
            stage: RefObserverFailureStage::Stream,
            class: DependencyFailureClass::Retryable,
            code: RefObserverFailureCode::StreamUnavailable,
        }),
    );

    assert_eq!(health.readiness(), RefObserverReadiness::Retrying);
}

/// The bridge keeps reconnecting after a refusal — a later login can still fix
/// it — but the schedule it asks for carries the authentication stage, which is
/// what keeps it off the fast transport ceiling.
#[test]
fn a_refused_subscription_blocks_without_a_time_based_retry() {
    let (events_tx, _events_rx) = crossbeam_channel::bounded(64);
    let starts = Arc::new(AtomicUsize::new(0));
    let counted_starts = Arc::clone(&starts);
    let requested = Arc::new(Mutex::new(Vec::<ReconnectAttempt>::new()));
    let recorded = Arc::clone(&requested);
    let starter: StreamStarter = Box::new(move |stream_tx| {
        counted_starts.fetch_add(1, Ordering::SeqCst);
        let (shutdown, _cancellation) = workspace_ref_stream_shutdown_pair();
        let worker = thread::Builder::new()
            .name("test-refused-ref-stream".to_string())
            .spawn(move || {
                let _receiver_gone = stream_tx.send(WorkspaceRefStreamEvent::Ref(Err(
                    ControlPlaneError::Rejected {
                        code: RejectionCode::Unauthorized,
                        message: "account session expired".to_string(),
                    },
                )));
            })
            .expect("test stream thread");
        Ok(StreamAttempt { shutdown, worker })
    });
    let delay: ReconnectDelay = Arc::new(move |attempt| {
        recorded.lock().expect("schedule requests").push(attempt);
        Duration::from_millis(2)
    });

    let subscription = RefChangeSubscription::spawn_with_starter(
        starter,
        events_tx,
        delay,
        never_refreshed_trust(),
    );
    let health = subscription.health_handle();
    for _ in 0..500 {
        if health.current().state == RefObserverState::Blocked {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
    let readiness = health.readiness();
    let attempts = requested.lock().expect("schedule requests").clone();
    assert!(attempts.is_empty(), "authority loss never enters backoff");
    assert_eq!(starts.load(Ordering::SeqCst), 1);
    assert_eq!(
        readiness,
        RefObserverReadiness::Blocked {
            class: DependencyFailureClass::AuthenticationRequired,
            code: RefObserverFailureCode::AuthenticationRequired,
        }
    );
    drop(subscription);
}

/// A bridge whose signer trust never changes, for the tests that are not about
/// device trust.
fn never_refreshed_trust() -> SignerTrustRefresh {
    Arc::new(|_device_id| TrustRefreshOutcome::RateLimited)
}

fn unknown_signer_error(device_id: &str) -> ControlPlaneError {
    ControlPlaneError::UnknownSigningDevice {
        workspace_id: WorkspaceId::new(WORKSPACE),
        device_id: DeviceId::new(device_id),
    }
}

/// A stream that refuses its first attempt because the head was signed by a
/// device this host does not know, and serves values from the attempt after the
/// one that learned it.
fn stream_learning_after_first_attempt(
    signer: &'static str,
    attempts: Arc<AtomicUsize>,
) -> StreamStarter {
    Box::new(move |stream_tx| {
        let attempt = attempts.fetch_add(1, Ordering::SeqCst);
        let (shutdown, _cancellation) = workspace_ref_stream_shutdown_pair();
        let worker = thread::Builder::new()
            .name("test-unknown-signer-stream".to_string())
            .spawn(move || {
                let event = if attempt == 0 {
                    WorkspaceRefStreamEvent::Ref(Err(unknown_signer_error(signer)))
                } else {
                    WorkspaceRefStreamEvent::Ref(Ok(None))
                };
                let _receiver_gone = stream_tx.send(event);
            })
            .expect("test stream thread");
        Ok(StreamAttempt { shutdown, worker })
    })
}

/// The release blocker this path exists for: trusting a second device while
/// this daemon runs publishes heads signed by a device its client has never
/// heard of. The observer must learn that device and carry on — no restart, no
/// user action.
#[test]
fn a_device_trusted_after_startup_is_learned_without_a_restart() {
    let (events_tx, events_rx) = crossbeam_channel::bounded(64);
    let refreshed = Arc::new(Mutex::new(Vec::<DeviceId>::new()));
    let recorded_refresh = Arc::clone(&refreshed);
    let trust_refresh: SignerTrustRefresh = Arc::new(move |device_id| {
        recorded_refresh
            .lock()
            .expect("refresh log")
            .push(device_id.clone());
        TrustRefreshOutcome::Learned
    });
    let requested = Arc::new(Mutex::new(Vec::<ReconnectAttempt>::new()));
    let recorded_delay = Arc::clone(&requested);
    let delay: ReconnectDelay = Arc::new(move |attempt| {
        recorded_delay
            .lock()
            .expect("schedule requests")
            .push(attempt);
        Duration::from_millis(2)
    });

    let subscription = RefChangeSubscription::spawn_with_starter(
        stream_learning_after_first_attempt("device_second", Arc::new(AtomicUsize::new(0))),
        events_tx,
        delay,
        trust_refresh,
    );

    assert!(
        matches!(
            events_rx.recv_timeout(Duration::from_secs(5)),
            Ok(EngineEvent::RefChanged)
        ),
        "the engine must be woken once the new device's head can be verified"
    );
    let attempts = requested.lock().expect("schedule requests").clone();
    drop(subscription);

    assert_eq!(
        refreshed.lock().expect("refresh log").as_slice(),
        &[DeviceId::new("device_second")],
        "the observer refreshes trust for exactly the device that signed the head"
    );
    assert!(
        !attempts.iter().any(|attempt| matches!(
            attempt.stage,
            RefObserverFailureStage::UnknownSigner(_) | RefObserverFailureStage::UntrustedSigner(_)
        )),
        "learning the signer is progress, not something to back off from: {attempts:?}"
    );
}

/// A signer this host is not allowed to verify is a different fact from one it
/// has not learned yet, and it must not read like a transport blip: status says
/// unavailable rather than retrying, and the schedule stops treating reopening
/// as the fix.
#[test]
fn a_signer_the_control_plane_disowns_is_reported_apart_from_a_retry() {
    let (events_tx, _events_rx) = crossbeam_channel::bounded(64);
    let refreshes = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&refreshes);
    let trust_refresh: SignerTrustRefresh = Arc::new(move |_device_id| {
        counted.fetch_add(1, Ordering::SeqCst);
        TrustRefreshOutcome::NotAuthorized
    });
    let requested = Arc::new(Mutex::new(Vec::<ReconnectAttempt>::new()));
    let recorded = Arc::clone(&requested);
    let delay: ReconnectDelay = Arc::new(move |attempt| {
        recorded.lock().expect("schedule requests").push(attempt);
        Duration::from_millis(2)
    });
    let starter: StreamStarter = Box::new(move |stream_tx| {
        let (shutdown, _cancellation) = workspace_ref_stream_shutdown_pair();
        let worker = thread::Builder::new()
            .name("test-untrusted-signer-stream".to_string())
            .spawn(move || {
                let _receiver_gone = stream_tx.send(WorkspaceRefStreamEvent::Ref(Err(
                    unknown_signer_error("device_stranger"),
                )));
            })
            .expect("test stream thread");
        Ok(StreamAttempt { shutdown, worker })
    });

    let subscription =
        RefChangeSubscription::spawn_with_starter(starter, events_tx, delay, trust_refresh);
    let health = subscription.health_handle();
    for _ in 0..500 {
        if health.current().state == RefObserverState::Blocked {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
    let readiness = health.readiness();
    let attempts = requested.lock().expect("schedule requests").clone();
    drop(subscription);

    assert!(
        attempts.is_empty(),
        "authorization loss never enters backoff"
    );
    assert_eq!(
        readiness,
        RefObserverReadiness::Blocked {
            class: DependencyFailureClass::AuthorizationLost,
            code: RefObserverFailureCode::AuthorizationLost,
        }
    );
    assert_eq!(refreshes.load(Ordering::SeqCst), 1);
}

/// Learning a signer skips the backoff because it is progress. If the head stays
/// unverifiable anyway — a disagreement between the resolver and the trust
/// handle, which nothing else would catch — the bridge must stop calling it
/// progress rather than reopening the subscription in a tight loop.
#[test]
fn learning_a_signer_that_never_helps_falls_back_to_the_backoff() {
    let (events_tx, _events_rx) = crossbeam_channel::bounded(64);
    let trust_refresh: SignerTrustRefresh = Arc::new(|_device_id| TrustRefreshOutcome::Learned);
    let requested = Arc::new(Mutex::new(Vec::<ReconnectAttempt>::new()));
    let recorded = Arc::clone(&requested);
    let delay: ReconnectDelay = Arc::new(move |attempt| {
        recorded.lock().expect("schedule requests").push(attempt);
        Duration::from_millis(2)
    });
    let starter: StreamStarter = Box::new(move |stream_tx| {
        let (shutdown, _cancellation) = workspace_ref_stream_shutdown_pair();
        let worker = thread::Builder::new()
            .name("test-unhelpful-trust-stream".to_string())
            .spawn(move || {
                let _receiver_gone = stream_tx.send(WorkspaceRefStreamEvent::Ref(Err(
                    unknown_signer_error("device_second"),
                )));
            })
            .expect("test stream thread");
        Ok(StreamAttempt { shutdown, worker })
    });

    let subscription =
        RefChangeSubscription::spawn_with_starter(starter, events_tx, delay, trust_refresh);
    for _ in 0..500 {
        if !requested.lock().expect("schedule requests").is_empty() {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
    let attempts = requested.lock().expect("schedule requests").clone();
    drop(subscription);

    assert!(
        !attempts.is_empty(),
        "an immediate retry that never delivers a value must fall back to the backoff"
    );
}

/// The observed failure mode was 1,853 identical log lines. A condition that
/// cannot clear must stay visible without writing one line per reconnect.
#[test]
fn a_repeating_failure_is_logged_on_a_thinning_schedule() {
    let failure = RefObserverFailure {
        stage: RefObserverFailureStage::UntrustedSigner(DeviceId::new("device_stranger")),
        class: DependencyFailureClass::AuthorizationLost,
        code: RefObserverFailureCode::AuthorizationLost,
    };
    let mut history = AttemptHistory::default();
    let mut logged = 0_u32;

    for _ in 0..1853 {
        history.failures = history.failures.saturating_add(1);
        if should_log_observer_failure(&failure, &history) {
            logged += 1;
            history.last_logged = Some(failure.clone());
        }
    }

    assert_eq!(
        logged, 11,
        "1,853 repeats of one condition must not be 1,853 log lines"
    );

    // A condition that changes is always reported at once: the thinning must
    // never hide something new.
    let different = RefObserverFailure {
        stage: RefObserverFailureStage::Stream,
        class: DependencyFailureClass::Retryable,
        code: RefObserverFailureCode::StreamUnavailable,
    };
    assert!(should_log_observer_failure(&different, &history));
}

fn drain_one_stream_error(error: ControlPlaneError) -> RefObserverFailure {
    let (stream_tx, stream_rx) = mpsc::channel();
    let (events_tx, _events_rx) = crossbeam_channel::bounded(64);
    let shutdown = AtomicBool::new(false);
    let health = RefObserverHealthHandle::new();
    stream_tx
        .send(WorkspaceRefStreamEvent::Ref(Err(error)))
        .expect("stream error is queued");
    drop(stream_tx);

    match drain_stream(
        &stream_rx,
        &events_tx,
        &shutdown,
        &health,
        Duration::from_secs(1),
    ) {
        DrainOutcome::Reconnect { failure, .. } => failure,
        DrainOutcome::DriverGone => panic!("a stream error must ask for a reconnect"),
    }
}

#[test]
fn ref_observer_times_out_without_initial_value() {
    let (_stream_tx, stream_rx) = mpsc::channel();
    let (events_tx, _events_rx) = crossbeam_channel::bounded(64);
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
    let (events_tx, events_rx) = crossbeam_channel::bounded(64);
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
