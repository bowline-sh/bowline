use std::{
    cell::RefCell,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use bowline_core::ids::{DeviceId, WorkspaceId};
use bowline_storage::{
    ByteRange, ByteStore, ByteStoreError, ByteStoreMetrics, IntentFailureKind, ObjectKey,
    ObjectKind as StorageObjectKind, ObjectMetadata, PutObjectReaderRequest, RetentionState,
    TransferOperation, stable_object_hash,
};
use reqwest::blocking::Client;

mod streaming_upload;
use streaming_upload::{send_streaming_put, verify_matching_readers};

use crate::{
    ControlPlaneClient, ControlPlaneError, DownloadIntentRequest, ObjectKind as ControlObjectKind,
    RejectionCode, UploadIntentRequest, UploadVerificationIntentRequest,
};

const SIGNED_URL_HTTP_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct SignedUrlHttpClient(Client);

impl std::ops::Deref for SignedUrlHttpClient {
    type Target = Client;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Debug)]
pub struct SignedUrlByteStore<'a, C> {
    control_plane: &'a C,
    workspace_id: String,
    http: SignedUrlHttpClient,
    metrics: RefCell<ByteStoreMetrics>,
}

impl<'a, C: ControlPlaneClient> SignedUrlByteStore<'a, C> {
    pub fn new(control_plane: &'a C, workspace_id: impl Into<String>) -> Self {
        Self::with_http_client(control_plane, workspace_id, Self::build_http_client())
    }

    pub fn with_http_client(
        control_plane: &'a C,
        workspace_id: impl Into<String>,
        http: SignedUrlHttpClient,
    ) -> Self {
        Self {
            control_plane,
            workspace_id: workspace_id.into(),
            http,
            metrics: RefCell::default(),
        }
    }

    pub fn build_http_client() -> SignedUrlHttpClient {
        SignedUrlHttpClient(
            Client::builder()
                .timeout(SIGNED_URL_HTTP_TIMEOUT)
                .build()
                .expect("reqwest client with timeout should build"),
        )
    }

    pub fn put_object_with_content_id(
        &self,
        key: ObjectKey,
        kind: StorageObjectKind,
        content_id: impl Into<String>,
        bytes: &[u8],
        created_by_device_id: Option<&DeviceId>,
    ) -> Result<ObjectMetadata, ByteStoreError> {
        self.put_object_with_content_id_at_epoch(
            key,
            kind,
            content_id,
            bytes,
            1,
            created_by_device_id,
        )
    }

    pub fn put_object_with_content_id_at_epoch(
        &self,
        key: ObjectKey,
        kind: StorageObjectKind,
        content_id: impl Into<String>,
        bytes: &[u8],
        key_epoch: u32,
        created_by_device_id: Option<&DeviceId>,
    ) -> Result<ObjectMetadata, ByteStoreError> {
        let content_id = content_id.into();
        let expected_hash = stable_object_hash(bytes);
        self.metrics.borrow_mut().convex_action_count += 1;
        let intent = self
            .control_plane
            .create_upload_intent(
                UploadIntentRequest::new(
                    self.workspace_id.clone(),
                    ControlObjectKind::try_from(kind)?,
                    bytes.len() as u64,
                )
                .with_content_id(content_id.clone())
                .with_object_key(key.as_str()),
            )
            .map_err(|error| map_control_error(TransferOperation::Upload, error))?;

        let response = self
            .http
            .put(&intent.signed_url.url)
            .header(reqwest::header::IF_NONE_MATCH, "*")
            .body(bytes.to_vec())
            .send()
            .map_err(|error| map_http_error(TransferOperation::Upload, error))?;
        let status = response.status();
        if status == reqwest::StatusCode::PRECONDITION_FAILED {
            self.metrics.borrow_mut().conditional_write_conflict_count += 1;
            self.verify_existing_upload(
                &key,
                bytes.len() as u64,
                &content_id,
                bytes,
                &expected_hash,
            )?;
        } else if !status.is_success() {
            return Err(ByteStoreError::HttpStatus {
                key,
                operation: TransferOperation::Upload,
                status: status.as_u16(),
            });
        }

        let metadata = ObjectMetadata {
            key: key.clone(),
            kind,
            byte_len: bytes.len() as u64,
            hash: expected_hash,
            key_epoch,
            created_by_device_id: created_by_device_id.cloned(),
            created_at_unix_ms: current_unix_ms(),
            retention_state: RetentionState::Pending,
            retain_until_unix_ms: None,
        };
        let mut metrics = self.metrics.borrow_mut();
        metrics.put_count += 1;
        metrics.bytes_uploaded += bytes.len() as u64;

        Ok(metadata)
    }

    fn verify_existing_upload(
        &self,
        key: &ObjectKey,
        byte_len: u64,
        content_id: &str,
        expected_bytes: &[u8],
        expected_hash: &str,
    ) -> Result<(), ByteStoreError> {
        self.metrics.borrow_mut().convex_action_count += 1;
        let intent = self
            .control_plane
            .create_upload_verification_intent(
                UploadVerificationIntentRequest::new(
                    self.workspace_id.clone(),
                    key.as_str(),
                    byte_len,
                )
                .with_content_id(content_id),
            )
            .map_err(|error| map_control_error(TransferOperation::Upload, error))?;
        let existing = fetch_signed_url(&self.http, key, &intent.signed_url.url, None)?;
        if existing.len() as u64 != byte_len || stable_object_hash(&existing) != expected_hash {
            return Err(ByteStoreError::CorruptObject {
                key: key.clone(),
                reason: "existing upload does not match retry bytes",
            });
        }
        if existing != expected_bytes {
            return Err(ByteStoreError::CorruptObject {
                key: key.clone(),
                reason: "existing upload differs from retry bytes",
            });
        }
        Ok(())
    }

    fn verify_existing_upload_source(
        &self,
        request: &PutObjectReaderRequest<'_>,
    ) -> Result<(), ByteStoreError> {
        self.metrics.borrow_mut().convex_action_count += 1;
        let intent = self
            .control_plane
            .create_upload_verification_intent(
                UploadVerificationIntentRequest::new(
                    self.workspace_id.clone(),
                    request.key.as_str(),
                    request.byte_len,
                )
                .with_content_id(request.content_id.as_str()),
            )
            .map_err(|error| map_control_error(TransferOperation::Upload, error))?;
        let mut response = self
            .http
            .get(&intent.signed_url.url)
            .send()
            .map_err(|error| map_http_error(TransferOperation::Upload, error))?;
        if !response.status().is_success() {
            return Err(upload_verification_status_error(
                &request.key,
                response.status(),
            ));
        }
        verify_matching_readers(
            &request.key,
            &mut response,
            request.source.open()?.as_mut(),
            request.byte_len,
            request.expected_hash.as_str(),
        )
    }
}

fn upload_verification_status_error(
    key: &ObjectKey,
    status: reqwest::StatusCode,
) -> ByteStoreError {
    ByteStoreError::HttpStatus {
        key: key.clone(),
        operation: TransferOperation::Upload,
        status: status.as_u16(),
    }
}

impl<C: ControlPlaneClient> ByteStore for SignedUrlByteStore<'_, C> {
    fn put_object_reader_with_content_id_at_epoch(
        &self,
        request: PutObjectReaderRequest<'_>,
    ) -> Result<ObjectMetadata, ByteStoreError> {
        self.metrics.borrow_mut().convex_action_count += 1;
        let intent = self
            .control_plane
            .create_upload_intent(
                UploadIntentRequest::new(
                    self.workspace_id.clone(),
                    ControlObjectKind::try_from(request.kind)?,
                    request.byte_len,
                )
                .with_content_id(request.content_id.as_str())
                .with_object_key(request.key.as_str()),
            )
            .map_err(|error| map_control_error(TransferOperation::Upload, error))?;
        let response = send_streaming_put(
            &self.http,
            &intent.signed_url.url,
            &request.key,
            request.source,
            request.byte_len,
            request.expected_hash.as_str(),
        )?;
        let status = response.status();
        if status == reqwest::StatusCode::PRECONDITION_FAILED {
            self.metrics.borrow_mut().conditional_write_conflict_count += 1;
            if let Err(error) = self.verify_existing_upload_source(&request) {
                let mut metrics = self.metrics.borrow_mut();
                metrics.verification_failure_count += 1;
                if matches!(
                    error,
                    ByteStoreError::Network { .. }
                        | ByteStoreError::HttpStatus { .. }
                        | ByteStoreError::IntentFailed { .. }
                ) {
                    metrics.retryable_failure_count += 1;
                }
                return Err(error);
            }
        } else if !status.is_success() {
            return Err(ByteStoreError::HttpStatus {
                key: request.key,
                operation: TransferOperation::Upload,
                status: status.as_u16(),
            });
        }
        let metadata = ObjectMetadata {
            key: request.key,
            kind: request.kind,
            byte_len: request.byte_len,
            hash: request.expected_hash.as_str().to_string(),
            key_epoch: request.key_epoch,
            created_by_device_id: request.created_by_device_id.cloned(),
            created_at_unix_ms: current_unix_ms(),
            retention_state: RetentionState::Pending,
            retain_until_unix_ms: None,
        };
        let mut metrics = self.metrics.borrow_mut();
        metrics.put_count += 1;
        metrics.bytes_uploaded += request.byte_len;
        metrics.peak_object_bytes_in_flight = metrics.peak_object_bytes_in_flight.max(64 * 1024);
        Ok(metadata)
    }

    fn supports_streaming_puts(&self) -> bool {
        true
    }

    fn put_object_with_content_id(
        &self,
        key: ObjectKey,
        kind: StorageObjectKind,
        content_id: &str,
        bytes: &[u8],
        created_by_device_id: Option<&DeviceId>,
    ) -> Result<ObjectMetadata, ByteStoreError> {
        SignedUrlByteStore::put_object_with_content_id(
            self,
            key,
            kind,
            content_id.to_string(),
            bytes,
            created_by_device_id,
        )
    }

    fn put_object_with_content_id_at_epoch(
        &self,
        key: ObjectKey,
        kind: StorageObjectKind,
        content_id: &str,
        bytes: &[u8],
        key_epoch: u32,
        created_by_device_id: Option<&DeviceId>,
    ) -> Result<ObjectMetadata, ByteStoreError> {
        SignedUrlByteStore::put_object_with_content_id_at_epoch(
            self,
            key,
            kind,
            content_id.to_string(),
            bytes,
            key_epoch,
            created_by_device_id,
        )
    }

    fn put_object(
        &self,
        key: ObjectKey,
        kind: StorageObjectKind,
        bytes: &[u8],
        created_by_device_id: Option<&DeviceId>,
    ) -> Result<ObjectMetadata, ByteStoreError> {
        SignedUrlByteStore::put_object_with_content_id(
            self,
            key,
            kind,
            stable_object_hash(bytes),
            bytes,
            created_by_device_id,
        )
    }

    fn get_object(&self, key: &ObjectKey) -> Result<Vec<u8>, ByteStoreError> {
        self.metrics.borrow_mut().convex_action_count += 1;
        let intent = self
            .control_plane
            .create_download_intent(DownloadIntentRequest::full(
                self.workspace_id.clone(),
                key.as_str(),
            ))
            .map_err(|error| map_control_error(TransferOperation::Download, error))?;
        let bytes = fetch_signed_url(&self.http, key, &intent.signed_url.url, None)?;

        let mut metrics = self.metrics.borrow_mut();
        metrics.full_read_count += 1;
        metrics.bytes_downloaded += bytes.len() as u64;

        Ok(bytes)
    }

    fn get_range(&self, key: &ObjectKey, range: ByteRange) -> Result<Vec<u8>, ByteStoreError> {
        self.metrics.borrow_mut().convex_action_count += 1;
        let intent = self
            .control_plane
            .create_download_intent(DownloadIntentRequest {
                workspace_id: WorkspaceId::new(self.workspace_id.clone()),
                object_key: key.as_str().to_string(),
                range: Some(range),
            })
            .map_err(|error| map_control_error(TransferOperation::Download, error))?;
        let bytes = fetch_signed_url(&self.http, key, &intent.signed_url.url, Some(range))?;

        let mut metrics = self.metrics.borrow_mut();
        metrics.range_read_count += 1;
        metrics.bytes_downloaded += bytes.len() as u64;

        Ok(bytes)
    }

    fn head_object(&self, key: &ObjectKey) -> Result<ObjectMetadata, ByteStoreError> {
        self.metrics.borrow_mut().convex_query_count += 1;
        let metadata = self
            .control_plane
            .head_object_metadata(&WorkspaceId::new(self.workspace_id.clone()), key.as_str())
            .map_err(|error| match error {
                ControlPlaneError::ObjectMissing { .. } => ByteStoreError::MissingObject {
                    key: key.clone(),
                    component: "hosted object metadata",
                },
                other => map_control_error(TransferOperation::Download, other),
            })?;
        self.metrics.borrow_mut().head_count += 1;
        Ok(metadata)
    }

    fn delete_object(&self, key: &ObjectKey) -> Result<(), ByteStoreError> {
        self.metrics.borrow_mut().convex_query_count += 1;
        let metadata = self
            .control_plane
            .head_object_metadata(&WorkspaceId::new(self.workspace_id.clone()), key.as_str())
            .map_err(|error| map_control_error(TransferOperation::Delete, error))?;
        self.metrics.borrow_mut().head_count += 1;
        if metadata.retention_state != RetentionState::DeleteEligible {
            return Err(ByteStoreError::UnsupportedOperation(
                "delete requires delete-eligible metadata",
            ));
        }

        self.metrics.borrow_mut().convex_action_count += 1;
        let intent = self
            .control_plane
            .create_storage_gc_delete_intent(
                &WorkspaceId::new(self.workspace_id.clone()),
                key.as_str(),
            )
            .map_err(|error| map_control_error(TransferOperation::Delete, error))?;

        let response = self
            .http
            .delete(&intent.signed_url.url)
            .send()
            .map_err(|error| map_http_error(TransferOperation::Delete, error))?;
        let status = response.status();
        if !status.is_success() {
            self.metrics.borrow_mut().retryable_failure_count += 1;
            return Err(ByteStoreError::HttpStatus {
                key: key.clone(),
                operation: TransferOperation::Delete,
                status: status.as_u16(),
            });
        }
        self.metrics.borrow_mut().delete_count += 1;
        Ok(())
    }

    fn creates_upload_intents(&self) -> bool {
        true
    }

    fn metrics(&self) -> ByteStoreMetrics {
        *self.metrics.borrow()
    }
}

fn fetch_signed_url(
    http: &Client,
    key: &ObjectKey,
    url: &str,
    range: Option<ByteRange>,
) -> Result<Vec<u8>, ByteStoreError> {
    let mut request = http.get(url);
    if let Some(range) = range {
        let end = range
            .offset
            .checked_add(range.length)
            .and_then(|value| value.checked_sub(1))
            .ok_or(ByteStoreError::RangeOutOfBounds {
                offset: range.offset,
                length: range.length,
                byte_len: 0,
            })?;
        request = request.header(
            reqwest::header::RANGE,
            format!("bytes={}-{}", range.offset, end),
        );
    }

    let response = request
        .send()
        .map_err(|error| map_http_error(TransferOperation::Download, error))?;
    let status = response.status();
    if !status.is_success() || (range.is_some() && status != reqwest::StatusCode::PARTIAL_CONTENT) {
        return Err(ByteStoreError::HttpStatus {
            key: key.clone(),
            operation: TransferOperation::Download,
            status: status.as_u16(),
        });
    }
    let bytes = response
        .bytes()
        .map(|bytes| bytes.to_vec())
        .map_err(|error| map_http_error(TransferOperation::Download, error))?;
    if let Some(range) = range
        && bytes.len() as u64 != range.length
    {
        return Err(ByteStoreError::CorruptObject {
            key: key.clone(),
            reason: "range response length does not match requested length",
        });
    }
    Ok(bytes)
}

fn current_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

fn map_control_error(operation: TransferOperation, error: ControlPlaneError) -> ByteStoreError {
    let kind = match &error {
        ControlPlaneError::Timeout { .. } => IntentFailureKind::Timeout,
        ControlPlaneError::Transport { .. } => IntentFailureKind::Transport,
        ControlPlaneError::Rejected {
            code:
                RejectionCode::DeviceNotTrusted
                | RejectionCode::Unauthorized
                | RejectionCode::WorkspaceMembershipRequired
                | RejectionCode::WorkspaceOwnerRequired,
            ..
        } => IntentFailureKind::DeviceNotTrusted,
        ControlPlaneError::Rejected {
            code: RejectionCode::InvalidRequest | RejectionCode::Unknown,
            ..
        }
        | ControlPlaneError::WorkspaceMissing { .. }
        | ControlPlaneError::WorkViewMissing { .. }
        | ControlPlaneError::LeaseMissing { .. }
        | ControlPlaneError::CompareAndSwap(_)
        | ControlPlaneError::InvalidObjectKey { .. }
        | ControlPlaneError::ObjectMissing { .. }
        | ControlPlaneError::DeviceRequestMissing { .. }
        | ControlPlaneError::Limited { .. }
        | ControlPlaneError::Unsupported { .. }
        | ControlPlaneError::Conflict { .. }
        | ControlPlaneError::Storage(_) => IntentFailureKind::Other,
    };
    ByteStoreError::IntentFailed {
        operation,
        kind,
        detail: error.to_string(),
    }
}

fn map_http_error(operation: TransferOperation, error: reqwest::Error) -> ByteStoreError {
    ByteStoreError::Network {
        operation,
        detail: error.without_url().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bowline_core::ids::PackId;
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
        let key = ObjectKey::new("packs_pk_00112233445566d4").expect("key");
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
        let key = ObjectKey::new("manifests_mf_0000000000000001").expect("object key");

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
        let pack_id = PackId::new("pk_00112233445566d5");
        let key = ObjectKey::from_pack_id(&pack_id).expect("object key");
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
                kind: StorageObjectKind::SourcePack,
                content_id: ObjectContentId::from_pack_id(&pack_id),
                source: &source,
                byte_len: bytes.len() as u64,
                expected_hash: ObjectHash::from_stable_hash(hash.clone()),
                key_epoch: 1,
                created_by_device_id: None,
            })
            .expect("matching existing object should verify");

        assert_eq!(metadata.key, key);
        assert_eq!(metadata.hash, hash);
        assert_eq!(source.opens.load(Ordering::Relaxed), 2);
        let metrics = store.metrics();
        assert_eq!(metrics.conditional_write_conflict_count, 1);
        assert_eq!(metrics.verification_failure_count, 0);
        assert_eq!(metrics.convex_action_count, 2);
        assert_eq!(metrics.put_count, 1);
        assert_eq!(metrics.bytes_uploaded, bytes.len() as u64);
    }

    #[test]
    fn range_fetch_rejects_full_body_success() {
        let client = Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("client");
        let key = ObjectKey::new("packs_pk_0000000000000001").expect("object key");
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
        let key = ObjectKey::new("packs_pk_0000000000000002").expect("object key");
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
        let key = ObjectKey::new("packs_pk_0000000000000004").expect("object key");
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
        let key = ObjectKey::new("packs_pk_0000000000000003").expect("object key");
        let url = signed_url_response("206 Partial Content", b"bc");

        let bytes = fetch_signed_url(&client, &key, &url, Some(ByteRange::new(1, 2)))
            .expect("exact 206 response");

        assert_eq!(bytes, b"bc");
    }
}
