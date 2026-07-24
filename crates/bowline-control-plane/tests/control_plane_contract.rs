use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as BASE64_URL};
use bowline_control_plane::{
    ByteRange, Capability, CompactEventKind, CompareAndSwapError, ControlPlaneClient,
    ControlPlaneError, ControlPlaneTimestamp, DeviceApprovalInput, DeviceControlPlaneClient,
    DeviceRequestInput, DeviceRequestInputDraft, DeviceRevocationInput, DownloadIntentRequest,
    FakeControlPlaneClient, FirstAuthorizedDeviceInput, GrantAcceptanceInput,
    ObjectControlPlaneClient, ObjectKind, ObjectMetadataCommit, ObjectPointer,
    ObjectRetentionStateUpdate, RecoveryControlPlaneClient, RecoveryDeviceAuthorizationInput,
    RecoveryEnvelopeInput, RecoveryEnvelopeState, RejectionCode, Sha256Checksum,
    UploadIntentRequest, WorkspaceControlPlaneClient, device_request_proof_subject,
    device_revocation_proof_subject, is_opaque_object_key, recovery_envelope_payload_proof_subject,
    recovery_envelope_proof_subject,
};
use bowline_core::ids::*;
use bowline_storage::RetentionState;
use p256::ecdsa::{Signature, SigningKey, VerifyingKey, signature::Signer};
use sha2::{Digest, Sha256};

fn assert_device_not_trusted(error: ControlPlaneError) {
    assert!(matches!(
        error,
        ControlPlaneError::Rejected {
            code: RejectionCode::DeviceNotTrusted,
            ..
        }
    ));
}

fn assert_domain_contract<T: ControlPlaneClient>() {}

#[test]
fn fake_client_implements_the_canonical_domain_contract() {
    assert_domain_contract::<FakeControlPlaneClient>();
}

#[cfg(feature = "hosted-convex")]
#[test]
fn hosted_client_implements_the_same_canonical_domain_contract() {
    assert_domain_contract::<bowline_control_plane::HostedControlPlaneClient>();
}

#[test]
fn fake_client_creates_workspace_ref_and_returns_it() {
    let control_plane = FakeControlPlaneClient::default();
    let initial_ref = control_plane.create_workspace("workspace-1");

    assert_eq!(
        control_plane
            .get_workspace_ref(&WorkspaceId::new("workspace-1"))
            .expect("fake control plane reads refs"),
        Some(initial_ref)
    );
}

#[test]
fn fake_cas_advances_ref_and_appends_one_compact_event() {
    let control_plane = FakeControlPlaneClient::default();
    let initial_ref = control_plane.create_workspace("workspace-1");
    let before_events = control_plane
        .list_events(&WorkspaceId::new("workspace-1"))
        .expect("events are readable");

    let advanced_ref = control_plane
        .compare_and_swap_workspace_ref(
            &WorkspaceId::new("workspace-1"),
            initial_ref.version,
            &SnapshotId::new("snapshot-1"),
            &DeviceId::new("device-1"),
        )
        .expect("matching CAS advances");

    let after_events = control_plane
        .list_events(&WorkspaceId::new("workspace-1"))
        .expect("events are readable");
    assert_eq!(advanced_ref.version, 1);
    assert_eq!(
        advanced_ref.snapshot_id,
        Some(SnapshotId::new("snapshot-1"))
    );
    assert_eq!(after_events.len(), before_events.len() + 1);
    assert_eq!(
        after_events.last().expect("CAS event exists").kind,
        CompactEventKind::WorkspaceRefAdvanced
    );
}

#[test]
fn fake_cas_with_stale_base_returns_current_ref() {
    let control_plane = FakeControlPlaneClient::default();
    let initial_ref = control_plane.create_workspace("workspace-1");

    control_plane
        .compare_and_swap_workspace_ref(
            &WorkspaceId::new("workspace-1"),
            initial_ref.version,
            &SnapshotId::new("snapshot-a"),
            &DeviceId::new("device-a"),
        )
        .expect("first writer wins");

    let stale = control_plane
        .compare_and_swap_workspace_ref(
            &WorkspaceId::new("workspace-1"),
            initial_ref.version,
            &SnapshotId::new("snapshot-b"),
            &DeviceId::new("device-b"),
        )
        .expect_err("second writer sees a typed stale-ref error");

    assert!(matches!(
        stale,
        CompareAndSwapError::StaleRef(stale)
            if stale.expected_version == initial_ref.version
                && stale.current.snapshot_id == Some(SnapshotId::new("snapshot-a"))
                && stale.current.updated_by_device_id.as_deref() == Some("device-a")
    ));
}

#[test]
fn fake_project_scoped_cas_records_project_id_in_ref_history() {
    let control_plane = FakeControlPlaneClient::default();
    let initial_ref = control_plane.create_workspace("workspace-1");

    control_plane
        .compare_and_swap_workspace_ref_for_project(
            &WorkspaceId::new("workspace-1"),
            initial_ref.version,
            &SnapshotId::new("snapshot-project-a"),
            &DeviceId::new("device-a"),
            Some(&ProjectId::new("project-a")),
        )
        .expect("scoped CAS advances");

    let history = control_plane
        .list_workspace_ref_history(&WorkspaceId::new("workspace-1"), 10)
        .expect("history is readable");

    assert_eq!(history.len(), 1);
    assert_eq!(history[0].project_id.as_deref(), Some("project-a"));
}

#[test]
fn fake_client_reports_only_typed_supported_capabilities() {
    let control_plane = FakeControlPlaneClient::default();
    assert_capability_matrix(
        &control_plane,
        &[
            (Capability::WorkspaceRefHistory, true),
            (Capability::StorageGc, true),
            (Capability::ObjectMetadata, true),
            (Capability::DeviceBootstrap, true),
            (Capability::DeviceTrust, true),
            (Capability::RecoveryKey, true),
        ],
    );
}

#[cfg(feature = "hosted-convex")]
#[test]
fn hosted_client_reports_only_typed_supported_capabilities() {
    let control_plane = bowline_control_plane::HostedControlPlaneClient::try_new_with_token(
        "http://127.0.0.1:3210",
        "token",
    )
    .expect("hosted client can be constructed for capability reporting");
    assert_capability_matrix(
        &control_plane,
        &[
            (Capability::WorkspaceRefHistory, true),
            (Capability::StorageGc, true),
            (Capability::ObjectMetadata, true),
            (Capability::DeviceBootstrap, true),
            (Capability::DeviceTrust, true),
            (Capability::RecoveryKey, true),
        ],
    );
}

fn assert_capability_matrix(
    control_plane: &dyn ControlPlaneClient,
    expected: &[(Capability, bool)],
) {
    let capabilities = control_plane.capabilities();

    assert_eq!(
        expected.len(),
        Capability::ALL.len(),
        "capability matrix must make one support decision per known capability"
    );

    for capability in Capability::ALL {
        let matching_rows = expected
            .iter()
            .filter(|(expected_capability, _)| expected_capability == capability)
            .count();
        assert_eq!(
            matching_rows, 1,
            "capability matrix must contain exactly one row for {capability}"
        );
    }

    for (capability, supported) in expected {
        assert!(
            capabilities.contains(capability) == *supported,
            "reported capability support for {capability} did not match expected {supported}"
        );
        assert_eq!(control_plane.supports_capability(*capability), *supported);
    }
}

#[test]
fn upload_and_download_intents_use_opaque_object_keys() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-1");

    let blob_upload = control_plane
        .create_upload_intent(
            UploadIntentRequest::new(
                "workspace-1",
                ObjectKind::Blob,
                128,
                Sha256Checksum::for_bytes(b"fixture"),
            )
            .with_content_id("content-1"),
        )
        .expect("upload intent");
    let upload_retry = control_plane
        .create_upload_intent(
            UploadIntentRequest::new(
                "workspace-1",
                ObjectKind::Blob,
                128,
                Sha256Checksum::for_bytes(b"fixture"),
            )
            .with_content_id("content-1"),
        )
        .expect("idempotent upload retry");

    assert_eq!(upload_retry, blob_upload);
    assert!(is_opaque_object_key(&blob_upload.object_key));
    assert!(!blob_upload.object_key.contains("Users"));
    assert!(!blob_upload.object_key.contains("src"));
    assert_eq!(blob_upload.object_kind, ObjectKind::Blob);

    let missing = control_plane
        .create_download_intent(DownloadIntentRequest::full(
            "workspace-1",
            blob_upload.object_key.clone(),
        ))
        .expect_err("reserved upload key is not downloadable until commit");
    assert!(matches!(missing, ControlPlaneError::ObjectMissing { .. }));

    control_plane
        .commit_uploaded_object_metadata(ObjectMetadataCommit {
            workspace_id: WorkspaceId::new("workspace-1"),
            object: ObjectPointer {
                object_key: blob_upload.object_key.clone(),
                content_id: ContentId::new("content-1"),
                byte_len: blob_upload.byte_len,
                hash: "b3_blob".to_string(),
                key_epoch: 1,
                kind: ObjectKind::Blob,
                created_at: ControlPlaneTimestamp { tick: 11 },
            },
            committed_by_device_id: DeviceId::new("device-1"),
        })
        .expect("metadata commit publishes the object pointer");

    let download = control_plane
        .create_download_intent(DownloadIntentRequest {
            workspace_id: WorkspaceId::new("workspace-1"),
            object_key: blob_upload.object_key.clone(),
            range: Some(ByteRange::new(4, 16)),
        })
        .expect("download intent");

    assert_eq!(download.object_key, blob_upload.object_key);
    assert_eq!(download.range, Some(ByteRange::new(4, 16)));
    assert!(download.signed_url.url.contains("action=download"));
    assert!(!format!("{:?}", download.signed_url).contains("action=download"));
    assert!(format!("{:?}", download.signed_url).contains("<redacted>"));
}

#[test]
fn explicit_object_keys_are_scoped_by_workspace() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-a");
    control_plane.create_workspace("workspace-b");

    let shared_blob_key = format!("b_{}", "aa".repeat(32));
    let shared_manifest_key = format!("m_{}", "bb".repeat(32));

    for workspace_id in ["workspace-a", "workspace-b"] {
        let blob_upload = control_plane
            .create_upload_intent(
                UploadIntentRequest::new(
                    workspace_id,
                    ObjectKind::Blob,
                    128,
                    Sha256Checksum::for_bytes(b"fixture"),
                )
                .with_content_id(format!("content-{workspace_id}"))
                .with_object_key(&shared_blob_key),
            )
            .expect("workspace-scoped blob upload intent");
        let manifest_upload = control_plane
            .create_upload_intent(
                UploadIntentRequest::new(
                    workspace_id,
                    ObjectKind::Manifest,
                    64,
                    Sha256Checksum::for_bytes(b"fixture"),
                )
                .with_content_id(format!("manifest-{workspace_id}"))
                .with_object_key(&shared_manifest_key),
            )
            .expect("workspace-scoped manifest upload intent");

        assert_eq!(blob_upload.object_key, shared_blob_key);
        assert_eq!(manifest_upload.object_key, shared_manifest_key);

        for (object_key, content_id, byte_len, kind) in [
            (
                shared_blob_key.clone(),
                format!("content-{workspace_id}"),
                128,
                ObjectKind::Blob,
            ),
            (
                shared_manifest_key.clone(),
                format!("manifest-{workspace_id}"),
                64,
                ObjectKind::Manifest,
            ),
        ] {
            control_plane
                .commit_uploaded_object_metadata(ObjectMetadataCommit {
                    workspace_id: WorkspaceId::new(workspace_id),
                    object: ObjectPointer {
                        object_key,
                        content_id: ContentId::new(content_id),
                        byte_len,
                        hash: format!("b3_{kind:?}_{workspace_id}").to_lowercase(),
                        key_epoch: 1,
                        kind,
                        created_at: ControlPlaneTimestamp { tick: 10 },
                    },
                    committed_by_device_id: DeviceId::new("device-1"),
                })
                .expect("same logical object key commits independently per workspace");
        }
    }

    let metadata_a = control_plane
        .head_object_metadata(&WorkspaceId::new("workspace-a"), &shared_blob_key)
        .expect("workspace a metadata");
    let metadata_b = control_plane
        .head_object_metadata(&WorkspaceId::new("workspace-b"), &shared_blob_key)
        .expect("workspace b metadata");

    assert_eq!(metadata_a.key.as_str(), shared_blob_key);
    assert_eq!(metadata_a.hash, "b3_blob_workspace-a");
    assert_eq!(metadata_b.key.as_str(), shared_blob_key);
    assert_eq!(metadata_b.hash, "b3_blob_workspace-b");

    assert!(matches!(
        control_plane
            .head_object_metadata(&WorkspaceId::new("workspace-missing"), &shared_blob_key),
        Err(ControlPlaneError::WorkspaceMissing { .. })
    ));
}

#[test]
fn delete_intents_require_delete_eligible_metadata() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-delete");
    let direct_object =
        commit_one_direct_object(&control_plane, "workspace-delete", "content-delete");

    let not_eligible = control_plane
        .create_storage_gc_delete_intent(
            &WorkspaceId::new("workspace-delete"),
            &direct_object.object_key,
        )
        .expect_err("current objects are not delete-eligible");
    assert!(matches!(
        not_eligible,
        ControlPlaneError::Conflict {
            resource: "storage GC delete intent",
            ..
        }
    ));

    let forbidden = control_plane
        .mark_object_retention_state(ObjectRetentionStateUpdate::new(
            "workspace-delete",
            &direct_object.object_key,
            RetentionState::DeleteEligible,
        ))
        .expect_err("delete eligibility requires the GC authority");
    assert!(matches!(
        forbidden,
        ControlPlaneError::Conflict {
            resource: "object retention",
            ..
        }
    ));

    let still_live = control_plane
        .create_storage_gc_delete_intent(
            &WorkspaceId::new("workspace-delete"),
            &direct_object.object_key,
        )
        .expect_err("a live object never receives a fake delete intent");
    assert!(matches!(
        still_live,
        ControlPlaneError::Conflict {
            resource: "storage GC delete intent",
            ..
        }
    ));
}

#[test]
fn path_shaped_object_keys_are_rejected() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-1");

    let error = control_plane
        .create_download_intent(DownloadIntentRequest::full(
            "workspace-1",
            "/workspace/Code/acme/src/main.rs",
        ))
        .expect_err("path-derived key is rejected before lookup");

    assert!(matches!(error, ControlPlaneError::InvalidObjectKey { .. }));
    assert!(!error.to_string().contains("src/main.rs"));
    assert!(!error.to_string().contains("/workspace/Code"));
}

#[path = "control_plane_contract/devices_recovery.rs"]
mod devices_recovery;

trait DeviceRequestInputTestExt {
    fn with_device_proof_verifier(self, device_id: &str) -> Self;
}

impl DeviceRequestInputTestExt for DeviceRequestInput {
    fn with_device_proof_verifier(mut self, device_id: &str) -> Self {
        self.device_authorization_proof_verifier = device_verifier(device_id);
        self
    }
}

fn create_first_device(
    control_plane: &FakeControlPlaneClient,
    workspace_id: &str,
    device_id: &str,
) {
    control_plane
        .create_first_authorized_device(FirstAuthorizedDeviceInput {
            workspace_id: WorkspaceId::new(workspace_id),
            device_id: DeviceId::new(device_id),
            device_name: format!("{device_id}-name"),
            platform: "macos".to_string(),
            device_fingerprint: format!("fp_{device_id}"),
            device_authorization_proof_verifier: device_verifier(device_id),
        })
        .expect("first device trust root");
}

fn commit_one_direct_object(
    control_plane: &FakeControlPlaneClient,
    workspace_id: &str,
    content_id: &str,
) -> ObjectPointer {
    let upload = control_plane
        .create_upload_intent(
            UploadIntentRequest::new(
                workspace_id,
                ObjectKind::Blob,
                128,
                Sha256Checksum::for_bytes(b"fixture"),
            )
            .with_content_id(content_id),
        )
        .expect("blob upload intent");

    let direct_object = ObjectPointer {
        object_key: upload.object_key,
        content_id: ContentId::new(content_id),
        byte_len: 128,
        hash: format!("b3_{content_id}"),
        key_epoch: 1,
        kind: ObjectKind::Blob,
        created_at: ControlPlaneTimestamp { tick: 11 },
    };

    control_plane
        .commit_uploaded_object_metadata(ObjectMetadataCommit {
            workspace_id: WorkspaceId::new(workspace_id),
            object: direct_object.clone(),
            committed_by_device_id: DeviceId::new("device-1"),
        })
        .expect("uploaded object metadata commit");

    direct_object
}

fn device_verifier(device_id: &str) -> String {
    let signing_key = test_device_signing_key(device_id);
    let verifying_key = VerifyingKey::from(&signing_key);
    let public_key = verifying_key.to_encoded_point(false);
    format!("dapv_p256_v1_{}", BASE64_URL.encode(public_key.as_bytes()))
}

fn device_proof(workspace_id: &str, device_id: &str, action: &str, subject: &str) -> String {
    let signing_key = test_device_signing_key(device_id);
    let signature: Signature = signing_key.sign(&device_authorization_message(&[
        "bowline device authorization proof v2",
        workspace_id,
        device_id,
        action,
        subject,
    ]));
    format!("dapp_p256_v1_{}", BASE64_URL.encode(signature.to_bytes()))
}

fn grant_acceptance_proof(workspace_id: &str, request_id: &str, device_id: &str) -> String {
    let hash = sha256_proof_fields(&[
        "bowline test grant acceptance proof v1",
        workspace_id,
        request_id,
        device_id,
    ]);
    format!("gap_{}", &hash[..32])
}

fn grant_acceptance_proof_verifier(
    workspace_id: &str,
    request_id: &str,
    device_id: &str,
) -> String {
    let proof = grant_acceptance_proof(workspace_id, request_id, device_id);
    let hash = sha256_proof_fields(&["bowline grant acceptance proof verifier v1", &proof]);
    format!("gapv_{}", &hash[..32])
}

fn recovery_proof(workspace_id: &str, envelope_id: &str, words: &str) -> String {
    let hash = sha256_proof_fields(&[
        "bowline recovery proof v2",
        workspace_id,
        envelope_id,
        words,
    ]);
    format!("rkp_{}", &hash[..32])
}

fn test_device_signing_key(device_id: &str) -> SigningKey {
    for counter in 0_u8..=u8::MAX {
        let digest = Sha256::digest(device_authorization_message(&[
            "bowline test device signing key v1",
            device_id,
            &counter.to_string(),
        ]));
        if let Ok(signing_key) = SigningKey::from_slice(&digest) {
            return signing_key;
        }
    }
    unreachable!("test P-256 signing key derivation failed")
}

fn device_authorization_message(fields: &[&str]) -> Vec<u8> {
    let mut message = Vec::new();
    for field in fields {
        message.extend_from_slice(&(field.len() as u64).to_le_bytes());
        message.extend_from_slice(field.as_bytes());
    }
    message
}

fn recovery_proof_verifier(workspace_id: &str, envelope_id: &str, words: &str) -> String {
    recovery_proof_verifier_from_proof(
        &recovery_proof(workspace_id, envelope_id, words),
        workspace_id,
        envelope_id,
    )
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

fn sha256_proof_fields(fields: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for field in fields {
        hasher.update((field.len() as u64).to_le_bytes());
        hasher.update(field.as_bytes());
    }
    let digest = hasher.finalize();
    format!("{digest:x}")
}
