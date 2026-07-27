//! Convergence tests for the storage GC sweep.
//!
//! The scenarios that matter are the interrupted ones: a sweep is only ever cut
//! in half between deleting an object's bytes and deleting its metadata row, and
//! the next sweep has to finish the job without a human.

use std::cell::RefCell;
use std::collections::BTreeMap;

use bowline_core::ids::WorkspaceId;
use bowline_storage::{
    ByteStore, ByteStoreError, ByteStoreMetrics, ObjectKey, ObjectMetadata, PutObjectRequest,
    RetentionState, StorageObjectRef,
};

use crate::{
    ControlPlaneError, ControlPlaneResult, DeleteIntent, DownloadIntent, DownloadIntentRequest,
    ObjectControlPlaneClient, ObjectMetadataCommit, ObjectRetentionStateUpdate,
    UploadIntentOutcome, UploadIntentRequest, UploadVerificationIntentRequest,
};

use super::{StorageGcSweepVerdict, sweep_storage_gc_until_converged};

const WORKSPACE: &str = "ws_gc_convergence";

fn object_key(seed: char) -> ObjectKey {
    ObjectKey::new(format!("b_{}", seed.to_string().repeat(64))).expect("test object key")
}

fn eligible(key: ObjectKey) -> StorageObjectRef {
    StorageObjectRef {
        key,
        retention_state: RetentionState::DeleteEligible,
        referenced_by_current_head: false,
        referenced_by_snapshot: None,
        referenced_by_work_view_base: false,
        referenced_by_active_overlay: false,
        verified: true,
    }
}

// ---- doubles ----------------------------------------------------------------

/// A control plane holding exactly the two things a sweep reads and writes: the
/// list of GC-eligible objects, and whether a metadata row still exists.
struct GcControlPlane {
    rows: RefCell<BTreeMap<String, StorageObjectRef>>,
    metadata_delete_failures: RefCell<usize>,
}

impl GcControlPlane {
    fn with_rows(rows: impl IntoIterator<Item = StorageObjectRef>) -> Self {
        Self {
            rows: RefCell::new(
                rows.into_iter()
                    .map(|row| (row.key.as_str().to_string(), row))
                    .collect(),
            ),
            metadata_delete_failures: RefCell::new(0),
        }
    }

    fn failing_metadata_deletes(self, failures: usize) -> Self {
        *self.metadata_delete_failures.borrow_mut() = failures;
        self
    }

    fn remaining_rows(&self) -> usize {
        self.rows.borrow().len()
    }

    fn unsupported<T>(operation: &'static str) -> ControlPlaneResult<T> {
        Err(ControlPlaneError::Internal { reason: operation })
    }
}

impl ObjectControlPlaneClient for GcControlPlane {
    fn create_upload_intent(
        &self,
        _request: UploadIntentRequest,
    ) -> ControlPlaneResult<UploadIntentOutcome> {
        Self::unsupported("create_upload_intent is not part of a GC sweep")
    }

    fn create_download_intent(
        &self,
        _request: DownloadIntentRequest,
    ) -> ControlPlaneResult<DownloadIntent> {
        Self::unsupported("create_download_intent is not part of a GC sweep")
    }

    fn create_upload_verification_intent(
        &self,
        _request: UploadVerificationIntentRequest,
    ) -> ControlPlaneResult<DownloadIntent> {
        Self::unsupported("create_upload_verification_intent is not part of a GC sweep")
    }

    fn mark_object_retention_state(
        &self,
        _update: ObjectRetentionStateUpdate,
    ) -> ControlPlaneResult<ObjectMetadata> {
        Self::unsupported("mark_object_retention_state is not part of a GC sweep")
    }

    fn create_storage_gc_delete_intent(
        &self,
        _workspace_id: &WorkspaceId,
        _object_key: &str,
    ) -> ControlPlaneResult<DeleteIntent> {
        Self::unsupported("the local byte store does not use delete intents")
    }

    fn head_object_metadata(
        &self,
        _workspace_id: &WorkspaceId,
        _object_key: &str,
    ) -> ControlPlaneResult<ObjectMetadata> {
        Self::unsupported("head_object_metadata is not part of a GC sweep")
    }

    fn list_storage_gc_objects(
        &self,
        _workspace_id: &WorkspaceId,
    ) -> ControlPlaneResult<Vec<StorageObjectRef>> {
        Ok(self.rows.borrow().values().cloned().collect())
    }

    fn delete_object_metadata_after_gc(
        &self,
        _workspace_id: &WorkspaceId,
        object_key: &str,
    ) -> ControlPlaneResult<bool> {
        let mut remaining_failures = self.metadata_delete_failures.borrow_mut();
        if *remaining_failures > 0 {
            *remaining_failures -= 1;
            return Err(ControlPlaneError::Internal {
                reason: "metadata delete rejected",
            });
        }
        Ok(self.rows.borrow_mut().remove(object_key).is_some())
    }

    fn commit_uploaded_object_metadata(
        &self,
        _commit: ObjectMetadataCommit,
    ) -> ControlPlaneResult<ObjectMetadata> {
        Self::unsupported("commit_uploaded_object_metadata is not part of a GC sweep")
    }
}

/// A byte store whose objects are already gone — the state a sweep interrupted
/// between its two halves leaves behind.
struct AlreadyEmptyStore;

impl ByteStore for AlreadyEmptyStore {
    fn put(&self, _request: PutObjectRequest<'_>) -> Result<ObjectMetadata, ByteStoreError> {
        Err(ByteStoreError::UnsupportedOperation("put"))
    }

    fn get_object_to_writer(
        &self,
        key: &ObjectKey,
        _writer: &mut dyn std::io::Write,
    ) -> Result<u64, ByteStoreError> {
        Err(ByteStoreError::MissingObject {
            key: key.clone(),
            component: "object",
        })
    }

    fn get_object(&self, key: &ObjectKey) -> Result<Vec<u8>, ByteStoreError> {
        Err(ByteStoreError::MissingObject {
            key: key.clone(),
            component: "object",
        })
    }

    fn get_range(
        &self,
        key: &ObjectKey,
        _range: bowline_storage::ByteRange,
    ) -> Result<Vec<u8>, ByteStoreError> {
        Err(ByteStoreError::MissingObject {
            key: key.clone(),
            component: "object",
        })
    }

    fn head_object(&self, key: &ObjectKey) -> Result<ObjectMetadata, ByteStoreError> {
        Err(ByteStoreError::MissingObject {
            key: key.clone(),
            component: "metadata",
        })
    }

    /// Deleting bytes that are already absent is a no-op, not an error: the
    /// post-condition already holds.
    fn delete_object(&self, _key: &ObjectKey) -> Result<(), ByteStoreError> {
        Ok(())
    }

    fn metrics(&self) -> ByteStoreMetrics {
        ByteStoreMetrics::default()
    }
}

/// A byte store that can never delete, so a sweep against it can only stall.
struct UndeletableStore;

impl ByteStore for UndeletableStore {
    fn put(&self, _request: PutObjectRequest<'_>) -> Result<ObjectMetadata, ByteStoreError> {
        Err(ByteStoreError::UnsupportedOperation("put"))
    }

    fn get_object_to_writer(
        &self,
        key: &ObjectKey,
        _writer: &mut dyn std::io::Write,
    ) -> Result<u64, ByteStoreError> {
        Err(ByteStoreError::MissingObject {
            key: key.clone(),
            component: "object",
        })
    }

    fn get_object(&self, key: &ObjectKey) -> Result<Vec<u8>, ByteStoreError> {
        Err(ByteStoreError::MissingObject {
            key: key.clone(),
            component: "object",
        })
    }

    fn get_range(
        &self,
        key: &ObjectKey,
        _range: bowline_storage::ByteRange,
    ) -> Result<Vec<u8>, ByteStoreError> {
        Err(ByteStoreError::MissingObject {
            key: key.clone(),
            component: "object",
        })
    }

    fn head_object(&self, key: &ObjectKey) -> Result<ObjectMetadata, ByteStoreError> {
        Err(ByteStoreError::MissingObject {
            key: key.clone(),
            component: "metadata",
        })
    }

    fn delete_object(&self, _key: &ObjectKey) -> Result<(), ByteStoreError> {
        Err(ByteStoreError::UnsupportedOperation("delete_object"))
    }

    fn metrics(&self) -> ByteStoreMetrics {
        ByteStoreMetrics::default()
    }
}

// ---- tests ------------------------------------------------------------------

/// The partially-completed delete: bytes gone, metadata row still listed. One
/// sweep must finish it and prove it finished, not leave it for next time.
#[test]
fn sweep_converges_past_a_partially_completed_delete() {
    let control_plane =
        GcControlPlane::with_rows([eligible(object_key('a')), eligible(object_key('b'))]);

    let sweep = sweep_storage_gc_until_converged(
        &control_plane,
        &WorkspaceId::new(WORKSPACE),
        &AlreadyEmptyStore,
    )
    .expect("sweep runs");

    assert_eq!(sweep.verdict, StorageGcSweepVerdict::Converged);
    assert_eq!(control_plane.remaining_rows(), 0);
    // First pass clears both rows; the second observes an empty workspace and
    // is what proves convergence rather than assuming it.
    assert_eq!(sweep.passes.len(), 2);
    assert_eq!(sweep.passes[0].metadata_deleted.len(), 2);
}

/// A metadata delete that fails once must be retried by the next pass rather
/// than leaving a row behind for an unbounded time.
#[test]
fn sweep_retries_a_failed_metadata_delete_on_the_next_pass() {
    let control_plane =
        GcControlPlane::with_rows([eligible(object_key('a')), eligible(object_key('b'))])
            .failing_metadata_deletes(1);

    let sweep = sweep_storage_gc_until_converged(
        &control_plane,
        &WorkspaceId::new(WORKSPACE),
        &AlreadyEmptyStore,
    )
    .expect("sweep runs");

    assert_eq!(sweep.verdict, StorageGcSweepVerdict::Converged);
    assert_eq!(control_plane.remaining_rows(), 0);
    assert_eq!(sweep.passes[0].metadata_failures.len(), 1);
}

/// A sweep that cannot make progress must say so once instead of looping.
#[test]
fn sweep_reports_a_stall_rather_than_retrying_forever() {
    let control_plane = GcControlPlane::with_rows([eligible(object_key('a'))]);

    let sweep = sweep_storage_gc_until_converged(
        &control_plane,
        &WorkspaceId::new(WORKSPACE),
        &UndeletableStore,
    )
    .expect("sweep runs");

    assert_eq!(
        sweep.verdict,
        StorageGcSweepVerdict::Stalled { unfinished: 1 }
    );
    assert_eq!(sweep.passes.len(), 1);
    assert_eq!(control_plane.remaining_rows(), 1);
}

#[test]
fn sweep_of_an_empty_workspace_converges_in_one_pass() {
    let control_plane = GcControlPlane::with_rows([]);

    let sweep = sweep_storage_gc_until_converged(
        &control_plane,
        &WorkspaceId::new(WORKSPACE),
        &AlreadyEmptyStore,
    )
    .expect("sweep runs");

    assert_eq!(sweep.verdict, StorageGcSweepVerdict::Converged);
    assert_eq!(sweep.passes.len(), 1);
}
