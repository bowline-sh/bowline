use std::io::{self, IsTerminal, Read, Write};

use bowline_control_plane::{RecoveryEnvelopeRecord, RecoveryEnvelopeState};
use bowline_core::{
    commands::{CONTRACT_VERSION, RecoveryCommandAction, RecoveryCommandOutput},
    devices::{RecoveryKeyLifecycle, RecoveryKeyState},
    ids::RecoveryEnvelopeId,
};
use bowline_local::trust::{grants, recovery as local_recovery};
use serde::Serialize;

use crate::runtime;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryArgs {
    Status,
    Create,
    Verify { envelope_id: RecoveryEnvelopeId },
    Rotate,
    Revoke { envelope_id: RecoveryEnvelopeId },
    Use { envelope_id: RecoveryEnvelopeId },
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

    /// The payload `bowline recover --json` writes. `create` and `rotate` mint
    /// words that exist nowhere else — the control plane stores only a verifier
    /// — so the words travel in the JSON too. Omitting them made every non-TTY
    /// invocation (pipe, CI, agent) destroy the workspace's only recovery path.
    pub fn json_payload(&self) -> RecoveryJsonPayload<'_> {
        RecoveryJsonPayload {
            output: &self.output,
            one_time_recovery_words: self.generated_words.as_deref(),
        }
    }
}

/// `oneTimeRecoveryWords` is emitted by exactly one invocation and can never be
/// re-read: bowline keeps no copy. A caller that receives it owns the only copy.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryJsonPayload<'a> {
    #[serde(flatten)]
    output: &'a RecoveryCommandOutput,
    #[serde(skip_serializing_if = "Option::is_none")]
    one_time_recovery_words: Option<&'a str>,
}

pub fn run(args: RecoveryArgs, generated_at: String) -> Result<RecoveryRunOutput, String> {
    let workspace_id = runtime::active_workspace_id();
    match args {
        RecoveryArgs::Status => {
            let control_plane = runtime::control_plane()?;
            let envelopes = control_plane
                .list_recovery_envelopes(&workspace_id)
                .map_err(|error| error.to_string())?;
            let recovery_key = current_recovery_state(envelopes);
            let next_actions = if recovery_key.lifecycle == RecoveryKeyLifecycle::Missing {
                vec![bowline_core::status::RepairCommand::mutating(
                    "Create a Recovery Key".to_string(),
                    Some("bowline recover create".to_string()),
                )]
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
            let words = read_recovery_words(&envelope_id)?;
            local_recovery::verify_recovery_key(
                &*control_plane,
                &*key_store,
                workspace_id,
                envelope_id,
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
                &grants::recovery_envelope_proof_subject(&envelope_id),
            )
            .map_err(|error| error.to_string())?;
            let envelope = control_plane
                .revoke_recovery_envelope(
                    &workspace_id,
                    &envelope_id,
                    &local_device_id,
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
                    envelope_id: Some(envelope.envelope_id),
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
            let words = read_recovery_words(&envelope_id)?;
            local_recovery::use_recovery_key(
                &*control_plane,
                &*key_store,
                local_recovery::UseRecoveryKeyOptions {
                    workspace_id,
                    envelope_id,
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

/// Recovery runs when everything else has failed, so an interactive terminal
/// gets a prompt instead of a silent block on stdin. The prompt goes to stderr
/// so a `--json` stdout stays a single parseable object.
fn read_recovery_words(envelope_id: &RecoveryEnvelopeId) -> Result<String, String> {
    let stdin = io::stdin();
    let words = if stdin.is_terminal() {
        eprintln!("Recovering with Recovery Key envelope {envelope_id}.");
        eprint!("Paste your Recovery Key words, then press Return: ");
        let _ = io::stderr().flush();
        let mut line = String::new();
        stdin
            .read_line(&mut line)
            .map_err(|error| format!("failed to read Recovery Key words: {error}"))?;
        eprintln!();
        line
    } else {
        let mut piped = String::new();
        stdin
            .lock()
            .read_to_string(&mut piped)
            .map_err(|error| format!("failed to read Recovery Key words from stdin: {error}"))?;
        piped
    };
    let words = words.trim().to_string();
    if words.is_empty() {
        return Err("Recovery Key words must be provided".to_string());
    }
    Ok(words)
}

/// The single derivation of "what Recovery Key does this workspace have".
/// Every command that reports `recoveryKey` reads it from here.
pub(crate) fn current_recovery_state(envelopes: Vec<RecoveryEnvelopeRecord>) -> RecoveryKeyState {
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
        envelope_id: Some(envelope.envelope_id),
        fingerprint: Some(envelope.fingerprint),
        created_at: Some(envelope.created_at.to_string()),
        verified_at: envelope.verified_at.map(|timestamp| timestamp.to_string()),
        rotated_at: envelope.rotated_at.map(|timestamp| timestamp.to_string()),
        revoked_at: envelope.revoked_at.map(|timestamp| timestamp.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn created_output(action: RecoveryCommandAction) -> RecoveryCommandOutput {
        RecoveryCommandOutput {
            contract_version: CONTRACT_VERSION,
            command: bowline_core::commands::CommandName::Recover,
            generated_at: "2026-07-25T12:00:00Z".to_string(),
            action,
            workspace_id: Some(bowline_core::ids::WorkspaceId::new("ws_recovery")),
            recovery_key: RecoveryKeyState {
                lifecycle: RecoveryKeyLifecycle::GeneratedUnverified,
                envelope_id: Some(RecoveryEnvelopeId::new("rec_words")),
                fingerprint: Some("rkp_words".to_string()),
                created_at: Some("2026-07-25T12:00:00Z".to_string()),
                verified_at: None,
                rotated_at: None,
                revoked_at: None,
            },
            device_request: None,
            encrypted_grant: None,
            next_actions: Vec::new(),
        }
    }

    /// `bowline recover create > key.txt` resolves to the JSON output mode. If
    /// the words are not in that payload they are gone for good.
    #[test]
    fn create_json_payload_carries_the_one_time_words() {
        let run = RecoveryRunOutput {
            output: created_output(RecoveryCommandAction::Create),
            generated_words: Some("alpha beta gamma".to_string()),
        };

        let json = serde_json::to_value(run.json_payload()).expect("payload serializes");

        assert_eq!(json["oneTimeRecoveryWords"], "alpha beta gamma");
        assert_eq!(json["action"], "create");
        assert_eq!(json["recoveryKey"]["lifecycle"], "generated-unverified");
    }

    /// Rotate invalidates the previous envelope before minting the new one, so
    /// dropping its words leaves the workspace with no usable Recovery Key.
    #[test]
    fn rotate_json_payload_carries_the_one_time_words() {
        let run = RecoveryRunOutput {
            output: created_output(RecoveryCommandAction::Rotate),
            generated_words: Some("delta epsilon zeta".to_string()),
        };

        let json = serde_json::to_value(run.json_payload()).expect("payload serializes");

        assert_eq!(json["oneTimeRecoveryWords"], "delta epsilon zeta");
    }

    #[test]
    fn actions_without_generated_words_omit_the_field() {
        let run = RecoveryRunOutput::without_words(created_output(RecoveryCommandAction::Status));

        let json = serde_json::to_value(run.json_payload()).expect("payload serializes");

        assert!(json.get("oneTimeRecoveryWords").is_none());
    }
}
