use bowline_control_plane::{
    ControlPlaneError, DeterministicClock, DeterministicIdGenerator, DeviceApprovalInput,
    DeviceControlPlaneClient, DeviceRequestInput, DeviceRequestInputDraft, FakeControlPlaneClient,
};
use bowline_core::{
    devices::DevicePlatform,
    ids::{DeviceApprovalRequestId, DeviceId, WorkspaceId},
};

use super::{
    ApproveDeviceOptions, DeviceRequestOptions, accept_device_grant, approve_device_request,
    create_device_request, ensure_first_device_trust_root, grants, matching_code,
};
use crate::{device_keys::DeviceKeyStore, fakes::FakeKeychain, trust::TrustError};

#[test]
fn matching_code_uses_full_public_key_digest_and_binds_verifier() {
    let code = matching_code(
        "workspace-1",
        "device-1",
        "age1examplepublickey",
        "dapv_example_verifier",
    );

    assert!(code.starts_with("bowline-"));
    assert_eq!(code.len(), "bowline-".len() + 64);
    assert_ne!(
        code,
        matching_code(
            "workspace-1",
            "device-1",
            "age1examplepublickey",
            "dapv_attacker_verifier",
        )
    );
}

#[test]
fn device_trust_flow_caches_requester_and_approver_verifiers() {
    let control_plane = FakeControlPlaneClient::new(
        DeterministicClock::new(1),
        DeterministicIdGenerator::new("approve-finish-action-test"),
    );
    let workspace_id = WorkspaceId::new("workspace-approve-finish-action");
    control_plane.create_workspace(workspace_id.as_str());
    let trusted_keychain = FakeKeychain::default();
    ensure_first_device_trust_root(
        &control_plane,
        &trusted_keychain,
        workspace_id.clone(),
        DeviceId::new("trusted-device"),
        "Trusted Mac",
        DevicePlatform::Macos,
        "t000000000001",
    )
    .expect("first device");
    let trusted_verifier = grants::device_authorization_proof_verifier(
        &trusted_keychain
            .load_or_create_device_identity()
            .expect("trusted identity"),
    )
    .expect("trusted verifier");
    let requester_keychain = FakeKeychain::default();
    let request = create_device_request(
        &control_plane,
        &requester_keychain,
        DeviceRequestOptions {
            workspace_id: workspace_id.clone(),
            device_id: DeviceId::new("fresh-linux"),
            device_name: "Fresh Linux".to_string(),
            platform: DevicePlatform::Linux,
            host: None,
            lease_id: None,
            root: Some("~/Code Projects".to_string()),
            runtime: None,
            generated_at: "t000000000002".to_string(),
        },
    )
    .expect("fresh device request");
    let request_id = request.request_id.clone();
    let requester_verifier = grants::device_authorization_proof_verifier(
        &requester_keychain
            .load_or_create_device_identity()
            .expect("requester identity"),
    )
    .expect("requester verifier");

    let output = approve_device_request(
        &control_plane,
        &trusted_keychain,
        ApproveDeviceOptions {
            workspace_id: workspace_id.clone(),
            request_id: request_id.clone(),
            approver_device_id: DeviceId::new("trusted-device"),
            generated_at: "t000000000003".to_string(),
        },
    )
    .expect("approve device request");

    assert!(has_cached_verifier(
        &trusted_keychain,
        &workspace_id,
        "fresh-linux",
        &requester_verifier,
    ));
    accept_device_grant(
        &control_plane,
        &requester_keychain,
        &workspace_id,
        &request_id,
        &DeviceId::new("fresh-linux"),
    )
    .expect("requester accepts grant");
    assert!(has_cached_verifier(
        &requester_keychain,
        &workspace_id,
        "trusted-device",
        &trusted_verifier,
    ));
    assert!(has_cached_verifier(
        &requester_keychain,
        &workspace_id,
        "fresh-linux",
        &requester_verifier,
    ));
    assert_eq!(
        output
            .next_actions
            .first()
            .and_then(|action| action.command.as_deref()),
        Some("bowline setup --root '~/Code Projects' --json")
    );
}

#[test]
fn approve_rejects_requester_verifier_not_bound_to_matching_code() {
    let control_plane = FakeControlPlaneClient::new(
        DeterministicClock::new(1),
        DeterministicIdGenerator::new("requester-verifier-binding-test"),
    );
    let workspace_id = WorkspaceId::new("workspace-requester-verifier-binding");
    control_plane.create_workspace(workspace_id.as_str());
    let trusted_keychain = trusted_keychain_for(&control_plane, &workspace_id);
    let requester_keychain = FakeKeychain::default();
    let requester_identity = requester_keychain
        .load_or_create_device_identity()
        .expect("requester identity");
    let genuine_verifier =
        grants::device_authorization_proof_verifier(&requester_identity).expect("verifier");
    let request = control_plane
        .create_device_request(DeviceRequestInput::new(DeviceRequestInputDraft {
            workspace_id: workspace_id.clone(),
            device_id: DeviceId::new("fresh-linux"),
            device_name: "Fresh Linux".to_string(),
            device_public_key: requester_identity.public_key.as_str().to_string(),
            device_fingerprint: requester_identity.fingerprint.as_str().to_string(),
            device_authorization_proof_verifier: "dapv_attacker".to_string(),
            matching_code: matching_code(
                workspace_id.as_str(),
                "fresh-linux",
                requester_identity.public_key.as_str(),
                &genuine_verifier,
            ),
        }))
        .expect("tampered request");

    let error = approve_device_request(
        &control_plane,
        &trusted_keychain,
        ApproveDeviceOptions {
            workspace_id,
            request_id: DeviceApprovalRequestId::new(request.request_id),
            approver_device_id: DeviceId::new("trusted-device"),
            generated_at: "t000000000003".to_string(),
        },
    )
    .expect_err("mismatched verifier binding is rejected");

    assert!(matches!(
        error,
        TrustError::ControlPlane(ControlPlaneError::Conflict {
            resource: "device-request",
            ..
        })
    ));
}

#[test]
fn grant_acceptance_rejects_authorizer_verifier_under_wrong_device_id() {
    let control_plane = FakeControlPlaneClient::new(
        DeterministicClock::new(1),
        DeterministicIdGenerator::new("grant-authorizer-binding-test"),
    );
    let workspace_id = WorkspaceId::new("workspace-grant-authorizer-binding");
    control_plane.create_workspace(workspace_id.as_str());
    let trusted_keychain = trusted_keychain_for(&control_plane, &workspace_id);
    let requester_keychain = FakeKeychain::default();
    let requester_device_id = DeviceId::new("fresh-linux");
    let request = create_device_request(
        &control_plane,
        &requester_keychain,
        DeviceRequestOptions {
            workspace_id: workspace_id.clone(),
            device_id: requester_device_id.clone(),
            device_name: "Fresh Linux".to_string(),
            platform: DevicePlatform::Linux,
            host: None,
            lease_id: None,
            root: None,
            runtime: None,
            generated_at: "t000000000002".to_string(),
        },
    )
    .expect("fresh device request");
    let workspace_key = trusted_keychain
        .load_workspace_key(&workspace_id)
        .expect("workspace key readable")
        .expect("trusted workspace key");
    let pending_request = control_plane
        .list_device_trust(&workspace_id)
        .expect("trust list")
        .pending_requests
        .into_iter()
        .find(|pending| pending.request_id == request.request_id.as_str())
        .expect("pending request");
    let ciphertext = grants::encrypt_workspace_key_for_request(
        &workspace_key,
        &pending_request,
        Some(grants::DeviceGrantAuthorizer {
            device_id: DeviceId::new("spoofed-device"),
            device_authorization_proof_verifier: "dapv_spoofed".to_string(),
        }),
    )
    .expect("grant ciphertext");
    let grant_acceptance_proof =
        grants::grant_acceptance_proof(&workspace_key, &request.request_id, &requester_device_id);
    control_plane
        .approve_device_request_for_harness(DeviceApprovalInput {
            request_id: request.request_id.clone(),
            approved_by_device_id: DeviceId::new("trusted-device"),
            approved_by_device_proof: String::new(),
            encrypted_grant_ciphertext: ciphertext,
            grant_acceptance_proof_verifier: grants::grant_acceptance_proof_verifier(
                &grant_acceptance_proof,
            ),
            key_epoch: workspace_key.key_epoch,
            expires_in_ticks: 600,
        })
        .expect("harness approval");

    let error = accept_device_grant(
        &control_plane,
        &requester_keychain,
        &workspace_id,
        &request.request_id,
        &requester_device_id,
    )
    .expect_err("spoofed authorizer device id is rejected");

    assert!(matches!(
        error,
        TrustError::Grant(grants::GrantError::AuthorizerMismatch)
    ));
    assert!(
        requester_keychain
            .load_workspace_key(&workspace_id)
            .expect("requester workspace key readable")
            .is_none()
    );
    assert!(!has_cached_verifier(
        &requester_keychain,
        &workspace_id,
        "spoofed-device",
        "dapv_spoofed",
    ));
}

fn trusted_keychain_for(
    control_plane: &FakeControlPlaneClient,
    workspace_id: &WorkspaceId,
) -> FakeKeychain {
    let trusted_keychain = FakeKeychain::default();
    ensure_first_device_trust_root(
        control_plane,
        &trusted_keychain,
        workspace_id.clone(),
        DeviceId::new("trusted-device"),
        "Trusted Mac",
        DevicePlatform::Macos,
        "t000000000001",
    )
    .expect("first device");
    trusted_keychain
}

fn has_cached_verifier(
    keychain: &FakeKeychain,
    workspace_id: &WorkspaceId,
    device_id: &str,
    proof_verifier: &str,
) -> bool {
    keychain
        .load_device_proof_verifiers()
        .expect("cache readable")
        .iter()
        .any(|verifier| {
            &verifier.workspace_id == workspace_id
                && verifier.device_id.as_str() == device_id
                && verifier.proof_verifier == proof_verifier
        })
}
