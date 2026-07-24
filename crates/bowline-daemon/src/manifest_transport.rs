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

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use bowline_control_plane::{
    CompareAndSwapError, ControlPlaneClient, ControlPlaneTimestamp, HostedControlPlaneClient,
    ObjectKind as ControlObjectKind, ObjectMetadataCommit, ObjectPointer, SignedUrlByteStore,
    SignedUrlHttpClient, WorkspaceRef, WorkspaceRefStreamConnectionState, WorkspaceRefStreamEvent,
    WorkspaceRefStreamShutdown, workspace_ref_stream_shutdown_pair,
};
use bowline_core::ids::{ContentId, DeviceId, SnapshotId, WorkspaceId};
use bowline_local::sync::manifest_engine::{
    BlobKey, BlobReaderUpload, BlobUpload, CasOutcome, EngineEvent, KeyEpoch, ManifestKey,
    ManifestUpload, RefObservation, RemoteObjects, RemoteRef, TransportError,
};
use bowline_storage::{
    ByteStore, ObjectContentId, ObjectHash, ObjectKey, ObjectKind as StorageObjectKind,
    ObjectMetadata, PutObjectReaderRequest,
};

#[path = "manifest_transport/helpers.rs"]
mod helpers;

use helpers::{
    SpoolSource, byte_store_error, committed_metadata_error, control_plane_error, hash_spool,
    parse_object_key,
};

/// `stable_object_hash` prefixes every digest with `b3_`; the sealed-hash suffix
/// after it must equal the physical object key's hex, which is the entire
/// server-side integrity contract for `b_`/`m_` keys (Plan 110).
const STABLE_OBJECT_HASH_PREFIX: &str = "b3_";

// ---- object + ref transport -------------------------------------------------

/// One workspace's object and ref transport. Implements both engine seams so the
/// driver can pass a single `&transport` as both `objects` and `refs`.
pub struct ManifestTransport<'a, C: ControlPlaneClient> {
    control_plane: &'a C,
    store: SignedUrlByteStore<'a, C>,
    workspace_id: WorkspaceId,
    device_id: DeviceId,
}

impl<'a, C: ControlPlaneClient> ManifestTransport<'a, C> {
    /// Build a transport with a fresh HTTP client for the signed-URL transfers.
    pub fn new(control_plane: &'a C, workspace_id: WorkspaceId, device_id: DeviceId) -> Self {
        let store = SignedUrlByteStore::new(control_plane, workspace_id.as_str());
        Self::with_store(control_plane, store, workspace_id, device_id)
    }

    /// Build a transport that reuses an existing HTTP client (the daemon shares
    /// one client across a workspace's transfers).
    pub fn with_http_client(
        control_plane: &'a C,
        workspace_id: WorkspaceId,
        device_id: DeviceId,
        http: SignedUrlHttpClient,
    ) -> Self {
        let store =
            SignedUrlByteStore::with_http_client(control_plane, workspace_id.as_str(), http);
        Self::with_store(control_plane, store, workspace_id, device_id)
    }

    fn with_store(
        control_plane: &'a C,
        store: SignedUrlByteStore<'a, C>,
        workspace_id: WorkspaceId,
        device_id: DeviceId,
    ) -> Self {
        Self {
            control_plane,
            store,
            workspace_id,
            device_id,
        }
    }

    /// Reserve + create-only PUT a buffered sealed object, then complete it.
    fn upload_buffered(
        &self,
        kind: UploadKind,
        content_id: &ContentId,
        key: &str,
        sealed: &[u8],
        key_epoch: KeyEpoch,
    ) -> Result<(), TransportError> {
        let object_key = parse_object_key(key)?;
        let metadata = self
            .store
            .put_object_with_content_id_at_epoch(
                object_key,
                kind.storage_kind(),
                content_id.as_str(),
                sealed,
                key_epoch.get(),
                Some(&self.device_id),
            )
            .map_err(|error| byte_store_error(kind.put_operation(), error))?;
        self.complete_upload(
            kind,
            content_id,
            key,
            sealed.len() as u64,
            key_epoch,
            metadata,
        )
    }

    /// Reserve + streamed create-only PUT of a sealed object spooled to disk.
    fn upload_streaming(
        &self,
        content_id: &ContentId,
        key: &str,
        spool_path: &Path,
        byte_len: u64,
        key_epoch: KeyEpoch,
    ) -> Result<(), TransportError> {
        let object_key = parse_object_key(key)?;
        let expected_hash = hash_spool(spool_path)?;
        let source = SpoolSource {
            path: spool_path.to_path_buf(),
        };
        let metadata = self
            .store
            .put_object_reader_with_content_id_at_epoch(PutObjectReaderRequest {
                key: object_key,
                kind: StorageObjectKind::WorkspaceFileV1,
                content_id: ObjectContentId::new(content_id.as_str()),
                source: &source,
                byte_len,
                expected_hash: ObjectHash::from_stable_hash(expected_hash),
                key_epoch: key_epoch.get(),
                created_by_device_id: Some(&self.device_id),
            })
            .map_err(|error| byte_store_error("put-blob-reader", error))?;
        self.complete_upload(
            UploadKind::Blob,
            content_id,
            key,
            byte_len,
            key_epoch,
            metadata,
        )
    }

    /// Commit hosted metadata and fail closed on any returned-field mismatch.
    /// The commit must land before this returns success so nothing references an
    /// object the hosted service has not recorded (Plan 108).
    fn complete_upload(
        &self,
        kind: UploadKind,
        content_id: &ContentId,
        key: &str,
        expected_byte_len: u64,
        key_epoch: KeyEpoch,
        metadata: ObjectMetadata,
    ) -> Result<(), TransportError> {
        let pointer = ObjectPointer {
            object_key: metadata.key.as_str().to_string(),
            content_id: content_id.clone(),
            byte_len: metadata.byte_len,
            hash: metadata.hash.clone(),
            key_epoch: metadata.key_epoch,
            kind: kind.control_kind(),
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
            .map_err(|error| control_plane_error(kind.commit_operation(), error))?;
        validate_committed_metadata(CommittedMetadataExpectation {
            key_prefix: kind.key_prefix(),
            key,
            expected_hash: &metadata.hash,
            expected_byte_len,
            expected_key_epoch: key_epoch,
            committed: &committed,
        })
    }

    fn download(&self, operation: &'static str, key: &str) -> Result<Vec<u8>, TransportError> {
        let object_key = parse_object_key(key)?;
        self.store
            .get_object(&object_key)
            .map_err(|error| byte_store_error(operation, error))
    }
}

impl<C: ControlPlaneClient> RemoteObjects for ManifestTransport<'_, C> {
    fn put_blob(&self, upload: BlobUpload<'_>) -> Result<(), TransportError> {
        self.upload_buffered(
            UploadKind::Blob,
            upload.content_id,
            upload.key.as_str(),
            upload.sealed,
            upload.key_epoch,
        )
    }

    fn put_blob_reader(&self, upload: BlobReaderUpload<'_>) -> Result<(), TransportError> {
        self.upload_streaming(
            upload.content_id,
            upload.key.as_str(),
            upload.spool_path,
            upload.byte_len,
            upload.key_epoch,
        )
    }

    fn put_manifest(&self, upload: ManifestUpload<'_>) -> Result<(), TransportError> {
        self.upload_buffered(
            UploadKind::Manifest,
            upload.content_id,
            upload.key.as_str(),
            upload.sealed,
            upload.key_epoch,
        )
    }

    fn get_blob(&self, key: &BlobKey) -> Result<Vec<u8>, TransportError> {
        self.download("get-blob", key.as_str())
    }

    fn get_manifest(&self, key: &ManifestKey) -> Result<Vec<u8>, TransportError> {
        self.download("get-manifest", key.as_str())
    }
}

impl<C: ControlPlaneClient> RemoteRef for ManifestTransport<'_, C> {
    fn read_ref(&self) -> Result<Option<RefObservation>, TransportError> {
        let current = self
            .control_plane
            .get_workspace_ref(&self.workspace_id)
            .map_err(|error| control_plane_error("read-ref", error))?;
        Ok(current.and_then(head_observation))
    }

    fn compare_and_swap(
        &self,
        expected_version: Option<u64>,
        new_manifest_key: &ManifestKey,
    ) -> Result<CasOutcome, TransportError> {
        let new_snapshot_id = SnapshotId::new(new_manifest_key.as_str());
        // No prior head observed (genesis) expects the pre-head baseline version
        // the hosted service seeds a workspace ref at.
        let expected = expected_version.unwrap_or(GENESIS_REF_VERSION);
        match self.cas_attempt(expected, &new_snapshot_id) {
            Ok(outcome) => Ok(outcome),
            // First push on a brand-new workspace has no refs row yet. Seed the
            // headless genesis ref (idempotent server-side) and retry CAS once.
            Err(CasAttemptError::WorkspaceMissing) => {
                self.control_plane
                    .create_workspace_ref(&self.workspace_id)
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

impl<'a, C: ControlPlaneClient> ManifestTransport<'a, C> {
    /// One hosted CAS attempt, preserving typed outcomes and observation errors.
    fn cas_attempt(
        &self,
        expected: u64,
        new_snapshot_id: &SnapshotId,
    ) -> Result<CasOutcome, CasAttemptError> {
        match self.control_plane.compare_and_swap_workspace_ref(
            &self.workspace_id,
            expected,
            new_snapshot_id,
            &self.device_id,
        ) {
            Ok(updated) => real_head_observation(&updated)
                .map(CasOutcome::Advanced)
                .map_err(CasAttemptError::Failed),
            Err(CompareAndSwapError::StaleRef(stale)) => real_head_observation(&stale.current)
                .map(CasOutcome::Lost)
                .map_err(CasAttemptError::Failed),
            // A transport/storage failure at CAS time is a lost ack: the swap may
            // or may not have committed. The engine resolves it by reading the ref
            // (Plan 108 Ambiguous CAS); never a silent success or a hard failure.
            Err(CompareAndSwapError::Storage(_)) => Ok(CasOutcome::Ambiguous),
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

// ---- upload kind dispatch ---------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UploadKind {
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

// ---- committed metadata validation -----------------------------------------

struct CommittedMetadataExpectation<'a> {
    key_prefix: &'a str,
    key: &'a str,
    expected_hash: &'a str,
    expected_byte_len: u64,
    expected_key_epoch: KeyEpoch,
    committed: &'a ObjectMetadata,
}

/// Fail closed unless the hosted commit response matches every dimension the
/// engine will later trust. The commit action verifies R2 existence and returns
/// the just-committed row, so a second hosted read adds no safety.
fn validate_committed_metadata(
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

// ---- ref-change subscription bridge -----------------------------------------

/// A reconnect backoff schedule (failure count -> delay). The daemon driver
/// injects the shared observer schedule; keeping it a parameter avoids a second
/// copy of the backoff formula and lets tests drive fast reconnects.
pub type ReconnectDelay = Arc<dyn Fn(u32) -> Duration + Send + Sync>;

// The ref values themselves wake this receiver immediately. This bound only
// limits how long local shutdown and websocket-state transitions wait to be
// observed; it is not a remote polling interval.
const REF_SUBSCRIPTION_DRAIN_INTERVAL: Duration = Duration::from_millis(100);
const REF_SUBSCRIPTION_SHUTDOWN_POLL: Duration = Duration::from_millis(50);
const REF_SUBSCRIPTION_FIRST_VALUE_TIMEOUT: Duration = Duration::from_secs(5);

type StreamEventSender = Sender<WorkspaceRefStreamEvent>;

struct StreamAttempt {
    shutdown: WorkspaceRefStreamShutdown,
    worker: JoinHandle<()>,
}

type StreamStarter = Box<dyn FnMut(StreamEventSender) -> std::io::Result<StreamAttempt> + Send>;

/// Lifecycle of the reactive hosted-ref observer. `Live` is reached only after
/// Convex has delivered the subscription's initial value; owning a worker thread
/// is not sufficient evidence that remote changes can reach the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefObserverState {
    Connecting,
    Live,
    Retrying,
    Stopped,
}

/// The operation that ended the most recent observer attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefObserverFailureStage {
    Start,
    InitialValue,
    Stream,
}

/// Structured failure retained for diagnostics and rate-limited logging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefObserverFailure {
    pub stage: RefObserverFailureStage,
    pub message: String,
}

/// Current observer health. The revision changes on every lifecycle transition
/// so connection loss is visible even while the engine remains idle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefObserverHealth {
    pub revision: u64,
    pub state: RefObserverState,
    pub consecutive_failures: u32,
    pub reconnects: u64,
    pub last_failure: Option<RefObserverFailure>,
}

impl Default for RefObserverHealth {
    fn default() -> Self {
        Self {
            revision: 0,
            state: RefObserverState::Connecting,
            consecutive_failures: 0,
            reconnects: 0,
            last_failure: None,
        }
    }
}

/// Cloneable, lock-bounded view of the observer lifecycle.
#[derive(Clone, Debug)]
pub struct RefObserverHealthHandle(Arc<Mutex<RefObserverHealth>>);

impl RefObserverHealthHandle {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(RefObserverHealth::default())))
    }

    pub fn current(&self) -> RefObserverHealth {
        self.0
            .lock()
            .map(|health| health.clone())
            .unwrap_or_default()
    }

    pub fn is_live(&self) -> bool {
        self.current().state == RefObserverState::Live
    }

    fn connecting(&self, consecutive_failures: u32) {
        let last_failure = self.current().last_failure;
        self.transition(
            RefObserverState::Connecting,
            consecutive_failures,
            false,
            last_failure,
        );
    }

    fn transition(
        &self,
        state: RefObserverState,
        consecutive_failures: u32,
        reconnect: bool,
        last_failure: Option<RefObserverFailure>,
    ) {
        if let Ok(mut health) = self.0.lock() {
            health.revision = health.revision.saturating_add(1);
            health.state = state;
            health.consecutive_failures = consecutive_failures;
            health.reconnects = health.reconnects.saturating_add(u64::from(reconnect));
            health.last_failure = last_failure;
        }
    }
}

/// Bridges the hosted workspace-ref subscription into engine wakeups. A
/// signature-verified real head received during a live subscription is carried
/// as a freshness-checked pull hint. The first value after startup or reconnect
/// remains a payload-free wakeup so the driver synchronously re-establishes
/// authority. Reconnects on stream loss with the injected backoff. Dropping the
/// handle stops the worker.
pub struct RefChangeSubscription {
    shutdown: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    health: RefObserverHealthHandle,
}

impl RefChangeSubscription {
    /// Subscribe over a hosted control-plane client.
    pub fn spawn(
        client: Arc<HostedControlPlaneClient>,
        workspace_id: String,
        events: Sender<EngineEvent>,
        reconnect_delay: ReconnectDelay,
    ) -> Self {
        let starter: StreamStarter = Box::new(move |stream_tx| {
            let (shutdown, cancellation) = workspace_ref_stream_shutdown_pair();
            let client = Arc::clone(&client);
            let workspace_id = workspace_id.clone();
            let terminal_tx = stream_tx.clone();
            let worker = thread::Builder::new()
                .name("bowline-manifest-ref-stream".to_string())
                .spawn(move || {
                    if let Err(error) = client.stream_workspace_ref_events_until(
                        &workspace_id,
                        stream_tx,
                        cancellation,
                    ) {
                        let _receiver_gone =
                            terminal_tx.send(WorkspaceRefStreamEvent::Ref(Err(error)));
                    }
                })?;
            Ok(StreamAttempt { shutdown, worker })
        });
        Self::spawn_with_starter(starter, events, reconnect_delay)
    }

    fn spawn_with_starter(
        mut starter: StreamStarter,
        events: Sender<EngineEvent>,
        reconnect_delay: ReconnectDelay,
    ) -> Self {
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = Arc::clone(&shutdown);
        let health = RefObserverHealthHandle::new();
        let worker_health = health.clone();
        let worker = thread::Builder::new()
            .name("bowline-manifest-ref-bridge".to_string())
            .spawn(move || {
                run_ref_bridge(
                    &mut starter,
                    &events,
                    &reconnect_delay,
                    &worker_shutdown,
                    &worker_health,
                )
            })
            .expect("ref-change subscription bridge thread spawns");
        Self {
            shutdown,
            worker: Some(worker),
            health,
        }
    }

    pub fn health_handle(&self) -> RefObserverHealthHandle {
        self.health.clone()
    }

    pub fn is_finished(&self) -> bool {
        self.worker
            .as_ref()
            .is_none_or(std::thread::JoinHandle::is_finished)
    }
}

impl Drop for RefChangeSubscription {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        self.health
            .transition(RefObserverState::Stopped, 0, false, None);
    }
}

/// Outcome of draining one stream attempt.
enum DrainOutcome {
    /// The driver's receiver was dropped; stop the bridge entirely.
    DriverGone,
    /// The stream ended or errored; reconnect (with backoff unless a value was
    /// seen, which resets the failure count).
    Reconnect {
        received_value: bool,
        failure: RefObserverFailure,
    },
}

fn run_ref_bridge(
    starter: &mut StreamStarter,
    events: &Sender<EngineEvent>,
    reconnect_delay: &ReconnectDelay,
    shutdown: &AtomicBool,
    health: &RefObserverHealthHandle,
) {
    let mut failures: u32 = 0;
    while !shutdown.load(Ordering::SeqCst) {
        let (stream_tx, stream_rx) = mpsc::channel();
        let attempt = match starter(stream_tx) {
            Ok(attempt) => attempt,
            Err(error) => {
                failures = failures.saturating_add(1);
                let failure = RefObserverFailure {
                    stage: RefObserverFailureStage::Start,
                    message: error.to_string(),
                };
                health.transition(
                    RefObserverState::Retrying,
                    failures,
                    true,
                    Some(failure.clone()),
                );
                log_observer_failure(&failure, failures);
                if !sleep_until_shutdown(reconnect_delay(failures), shutdown) {
                    break;
                }
                health.connecting(failures);
                continue;
            }
        };
        health.connecting(failures);
        let outcome = drain_stream(
            &stream_rx,
            events,
            shutdown,
            health,
            REF_SUBSCRIPTION_FIRST_VALUE_TIMEOUT,
        );
        drop(attempt.shutdown);
        let _ = attempt.worker.join();
        match outcome {
            DrainOutcome::DriverGone => break,
            DrainOutcome::Reconnect {
                received_value,
                failure,
            } => {
                if shutdown.load(Ordering::SeqCst) {
                    break;
                }
                failures = if received_value {
                    1
                } else {
                    failures.saturating_add(1)
                };
                health.transition(
                    RefObserverState::Retrying,
                    failures,
                    true,
                    Some(failure.clone()),
                );
                log_observer_failure(&failure, failures);
                if !sleep_until_shutdown(reconnect_delay(failures), shutdown) {
                    break;
                }
                health.connecting(failures);
            }
        }
    }
}

fn drain_stream(
    stream_rx: &Receiver<WorkspaceRefStreamEvent>,
    events: &Sender<EngineEvent>,
    shutdown: &AtomicBool,
    health: &RefObserverHealthHandle,
    first_value_timeout: Duration,
) -> DrainOutcome {
    let mut received_any_value = false;
    let mut received_initial_value = false;
    let mut websocket_connected = false;
    let mut value_wait_started = Instant::now();
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return DrainOutcome::DriverGone;
        }
        let drain_interval = if received_initial_value {
            REF_SUBSCRIPTION_DRAIN_INTERVAL
        } else {
            REF_SUBSCRIPTION_DRAIN_INTERVAL
                .min(first_value_timeout.saturating_sub(value_wait_started.elapsed()))
        };
        match stream_rx.recv_timeout(drain_interval) {
            Ok(WorkspaceRefStreamEvent::ConnectionState(
                WorkspaceRefStreamConnectionState::Connecting,
            )) if websocket_connected => {
                // Convex owns transport reconnection and keeps the query
                // subscription registered across it. Destroying the client here
                // fights that recovery loop and can chain first-value timeouts.
                // Mark readiness degraded until this same subscription pushes a
                // fresh value, then return to Live without polling or rebuilding.
                websocket_connected = false;
                received_initial_value = false;
                value_wait_started = Instant::now();
                health.connecting(health.current().consecutive_failures);
            }
            Ok(WorkspaceRefStreamEvent::ConnectionState(
                WorkspaceRefStreamConnectionState::Connecting,
            )) => health.connecting(health.current().consecutive_failures),
            Ok(WorkspaceRefStreamEvent::ConnectionState(
                WorkspaceRefStreamConnectionState::Connected,
            )) => websocket_connected = true,
            Ok(WorkspaceRefStreamEvent::Ref(Ok(workspace_ref))) => {
                let requires_authoritative_read = !received_initial_value;
                if !received_initial_value {
                    health.transition(RefObserverState::Live, 0, false, None);
                }
                received_any_value = true;
                received_initial_value = true;
                let event = if requires_authoritative_read {
                    EngineEvent::RefChanged
                } else {
                    workspace_ref
                        .and_then(head_observation)
                        .map_or(EngineEvent::RefChanged, EngineEvent::RefObserved)
                };
                if events.send(event).is_err() {
                    return DrainOutcome::DriverGone;
                }
            }
            Ok(WorkspaceRefStreamEvent::Ref(Err(error))) => {
                return DrainOutcome::Reconnect {
                    received_value: received_any_value,
                    failure: RefObserverFailure {
                        stage: RefObserverFailureStage::Stream,
                        message: error.to_string(),
                    },
                };
            }
            Err(RecvTimeoutError::Timeout) => {
                if !received_initial_value && value_wait_started.elapsed() >= first_value_timeout {
                    return DrainOutcome::Reconnect {
                        received_value: received_any_value,
                        failure: RefObserverFailure {
                            stage: RefObserverFailureStage::InitialValue,
                            message: format!(
                                "no initial subscription value within {:?}",
                                first_value_timeout
                            ),
                        },
                    };
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                return DrainOutcome::Reconnect {
                    received_value: received_any_value,
                    failure: RefObserverFailure {
                        stage: RefObserverFailureStage::Stream,
                        message: "workspace-ref subscription ended".to_string(),
                    },
                };
            }
        }
    }
}

fn log_observer_failure(failure: &RefObserverFailure, consecutive_failures: u32) {
    eprintln!(
        "bowline-daemon reactive ref observer {:?} failure #{consecutive_failures}: {}",
        failure.stage, failure.message
    );
}

/// Sleep for `delay`, waking early if shutdown is requested. Returns `false` when
/// shutdown fired (the caller must stop).
fn sleep_until_shutdown(delay: Duration, shutdown: &AtomicBool) -> bool {
    let deadline = Instant::now() + delay;
    while Instant::now() < deadline {
        if shutdown.load(Ordering::SeqCst) {
            return false;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        thread::sleep(REF_SUBSCRIPTION_SHUTDOWN_POLL.min(remaining));
    }
    !shutdown.load(Ordering::SeqCst)
}

#[cfg(test)]
#[path = "manifest_transport/tests.rs"]
mod tests;
