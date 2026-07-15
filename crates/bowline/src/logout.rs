use bowline_core::{
    commands::{CONTRACT_VERSION, CommandName, LogoutCommandOutput},
    status::RepairCommand,
};
use bowline_local::device_keys::DeviceKeyStore;
use std::process::ExitCode;

use crate::{
    EXIT_RUNTIME, generated_at, print_json, print_runtime_error, render_logout_human, runtime,
};

pub fn run(generated_at: String) -> Result<LogoutCommandOutput, String> {
    let key_store = runtime::key_store()?;
    let stored_account_session = runtime::stored_account_session_revocation(&*key_store)?;
    let environment_account_session = runtime::environment_account_session_revocation()?;
    run_with(
        generated_at,
        &*key_store,
        stored_account_session,
        environment_account_session,
        runtime::revoke_account_session,
    )
}

fn run_with<F>(
    generated_at: String,
    key_store: &dyn DeviceKeyStore,
    stored_account_session: Option<runtime::AccountSessionRevocation>,
    environment_account_session: Option<runtime::AccountSessionRevocation>,
    mut revoke_account_session: F,
) -> Result<LogoutCommandOutput, String>
where
    F: FnMut(&str, &str) -> Result<(), String>,
{
    let mut revoked_remote_session = false;
    if let Some(session) = stored_account_session.as_ref() {
        revoke_account_session(&session.session_id, &session.revocation_token).map_err(
            |error| {
                format!(
                    "could not revoke the remote account session; local login was kept: {error}"
                )
            },
        )?;
        revoked_remote_session = true;
    }
    if let Some(session) = environment_account_session.as_ref()
        && Some(session.session_id.as_str())
            != stored_account_session
                .as_ref()
                .map(|stored| stored.session_id.as_str())
    {
        revoke_account_session(&session.session_id, &session.revocation_token).map_err(|error| {
            format!(
                "could not revoke the environment-provided account session; local login was kept: {error}"
            )
        })?;
        revoked_remote_session = true;
    }
    let cleared_local_login = key_store
        .clear_account_tokens()
        .map_err(|error| error.to_string())?;
    Ok(LogoutCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::Logout,
        generated_at,
        signed_out: revoked_remote_session || cleared_local_login,
        next_actions: vec![RepairCommand::inspect(
            "Sign in again".to_string(),
            Some("bowline login".to_string()),
        )],
    })
}

pub(super) fn print_logout(json: bool) -> ExitCode {
    let generated_at = generated_at();
    match run(generated_at.clone()) {
        Ok(output) if json => {
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(output) => {
            print!("{}", render_logout_human(&output));
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

    #[test]
    fn failed_remote_revocation_keeps_the_local_login() {
        let store = FakeKeychain::default();
        store
            .store_account_tokens(account_tokens())
            .expect("store account tokens");

        let error = run_with(
            "2026-07-15T12:00:00Z".to_string(),
            &store,
            Some(session(
                "bowline_session_existing",
                "bowline_revoke_existing",
            )),
            None,
            |_, _| Err("control plane unavailable".to_string()),
        )
        .expect_err("logout must fail closed");

        assert!(error.contains("control plane unavailable"));
        assert!(
            store
                .load_account_tokens()
                .expect("load account tokens")
                .is_some()
        );
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
            |session_id, revocation_token| {
                assert_eq!(session_id, "bowline_session_existing");
                assert_eq!(revocation_token, "bowline_revoke_existing");
                Ok(())
            },
        )
        .expect("logout succeeds");

        assert!(output.signed_out);
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
            |session_id, revocation_token| {
                revoked.push((session_id.to_string(), revocation_token.to_string()));
                Ok(())
            },
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
        assert!(output.signed_out);
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
            |session_id, revocation_token| {
                revoked.push((session_id.to_string(), revocation_token.to_string()));
                Ok(())
            },
        )
        .expect("logout succeeds");

        assert_eq!(
            revoked,
            vec![(
                "bowline_session_environment".to_string(),
                "bowline_revoke_environment".to_string(),
            )]
        );
        assert!(output.signed_out);
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
