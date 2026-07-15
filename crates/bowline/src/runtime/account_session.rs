use std::env;

use bowline_control_plane::HostedControlPlaneClient;
use bowline_core::ids::WorkspaceId;
use bowline_local::device_keys::{AccountSessionCredentials, DeviceKeyStore};

use super::{hosted_convex_url, nonempty_env_value, workos_access_token};

pub type AccountSessionRevocation = AccountSessionCredentials;

pub fn account_session_id(store: &dyn DeviceKeyStore) -> Option<String> {
    environment_account_session_revocation()
        .ok()
        .flatten()
        .map(|session| session.session_id)
        .or_else(|| stored_account_session_id(store))
}

fn stored_account_session_id(store: &dyn DeviceKeyStore) -> Option<String> {
    store
        .load_account_tokens()
        .ok()
        .flatten()
        .and_then(|tokens| tokens.account_session.map(|session| session.session_id))
        .filter(|session_id| durable_account_session_id(session_id))
}

pub fn environment_account_session_revocation() -> Result<Option<AccountSessionRevocation>, String>
{
    let session_id = nonempty_env_value(env::var("BOWLINE_ACCOUNT_SESSION_ID").ok());
    let revocation_token =
        nonempty_env_value(env::var("BOWLINE_ACCOUNT_SESSION_REVOCATION_TOKEN").ok());
    match (session_id, revocation_token) {
        (None, None) => Ok(None),
        (Some(session_id), Some(revocation_token))
            if durable_account_session_id(&session_id)
                && durable_revocation_token(&revocation_token) =>
        {
            Ok(Some(AccountSessionRevocation {
                session_id,
                revocation_token,
            }))
        }
        _ => Err(
            "BOWLINE_ACCOUNT_SESSION_ID and BOWLINE_ACCOUNT_SESSION_REVOCATION_TOKEN must be configured together"
                .to_string(),
        ),
    }
}

pub fn stored_account_session_revocation(
    store: &dyn DeviceKeyStore,
) -> Result<Option<AccountSessionRevocation>, String> {
    let Some(tokens) = store
        .load_account_tokens()
        .map_err(|error| error.to_string())?
    else {
        return Ok(None);
    };
    let Some(session) = tokens.account_session else {
        return Ok(None);
    };
    if !durable_account_session_id(&session.session_id)
        || !durable_revocation_token(&session.revocation_token)
    {
        return Ok(None);
    }
    Ok(Some(session))
}

pub fn ensure_durable_account_session(
    store: &dyn DeviceKeyStore,
    workspace_id: Option<&WorkspaceId>,
) -> Result<Option<String>, String> {
    if let Some(session_id) = account_session_id(store) {
        return Ok(Some(session_id));
    }
    if store
        .load_account_tokens()
        .map_err(|error| error.to_string())?
        .is_none()
    {
        return Ok(None);
    };
    let Some(access_token) = workos_access_token(store) else {
        return Ok(None);
    };
    let Some(mut tokens) = store
        .load_account_tokens()
        .map_err(|error| error.to_string())?
    else {
        return Ok(None);
    };
    let convex_url =
        hosted_convex_url().ok_or_else(|| "hosted control plane is missing".to_string())?;
    let client = HostedControlPlaneClient::try_new_with_token(convex_url, String::new())
        .map_err(|error| error.to_string())?;
    let registration = client
        .register_account_session(access_token, workspace_id.map(|id| id.as_str()))
        .map_err(|error| error.to_string())?;
    tokens.account_session = Some(AccountSessionCredentials {
        session_id: registration.session_id.clone(),
        revocation_token: registration.revocation_token,
    });
    store
        .store_account_tokens(tokens)
        .map_err(|error| error.to_string())?;
    Ok(Some(registration.session_id))
}

pub fn revoke_account_session(session_id: &str, revocation_token: &str) -> Result<(), String> {
    let convex_url =
        hosted_convex_url().ok_or_else(|| "hosted control plane is missing".to_string())?;
    HostedControlPlaneClient::try_new_with_token(convex_url, String::new())
        .map_err(|error| error.to_string())?
        .revoke_account_session(session_id, revocation_token)
        .map_err(|error| error.to_string())
}

fn durable_account_session_id(session_id: &str) -> bool {
    session_id.starts_with("bowline_session_")
}

fn durable_revocation_token(token: &str) -> bool {
    token.starts_with("bowline_revoke_")
}
