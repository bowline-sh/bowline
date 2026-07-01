use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as BASE64_URL};
use bowline_storage::{
    ObjectKey as StorageObjectKey, ObjectKind as StorageObjectKind, ObjectMetadata, RetentionState,
};
use p256::ecdsa::{Signature, VerifyingKey, signature::Verifier};
use sha2::{Digest, Sha256};

use crate::{
    AuthorizedDeviceRecord, BootstrapSession, BootstrapSessionInput, ByteRange, CompactEvent,
    CompactEventKind, CompareAndSwapError, ConflictMetadataPublish, ConflictMetadataRecord,
    ConflictResolutionMark, ControlPlaneError, ControlPlaneResult, ControlPlaneTimestamp,
    DeleteIntent, DeleteIntentRequest, DeterministicClock, DeterministicIdGenerator,
    DeviceApproval, DeviceApprovalInput, DeviceApprovalRequestList, DeviceDenial,
    DeviceDenialInput, DeviceRequest, DeviceRequestInput, DeviceRequestInputDraft,
    DeviceRequestState, DeviceRevocationInput, DownloadIntent, DownloadIntentRequest,
    FirstAuthorizedDeviceInput, GrantAcceptanceInput, Lease, LeaseCreate, LeaseExecutionState,
    LeaseOutputState, LeaseUpdate, LeaseWriteTargetMode, ObjectKind, ObjectManifestCommit,
    ObjectManifestRecord, ObjectMetadataCommit, ObjectPointer, ObjectRetentionStateUpdate,
    RecoveryDeviceAuthorizationInput, RecoveryEnvelopeInput, RecoveryEnvelopeRecord,
    RecoveryEnvelopeState, RevokedDeviceRecord, SignedUrlIntent, StaleWorkViewOverlayHead,
    StaleWorkspaceRef, UploadIntent, UploadIntentRequest, UploadVerificationIntentRequest,
    WorkViewCreate, WorkViewLifecycleState, WorkViewLifecycleUpdate, WorkViewOverlayCommit,
    WorkViewRecord, WorkViewUpdateError, WorkspaceRef, validate_object_key,
};

mod client;
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
}

impl Default for FakeControlPlaneClient {
    fn default() -> Self {
        Self::new(
            DeterministicClock::default(),
            DeterministicIdGenerator::default(),
        )
    }
}

#[derive(Debug, Default)]
struct FakeControlPlaneState {
    workspace_refs: BTreeMap<String, WorkspaceRef>,
    events: BTreeMap<String, Vec<CompactEvent>>,
    device_requests: BTreeMap<String, DeviceRequest>,
    device_request_by_device: BTreeMap<(String, String), String>,
    pending_device_proof_verifiers: BTreeMap<String, String>,
    authorized_devices: BTreeMap<(String, String), AuthorizedDeviceRecord>,
    device_authorization_proof_verifiers: BTreeMap<(String, String), String>,
    revoked_devices: BTreeMap<(String, String), RevokedDeviceRecord>,
    grants: BTreeMap<String, DeviceApproval>,
    grant_acceptance_proof_verifiers: BTreeMap<String, String>,
    denials: BTreeMap<String, DeviceDenial>,
    recovery_envelopes: BTreeMap<(String, String), RecoveryEnvelopeRecord>,
    recovery_proof_verifiers: BTreeMap<(String, String), String>,
    leases: BTreeMap<String, Lease>,
    upload_reservations: BTreeMap<(String, String), UploadReservation>,
    upload_idempotency_keys: BTreeMap<String, String>,
    committed_object_keys: BTreeSet<(String, String)>,
    object_retention_states: BTreeMap<(String, String), RetentionState>,
    same_object_stale_overlay_commits: BTreeSet<(String, String)>,
    object_keys: BTreeSet<(String, String)>,
    object_pointers: BTreeMap<String, Vec<ObjectPointer>>,
    object_manifests: BTreeMap<(String, String), ObjectManifestRecord>,
    manifests_by_snapshot: BTreeMap<(String, String), String>,
    work_views: BTreeMap<(String, String), WorkViewRecord>,
    conflicts: BTreeMap<(String, String), ConflictMetadataRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UploadReservation {
    workspace_id: String,
    object_kind: ObjectKind,
    byte_len: u64,
    content_id: Option<String>,
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

fn generated_object_key(kind: ObjectKind, seed: u64) -> String {
    match kind {
        ObjectKind::SourcePack => format!("packs_pk_{seed:016x}"),
        ObjectKind::IndexPack | ObjectKind::LocatorIndex => format!("indexes_ix_{seed:016x}"),
        ObjectKind::SnapshotManifest => format!("manifests_mf_{seed:016x}"),
        ObjectKind::AgentOverlay => format!("packs_pk_{seed:016x}"),
    }
}

fn upload_idempotency_key(request: &UploadIntentRequest) -> Option<String> {
    request.object_key.as_ref().map_or_else(
        || {
            request.content_id.as_ref().map(|content_id| {
                format!(
                    "content:{}:{}:{}:{}",
                    request.workspace_id,
                    request.object_kind.as_str(),
                    content_id,
                    request.byte_len
                )
            })
        },
        |object_key| Some(format!("object-key:{}:{object_key}", request.workspace_id)),
    )
}

fn validate_lease_create(input: &LeaseCreate) -> ControlPlaneResult<()> {
    validate_opaque_id(&input.lease_id, "lease ID")?;
    validate_opaque_id(&input.project_id, "project ID")?;
    validate_opaque_id(&input.device_id, "device ID")?;
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
            let Some(work_view_id) = input.work_view_id.as_deref() else {
                return Err(ControlPlaneError::Conflict {
                    resource: "agent lease",
                    reason: "work-view leases require a work view ID",
                });
            };
            validate_opaque_id(work_view_id, "work view ID")?;
        }
    }
    validate_opaque_id(&input.base_snapshot_id, "base snapshot ID")?;
    validate_status_code(&input.status_code)?;
    Ok(())
}

fn conflict_metadata_same_occurrence(
    existing: &ConflictMetadataRecord,
    base_snapshot_id: &str,
    remote_snapshot_id: &str,
) -> bool {
    existing.base_snapshot_id == base_snapshot_id
        && existing.remote_snapshot_id == remote_snapshot_id
}

fn validate_lease_update(input: &LeaseUpdate) -> ControlPlaneResult<()> {
    validate_opaque_id(&input.lease_id, "lease ID")?;
    validate_opaque_id(&input.updated_by_device_id, "device ID")?;
    if let Some(status_code) = &input.status_code {
        validate_status_code(status_code)?;
    }
    match input.event_kind {
        None
        | Some(
            CompactEventKind::LeaseBlocked
            | CompactEventKind::LeaseCleanupCompleted
            | CompactEventKind::LeaseCompleted
            | CompactEventKind::LeaseExpired
            | CompactEventKind::LeaseHydrationRequested
            | CompactEventKind::LeaseRevoked
            | CompactEventKind::LeaseReviewReady
            | CompactEventKind::LeaseToolDenied
            | CompactEventKind::LeaseToolInvoked
            | CompactEventKind::LeaseUpdated
            | CompactEventKind::OverlayChanged
            | CompactEventKind::PublishRequested,
        ) => Ok(()),
        Some(_) => Err(ControlPlaneError::Conflict {
            resource: "agent lease",
            reason: "event kind is not a lease event",
        }),
    }
}

fn validate_opaque_id(value: &str, label: &'static str) -> ControlPlaneResult<()> {
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

fn validate_optional_lease_pointer(
    state: &FakeControlPlaneState,
    workspace_id: &str,
    pointer: Option<&ObjectPointer>,
) -> ControlPlaneResult<()> {
    let Some(pointer) = pointer else {
        return Ok(());
    };
    validate_object_key(&pointer.object_key)?;
    validate_committed_pointer(state, workspace_id, pointer, ObjectKind::AgentOverlay)
}

fn lease_create_matches(existing: &Lease, input: &LeaseCreate) -> bool {
    existing.workspace_id == input.workspace_id
        && existing.project_id == input.project_id
        && existing.device_id == input.device_id
        && existing.write_target_mode == input.write_target_mode
        && existing.work_view_id == input.work_view_id
        && existing.base_snapshot_id == input.base_snapshot_id
        && existing.execution_state == input.execution_state
        && existing.output_state == input.output_state
        && existing.status_code == input.status_code
        && existing.output_object == input.output_object
        && existing.audit_object == input.audit_object
        && existing.expires_at == input.expires_at
}

fn lease_event_for_update(lease: &Lease) -> CompactEventKind {
    if lease.output_state == LeaseOutputState::ReviewReady {
        return CompactEventKind::LeaseReviewReady;
    }
    match lease.execution_state {
        LeaseExecutionState::Active => CompactEventKind::LeaseUpdated,
        LeaseExecutionState::Blocked => CompactEventKind::LeaseBlocked,
        LeaseExecutionState::Completed => CompactEventKind::LeaseCompleted,
        LeaseExecutionState::Expired => CompactEventKind::LeaseExpired,
        LeaseExecutionState::Revoked => CompactEventKind::LeaseRevoked,
    }
}

fn device_authorization_proof_valid(
    verifier: &str,
    proof: &str,
    workspace_id: &str,
    device_id: &str,
    action: &str,
    subject: &str,
) -> bool {
    let Some(public_key) = verifier.strip_prefix("dapv_p256_v1_") else {
        return false;
    };
    let Some(signature) = proof.strip_prefix("dapp_p256_v1_") else {
        return false;
    };
    let Ok(public_key) = BASE64_URL.decode(public_key) else {
        return false;
    };
    let Ok(signature) = BASE64_URL.decode(signature) else {
        return false;
    };
    let Ok(verifying_key) = VerifyingKey::from_sec1_bytes(&public_key) else {
        return false;
    };
    let Ok(signature) = Signature::from_slice(&signature) else {
        return false;
    };
    verifying_key
        .verify(
            &device_authorization_message(&[
                "bowline device authorization proof v2",
                workspace_id,
                device_id,
                action,
                subject,
            ]),
            &signature,
        )
        .is_ok()
}

fn device_authorization_message(fields: &[&str]) -> Vec<u8> {
    let mut message = Vec::new();
    for field in fields {
        message.extend_from_slice(&(field.len() as u64).to_le_bytes());
        message.extend_from_slice(field.as_bytes());
    }
    message
}

fn recovery_proof_verifier_from_proof(
    proof: &str,
    workspace_id: &str,
    envelope_id: &str,
) -> String {
    let hash = sha256_proof_fields(&[
        "bowline recovery proof verifier v2",
        workspace_id,
        envelope_id,
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
    workspace_id: &str,
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
            reason: "committed object metadata does not match manifest pointer",
        });
    }

    let reservation = state
        .upload_reservations
        .get(&(workspace_id.to_string(), pointer.object_key.clone()))
        .ok_or_else(|| ControlPlaneError::ObjectMissing {
            object_key: pointer.object_key.clone(),
        })?;
    if reservation.workspace_id != workspace_id {
        return Err(ControlPlaneError::Conflict {
            resource: "upload intent",
            reason: "object key is reserved for another workspace",
        });
    }
    if reservation.object_kind != expected_kind
        || reservation.byte_len != pointer.byte_len
        || !pointer.hash.starts_with("b3_")
        || pointer.key_epoch == 0
        || reservation.content_id.as_deref() != Some(pointer.content_id.as_str())
    {
        return Err(ControlPlaneError::Conflict {
            resource: "upload intent",
            reason: "reserved object metadata does not match committed pointer",
        });
    }
    Ok(())
}

fn manifest_commit_matches(existing: &ObjectManifestRecord, commit: &ObjectManifestCommit) -> bool {
    existing.workspace_id == commit.workspace_id
        && existing.snapshot_id == commit.snapshot_id
        && existing.manifest_id == commit.manifest_id
        && existing.manifest_object == commit.manifest_object
        && existing.pack_objects == commit.pack_objects
}

fn pointer_storage_metadata(pointer: &ObjectPointer) -> ControlPlaneResult<ObjectMetadata> {
    Ok(ObjectMetadata {
        key: StorageObjectKey::new(pointer.object_key.clone()).map_err(|_| {
            ControlPlaneError::InvalidObjectKey {
                reason: "object keys must be generated opaque pack, manifest, or overlay keys",
            }
        })?,
        kind: match pointer.kind {
            ObjectKind::SourcePack => StorageObjectKind::SourcePack,
            ObjectKind::IndexPack => StorageObjectKind::IndexPack,
            ObjectKind::LocatorIndex => StorageObjectKind::LocatorIndex,
            ObjectKind::SnapshotManifest => StorageObjectKind::SnapshotManifest,
            ObjectKind::AgentOverlay => StorageObjectKind::AgentOverlay,
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
        WorkViewLifecycleState::Expired => CompactEventKind::WorkExpired,
        WorkViewLifecycleState::Archived => CompactEventKind::WorkArchived,
    }
}
