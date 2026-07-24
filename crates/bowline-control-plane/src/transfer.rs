use std::{
    cell::RefCell,
    io::Read,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use bowline_core::ids::{DeviceId, WorkspaceId};
use bowline_storage::{
    ByteRange, ByteStore, ByteStoreError, ByteStoreMetrics, IntentFailureKind, ObjectKey,
    ObjectKind as StorageObjectKind, ObjectMetadata, PutObjectReaderRequest, RetentionState,
    TransferOperation, stable_object_hash,
};
use reqwest::blocking::Client;
use sha2::{Digest, Sha256};

mod streaming_upload;
use streaming_upload::{
    StreamingPutRequest, send_streaming_put, send_streaming_put_with_create_only,
    verify_matching_readers,
};

use crate::{
    ControlPlaneClient, ControlPlaneError, DownloadIntentRequest, ObjectKind as ControlObjectKind,
    RejectionCode, Sha256Checksum, UploadIntentRequest, UploadVerificationIntentRequest,
};

const SIGNED_URL_HTTP_TIMEOUT: Duration = Duration::from_secs(30);

fn sha256_checksum_reader(reader: &mut dyn Read) -> Result<Sha256Checksum, ByteStoreError> {
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(Sha256Checksum::from_digest(hasher.finalize().into()))
}

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
        let checksum_sha256 = Sha256Checksum::for_bytes(bytes);
        self.metrics.borrow_mut().convex_action_count += 1;
        let intent = self
            .control_plane
            .create_upload_intent(
                UploadIntentRequest::new(
                    self.workspace_id.clone(),
                    ControlObjectKind::from(kind),
                    bytes.len() as u64,
                    checksum_sha256.clone(),
                )
                .with_content_id(content_id.clone())
                .with_object_key(key.as_str()),
            )
            .map_err(|error| map_control_error(TransferOperation::Upload, error))?;

        let response = self
            .http
            .put(&intent.signed_url.url)
            .header(reqwest::header::IF_NONE_MATCH, "*")
            .header("x-amz-checksum-sha256", checksum_sha256.as_str())
            .body(bytes.to_vec())
            .send()
            .map_err(|error| map_http_error(TransferOperation::Upload, error))?;
        let status = response.status();
        if status == reqwest::StatusCode::PRECONDITION_FAILED {
            self.metrics.borrow_mut().conditional_write_conflict_count += 1;
            if let Err(error) = self.verify_existing_upload(
                &key,
                bytes.len() as u64,
                &content_id,
                bytes,
                &expected_hash,
            ) {
                // Random-nonce envelopes re-seal the same logical object to
                // different ciphertext under the same content-id key. Greenfield
                // R2 residue can also leave a foreign object at that key. Either
                // way, create-only 412 + hash mismatch must recover by overwrite
                // rather than wedging preparation in referenced-by-upload.
                if !matches!(error, ByteStoreError::CorruptObject { .. }) {
                    return Err(error);
                }
                self.metrics.borrow_mut().verification_failure_count += 1;
                self.overwrite_object_bytes(&key, kind, &content_id, bytes)?;
            }
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

    fn overwrite_object_bytes(
        &self,
        key: &ObjectKey,
        kind: StorageObjectKind,
        content_id: &str,
        bytes: &[u8],
    ) -> Result<(), ByteStoreError> {
        let checksum_sha256 = Sha256Checksum::for_bytes(bytes);
        self.metrics.borrow_mut().convex_action_count += 1;
        let intent = self
            .control_plane
            .create_upload_intent(
                UploadIntentRequest::new(
                    self.workspace_id.clone(),
                    ControlObjectKind::from(kind),
                    bytes.len() as u64,
                    checksum_sha256.clone(),
                )
                .with_content_id(content_id)
                .with_object_key(key.as_str()),
            )
            .map_err(|error| map_control_error(TransferOperation::Upload, error))?;
        let response = self
            .http
            .put(&intent.signed_url.url)
            .header("x-amz-checksum-sha256", checksum_sha256.as_str())
            .body(bytes.to_vec())
            .send()
            .map_err(|error| map_http_error(TransferOperation::Upload, error))?;
        if !response.status().is_success() {
            return Err(ByteStoreError::HttpStatus {
                key: key.clone(),
                operation: TransferOperation::Upload,
                status: response.status().as_u16(),
            });
        }
        Ok(())
    }

    fn overwrite_object_reader(
        &self,
        request: &PutObjectReaderRequest<'_>,
    ) -> Result<(), ByteStoreError> {
        let checksum_sha256 = sha256_checksum_reader(request.source.open()?.as_mut())?;
        self.metrics.borrow_mut().convex_action_count += 1;
        let intent = self
            .control_plane
            .create_upload_intent(
                UploadIntentRequest::new(
                    self.workspace_id.clone(),
                    ControlObjectKind::from(request.kind),
                    request.byte_len,
                    checksum_sha256.clone(),
                )
                .with_content_id(request.content_id.as_str())
                .with_object_key(request.key.as_str()),
            )
            .map_err(|error| map_control_error(TransferOperation::Upload, error))?;
        let response = send_streaming_put_with_create_only(
            &self.http,
            &intent.signed_url.url,
            StreamingPutRequest {
                key: &request.key,
                source: request.source,
                byte_len: request.byte_len,
                expected_hash: request.expected_hash.as_str(),
                checksum_sha256: checksum_sha256.as_str(),
                create_only: false,
            },
        )?;
        if !response.status().is_success() {
            return Err(ByteStoreError::HttpStatus {
                key: request.key.clone(),
                operation: TransferOperation::Upload,
                status: response.status().as_u16(),
            });
        }
        Ok(())
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
        let checksum_sha256 = sha256_checksum_reader(request.source.open()?.as_mut())?;
        self.metrics.borrow_mut().convex_action_count += 1;
        let intent = self
            .control_plane
            .create_upload_intent(
                UploadIntentRequest::new(
                    self.workspace_id.clone(),
                    ControlObjectKind::from(request.kind),
                    request.byte_len,
                    checksum_sha256.clone(),
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
            checksum_sha256.as_str(),
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
                    return Err(error);
                }
                if !matches!(error, ByteStoreError::CorruptObject { .. }) {
                    return Err(error);
                }
                // Same recovery as the buffered put path: re-seal / R2 residue
                // under a content-id key must overwrite after hash mismatch.
                self.overwrite_object_reader(&request)?;
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
mod tests;
