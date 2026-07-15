use super::*;

fn signed_recovery_envelope_input(
    workspace_id: &str,
    envelope_id: &str,
    created_by_device_id: &str,
    action: &str,
    ciphertext: &str,
    fingerprint: &str,
    recovery_proof_verifier: String,
) -> RecoveryEnvelopeInput {
    let subject = recovery_envelope_payload_proof_subject(&RecoveryEnvelopeInput {
        workspace_id: WorkspaceId::new(workspace_id),
        envelope_id: RecoveryEnvelopeId::new(envelope_id),
        created_by_device_id: DeviceId::new(created_by_device_id),
        created_by_device_proof: String::new(),
        ciphertext: ciphertext.to_string(),
        fingerprint: fingerprint.to_string(),
        recovery_proof_verifier: recovery_proof_verifier.clone(),
    });
    RecoveryEnvelopeInput {
        workspace_id: WorkspaceId::new(workspace_id),
        envelope_id: RecoveryEnvelopeId::new(envelope_id),
        created_by_device_id: DeviceId::new(created_by_device_id),
        created_by_device_proof: device_proof(workspace_id, created_by_device_id, action, &subject),
        ciphertext: ciphertext.to_string(),
        fingerprint: fingerprint.to_string(),
        recovery_proof_verifier,
    }
}

#[test]
fn fake_device_approval_creates_encrypted_grant_and_authorized_device() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-1");
    control_plane
        .create_first_authorized_device(FirstAuthorizedDeviceInput {
            workspace_id: WorkspaceId::new("workspace-1"),
            device_id: DeviceId::new("device-1"),
            device_name: "macbook".to_string(),
            platform: "macos".to_string(),
            device_fingerprint: "fp_device_1".to_string(),
            device_authorization_proof_verifier: device_verifier("device-1"),
        })
        .expect("first device trust root");
    let request = control_plane
        .create_device_request(
            DeviceRequestInput::new(DeviceRequestInputDraft {
                workspace_id: WorkspaceId::new("workspace-1"),
                device_id: DeviceId::new("device-2"),
                device_name: "laptop".to_string(),
                device_public_key: "age1device2".to_string(),
                device_fingerprint: "fp_device_2".to_string(),
                device_authorization_proof_verifier: device_verifier("device-2"),
                matching_code: "maple-river-4821".to_string(),
            })
            .with_device_proof_verifier("device-2"),
        )
        .expect("device request");

    let approval_input = DeviceApprovalInput {
        request_id: request.request_id.clone(),
        approved_by_device_id: DeviceId::new("device-1"),
        approved_by_device_proof: device_proof(
            "workspace-1",
            "device-1",
            "approve-device-request",
            &device_request_proof_subject(&request.request_id),
        ),
        encrypted_grant_ciphertext: "age-encrypted-workspace-key".to_string(),
        grant_acceptance_proof_verifier: grant_acceptance_proof_verifier(
            "workspace-1",
            &request.request_id,
            "device-2",
        ),
        key_epoch: 1,
        expires_in_ticks: 600,
    };
    let approval = control_plane
        .approve_device_request(approval_input)
        .expect("trusted device can approve");

    assert!(!approval.harness_only);
    assert_eq!(approval.device_id, "device-2");
    assert_eq!(
        approval.encrypted_grant_ciphertext,
        "age-encrypted-workspace-key"
    );

    let trust_after_approval = control_plane
        .list_device_trust(&WorkspaceId::new("workspace-1"))
        .expect("trust list");
    assert_eq!(trust_after_approval.authorized_devices.len(), 1);
    assert_eq!(trust_after_approval.pending_requests.len(), 1);
    assert_eq!(
        trust_after_approval.pending_requests[0].state,
        bowline_control_plane::DeviceRequestState::Approved
    );

    let fetched = control_plane
        .get_encrypted_device_grant(&request.request_id, &DeviceId::new("device-2"))
        .expect("grant lookup")
        .expect("approved grant");
    assert_eq!(fetched.grant_id, approval.grant_id);

    let accepted = control_plane
        .confirm_device_grant_accepted(GrantAcceptanceInput {
            request_id: request.request_id.clone(),
            device_id: DeviceId::new("device-2"),
            grant_acceptance_proof: grant_acceptance_proof(
                "workspace-1",
                &request.request_id,
                "device-2",
            ),
        })
        .expect("requester accepts grant");
    assert!(accepted.accepted_at.is_some());

    let trust_after_acceptance = control_plane
        .list_device_trust(&WorkspaceId::new("workspace-1"))
        .expect("trust list");
    assert_eq!(trust_after_acceptance.authorized_devices.len(), 2);
    assert!(
        trust_after_acceptance
            .authorized_devices
            .iter()
            .all(|device| {
                device
                    .device_authorization_proof_verifier
                    .as_deref()
                    .is_some_and(|verifier| verifier.starts_with("dapv_p256_v1_"))
            })
    );
    assert!(trust_after_acceptance.pending_requests.is_empty());
}

#[test]
fn authorized_device_id_cannot_create_pending_request_with_new_key() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-duplicate-device");
    create_first_device(&control_plane, "workspace-duplicate-device", "device-1");

    let error = control_plane
        .create_device_request(
            DeviceRequestInput::new(DeviceRequestInputDraft {
                workspace_id: WorkspaceId::new("workspace-duplicate-device"),
                device_id: DeviceId::new("device-1"),
                device_name: "attacker".to_string(),
                device_public_key: "age1attacker".to_string(),
                device_fingerprint: "fp_attacker".to_string(),
                device_authorization_proof_verifier: device_verifier("attacker"),
                matching_code: "maple-river-4821".to_string(),
            })
            .with_device_proof_verifier("attacker"),
        )
        .expect_err("authorized device id cannot request trust again");

    assert!(matches!(error, ControlPlaneError::Conflict { .. }));
}

#[test]
fn spoofed_approver_device_id_cannot_approve_without_device_proof() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-spoofed-approver");
    create_first_device(&control_plane, "workspace-spoofed-approver", "device-1");
    let request = control_plane
        .create_device_request(
            DeviceRequestInput::new(DeviceRequestInputDraft {
                workspace_id: WorkspaceId::new("workspace-spoofed-approver"),
                device_id: DeviceId::new("device-2"),
                device_name: "linux".to_string(),
                device_public_key: "age1device2".to_string(),
                device_fingerprint: "fp_device_2".to_string(),
                device_authorization_proof_verifier: device_verifier("device-2"),
                matching_code: "maple-river-4821".to_string(),
            })
            .with_device_proof_verifier("device-2"),
        )
        .expect("device request");

    let error = control_plane
        .approve_device_request(DeviceApprovalInput {
            request_id: request.request_id.clone(),
            approved_by_device_id: DeviceId::new("device-1"),
            approved_by_device_proof: device_proof(
                "workspace-spoofed-approver",
                "device-2",
                "approve-device-request",
                &device_request_proof_subject(&request.request_id),
            ),
            encrypted_grant_ciphertext: "age-encrypted-workspace-key".to_string(),
            grant_acceptance_proof_verifier: grant_acceptance_proof_verifier(
                "workspace-spoofed-approver",
                &request.request_id,
                "device-2",
            ),
            key_epoch: 1,
            expires_in_ticks: 600,
        })
        .expect_err("device id alone is not enough to approve a request");

    assert_device_not_trusted(error);

    let public_verifier = control_plane
        .approve_device_request(DeviceApprovalInput {
            request_id: request.request_id.clone(),
            approved_by_device_id: DeviceId::new("device-1"),
            approved_by_device_proof: device_verifier("device-1"),
            encrypted_grant_ciphertext: "age-encrypted-workspace-key".to_string(),
            grant_acceptance_proof_verifier: grant_acceptance_proof_verifier(
                "workspace-spoofed-approver",
                &request.request_id,
                "device-2",
            ),
            key_epoch: 1,
            expires_in_ticks: 600,
        })
        .expect_err("stored public verifier is not a bearer proof");
    assert_device_not_trusted(public_verifier);
}

#[test]
fn accepted_grant_cannot_reauthorize_a_revoked_device() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-revoked-grant");
    create_first_device(&control_plane, "workspace-revoked-grant", "device-1");
    let request = control_plane
        .create_device_request(
            DeviceRequestInput::new(DeviceRequestInputDraft {
                workspace_id: WorkspaceId::new("workspace-revoked-grant"),
                device_id: DeviceId::new("device-2"),
                device_name: "linux".to_string(),
                device_public_key: "age1device2".to_string(),
                device_fingerprint: "fp_device_2".to_string(),
                device_authorization_proof_verifier: device_verifier("device-2"),
                matching_code: "maple-river-4821".to_string(),
            })
            .with_device_proof_verifier("device-2"),
        )
        .expect("device request");
    control_plane
        .approve_device_request(DeviceApprovalInput {
            request_id: request.request_id.clone(),
            approved_by_device_id: DeviceId::new("device-1"),
            approved_by_device_proof: device_proof(
                "workspace-revoked-grant",
                "device-1",
                "approve-device-request",
                &device_request_proof_subject(&request.request_id),
            ),
            encrypted_grant_ciphertext: "age-encrypted-workspace-key".to_string(),
            grant_acceptance_proof_verifier: grant_acceptance_proof_verifier(
                "workspace-revoked-grant",
                &request.request_id,
                "device-2",
            ),
            key_epoch: 1,
            expires_in_ticks: 600,
        })
        .expect("trusted device approves");
    control_plane
        .confirm_device_grant_accepted(GrantAcceptanceInput {
            request_id: request.request_id.clone(),
            device_id: DeviceId::new("device-2"),
            grant_acceptance_proof: grant_acceptance_proof(
                "workspace-revoked-grant",
                &request.request_id,
                "device-2",
            ),
        })
        .expect("requester accepts grant");
    control_plane
        .revoke_device(DeviceRevocationInput {
            workspace_id: WorkspaceId::new("workspace-revoked-grant"),
            device_id: DeviceId::new("device-2"),
            revoked_by_device_id: DeviceId::new("device-1"),
            revoked_by_device_proof: device_proof(
                "workspace-revoked-grant",
                "device-1",
                "revoke-device",
                &device_revocation_proof_subject("device-2"),
            ),
            reason: "lost device".to_string(),
        })
        .expect("trusted device revokes the accepted device");

    let error = control_plane
        .confirm_device_grant_accepted(GrantAcceptanceInput {
            request_id: request.request_id.clone(),
            device_id: DeviceId::new("device-2"),
            grant_acceptance_proof: grant_acceptance_proof(
                "workspace-revoked-grant",
                &request.request_id,
                "device-2",
            ),
        })
        .expect_err("accepted grant cannot reauthorize a revoked device");
    assert_device_not_trusted(error);

    let trust = control_plane
        .list_device_trust(&WorkspaceId::new("workspace-revoked-grant"))
        .expect("trust list");
    assert!(
        !trust
            .authorized_devices
            .iter()
            .any(|device| device.device_id == "device-2")
    );
    assert!(
        trust
            .revoked_devices
            .iter()
            .any(|device| device.device_id == "device-2")
    );
}

#[test]
fn revoking_last_trusted_device_requires_recovery_or_another_device() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-last-device");
    create_first_device(&control_plane, "workspace-last-device", "device-1");

    let blocked = control_plane
        .revoke_device(DeviceRevocationInput {
            workspace_id: WorkspaceId::new("workspace-last-device"),
            device_id: DeviceId::new("device-1"),
            revoked_by_device_id: DeviceId::new("device-1"),
            revoked_by_device_proof: device_proof(
                "workspace-last-device",
                "device-1",
                "revoke-device",
                &device_revocation_proof_subject("device-1"),
            ),
            reason: "self revoke without recovery".to_string(),
        })
        .expect_err("last trust path cannot be removed");
    assert_device_not_trusted(blocked);

    control_plane
        .create_recovery_envelope(signed_recovery_envelope_input(
            "workspace-last-device",
            "rk_last_device",
            "device-1",
            "create-recovery-envelope",
            "encrypted-workspace-key",
            "rk_last_device",
            recovery_proof_verifier(
                "workspace-last-device",
                "rk_last_device",
                "last device recovery words",
            ),
        ))
        .expect("trusted device creates recovery envelope");
    control_plane
        .verify_recovery_envelope(
            &WorkspaceId::new("workspace-last-device"),
            &RecoveryEnvelopeId::new("rk_last_device"),
            &DeviceId::new("device-1"),
            &device_proof(
                "workspace-last-device",
                "device-1",
                "verify-recovery-envelope",
                &recovery_envelope_proof_subject("rk_last_device"),
            ),
            &recovery_proof(
                "workspace-last-device",
                "rk_last_device",
                "last device recovery words",
            ),
        )
        .expect("trusted device verifies recovery envelope");

    let revoked = control_plane
        .revoke_device(DeviceRevocationInput {
            workspace_id: WorkspaceId::new("workspace-last-device"),
            device_id: DeviceId::new("device-1"),
            revoked_by_device_id: DeviceId::new("device-1"),
            revoked_by_device_proof: device_proof(
                "workspace-last-device",
                "device-1",
                "revoke-device",
                &device_revocation_proof_subject("device-1"),
            ),
            reason: "recovery key exists".to_string(),
        })
        .expect("active recovery key preserves a trust path");
    assert_eq!(revoked.device_id, "device-1");

    let recreate = control_plane
        .create_first_authorized_device(FirstAuthorizedDeviceInput {
            workspace_id: WorkspaceId::new("workspace-last-device"),
            device_id: DeviceId::new("device-2"),
            device_name: "new macbook".to_string(),
            platform: "macos".to_string(),
            device_fingerprint: "fp_device_2".to_string(),
            device_authorization_proof_verifier: device_verifier("device-2"),
        })
        .expect_err("trust history must use recovery instead of first-device init");
    assert!(matches!(recreate, ControlPlaneError::Conflict { .. }));
}

#[test]
fn expired_device_request_cannot_be_approved() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-expired-request");
    control_plane
        .create_first_authorized_device(FirstAuthorizedDeviceInput {
            workspace_id: WorkspaceId::new("workspace-expired-request"),
            device_id: DeviceId::new("device-1"),
            device_name: "macbook".to_string(),
            platform: "macos".to_string(),
            device_fingerprint: "fp_device_1".to_string(),
            device_authorization_proof_verifier: device_verifier("device-1"),
        })
        .expect("first device trust root");
    let mut request_input = DeviceRequestInput::new(DeviceRequestInputDraft {
        workspace_id: WorkspaceId::new("workspace-expired-request"),
        device_id: DeviceId::new("device-2"),
        device_name: "laptop".to_string(),
        device_public_key: "age1device2".to_string(),
        device_fingerprint: "fp_device_2".to_string(),
        device_authorization_proof_verifier: device_verifier("device-2"),
        matching_code: "maple-river-4821".to_string(),
    });
    request_input.expires_in_ticks = 1;
    let request = control_plane
        .create_device_request(request_input)
        .expect("device request");

    let error = control_plane
        .approve_device_request(DeviceApprovalInput {
            request_id: request.request_id.clone(),
            approved_by_device_id: DeviceId::new("device-1"),
            approved_by_device_proof: device_proof(
                "workspace-expired-request",
                "device-1",
                "approve-device-request",
                &device_request_proof_subject(&request.request_id),
            ),
            encrypted_grant_ciphertext: "age-encrypted-workspace-key".to_string(),
            grant_acceptance_proof_verifier: grant_acceptance_proof_verifier(
                "workspace-expired-request",
                &request.request_id,
                "device-2",
            ),
            key_epoch: 1,
            expires_in_ticks: 600,
        })
        .expect_err("expired request is rejected");

    assert!(matches!(error, ControlPlaneError::Conflict { .. }));
}

#[test]
fn expired_device_grant_cannot_be_accepted() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-expired-grant");
    control_plane
        .create_first_authorized_device(FirstAuthorizedDeviceInput {
            workspace_id: WorkspaceId::new("workspace-expired-grant"),
            device_id: DeviceId::new("device-1"),
            device_name: "macbook".to_string(),
            platform: "macos".to_string(),
            device_fingerprint: "fp_device_1".to_string(),
            device_authorization_proof_verifier: device_verifier("device-1"),
        })
        .expect("first device trust root");
    let request = control_plane
        .create_device_request(
            DeviceRequestInput::new(DeviceRequestInputDraft {
                workspace_id: WorkspaceId::new("workspace-expired-grant"),
                device_id: DeviceId::new("device-2"),
                device_name: "laptop".to_string(),
                device_public_key: "age1device2".to_string(),
                device_fingerprint: "fp_device_2".to_string(),
                device_authorization_proof_verifier: device_verifier("device-2"),
                matching_code: "maple-river-4821".to_string(),
            })
            .with_device_proof_verifier("device-2"),
        )
        .expect("device request");
    control_plane
        .approve_device_request(DeviceApprovalInput {
            request_id: request.request_id.clone(),
            approved_by_device_id: DeviceId::new("device-1"),
            approved_by_device_proof: device_proof(
                "workspace-expired-grant",
                "device-1",
                "approve-device-request",
                &device_request_proof_subject(&request.request_id),
            ),
            encrypted_grant_ciphertext: "age-encrypted-workspace-key".to_string(),
            grant_acceptance_proof_verifier: grant_acceptance_proof_verifier(
                "workspace-expired-grant",
                &request.request_id,
                "device-2",
            ),
            key_epoch: 1,
            expires_in_ticks: 1,
        })
        .expect("trusted device approves");

    let fetch_error = control_plane
        .get_encrypted_device_grant(&request.request_id, &DeviceId::new("device-2"))
        .expect_err("expired grant ciphertext is not returned");
    assert!(matches!(
        fetch_error,
        ControlPlaneError::Rejected {
            code: RejectionCode::InvalidRequest,
            ..
        }
    ));

    let error = control_plane
        .confirm_device_grant_accepted(GrantAcceptanceInput {
            request_id: request.request_id.clone(),
            device_id: DeviceId::new("device-2"),
            grant_acceptance_proof: grant_acceptance_proof(
                "workspace-expired-grant",
                &request.request_id,
                "device-2",
            ),
        })
        .expect_err("expired grant is rejected");

    assert!(matches!(
        error,
        ControlPlaneError::Rejected {
            code: RejectionCode::InvalidRequest,
            ..
        }
    ));
}

#[test]
fn recovery_authorization_requires_private_proof_not_public_fingerprint() {
    let clock = bowline_control_plane::DeterministicClock::default();
    let control_plane = FakeControlPlaneClient::new(
        clock.clone(),
        bowline_control_plane::DeterministicIdGenerator::default(),
    );
    control_plane.create_workspace("workspace-recovery-proof");
    create_first_device(&control_plane, "workspace-recovery-proof", "device-1");
    let recovery_proof = recovery_proof(
        "workspace-recovery-proof",
        "rk_public",
        "private recovery words",
    );
    let recovery_proof_verifier = recovery_proof_verifier(
        "workspace-recovery-proof",
        "rk_public",
        "private recovery words",
    );
    control_plane
        .create_recovery_envelope(signed_recovery_envelope_input(
            "workspace-recovery-proof",
            "rk_public",
            "device-1",
            "create-recovery-envelope",
            "encrypted-workspace-key",
            "rk_public",
            recovery_proof_verifier.clone(),
        ))
        .expect("trusted device creates recovery envelope");
    let invalid_verify = control_plane
        .verify_recovery_envelope(
            &WorkspaceId::new("workspace-recovery-proof"),
            &RecoveryEnvelopeId::new("rk_public"),
            &DeviceId::new("device-1"),
            &device_proof(
                "workspace-recovery-proof",
                "device-1",
                "verify-recovery-envelope",
                &recovery_envelope_proof_subject("rk_public"),
            ),
            "rkp_wrong",
        )
        .expect_err("trusted device proof alone cannot verify recovery");
    assert!(matches!(invalid_verify, ControlPlaneError::Conflict { .. }));
    let envelope = control_plane
        .list_recovery_envelopes(&WorkspaceId::new("workspace-recovery-proof"))
        .expect("recovery envelopes")
        .into_iter()
        .find(|envelope| envelope.envelope_id == "rk_public")
        .expect("created envelope");
    assert_eq!(envelope.state, RecoveryEnvelopeState::GeneratedUnverified);

    control_plane
        .verify_recovery_envelope(
            &WorkspaceId::new("workspace-recovery-proof"),
            &RecoveryEnvelopeId::new("rk_public"),
            &DeviceId::new("device-1"),
            &device_proof(
                "workspace-recovery-proof",
                "device-1",
                "verify-recovery-envelope",
                &recovery_envelope_proof_subject("rk_public"),
            ),
            &recovery_proof,
        )
        .expect("trusted device verifies envelope");
    let request = control_plane
        .create_device_request(
            DeviceRequestInput::new(DeviceRequestInputDraft {
                workspace_id: WorkspaceId::new("workspace-recovery-proof"),
                device_id: DeviceId::new("device-2"),
                device_name: "linux".to_string(),
                device_public_key: "age1device2".to_string(),
                device_fingerprint: "fp_device_2".to_string(),
                device_authorization_proof_verifier: device_verifier("device-2"),
                matching_code: "maple-river-4821".to_string(),
            })
            .with_device_proof_verifier("device-2"),
        )
        .expect("device request");

    for invalid_key_epoch in [0, 2] {
        let invalid_epoch = control_plane
            .authorize_device_with_recovery(RecoveryDeviceAuthorizationInput {
                workspace_id: WorkspaceId::new("workspace-recovery-proof"),
                envelope_id: RecoveryEnvelopeId::new("rk_public"),
                request_id: request.request_id.clone(),
                encrypted_grant_ciphertext: "grant-ciphertext".to_string(),
                grant_acceptance_proof_verifier: grant_acceptance_proof_verifier(
                    "workspace-recovery-proof",
                    &request.request_id,
                    "device-2",
                ),
                key_epoch: invalid_key_epoch,
                recovery_proof: recovery_proof.clone(),
                expires_in_ticks: 600,
            })
            .expect_err("recovery grant must use the current workspace key epoch");
        assert!(matches!(invalid_epoch, ControlPlaneError::Conflict { .. }));
    }

    let mut expired_input = DeviceRequestInput::new(DeviceRequestInputDraft {
        workspace_id: WorkspaceId::new("workspace-recovery-proof"),
        device_id: DeviceId::new("device-expired"),
        device_name: "expired linux".to_string(),
        device_public_key: "age1expired".to_string(),
        device_fingerprint: "fp_expired".to_string(),
        device_authorization_proof_verifier: device_verifier("device-expired"),
        matching_code: "expired-code".to_string(),
    });
    expired_input.expires_in_ticks = 1;
    let expired_request = control_plane
        .create_device_request(expired_input)
        .expect("expired recovery request seed");
    clock.now();
    let expired = control_plane
        .authorize_device_with_recovery(RecoveryDeviceAuthorizationInput {
            workspace_id: WorkspaceId::new("workspace-recovery-proof"),
            envelope_id: RecoveryEnvelopeId::new("rk_public"),
            request_id: expired_request.request_id.clone(),
            encrypted_grant_ciphertext: "grant-ciphertext".to_string(),
            grant_acceptance_proof_verifier: "expired-verifier".to_string(),
            key_epoch: 1,
            recovery_proof: recovery_proof.clone(),
            expires_in_ticks: 600,
        })
        .expect_err("expired recovery requests fail without mutation");
    assert!(matches!(expired, ControlPlaneError::Conflict { .. }));
    let expired_after = control_plane
        .list_device_trust(&WorkspaceId::new("workspace-recovery-proof"))
        .expect("trust list after expired recovery attempt")
        .pending_requests
        .into_iter()
        .find(|candidate| candidate.request_id == expired_request.request_id)
        .expect("expired request remains represented");
    assert_eq!(
        expired_after.state,
        bowline_control_plane::DeviceRequestState::Pending
    );

    let public_fingerprint = control_plane
        .authorize_device_with_recovery(RecoveryDeviceAuthorizationInput {
            workspace_id: WorkspaceId::new("workspace-recovery-proof"),
            envelope_id: RecoveryEnvelopeId::new("rk_public"),
            request_id: request.request_id.clone(),
            encrypted_grant_ciphertext: "grant-ciphertext".to_string(),
            grant_acceptance_proof_verifier: grant_acceptance_proof_verifier(
                "workspace-recovery-proof",
                &request.request_id,
                "device-2",
            ),
            key_epoch: 1,
            recovery_proof: "rk_public".to_string(),
            expires_in_ticks: 600,
        })
        .expect_err("public fingerprint is not a recovery proof");
    assert!(matches!(
        public_fingerprint,
        ControlPlaneError::Conflict { .. }
    ));

    let stored_verifier = control_plane
        .authorize_device_with_recovery(RecoveryDeviceAuthorizationInput {
            workspace_id: WorkspaceId::new("workspace-recovery-proof"),
            envelope_id: RecoveryEnvelopeId::new("rk_public"),
            request_id: request.request_id.clone(),
            encrypted_grant_ciphertext: "grant-ciphertext".to_string(),
            grant_acceptance_proof_verifier: grant_acceptance_proof_verifier(
                "workspace-recovery-proof",
                &request.request_id,
                "device-2",
            ),
            key_epoch: 1,
            recovery_proof: recovery_proof_verifier,
            expires_in_ticks: 600,
        })
        .expect_err("stored recovery verifier is not a bearer proof");
    assert!(matches!(
        stored_verifier,
        ControlPlaneError::Conflict { .. }
    ));

    let recovered = control_plane
        .authorize_device_with_recovery(RecoveryDeviceAuthorizationInput {
            workspace_id: WorkspaceId::new("workspace-recovery-proof"),
            envelope_id: RecoveryEnvelopeId::new("rk_public"),
            request_id: request.request_id.clone(),
            encrypted_grant_ciphertext: "grant-ciphertext".to_string(),
            grant_acceptance_proof_verifier: grant_acceptance_proof_verifier(
                "workspace-recovery-proof",
                &request.request_id,
                "device-2",
            ),
            key_epoch: 1,
            recovery_proof: recovery_proof.clone(),
            expires_in_ticks: 600,
        })
        .expect("private recovery proof authorizes the pending device");
    assert_eq!(recovered.approved_by_device_id, "recovery:rk_public");
    assert_eq!(
        recovered.grant_id.as_str(),
        format!("recovery-grant:{}", request.request_id.as_str())
    );
    let replayed = control_plane
        .authorize_device_with_recovery(RecoveryDeviceAuthorizationInput {
            workspace_id: WorkspaceId::new("workspace-recovery-proof"),
            envelope_id: RecoveryEnvelopeId::new("rk_public"),
            request_id: request.request_id.clone(),
            encrypted_grant_ciphertext: "different-settled-replay".to_string(),
            grant_acceptance_proof_verifier: "different-settled-replay".to_string(),
            key_epoch: 1,
            recovery_proof: recovery_proof.clone(),
            expires_in_ticks: 0,
        })
        .expect("settled recovery authorization replays the existing grant");
    assert_eq!(replayed, recovered);
    control_plane.set_workspace_key_epoch("workspace-recovery-proof", 2);
    let stale_replay = control_plane
        .authorize_device_with_recovery(RecoveryDeviceAuthorizationInput {
            workspace_id: WorkspaceId::new("workspace-recovery-proof"),
            envelope_id: RecoveryEnvelopeId::new("rk_public"),
            request_id: request.request_id.clone(),
            encrypted_grant_ciphertext: "settled-replay".to_string(),
            grant_acceptance_proof_verifier: "settled-replay".to_string(),
            key_epoch: 2,
            recovery_proof: recovery_proof.clone(),
            expires_in_ticks: 600,
        })
        .expect_err("stale settled recovery grants are rejected");
    assert!(matches!(stale_replay, ControlPlaneError::Conflict { .. }));
    control_plane.set_workspace_key_epoch("workspace-recovery-proof", 1);
    control_plane.revoke_device_grant(&request.request_id);
    let revoked_replay = control_plane
        .authorize_device_with_recovery(RecoveryDeviceAuthorizationInput {
            workspace_id: WorkspaceId::new("workspace-recovery-proof"),
            envelope_id: RecoveryEnvelopeId::new("rk_public"),
            request_id: request.request_id.clone(),
            encrypted_grant_ciphertext: "settled-replay".to_string(),
            grant_acceptance_proof_verifier: "settled-replay".to_string(),
            key_epoch: 1,
            recovery_proof,
            expires_in_ticks: 600,
        })
        .expect_err("revoked settled recovery grants are rejected");
    assert!(matches!(revoked_replay, ControlPlaneError::Conflict { .. }));
}

#[test]
fn revoked_recovery_envelope_cannot_be_reactivated_or_used() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-recovery-revoked");
    create_first_device(&control_plane, "workspace-recovery-revoked", "device-1");
    let recovery_proof = recovery_proof(
        "workspace-recovery-revoked",
        "rk_revoked",
        "revoked recovery words",
    );
    control_plane
        .create_recovery_envelope(signed_recovery_envelope_input(
            "workspace-recovery-revoked",
            "rk_revoked",
            "device-1",
            "create-recovery-envelope",
            "encrypted-workspace-key",
            "rk_revoked",
            recovery_proof_verifier(
                "workspace-recovery-revoked",
                "rk_revoked",
                "revoked recovery words",
            ),
        ))
        .expect("trusted device creates recovery envelope");
    let revoked = control_plane
        .revoke_recovery_envelope(
            &WorkspaceId::new("workspace-recovery-revoked"),
            &RecoveryEnvelopeId::new("rk_revoked"),
            &DeviceId::new("device-1"),
            &device_proof(
                "workspace-recovery-revoked",
                "device-1",
                "revoke-recovery-envelope",
                &recovery_envelope_proof_subject("rk_revoked"),
            ),
        )
        .expect("trusted device revokes recovery envelope");
    let replayed_revocation = control_plane
        .revoke_recovery_envelope(
            &WorkspaceId::new("workspace-recovery-revoked"),
            &RecoveryEnvelopeId::new("rk_revoked"),
            &DeviceId::new("device-1"),
            &device_proof(
                "workspace-recovery-revoked",
                "device-1",
                "revoke-recovery-envelope",
                &recovery_envelope_proof_subject("rk_revoked"),
            ),
        )
        .expect("settled recovery revocation is replay-safe");
    assert_eq!(replayed_revocation, revoked);

    let verify = control_plane
        .verify_recovery_envelope(
            &WorkspaceId::new("workspace-recovery-revoked"),
            &RecoveryEnvelopeId::new("rk_revoked"),
            &DeviceId::new("device-1"),
            &device_proof(
                "workspace-recovery-revoked",
                "device-1",
                "verify-recovery-envelope",
                &recovery_envelope_proof_subject("rk_revoked"),
            ),
            &recovery_proof,
        )
        .expect_err("revoked envelopes cannot be reactivated");
    assert!(matches!(verify, ControlPlaneError::Conflict { .. }));
    let envelope = control_plane
        .list_recovery_envelopes(&WorkspaceId::new("workspace-recovery-revoked"))
        .expect("envelopes")
        .into_iter()
        .find(|envelope| envelope.envelope_id == "rk_revoked")
        .expect("revoked envelope remains listed");
    assert_eq!(envelope.state, RecoveryEnvelopeState::Revoked);

    let request = control_plane
        .create_device_request(
            DeviceRequestInput::new(DeviceRequestInputDraft {
                workspace_id: WorkspaceId::new("workspace-recovery-revoked"),
                device_id: DeviceId::new("device-2"),
                device_name: "linux".to_string(),
                device_public_key: "age1device2".to_string(),
                device_fingerprint: "fp_device_2".to_string(),
                device_authorization_proof_verifier: device_verifier("device-2"),
                matching_code: "maple-river-4821".to_string(),
            })
            .with_device_proof_verifier("device-2"),
        )
        .expect("device request");
    let authorize = control_plane
        .authorize_device_with_recovery(RecoveryDeviceAuthorizationInput {
            workspace_id: WorkspaceId::new("workspace-recovery-revoked"),
            envelope_id: RecoveryEnvelopeId::new("rk_revoked"),
            request_id: request.request_id.clone(),
            encrypted_grant_ciphertext: "grant-ciphertext".to_string(),
            grant_acceptance_proof_verifier: grant_acceptance_proof_verifier(
                "workspace-recovery-revoked",
                &request.request_id,
                "device-2",
            ),
            key_epoch: 1,
            recovery_proof,
            expires_in_ticks: 600,
        })
        .expect_err("revoked envelopes cannot authorize devices");
    assert!(matches!(authorize, ControlPlaneError::Conflict { .. }));
}

#[test]
fn recovery_rotation_uses_rotate_proof_and_does_not_corrupt_on_conflict() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-recovery-rotate");
    create_first_device(&control_plane, "workspace-recovery-rotate", "device-1");
    control_plane
        .create_recovery_envelope(signed_recovery_envelope_input(
            "workspace-recovery-rotate",
            "rk_current",
            "device-1",
            "create-recovery-envelope",
            "current-ciphertext",
            "rk_current",
            recovery_proof_verifier(
                "workspace-recovery-rotate",
                "rk_current",
                "current recovery words",
            ),
        ))
        .expect("trusted device creates current recovery envelope");
    control_plane
        .verify_recovery_envelope(
            &WorkspaceId::new("workspace-recovery-rotate"),
            &RecoveryEnvelopeId::new("rk_current"),
            &DeviceId::new("device-1"),
            &device_proof(
                "workspace-recovery-rotate",
                "device-1",
                "verify-recovery-envelope",
                &recovery_envelope_proof_subject("rk_current"),
            ),
            &recovery_proof(
                "workspace-recovery-rotate",
                "rk_current",
                "current recovery words",
            ),
        )
        .expect("current recovery envelope is active");

    let conflict = control_plane
        .rotate_recovery_envelope(signed_recovery_envelope_input(
            "workspace-recovery-rotate",
            "rk_current",
            "device-1",
            "rotate-recovery-envelope",
            "different-ciphertext",
            "different-fingerprint",
            recovery_proof_verifier(
                "workspace-recovery-rotate",
                "rk_current",
                "different recovery words",
            ),
        ))
        .expect_err("conflicting rotation does not mutate existing envelopes first");
    assert!(matches!(conflict, ControlPlaneError::Conflict { .. }));
    let current_after_conflict = control_plane
        .list_recovery_envelopes(&WorkspaceId::new("workspace-recovery-rotate"))
        .expect("envelopes")
        .into_iter()
        .find(|envelope| envelope.envelope_id == "rk_current")
        .expect("current envelope remains");
    assert_eq!(current_after_conflict.state, RecoveryEnvelopeState::Active);

    let rotated = control_plane
        .rotate_recovery_envelope(signed_recovery_envelope_input(
            "workspace-recovery-rotate",
            "rk_next",
            "device-1",
            "rotate-recovery-envelope",
            "next-ciphertext",
            "rk_next",
            recovery_proof_verifier(
                "workspace-recovery-rotate",
                "rk_next",
                "next recovery words",
            ),
        ))
        .expect("rotate proof creates next recovery envelope");
    assert_eq!(rotated.state, RecoveryEnvelopeState::GeneratedUnverified);

    let envelopes = control_plane
        .list_recovery_envelopes(&WorkspaceId::new("workspace-recovery-rotate"))
        .expect("envelopes after rotation");
    assert_eq!(
        envelopes
            .iter()
            .find(|envelope| envelope.envelope_id == "rk_current")
            .expect("current envelope")
            .state,
        RecoveryEnvelopeState::Rotated
    );
    assert!(
        envelopes
            .iter()
            .any(|envelope| envelope.envelope_id == "rk_next")
    );
}

#[test]
fn recovery_envelope_idempotency_is_scoped_to_workspace() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-a");
    control_plane.create_workspace("workspace-b");
    create_first_device(&control_plane, "workspace-a", "device-a");
    create_first_device(&control_plane, "workspace-b", "device-b");

    control_plane
        .create_recovery_envelope(signed_recovery_envelope_input(
            "workspace-a",
            "rk_same",
            "device-a",
            "create-recovery-envelope",
            "ciphertext-a",
            "fingerprint-a",
            "proof-a".to_string(),
        ))
        .expect("workspace a creates envelope");

    let workspace_b = control_plane
        .create_recovery_envelope(signed_recovery_envelope_input(
            "workspace-b",
            "rk_same",
            "device-b",
            "create-recovery-envelope",
            "ciphertext-b",
            "fingerprint-b",
            "proof-b".to_string(),
        ))
        .expect("same envelope id in another workspace does not return workspace a metadata");
    assert_eq!(workspace_b.workspace_id, "workspace-b");
    assert_eq!(workspace_b.ciphertext, "ciphertext-b");

    let conflicting_retry = control_plane
        .create_recovery_envelope(signed_recovery_envelope_input(
            "workspace-a",
            "rk_same",
            "device-a",
            "create-recovery-envelope",
            "different-ciphertext",
            "fingerprint-a",
            "proof-a".to_string(),
        ))
        .expect_err("same workspace idempotency still rejects different metadata");
    assert!(matches!(
        conflicting_retry,
        ControlPlaneError::Conflict { .. }
    ));
}
