use bowline_core::{
    commands::{CONTRACT_VERSION, CommandName, LogoutCommandOutput},
    status::RepairCommand,
};
use bowline_local::device_keys::DeviceKeyStore;
use serde::Serialize;
use std::process::ExitCode;

use crate::{
    EXIT_RUNTIME, generated_at, print_runtime_error, render_logout_human, runtime, write_json_line,
};

/// Where a revocable account session was found. Logout reports the source in its
/// failure message, so it is an enum rather than a bare label string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionSource {
    Stored,
    EnvironmentProvided,
    Persisted,
}

impl SessionSource {
    fn label(self) -> &'static str {
        match self {
            Self::Stored => "stored",
            Self::EnvironmentProvided => "environment-provided",
            Self::Persisted => "persisted",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogoutRunOutput {
    pub output: LogoutCommandOutput,
    /// The remote session bowline could not revoke, if any. Local credentials are
    /// gone either way — a machine must always be able to sign itself out — so
    /// this is reported, never a reason to keep the tokens on disk.
    pub unrevoked_remote_session: Option<String>,
}

impl LogoutRunOutput {
    fn json_payload(&self) -> LogoutJsonPayload<'_> {
        LogoutJsonPayload {
            output: &self.output,
            remote_session_still_active: self.unrevoked_remote_session.is_some(),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogoutJsonPayload<'a> {
    #[serde(flatten)]
    output: &'a LogoutCommandOutput,
    remote_session_still_active: bool,
}

pub fn run(generated_at: String) -> Result<LogoutRunOutput, String> {
    let key_store = runtime::key_store()?;
    let stored_account_session = runtime::stored_account_session_revocation(&*key_store)?;
    let environment_account_session = runtime::environment_account_session_revocation()?;
    let persisted_account_session = runtime::persisted_account_session_revocation()?;
    run_with(
        generated_at,
        &*key_store,
        stored_account_session,
        environment_account_session,
        persisted_account_session,
        runtime::revoke_account_session,
        runtime::clear_persisted_account_session,
    )
}

fn run_with<F, C>(
    generated_at: String,
    key_store: &dyn DeviceKeyStore,
    stored_account_session: Option<runtime::AccountSessionRevocation>,
    environment_account_session: Option<runtime::AccountSessionRevocation>,
    persisted_account_session: Option<runtime::AccountSessionRevocation>,
    mut revoke_account_session: F,
    clear_persisted_account_session: C,
) -> Result<LogoutRunOutput, String>
where
    F: FnMut(&str, &str) -> Result<(), String>,
    C: FnOnce() -> Result<bool, String>,
{
    let sessions = [
        (SessionSource::Stored, stored_account_session.as_ref()),
        (
            SessionSource::EnvironmentProvided,
            environment_account_session.as_ref(),
        ),
        (SessionSource::Persisted, persisted_account_session.as_ref()),
    ];
    // Remote revocation is best effort. A machine that cannot reach the control
    // plane must still be able to remove its own credentials, so a failure here
    // is reported rather than allowed to strand the tokens on disk.
    let revocation = revoke_sessions(&sessions, &mut revoke_account_session);
    let cleared_persisted_session = clear_persisted_account_session()?;
    let cleared_local_login = key_store
        .clear_account_tokens()
        .map_err(|error| error.to_string())?;
    let mut next_actions = Vec::new();
    if let Some(failure) = &revocation.failure {
        next_actions.push(RepairCommand::inspect(
            format!(
                "Revoke the {} account session from another signed-in device",
                failure.source.label()
            ),
            Some("bowline device list --json".to_string()),
        ));
    }
    next_actions.push(RepairCommand::inspect(
        "Sign in again".to_string(),
        Some("bowline login".to_string()),
    ));
    Ok(LogoutRunOutput {
        output: LogoutCommandOutput {
            contract_version: CONTRACT_VERSION,
            command: CommandName::Logout,
            generated_at,
            signed_out: revocation.revoked_remote_session
                || cleared_persisted_session
                || cleared_local_login,
            next_actions,
        },
        unrevoked_remote_session: revocation.failure.map(|failure| failure.message),
    })
}

#[derive(Debug)]
struct RevocationFailure {
    source: SessionSource,
    message: String,
}

#[derive(Debug)]
struct RevocationOutcome {
    revoked_remote_session: bool,
    failure: Option<RevocationFailure>,
}

fn revoke_sessions<F>(
    sessions: &[(SessionSource, Option<&runtime::AccountSessionRevocation>)],
    revoke_account_session: &mut F,
) -> RevocationOutcome
where
    F: FnMut(&str, &str) -> Result<(), String>,
{
    let mut revoked_session_ids = Vec::new();
    let mut failure = None;
    for (source, session) in sessions {
        let Some(session) = session else {
            continue;
        };
        if revoked_session_ids.contains(&session.session_id) {
            continue;
        }
        let mut attempted_tokens = Vec::new();
        let mut last_error = None;
        for (_, candidate) in sessions {
            let Some(candidate) = candidate.filter(|candidate| {
                candidate.session_id == session.session_id
                    && !attempted_tokens.contains(&candidate.revocation_token)
            }) else {
                continue;
            };
            attempted_tokens.push(candidate.revocation_token.clone());
            match revoke_account_session(&candidate.session_id, &candidate.revocation_token) {
                Ok(()) => {
                    last_error = None;
                    break;
                }
                Err(error) => last_error = Some(error),
            }
        }
        match last_error {
            Some(error) => {
                failure.get_or_insert(RevocationFailure {
                    source: *source,
                    message: format!(
                        "could not revoke the {} account session: {error}",
                        source.label()
                    ),
                });
            }
            None => revoked_session_ids.push(session.session_id.clone()),
        }
    }
    RevocationOutcome {
        revoked_remote_session: !revoked_session_ids.is_empty(),
        failure,
    }
}

pub(super) fn print_logout(json: bool) -> ExitCode {
    let generated_at = generated_at();
    match run(generated_at.clone()) {
        Ok(output) if json => {
            if let Err(error) = write_json_line(&output.json_payload()) {
                print_runtime_error(CommandName::Logout, generated_at, &error.to_string(), true);
                return ExitCode::from(EXIT_RUNTIME);
            }
            ExitCode::SUCCESS
        }
        Ok(output) => {
            print!("{}", render_logout_human(&output.output));
            if let Some(message) = &output.unrevoked_remote_session {
                eprintln!("Signed out on this machine, but {message}");
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error(CommandName::Logout, generated_at, &error, json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bowline_core::ids::AccountId;
    use bowline_local::{
        device_keys::{AccountSessionCredentials, AccountTokens, DeviceKeyStore},
        fakes::FakeKeychain,
    };

    /// Offline, or during a control-plane outage, sign-out must still remove the
    /// credentials from this machine; the unrevoked remote session is reported.
    #[test]
    fn failed_remote_revocation_still_clears_the_local_login() {
        let store = FakeKeychain::default();
        store
            .store_account_tokens(account_tokens())
            .expect("store account tokens");

        let output = run_with(
            "2026-07-15T12:00:00Z".to_string(),
            &store,
            Some(session(
                "bowline_session_existing",
                "bowline_revoke_existing",
            )),
            None,
            None,
            |_, _| Err("control plane unavailable".to_string()),
            || Ok(false),
        )
        .expect("logout removes local credentials even when the remote call fails");

        let message = output
            .unrevoked_remote_session
            .as_deref()
            .expect("the unrevoked session is reported");
        assert!(message.contains("control plane unavailable"));
        assert!(message.contains("stored"));
        assert!(output.output.signed_out);
        assert!(
            store
                .load_account_tokens()
                .expect("load account tokens")
                .is_none()
        );
        let json = serde_json::to_value(output.json_payload()).expect("payload serializes");
        assert_eq!(json["remoteSessionStillActive"], true);
        assert_eq!(json["signedOut"], true);
    }

    #[test]
    fn successful_logout_reports_no_active_remote_session() {
        let store = FakeKeychain::default();

        let output = run_with(
            "2026-07-15T12:00:00Z".to_string(),
            &store,
            Some(session(
                "bowline_session_existing",
                "bowline_revoke_existing",
            )),
            None,
            None,
            |_, _| Ok(()),
            || Ok(false),
        )
        .expect("logout succeeds");

        assert!(output.unrevoked_remote_session.is_none());
        let json = serde_json::to_value(output.json_payload()).expect("payload serializes");
        assert_eq!(json["remoteSessionStillActive"], false);
    }

    #[test]
    fn successful_remote_revocation_clears_the_local_login() {
        let store = FakeKeychain::default();
        store
            .store_account_tokens(account_tokens())
            .expect("store account tokens");

        let output = run_with(
            "2026-07-15T12:00:00Z".to_string(),
            &store,
            Some(session(
                "bowline_session_existing",
                "bowline_revoke_existing",
            )),
            None,
            None,
            |session_id, revocation_token| {
                assert_eq!(session_id, "bowline_session_existing");
                assert_eq!(revocation_token, "bowline_revoke_existing");
                Ok(())
            },
            || Ok(false),
        )
        .expect("logout succeeds");

        assert!(output.output.signed_out);
        assert!(
            store
                .load_account_tokens()
                .expect("load account tokens")
                .is_none()
        );
    }

    #[test]
    fn environment_override_does_not_hide_the_stored_session_from_logout() {
        let store = FakeKeychain::default();
        store
            .store_account_tokens(account_tokens())
            .expect("store account tokens");
        let mut revoked = Vec::new();

        let output = run_with(
            "2026-07-15T12:00:00Z".to_string(),
            &store,
            Some(session(
                "bowline_session_existing",
                "bowline_revoke_existing",
            )),
            Some(session(
                "bowline_session_environment",
                "bowline_revoke_environment",
            )),
            None,
            |session_id, revocation_token| {
                revoked.push((session_id.to_string(), revocation_token.to_string()));
                Ok(())
            },
            || Ok(false),
        )
        .expect("logout succeeds");

        assert_eq!(
            revoked,
            vec![
                (
                    "bowline_session_existing".to_string(),
                    "bowline_revoke_existing".to_string(),
                ),
                (
                    "bowline_session_environment".to_string(),
                    "bowline_revoke_environment".to_string(),
                ),
            ]
        );
        assert!(output.output.signed_out);
        assert!(
            store
                .load_account_tokens()
                .expect("load account tokens")
                .is_none()
        );
    }

    #[test]
    fn environment_only_session_is_revoked_and_reported_signed_out() {
        let store = FakeKeychain::default();
        let mut revoked = Vec::new();

        let output = run_with(
            "2026-07-15T12:00:00Z".to_string(),
            &store,
            None,
            Some(session(
                "bowline_session_environment",
                "bowline_revoke_environment",
            )),
            None,
            |session_id, revocation_token| {
                revoked.push((session_id.to_string(), revocation_token.to_string()));
                Ok(())
            },
            || Ok(false),
        )
        .expect("logout succeeds");

        assert_eq!(
            revoked,
            vec![(
                "bowline_session_environment".to_string(),
                "bowline_revoke_environment".to_string(),
            )]
        );
        assert!(output.output.signed_out);
    }

    #[test]
    fn persisted_session_is_revoked_once_and_removed() {
        let store = FakeKeychain::default();
        let persisted = session("bowline_session_remote", "bowline_revoke_remote");
        let mut revoked = Vec::new();
        let mut cleared = false;

        let output = run_with(
            "2026-07-15T12:00:00Z".to_string(),
            &store,
            None,
            Some(persisted.clone()),
            Some(persisted),
            |session_id, revocation_token| {
                revoked.push((session_id.to_string(), revocation_token.to_string()));
                Ok(())
            },
            || {
                cleared = true;
                Ok(true)
            },
        )
        .expect("logout succeeds");

        assert_eq!(
            revoked,
            vec![(
                "bowline_session_remote".to_string(),
                "bowline_revoke_remote".to_string(),
            )]
        );
        assert!(cleared);
        assert!(output.output.signed_out);
    }

    #[test]
    fn alternate_token_for_the_same_session_can_complete_logout() {
        let store = FakeKeychain::default();
        let mut attempts = Vec::new();

        let output = run_with(
            "2026-07-15T12:00:00Z".to_string(),
            &store,
            None,
            Some(session("bowline_session_remote", "bowline_revoke_stale")),
            Some(session("bowline_session_remote", "bowline_revoke_current")),
            |session_id, revocation_token| {
                attempts.push((session_id.to_string(), revocation_token.to_string()));
                if revocation_token == "bowline_revoke_current" {
                    Ok(())
                } else {
                    Err("revocation token rejected".to_string())
                }
            },
            || Ok(true),
        )
        .expect("current persisted token completes logout");

        assert_eq!(
            attempts,
            vec![
                (
                    "bowline_session_remote".to_string(),
                    "bowline_revoke_stale".to_string(),
                ),
                (
                    "bowline_session_remote".to_string(),
                    "bowline_revoke_current".to_string(),
                ),
            ]
        );
        assert!(output.output.signed_out);
    }

    fn account_tokens() -> AccountTokens {
        AccountTokens {
            account_id: AccountId::new("account_test"),
            access_token: "access-token".to_string(),
            refresh_token: "refresh-token".to_string(),
            expires_at: "2026-07-16T12:00:00Z".to_string(),
            account_session: Some(AccountSessionCredentials {
                session_id: "bowline_session_existing".to_string(),
                revocation_token: "bowline_revoke_existing".to_string(),
            }),
        }
    }

    fn session(session_id: &str, revocation_token: &str) -> runtime::AccountSessionRevocation {
        runtime::AccountSessionRevocation {
            session_id: session_id.to_string(),
            revocation_token: revocation_token.to_string(),
        }
    }
}
