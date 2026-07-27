//! The account session this daemon authenticates hosted calls with.
//!
//! Three places can name a session, and they are not equally current: a
//! replacement this process registered, the `daemon.env` pair the daemon was
//! provisioned with, and the key store's `AccountTokens` record. This module
//! owns that precedence and the writes that keep it true.

use std::error::Error;
use std::fmt;
use std::io;
use std::path::Path;
use std::sync::Mutex;

use bowline_control_plane::HostedControlPlaneClient;
use bowline_control_plane::hosted::RegisteredAccountSession;
use bowline_core::ids::WorkspaceId;
use bowline_local::device_keys::{
    AccountSessionCredentials, DeviceKeyError, DeviceKeyStore, store_account_session_in_daemon_env,
};

use crate::daemon::control_plane::{key_store, require_convex_url, workos_access_token};
use crate::daemon::{daemon_env_var, daemon_state_root};

/// The session this process registered to replace the one it started with.
///
/// `daemon.env` and the ambient environment are *provisioning*: they name the
/// session this daemon was launched with, and a running process cannot rewrite
/// its own environment. A session the control plane has already refused must not
/// come back at the next hosted-context rebuild, so a replacement registered
/// in-process outranks both while the durable copy lands in `daemon.env` for the
/// next start.
static REGISTERED_ACCOUNT_SESSION: Mutex<Option<AccountSessionCredentials>> = Mutex::new(None);

const SESSION_ID_PREFIX: &str = "bowline_session_";
const REVOCATION_TOKEN_PREFIX: &str = "bowline_revoke_";

pub(super) fn account_session_id(key_store: &dyn DeviceKeyStore) -> Option<String> {
    resolve_account_session_id(
        registered_account_session(),
        provisioned_account_session_id(),
        key_store,
    )
}

/// Freshest first. The provisioned session is what this daemon was handed at
/// start; once a replacement has been registered, that pair is spent, so
/// preferring it would re-pin the credential the control plane just refused.
fn resolve_account_session_id(
    registered: Option<AccountSessionCredentials>,
    provisioned: Option<String>,
    key_store: &dyn DeviceKeyStore,
) -> Option<String> {
    registered
        .map(|session| session.session_id)
        .or(provisioned)
        .or_else(|| stored_account_session_id(key_store))
}

fn persistent_account_session_id(session_id: &str) -> bool {
    session_id.starts_with(SESSION_ID_PREFIX)
}

fn registered_account_session() -> Option<AccountSessionCredentials> {
    REGISTERED_ACCOUNT_SESSION.lock().ok()?.clone()
}

/// The pair this daemon was provisioned with. Both halves are required: a
/// session id whose revocation token never arrived is a credential nothing on
/// this host could ever revoke.
fn provisioned_account_session_id() -> Option<String> {
    daemon_env_var("BOWLINE_ACCOUNT_SESSION_ID")
        .filter(|session_id| persistent_account_session_id(session_id))
        .filter(|_| {
            daemon_env_var("BOWLINE_ACCOUNT_SESSION_REVOCATION_TOKEN")
                .is_some_and(|token| token.starts_with(REVOCATION_TOKEN_PREFIX))
        })
}

fn stored_account_session_id(key_store: &dyn DeviceKeyStore) -> Option<String> {
    key_store
        .load_account_tokens()
        .ok()
        .flatten()
        .and_then(|tokens| tokens.account_session.map(|session| session.session_id))
        .filter(|session_id| persistent_account_session_id(session_id))
}

pub(super) fn ensure_persistent_account_session(
    key_store: &dyn DeviceKeyStore,
    workspace_id: &WorkspaceId,
) -> Result<Option<String>, Box<dyn Error>> {
    if let Some(session_id) = account_session_id(key_store) {
        return Ok(Some(session_id));
    }
    // No stored `AccountTokens` is not a reason to stop: a bootstrapped agent
    // host is handed a WorkOS access token and nothing else, and registering a
    // session from it is exactly how it gets a durable credential.
    let Some(access_token) = workos_access_token(key_store) else {
        return Ok(None);
    };
    let client =
        HostedControlPlaneClient::try_new_with_token(require_convex_url()?, String::new())?;
    let registration =
        client.register_account_session(access_token, Some(workspace_id.as_str()))?;
    store_account_session(key_store, &credentials(&registration))?;
    Ok(Some(registration.session_id))
}

/// Records a session the client registered, so the rest of this process stops
/// authenticating with the one the control plane refused and the next process
/// start does not begin with a refused call.
///
/// A failure here costs one refused call after the next restart, not
/// correctness, so it is logged rather than propagated into the caller's
/// control-plane result — but it is never dropped in silence.
pub(super) fn persist_registered_account_session(registration: &RegisteredAccountSession) {
    if let Err(error) = record_registered_account_session(registration) {
        eprintln!("bowline-daemon could not persist a refreshed account session: {error}");
    }
}

fn record_registered_account_session(
    registration: &RegisteredAccountSession,
) -> Result<(), AccountSessionError> {
    let session = credentials(registration);
    // Adopted before the secret store is even opened: a host that cannot reach
    // its secret store must still stop authenticating with the session the
    // control plane has just refused.
    adopt_registered_account_session(&session)?;
    let store = key_store().map_err(AccountSessionError::KeyStore)?;
    store_account_session_durably(daemon_state_root().as_deref(), store.as_ref(), &session)
}

fn credentials(registration: &RegisteredAccountSession) -> AccountSessionCredentials {
    AccountSessionCredentials {
        session_id: registration.session_id.clone(),
        revocation_token: registration.revocation_token.clone(),
    }
}

/// Adopts and persists a session registered eagerly at startup. The reactive
/// replacement path above does the same two steps in the same order, so the
/// recorded credential pair can never come from two different shapes.
fn store_account_session(
    key_store: &dyn DeviceKeyStore,
    session: &AccountSessionCredentials,
) -> Result<(), AccountSessionError> {
    adopt_registered_account_session(session)?;
    store_account_session_durably(daemon_state_root().as_deref(), key_store, session)
}

/// Writes every durable home this host actually has.
///
/// Keying persistence off the `AccountTokens` record alone loses the session on
/// a bootstrapped agent host, which has no such record — only `bowline login`
/// writes one — and losing it means losing the revocation token, leaving nothing
/// on the host able to revoke the session it just created. A host with neither
/// home is an error rather than a silent success, because the caller's only
/// other copy is process memory.
fn store_account_session_durably(
    state_root: Option<&Path>,
    key_store: &dyn DeviceKeyStore,
    session: &AccountSessionCredentials,
) -> Result<(), AccountSessionError> {
    let mut homes = 0_usize;
    if let Some(state_root) = state_root {
        store_account_session_in_daemon_env(state_root, session)
            .map_err(AccountSessionError::DaemonEnv)?;
        homes += 1;
    }
    if let Some(mut tokens) = key_store
        .load_account_tokens()
        .map_err(AccountSessionError::KeyStore)?
    {
        tokens.account_session = Some(session.clone());
        key_store
            .store_account_tokens(tokens)
            .map_err(AccountSessionError::KeyStore)?;
        homes += 1;
    }
    if homes == 0 {
        return Err(AccountSessionError::NoDurableHome);
    }
    Ok(())
}

fn adopt_registered_account_session(
    session: &AccountSessionCredentials,
) -> Result<(), AccountSessionError> {
    *REGISTERED_ACCOUNT_SESSION
        .lock()
        .map_err(|_| AccountSessionError::RegisteredSessionLockPoisoned)? = Some(session.clone());
    Ok(())
}

#[derive(Debug)]
pub(super) enum AccountSessionError {
    DaemonEnv(io::Error),
    KeyStore(DeviceKeyError),
    RegisteredSessionLockPoisoned,
    NoDurableHome,
}

impl fmt::Display for AccountSessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DaemonEnv(error) => {
                write!(formatter, "daemon environment write failed: {error}")
            }
            Self::KeyStore(error) => error.fmt(formatter),
            Self::RegisteredSessionLockPoisoned => {
                formatter.write_str("registered account session lock poisoned")
            }
            Self::NoDurableHome => formatter.write_str(
                "this host has nowhere to keep an account session: no daemon state root and no stored account tokens",
            ),
        }
    }
}

impl Error for AccountSessionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::DaemonEnv(error) => Some(error),
            Self::KeyStore(error) => Some(error),
            Self::RegisteredSessionLockPoisoned | Self::NoDurableHome => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bowline_core::ids::AccountId;
    use bowline_local::device_keys::AccountTokens;
    use bowline_local::fakes::FakeKeychain;
    use std::fs;

    fn session(suffix: &str) -> AccountSessionCredentials {
        AccountSessionCredentials {
            session_id: format!("{SESSION_ID_PREFIX}{suffix}"),
            revocation_token: format!("{REVOCATION_TOKEN_PREFIX}{suffix}"),
        }
    }

    fn account_tokens(account_session: Option<AccountSessionCredentials>) -> AccountTokens {
        AccountTokens {
            account_id: AccountId::new("acct_test"),
            access_token: "access".to_string(),
            refresh_token: "refresh".to_string(),
            expires_at: "later".to_string(),
            account_session,
        }
    }

    fn temp_state_root(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "bowline-account-session-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("state root");
        path
    }

    /// A bootstrapped agent host never runs `bowline login`, so it has no
    /// `AccountTokens` record. Keying persistence off that record dropped every
    /// replacement session — and its revocation token with it.
    #[test]
    fn a_host_without_account_tokens_still_persists_the_replacement_and_its_revocation_token() {
        let state_root = temp_state_root("no-tokens");
        let key_store = FakeKeychain::default();

        store_account_session_durably(Some(&state_root), &key_store, &session("fresh"))
            .expect("a host with a state root has somewhere to persist");

        let persisted = bowline_local::daemon_env::read(&state_root);
        assert_eq!(
            persisted
                .get(bowline_local::daemon_env::ACCOUNT_SESSION_ID_KEY)
                .map(String::as_str),
            Some("bowline_session_fresh")
        );
        assert_eq!(
            persisted
                .get(bowline_local::daemon_env::ACCOUNT_SESSION_REVOCATION_TOKEN_KEY)
                .map(String::as_str),
            Some("bowline_revoke_fresh")
        );
        let _ = fs::remove_dir_all(state_root);
    }

    /// The provisioned pair is what the next start reads, so a replacement has to
    /// overwrite it rather than sit beside it.
    #[test]
    fn a_replacement_overwrites_the_provisioned_pair_in_the_daemon_environment() {
        let state_root = temp_state_root("overwrite");
        let stale = session("stale");
        // Rendered rather than spelled out so the source never carries a line
        // that reads as a real credential assignment.
        fs::write(
            state_root.join(bowline_local::daemon_env::FILE_NAME),
            format!(
                "BOWLINE_DEVICE_ID=device_a\n{}={}\n{}={}\n",
                bowline_local::daemon_env::ACCOUNT_SESSION_ID_KEY,
                stale.session_id,
                bowline_local::daemon_env::ACCOUNT_SESSION_REVOCATION_TOKEN_KEY,
                stale.revocation_token,
            ),
        )
        .expect("provisioned daemon environment");

        store_account_session_durably(
            Some(&state_root),
            &FakeKeychain::default(),
            &session("fresh"),
        )
        .expect("persisted");

        let persisted = bowline_local::daemon_env::read(&state_root);
        assert_eq!(
            persisted
                .get(bowline_local::daemon_env::ACCOUNT_SESSION_ID_KEY)
                .map(String::as_str),
            Some("bowline_session_fresh")
        );
        assert_eq!(
            persisted.get("BOWLINE_DEVICE_ID").map(String::as_str),
            Some("device_a")
        );
        let _ = fs::remove_dir_all(state_root);
    }

    #[test]
    fn a_keychain_host_keeps_the_session_beside_its_account_tokens() {
        let key_store = FakeKeychain::default();
        key_store
            .store_account_tokens(account_tokens(None))
            .expect("seed tokens");

        store_account_session_durably(None, &key_store, &session("fresh")).expect("persisted");

        assert_eq!(
            key_store
                .load_account_tokens()
                .expect("tokens")
                .and_then(|tokens| tokens.account_session),
            Some(session("fresh"))
        );
    }

    /// Answering `Ok(())` with nothing written is how the replacement — and the
    /// only token able to revoke it — used to disappear.
    #[test]
    fn a_host_with_no_durable_home_reports_it_instead_of_answering_ok() {
        let error =
            store_account_session_durably(None, &FakeKeychain::default(), &session("fresh"))
                .expect_err("a host with neither home cannot persist a session");

        assert!(matches!(error, AccountSessionError::NoDurableHome));
    }

    /// The provisioned session is the one the control plane refused. Preferring
    /// it over the replacement is what made every restart burn a refused call
    /// and a fresh session row.
    #[test]
    fn a_replacement_registered_in_process_outranks_the_provisioned_session() {
        let key_store = FakeKeychain::default();
        key_store
            .store_account_tokens(account_tokens(Some(session("stored"))))
            .expect("seed tokens");

        assert_eq!(
            resolve_account_session_id(
                Some(session("fresh")),
                Some("bowline_session_provisioned".to_string()),
                &key_store,
            )
            .as_deref(),
            Some("bowline_session_fresh")
        );
        assert_eq!(
            resolve_account_session_id(
                None,
                Some("bowline_session_provisioned".to_string()),
                &key_store,
            )
            .as_deref(),
            Some("bowline_session_provisioned")
        );
        assert_eq!(
            resolve_account_session_id(None, None, &key_store).as_deref(),
            Some("bowline_session_stored")
        );
    }
}
