use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{Arc, Mutex},
};

use bowline_core::ids::{
    ConflictId, DeviceApprovalRequestId, DeviceId, LeaseId, RecoveryEnvelopeId, SnapshotId,
    WorkViewId, WorkspaceId,
};
use bowline_storage::{
    ObjectKey as StorageObjectKey, ObjectKind as StorageObjectKind, ObjectMetadata, RetentionState,
};
use sha2::{Digest, Sha256};

use crate::{
    AuthorizedDeviceRecord, BootstrapSession, BootstrapSessionInput, ByteRange, Capability,
    CapabilityReporting, CompactEvent, CompactEventKind, CompareAndSwapError,
    ConflictMetadataRecord, ConflictOccurrenceReconcile, ConflictOccurrenceState,
    ConflictReconcileOutcome, ConflictReconcileResult, ControlPlaneError, ControlPlaneResult,
    ControlPlaneTimestamp, DeleteIntent, DeterministicClock, DeterministicIdGenerator,
    DeviceApproval, DeviceApprovalInput, DeviceApprovalRequestList, DeviceDenial,
    DeviceDenialInput, DeviceRequest, DeviceRequestInput, DeviceRequestInputDraft,
    DeviceRequestState, DeviceRevocationInput, DownloadIntent, DownloadIntentRequest,
    FirstAuthorizedDeviceInput, GrantAcceptanceInput, Lease, LeaseCreate, LeaseSessionState,
    LeaseUpdate, LeaseWriteTargetMode, MetadataBindingBatch, MetadataBindingCommit,
    MetadataBindingInput, MetadataBindingOutcome, MetadataBindingRecord, MetadataRecordKind,
    MetadataSidecar, ObjectKind, ObjectMetadataCommit, ObjectPointer, ObjectRetentionStateUpdate,
    RecoveryDeviceAuthorizationInput, RecoveryEnvelopeRecord, RecoveryEnvelopeState, RejectionCode,
    RevokedDeviceRecord, SignedUrlIntent, SnapshotRootCommit, SnapshotRootRecord,
    StaleWorkViewOverlayHead, StaleWorkspaceRef, UploadIntent, UploadIntentRequest,
    UploadVerificationIntentRequest, WorkViewCreate, WorkViewLifecycleState,
    WorkViewLifecycleUpdate, WorkViewOverlayCommit, WorkViewRecord, WorkViewUpdateError,
    WorkspaceRef, WorkspaceRefHistoryRecord, validate_object_key,
    verify_device_authorization_proof,
};

mod devices;
mod harness;
mod leases;
mod objects;
mod recovery;
mod sync;
mod work_views;

#[derive(Debug, Clone)]
pub struct FakeControlPlaneClient {
    clock: DeterministicClock,
    ids: DeterministicIdGenerator,
    local_device_id: Option<String>,
    state: Arc<Mutex<FakeControlPlaneState>>,
    #[cfg(test)]
    signed_url_overrides: Arc<Mutex<BTreeMap<String, String>>>,
}

impl Default for FakeControlPlaneClient {
    fn default() -> Self {
        Self::new(
            DeterministicClock::default(),
            DeterministicIdGenerator::default(),
        )
    }
}

impl CapabilityReporting for FakeControlPlaneClient {
    fn capabilities(&self) -> BTreeSet<Capability> {
        fake_supported_capabilities().iter().copied().collect()
    }
}

fn fake_supported_capabilities() -> &'static [Capability] {
    &[
        Capability::WorkspaceRefHistory,
        Capability::StorageGc,
        Capability::ObjectMetadata,
        Capability::WorkViews,
        Capability::AgentLeases,
        Capability::DeviceBootstrap,
        Capability::DeviceTrust,
        Capability::RecoveryKey,
    ]
}

fn device_not_trusted(message: &'static str) -> ControlPlaneError {
    ControlPlaneError::Rejected {
        code: RejectionCode::DeviceNotTrusted,
        message: message.to_string(),
    }
}

#[derive(Debug, Clone, Default)]
struct FakeControlPlaneState {
    offline: bool,
    workspace_refs: BTreeMap<WorkspaceId, WorkspaceRef>,
    // One-shot harness injection: the next workspace-ref CAS for this workspace
    // is answered with a StaleRef carrying this current head, simulating a
    // control-plane advance that races an in-flight upload (the runtime
    // Upload->Stale edge that cannot be reached by pre-advancing the ref).
    next_workspace_ref_cas_stale: BTreeMap<WorkspaceId, WorkspaceRef>,
    workspace_ref_history: BTreeMap<WorkspaceId, Vec<WorkspaceRefHistoryRecord>>,
    events: BTreeMap<WorkspaceId, Vec<CompactEvent>>,
    device_requests: BTreeMap<DeviceApprovalRequestId, DeviceRequest>,
    device_request_by_device: BTreeMap<(WorkspaceId, DeviceId), DeviceApprovalRequestId>,
    pending_device_proof_verifiers: BTreeMap<DeviceApprovalRequestId, String>,
    authorized_devices: BTreeMap<(WorkspaceId, DeviceId), AuthorizedDeviceRecord>,
    device_authorization_proof_verifiers: BTreeMap<(WorkspaceId, DeviceId), String>,
    revoked_devices: BTreeMap<(WorkspaceId, DeviceId), RevokedDeviceRecord>,
    grants: BTreeMap<DeviceApprovalRequestId, DeviceApproval>,
    revoked_grants: BTreeSet<DeviceApprovalRequestId>,
    grant_acceptance_proof_verifiers: BTreeMap<DeviceApprovalRequestId, String>,
    denials: BTreeMap<DeviceApprovalRequestId, DeviceDenial>,
    recovery_envelopes: BTreeMap<(WorkspaceId, RecoveryEnvelopeId), RecoveryEnvelopeRecord>,
    recovery_proof_verifiers: BTreeMap<(WorkspaceId, RecoveryEnvelopeId), String>,
    workspace_key_epochs: BTreeMap<WorkspaceId, u32>,
    leases: BTreeMap<LeaseId, Lease>,
    upload_intent_requests: Vec<UploadIntentRequest>,
    metadata_binding_resolution_requests: Vec<Vec<String>>,
    upload_reservations: BTreeMap<(WorkspaceId, String), UploadReservation>,
    upload_idempotency_keys: BTreeMap<String, String>,
    committed_object_keys: BTreeSet<(WorkspaceId, String)>,
    object_retention_states: BTreeMap<(WorkspaceId, String), RetentionState>,
    same_object_stale_overlay_commits: BTreeSet<(WorkspaceId, WorkViewId)>,
    object_keys: BTreeSet<(WorkspaceId, String)>,
    object_pointers: BTreeMap<WorkspaceId, Vec<ObjectPointer>>,
    metadata_bindings: BTreeMap<(WorkspaceId, String), MetadataBindingRecord>,
    snapshot_roots: BTreeMap<(WorkspaceId, SnapshotId), SnapshotRootRecord>,
    work_views: BTreeMap<(WorkspaceId, WorkViewId), WorkViewRecord>,
    conflicts: BTreeMap<(WorkspaceId, ConflictId), ConflictMetadataRecord>,
    conflict_reconcile_failures: VecDeque<ControlPlaneError>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UploadReservation {
    workspace_id: WorkspaceId,
    object_kind: ObjectKind,
    byte_len: u64,
    content_id: Option<bowline_core::ids::ContentId>,
    intent: UploadIntent,
}

impl UploadReservation {
    fn matches_request(&self, request: &UploadIntentRequest) -> bool {
        self.workspace_id == request.workspace_id
            && self.object_kind == request.object_kind
            && self.byte_len == request.byte_len
            && self.content_id == request.content_id
    }
}

impl FakeControlPlaneClient {
    pub fn set_offline(&self, offline: bool) {
        self.state
            .lock()
            .expect("fake control plane poisoned")
            .offline = offline;
    }

    pub fn is_offline(&self) -> bool {
        self.state
            .lock()
            .expect("fake control plane poisoned")
            .offline
    }

    pub fn offline_transport_error() -> ControlPlaneError {
        ControlPlaneError::Transport {
            detail: "fake control plane is offline".to_string(),
        }
    }

    pub fn upload_intent_requests(&self) -> Vec<UploadIntentRequest> {
        self.state
            .lock()
            .expect("fake control plane poisoned")
            .upload_intent_requests
            .clone()
    }

    pub fn upload_intent_request_count(&self) -> usize {
        self.state
            .lock()
            .expect("fake control plane poisoned")
            .upload_intent_requests
            .len()
    }

    pub fn metadata_binding_resolution_requests(&self) -> Vec<Vec<String>> {
        self.state
            .lock()
            .expect("fake control plane poisoned")
            .metadata_binding_resolution_requests
            .clone()
    }

    #[cfg(test)]
    pub(crate) fn set_signed_url_override(&self, action: &str, url: String) {
        self.signed_url_overrides
            .lock()
            .expect("fake signed URL overrides poisoned")
            .insert(action.to_string(), url);
    }
}

fn generated_object_key(kind: ObjectKind, seed: u64) -> String {
    match kind {
        ObjectKind::SourcePack => format!("packs_pk_{seed:016x}"),
        ObjectKind::LocatorIndex => format!("indexes_ix_{seed:016x}"),
        ObjectKind::SnapshotMetadataPage => {
            let suffix = format!("{seed:016x}");
            format!("metadata_mp_{suffix}{suffix}{suffix}{suffix}")
        }
        ObjectKind::SnapshotManifest => format!("manifests_mf_{seed:016x}"),
        ObjectKind::AgentOverlay => format!("packs_pk_{seed:016x}"),
        ObjectKind::ConflictBundle => format!("conflicts_cb_{seed:016x}"),
    }
}

fn upload_idempotency_key(request: &UploadIntentRequest) -> Option<String> {
    request.object_key.as_ref().map_or_else(
        || {
            request.content_id.as_ref().map(|content_id| {
                format!(
                    "content:{}:{}:{}:{}",
                    request.workspace_id.as_str(),
                    request.object_kind.as_str(),
                    content_id.as_str(),
                    request.byte_len
                )
            })
        },
        |object_key| {
            Some(format!(
                "object-key:{}:{object_key}",
                request.workspace_id.as_str()
            ))
        },
    )
}

fn validate_lease_create(input: &LeaseCreate) -> ControlPlaneResult<()> {
    validate_opaque_id(&input.lease_id, "lease ID")?;
    validate_opaque_id(&input.project_id, "project ID")?;
    validate_opaque_id(&input.device_id, "device ID")?;
    validate_lease_dispatch_target(input)?;
    match input.write_target_mode {
        LeaseWriteTargetMode::Direct => {
            if input.work_view_id.is_some() {
                return Err(ControlPlaneError::Conflict {
                    resource: "agent lease",
                    reason: "direct leases must not carry a work view ID",
                });
            }
        }
        LeaseWriteTargetMode::WorkView => {
            let Some(work_view_id) = input.work_view_id.as_ref() else {
                return Err(ControlPlaneError::Conflict {
                    resource: "agent lease",
                    reason: "work-view leases require a work view ID",
                });
            };
            validate_opaque_id(work_view_id, "work view ID")?;
        }
    }
    validate_opaque_id(&input.base_snapshot_id, "base snapshot ID")?;
    if let Some(task_label) = input.task_label.as_ref() {
        validate_task_label(task_label)?;
    }
    if input.target_device_ref.is_some() && input.task_label.is_none() {
        return Err(ControlPlaneError::Conflict {
            resource: "agent lease",
            reason: "handoff leases require a task label",
        });
    }
    validate_status_code(&input.status_code)?;
    Ok(())
}

fn conflict_metadata_same_occurrence(
    existing: &ConflictMetadataRecord,
    input: &ConflictOccurrenceReconcile,
) -> bool {
    existing.base_snapshot_id == input.base_snapshot_id
        && existing.remote_snapshot_id == input.remote_snapshot_id
        && existing.occurrence_version == input.occurrence_version
        && existing.conflict_kind == input.conflict_kind
        && existing.paths == input.paths
        && existing.contains_secrets == input.contains_secrets
        && existing.reason == input.reason
        && conflict_bundle_same(
            existing.bundle_object.as_ref(),
            input.bundle_object.as_ref(),
        )
}

fn conflict_bundle_same(left: Option<&ObjectPointer>, right: Option<&ObjectPointer>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => {
            left.object_key == right.object_key
                && left.content_id == right.content_id
                && left.byte_len == right.byte_len
                && left.hash == right.hash
                && left.key_epoch == right.key_epoch
                && left.kind == right.kind
        }
        (None, Some(_)) | (Some(_), None) => false,
    }
}

fn validate_lease_update(input: &LeaseUpdate) -> ControlPlaneResult<()> {
    validate_opaque_id(&input.lease_id, "lease ID")?;
    validate_opaque_id(&input.updated_by_device_id, "device ID")?;
    if let Some(status_code) = &input.status_code {
        validate_status_code(status_code)?;
    }
    match input.event_kind {
        None => Ok(()),
        Some(kind) if kind.is_lease_update_event() => Ok(()),
        Some(_) => Err(ControlPlaneError::Conflict {
            resource: "agent lease",
            reason: "event kind is not a lease event",
        }),
    }
}

// A handoff lease is one that carries both a target host (to materialize on) and
// an origin device (that created it). A local lease carries neither. Any partial
// combination is rejected.
fn validate_lease_dispatch_target(input: &LeaseCreate) -> ControlPlaneResult<()> {
    match (
        input.target_device_ref.as_deref(),
        input.origin_device_ref.as_deref(),
    ) {
        (None, None) => Ok(()),
        (Some(target_device_ref), Some(origin_device_ref)) => {
            validate_opaque_id(target_device_ref, "target device ref")?;
            validate_opaque_id(origin_device_ref, "origin device ref")?;
            if origin_device_ref != input.device_id.as_str() {
                return Err(ControlPlaneError::Conflict {
                    resource: "agent lease",
                    reason: "origin device ref must match creating device",
                });
            }
            Ok(())
        }
        _ => Err(ControlPlaneError::Conflict {
            resource: "agent lease",
            reason: "handoff leases require both target and origin device refs",
        }),
    }
}

fn validate_opaque_id(value: impl AsRef<str>, label: &'static str) -> ControlPlaneResult<()> {
    let value = value.as_ref();
    if value.is_empty()
        || value.len() > 160
        || value.contains('/')
        || value.contains('\\')
        || value.contains('.')
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'_' | b'-'))
    {
        return Err(ControlPlaneError::Conflict {
            resource: "agent lease",
            reason: label,
        });
    }
    Ok(())
}

fn validate_status_code(status_code: &str) -> ControlPlaneResult<()> {
    if status_code.is_empty()
        || status_code.len() > 80
        || status_code.contains('/')
        || status_code.contains('\\')
        || status_code.contains('.')
        || !status_code
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'_' | b'-'))
    {
        return Err(ControlPlaneError::Conflict {
            resource: "agent lease",
            reason: "status code must be compact and pathless",
        });
    }
    Ok(())
}

fn validate_task_label(task_label: &str) -> ControlPlaneResult<()> {
    if task_label.is_empty() || task_label.len() > 512 || task_label.contains('\n') {
        return Err(ControlPlaneError::Conflict {
            resource: "agent lease",
            reason: "task label must be a single non-empty redacted line",
        });
    }
    Ok(())
}

fn lease_create_matches(existing: &Lease, input: &LeaseCreate) -> bool {
    existing.workspace_id == input.workspace_id
        && existing.project_id == input.project_id
        && existing.device_id == input.device_id
        && existing.target_device_ref == input.target_device_ref
        && existing.origin_device_ref == input.origin_device_ref
        && existing.write_target_mode == input.write_target_mode
        && existing.work_view_id == input.work_view_id
        && existing.base_snapshot_id == input.base_snapshot_id
        && existing.task_label == input.task_label
        && existing.session_state == input.session_state
        && existing.status_code == input.status_code
        && existing.expires_at == input.expires_at
}

fn lease_event_for_update(lease: &Lease) -> CompactEventKind {
    match lease.session_state {
        LeaseSessionState::Provisional | LeaseSessionState::Open => CompactEventKind::LeaseUpdated,
        LeaseSessionState::Completed => CompactEventKind::LeaseCompleted,
    }
}

fn device_authorization_proof_valid(
    verifier: &str,
    proof: &str,
    workspace_id: &WorkspaceId,
    device_id: &DeviceId,
    action: &str,
    subject: &str,
) -> bool {
    verify_device_authorization_proof(
        verifier,
        proof,
        workspace_id.as_str(),
        device_id.as_str(),
        action,
        subject,
    )
    .is_ok()
}

fn recovery_proof_verifier_from_proof(
    proof: &str,
    workspace_id: &WorkspaceId,
    envelope_id: &RecoveryEnvelopeId,
) -> String {
    let hash = sha256_proof_fields(&[
        "bowline recovery proof verifier v2",
        workspace_id.as_str(),
        envelope_id.as_str(),
        proof,
    ]);
    format!("rkpv_{}", &hash[..32])
}

fn grant_acceptance_proof_verifier(proof: &str) -> String {
    let hash = sha256_proof_fields(&["bowline grant acceptance proof verifier v1", proof]);
    format!("gapv_{}", &hash[..32])
}

fn sha256_proof_fields(fields: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for field in fields {
        hasher.update((field.len() as u64).to_le_bytes());
        hasher.update(field.as_bytes());
    }
    let digest = hasher.finalize();
    format!("{digest:x}")
}

fn validate_committed_pointer(
    state: &FakeControlPlaneState,
    workspace_id: &WorkspaceId,
    pointer: &ObjectPointer,
    expected_kind: ObjectKind,
) -> ControlPlaneResult<()> {
    if let Some(existing) = state
        .object_pointers
        .get(workspace_id)
        .and_then(|pointers| {
            pointers
                .iter()
                .find(|existing| existing.object_key == pointer.object_key)
        })
    {
        if existing == pointer {
            return Ok(());
        }
        return Err(ControlPlaneError::Conflict {
            resource: "object pointer",
            reason: "committed object metadata does not match object pointer",
        });
    }

    let reservation = state
        .upload_reservations
        .get(&(workspace_id.clone(), pointer.object_key.clone()))
        .ok_or_else(|| ControlPlaneError::ObjectMissing {
            object_key: pointer.object_key.clone(),
        })?;
    if reservation.workspace_id != *workspace_id {
        return Err(ControlPlaneError::Conflict {
            resource: "upload intent",
            reason: "object key is reserved for another workspace",
        });
    }
    if reservation.object_kind != expected_kind
        || reservation.byte_len != pointer.byte_len
        || !pointer.hash.starts_with("b3_")
        || pointer.key_epoch == 0
        || reservation.content_id.as_ref() != Some(&pointer.content_id)
    {
        return Err(ControlPlaneError::Conflict {
            resource: "upload intent",
            reason: "reserved object metadata does not match committed pointer",
        });
    }
    Ok(())
}

fn pointer_storage_metadata(pointer: &ObjectPointer) -> ControlPlaneResult<ObjectMetadata> {
    Ok(ObjectMetadata {
        key: StorageObjectKey::new(pointer.object_key.clone()).map_err(|_| {
            ControlPlaneError::InvalidObjectKey {
                reason: "object keys must be generated opaque pack, manifest, overlay, or conflict-bundle keys",
            }
        })?,
        kind: match pointer.kind {
            ObjectKind::SourcePack => StorageObjectKind::SourcePack,
            ObjectKind::LocatorIndex => StorageObjectKind::LocatorIndex,
            ObjectKind::SnapshotMetadataPage => StorageObjectKind::SnapshotMetadataPage,
            ObjectKind::SnapshotManifest => StorageObjectKind::SnapshotManifest,
            ObjectKind::AgentOverlay => StorageObjectKind::AgentOverlay,
            ObjectKind::ConflictBundle => StorageObjectKind::ConflictBundle,
        },
        byte_len: pointer.byte_len,
        hash: pointer.hash.clone(),
        key_epoch: pointer.key_epoch,
        created_by_device_id: None,
        created_at_unix_ms: pointer.created_at.tick,
        retention_state: RetentionState::Current,
        retain_until_unix_ms: None,
    })
}

fn work_event_for_lifecycle(lifecycle: WorkViewLifecycleState) -> CompactEventKind {
    match lifecycle {
        WorkViewLifecycleState::Active => CompactEventKind::WorkUpdated,
        WorkViewLifecycleState::ReviewReady => CompactEventKind::WorkReviewReady,
        WorkViewLifecycleState::Accepted => CompactEventKind::WorkAccepted,
        WorkViewLifecycleState::Discarded => CompactEventKind::WorkDiscarded,
    }
}
