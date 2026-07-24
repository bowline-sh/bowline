use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

use bowline_core::ids::{
    DeviceApprovalRequestId, DeviceId, RecoveryEnvelopeId, SnapshotId, WorkspaceId,
};
use bowline_storage::{
    ObjectKey as StorageObjectKey, ObjectKind as StorageObjectKind, ObjectMetadata, RetentionState,
};
use sha2::{Digest, Sha256};

use crate::{
    AuthorizedDeviceRecord, BootstrapSession, BootstrapSessionInput, ByteRange, Capability,
    CapabilityReporting, CompactEvent, CompactEventKind, CompareAndSwapError, ControlPlaneError,
    ControlPlaneResult, ControlPlaneTimestamp, DeleteIntent, DeterministicClock,
    DeterministicIdGenerator, DeviceApproval, DeviceApprovalInput, DeviceApprovalRequestList,
    DeviceDenial, DeviceDenialInput, DeviceRequest, DeviceRequestInput, DeviceRequestInputDraft,
    DeviceRequestState, DeviceRevocationInput, DownloadIntent, DownloadIntentRequest,
    FirstAuthorizedDeviceInput, GrantAcceptanceInput, ObjectKind, ObjectMetadataCommit,
    ObjectPointer, ObjectRetentionStateUpdate, RecoveryDeviceAuthorizationInput,
    RecoveryEnvelopeRecord, RecoveryEnvelopeState, RejectionCode, RevokedDeviceRecord,
    Sha256Checksum, SignedUrlIntent, StaleWorkspaceRef, UploadIntent, UploadIntentRequest,
    UploadVerificationIntentRequest, WorkspaceRef, WorkspaceRefHistoryRecord, validate_object_key,
    verify_device_authorization_proof,
};

mod devices;
mod harness;
mod objects;
mod recovery;
mod sync;

#[derive(Debug, Clone)]
pub struct FakeControlPlaneClient {
    clock: DeterministicClock,
    ids: DeterministicIdGenerator,
    local_device_id: Option<String>,
    state: Arc<Mutex<FakeControlPlaneState>>,
    // Test doubles in dependent crates (e.g. the daemon manifest transport)
    // point the fake's signed URLs at local HTTP servers to exercise the real
    // upload/download path; always compiled so cross-crate tests can reach it.
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
    upload_intent_requests: Vec<UploadIntentRequest>,
    upload_reservations: BTreeMap<(WorkspaceId, String), UploadReservation>,
    upload_idempotency_keys: BTreeMap<String, String>,
    committed_object_keys: BTreeSet<(WorkspaceId, String)>,
    object_retention_states: BTreeMap<(WorkspaceId, String), RetentionState>,
    object_keys: BTreeSet<(WorkspaceId, String)>,
    object_pointers: BTreeMap<WorkspaceId, Vec<ObjectPointer>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UploadReservation {
    workspace_id: WorkspaceId,
    object_kind: ObjectKind,
    byte_len: u64,
    checksum_sha256: Sha256Checksum,
    content_id: Option<bowline_core::ids::ContentId>,
    intent: UploadIntent,
}

impl UploadReservation {
    fn matches_request(&self, request: &UploadIntentRequest) -> bool {
        self.workspace_id == request.workspace_id
            && self.object_kind == request.object_kind
            && self.byte_len == request.byte_len
            && self.checksum_sha256 == request.checksum_sha256
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

    pub fn set_signed_url_override(&self, action: &str, url: String) {
        self.signed_url_overrides
            .lock()
            .expect("fake signed URL overrides poisoned")
            .insert(action.to_string(), url);
    }
}

fn generated_object_key(kind: ObjectKind, seed: u64) -> String {
    // Manifest-sync keys are exactly `<prefix><64 hex>`: repeat the 16-hex
    // seed to fill the full sealed-hash-width suffix.
    let suffix = format!("{seed:016x}");
    match kind {
        ObjectKind::Blob => format!("b_{suffix}{suffix}{suffix}{suffix}"),
        ObjectKind::Manifest => format!("m_{suffix}{suffix}{suffix}{suffix}"),
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
                reason: "object keys must be sealed-hash b_/m_ keys",
            }
        })?,
        kind: match pointer.kind {
            ObjectKind::Blob => StorageObjectKind::WorkspaceFileV1,
            ObjectKind::Manifest => StorageObjectKind::WorkspaceManifestV1,
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
