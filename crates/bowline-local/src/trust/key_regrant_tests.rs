//! Revocation is only a security boundary if the workspace really moves onto a
//! key epoch the revoked device never held, on its own, on every remaining
//! device. These tests exercise that end to end against the in-memory control
//! plane with real device identities and real sealing.

use bowline_control_plane::{
    ControlPlaneError, DeterministicClock, DeterministicIdGenerator, DeviceControlPlaneClient,
    DeviceRevocationInput, FakeControlPlaneClient, RejectionCode, WorkspaceKeyControlPlaneClient,
    WorkspaceKeyRegrantWorkRequest, device_revocation_proof_subject,
    key_regrant_work_proof_subject,
};
use bowline_core::{
    devices::DevicePlatform,
    ids::{DeviceId, WorkspaceId},
};

use super::{
    ApproveDeviceOptions, DeviceRequestOptions, accept_device_grant, approve_device_request,
    converge_workspace_key_epoch, create_device_request, ensure_first_device_trust_root, grants,
};
use crate::{
    device_keys::{DeviceKeyStore, WorkspaceKeyring},
    fakes::FakeKeychain,
};

const REVOKE_DEVICE_ACTION: &str = "revoke-device";

struct Workspace {
    control_plane: FakeControlPlaneClient,
    workspace_id: WorkspaceId,
}

impl Workspace {
    fn new(name: &str) -> (Self, FakeKeychain, DeviceId) {
        let control_plane = FakeControlPlaneClient::new(
            DeterministicClock::new(1),
            DeterministicIdGenerator::new(name),
        );
        let workspace_id = WorkspaceId::new(format!("workspace-{name}"));
        control_plane.create_workspace(workspace_id.as_str());
        let keychain = FakeKeychain::default();
        let device_id = DeviceId::new("device-root");
        ensure_first_device_trust_root(
            &control_plane,
            &keychain,
            workspace_id.clone(),
            device_id.clone(),
            "Root Mac",
            DevicePlatform::Macos,
            "t000000000001",
        )
        .expect("first device trust root");
        (
            Self {
                control_plane,
                workspace_id,
            },
            keychain,
            device_id,
        )
    }

    /// Enrols a device through the real request/approve/accept path so it ends
    /// up with genuine key material, a published attestation, and a proof
    /// verifier the control plane can check.
    fn enrol(
        &self,
        approver_keychain: &FakeKeychain,
        approver_device_id: &DeviceId,
        device_id: &str,
    ) -> (FakeKeychain, DeviceId) {
        let keychain = FakeKeychain::default();
        let device_id = DeviceId::new(device_id);
        let request = create_device_request(
            &self.control_plane,
            &keychain,
            DeviceRequestOptions {
                workspace_id: self.workspace_id.clone(),
                device_id: device_id.clone(),
                device_name: format!("{} laptop", device_id.as_str()),
                platform: DevicePlatform::Linux,
                host: None,
                root: None,
                runtime: None,
                generated_at: "t000000000002".to_string(),
            },
        )
        .expect("device request");
        approve_device_request(
            &self.control_plane,
            approver_keychain,
            ApproveDeviceOptions {
                workspace_id: self.workspace_id.clone(),
                request_id: request.request_id.clone(),
                approver_device_id: approver_device_id.clone(),
                generated_at: "t000000000003".to_string(),
            },
        )
        .expect("approve device request");
        accept_device_grant(
            &self.control_plane,
            &keychain,
            &self.workspace_id,
            &request.request_id,
            &device_id,
        )
        .expect("accept device grant");
        (keychain, device_id)
    }

    fn revoke(
        &self,
        revoker_keychain: &FakeKeychain,
        revoker_device_id: &DeviceId,
        revoked_device_id: &DeviceId,
    ) {
        let identity = revoker_keychain
            .load_or_create_device_identity()
            .expect("revoker identity");
        let proof = grants::device_authorization_proof(
            &identity,
            &self.workspace_id,
            revoker_device_id,
            REVOKE_DEVICE_ACTION,
            &device_revocation_proof_subject(revoked_device_id),
        )
        .expect("revocation proof");
        self.control_plane
            .revoke_device(DeviceRevocationInput {
                workspace_id: self.workspace_id.clone(),
                device_id: revoked_device_id.clone(),
                revoked_by_device_id: revoker_device_id.clone(),
                revoked_by_device_proof: proof,
                reason: "test revocation".to_string(),
            })
            .expect("revoke device");
    }

    fn converge(
        &self,
        keychain: &FakeKeychain,
        device_id: &DeviceId,
    ) -> super::KeyEpochConvergence {
        converge_workspace_key_epoch(&self.control_plane, keychain, &self.workspace_id, device_id)
            .expect("convergence pass")
    }

    fn keyring(&self, keychain: &FakeKeychain) -> WorkspaceKeyring {
        keychain
            .load_workspace_keyring(&self.workspace_id)
            .expect("keyring readable")
            .expect("keyring exists")
    }
}

#[test]
fn remaining_device_converges_onto_the_new_epoch_with_no_user_action() {
    let (workspace, root_keychain, root_device_id) = Workspace::new("regrant-converge");
    let (kept_keychain, kept_device_id) = workspace.enrol(&root_keychain, &root_device_id, "kept");
    let (_, revoked_device_id) = workspace.enrol(&root_keychain, &root_device_id, "revoked");

    workspace.revoke(&root_keychain, &root_device_id, &revoked_device_id);

    // The revoking device seeds and publishes offers in a single pass...
    let seeded = workspace.converge(&root_keychain, &root_device_id);
    assert_eq!(seeded.seeded_key_epoch, Some(2));
    assert_eq!(seeded.established_key_epoch, 2);
    assert_eq!(seeded.offers_published, 1);

    // ...and the remaining device picks the material up on its own next pass,
    // with nothing asked of the user in between.
    let converged = workspace.converge(&kept_keychain, &kept_device_id);
    assert_eq!(converged.accepted_key_epoch, Some(2));
    assert_eq!(converged.established_key_epoch, 2);
    assert_eq!(converged.outstanding_devices, 0);

    let kept_keyring = workspace.keyring(&kept_keychain);
    let root_keyring = workspace.keyring(&root_keychain);
    assert!(kept_keyring.holds(2));
    assert_eq!(
        kept_keyring.key_bytes(2),
        root_keyring.key_bytes(2),
        "both devices must land on the same epoch-2 material"
    );
}

#[test]
fn revoked_device_is_refused_the_regrant_and_never_reaches_the_new_epoch() {
    let (workspace, root_keychain, root_device_id) = Workspace::new("regrant-refuses-revoked");
    let (revoked_keychain, revoked_device_id) =
        workspace.enrol(&root_keychain, &root_device_id, "revoked");
    let epoch_one_material = workspace
        .keyring(&revoked_keychain)
        .material(1)
        .expect("revoked device held epoch 1");

    workspace.revoke(&root_keychain, &root_device_id, &revoked_device_id);
    workspace.converge(&root_keychain, &root_device_id);

    let identity = revoked_keychain
        .load_or_create_device_identity()
        .expect("revoked identity");
    let error = workspace
        .control_plane
        .get_workspace_key_regrant_work(WorkspaceKeyRegrantWorkRequest {
            device_proof: grants::device_authorization_proof(
                &identity,
                &workspace.workspace_id,
                &revoked_device_id,
                "list-workspace-key-regrant-work",
                &key_regrant_work_proof_subject(&workspace.workspace_id),
            )
            .expect("revoked device can still sign"),
            device_id: revoked_device_id.clone(),
            workspace_id: workspace.workspace_id.clone(),
        })
        .expect_err("a revoked device must not be served regrant work");
    assert!(matches!(
        error,
        ControlPlaneError::Rejected {
            code: RejectionCode::DeviceNotTrusted,
            ..
        }
    ));

    // Its keyring is frozen at the epoch it held when it was revoked, so
    // anything sealed at epoch 2 is out of reach.
    let revoked_keyring = workspace.keyring(&revoked_keychain);
    assert!(!revoked_keyring.holds(2));
    assert_eq!(revoked_keyring.highest_key_epoch(), Some(1));
    let root_keyring = workspace.keyring(&root_keychain);
    assert_ne!(
        root_keyring.key_bytes(2),
        Some(epoch_one_material.key_bytes.as_slice()),
        "the new epoch must not be derivable from the epoch the revoked device holds"
    );
}

#[test]
fn old_epoch_material_stays_readable_after_the_rotation() {
    let (workspace, root_keychain, root_device_id) = Workspace::new("regrant-keeps-history");
    let (kept_keychain, kept_device_id) = workspace.enrol(&root_keychain, &root_device_id, "kept");
    let (_, revoked_device_id) = workspace.enrol(&root_keychain, &root_device_id, "revoked");
    let epoch_one = workspace
        .keyring(&kept_keychain)
        .material(1)
        .expect("epoch 1 held before rotation");

    workspace.revoke(&root_keychain, &root_device_id, &revoked_device_id);
    workspace.converge(&root_keychain, &root_device_id);
    workspace.converge(&kept_keychain, &kept_device_id);

    let keyring = workspace.keyring(&kept_keychain);
    assert_eq!(
        keyring.material(1),
        Some(epoch_one),
        "prior epoch material is custody, not history: objects sealed under it are still referenced"
    );
    assert_eq!(keyring.established_key_epoch(), 2, "new writes use epoch 2");
}

#[test]
fn a_device_offline_across_the_rotation_converges_when_it_returns() {
    let (workspace, root_keychain, root_device_id) = Workspace::new("regrant-offline-one");
    let (absent_keychain, absent_device_id) =
        workspace.enrol(&root_keychain, &root_device_id, "absent");
    let (_, revoked_device_id) = workspace.enrol(&root_keychain, &root_device_id, "revoked");

    workspace.revoke(&root_keychain, &root_device_id, &revoked_device_id);
    // The absent device runs no pass at all while the rotation happens.
    workspace.converge(&root_keychain, &root_device_id);
    assert_eq!(
        workspace.keyring(&absent_keychain).highest_key_epoch(),
        Some(1)
    );

    let converged = workspace.converge(&absent_keychain, &absent_device_id);

    assert_eq!(converged.accepted_key_epoch, Some(2));
    assert_eq!(converged.established_key_epoch, 2);
    assert!(workspace.keyring(&absent_keychain).holds(1));
}

#[test]
fn a_device_offline_across_two_rotations_receives_every_epoch_it_missed() {
    let (workspace, root_keychain, root_device_id) = Workspace::new("regrant-offline-two");
    let (absent_keychain, absent_device_id) =
        workspace.enrol(&root_keychain, &root_device_id, "absent");
    let (_, first_revoked) = workspace.enrol(&root_keychain, &root_device_id, "revoked-one");
    let (_, second_revoked) = workspace.enrol(&root_keychain, &root_device_id, "revoked-two");

    workspace.revoke(&root_keychain, &root_device_id, &first_revoked);
    workspace.converge(&root_keychain, &root_device_id);
    workspace.revoke(&root_keychain, &root_device_id, &second_revoked);
    workspace.converge(&root_keychain, &root_device_id);

    let converged = workspace.converge(&absent_keychain, &absent_device_id);

    assert_eq!(converged.established_key_epoch, 3);
    let absent_keyring = workspace.keyring(&absent_keychain);
    let root_keyring = workspace.keyring(&root_keychain);
    // Every epoch, not only the newest: objects written between the two
    // rotations are sealed under epoch 2 and would otherwise be unreadable.
    for key_epoch in [1, 2, 3] {
        assert_eq!(
            absent_keyring.key_bytes(key_epoch),
            root_keyring.key_bytes(key_epoch),
            "epoch {key_epoch} must be delivered"
        );
    }
}

#[test]
fn rotation_completes_when_the_revoking_device_never_comes_back() {
    let (workspace, root_keychain, root_device_id) = Workspace::new("regrant-absent-revoker");
    let (holder_keychain, holder_device_id) =
        workspace.enrol(&root_keychain, &root_device_id, "holder");
    let (straggler_keychain, straggler_device_id) =
        workspace.enrol(&root_keychain, &root_device_id, "straggler");
    let (_, revoked_device_id) = workspace.enrol(&root_keychain, &root_device_id, "revoked");

    workspace.revoke(&root_keychain, &root_device_id, &revoked_device_id);
    // The revoking device goes offline immediately: it never runs a pass, so it
    // neither seeds nor offers. Another holder has to carry the rotation.
    let seeded = workspace.converge(&holder_keychain, &holder_device_id);
    assert_eq!(seeded.seeded_key_epoch, Some(2));
    assert_eq!(seeded.established_key_epoch, 2);

    let converged = workspace.converge(&straggler_keychain, &straggler_device_id);
    assert_eq!(converged.accepted_key_epoch, Some(2));
    assert_eq!(
        workspace.keyring(&straggler_keychain).key_bytes(2),
        workspace.keyring(&holder_keychain).key_bytes(2)
    );
}

#[test]
fn only_one_device_can_seed_an_epoch_and_the_loser_drops_its_candidate() {
    let (workspace, root_keychain, root_device_id) = Workspace::new("regrant-seed-race");
    let (rival_keychain, rival_device_id) =
        workspace.enrol(&root_keychain, &root_device_id, "rival");
    let (_, revoked_device_id) = workspace.enrol(&root_keychain, &root_device_id, "revoked");

    workspace.revoke(&root_keychain, &root_device_id, &revoked_device_id);
    let winner = workspace.converge(&root_keychain, &root_device_id);
    // The rival minted its own epoch-2 candidate before losing the claim; the
    // pass must discard it rather than keep a second key for one epoch.
    let loser = workspace.converge(&rival_keychain, &rival_device_id);

    assert_eq!(winner.seeded_key_epoch, Some(2));
    assert_eq!(loser.seeded_key_epoch, None);
    assert_eq!(
        workspace.keyring(&rival_keychain).key_bytes(2),
        workspace.keyring(&root_keychain).key_bytes(2),
        "the loser adopts the winner's material, never its own"
    );
}

#[test]
fn convergence_is_idempotent_once_the_workspace_has_settled() {
    let (workspace, root_keychain, root_device_id) = Workspace::new("regrant-idempotent");
    let (kept_keychain, kept_device_id) = workspace.enrol(&root_keychain, &root_device_id, "kept");
    let (_, revoked_device_id) = workspace.enrol(&root_keychain, &root_device_id, "revoked");

    workspace.revoke(&root_keychain, &root_device_id, &revoked_device_id);
    workspace.converge(&root_keychain, &root_device_id);
    workspace.converge(&kept_keychain, &kept_device_id);

    let settled = workspace.converge(&kept_keychain, &kept_device_id);
    assert_eq!(settled.accepted_key_epoch, None);
    assert_eq!(settled.seeded_key_epoch, None);
    assert_eq!(settled.offers_published, 0);
    assert_eq!(settled.outstanding_devices, 0);
    assert_eq!(settled.established_key_epoch, 2);
}
