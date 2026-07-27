use super::generated::{
    HostedWorkspaceKeyRegrantAssignment, HostedWorkspaceKeyRegrantOffer,
    HostedWorkspaceKeyRegrantRecipient, HostedWorkspaceKeyRegrantState,
    HostedWorkspaceKeyRegrantWork, HostedWorkspaceKeysAcceptKeyRegrantRequest,
    HostedWorkspaceKeysGetKeyRegrantWorkRequest, HostedWorkspaceKeysOfferKeyRegrantsRequest,
    HostedWorkspaceKeysSeedKeyEpochRequest, WorkspaceKeysAcceptKeyRegrant,
    WorkspaceKeysGetKeyRegrantWork, WorkspaceKeysOfferKeyRegrants, WorkspaceKeysSeedKeyEpoch,
};
use super::*;
use crate::{
    WorkspaceKeyAcceptanceInput, WorkspaceKeyControlPlaneClient, WorkspaceKeyPublicationInput,
    WorkspaceKeyRegrantAssignment, WorkspaceKeyRegrantOffer, WorkspaceKeyRegrantRecipient,
    WorkspaceKeyRegrantState, WorkspaceKeyRegrantWork, WorkspaceKeyRegrantWorkRequest,
};

impl WorkspaceKeyControlPlaneClient for HostedControlPlaneClient {
    fn get_workspace_key_regrant_work(
        &self,
        request: WorkspaceKeyRegrantWorkRequest,
    ) -> ControlPlaneResult<WorkspaceKeyRegrantWork> {
        self.require_local_device(&request.device_id)?;
        // Proof subjects are built and signed by the caller; this boundary never
        // re-derives one, so the bytes that were signed are the bytes that ride.
        let wire = HostedWorkspaceKeysGetKeyRegrantWorkRequest {
            device_id: request.device_id.as_str().to_string(),
            device_proof: request.device_proof,
            workspace_id: request.workspace_id.as_str().to_string(),
        };
        WorkspaceKeyRegrantWork::try_from(self.call::<WorkspaceKeysGetKeyRegrantWork>(&wire)?)
    }

    fn seed_workspace_key_epoch(
        &self,
        input: WorkspaceKeyPublicationInput,
    ) -> ControlPlaneResult<WorkspaceKeyRegrantWork> {
        self.require_local_device(&input.device_id)?;
        let wire = HostedWorkspaceKeysSeedKeyEpochRequest {
            device_id: input.device_id.as_str().to_string(),
            device_proof: input.device_proof,
            key_epoch: input.key_epoch,
            offers: input.offers.iter().map(offer_dto).collect(),
            workspace_id: input.workspace_id.as_str().to_string(),
        };
        WorkspaceKeyRegrantWork::try_from(self.call::<WorkspaceKeysSeedKeyEpoch>(&wire)?)
    }

    fn offer_workspace_key_regrants(
        &self,
        input: WorkspaceKeyPublicationInput,
    ) -> ControlPlaneResult<WorkspaceKeyRegrantWork> {
        self.require_local_device(&input.device_id)?;
        let wire = HostedWorkspaceKeysOfferKeyRegrantsRequest {
            device_id: input.device_id.as_str().to_string(),
            device_proof: input.device_proof,
            key_epoch: input.key_epoch,
            offers: input.offers.iter().map(offer_dto).collect(),
            workspace_id: input.workspace_id.as_str().to_string(),
        };
        WorkspaceKeyRegrantWork::try_from(self.call::<WorkspaceKeysOfferKeyRegrants>(&wire)?)
    }

    fn accept_workspace_key_regrant(
        &self,
        input: WorkspaceKeyAcceptanceInput,
    ) -> ControlPlaneResult<WorkspaceKeyRegrantWork> {
        self.require_local_device(&input.device_id)?;
        let wire = HostedWorkspaceKeysAcceptKeyRegrantRequest {
            device_id: input.device_id.as_str().to_string(),
            device_proof: input.device_proof,
            grant_acceptance_proof: input.grant_acceptance_proof,
            key_epoch: input.key_epoch,
            workspace_id: input.workspace_id.as_str().to_string(),
        };
        WorkspaceKeyRegrantWork::try_from(self.call::<WorkspaceKeysAcceptKeyRegrant>(&wire)?)
    }
}

fn offer_dto(offer: &WorkspaceKeyRegrantOffer) -> HostedWorkspaceKeyRegrantOffer {
    HostedWorkspaceKeyRegrantOffer {
        acceptance_proof_verifier: offer.acceptance_proof_verifier.clone(),
        ciphertext: offer.ciphertext.clone(),
        recipient_device_id: offer.recipient_device_id.as_str().to_string(),
    }
}

impl TryFrom<HostedWorkspaceKeyRegrantWork> for WorkspaceKeyRegrantWork {
    type Error = ControlPlaneError;

    fn try_from(dto: HostedWorkspaceKeyRegrantWork) -> Result<Self, Self::Error> {
        Ok(Self {
            current_key_epoch: dto.current_key_epoch,
            outstanding: dto
                .outstanding
                .into_iter()
                .map(WorkspaceKeyRegrantRecipient::from)
                .collect(),
            own_regrant: dto.own_regrant.map(WorkspaceKeyRegrantAssignment::from),
            pending_key_epoch: dto.pending_key_epoch,
            pending_seeded_by_device_id: dto.pending_seeded_by_device_id.map(DeviceId::new),
            workspace_id: WorkspaceId::new(dto.workspace_id),
        })
    }
}

impl From<HostedWorkspaceKeyRegrantRecipient> for WorkspaceKeyRegrantRecipient {
    fn from(dto: HostedWorkspaceKeyRegrantRecipient) -> Self {
        Self {
            device_authorization_proof_verifier: dto.device_authorization_proof_verifier,
            device_fingerprint: dto.device_fingerprint,
            device_id: DeviceId::new(dto.device_id),
            device_name: dto.device_name,
            device_public_key: dto.device_public_key,
            device_public_key_proof: dto.device_public_key_proof,
            held_key_epoch: dto.held_key_epoch,
            key_epoch: dto.key_epoch,
            platform: dto.platform,
            sealed_by_device_id: dto.sealed_by_device_id.map(DeviceId::new),
            state: dto.state.into(),
        }
    }
}

impl From<HostedWorkspaceKeyRegrantAssignment> for WorkspaceKeyRegrantAssignment {
    fn from(dto: HostedWorkspaceKeyRegrantAssignment) -> Self {
        Self {
            ciphertext: dto.ciphertext,
            key_epoch: dto.key_epoch,
            sealed_by_device_id: dto.sealed_by_device_id.map(DeviceId::new),
            state: dto.state.into(),
        }
    }
}

impl From<HostedWorkspaceKeyRegrantState> for WorkspaceKeyRegrantState {
    fn from(dto: HostedWorkspaceKeyRegrantState) -> Self {
        match dto {
            HostedWorkspaceKeyRegrantState::Pending => Self::Pending,
            HostedWorkspaceKeyRegrantState::Offered => Self::Offered,
            HostedWorkspaceKeyRegrantState::Fulfilled => Self::Fulfilled,
            HostedWorkspaceKeyRegrantState::Superseded => Self::Superseded,
        }
    }
}
