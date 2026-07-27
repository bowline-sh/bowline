use std::{error::Error, fmt, str::FromStr};

pub use super::device_proofs::{
    DeviceAuthorizationProofCheck, device_authorization_proof, device_authorization_proof_verifier,
    device_matching_code, verify_device_authorization_proof,
};
pub use super::recovery_envelopes::{
    DecryptedRecoveryEnvelope, decrypt_recovery_envelope, encrypted_recovery_envelope,
    recovery_fingerprint, recovery_proof, recovery_proof_verifier,
    recovery_proof_verifier_from_proof, redacted_words_debug,
};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
pub use bowline_control_plane::{
    device_authorization_message, device_request_proof_subject, device_revocation_proof_subject,
    recovery_envelope_payload_proof_subject_parts as recovery_envelope_payload_proof_subject,
    recovery_envelope_proof_subject,
};
use bowline_core::ids::{DeviceApprovalRequestId, DeviceId, WorkspaceId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::device_keys::{DeviceIdentity, WorkspaceKeyMaterial};

/// Action label for the approver signature over a sealed device grant. Kept
/// distinct from `approve-device-request` so a proof captured from the
/// control-plane approval call cannot be replayed as a grant seal.
const GRANT_SEAL_ACTION: &str = "seal-device-grant";

#[derive(Debug)]
pub enum GrantError {
    Age(String),
    Base64(base64::DecodeError),
    Json(serde_json::Error),
    SigningKeyDerivation,
    WorkspaceMismatch,
    AuthorizerMismatch,
    UnsealedGrant,
    MalformedProofMaterial(&'static str),
    GrantSealInvalid,
    RequesterKeyMismatch,
    KeyEpochMismatch,
    EmptyKeyring,
    DuplicateKeyEpoch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GrantPayload {
    workspace_id: WorkspaceId,
    scope: GrantScope,
    recipient_device_id: DeviceId,
    recipient_device_fingerprint: String,
    recipient_device_public_key: String,
    authorization: GrantAuthorization,
    /// The newest epoch carried. Redundant with `workspace_keys` and checked
    /// against it on open, so a payload cannot claim one epoch and deliver
    /// another.
    key_epoch: u32,
    /// Every epoch the sealer is handing over, ascending. A device that was
    /// offline across several rotations needs more than the newest epoch: the
    /// objects it has yet to open were sealed under the ones it missed.
    workspace_keys: Vec<WorkspaceKeyMaterial>,
}

/// Why this payload exists. The two arms are sealed and opened by different
/// code paths and must not be interchangeable: an enrollment payload is bound
/// to the request a human compared a matching code for, while a re-grant is
/// addressed to a device that is already trusted and has no request at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum GrantScope {
    #[serde(rename_all = "camelCase")]
    DeviceEnrollment {
        request_id: DeviceApprovalRequestId,
    },
    KeyRegrant,
}

impl GrantScope {
    fn tag(&self) -> &'static str {
        match self {
            Self::DeviceEnrollment { .. } => "device-enrollment",
            Self::KeyRegrant => "key-regrant",
        }
    }

    fn request_id(&self) -> &str {
        match self {
            Self::DeviceEnrollment { request_id } => request_id.as_str(),
            Self::KeyRegrant => "",
        }
    }
}

/// How a grant's key material was authorized. The two arms have genuinely
/// different trust roots, so they are not an `Option`: an approver-sealed grant
/// carries a signature the requester verifies, while a recovery-sealed grant is
/// minted by the recovering device itself out of an envelope it already opened
/// and is never consumed by `open_approver_sealed_grant`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum GrantAuthorization {
    #[serde(rename_all = "camelCase")]
    ApproverSealed {
        authorizing_device: DeviceGrantAuthorizer,
        seal_proof: String,
    },
    #[serde(rename_all = "camelCase")]
    RecoverySelfSealed {
        device_proof_verifiers: Vec<DeviceGrantAuthorizer>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceGrantAuthorizer {
    pub device_id: DeviceId,
    pub device_authorization_proof_verifier: String,
}

/// Who is sealing a grant, and with what authority.
pub enum GrantSealSource<'a> {
    Approver {
        identity: &'a DeviceIdentity,
        device_id: DeviceId,
    },
    RecoveryEnvelope {
        device_proof_verifiers: Vec<DeviceGrantAuthorizer>,
    },
}

/// The device a payload is addressed to, as the sealer knows it.
pub struct GrantRecipient<'a> {
    pub device_id: &'a DeviceId,
    pub device_fingerprint: &'a str,
    pub device_public_key: &'a str,
}

/// Everything the recipient needs to decide whether a sealed payload is an
/// authenticated key transfer from a device it can name, rather than bytes the
/// control plane chose.
pub struct SealedGrantCheck<'a> {
    pub identity: &'a DeviceIdentity,
    pub ciphertext: &'a str,
    pub expected_workspace_id: &'a WorkspaceId,
    pub expected_scope: GrantScope,
    pub expected_recipient_device_id: &'a DeviceId,
    pub expected_recipient_fingerprint: &'a str,
    pub expected_key_epoch: u32,
    pub sealer_device_id: &'a DeviceId,
    /// The sealer's proof verifier as published on the authenticated device
    /// trust list — never the copy carried inside the payload.
    pub published_sealer_proof_verifier: &'a str,
}

pub fn encrypt_workspace_keys_for_request(
    keys: &[WorkspaceKeyMaterial],
    request: &bowline_control_plane::DeviceRequest,
    seal: GrantSealSource<'_>,
) -> Result<String, GrantError> {
    encrypt_workspace_keys(
        keys,
        GrantScope::DeviceEnrollment {
            request_id: request.request_id.clone(),
        },
        GrantRecipient {
            device_id: &request.device_id,
            device_fingerprint: &request.device_fingerprint,
            device_public_key: &request.device_public_key,
        },
        seal,
    )
}

pub fn encrypt_workspace_keys_for_regrant(
    keys: &[WorkspaceKeyMaterial],
    recipient: GrantRecipient<'_>,
    seal: GrantSealSource<'_>,
) -> Result<String, GrantError> {
    encrypt_workspace_keys(keys, GrantScope::KeyRegrant, recipient, seal)
}

/// The one sealing path. Enrollment and re-grant differ only in scope, so they
/// share the payload shape, the seal subject, and the signature check rather
/// than growing a second, subtly different crypto path.
fn encrypt_workspace_keys(
    keys: &[WorkspaceKeyMaterial],
    scope: GrantScope,
    recipient: GrantRecipient<'_>,
    seal: GrantSealSource<'_>,
) -> Result<String, GrantError> {
    let keys = normalized_keys(keys)?;
    let workspace_id = keys
        .first()
        .map(|key| key.workspace_id.clone())
        .ok_or(GrantError::EmptyKeyring)?;
    let key_epoch = keys
        .last()
        .map(|key| key.key_epoch)
        .ok_or(GrantError::EmptyKeyring)?;
    let age_recipient = age::x25519::Recipient::from_str(recipient.device_public_key)
        .map_err(|error| GrantError::Age(error.to_string()))?;
    let seal_subject = grant_seal_subject(GrantSealFields {
        workspace_id: &workspace_id,
        scope: &scope,
        recipient_device_id: recipient.device_id,
        recipient_device_fingerprint: recipient.device_fingerprint,
        recipient_device_public_key: recipient.device_public_key,
        key_epoch,
        workspace_keys: &keys,
    });
    let authorization = match seal {
        GrantSealSource::Approver {
            identity,
            device_id,
        } => GrantAuthorization::ApproverSealed {
            seal_proof: device_authorization_proof(
                identity,
                &workspace_id,
                &device_id,
                GRANT_SEAL_ACTION,
                &seal_subject,
            )?,
            authorizing_device: DeviceGrantAuthorizer {
                device_id,
                device_authorization_proof_verifier: device_authorization_proof_verifier(identity)?,
            },
        },
        GrantSealSource::RecoveryEnvelope {
            device_proof_verifiers,
        } => GrantAuthorization::RecoverySelfSealed {
            device_proof_verifiers,
        },
    };
    let payload = GrantPayload {
        workspace_id,
        scope,
        recipient_device_id: recipient.device_id.clone(),
        recipient_device_fingerprint: recipient.device_fingerprint.to_string(),
        recipient_device_public_key: recipient.device_public_key.to_string(),
        authorization,
        key_epoch,
        workspace_keys: keys,
    };
    let plaintext = serde_json::to_vec(&payload)?;
    let ciphertext = age::encrypt(&age_recipient, &plaintext)
        .map_err(|error| GrantError::Age(error.to_string()))?;
    Ok(BASE64.encode(ciphertext))
}

/// Opens a sealed payload only when it is an authenticated key transfer: sealed
/// by the device the control plane names as sealer, signed under the verifier
/// that device published on the trust list, bound to this device's own public
/// key, and scoped to the operation the recipient thinks it is performing.
/// Anything else is refused before the key material is returned.
pub fn open_approver_sealed_grant(
    check: SealedGrantCheck<'_>,
) -> Result<Vec<WorkspaceKeyMaterial>, GrantError> {
    let ciphertext = BASE64.decode(check.ciphertext)?;
    let age_identity = check
        .identity
        .age_identity()
        .map_err(|error| GrantError::Age(error.to_string()))?;
    let plaintext = age::decrypt(&age_identity, &ciphertext)
        .map_err(|error| GrantError::Age(error.to_string()))?;
    let payload: GrantPayload = serde_json::from_slice(&plaintext)?;

    if payload.scope != check.expected_scope
        || payload.recipient_device_id.as_str() != check.expected_recipient_device_id.as_str()
        || payload.recipient_device_fingerprint != check.expected_recipient_fingerprint
        || &payload.workspace_id != check.expected_workspace_id
    {
        return Err(GrantError::WorkspaceMismatch);
    }
    if payload.recipient_device_public_key != check.identity.public_key.as_str() {
        return Err(GrantError::RequesterKeyMismatch);
    }
    let keys = normalized_keys(&payload.workspace_keys)?;
    if keys != payload.workspace_keys
        || keys
            .iter()
            .any(|key| &key.workspace_id != check.expected_workspace_id)
        || keys.last().map(|key| key.key_epoch) != Some(payload.key_epoch)
        || payload.key_epoch != check.expected_key_epoch
    {
        return Err(GrantError::KeyEpochMismatch);
    }

    let GrantAuthorization::ApproverSealed {
        authorizing_device,
        seal_proof,
    } = &payload.authorization
    else {
        return Err(GrantError::UnsealedGrant);
    };
    if &authorizing_device.device_id != check.sealer_device_id
        || authorizing_device.device_authorization_proof_verifier
            != check.published_sealer_proof_verifier
    {
        return Err(GrantError::AuthorizerMismatch);
    }

    let seal_subject = grant_seal_subject(GrantSealFields {
        workspace_id: &payload.workspace_id,
        scope: &payload.scope,
        recipient_device_id: &payload.recipient_device_id,
        recipient_device_fingerprint: &payload.recipient_device_fingerprint,
        recipient_device_public_key: &payload.recipient_device_public_key,
        key_epoch: payload.key_epoch,
        workspace_keys: &keys,
    });
    verify_device_authorization_proof(DeviceAuthorizationProofCheck {
        proof_verifier: check.published_sealer_proof_verifier,
        proof: seal_proof,
        workspace_id: &payload.workspace_id,
        device_id: &authorizing_device.device_id,
        action: GRANT_SEAL_ACTION,
        subject: &seal_subject,
    })?;

    Ok(keys)
}

/// Ascending by epoch with no duplicates. The order is hashed into the seal
/// subject, so it is normalized in one place rather than trusted from either
/// side of the wire.
fn normalized_keys(keys: &[WorkspaceKeyMaterial]) -> Result<Vec<WorkspaceKeyMaterial>, GrantError> {
    if keys.is_empty() {
        return Err(GrantError::EmptyKeyring);
    }
    let mut sorted = keys.to_vec();
    sorted.sort_by_key(|key| key.key_epoch);
    if sorted
        .windows(2)
        .any(|pair| pair[0].key_epoch == pair[1].key_epoch)
    {
        return Err(GrantError::DuplicateKeyEpoch);
    }
    Ok(sorted)
}

struct GrantSealFields<'a> {
    workspace_id: &'a WorkspaceId,
    scope: &'a GrantScope,
    recipient_device_id: &'a DeviceId,
    recipient_device_fingerprint: &'a str,
    recipient_device_public_key: &'a str,
    key_epoch: u32,
    workspace_keys: &'a [WorkspaceKeyMaterial],
}

/// Signing the sealed key material — not the age ciphertext — avoids the
/// circularity of putting a signature over a blob inside that same blob, and
/// binds strictly more: the scope, the epoch, and every key byte the recipient
/// will adopt.
fn grant_seal_subject(fields: GrantSealFields<'_>) -> String {
    let key_epoch = fields.key_epoch.to_string();
    let key_count = fields.workspace_keys.len().to_string();
    let epochs = fields
        .workspace_keys
        .iter()
        .map(|key| key.key_epoch.to_string())
        .collect::<Vec<_>>();
    let mut parts: Vec<&[u8]> = vec![
        b"bowline device grant seal v2",
        fields.workspace_id.as_str().as_bytes(),
        fields.scope.tag().as_bytes(),
        fields.scope.request_id().as_bytes(),
        fields.recipient_device_id.as_str().as_bytes(),
        fields.recipient_device_fingerprint.as_bytes(),
        fields.recipient_device_public_key.as_bytes(),
        key_epoch.as_bytes(),
        key_count.as_bytes(),
    ];
    for (epoch, key) in epochs.iter().zip(fields.workspace_keys) {
        parts.push(epoch.as_bytes());
        parts.push(key.key_bytes.as_slice());
    }
    sha256_proof_parts(&parts)
}

pub fn grant_acceptance_proof(
    keys: &[WorkspaceKeyMaterial],
    scope: &GrantScope,
    recipient_device_id: &DeviceId,
) -> String {
    let workspace_id = keys
        .first()
        .map(|key| key.workspace_id.as_str().to_string())
        .unwrap_or_default();
    let key_count = keys.len().to_string();
    let epochs = keys
        .iter()
        .map(|key| key.key_epoch.to_string())
        .collect::<Vec<_>>();
    let mut parts: Vec<&[u8]> = vec![
        b"bowline grant acceptance proof v2",
        workspace_id.as_bytes(),
        scope.tag().as_bytes(),
        scope.request_id().as_bytes(),
        recipient_device_id.as_str().as_bytes(),
        key_count.as_bytes(),
    ];
    for (epoch, key) in epochs.iter().zip(keys) {
        parts.push(epoch.as_bytes());
        parts.push(key.key_bytes.as_slice());
    }
    let hash = sha256_proof_parts(&parts);
    format!("gap_{}", &hash[..32])
}

pub fn grant_acceptance_proof_verifier(proof: &str) -> String {
    let hash = sha256_proof_fields(&["bowline grant acceptance proof verifier v1", proof]);
    format!("gapv_{}", &hash[..32])
}

pub(super) fn sha256_proof_fields(fields: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for field in fields {
        hasher.update((field.len() as u64).to_le_bytes());
        hasher.update(field.as_bytes());
    }
    let digest = hasher.finalize();
    format!("{digest:x}")
}

fn sha256_proof_parts(fields: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    for field in fields {
        hasher.update((field.len() as u64).to_le_bytes());
        hasher.update(field);
    }
    let digest = hasher.finalize();
    format!("{digest:x}")
}

impl fmt::Display for GrantError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Age(error) => write!(formatter, "grant encryption failed: {error}"),
            Self::Base64(error) => write!(formatter, "grant ciphertext is malformed: {error}"),
            Self::Json(error) => write!(formatter, "grant payload is malformed: {error}"),
            Self::SigningKeyDerivation => {
                write!(formatter, "device signing key derivation failed")
            }
            Self::WorkspaceMismatch => {
                write!(formatter, "grant does not match this workspace or device")
            }
            Self::AuthorizerMismatch => {
                write!(
                    formatter,
                    "grant authorizer does not match the approved device"
                )
            }
            Self::UnsealedGrant => {
                write!(
                    formatter,
                    "grant carries no approver seal and cannot be trusted"
                )
            }
            Self::MalformedProofMaterial(field) => {
                write!(formatter, "device authorization {field} is malformed")
            }
            Self::GrantSealInvalid => {
                write!(
                    formatter,
                    "grant seal was not signed by the approving device"
                )
            }
            Self::RequesterKeyMismatch => {
                write!(
                    formatter,
                    "grant was sealed to a different device public key"
                )
            }
            Self::KeyEpochMismatch => {
                write!(
                    formatter,
                    "grant key epoch does not match the sealed payload"
                )
            }
            Self::EmptyKeyring => {
                write!(formatter, "grant carries no workspace key material")
            }
            Self::DuplicateKeyEpoch => {
                write!(formatter, "grant carries two keys for the same epoch")
            }
        }
    }
}

impl Error for GrantError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Base64(error) => Some(error),
            Self::Json(error) => Some(error),
            _ => None,
        }
    }
}

impl From<base64::DecodeError> for GrantError {
    fn from(error: base64::DecodeError) -> Self {
        Self::Base64(error)
    }
}

impl From<serde_json::Error> for GrantError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use bowline_control_plane::{DeviceRequest, DeviceRequestState};
    use bowline_core::ids::{DeviceApprovalRequestId, DeviceId, WorkspaceId};
    use serde::Deserialize;
    use sha2::Digest;

    use super::{
        GrantError, GrantRecipient, GrantScope, GrantSealSource, SealedGrantCheck,
        device_authorization_proof_verifier, encrypt_workspace_keys_for_regrant,
        encrypt_workspace_keys_for_request, open_approver_sealed_grant,
    };
    use crate::device_keys::{DeviceIdentity, WorkspaceKeyMaterial};

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct FixtureFile {
        message_vectors: Vec<MessageVector>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct MessageVector {
        fields: Vec<String>,
        name: String,
        sha256_hex: String,
    }

    fn workspace_id() -> WorkspaceId {
        WorkspaceId::new("workspace-1")
    }

    fn key(key_epoch: u32, fill: u8) -> WorkspaceKeyMaterial {
        WorkspaceKeyMaterial {
            workspace_id: workspace_id(),
            key_epoch,
            key_bytes: vec![fill; 32],
        }
    }

    fn request_for(identity: &DeviceIdentity) -> DeviceRequest {
        DeviceRequest {
            request_id: DeviceApprovalRequestId::new("request-1"),
            workspace_id: workspace_id(),
            device_id: DeviceId::new("device-2"),
            device_name: "linux".to_string(),
            platform: "linux".to_string(),
            device_public_key: identity.public_key.as_str().to_string(),
            device_public_key_proof: "dapp_p256_v1_attestation".to_string(),
            device_fingerprint: identity.fingerprint.as_str().to_string(),
            device_authorization_proof_verifier: device_authorization_proof_verifier(identity)
                .expect("verifier"),
            matching_code: "bowline-abc123".to_string(),
            account_id: None,
            host: None,
            root: None,
            runtime: None,
            setup_receipts_digest: None,
            requested_at: bowline_control_plane::ControlPlaneTimestamp { tick: 1 },
            expires_at: bowline_control_plane::ControlPlaneTimestamp { tick: 2 },
            state: DeviceRequestState::Pending,
        }
    }

    fn enrollment_check<'a>(
        identity: &'a DeviceIdentity,
        ciphertext: &'a str,
        request: &'a DeviceRequest,
        sealer_device_id: &'a DeviceId,
        published_sealer_proof_verifier: &'a str,
        expected_key_epoch: u32,
    ) -> SealedGrantCheck<'a> {
        SealedGrantCheck {
            identity,
            ciphertext,
            expected_workspace_id: &request.workspace_id,
            expected_scope: GrantScope::DeviceEnrollment {
                request_id: request.request_id.clone(),
            },
            expected_recipient_device_id: &request.device_id,
            expected_recipient_fingerprint: &request.device_fingerprint,
            expected_key_epoch,
            sealer_device_id,
            published_sealer_proof_verifier,
        }
    }

    #[test]
    fn correct_device_key_decrypts_grant() {
        let identity = DeviceIdentity::generate();
        let request = request_for(&identity);
        let keys = vec![key(1, 9)];
        let approver = DeviceIdentity::generate();
        let approver_verifier =
            device_authorization_proof_verifier(&approver).expect("approver verifier");
        let approver_device_id = DeviceId::new("device-1");
        let ciphertext = encrypt_workspace_keys_for_request(
            &keys,
            &request,
            GrantSealSource::Approver {
                identity: &approver,
                device_id: approver_device_id.clone(),
            },
        )
        .expect("encrypt");

        let opened = open_approver_sealed_grant(enrollment_check(
            &identity,
            &ciphertext,
            &request,
            &approver_device_id,
            &approver_verifier,
            1,
        ))
        .expect("open");
        assert_eq!(opened, keys);

        let other_verifier =
            device_authorization_proof_verifier(&DeviceIdentity::generate()).expect("other");
        let forged = open_approver_sealed_grant(enrollment_check(
            &identity,
            &ciphertext,
            &request,
            &approver_device_id,
            &other_verifier,
            1,
        ))
        .expect_err("a grant sealed under another key is refused");
        assert!(matches!(forged, GrantError::AuthorizerMismatch));
    }

    #[test]
    fn wrong_device_key_fails_to_decrypt_grant() {
        let identity = DeviceIdentity::generate();
        let wrong_identity = DeviceIdentity::generate();
        let request = request_for(&identity);
        let approver_device_id = DeviceId::new("device-1");
        let ciphertext = encrypt_workspace_keys_for_request(
            &[key(1, 9)],
            &request,
            GrantSealSource::Approver {
                identity: &DeviceIdentity::generate(),
                device_id: approver_device_id.clone(),
            },
        )
        .expect("encrypt");

        assert!(
            open_approver_sealed_grant(enrollment_check(
                &wrong_identity,
                &ciphertext,
                &request,
                &approver_device_id,
                "dapv_p256_v1_unused",
                1,
            ))
            .is_err()
        );
    }

    #[test]
    fn a_regrant_carries_every_epoch_the_sealer_holds() {
        let recipient = DeviceIdentity::generate();
        let sealer = DeviceIdentity::generate();
        let sealer_device_id = DeviceId::new("device-sealer");
        let recipient_device_id = DeviceId::new("device-recipient");
        let keys = vec![key(1, 1), key(2, 2), key(3, 3)];
        let ciphertext = encrypt_workspace_keys_for_regrant(
            &keys,
            GrantRecipient {
                device_id: &recipient_device_id,
                device_fingerprint: recipient.fingerprint.as_str(),
                device_public_key: recipient.public_key.as_str(),
            },
            GrantSealSource::Approver {
                identity: &sealer,
                device_id: sealer_device_id.clone(),
            },
        )
        .expect("encrypt regrant");

        let opened = open_approver_sealed_grant(SealedGrantCheck {
            identity: &recipient,
            ciphertext: &ciphertext,
            expected_workspace_id: &workspace_id(),
            expected_scope: GrantScope::KeyRegrant,
            expected_recipient_device_id: &recipient_device_id,
            expected_recipient_fingerprint: recipient.fingerprint.as_str(),
            expected_key_epoch: 3,
            sealer_device_id: &sealer_device_id,
            published_sealer_proof_verifier: &device_authorization_proof_verifier(&sealer)
                .expect("sealer verifier"),
        })
        .expect("open regrant");

        assert_eq!(opened, keys);
    }

    /// An enrollment payload replayed as a re-grant (or the reverse) would let a
    /// grant issued for one trust decision satisfy a different one.
    #[test]
    fn a_regrant_payload_is_not_accepted_as_an_enrollment_grant() {
        let recipient = DeviceIdentity::generate();
        let sealer = DeviceIdentity::generate();
        let sealer_device_id = DeviceId::new("device-sealer");
        let request = request_for(&recipient);
        let ciphertext = encrypt_workspace_keys_for_regrant(
            &[key(1, 4)],
            GrantRecipient {
                device_id: &request.device_id,
                device_fingerprint: &request.device_fingerprint,
                device_public_key: &request.device_public_key,
            },
            GrantSealSource::Approver {
                identity: &sealer,
                device_id: sealer_device_id.clone(),
            },
        )
        .expect("encrypt regrant");

        let error = open_approver_sealed_grant(enrollment_check(
            &recipient,
            &ciphertext,
            &request,
            &sealer_device_id,
            &device_authorization_proof_verifier(&sealer).expect("sealer verifier"),
            1,
        ))
        .expect_err("scopes must not be interchangeable");
        assert!(matches!(error, GrantError::WorkspaceMismatch));
    }

    #[test]
    fn a_regrant_sealed_to_another_device_key_is_refused() {
        let intended = DeviceIdentity::generate();
        let attacker = DeviceIdentity::generate();
        let sealer = DeviceIdentity::generate();
        let sealer_device_id = DeviceId::new("device-sealer");
        let recipient_device_id = DeviceId::new("device-recipient");
        let ciphertext = encrypt_workspace_keys_for_regrant(
            &[key(2, 7)],
            GrantRecipient {
                device_id: &recipient_device_id,
                device_fingerprint: attacker.fingerprint.as_str(),
                device_public_key: attacker.public_key.as_str(),
            },
            GrantSealSource::Approver {
                identity: &sealer,
                device_id: sealer_device_id.clone(),
            },
        )
        .expect("encrypt regrant");

        assert!(
            open_approver_sealed_grant(SealedGrantCheck {
                identity: &intended,
                ciphertext: &ciphertext,
                expected_workspace_id: &workspace_id(),
                expected_scope: GrantScope::KeyRegrant,
                expected_recipient_device_id: &recipient_device_id,
                expected_recipient_fingerprint: intended.fingerprint.as_str(),
                expected_key_epoch: 2,
                sealer_device_id: &sealer_device_id,
                published_sealer_proof_verifier: &device_authorization_proof_verifier(&sealer)
                    .expect("sealer verifier"),
            })
            .is_err()
        );
    }

    #[test]
    fn device_authorization_message_matches_shared_vectors() {
        let fixture = load_fixture();
        for vector in fixture.message_vectors {
            let fields = vector.fields.iter().map(String::as_str).collect::<Vec<_>>();
            let digest = sha2::Sha256::digest(super::device_authorization_message(&fields));
            assert_eq!(format!("{digest:x}"), vector.sha256_hex, "{}", vector.name);
        }
    }

    fn load_fixture() -> FixtureFile {
        let text = fs::read_to_string(fixture_path()).expect("proof fixture is readable");
        serde_json::from_str(&text).expect("proof fixture parses")
    }

    fn fixture_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/contracts/proofs/device-proof-subjects.json")
    }
}
