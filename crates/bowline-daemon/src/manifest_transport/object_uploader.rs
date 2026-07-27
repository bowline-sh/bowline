//! The stateless half of the object transport: reserve, create-only PUT, commit
//! hosted metadata, validate the commit response.
//!
//! Split out from `ManifestTransport` so it holds no interior mutability and is
//! therefore `Sync`. That is what lets [`super::upload_pipeline`] hand the same
//! `&ObjectUploader` to a scoped worker pool: the engine thread stays
//! single-threaded and deterministic *because* the I/O is offloaded, not by
//! doing every round trip inline.

use std::path::Path;

use bowline_control_plane::{
    ControlPlaneClient, ControlPlaneTimestamp, ObjectKind as ControlObjectKind,
    ObjectMetadataCommit, ObjectPointer, PutOutcome, SignedUrlByteStore, SignedUrlHttpClient,
};
use bowline_core::ids::{ContentId, DeviceId, WorkspaceId};
use bowline_local::sync::manifest_engine::{KeyEpoch, TransportError};
use bowline_storage::{
    ByteStore, ObjectContentId, ObjectHash, ObjectKey, ObjectKind as StorageObjectKind,
    ObjectMetadata, PutObjectRequest, PutObjectSource, stable_object_hash,
};

use super::helpers::{
    SpoolSource, byte_store_error, committed_metadata_error, control_plane_error, hash_spool,
    parse_object_key,
};

/// `stable_object_hash` prefixes every digest with `b3_`; the sealed-hash suffix
/// after it must equal the physical object key's hex, which is the entire
/// server-side integrity contract for `b_`/`m_` keys (Plan 110).
const STABLE_OBJECT_HASH_PREFIX: &str = "b3_";

// ---- upload kind dispatch ---------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum UploadKind {
    Blob,
    Manifest,
}

impl UploadKind {
    fn storage_kind(self) -> StorageObjectKind {
        match self {
            Self::Blob => StorageObjectKind::WorkspaceFileV1,
            Self::Manifest => StorageObjectKind::WorkspaceManifestV1,
        }
    }

    fn control_kind(self) -> ControlObjectKind {
        match self {
            Self::Blob => ControlObjectKind::Blob,
            Self::Manifest => ControlObjectKind::Manifest,
        }
    }

    fn key_prefix(self) -> &'static str {
        match self {
            Self::Blob => ObjectKey::BLOB_PREFIX,
            Self::Manifest => ObjectKey::MANIFEST_PREFIX,
        }
    }

    fn put_operation(self) -> &'static str {
        match self {
            Self::Blob => "put-blob",
            Self::Manifest => "put-manifest",
        }
    }

    fn commit_operation(self) -> &'static str {
        match self {
            Self::Blob => "commit-blob",
            Self::Manifest => "commit-manifest",
        }
    }
}

// ---- upload requests --------------------------------------------------------

/// One buffered sealed object to reserve, PUT create-only, and commit.
pub(super) struct BufferedUpload<'a> {
    pub(super) kind: UploadKind,
    pub(super) content_id: &'a ContentId,
    pub(super) key: &'a str,
    pub(super) sealed: &'a [u8],
    pub(super) key_epoch: KeyEpoch,
}

/// One large sealed object streamed from a 0600 on-disk spool.
pub(super) struct StreamedUpload<'a> {
    pub(super) content_id: &'a ContentId,
    pub(super) key: &'a str,
    pub(super) spool_path: &'a Path,
    pub(super) byte_len: u64,
    pub(super) key_epoch: KeyEpoch,
}

// ---- uploader ---------------------------------------------------------------

/// Shared, immutable transport state for one workspace. A fresh
/// [`SignedUrlByteStore`] is built per call: it is a borrow plus a cloned
/// `reqwest` handle (an `Arc` bump onto the shared connection pool), and keeping
/// it out of `self` is what keeps this type `Sync`.
pub(super) struct ObjectUploader<'a, C> {
    control_plane: &'a C,
    workspace_id: WorkspaceId,
    device_id: DeviceId,
    http: SignedUrlHttpClient,
}

impl<'a, C: ControlPlaneClient> ObjectUploader<'a, C> {
    pub(super) fn new(
        control_plane: &'a C,
        workspace_id: WorkspaceId,
        device_id: DeviceId,
        http: SignedUrlHttpClient,
    ) -> Self {
        Self {
            control_plane,
            workspace_id,
            device_id,
            http,
        }
    }

    pub(super) fn control_plane(&self) -> &'a C {
        self.control_plane
    }

    pub(super) fn workspace_id(&self) -> &WorkspaceId {
        &self.workspace_id
    }

    pub(super) fn device_id(&self) -> &DeviceId {
        &self.device_id
    }

    fn store(&self) -> SignedUrlByteStore<'a, C> {
        SignedUrlByteStore::with_http_client(
            self.control_plane,
            self.workspace_id.as_str(),
            self.http.clone(),
        )
    }

    /// Reserve + create-only PUT a buffered sealed object, then complete it.
    pub(super) fn upload_buffered(&self, upload: BufferedUpload<'_>) -> Result<(), TransportError> {
        let object_key = parse_object_key(upload.key)?;
        let outcome = self
            .store()
            .put_object(PutObjectRequest {
                key: object_key,
                kind: upload.kind.storage_kind(),
                content_id: ObjectContentId::new(upload.content_id.as_str()),
                source: PutObjectSource::Bytes(upload.sealed),
                byte_len: upload.sealed.len() as u64,
                expected_hash: ObjectHash::from_stable_hash(stable_object_hash(upload.sealed)),
                key_epoch: upload.key_epoch.get(),
                created_by_device_id: Some(&self.device_id),
            })
            .map_err(|error| byte_store_error(upload.kind.put_operation(), error))?;
        self.settle_upload(
            outcome,
            CompletionRequest {
                kind: upload.kind,
                content_id: upload.content_id,
                key: upload.key,
                expected_byte_len: upload.sealed.len() as u64,
                key_epoch: upload.key_epoch,
            },
        )
    }

    /// Reserve + streamed create-only PUT of a sealed object spooled to disk.
    pub(super) fn upload_streaming(
        &self,
        upload: StreamedUpload<'_>,
    ) -> Result<(), TransportError> {
        let object_key = parse_object_key(upload.key)?;
        let expected_hash = hash_spool(upload.spool_path)?;
        let source = SpoolSource {
            path: upload.spool_path.to_path_buf(),
        };
        let outcome = self
            .store()
            .put_object(PutObjectRequest {
                key: object_key,
                kind: StorageObjectKind::WorkspaceFileV1,
                content_id: ObjectContentId::new(upload.content_id.as_str()),
                source: PutObjectSource::Reader(&source),
                byte_len: upload.byte_len,
                expected_hash: ObjectHash::from_stable_hash(expected_hash),
                key_epoch: upload.key_epoch.get(),
                created_by_device_id: Some(&self.device_id),
            })
            .map_err(|error| byte_store_error("put-blob-reader", error))?;
        self.settle_upload(
            outcome,
            CompletionRequest {
                kind: UploadKind::Blob,
                content_id: upload.content_id,
                key: upload.key,
                expected_byte_len: upload.byte_len,
                key_epoch: upload.key_epoch,
            },
        )
    }

    /// Make the object referenceable, or recognise that it already is.
    ///
    /// Both arms end in the same fail-closed check, so an already-present object
    /// is trusted on exactly the evidence a fresh upload is: the hosted row must
    /// match the identity this device was about to publish.
    fn settle_upload(
        &self,
        outcome: PutOutcome,
        request: CompletionRequest<'_>,
    ) -> Result<(), TransportError> {
        match outcome {
            PutOutcome::Uploaded(metadata) => self.complete_upload(&metadata, request),
            // The key is the hash of the sealed bytes, so a committed row under
            // it means the object is already recorded and referenceable. A
            // second commit would only re-assert what the server just told us.
            PutOutcome::AlreadyCommitted(committed) => {
                validate_committed_metadata(CommittedMetadataExpectation {
                    key_prefix: request.kind.key_prefix(),
                    key: request.key,
                    expected_hash: &committed.hash,
                    expected_byte_len: request.expected_byte_len,
                    expected_key_epoch: request.key_epoch,
                    committed: &committed,
                })
            }
        }
    }

    /// Commit hosted metadata and fail closed on any returned-field mismatch.
    /// The commit must land before this returns success so nothing references an
    /// object the hosted service has not recorded (Plan 108).
    fn complete_upload(
        &self,
        metadata: &ObjectMetadata,
        request: CompletionRequest<'_>,
    ) -> Result<(), TransportError> {
        let pointer = ObjectPointer {
            object_key: metadata.key.as_str().to_string(),
            content_id: request.content_id.clone(),
            byte_len: metadata.byte_len,
            hash: metadata.hash.clone(),
            key_epoch: metadata.key_epoch,
            kind: request.kind.control_kind(),
            created_at: ControlPlaneTimestamp {
                tick: metadata.created_at_unix_ms,
            },
        };
        let committed = self
            .control_plane
            .commit_uploaded_object_metadata(ObjectMetadataCommit {
                workspace_id: self.workspace_id.clone(),
                object: pointer,
                committed_by_device_id: self.device_id.clone(),
            })
            .map_err(|error| control_plane_error(request.kind.commit_operation(), error))?;
        validate_committed_metadata(CommittedMetadataExpectation {
            key_prefix: request.kind.key_prefix(),
            key: request.key,
            expected_hash: &metadata.hash,
            expected_byte_len: request.expected_byte_len,
            expected_key_epoch: request.key_epoch,
            committed: &committed,
        })
    }

    pub(super) fn download(
        &self,
        operation: &'static str,
        key: &str,
    ) -> Result<Vec<u8>, TransportError> {
        let object_key = parse_object_key(key)?;
        self.store()
            .get_object(&object_key)
            .map_err(|error| byte_store_error(operation, error))
    }
}

struct CompletionRequest<'a> {
    kind: UploadKind,
    content_id: &'a ContentId,
    key: &'a str,
    expected_byte_len: u64,
    key_epoch: KeyEpoch,
}

// ---- committed metadata validation -----------------------------------------

pub(super) struct CommittedMetadataExpectation<'a> {
    pub(super) key_prefix: &'a str,
    pub(super) key: &'a str,
    pub(super) expected_hash: &'a str,
    pub(super) expected_byte_len: u64,
    pub(super) expected_key_epoch: KeyEpoch,
    pub(super) committed: &'a ObjectMetadata,
}

/// Fail closed unless the hosted commit response matches every dimension the
/// engine will later trust. The commit action verifies R2 existence and returns
/// the just-committed row, so a second hosted read adds no safety.
pub(super) fn validate_committed_metadata(
    expectation: CommittedMetadataExpectation<'_>,
) -> Result<(), TransportError> {
    let Some(hash_suffix) = expectation
        .expected_hash
        .strip_prefix(STABLE_OBJECT_HASH_PREFIX)
    else {
        return Err(committed_metadata_error("hash-format"));
    };
    if expectation.key != format!("{}{hash_suffix}", expectation.key_prefix) {
        return Err(committed_metadata_error("key-hash-coupling"));
    }
    if expectation.committed.key.as_str() != expectation.key {
        return Err(committed_metadata_error("key"));
    }
    if expectation.committed.hash != expectation.expected_hash {
        return Err(committed_metadata_error("hash"));
    }
    if expectation.committed.byte_len != expectation.expected_byte_len {
        return Err(committed_metadata_error("byte-length"));
    }
    if expectation.committed.key_epoch != expectation.expected_key_epoch.get() {
        return Err(committed_metadata_error("key-epoch"));
    }
    Ok(())
}
