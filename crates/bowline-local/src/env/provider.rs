use std::{collections::BTreeMap, fmt};

use bowline_core::ids::{DeviceId, LeaseId, ProjectId};

use super::parser::SecretBytes;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvReadScope {
    Project,
    Lease,
}

#[derive(Clone, PartialEq, Eq)]
pub struct EnvProviderRequest {
    pub caller_device_id: Option<DeviceId>,
    pub lease_id: Option<LeaseId>,
    pub project_id: ProjectId,
    pub read_scope: EnvReadScope,
    pub profile: String,
}

impl fmt::Debug for EnvProviderRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EnvProviderRequest")
            .field("caller_device_id", &self.caller_device_id)
            .field("lease_id", &self.lease_id)
            .field("project_id", &self.project_id)
            .field("read_scope", &self.read_scope)
            .field("profile", &self.profile)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct EnvProviderRecord {
    pub project_id: ProjectId,
    pub source_path: String,
    pub profile: String,
    pub key: String,
    pub occurrence_index: usize,
    pub value: SecretBytes,
    pub restriction: EnvRecordRestriction,
    pub freshness: EnvRecordFreshness,
}

impl fmt::Debug for EnvProviderRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EnvProviderRecord")
            .field("project_id", &self.project_id)
            .field("source_path", &self.source_path)
            .field("profile", &self.profile)
            .field("key", &self.key)
            .field("occurrence_index", &self.occurrence_index)
            .field("value", &self.value)
            .field("restriction", &self.restriction)
            .field("freshness", &self.freshness)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvRecordRestriction {
    Inherited,
    Restricted {
        allowed_device_ids: Vec<DeviceId>,
        lease_only: bool,
    },
    Revoked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvRecordFreshness {
    Fresh,
    Stale,
    Expired,
}

#[derive(Clone, PartialEq, Eq)]
pub struct EnvProviderResponse {
    pub values: BTreeMap<String, SecretBytes>,
    pub denials: Vec<EnvProviderDenial>,
}

impl fmt::Debug for EnvProviderResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EnvProviderResponse")
            .field("value_keys", &self.values.keys().collect::<Vec<_>>())
            .field("denials", &self.denials)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvProviderDenial {
    pub key: String,
    pub source_path: String,
    pub reason: EnvProviderDenialReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvProviderDenialReason {
    MissingCaller,
    WrongProject,
    WrongProfile,
    WrongReadScope,
    Restricted,
    Revoked,
    Stale,
}

pub fn resolve_env_provider_request(
    request: &EnvProviderRequest,
    records: &[EnvProviderRecord],
) -> EnvProviderResponse {
    let mut values = BTreeMap::new();
    let mut denials = Vec::new();

    for record in records {
        match denial_reason(request, record) {
            Some(reason) => {
                if should_block_effective_value(reason) {
                    values.remove(&record.key);
                }
                denials.push(EnvProviderDenial {
                    key: record.key.clone(),
                    source_path: record.source_path.clone(),
                    reason,
                });
            }
            None => {
                values.insert(record.key.clone(), record.value.clone());
            }
        }
    }

    EnvProviderResponse { values, denials }
}

fn should_block_effective_value(reason: EnvProviderDenialReason) -> bool {
    !matches!(
        reason,
        EnvProviderDenialReason::WrongProject | EnvProviderDenialReason::WrongProfile
    )
}

fn denial_reason(
    request: &EnvProviderRequest,
    record: &EnvProviderRecord,
) -> Option<EnvProviderDenialReason> {
    let caller = match request.caller_device_id.as_ref() {
        Some(caller) => caller,
        None => return Some(EnvProviderDenialReason::MissingCaller),
    };
    if record.project_id != request.project_id {
        return Some(EnvProviderDenialReason::WrongProject);
    }
    if record.profile != request.profile {
        return Some(EnvProviderDenialReason::WrongProfile);
    }
    match record.freshness {
        EnvRecordFreshness::Fresh => {}
        EnvRecordFreshness::Stale | EnvRecordFreshness::Expired => {
            return Some(EnvProviderDenialReason::Stale);
        }
    }
    match &record.restriction {
        EnvRecordRestriction::Inherited => None,
        EnvRecordRestriction::Revoked => Some(EnvProviderDenialReason::Revoked),
        EnvRecordRestriction::Restricted {
            allowed_device_ids,
            lease_only,
        } => {
            if *lease_only
                && (request.read_scope != EnvReadScope::Lease || request.lease_id.is_none())
            {
                return Some(EnvProviderDenialReason::WrongReadScope);
            }
            if allowed_device_ids
                .iter()
                .any(|device_id| device_id == caller)
            {
                None
            } else {
                Some(EnvProviderDenialReason::Restricted)
            }
        }
    }
}
