use std::{
    cell::RefCell,
    io::Read,
    time::{SystemTime, UNIX_EPOCH},
};

use bowline_core::ids::WorkspaceId;
use bowline_storage::{
    ByteRange, ByteStore, ByteStoreError, ByteStoreMetrics, IntentFailureKind, ObjectHash,
    ObjectKey, ObjectMetadata, PutObjectRequest, PutObjectSource, ReopenableObjectSource,
    RetentionState, TransferOperation,
};
use reqwest::blocking::Client;
use sha2::{Digest, Sha256};

mod deadline;
mod streaming_upload;
use deadline::{signed_url_connect_timeout, signed_url_transfer_timeout};
use streaming_upload::{send_streaming_put, verify_matching_readers};

use crate::{
    ControlPlaneClient, ControlPlaneError, DownloadIntentRequest, ObjectKind as ControlObjectKind,
    RejectionCode, Sha256Checksum, UploadIntentOutcome, UploadIntentRequest,
    UploadVerificationIntentRequest,
};

fn sha256_checksum_source(
    source: &dyn ReopenableObjectSource,
) -> Result<Sha256Checksum, ByteStoreError> {
    let mut reader = source.open()?;
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

/// What a put had to do under a content-addressed key.
///
/// Sealing is convergent, so an object key is a pure function of the plaintext
/// and re-presenting content the workspace already holds is routine rather than
/// exceptional. Callers that owe a follow-up metadata commit must branch on
/// this: an already-committed object owes nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PutOutcome {
    /// The sealed bytes were transferred. The caller still owes the hosted
    /// metadata commit that makes the object referenceable.
    Uploaded(ObjectMetadata),
    /// The control plane already held committed metadata under this key. The
    /// key is the hash of the sealed bytes, so that is proof the exact bytes
    /// are stored: nothing was transferred and no commit is owed. Carries the
    /// server's committed row, not a locally constructed one.
    AlreadyCommitted(ObjectMetadata),
}

impl PutOutcome {
    pub fn metadata(&self) -> &ObjectMetadata {
        match self {
            Self::Uploaded(metadata) | Self::AlreadyCommitted(metadata) => metadata,
        }
    }

    pub fn into_metadata(self) -> ObjectMetadata {
        match self {
            Self::Uploaded(metadata) | Self::AlreadyCommitted(metadata) => metadata,
        }
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
                .connect_timeout(signed_url_connect_timeout())
                // No client-wide request deadline: each request sets its own,
                // derived from how many bytes it is moving.
                .timeout(None)
                .build()
                .expect("reqwest client with connect timeout should build"),
        )
    }

    /// Store the sealed bytes, reporting whether the control plane already held
    /// them. `ByteStore::put` drops that distinction; callers that owe a
    /// metadata commit must use this instead.
    pub fn put_object(&self, request: PutObjectRequest<'_>) -> Result<PutOutcome, ByteStoreError> {
        let outcome = match request.source {
            PutObjectSource::Bytes(bytes) => self.put_bytes(&request, bytes)?,
            PutObjectSource::Reader(source) => self.put_reader(&request, source)?,
        };
        let mut metrics = self.metrics.borrow_mut();
        metrics.put_count += 1;
        if matches!(outcome, PutOutcome::Uploaded(_)) {
            metrics.bytes_uploaded += request.byte_len;
        }
        Ok(outcome)
    }

    /// Perform the create-only transfer using an upload reservation obtained by
    /// a caller's batch request. The reservation is still checked against the
    /// immutable object request before its signed URL is used.
    pub fn put_object_with_upload_intent(
        &self,
        request: PutObjectRequest<'_>,
        intent: UploadIntentOutcome,
    ) -> Result<PutOutcome, ByteStoreError> {
        let PutObjectSource::Bytes(bytes) = request.source else {
            return Err(ByteStoreError::UnsupportedOperation(
                "batched upload requires buffered bytes",
            ));
        };
        let outcome = self.put_bytes_after_intent(&request, bytes, intent)?;
        let mut metrics = self.metrics.borrow_mut();
        metrics.put_count += 1;
        if matches!(outcome, PutOutcome::Uploaded(_)) {
            metrics.bytes_uploaded += request.byte_len;
        }
        Ok(outcome)
    }

    /// Upload the buffered form of an object. The identity is checked before
    /// the PUT starts so wrong bytes never reach R2 under a content-addressed
    /// key; the mid-stream check on the reader path is the same guarantee.
    fn put_bytes(
        &self,
        request: &PutObjectRequest<'_>,
        bytes: &[u8],
    ) -> Result<PutOutcome, ByteStoreError> {
        if bytes.len() as u64 != request.byte_len
            || ObjectHash::of_bytes(bytes) != request.expected_hash
        {
            return Err(ByteStoreError::CorruptObject {
                key: request.key.clone(),
                reason: "buffered upload bytes did not match immutable identity",
            });
        }
        let checksum_sha256 = Sha256Checksum::for_bytes(bytes);
        let intent = self.create_upload_intent(request, checksum_sha256)?;
        self.put_bytes_after_intent(request, bytes, intent)
    }

    fn put_bytes_after_intent(
        &self,
        request: &PutObjectRequest<'_>,
        bytes: &[u8],
        intent: UploadIntentOutcome,
    ) -> Result<PutOutcome, ByteStoreError> {
        if bytes.len() as u64 != request.byte_len
            || ObjectHash::of_bytes(bytes) != request.expected_hash
        {
            return Err(ByteStoreError::CorruptObject {
                key: request.key.clone(),
                reason: "buffered upload bytes did not match immutable identity",
            });
        }
        let checksum_sha256 = Sha256Checksum::for_bytes(bytes);
        let intent = match intent {
            UploadIntentOutcome::Reserved(intent) => intent,
            UploadIntentOutcome::AlreadyCommitted(committed) => {
                return Ok(PutOutcome::AlreadyCommitted(committed));
            }
        };
        if intent.workspace_id.as_str() != self.workspace_id
            || intent.object_key != request.key.as_str()
            || intent.object_kind != ControlObjectKind::from(request.kind)
            || intent.byte_len != request.byte_len
        {
            return Err(ByteStoreError::CorruptObject {
                key: request.key.clone(),
                reason: "upload reservation does not match immutable object request",
            });
        }

        let response = self
            .http
            .put(&intent.signed_url.url)
            .timeout(signed_url_transfer_timeout(Some(request.byte_len)))
            .header(reqwest::header::IF_NONE_MATCH, "*")
            .header("x-amz-checksum-sha256", checksum_sha256.as_str())
            .body(bytes.to_vec())
            .send()
            .map_err(|error| map_http_error(TransferOperation::Upload, error))?;
        let status = response.status();
        if status == reqwest::StatusCode::PRECONDITION_FAILED {
            self.metrics.borrow_mut().conditional_write_conflict_count += 1;
            if let Err(error) = self.verify_existing_upload(request, bytes) {
                if !matches!(error, ByteStoreError::CorruptObject { .. }) {
                    return Err(error);
                }
                self.metrics.borrow_mut().verification_failure_count += 1;
                return Err(immutable_identity_violation(request));
            }
        } else if !status.is_success() {
            return Err(ByteStoreError::HttpStatus {
                key: request.key.clone(),
                operation: TransferOperation::Upload,
                status: status.as_u16(),
            });
        }
        Ok(PutOutcome::Uploaded(uploaded_metadata(request)))
    }

    fn put_reader(
        &self,
        request: &PutObjectRequest<'_>,
        source: &dyn ReopenableObjectSource,
    ) -> Result<PutOutcome, ByteStoreError> {
        let checksum_sha256 = sha256_checksum_source(source)?;
        let intent = match self.create_upload_intent(request, checksum_sha256.clone())? {
            UploadIntentOutcome::Reserved(intent) => intent,
            UploadIntentOutcome::AlreadyCommitted(committed) => {
                return Ok(PutOutcome::AlreadyCommitted(committed));
            }
        };
        let response = send_streaming_put(
            &self.http,
            &intent.signed_url.url,
            &request.key,
            source,
            request.byte_len,
            request.expected_hash.as_str(),
            checksum_sha256.as_str(),
        )?;
        let status = response.status();
        if status == reqwest::StatusCode::PRECONDITION_FAILED {
            self.metrics.borrow_mut().conditional_write_conflict_count += 1;
            if let Err(error) = self.verify_existing_upload_source(request, source) {
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
                return Err(immutable_identity_violation(request));
            }
        } else if !status.is_success() {
            return Err(ByteStoreError::HttpStatus {
                key: request.key.clone(),
                operation: TransferOperation::Upload,
                status: status.as_u16(),
            });
        }
        self.record_streamed_peak();
        Ok(PutOutcome::Uploaded(uploaded_metadata(request)))
    }

    fn record_streamed_peak(&self) {
        let mut metrics = self.metrics.borrow_mut();
        metrics.peak_object_bytes_in_flight = metrics.peak_object_bytes_in_flight.max(64 * 1024);
    }

    fn create_upload_intent(
        &self,
        request: &PutObjectRequest<'_>,
        checksum_sha256: Sha256Checksum,
    ) -> Result<UploadIntentOutcome, ByteStoreError> {
        self.metrics.borrow_mut().convex_action_count += 1;
        self.control_plane
            .create_upload_intent(
                UploadIntentRequest::new(
                    self.workspace_id.clone(),
                    ControlObjectKind::from(request.kind),
                    request.byte_len,
                    checksum_sha256,
                )
                .with_content_id(request.content_id.as_str())
                .with_object_key(request.key.as_str()),
            )
            .map_err(|error| map_control_error(TransferOperation::Upload, error))
    }

    fn verify_existing_upload(
        &self,
        request: &PutObjectRequest<'_>,
        expected_bytes: &[u8],
    ) -> Result<(), ByteStoreError> {
        let intent = self.create_upload_verification_intent(request)?;
        let existing = fetch_signed_url(&self.http, &request.key, &intent.signed_url.url, None)?;
        if existing.len() as u64 != request.byte_len
            || ObjectHash::of_bytes(&existing) != request.expected_hash
        {
            return Err(ByteStoreError::CorruptObject {
                key: request.key.clone(),
                reason: "existing upload does not match retry bytes",
            });
        }
        if existing != expected_bytes {
            return Err(ByteStoreError::CorruptObject {
                key: request.key.clone(),
                reason: "existing upload differs from retry bytes",
            });
        }
        Ok(())
    }

    fn verify_existing_upload_source(
        &self,
        request: &PutObjectRequest<'_>,
        source: &dyn ReopenableObjectSource,
    ) -> Result<(), ByteStoreError> {
        let intent = self.create_upload_verification_intent(request)?;
        let mut response = self
            .http
            .get(&intent.signed_url.url)
            .timeout(signed_url_transfer_timeout(Some(request.byte_len)))
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
            source.open()?.as_mut(),
            request.byte_len,
            request.expected_hash.as_str(),
        )
    }

    fn create_upload_verification_intent(
        &self,
        request: &PutObjectRequest<'_>,
    ) -> Result<crate::DownloadIntent, ByteStoreError> {
        self.metrics.borrow_mut().convex_action_count += 1;
        self.control_plane
            .create_upload_verification_intent(
                UploadVerificationIntentRequest::new(
                    self.workspace_id.clone(),
                    request.key.as_str(),
                    request.byte_len,
                )
                .with_content_id(request.content_id.as_str()),
            )
            .map_err(|error| map_control_error(TransferOperation::Upload, error))
    }
}

fn immutable_identity_violation(request: &PutObjectRequest<'_>) -> ByteStoreError {
    ByteStoreError::IntegrityViolation {
        key: request.key.clone(),
        reason: "existing bytes do not match the content-addressed key",
    }
}

/// The metadata a completed transfer establishes locally. Retention is
/// `Pending` because only the hosted commit that follows can make the object
/// current; an already-committed object carries the server's row instead.
fn uploaded_metadata(request: &PutObjectRequest<'_>) -> ObjectMetadata {
    ObjectMetadata {
        key: request.key.clone(),
        kind: request.kind,
        byte_len: request.byte_len,
        hash: request.expected_hash.as_str().to_string(),
        key_epoch: request.key_epoch,
        created_by_device_id: request.created_by_device_id.cloned(),
        created_at_unix_ms: current_unix_ms(),
        retention_state: RetentionState::Pending,
        retain_until_unix_ms: None,
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
    fn put(&self, request: PutObjectRequest<'_>) -> Result<ObjectMetadata, ByteStoreError> {
        Ok(self.put_object(request)?.into_metadata())
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

    /// Streams the object straight into `writer`, so a multi-gigabyte object
    /// never has to be resident.
    fn get_object_to_writer(
        &self,
        key: &ObjectKey,
        writer: &mut dyn std::io::Write,
    ) -> Result<u64, ByteStoreError> {
        self.metrics.borrow_mut().convex_action_count += 1;
        let intent = self
            .control_plane
            .create_download_intent(DownloadIntentRequest::full(
                self.workspace_id.clone(),
                key.as_str(),
            ))
            .map_err(|error| map_control_error(TransferOperation::Download, error))?;
        let mut response = self
            .http
            .get(&intent.signed_url.url)
            .timeout(signed_url_transfer_timeout(None))
            .send()
            .map_err(|error| map_http_error(TransferOperation::Download, error))?;
        let status = response.status();
        if !status.is_success() {
            return Err(ByteStoreError::HttpStatus {
                key: key.clone(),
                operation: TransferOperation::Download,
                status: status.as_u16(),
            });
        }
        let byte_len = std::io::copy(&mut response, writer)?;

        let mut metrics = self.metrics.borrow_mut();
        metrics.full_read_count += 1;
        metrics.bytes_downloaded += byte_len;

        Ok(byte_len)
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
        let metadata = match self
            .control_plane
            .head_object_metadata(&WorkspaceId::new(self.workspace_id.clone()), key.as_str())
        {
            Ok(metadata) => metadata,
            // Idempotent: an interrupted sweep that already removed the metadata
            // row must not wedge the next sweep on the same key.
            Err(ControlPlaneError::ObjectMissing { .. }) => return Ok(()),
            Err(error) => return Err(map_control_error(TransferOperation::Delete, error)),
        };
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
            .timeout(signed_url_transfer_timeout(Some(0)))
            .send()
            .map_err(|error| map_http_error(TransferOperation::Delete, error))?;
        let status = response.status();
        // R2 answers 204 for an absent key, but treat an explicit 404 the same
        // way: the post-condition (no bytes at this key) already holds.
        if !status.is_success() && status != reqwest::StatusCode::NOT_FOUND {
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
    let mut request = http
        .get(url)
        .timeout(signed_url_transfer_timeout(range.map(|range| range.length)));
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
    ByteStoreError::IntentFailed {
        operation,
        kind: intent_failure_kind(&error),
        detail: error.to_string(),
    }
}

/// Translate a control-plane failure into the storage layer's failure taxonomy.
/// The arms are exhaustive on purpose: a new `ControlPlaneError` variant must
/// be classified here rather than silently defaulting to `Other`.
fn intent_failure_kind(error: &ControlPlaneError) -> IntentFailureKind {
    match error {
        ControlPlaneError::Timeout { .. } => IntentFailureKind::Timeout,
        // A server exception is a remote fault the caller can retry, exactly
        // like a dropped connection.
        ControlPlaneError::Transport { .. } | ControlPlaneError::ServerError { .. } => {
            IntentFailureKind::Transport
        }
        ControlPlaneError::Rejected {
            code:
                RejectionCode::AccountSessionExpired
                | RejectionCode::AccountSessionMissing
                | RejectionCode::AccountSessionRevoked
                | RejectionCode::DeviceNotTrusted
                | RejectionCode::Unauthorized
                | RejectionCode::WorkspaceMembershipRequired
                | RejectionCode::WorkspaceOwnerRequired,
            ..
        } => IntentFailureKind::DeviceNotTrusted,
        ControlPlaneError::Rejected {
            code:
                RejectionCode::Conflict
                | RejectionCode::Expired
                | RejectionCode::InvalidRequest
                | RejectionCode::Unknown,
            ..
        }
        | ControlPlaneError::WorkspaceMissing { .. }
        | ControlPlaneError::CompareAndSwap(_)
        | ControlPlaneError::InvalidObjectKey { .. }
        | ControlPlaneError::ObjectMissing { .. }
        | ControlPlaneError::DeviceRequestMissing { .. }
        | ControlPlaneError::Unsupported { .. }
        | ControlPlaneError::Conflict { .. }
        | ControlPlaneError::ContractViolation { .. }
        | ControlPlaneError::ContractSkew { .. }
        | ControlPlaneError::ResponseShape { .. }
        // Not `DeviceNotTrusted`: that kind means the control plane refused this
        // device, while an unknown signer is another device this host has not
        // learned yet. No object intent is signed by a peer, so this cannot
        // arise here; classifying it as a refusal of the local device would make
        // the storage taxonomy lie if it ever did.
        | ControlPlaneError::UnknownSigningDevice { .. }
        | ControlPlaneError::Internal { .. } => IntentFailureKind::Other,
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
