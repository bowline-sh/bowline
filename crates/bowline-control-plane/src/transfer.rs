use std::{
    cell::RefCell,
    io,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use bowline_core::ids::DeviceId;
use bowline_storage::{
    ByteRange, ByteStore, ByteStoreError, ByteStoreMetrics, ObjectKey,
    ObjectKind as StorageObjectKind, ObjectMetadata, RetentionState,
};
use reqwest::blocking::Client;

use crate::{
    ControlPlaneClient, DeleteIntentRequest, DownloadIntentRequest,
    ObjectKind as ControlObjectKind, UploadIntentRequest, UploadVerificationIntentRequest,
};

const SIGNED_URL_HTTP_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub struct SignedUrlByteStore<'a, C> {
    control_plane: &'a C,
    workspace_id: String,
    http: Client,
    metrics: RefCell<ByteStoreMetrics>,
}

impl<'a, C: ControlPlaneClient> SignedUrlByteStore<'a, C> {
    pub fn new(control_plane: &'a C, workspace_id: impl Into<String>) -> Self {
        Self {
            control_plane,
            workspace_id: workspace_id.into(),
            http: Client::builder()
                .timeout(SIGNED_URL_HTTP_TIMEOUT)
                .build()
                .expect("reqwest client with timeout should build"),
            metrics: RefCell::default(),
        }
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
                    map_storage_kind(kind)?,
                    bytes.len() as u64,
                )
                .with_content_id(content_id.clone())
                .with_object_key(key.as_str()),
            )
            .map_err(map_control_error)?;

        let response = self
            .http
            .put(&intent.signed_url.url)
            .header(reqwest::header::IF_NONE_MATCH, "*")
            .body(bytes.to_vec())
            .send()
            .map_err(map_http_error)?;
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
            return Err(io_error(format!(
                "R2 upload for object `{}` returned HTTP {status}",
                key.as_str()
            )));
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
            .map_err(map_control_error)?;
        let existing = fetch_signed_url(&self.http, key, &intent.signed_url.url, None)?;
        if existing.len() as u64 != byte_len || stable_object_hash(&existing) != expected_hash {
            return Err(io_error(format!(
                "R2 existing upload for object `{}` does not match retry bytes",
                key.as_str()
            )));
        }
        if existing != expected_bytes {
            return Err(io_error(format!(
                "R2 existing upload for object `{}` differs from retry bytes",
                key.as_str()
            )));
        }
        Ok(())
    }
}

impl<C: ControlPlaneClient> ByteStore for SignedUrlByteStore<'_, C> {
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
            .map_err(map_control_error)?;
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
                workspace_id: self.workspace_id.clone(),
                object_key: key.as_str().to_string(),
                range: Some(range),
            })
            .map_err(map_control_error)?;
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
            .head_object_metadata(&self.workspace_id, key.as_str())
            .map_err(map_control_error)?;
        self.metrics.borrow_mut().head_count += 1;
        Ok(metadata)
    }

    fn delete_object(&self, key: &ObjectKey) -> Result<(), ByteStoreError> {
        self.metrics.borrow_mut().convex_query_count += 1;
        let metadata = self
            .control_plane
            .head_object_metadata(&self.workspace_id, key.as_str())
            .map_err(map_control_error)?;
        self.metrics.borrow_mut().head_count += 1;
        if metadata.retention_state != RetentionState::DeleteEligible {
            return Err(io_error(format!(
                "object `{}` is not delete-eligible",
                key.as_str()
            )));
        }

        self.metrics.borrow_mut().convex_action_count += 1;
        let intent = self
            .control_plane
            .create_delete_intent(
                DeleteIntentRequest::new(self.workspace_id.clone(), key.as_str())
                    .with_object_kind(map_storage_kind(metadata.kind)?)
                    .with_key_epoch(metadata.key_epoch),
            )
            .map_err(map_control_error)?;

        let response = self
            .http
            .delete(&intent.signed_url.url)
            .send()
            .map_err(map_http_error)?;
        let status = response.status();
        if !status.is_success() {
            self.metrics.borrow_mut().retryable_failure_count += 1;
            return Err(io_error(format!(
                "R2 delete for object `{}` returned HTTP {status}",
                key.as_str()
            )));
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

    let response = request.send().map_err(map_http_error)?;
    let status = response.status();
    if !status.is_success() {
        return Err(io_error(format!(
            "R2 download for object `{}` returned HTTP {status}",
            key.as_str()
        )));
    }
    response
        .bytes()
        .map(|bytes| bytes.to_vec())
        .map_err(map_http_error)
}

fn map_storage_kind(kind: StorageObjectKind) -> Result<ControlObjectKind, ByteStoreError> {
    match kind {
        StorageObjectKind::SourcePack => Ok(ControlObjectKind::SourcePack),
        StorageObjectKind::IndexPack => Ok(ControlObjectKind::IndexPack),
        StorageObjectKind::LocatorIndex => Ok(ControlObjectKind::LocatorIndex),
        StorageObjectKind::SnapshotManifest => Ok(ControlObjectKind::SnapshotManifest),
        StorageObjectKind::AgentOverlay => Ok(ControlObjectKind::AgentOverlay),
        _ => Err(io_error(format!(
            "hosted signed URL store does not support {kind:?}"
        ))),
    }
}

fn stable_object_hash(bytes: &[u8]) -> String {
    let hash = blake3::hash(bytes);
    format!("b3_{}", hash.to_hex())
}

fn current_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

fn map_control_error(error: impl ToString) -> ByteStoreError {
    io_error(error.to_string())
}

fn map_http_error(error: reqwest::Error) -> ByteStoreError {
    io_error(error.without_url().to_string())
}

fn io_error(message: impl Into<String>) -> ByteStoreError {
    ByteStoreError::Io(io::Error::other(message.into()))
}
