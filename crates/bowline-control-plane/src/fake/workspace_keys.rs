use super::*;
use crate::{
    WorkspaceKeyAcceptanceInput, WorkspaceKeyControlPlaneClient, WorkspaceKeyPublicationInput,
    WorkspaceKeyRegrantAssignment, WorkspaceKeyRegrantRecipient, WorkspaceKeyRegrantState,
    WorkspaceKeyRegrantWork, WorkspaceKeyRegrantWorkRequest, key_regrant_accept_proof_subject,
    key_regrant_offer_proof_subject, key_regrant_work_proof_subject,
};

const LIST_KEY_REGRANT_WORK_ACTION: &str = "list-workspace-key-regrant-work";
const SEED_KEY_EPOCH_ACTION: &str = "seed-workspace-key-epoch";
const OFFER_KEY_REGRANT_ACTION: &str = "offer-workspace-key-regrant";
const ACCEPT_KEY_REGRANT_ACTION: &str = "accept-workspace-key-regrant";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FakeKeyRegrant {
    pub(super) state: WorkspaceKeyRegrantState,
    pub(super) ciphertext: Option<String>,
    pub(super) acceptance_proof_verifier: Option<String>,
    pub(super) sealed_by_device_id: Option<DeviceId>,
}

impl FakeKeyRegrant {
    pub(super) fn pending() -> Self {
        Self {
            state: WorkspaceKeyRegrantState::Pending,
            ciphertext: None,
            acceptance_proof_verifier: None,
            sealed_by_device_id: None,
        }
    }
}

impl WorkspaceKeyControlPlaneClient for FakeControlPlaneClient {
    fn get_workspace_key_regrant_work(
        &self,
        request: WorkspaceKeyRegrantWorkRequest,
    ) -> ControlPlaneResult<WorkspaceKeyRegrantWork> {
        self.ensure_workspace(&request.workspace_id)?;
        let state = self.state.lock().expect("fake control plane poisoned");
        Self::ensure_authorized_approver(
            &state,
            &request.workspace_id,
            &request.device_id,
            &request.device_proof,
            LIST_KEY_REGRANT_WORK_ACTION,
            &key_regrant_work_proof_subject(&request.workspace_id),
        )?;
        Ok(Self::regrant_work(
            &state,
            &request.workspace_id,
            &request.device_id,
        ))
    }

    fn seed_workspace_key_epoch(
        &self,
        input: WorkspaceKeyPublicationInput,
    ) -> ControlPlaneResult<WorkspaceKeyRegrantWork> {
        self.ensure_workspace(&input.workspace_id)?;
        let subject = key_regrant_offer_proof_subject(input.key_epoch, &input.offers);
        let mut state = self.state.lock().expect("fake control plane poisoned");
        Self::ensure_authorized_approver(
            &state,
            &input.workspace_id,
            &input.device_id,
            &input.device_proof,
            SEED_KEY_EPOCH_ACTION,
            &subject,
        )?;
        if state.pending_workspace_key_epochs.get(&input.workspace_id) != Some(&input.key_epoch) {
            return Err(ControlPlaneError::Conflict {
                resource: "workspace-key-epoch",
                reason: "workspace has no rotation pending at this key epoch",
            });
        }
        if state
            .workspace_key_epoch_seeders
            .contains_key(&input.workspace_id)
        {
            return Err(ControlPlaneError::Conflict {
                resource: "workspace-key-epoch",
                reason: "workspace key epoch has already been seeded by another device",
            });
        }
        state
            .workspace_key_epochs
            .insert(input.workspace_id.clone(), input.key_epoch);
        state
            .pending_workspace_key_epochs
            .remove(&input.workspace_id);
        state
            .workspace_key_epoch_seeders
            .insert(input.workspace_id.clone(), input.device_id.clone());
        Self::settle_regrant(
            &mut state,
            &input.workspace_id,
            &input.device_id,
            input.key_epoch,
        );
        Self::publish_offers(&mut state, &input)?;
        let work = Self::regrant_work(&state, &input.workspace_id, &input.device_id);
        drop(state);
        self.record_workspace_key_event(&input.workspace_id, input.key_epoch);
        Ok(work)
    }

    fn offer_workspace_key_regrants(
        &self,
        input: WorkspaceKeyPublicationInput,
    ) -> ControlPlaneResult<WorkspaceKeyRegrantWork> {
        self.ensure_workspace(&input.workspace_id)?;
        let subject = key_regrant_offer_proof_subject(input.key_epoch, &input.offers);
        let mut state = self.state.lock().expect("fake control plane poisoned");
        Self::ensure_authorized_approver(
            &state,
            &input.workspace_id,
            &input.device_id,
            &input.device_proof,
            OFFER_KEY_REGRANT_ACTION,
            &subject,
        )?;
        let held = Self::held_key_epoch(&state, &input.workspace_id, &input.device_id);
        if held < input.key_epoch {
            return Err(ControlPlaneError::Conflict {
                resource: "workspace-key-epoch",
                reason: "device does not hold the workspace key epoch it is offering",
            });
        }
        Self::publish_offers(&mut state, &input)?;
        Ok(Self::regrant_work(
            &state,
            &input.workspace_id,
            &input.device_id,
        ))
    }

    fn accept_workspace_key_regrant(
        &self,
        input: WorkspaceKeyAcceptanceInput,
    ) -> ControlPlaneResult<WorkspaceKeyRegrantWork> {
        self.ensure_workspace(&input.workspace_id)?;
        let subject = key_regrant_accept_proof_subject(input.key_epoch);
        let mut state = self.state.lock().expect("fake control plane poisoned");
        Self::ensure_authorized_approver(
            &state,
            &input.workspace_id,
            &input.device_id,
            &input.device_proof,
            ACCEPT_KEY_REGRANT_ACTION,
            &subject,
        )?;
        let key = (
            input.workspace_id.clone(),
            input.key_epoch,
            input.device_id.clone(),
        );
        let regrant =
            state
                .workspace_key_regrants
                .get(&key)
                .ok_or(ControlPlaneError::Conflict {
                    resource: "workspace-key-regrant",
                    reason: "device has no key regrant at that epoch",
                })?;
        let expected = regrant.acceptance_proof_verifier.clone();
        if regrant.state != WorkspaceKeyRegrantState::Offered {
            return Err(ControlPlaneError::Conflict {
                resource: "workspace-key-regrant",
                reason: "no sealed key material is waiting for this device at that epoch",
            });
        }
        if expected.as_deref()
            != Some(grant_acceptance_proof_verifier(&input.grant_acceptance_proof).as_str())
        {
            return Err(device_not_trusted(
                "key regrant acceptance proof does not match",
            ));
        }
        Self::settle_regrant(
            &mut state,
            &input.workspace_id,
            &input.device_id,
            input.key_epoch,
        );
        Ok(Self::regrant_work(
            &state,
            &input.workspace_id,
            &input.device_id,
        ))
    }
}

impl FakeControlPlaneClient {
    /// Rotation bookkeeping shared by device revocation: name a new pending
    /// epoch, close obligations the rotation makes moot, and open one for every
    /// device that survives it.
    pub(super) fn rotate_workspace_key_epoch(
        state: &mut FakeControlPlaneState,
        workspace_id: &WorkspaceId,
    ) -> u32 {
        let current = Self::current_key_epoch(state, workspace_id);
        let pending = state
            .pending_workspace_key_epochs
            .get(workspace_id)
            .copied()
            .unwrap_or(current)
            .max(current)
            + 1;
        state
            .pending_workspace_key_epochs
            .insert(workspace_id.clone(), pending);
        state.workspace_key_epoch_seeders.remove(workspace_id);
        for ((regrant_workspace, epoch, _), regrant) in state.workspace_key_regrants.iter_mut() {
            if regrant_workspace != workspace_id || *epoch >= pending {
                continue;
            }
            if regrant.state.is_outstanding() {
                regrant.state = WorkspaceKeyRegrantState::Superseded;
            }
        }
        let remaining = state
            .authorized_devices
            .keys()
            .filter(|(device_workspace, _)| device_workspace == workspace_id)
            .map(|(_, device_id)| device_id.clone())
            .collect::<Vec<_>>();
        for device_id in remaining {
            state.workspace_key_regrants.insert(
                (workspace_id.clone(), pending, device_id),
                FakeKeyRegrant::pending(),
            );
        }
        pending
    }

    pub(super) fn current_key_epoch(
        state: &FakeControlPlaneState,
        workspace_id: &WorkspaceId,
    ) -> u32 {
        state
            .workspace_key_epochs
            .get(workspace_id)
            .copied()
            .unwrap_or(1)
    }

    pub(super) fn record_device_key_material(
        state: &mut FakeControlPlaneState,
        workspace_id: &WorkspaceId,
        device_id: &DeviceId,
        device_public_key: String,
        device_public_key_proof: String,
        held_key_epoch: u32,
    ) {
        let key = (workspace_id.clone(), device_id.clone());
        state
            .device_public_keys
            .insert(key.clone(), device_public_key);
        state
            .device_public_key_proofs
            .insert(key.clone(), device_public_key_proof);
        state.device_held_key_epochs.insert(key, held_key_epoch);
        let current = Self::current_key_epoch(state, workspace_id);
        if held_key_epoch < current {
            state
                .workspace_key_regrants
                .entry((workspace_id.clone(), current, device_id.clone()))
                .or_insert_with(FakeKeyRegrant::pending);
        }
    }

    pub(super) fn forget_device_key_material(
        state: &mut FakeControlPlaneState,
        workspace_id: &WorkspaceId,
        device_id: &DeviceId,
    ) {
        let key = (workspace_id.clone(), device_id.clone());
        state.device_public_keys.remove(&key);
        state.device_public_key_proofs.remove(&key);
        state.device_held_key_epochs.remove(&key);
        state
            .workspace_key_regrants
            .retain(|(regrant_workspace, _, regrant_device), _| {
                regrant_workspace != workspace_id || regrant_device != device_id
            });
    }

    fn held_key_epoch(
        state: &FakeControlPlaneState,
        workspace_id: &WorkspaceId,
        device_id: &DeviceId,
    ) -> u32 {
        state
            .device_held_key_epochs
            .get(&(workspace_id.clone(), device_id.clone()))
            .copied()
            .unwrap_or(0)
    }

    fn settle_regrant(
        state: &mut FakeControlPlaneState,
        workspace_id: &WorkspaceId,
        device_id: &DeviceId,
        key_epoch: u32,
    ) {
        if let Some(regrant) = state.workspace_key_regrants.get_mut(&(
            workspace_id.clone(),
            key_epoch,
            device_id.clone(),
        )) && regrant.state.is_outstanding()
        {
            regrant.state = WorkspaceKeyRegrantState::Fulfilled;
        }
        let held = state
            .device_held_key_epochs
            .entry((workspace_id.clone(), device_id.clone()))
            .or_insert(0);
        *held = (*held).max(key_epoch);
    }

    fn publish_offers(
        state: &mut FakeControlPlaneState,
        input: &WorkspaceKeyPublicationInput,
    ) -> ControlPlaneResult<()> {
        for offer in &input.offers {
            let key = (
                input.workspace_id.clone(),
                input.key_epoch,
                offer.recipient_device_id.clone(),
            );
            let Some(regrant) = state.workspace_key_regrants.get(&key) else {
                return Err(ControlPlaneError::Conflict {
                    resource: "workspace-key-regrant",
                    reason: "key regrant offer targets a device with no outstanding obligation",
                });
            };
            if !regrant.state.is_outstanding() {
                return Err(ControlPlaneError::Conflict {
                    resource: "workspace-key-regrant",
                    reason: "key regrant offer targets a device with no outstanding obligation",
                });
            }
            if regrant.state == WorkspaceKeyRegrantState::Offered
                && regrant.sealed_by_device_id.as_ref() != Some(&input.device_id)
            {
                return Err(ControlPlaneError::Conflict {
                    resource: "workspace-key-regrant",
                    reason: "key regrant has already been offered by another device",
                });
            }
        }
        for offer in &input.offers {
            let key = (
                input.workspace_id.clone(),
                input.key_epoch,
                offer.recipient_device_id.clone(),
            );
            let regrant = state
                .workspace_key_regrants
                .get_mut(&key)
                .expect("key regrant publication was preflighted");
            regrant.state = WorkspaceKeyRegrantState::Offered;
            regrant.ciphertext = Some(offer.ciphertext.clone());
            regrant.acceptance_proof_verifier = Some(offer.acceptance_proof_verifier.clone());
            regrant.sealed_by_device_id = Some(input.device_id.clone());
        }
        Ok(())
    }

    fn regrant_work(
        state: &FakeControlPlaneState,
        workspace_id: &WorkspaceId,
        device_id: &DeviceId,
    ) -> WorkspaceKeyRegrantWork {
        let mut outstanding = Vec::new();
        let mut own_regrant = None;
        // BTreeMap iteration is (workspace, epoch, device) ordered, which is the
        // convergence order the hosted endpoint promises.
        for ((regrant_workspace, epoch, regrant_device), regrant) in &state.workspace_key_regrants {
            if regrant_workspace != workspace_id || !regrant.state.is_outstanding() {
                continue;
            }
            if regrant_device == device_id {
                own_regrant = Some(WorkspaceKeyRegrantAssignment {
                    ciphertext: regrant.ciphertext.clone(),
                    key_epoch: *epoch,
                    sealed_by_device_id: regrant.sealed_by_device_id.clone(),
                    state: regrant.state,
                });
            }
            let Some(device) = state
                .authorized_devices
                .get(&(workspace_id.clone(), regrant_device.clone()))
            else {
                continue;
            };
            outstanding.push(WorkspaceKeyRegrantRecipient {
                device_authorization_proof_verifier: state
                    .device_authorization_proof_verifiers
                    .get(&(workspace_id.clone(), regrant_device.clone()))
                    .cloned(),
                device_fingerprint: device.device_fingerprint.clone(),
                device_id: regrant_device.clone(),
                device_name: device.device_name.clone(),
                device_public_key: state
                    .device_public_keys
                    .get(&(workspace_id.clone(), regrant_device.clone()))
                    .cloned()
                    .unwrap_or_default(),
                device_public_key_proof: state
                    .device_public_key_proofs
                    .get(&(workspace_id.clone(), regrant_device.clone()))
                    .cloned()
                    .unwrap_or_default(),
                held_key_epoch: Self::held_key_epoch(state, workspace_id, regrant_device),
                key_epoch: *epoch,
                platform: device.platform.clone(),
                sealed_by_device_id: regrant.sealed_by_device_id.clone(),
                state: regrant.state,
            });
        }
        WorkspaceKeyRegrantWork {
            current_key_epoch: Self::current_key_epoch(state, workspace_id),
            outstanding,
            own_regrant,
            pending_key_epoch: state
                .pending_workspace_key_epochs
                .get(workspace_id)
                .copied(),
            pending_seeded_by_device_id: state
                .workspace_key_epoch_seeders
                .get(workspace_id)
                .cloned(),
            workspace_id: workspace_id.clone(),
        }
    }

    fn record_workspace_key_event(&self, workspace_id: &WorkspaceId, key_epoch: u32) {
        let event = self.build_event(
            workspace_id,
            CompactEventKind::WorkspaceKeyRotated,
            &format!("key_epoch:{key_epoch}"),
        );
        self.state
            .lock()
            .expect("fake control plane poisoned")
            .events
            .entry(workspace_id.clone())
            .or_default()
            .push(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WorkspaceKeyRegrantOffer;

    #[test]
    fn offered_regrant_only_accepts_retries_from_its_original_sealer() {
        let workspace_id = WorkspaceId::new("workspace-regrant-sealer");
        let recipient_device_id = DeviceId::new("device-recipient");
        let pending_device_id = DeviceId::new("device-pending");
        let original_sealer = DeviceId::new("device-original-sealer");
        let mut state = FakeControlPlaneState::default();
        let key = (workspace_id.clone(), 2, recipient_device_id.clone());
        let pending_key = (workspace_id.clone(), 2, pending_device_id.clone());
        state.workspace_key_regrants.insert(
            key.clone(),
            FakeKeyRegrant {
                state: WorkspaceKeyRegrantState::Offered,
                ciphertext: Some("original-ciphertext".to_string()),
                acceptance_proof_verifier: Some("original-verifier".to_string()),
                sealed_by_device_id: Some(original_sealer.clone()),
            },
        );
        state
            .workspace_key_regrants
            .insert(pending_key.clone(), FakeKeyRegrant::pending());

        FakeControlPlaneClient::publish_offers(
            &mut state,
            &WorkspaceKeyPublicationInput {
                workspace_id: workspace_id.clone(),
                device_id: original_sealer,
                device_proof: "unused-by-publication-helper".to_string(),
                key_epoch: 2,
                offers: vec![WorkspaceKeyRegrantOffer {
                    recipient_device_id: recipient_device_id.clone(),
                    ciphertext: "retried-ciphertext".to_string(),
                    acceptance_proof_verifier: "retried-verifier".to_string(),
                }],
            },
        )
        .expect("the original sealer may retry its offer");

        let attacker = DeviceId::new("device-other-sealer");
        let attack = FakeControlPlaneClient::publish_offers(
            &mut state,
            &WorkspaceKeyPublicationInput {
                workspace_id,
                device_id: attacker,
                device_proof: "unused-by-publication-helper".to_string(),
                key_epoch: 2,
                offers: vec![
                    WorkspaceKeyRegrantOffer {
                        recipient_device_id: pending_device_id,
                        ciphertext: "pending-ciphertext".to_string(),
                        acceptance_proof_verifier: "pending-verifier".to_string(),
                    },
                    WorkspaceKeyRegrantOffer {
                        recipient_device_id,
                        ciphertext: "attacker-ciphertext".to_string(),
                        acceptance_proof_verifier: "attacker-verifier".to_string(),
                    },
                ],
            },
        );

        assert!(matches!(
            attack,
            Err(ControlPlaneError::Conflict {
                resource: "workspace-key-regrant",
                reason: "key regrant has already been offered by another device",
            })
        ));
        assert_eq!(
            state
                .workspace_key_regrants
                .get(&key)
                .expect("regrant remains")
                .ciphertext
                .as_deref(),
            Some("retried-ciphertext")
        );
        assert_eq!(
            state
                .workspace_key_regrants
                .get(&pending_key)
                .expect("pending regrant remains"),
            &FakeKeyRegrant::pending(),
            "a rejected batch does not partially publish earlier offers"
        );
    }
}
