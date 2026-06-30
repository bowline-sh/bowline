use std::io::{self, Read};

use bowline_control_plane::{RecoveryEnvelopeRecord, RecoveryEnvelopeState};
use bowline_core::{
    commands::{CONTRACT_VERSION, RecoveryCommandAction, RecoveryCommandOutput},
    devices::{RecoveryKeyLifecycle, RecoveryKeyState},
    ids::RecoveryEnvelopeId,
};
use bowline_local::trust::{grants, recovery as local_recovery};

use crate::runtime;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryArgs {
    Status,
    Create,
    Verify { envelope_id: String },
    Rotate,
    Revoke { envelope_id: String },
    Use { envelope_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryRunOutput {
    pub output: RecoveryCommandOutput,
    pub generated_words: Option<String>,
}

impl RecoveryRunOutput {
    fn without_words(output: RecoveryCommandOutput) -> Self {
        Self {
            output,
            generated_words: None,
        }
    }
}

pub fn run(args: RecoveryArgs, generated_at: String) -> Result<RecoveryRunOutput, String> {
    let workspace_id = runtime::active_workspace_id();
    match args {
        RecoveryArgs::Status => {
            let control_plane = runtime::control_plane()?;
            let envelopes = control_plane
                .list_recovery_envelopes(workspace_id.as_str())
                .map_err(|error| error.to_string())?;
            let recovery_key = current_recovery_state(envelopes);
            let next_actions = if recovery_key.lifecycle == RecoveryKeyLifecycle::Missing {
                vec![bowline_core::status::SafeAction {
                    label: "Create a Recovery Key".to_string(),
                    command: Some("bowline recover create".to_string()),
                }]
            } else {
                Vec::new()
            };
            Ok(RecoveryRunOutput::without_words(RecoveryCommandOutput {
                contract_version: CONTRACT_VERSION,
                command: bowline_core::commands::CommandName::Recover,
                generated_at,
                action: RecoveryCommandAction::Status,
                workspace_id: Some(workspace_id),
                recovery_key,
                device_request: None,
                encrypted_grant: None,
                next_actions,
            }))
        }
        RecoveryArgs::Create => {
            let control_plane = runtime::control_plane()?;
            let key_store = runtime::key_store()?;
            let (key, output) = local_recovery::create_recovery_key(
                &*control_plane,
                &*key_store,
                workspace_id,
                runtime::device_id(),
                generated_at,
            )
            .map_err(|error| error.to_string())?;
            Ok(RecoveryRunOutput {
                output,
                generated_words: Some(key.words),
            })
        }
        RecoveryArgs::Verify { envelope_id } => {
            let control_plane = runtime::control_plane()?;
            let key_store = runtime::key_store()?;
            let words = read_words_from_stdin()?;
            local_recovery::verify_recovery_key(
                &*control_plane,
                &*key_store,
                workspace_id,
                RecoveryEnvelopeId::new(envelope_id),
                runtime::device_id(),
                &words,
                generated_at,
            )
            .map(RecoveryRunOutput::without_words)
            .map_err(|error| error.to_string())
        }
        RecoveryArgs::Rotate => {
            let control_plane = runtime::control_plane()?;
            let key_store = runtime::key_store()?;
            let (key, output) = local_recovery::rotate_recovery_key(
                &*control_plane,
                &*key_store,
                workspace_id,
                runtime::device_id(),
                generated_at,
            )
            .map_err(|error| error.to_string())?;
            Ok(RecoveryRunOutput {
                output,
                generated_words: Some(key.words),
            })
        }
        RecoveryArgs::Revoke { envelope_id } => {
            let control_plane = runtime::control_plane()?;
            let key_store = runtime::key_store()?;
            let local_device_id = runtime::device_id();
            let identity = key_store
                .load_or_create_device_identity()
                .map_err(|error| error.to_string())?;
            let revoked_by_device_proof = grants::device_authorization_proof(
                &identity,
                &workspace_id,
                &local_device_id,
                "revoke-recovery-envelope",
                &envelope_id,
            );
            let envelope = control_plane
                .revoke_recovery_envelope(
                    workspace_id.as_str(),
                    &envelope_id,
                    local_device_id.as_str(),
                    &revoked_by_device_proof,
                )
                .map_err(|error| error.to_string())?;
            Ok(RecoveryRunOutput::without_words(RecoveryCommandOutput {
                contract_version: CONTRACT_VERSION,
                command: bowline_core::commands::CommandName::Recover,
                generated_at: generated_at.clone(),
                action: RecoveryCommandAction::Revoke,
                workspace_id: Some(workspace_id),
                recovery_key: RecoveryKeyState {
                    lifecycle: RecoveryKeyLifecycle::Revoked,
                    envelope_id: Some(RecoveryEnvelopeId::new(envelope.envelope_id)),
                    fingerprint: Some(envelope.fingerprint),
                    created_at: Some(envelope.created_at.to_string()),
                    verified_at: envelope.verified_at.map(|timestamp| timestamp.to_string()),
                    rotated_at: envelope.rotated_at.map(|timestamp| timestamp.to_string()),
                    revoked_at: Some(generated_at),
                },
                device_request: None,
                encrypted_grant: None,
                next_actions: Vec::new(),
            }))
        }
        RecoveryArgs::Use { envelope_id } => {
            let control_plane = runtime::control_plane()?;
            let key_store = runtime::key_store()?;
            let words = read_words_from_stdin()?;
            local_recovery::use_recovery_key(
                &*control_plane,
                &*key_store,
                local_recovery::UseRecoveryKeyOptions {
                    workspace_id,
                    envelope_id: RecoveryEnvelopeId::new(envelope_id),
                    words,
                    device_id: runtime::device_id(),
                    device_name: runtime::device_name(),
                    platform: runtime::platform(),
                    generated_at,
                },
            )
            .map(RecoveryRunOutput::without_words)
            .map_err(|error| error.to_string())
        }
    }
}

fn read_words_from_stdin() -> Result<String, String> {
    let mut words = String::new();
    io::stdin()
        .read_to_string(&mut words)
        .map_err(|error| format!("failed to read Recovery Key words from stdin: {error}"))?;
    let words = words.trim().to_string();
    if words.is_empty() {
        return Err("Recovery Key words must be provided on stdin".to_string());
    }
    Ok(words)
}

fn current_recovery_state(envelopes: Vec<RecoveryEnvelopeRecord>) -> RecoveryKeyState {
    envelopes
        .into_iter()
        .max_by_key(|envelope| {
            (
                recovery_state_priority(envelope.state),
                envelope
                    .revoked_at
                    .or(envelope.rotated_at)
                    .or(envelope.verified_at)
                    .unwrap_or(envelope.created_at),
            )
        })
        .map(recovery_state_from_envelope)
        .unwrap_or_else(RecoveryKeyState::missing)
}

fn recovery_state_priority(state: RecoveryEnvelopeState) -> u8 {
    match state {
        RecoveryEnvelopeState::Active => 4,
        RecoveryEnvelopeState::GeneratedUnverified => 3,
        RecoveryEnvelopeState::Rotated => 2,
        RecoveryEnvelopeState::Revoked => 1,
    }
}

fn recovery_state_from_envelope(envelope: RecoveryEnvelopeRecord) -> RecoveryKeyState {
    RecoveryKeyState {
        lifecycle: match envelope.state {
            RecoveryEnvelopeState::GeneratedUnverified => RecoveryKeyLifecycle::GeneratedUnverified,
            RecoveryEnvelopeState::Active => RecoveryKeyLifecycle::Active,
            RecoveryEnvelopeState::Rotated => RecoveryKeyLifecycle::Rotated,
            RecoveryEnvelopeState::Revoked => RecoveryKeyLifecycle::Revoked,
        },
        envelope_id: Some(RecoveryEnvelopeId::new(envelope.envelope_id)),
        fingerprint: Some(envelope.fingerprint),
        created_at: Some(envelope.created_at.to_string()),
        verified_at: envelope.verified_at.map(|timestamp| timestamp.to_string()),
        rotated_at: envelope.rotated_at.map(|timestamp| timestamp.to_string()),
        revoked_at: envelope.revoked_at.map(|timestamp| timestamp.to_string()),
    }
}
