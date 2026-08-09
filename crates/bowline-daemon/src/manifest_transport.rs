//! Daemon-side transport adapter for the manifest-sync engine (Plan 111 Step 1a).
//!
//! The engine (`bowline_local::sync::manifest_engine`) depends on two abstract
//! seams — [`RemoteObjects`] and [`RemoteRef`] — plus a ref-change wakeup. This
//! module implements both over the existing hosted platform:
//! `SignedUrlByteStore` for blob/manifest bytes, `commit_uploaded_object_metadata`
//! for the metadata completion, and `get_workspace_ref`/
//! `compare_and_swap_workspace_ref` for the CAS ref. It also bridges the hosted
//! ref subscription into freshness-checked engine ref observations.
//!
//! Object identity (Plan 108): the engine seals bytes and derives the physical
//! key `blake3(sealed)` (`b_<64hex>` blob / `m_<64hex>` manifest). This adapter
//! never re-seals: it reserves, PUTs create-only, commits hosted metadata, then
//! reads the metadata back and fails closed unless it matches (workspace, the
//! `key == prefix + sealed-hash` server contract, byte length, key epoch).
//!
//! The daemon runtime does not wire this in yet; a later chunk (the driver) owns
//! the engine loop and the ref-change receiver.
//!
//! Blob uploads do not travel this thread inline (R6). `put_blob` queues sealed
//! bytes and returns; the queue drains through a bounded scoped worker pool at
//! every point where an object could become referenced. See
//! [`upload_pipeline`] for the ordering contract that makes deferral safe.

use std::collections::BTreeMap;

use bowline_control_plane::{
    CompareAndSwapError, ControlPlaneClient, SignedUrlByteStore, SignedUrlHttpClient, WorkspaceRef,
};
use bowline_core::ids::{DeviceId, SnapshotId, WorkspaceId};
use bowline_local::sync::manifest_engine::{
    BlobKey, BlobPrefetchRequest, BlobReaderUpload, BlobUpload, CasOutcome, ManifestBatchUpload,
    ManifestKey, ManifestUpload, PrefetchedBlobs, RefObservation, RefVersionLookup, RemoteObjects,
    RemoteRef, TransportError,
};
use bowline_storage::ObjectKey;

#[path = "manifest_transport/helpers.rs"]
mod helpers;
#[path = "manifest_transport/object_uploader.rs"]
mod object_uploader;
#[path = "manifest_transport/upload_pipeline.rs"]
mod upload_pipeline;

use object_uploader::{BufferedUpload, ObjectUploader, StreamedUpload, UploadKind};
use upload_pipeline::{QueueAdmission, QueuedUpload, UploadQueue, map_slice_in_parallel};

const DOWNLOAD_CONCURRENCY: usize = 32;
const MAX_PREFETCH_BYTES: u64 = 32 * 1024 * 1024;

// ---- object + ref transport -------------------------------------------------

/// One workspace's object and ref transport. Implements both engine seams so the
/// driver can pass a single `&transport` as both `objects` and `refs`.
pub struct ManifestTransport<'a, C: ControlPlaneClient + Sync> {
    uploader: ObjectUploader<'a, C>,
    queue: UploadQueue,
}

impl<'a, C: ControlPlaneClient + Sync> ManifestTransport<'a, C> {
    /// Build a transport with a fresh HTTP client for the signed-URL transfers.
    pub fn new(control_plane: &'a C, workspace_id: WorkspaceId, device_id: DeviceId) -> Self {
        Self::with_http_client(
            control_plane,
            workspace_id,
            device_id,
            SignedUrlByteStore::<'a, C>::build_http_client(),
        )
    }

    /// Build a transport that reuses an existing HTTP client (the daemon shares
    /// one client across a workspace's transfers).
    pub fn with_http_client(
        control_plane: &'a C,
        workspace_id: WorkspaceId,
        device_id: DeviceId,
        http: SignedUrlHttpClient,
    ) -> Self {
        Self {
            uploader: ObjectUploader::new(control_plane, workspace_id, device_id, http),
            queue: UploadQueue::new(),
        }
    }

    /// Upload every queued blob, then return the lowest-indexed failure.
    ///
    /// Called before anything that could publish a reference to a queued object,
    /// and before any read, so a caller can never observe a half-flushed queue.
    fn drain_queue(&self) -> Result<(), TransportError> {
        let jobs = self.queue.take();
        self.uploader.upload_buffered_batch(&jobs)
    }
}

impl<C: ControlPlaneClient + Sync> RemoteObjects for ManifestTransport<'_, C> {
    fn put_blob(&self, upload: BlobUpload<'_>) -> Result<(), TransportError> {
        let admission = self.queue.push(QueuedUpload {
            kind: UploadKind::Blob,
            content_id: upload.content_id.clone(),
            key: upload.key.as_str().to_string(),
            sealed: upload.sealed.to_vec(),
            key_epoch: upload.key_epoch,
        });
        match admission {
            QueueAdmission::Queued => Ok(()),
            QueueAdmission::DrainNow => self.drain_queue(),
        }
    }

    fn put_blob_reader(&self, upload: BlobReaderUpload<'_>) -> Result<(), TransportError> {
        // The caller deletes the spool as soon as this returns, so a streamed
        // upload can never be deferred. Large files are also the case the
        // pipeline helps least: one 8 MiB body already fills the pipe.
        self.uploader.upload_streaming(StreamedUpload {
            content_id: upload.content_id,
            key: upload.key.as_str(),
            spool_path: upload.spool_path,
            byte_len: upload.byte_len,
            key_epoch: upload.key_epoch,
        })
    }

    fn ensure_uploads_settled(&self) -> Result<(), TransportError> {
        // `put_blob` accepts into a queue, so the engine's durable "this blob is
        // stored" ledger must not be written until that queue has drained. A
        // failure here fails the cycle before any row claims a PUT that never
        // happened.
        self.drain_queue()
    }

    fn put_manifest(&self, upload: ManifestUpload<'_>) -> Result<(), TransportError> {
        // The manifest is the only thing that names a blob, so this is the
        // barrier: every queued blob's metadata row must exist before the
        // manifest's own row does.
        self.drain_queue()?;
        self.uploader.upload_buffered(BufferedUpload {
            kind: UploadKind::Manifest,
            content_id: upload.content_id,
            key: upload.key.as_str(),
            sealed: upload.sealed,
            key_epoch: upload.key_epoch,
        })
    }

    fn put_manifests(&self, uploads: &[ManifestBatchUpload]) -> Result<(), TransportError> {
        self.drain_queue()?;
        let queued = uploads
            .iter()
            .map(|upload| QueuedUpload {
                kind: UploadKind::Manifest,
                content_id: upload.content_id.clone(),
                key: upload.key.as_str().to_string(),
                sealed: upload.sealed.clone(),
                key_epoch: upload.key_epoch,
            })
            .collect::<Vec<_>>();
        self.uploader.upload_buffered_batch(&queued)
    }

    fn get_blob(&self, key: &BlobKey) -> Result<Vec<u8>, TransportError> {
        self.drain_queue()?;
        self.uploader.download("get-blob", key.as_str())
    }

    fn prefetch_blobs(
        &self,
        requests: &[BlobPrefetchRequest],
    ) -> Result<PrefetchedBlobs, TransportError> {
        self.drain_queue()?;
        let mut unique = BTreeMap::new();
        for request in requests {
            unique
                .entry(request.key.clone())
                .and_modify(|byte_len: &mut u64| *byte_len = (*byte_len).max(request.byte_len))
                .or_insert(request.byte_len);
        }
        let requests = unique
            .into_iter()
            .map(|(key, byte_len)| BlobPrefetchRequest { key, byte_len })
            .collect::<Vec<_>>();
        if requests
            .iter()
            .any(|request| request.byte_len > MAX_PREFETCH_BYTES)
        {
            return Err(TransportError::new(
                "prefetch-blob",
                "one sealed blob exceeded the bounded prefetch budget",
            ));
        }
        let mut prefetched = BTreeMap::new();
        let mut batch_start = 0;
        let uploader = &self.uploader;
        while batch_start < requests.len() {
            let mut batch_end = batch_start;
            let mut batch_bytes = 0_u64;
            while batch_end < requests.len() && batch_end - batch_start < DOWNLOAD_CONCURRENCY {
                let next = requests[batch_end].byte_len;
                if batch_end > batch_start && batch_bytes.saturating_add(next) > MAX_PREFETCH_BYTES
                {
                    break;
                }
                batch_bytes = batch_bytes.saturating_add(next);
                batch_end += 1;
            }
            let batch = &requests[batch_start..batch_end];
            let downloaded =
                map_slice_in_parallel(batch, DOWNLOAD_CONCURRENCY, |_index, request| {
                    let bytes = uploader.download("prefetch-blob", request.key.as_str())?;
                    if bytes.len() as u64 > request.byte_len {
                        return Err(TransportError::new(
                            "prefetch-blob",
                            "sealed blob exceeded its declared transfer budget",
                        ));
                    }
                    Ok((request.key.clone(), bytes))
                })?;
            prefetched.extend(downloaded);
            batch_start = batch_end;
        }
        Ok(prefetched)
    }

    fn get_blob_to_writer(
        &self,
        key: &BlobKey,
        writer: &mut dyn std::io::Write,
    ) -> Result<u64, TransportError> {
        self.drain_queue()?;
        self.uploader
            .download_to_writer("get-blob-to-writer", key.as_str(), writer)
    }

    fn get_manifest(&self, key: &ManifestKey) -> Result<Vec<u8>, TransportError> {
        self.drain_queue()?;
        self.uploader.download("get-manifest", key.as_str())
    }
}

impl<C: ControlPlaneClient + Sync> RemoteRef for ManifestTransport<'_, C> {
    fn read_ref(&self) -> Result<Option<RefObservation>, TransportError> {
        let current = self
            .uploader
            .control_plane()
            .get_workspace_ref(self.uploader.workspace_id())
            .map_err(|error| helpers::control_plane_error("read-ref", error))?;
        Ok(current.and_then(head_observation))
    }

    fn lookup_ref_version(&self, version: u64) -> Result<RefVersionLookup, TransportError> {
        const HISTORY_LIMIT: u64 = 500;

        let Some(current) = self.read_ref()? else {
            return Ok(RefVersionLookup::NotAdvanced);
        };
        if current.version == version {
            return Ok(RefVersionLookup::Found(current.manifest_key));
        }
        if current.version < version {
            return Ok(RefVersionLookup::NotAdvanced);
        }
        if current.version.saturating_sub(version).saturating_add(1) > HISTORY_LIMIT {
            return Ok(RefVersionLookup::Unknown);
        }
        let rows = self
            .uploader
            .control_plane()
            .list_workspace_ref_history(self.uploader.workspace_id(), HISTORY_LIMIT as u32)
            .map_err(|error| helpers::control_plane_error("read-ref-history", error))?;
        if let Some(row) = rows.into_iter().find(|row| row.version == version) {
            return Ok(RefVersionLookup::Found(ManifestKey::new(
                row.target_snapshot_id.as_str(),
            )));
        }
        let Some(after_history) = self.read_ref()? else {
            return Ok(RefVersionLookup::NotAdvanced);
        };
        if after_history.version < version {
            return Ok(RefVersionLookup::NotAdvanced);
        }
        if after_history
            .version
            .saturating_sub(version)
            .saturating_add(1)
            > HISTORY_LIMIT
        {
            return Ok(RefVersionLookup::Unknown);
        }
        Ok(RefVersionLookup::NotAdvanced)
    }

    fn compare_and_swap(
        &self,
        expected_version: Option<u64>,
        new_manifest_key: &ManifestKey,
    ) -> Result<CasOutcome, TransportError> {
        // Last line of defence for the ordering contract: the ref is what makes
        // a manifest reachable, so nothing may still be queued when it moves.
        self.drain_queue()?;
        let new_snapshot_id = SnapshotId::new(new_manifest_key.as_str());
        // No prior head observed (genesis) expects the pre-head baseline version
        // the hosted service seeds a workspace ref at.
        let expected = expected_version.unwrap_or(GENESIS_REF_VERSION);
        match self.cas_attempt(expected, &new_snapshot_id) {
            Ok(outcome) => Ok(outcome),
            // First push on a brand-new workspace has no refs row yet. Seed the
            // headless genesis ref (idempotent server-side) and retry CAS once.
            Err(CasAttemptError::WorkspaceMissing) => {
                self.uploader
                    .control_plane()
                    .create_workspace_ref(self.uploader.workspace_id())
                    .map_err(|error| TransportError::new("create-ref", error.to_string()))?;
                self.cas_attempt(expected, &new_snapshot_id)
                    .map_err(|error| match error {
                        CasAttemptError::WorkspaceMissing => TransportError::new(
                            "compare-and-swap",
                            "workspace still missing after create-ref".to_string(),
                        ),
                        CasAttemptError::Failed(error) => error,
                    })
            }
            Err(CasAttemptError::Failed(error)) => Err(error),
        }
    }
}

/// Local CAS attempt outcome: seed-and-retry vs hard transport failure.
enum CasAttemptError {
    WorkspaceMissing,
    Failed(TransportError),
}

impl<'a, C: ControlPlaneClient + Sync> ManifestTransport<'a, C> {
    /// One hosted CAS attempt, preserving typed outcomes and observation errors.
    fn cas_attempt(
        &self,
        expected: u64,
        new_snapshot_id: &SnapshotId,
    ) -> Result<CasOutcome, CasAttemptError> {
        match self
            .uploader
            .control_plane()
            .compare_and_swap_workspace_ref(
                self.uploader.workspace_id(),
                expected,
                new_snapshot_id,
                self.uploader.device_id(),
            ) {
            Ok(updated) => real_head_observation(&updated)
                .map(CasOutcome::Advanced)
                .map_err(CasAttemptError::Failed),
            Err(CompareAndSwapError::StaleRef(stale)) => real_head_observation(&stale.current)
                .map(CasOutcome::Lost)
                .map_err(CasAttemptError::Failed),
            // Only a genuinely indeterminate swap is a lost ack: the mutation may
            // or may not have committed, so the engine resolves it by reading the
            // ref (Plan 108 Ambiguous CAS). A decisive rejection — revoked device,
            // lost membership, contract violation — must never be laundered into
            // "ambiguous", or the daemon retries a fatal auth failure forever
            // while status claims sync is merely catching up.
            Err(CompareAndSwapError::Ambiguous(_)) => Ok(CasOutcome::Ambiguous),
            Err(CompareAndSwapError::WorkspaceMissing { .. }) => {
                Err(CasAttemptError::WorkspaceMissing)
            }
            Err(error) => Err(CasAttemptError::Failed(TransportError::new(
                "compare-and-swap",
                error.to_string(),
            ))),
        }
    }
}

/// The hosted workspace ref seeds at version 0 as a headless genesis ref (no
/// snapshot, no head) before the first real head; a `None` expected version maps
/// to it for the genesis CAS.
const GENESIS_REF_VERSION: u64 = 0;

/// Map a hosted ref to an observation only when it carries a real manifest head.
/// A version-0 genesis ref reads as "no head yet" so the driver publishes
/// genesis rather than pulling a non-existent manifest. Every real head is
/// version >= 1 and carries a manifest-backed snapshot id.
fn head_observation(workspace_ref: WorkspaceRef) -> Option<RefObservation> {
    if workspace_ref.version == GENESIS_REF_VERSION {
        return None;
    }
    workspace_ref
        .snapshot_id
        .as_ref()
        .and_then(manifest_key_from_snapshot)
        .map(|manifest_key| RefObservation {
            version: workspace_ref.version,
            manifest_key,
        })
}

/// Map a ref that must carry a real head (version >= 1) into an observation.
/// Advanced and CAS-lost refs are always real heads under the corrected genesis
/// contract — a genesis loser receives the winner's version-1 head, never a
/// headless ref — so a headless ref here is a hosted contract violation,
/// surfaced as a transport error rather than a fabricated manifest key or a
/// panic.
fn real_head_observation(workspace_ref: &WorkspaceRef) -> Result<RefObservation, TransportError> {
    let manifest_key = workspace_ref
        .snapshot_id
        .as_ref()
        .and_then(manifest_key_from_snapshot)
        .ok_or_else(|| {
            TransportError::new(
                "cas-observation",
                "hosted ref carries no manifest-backed head".to_string(),
            )
        })?;
    Ok(RefObservation {
        version: workspace_ref.version,
        manifest_key,
    })
}

fn manifest_key_from_snapshot(snapshot_id: &SnapshotId) -> Option<ManifestKey> {
    snapshot_id
        .as_str()
        .starts_with(ObjectKey::MANIFEST_PREFIX)
        .then(|| ManifestKey::new(snapshot_id.as_str()))
}

// ---- ref-change subscription bridge -----------------------------------------

#[path = "manifest_transport/ref_observer.rs"]
mod ref_observer;

pub use ref_observer::{
    ReconnectAttempt, ReconnectDelay, RefChangeSubscription, RefObserverAuthoritySource,
    RefObserverEndpointGeneration, RefObserverFailure, RefObserverFailureCode,
    RefObserverFailureStage, RefObserverFrontier, RefObserverHealth, RefObserverHealthHandle,
    RefObserverLifecycleRevision, RefObserverProcessIdentity, RefObserverReadiness,
    RefObserverRemediation, RefObserverRemediationKind, RefObserverSnapshot,
    RefObserverSnapshotHandle, RefObserverState, SignerTrustRefresh, VerifiedWorkspaceRef,
    VerifiedWorkspaceRefView,
};

#[cfg(test)]
#[path = "manifest_transport/tests.rs"]
mod tests;
