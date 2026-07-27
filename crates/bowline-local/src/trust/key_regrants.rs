//! Unattended convergence onto the workspace key epoch.
//!
//! A revocation names a new epoch; this module is how every remaining device
//! ends up holding it without the user being asked anything. One pass is
//! idempotent and safe to run on every daemon tick:
//!
//! 1. accept any sealed material waiting for this device,
//! 2. seed the pending epoch if nobody has claimed it yet,
//! 3. seal what this device holds for every device that still owes it,
//! 4. adopt the epoch the workspace says it is writing at.
//!
//! Liveness never depends on one device staying online. Seeding publishes
//! offers for every outstanding recipient in the same call, so the seeder may
//! disappear immediately afterwards; and any device that later holds the epoch
//! can serve a straggler that was still offline when the offers were made.

use bowline_control_plane::{
    ControlPlaneClient, ControlPlaneError, RejectionCode, WorkspaceKeyAcceptanceInput,
    WorkspaceKeyPublicationInput, WorkspaceKeyRegrantOffer, WorkspaceKeyRegrantRecipient,
    WorkspaceKeyRegrantState, WorkspaceKeyRegrantWork, WorkspaceKeyRegrantWorkRequest,
    device_public_key_proof_subject, key_regrant_accept_proof_subject,
    key_regrant_offer_proof_subject, key_regrant_work_proof_subject,
};
use bowline_core::ids::{DeviceId, WorkspaceId};

use crate::device_keys::{DeviceIdentity, DeviceKeyStore, WorkspaceKeyMaterial, WorkspaceKeyring};

use super::{
    TrustError,
    grants::{
        self, DeviceAuthorizationProofCheck, GrantRecipient, GrantScope, GrantSealSource,
        SealedGrantCheck,
    },
};

const LIST_KEY_REGRANT_WORK_ACTION: &str = "list-workspace-key-regrant-work";
const SEED_KEY_EPOCH_ACTION: &str = "seed-workspace-key-epoch";
const OFFER_KEY_REGRANT_ACTION: &str = "offer-workspace-key-regrant";
const ACCEPT_KEY_REGRANT_ACTION: &str = "accept-workspace-key-regrant";
const ATTEST_DEVICE_PUBLIC_KEY_ACTION: &str = "attest-device-public-key";

/// What one convergence pass changed. Reported rather than logged so callers
/// (the daemon loop, the CLI, tests) can decide what is worth surfacing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KeyEpochConvergence {
    pub established_key_epoch: u32,
    pub accepted_key_epoch: Option<u32>,
    pub seeded_key_epoch: Option<u32>,
    pub offers_published: usize,
    pub outstanding_devices: usize,
}

/// Signs this device's own age public key so any holder of the workspace key
/// can seal for it later without trusting the control plane's copy of that key.
pub fn device_public_key_attestation(
    identity: &DeviceIdentity,
    workspace_id: &WorkspaceId,
    device_id: &DeviceId,
) -> Result<String, TrustError> {
    grants::device_authorization_proof(
        identity,
        workspace_id,
        device_id,
        ATTEST_DEVICE_PUBLIC_KEY_ACTION,
        &device_public_key_proof_subject(identity.public_key.as_str()),
    )
    .map_err(TrustError::Grant)
}

pub fn converge_workspace_key_epoch<C, K>(
    control_plane: &C,
    key_store: &K,
    workspace_id: &WorkspaceId,
    device_id: &DeviceId,
) -> Result<KeyEpochConvergence, TrustError>
where
    C: ControlPlaneClient + ?Sized,
    K: DeviceKeyStore + ?Sized,
{
    let identity = key_store.load_or_create_device_identity()?;
    let work = control_plane.get_workspace_key_regrant_work(WorkspaceKeyRegrantWorkRequest {
        device_proof: grants::device_authorization_proof(
            &identity,
            workspace_id,
            device_id,
            LIST_KEY_REGRANT_WORK_ACTION,
            &key_regrant_work_proof_subject(workspace_id),
        )
        .map_err(TrustError::Grant)?,
        device_id: device_id.clone(),
        workspace_id: workspace_id.clone(),
    })?;

    let mut converged = KeyEpochConvergence::default();
    let work = accept_own_regrant(
        control_plane,
        key_store,
        &identity,
        device_id,
        work,
        &mut converged,
    )?;
    let work = seed_pending_epoch(
        control_plane,
        key_store,
        &identity,
        device_id,
        work,
        &mut converged,
    )?;
    let work = serve_outstanding(
        control_plane,
        key_store,
        &identity,
        device_id,
        work,
        &mut converged,
    )?;

    converged.outstanding_devices = work
        .outstanding
        .iter()
        .filter(|recipient| recipient.state.is_outstanding())
        .count();
    converged.established_key_epoch =
        adopt_established_epoch(key_store, &work.workspace_id, work.current_key_epoch)?;
    Ok(converged)
}

fn accept_own_regrant<C, K>(
    control_plane: &C,
    key_store: &K,
    identity: &DeviceIdentity,
    device_id: &DeviceId,
    work: WorkspaceKeyRegrantWork,
    converged: &mut KeyEpochConvergence,
) -> Result<WorkspaceKeyRegrantWork, TrustError>
where
    C: ControlPlaneClient + ?Sized,
    K: DeviceKeyStore + ?Sized,
{
    let Some(own) = work.own_regrant.as_ref() else {
        return Ok(work);
    };
    let (Some(ciphertext), Some(sealed_by_device_id)) =
        (own.ciphertext.as_ref(), own.sealed_by_device_id.as_ref())
    else {
        return Ok(work);
    };
    if own.state != WorkspaceKeyRegrantState::Offered {
        return Ok(work);
    }

    // The sealer's verifier is read off the authenticated trust list, never out
    // of the payload: a verifier the payload itself supplied would let whoever
    // wrote it install itself as a trusted sealer.
    let published_sealer_proof_verifier = control_plane
        .list_device_trust(&work.workspace_id)?
        .authorized_devices
        .into_iter()
        .find(|device| &device.device_id == sealed_by_device_id)
        .and_then(|device| device.device_authorization_proof_verifier)
        .ok_or(TrustError::Grant(grants::GrantError::AuthorizerMismatch))?;
    let keys = grants::open_approver_sealed_grant(SealedGrantCheck {
        identity,
        ciphertext,
        expected_workspace_id: &work.workspace_id,
        expected_scope: GrantScope::KeyRegrant,
        expected_recipient_device_id: device_id,
        expected_recipient_fingerprint: identity.fingerprint.as_str(),
        expected_key_epoch: own.key_epoch,
        sealer_device_id: sealed_by_device_id,
        published_sealer_proof_verifier: &published_sealer_proof_verifier,
    })
    .map_err(TrustError::Grant)?;

    // Persist before confirming: a device recorded as holding an epoch it
    // cannot open is a workspace that never converges.
    let mut keyring = load_or_empty_keyring(key_store, &work.workspace_id)?;
    for key in &keys {
        keyring.insert(key.clone());
    }
    key_store.store_workspace_keyring(keyring)?;

    let accepted = control_plane.accept_workspace_key_regrant(WorkspaceKeyAcceptanceInput {
        device_proof: grants::device_authorization_proof(
            identity,
            &work.workspace_id,
            device_id,
            ACCEPT_KEY_REGRANT_ACTION,
            &key_regrant_accept_proof_subject(own.key_epoch),
        )
        .map_err(TrustError::Grant)?,
        device_id: device_id.clone(),
        grant_acceptance_proof: grants::grant_acceptance_proof(
            &keys,
            &GrantScope::KeyRegrant,
            device_id,
        ),
        key_epoch: own.key_epoch,
        workspace_id: work.workspace_id.clone(),
    })?;
    converged.accepted_key_epoch = Some(own.key_epoch);
    Ok(accepted)
}

fn seed_pending_epoch<C, K>(
    control_plane: &C,
    key_store: &K,
    identity: &DeviceIdentity,
    device_id: &DeviceId,
    work: WorkspaceKeyRegrantWork,
    converged: &mut KeyEpochConvergence,
) -> Result<WorkspaceKeyRegrantWork, TrustError>
where
    C: ControlPlaneClient + ?Sized,
    K: DeviceKeyStore + ?Sized,
{
    let Some(pending_key_epoch) = work.pending_key_epoch else {
        return Ok(work);
    };
    if work.pending_seeded_by_device_id.is_some() {
        return Ok(work);
    }
    let mut keyring = load_or_empty_keyring(key_store, &work.workspace_id)?;
    // Only a device that already holds the workspace can mint its successor:
    // the payload it publishes has to carry the older epochs too, or every
    // recipient loses reach of the objects sealed under them.
    if !keyring.holds(work.current_key_epoch) {
        return Ok(work);
    }
    let candidate = WorkspaceKeyMaterial::generate(work.workspace_id.clone(), pending_key_epoch)?;
    keyring.insert(candidate.clone());
    // Stored before the claim so a crash between the two leaves the material
    // recoverable; if the claim is lost the candidate is dropped below.
    key_store.store_workspace_keyring(keyring.clone())?;

    let offers = seal_offers(
        identity,
        device_id,
        &keyring,
        pending_key_epoch,
        &work.outstanding,
    )?;
    let publication = WorkspaceKeyPublicationInput {
        device_proof: grants::device_authorization_proof(
            identity,
            &work.workspace_id,
            device_id,
            SEED_KEY_EPOCH_ACTION,
            &key_regrant_offer_proof_subject(pending_key_epoch, &offers),
        )
        .map_err(TrustError::Grant)?,
        device_id: device_id.clone(),
        key_epoch: pending_key_epoch,
        offers,
        workspace_id: work.workspace_id.clone(),
    };
    let offers_published = publication.offers.len();
    match control_plane.seed_workspace_key_epoch(publication) {
        Ok(seeded) => {
            converged.seeded_key_epoch = Some(pending_key_epoch);
            converged.offers_published += offers_published;
            Ok(seeded)
        }
        // Another device won the claim. Its material is the workspace's, so the
        // candidate minted here is discarded rather than kept as a second key
        // for the same epoch.
        Err(ControlPlaneError::Conflict { .. })
        | Err(ControlPlaneError::Rejected {
            code: RejectionCode::Conflict,
            ..
        }) => {
            keyring.remove(pending_key_epoch);
            key_store.store_workspace_keyring(keyring)?;
            Ok(work)
        }
        Err(error) => Err(error.into()),
    }
}

fn serve_outstanding<C, K>(
    control_plane: &C,
    key_store: &K,
    identity: &DeviceIdentity,
    device_id: &DeviceId,
    work: WorkspaceKeyRegrantWork,
    converged: &mut KeyEpochConvergence,
) -> Result<WorkspaceKeyRegrantWork, TrustError>
where
    C: ControlPlaneClient + ?Sized,
    K: DeviceKeyStore + ?Sized,
{
    let keyring = load_or_empty_keyring(key_store, &work.workspace_id)?;
    if !keyring.holds(work.current_key_epoch) {
        return Ok(work);
    }
    let offers = seal_offers(
        identity,
        device_id,
        &keyring,
        work.current_key_epoch,
        &work.outstanding,
    )?;
    if offers.is_empty() {
        return Ok(work);
    }
    let offers_published = offers.len();
    let served = control_plane.offer_workspace_key_regrants(WorkspaceKeyPublicationInput {
        device_proof: grants::device_authorization_proof(
            identity,
            &work.workspace_id,
            device_id,
            OFFER_KEY_REGRANT_ACTION,
            &key_regrant_offer_proof_subject(work.current_key_epoch, &offers),
        )
        .map_err(TrustError::Grant)?,
        device_id: device_id.clone(),
        key_epoch: work.current_key_epoch,
        offers,
        workspace_id: work.workspace_id.clone(),
    })?;
    converged.offers_published += offers_published;
    Ok(served)
}

/// Seals the whole keyring for every recipient that owes `key_epoch` and has no
/// usable offer yet.
///
/// A recipient whose published age key is not signed under its published proof
/// verifier is skipped, not served. That check is what stops the control plane
/// from substituting a key it holds the secret for and harvesting the workspace
/// key: an unsigned or mis-signed recipient simply never gets material, and the
/// obligation stays visibly outstanding.
fn seal_offers(
    identity: &DeviceIdentity,
    device_id: &DeviceId,
    keyring: &WorkspaceKeyring,
    key_epoch: u32,
    outstanding: &[WorkspaceKeyRegrantRecipient],
) -> Result<Vec<WorkspaceKeyRegrantOffer>, TrustError> {
    let keys = keyring
        .materials()
        .into_iter()
        .filter(|key| key.key_epoch <= key_epoch)
        .collect::<Vec<_>>();
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    let mut offers = Vec::new();
    for recipient in outstanding {
        if recipient.device_id == *device_id
            || recipient.key_epoch != key_epoch
            || recipient.state != WorkspaceKeyRegrantState::Pending
        {
            continue;
        }
        if !recipient_key_is_attested(recipient, &keyring.workspace_id) {
            continue;
        }
        let ciphertext = grants::encrypt_workspace_keys_for_regrant(
            &keys,
            GrantRecipient {
                device_id: &recipient.device_id,
                device_fingerprint: &recipient.device_fingerprint,
                device_public_key: &recipient.device_public_key,
            },
            GrantSealSource::Approver {
                identity,
                device_id: device_id.clone(),
            },
        )
        .map_err(TrustError::Grant)?;
        offers.push(WorkspaceKeyRegrantOffer {
            acceptance_proof_verifier: grants::grant_acceptance_proof_verifier(
                &grants::grant_acceptance_proof(
                    &keys,
                    &GrantScope::KeyRegrant,
                    &recipient.device_id,
                ),
            ),
            ciphertext,
            recipient_device_id: recipient.device_id.clone(),
        });
    }
    Ok(offers)
}

fn recipient_key_is_attested(
    recipient: &WorkspaceKeyRegrantRecipient,
    workspace_id: &WorkspaceId,
) -> bool {
    let Some(proof_verifier) = recipient.device_authorization_proof_verifier.as_deref() else {
        return false;
    };
    grants::verify_device_authorization_proof(DeviceAuthorizationProofCheck {
        proof_verifier,
        proof: &recipient.device_public_key_proof,
        workspace_id,
        device_id: &recipient.device_id,
        action: ATTEST_DEVICE_PUBLIC_KEY_ACTION,
        subject: &device_public_key_proof_subject(&recipient.device_public_key),
    })
    .is_ok()
}

/// The write path's epoch. It is the workspace's answer, adopted only when this
/// device actually holds that material, so a device mid-convergence keeps
/// sealing under the epoch it can still prove rather than an epoch it has only
/// been told about.
fn adopt_established_epoch<K>(
    key_store: &K,
    workspace_id: &WorkspaceId,
    current_key_epoch: u32,
) -> Result<u32, TrustError>
where
    K: DeviceKeyStore + ?Sized,
{
    let mut keyring = load_or_empty_keyring(key_store, workspace_id)?;
    if keyring.established_key_epoch() == current_key_epoch
        || !keyring.set_established_key_epoch(current_key_epoch)
    {
        return Ok(keyring.established_key_epoch());
    }
    let established = keyring.established_key_epoch();
    key_store.store_workspace_keyring(keyring)?;
    Ok(established)
}

fn load_or_empty_keyring<K>(
    key_store: &K,
    workspace_id: &WorkspaceId,
) -> Result<WorkspaceKeyring, TrustError>
where
    K: DeviceKeyStore + ?Sized,
{
    Ok(key_store
        .load_workspace_keyring(workspace_id)?
        .unwrap_or_else(|| WorkspaceKeyring::empty(workspace_id.clone())))
}
