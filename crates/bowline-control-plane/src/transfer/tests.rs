use super::*;
use bowline_storage::{ObjectContentId, ObjectHash, ReopenableObjectSource};
use std::{
    io::{Cursor, Read, Write},
    net::TcpListener,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
};

struct ConditionalRetryTestSource {
    bytes: Arc<Vec<u8>>,
    first_upload_bytes: Option<Arc<Vec<u8>>>,
    opens: AtomicUsize,
}

impl ReopenableObjectSource for ConditionalRetryTestSource {
    fn open(&self) -> std::io::Result<Box<dyn Read + Send>> {
        let open = self.opens.fetch_add(1, Ordering::Relaxed);
        let bytes = if open == 0 {
            self.first_upload_bytes.as_ref().unwrap_or(&self.bytes)
        } else {
            &self.bytes
        };
        Ok(Box::new(Cursor::new(bytes.as_ref().clone())))
    }
}

fn assert_intent_failure(
    error: ControlPlaneError,
    expected_operation: TransferOperation,
    expected_kind: IntentFailureKind,
) {
    let mapped = map_control_error(expected_operation, error);
    match mapped {
        ByteStoreError::IntentFailed {
            operation,
            kind,
            detail,
        } => {
            assert_eq!(operation, expected_operation);
            assert_eq!(kind, expected_kind);
            assert!(!detail.is_empty());
        }
        other => panic!("expected intent failure, got {other:?}"),
    }
}

#[test]
fn control_plane_errors_map_to_transfer_intent_failures() {
    assert_intent_failure(
        ControlPlaneError::Timeout {
            capability: "hosted Convex",
        },
        TransferOperation::Upload,
        IntentFailureKind::Timeout,
    );
    assert_intent_failure(
        ControlPlaneError::Transport {
            detail: "connection refused".to_string(),
        },
        TransferOperation::Download,
        IntentFailureKind::Transport,
    );
    assert_intent_failure(
        ControlPlaneError::Rejected {
            code: RejectionCode::DeviceNotTrusted,
            message: "device is not trusted".to_string(),
        },
        TransferOperation::Delete,
        IntentFailureKind::DeviceNotTrusted,
    );
    assert_intent_failure(
        ControlPlaneError::Rejected {
            code: RejectionCode::Unauthorized,
            message: "device cannot update this lease".to_string(),
        },
        TransferOperation::Upload,
        IntentFailureKind::DeviceNotTrusted,
    );
    assert_intent_failure(
        ControlPlaneError::Rejected {
            code: RejectionCode::InvalidRequest,
            message: "device trust has expired".to_string(),
        },
        TransferOperation::Upload,
        IntentFailureKind::Other,
    );
}

#[test]
fn upload_verification_status_is_upload_classified() {
    let key = ObjectKey::new("b_00112233445566d400112233445566d400112233445566d400112233445566d4")
        .expect("key");
    assert!(matches!(
        upload_verification_status_error(&key, reqwest::StatusCode::SERVICE_UNAVAILABLE),
        ByteStoreError::HttpStatus {
            operation: TransferOperation::Upload,
            status: 503,
            ..
        }
    ));
}

#[test]
fn hosted_head_maps_missing_metadata_to_missing_object() {
    let control_plane = crate::FakeControlPlaneClient::default();
    control_plane.create_workspace("ws_head_missing");
    let store = SignedUrlByteStore::new(&control_plane, "ws_head_missing");
    let key = ObjectKey::new("m_0000000000000001000000000000000100000000000000010000000000000001")
        .expect("object key");

    let error = store
        .head_object(&key)
        .expect_err("missing metadata must remain a normal cache miss");

    assert!(matches!(
        error,
        ByteStoreError::MissingObject {
            key: missing,
            component: "hosted object metadata",
        } if missing == key
    ));
}

fn signed_url_response(status: &str, body: &'static [u8]) -> String {
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
        stream.write_all(body).expect("write body");
    });
    format!("http://{address}/object")
}

fn early_signed_url_response(status: &str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("test listener");
    let address = listener.local_addr().expect("listener address");
    let status = status.to_string();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept request");
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        let header_end = loop {
            let read = stream.read(&mut buffer).expect("read request headers");
            request.extend_from_slice(&buffer[..read]);
            if let Some(index) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                break index + 4;
            }
        };
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_len = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length: ")
                    .map(str::to_owned)
            })
            .expect("content length")
            .parse::<usize>()
            .expect("numeric content length");
        write!(stream, "HTTP/1.1 {status}\r\nContent-Length: 0\r\n\r\n")
            .expect("write early response");
        stream.flush().expect("flush early response");
        let mut body_len = request.len() - header_end;
        while body_len < content_len {
            let read = stream.read(&mut buffer).expect("drain request body");
            if read == 0 {
                break;
            }
            body_len += read;
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

#[test]
fn streaming_precondition_response_verifies_without_consuming_put_body() {
    let bytes = Arc::new(vec![0x5a; 8 * 1024 * 1024]);
    let key = ObjectKey::new(format!("b_{}", "d5".repeat(32))).expect("object key");
    let hash = stable_object_hash(&bytes);
    let source = ConditionalRetryTestSource {
        bytes: bytes.clone(),
        first_upload_bytes: Some(Arc::new(vec![0xa5; bytes.len()])),
        opens: AtomicUsize::new(0),
    };
    let control_plane = crate::FakeControlPlaneClient::default();
    control_plane.create_workspace("ws_streaming_412");
    control_plane.set_signed_url_override(
        "upload",
        early_signed_url_response("412 Precondition Failed"),
    );
    control_plane.set_signed_url_override(
        "verify-upload",
        owned_signed_url_response("200 OK", bytes.clone()),
    );
    let store = SignedUrlByteStore::new(&control_plane, "ws_streaming_412");

    let metadata = store
        .put_object_reader_with_content_id_at_epoch(PutObjectReaderRequest {
            key: key.clone(),
            kind: StorageObjectKind::WorkspaceFileV1,
            content_id: ObjectContentId::new("cid_streaming_412"),
            source: &source,
            byte_len: bytes.len() as u64,
            expected_hash: ObjectHash::from_stable_hash(hash.clone()),
            key_epoch: 1,
            created_by_device_id: None,
        })
        .expect("matching existing object should verify");

    assert_eq!(metadata.key, key);
    assert_eq!(metadata.hash, hash);
    assert_eq!(source.opens.load(Ordering::Relaxed), 3);
    let metrics = store.metrics();
    assert_eq!(metrics.conditional_write_conflict_count, 1);
    assert_eq!(metrics.verification_failure_count, 0);
    assert_eq!(metrics.convex_action_count, 2);
    assert_eq!(metrics.put_count, 1);
    assert_eq!(metrics.bytes_uploaded, bytes.len() as u64);
}

#[test]
fn buffered_put_overwrites_when_existing_object_hash_mismatches() {
    let desired = b"desired-sealed-bytes".to_vec();
    let foreign = b"foreign-or-resealed".to_vec();
    let key = ObjectKey::new(format!("b_{}", "e0".repeat(32))).expect("object key");
    let control_plane = crate::FakeControlPlaneClient::default();
    control_plane.create_workspace("ws_put_412_mismatch");
    control_plane.set_signed_url_override(
        "upload",
        sequenced_put_server(&[("412 Precondition Failed", b""), ("200 OK", b"")]),
    );
    control_plane.set_signed_url_override(
        "verify-upload",
        owned_signed_url_response("200 OK", Arc::new(foreign)),
    );
    let store = SignedUrlByteStore::new(&control_plane, "ws_put_412_mismatch");

    let metadata = store
        .put_object_with_content_id_at_epoch(
            key.clone(),
            StorageObjectKind::WorkspaceFileV1,
            ObjectContentId::new("cid_put_412_mismatch").as_str(),
            &desired,
            1,
            None,
        )
        .expect("hash mismatch must recover via overwrite");

    assert_eq!(metadata.key, key);
    assert_eq!(metadata.hash, stable_object_hash(&desired));
    let metrics = store.metrics();
    assert_eq!(metrics.conditional_write_conflict_count, 1);
    assert_eq!(metrics.verification_failure_count, 1);
    assert_eq!(metrics.put_count, 1);
}

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
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .expect("write headers");
            stream.write_all(&body).expect("write body");
        }
    });
    format!("http://{address}/object")
}

#[test]
fn range_fetch_rejects_full_body_success() {
    let client = Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("client");
    let key = ObjectKey::new("b_0000000000000001000000000000000100000000000000010000000000000001")
        .expect("object key");
    let url = signed_url_response("200 OK", b"abcdef");

    let error = fetch_signed_url(&client, &key, &url, Some(ByteRange::new(1, 2)))
        .expect_err("200 range response must fail");

    assert!(matches!(
        error,
        ByteStoreError::HttpStatus {
            operation: TransferOperation::Download,
            status: 200,
            ..
        }
    ));
}

#[test]
fn range_fetch_requires_exact_body_length() {
    let client = Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("client");
    let key = ObjectKey::new("b_0000000000000002000000000000000200000000000000020000000000000002")
        .expect("object key");
    let url = signed_url_response("206 Partial Content", b"a");

    let error = fetch_signed_url(&client, &key, &url, Some(ByteRange::new(1, 2)))
        .expect_err("short 206 range response must fail");

    assert!(matches!(error, ByteStoreError::CorruptObject { .. }));
}

#[test]
fn range_fetch_rejects_oversized_partial_content_body() {
    let client = Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("client");
    let key = ObjectKey::new("b_0000000000000004000000000000000400000000000000040000000000000004")
        .expect("object key");
    let url = signed_url_response("206 Partial Content", b"bcd");

    let error = fetch_signed_url(&client, &key, &url, Some(ByteRange::new(1, 2)))
        .expect_err("oversized 206 range response must fail");

    assert!(matches!(error, ByteStoreError::CorruptObject { .. }));
}

#[test]
fn range_fetch_accepts_partial_content_with_exact_body_length() {
    let client = Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("client");
    let key = ObjectKey::new("b_0000000000000003000000000000000300000000000000030000000000000003")
        .expect("object key");
    let url = signed_url_response("206 Partial Content", b"bc");

    let bytes = fetch_signed_url(&client, &key, &url, Some(ByteRange::new(1, 2)))
        .expect("exact 206 response");

    assert_eq!(bytes, b"bc");
}
