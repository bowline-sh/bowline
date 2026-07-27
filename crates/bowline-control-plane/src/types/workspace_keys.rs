use bowline_core::ids::{DeviceId, WorkspaceId};

/// Lifecycle of one device's obligation to hold a workspace key epoch. Closed
/// set: the wire enum rejects unknown variants rather than tolerating a string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WorkspaceKeyRegrantState {
    Pending,
    Offered,
    Fulfilled,
    Superseded,
}

impl WorkspaceKeyRegrantState {
    pub fn is_outstanding(self) -> bool {
        matches!(self, Self::Pending | Self::Offered)
    }
}

/// A device that still owes an epoch, with everything a holder needs to seal
/// for it. `device_authorization_proof_verifier` is the control plane's copy;
/// a sealer cross-checks it against the verifier it cached when it first met
/// this device, so a substituted recipient key is caught before any sealing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceKeyRegrantRecipient {
    pub device_id: DeviceId,
    pub device_name: String,
    pub platform: String,
    pub device_fingerprint: String,
    pub device_public_key: String,
    pub device_public_key_proof: String,
    pub device_authorization_proof_verifier: Option<String>,
    pub held_key_epoch: u32,
    pub key_epoch: u32,
    pub sealed_by_device_id: Option<DeviceId>,
    pub state: WorkspaceKeyRegrantState,
}

/// The calling device's own outstanding obligation, carrying the sealed
/// ciphertext once a holder has offered it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceKeyRegrantAssignment {
    pub key_epoch: u32,
    pub state: WorkspaceKeyRegrantState,
    pub ciphertext: Option<String>,
    pub sealed_by_device_id: Option<DeviceId>,
}

/// Everything a trusted device needs to converge on the workspace key epoch
/// without asking the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceKeyRegrantWork {
    pub workspace_id: WorkspaceId,
    pub current_key_epoch: u32,
    pub pending_key_epoch: Option<u32>,
    pub pending_seeded_by_device_id: Option<DeviceId>,
    pub own_regrant: Option<WorkspaceKeyRegrantAssignment>,
    pub outstanding: Vec<WorkspaceKeyRegrantRecipient>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceKeyRegrantOffer {
    pub recipient_device_id: DeviceId,
    pub ciphertext: String,
    pub acceptance_proof_verifier: String,
}

/// Every call in this family is authenticated by a device authorization proof
/// the caller signs itself, exactly as device approval and revocation are. The
/// proof is what makes a revoked device's request fail: its authorization row
/// and published verifier are deleted at revocation, so nothing it signs can
/// verify.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceKeyRegrantWorkRequest {
    pub workspace_id: WorkspaceId,
    pub device_id: DeviceId,
    pub device_proof: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceKeyPublicationInput {
    pub workspace_id: WorkspaceId,
    pub device_id: DeviceId,
    pub device_proof: String,
    pub key_epoch: u32,
    pub offers: Vec<WorkspaceKeyRegrantOffer>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceKeyAcceptanceInput {
    pub workspace_id: WorkspaceId,
    pub device_id: DeviceId,
    pub device_proof: String,
    pub key_epoch: u32,
    pub grant_acceptance_proof: String,
}
