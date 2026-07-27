use std::error::Error;
use std::sync::Arc;
use std::{fmt, io};

use bowline_control_plane::{ControlPlaneError, HostedControlPlaneClient};
use bowline_core::hosted::{DEFAULT_CONVEX_URL, DEFAULT_WORKOS_CLIENT_ID};
use bowline_core::ids::{DeviceId, WorkspaceId};
use bowline_daemon::device_trust::{TrustRefreshError, VerifierStore, WorkspaceDeviceTrust};
use bowline_local::account::workos;
use bowline_local::device_keys::{
    DeviceKeyError, DeviceKeyStore, DeviceProofVerifier, default_device_key_store,
};
use bowline_local::trust::grants;
use time::OffsetDateTime;

use crate::daemon::account_session::{
    account_session_id, ensure_persistent_account_session, persist_registered_account_session,
};
use crate::daemon::daemon_env_var;

#[derive(Clone, PartialEq, Eq)]
pub(super) struct DaemonCredentials {
    pub(super) deployment_url: String,
    pub(super) control_plane_token: Option<String>,
    pub(super) account_session_id: Option<String>,
    pub(super) workos_access_token: Option<String>,
}

impl fmt::Debug for DaemonCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DaemonCredentials")
            .field("deployment_url", &self.deployment_url)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub(super) struct ResolvedHostedContext {
    pub(super) credentials: DaemonCredentials,
    pub(super) identity: bowline_local::device_keys::DeviceIdentity,
    pub(super) verifiers: Vec<DeviceProofVerifier>,
}

pub(super) struct BuiltHostedControlPlane {
    pub(super) client: HostedControlPlaneClient,
    pub(super) installed_verifiers: Vec<DeviceProofVerifier>,
    /// The verifier set this client resolves through, still writable. Held by
    /// the caller so the ref observer can teach a running client about a device
    /// trusted after it was built.
    pub(super) trust: Arc<WorkspaceDeviceTrust>,
}

pub(super) fn resolve_daemon_credentials(
    key_store: &dyn DeviceKeyStore,
    workspace_id: &WorkspaceId,
) -> Result<DaemonCredentials, HostedSetupError> {
    let deployment_url =
        require_convex_url().map_err(|_| HostedSetupError::HostedConfigUnavailable)?;
    let account_session_id = account_session_id(key_store).or_else(|| {
        ensure_persistent_account_session(key_store, workspace_id)
            .ok()
            .flatten()
    });
    daemon_credentials(
        deployment_url,
        daemon_env_var("BOWLINE_CONTROL_PLANE_TOKEN"),
        account_session_id,
        || workos_access_token(key_store),
    )
}

/// Decide which credentials this daemon runs with.
///
/// `BOWLINE_CONTROL_PLANE_TOKEN` is deliberately absent from the decision: it
/// authenticates the deployment, never the account, and every workspace-ref,
/// event, and ref-history call requires a verified account session. A host
/// holding only the operator token can never sync, so it is refused here rather
/// than handed a client that looks configured and fails every read — and the
/// token never suppresses an account credential it cannot replace.
fn daemon_credentials(
    deployment_url: String,
    control_plane_token: Option<String>,
    account_session_id: Option<String>,
    // Resolving a WorkOS token can refresh it over the network, so it is only
    // asked for when there is no session to authenticate with. The built client
    // attaches a provider that resolves one on demand either way, so this
    // laziness never removes a credential from the client.
    resolve_workos_access_token: impl FnOnce() -> Option<String>,
) -> Result<DaemonCredentials, HostedSetupError> {
    let workos_access_token = if account_session_id.is_some() {
        None
    } else {
        resolve_workos_access_token()
    };
    if account_session_id.is_none() && workos_access_token.is_none() {
        return Err(HostedSetupError::AccountLoginRequired);
    }
    Ok(DaemonCredentials {
        deployment_url,
        control_plane_token,
        account_session_id,
        workos_access_token,
    })
}

pub(super) fn resolve_hosted_context(
    key_store: &dyn DeviceKeyStore,
    workspace_id: &WorkspaceId,
) -> Result<ResolvedHostedContext, HostedSetupError> {
    Ok(ResolvedHostedContext {
        credentials: resolve_daemon_credentials(key_store, workspace_id)?,
        identity: key_store.load_or_create_device_identity()?,
        verifiers: key_store.load_device_proof_verifiers()?,
    })
}

pub(super) fn hosted_control_plane(
    key_store: &dyn DeviceKeyStore,
    workspace_id: WorkspaceId,
    device_id: DeviceId,
) -> Result<HostedControlPlaneClient, HostedSetupError> {
    let resolved = resolve_hosted_context(key_store, &workspace_id)?;
    Ok(build_hosted_control_plane(workspace_id, device_id, resolved)?.client)
}

/// Build this host's hosted client for one workspace, with device trust wired
/// so it stays current for as long as the client lives.
pub(super) fn build_hosted_control_plane(
    workspace_id: WorkspaceId,
    device_id: DeviceId,
    resolved: ResolvedHostedContext,
) -> Result<BuiltHostedControlPlane, HostedSetupError> {
    let ResolvedHostedContext {
        credentials,
        identity,
        verifiers,
    } = resolved;
    let signer_device_id = device_id.clone();
    let signer_workspace_id = workspace_id.clone();
    let verifier_identity = identity.clone();
    let verifier_device_id = device_id.clone();
    let mut verifier_cache = verifiers
        .into_iter()
        .map(|verifier| {
            (
                (Some(verifier.workspace_id), verifier.device_id),
                verifier.proof_verifier,
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    verifier_cache.insert(
        (Some(workspace_id.clone()), verifier_device_id),
        grants::device_authorization_proof_verifier(&verifier_identity)
            .map_err(HostedSetupError::Grant)?,
    );
    let mut client = HostedControlPlaneClient::try_new_with_token(
        credentials.deployment_url,
        credentials.control_plane_token.unwrap_or_default(),
    )?
    .with_device_id(device_id)
    .with_device_proof_signer(move |workspace_id, proof_device_id, action, subject| {
        if workspace_id != &signer_workspace_id {
            return Err(ControlPlaneError::Internal {
                reason: "daemon refused to sign a device proof for a different workspace",
            });
        }
        if proof_device_id != &signer_device_id {
            return Err(ControlPlaneError::Internal {
                reason: "daemon refused to sign a device proof for a different device id",
            });
        }
        grants::device_authorization_proof(
            &identity,
            &signer_workspace_id,
            &signer_device_id,
            action,
            subject,
        )
        .map_err(|error| {
            // The typed error carries only a static reason, so the concrete
            // signing failure is logged here rather than dropped.
            eprintln!("bowline-daemon device authorization proof signing failed: {error}");
            ControlPlaneError::Internal {
                reason: "daemon could not sign a device authorization proof",
            }
        })
    });
    // A provider, not the token resolved at build time: this client outlives any
    // single WorkOS access token, and a session the control plane refuses can
    // only be replaced while a live credential is reachable. Attached whatever
    // else this host holds — the operator token authenticates the deployment,
    // not the account, so it can never stand in for these.
    client = client
        .with_workos_access_token_provider(current_workos_access_token)
        .with_account_session_sink(persist_registered_account_session);
    if let Some(session_id) = credentials.account_session_id.as_ref() {
        client = client.with_account_session_id(session_id.clone());
    }
    // A client with no account credential is refused every hosted call, so it
    // must never escape construction looking configured.
    if !client.can_authenticate_account_calls() {
        return Err(HostedSetupError::AccountLoginRequired);
    }
    // Shared, not moved: the resolver installed below reads this map for as long
    // as the client lives, and the ref observer writes to it when a device is
    // trusted after this point. A snapshot here is what left an already-running
    // daemon unable to verify anything the new device pushed.
    let trust = WorkspaceDeviceTrust::new(workspace_id.clone(), verifier_cache, verifier_store());
    let authoritative_verifiers = trust.refresh(&client)?;
    let installed_verifiers = trust.installed_verifiers()?;
    let client = client.with_device_proof_verifier_resolver(trust.resolver());
    debug_assert!(authoritative_verifiers.iter().all(|verifier| {
        installed_verifiers
            .iter()
            .any(|installed| installed == verifier)
    }));
    Ok(BuiltHostedControlPlane {
        client,
        installed_verifiers,
        trust,
    })
}

/// Persist refreshed device trust in this host's key store. Opened per write
/// rather than captured because the key store trait is not `Send + Sync` and
/// this runs on whichever thread learned the trust.
fn verifier_store() -> VerifierStore {
    Arc::new(|workspace_id, verifiers| {
        key_store()?.replace_device_proof_verifiers_for_workspace(workspace_id, verifiers.to_vec())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bowline_control_plane::{AuthorizedDeviceRecord, ControlPlaneTimestamp};
    use bowline_daemon::device_trust::AuthorizedDeviceSource;
    use bowline_local::device_keys::DeviceProofVerifierCache;
    use bowline_local::fakes::FakeKeychain;
    use std::cell::Cell;

    const TEST_DEPLOYMENT_URL: &str = "https://example.convex.cloud";

    #[test]
    fn an_operator_token_alone_is_refused_with_the_command_that_fixes_it() {
        let error = daemon_credentials(
            TEST_DEPLOYMENT_URL.to_string(),
            Some("operator-token".to_string()),
            None,
            || None,
        )
        .expect_err("an operator token cannot authenticate an account call");
        assert!(matches!(error, HostedSetupError::AccountLoginRequired));
        assert!(
            error.to_string().contains("bowline login"),
            "the refusal must name the command that fixes it: {error}"
        );
    }

    #[test]
    fn an_operator_token_does_not_suppress_the_account_credential() {
        let credentials = daemon_credentials(
            TEST_DEPLOYMENT_URL.to_string(),
            Some("operator-token".to_string()),
            None,
            || Some("workos-access-token".to_string()),
        )
        .expect("a host holding both credentials is configured");
        assert_eq!(
            credentials.workos_access_token.as_deref(),
            Some("workos-access-token")
        );
        assert_eq!(
            credentials.control_plane_token.as_deref(),
            Some("operator-token")
        );
    }

    #[test]
    fn a_persisted_session_is_enough_and_never_forces_a_workos_refresh() {
        let resolved_workos = Cell::new(false);
        let credentials = daemon_credentials(
            TEST_DEPLOYMENT_URL.to_string(),
            None,
            Some("bowline_session_persisted".to_string()),
            || {
                resolved_workos.set(true);
                Some("workos-access-token".to_string())
            },
        )
        .expect("a persisted session is enough");
        assert_eq!(
            credentials.account_session_id.as_deref(),
            Some("bowline_session_persisted")
        );
        assert!(
            !resolved_workos.get(),
            "resolving a WorkOS token can hit the network, so a session must skip it"
        );
    }

    #[test]
    fn a_host_with_no_account_credential_at_all_is_refused() {
        let error = daemon_credentials(TEST_DEPLOYMENT_URL.to_string(), None, None, || None)
            .expect_err("an unconfigured host cannot sync");
        assert!(matches!(error, HostedSetupError::AccountLoginRequired));
    }

    /// A refresh is authoritative in both directions, and it lands in this
    /// host's key store: a replaced verifier overwrites the old one and a
    /// revoked device leaves nothing behind for the next process to trust.
    #[test]
    fn a_refresh_persists_replacement_and_revocation_through_the_key_store() {
        let key_store = Arc::new(FakeKeychain::default());
        let workspace_id = WorkspaceId::new("workspace_test");
        let sibling_device_id = DeviceId::new("device_sibling");
        key_store
            .store_device_proof_verifier(DeviceProofVerifier {
                workspace_id: workspace_id.clone(),
                device_id: sibling_device_id.clone(),
                proof_verifier: "dapv_old".to_string(),
            })
            .expect("seed verifier");
        let stored = Arc::clone(&key_store);
        let trust = WorkspaceDeviceTrust::new(
            workspace_id.clone(),
            DeviceProofVerifierCache::new(),
            Arc::new(move |workspace_id, verifiers| {
                stored
                    .replace_device_proof_verifiers_for_workspace(workspace_id, verifiers.to_vec())
            }),
        );

        let control_plane = FakeAuthorizedDevices::authorizing(&[(
            sibling_device_id.clone(),
            "dapv_p256_v1_sibling".to_string(),
        )]);
        let replacement = trust.refresh(&control_plane).expect("replacement refresh");
        assert_eq!(
            key_store
                .load_device_proof_verifiers()
                .expect("replacement persisted"),
            replacement
        );

        let control_plane = FakeAuthorizedDevices::authorizing(&[]);
        trust.refresh(&control_plane).expect("revocation refresh");
        assert!(
            key_store
                .load_device_proof_verifiers()
                .expect("revocation persisted")
                .is_empty()
        );
    }

    /// The devices a workspace authorizes, as the control plane would report
    /// them.
    struct FakeAuthorizedDevices {
        authorized: Vec<(DeviceId, String)>,
    }

    impl FakeAuthorizedDevices {
        fn authorizing(authorized: &[(DeviceId, String)]) -> Self {
            Self {
                authorized: authorized.to_vec(),
            }
        }
    }

    impl AuthorizedDeviceSource for FakeAuthorizedDevices {
        fn authorized_devices(
            &self,
            workspace_id: &WorkspaceId,
        ) -> bowline_control_plane::ControlPlaneResult<Vec<AuthorizedDeviceRecord>> {
            Ok(self
                .authorized
                .iter()
                .map(|(device_id, proof_verifier)| AuthorizedDeviceRecord {
                    workspace_id: workspace_id.clone(),
                    device_id: device_id.clone(),
                    device_name: device_id.as_str().to_string(),
                    platform: "linux".to_string(),
                    device_fingerprint: format!("fingerprint_{}", device_id.as_str()),
                    authorized_at: ControlPlaneTimestamp { tick: 1 },
                    authorized_by_device_id: Some(DeviceId::new("device_approver")),
                    device_authorization_proof_verifier: Some(proof_verifier.clone()),
                    revoked_at: None,
                })
                .collect())
        }
    }
}

#[derive(Debug)]
pub(super) enum HostedSetupError {
    HostedConfigUnavailable,
    AccountLoginRequired,
    DeviceKeys(DeviceKeyError),
    DeviceTrust(TrustRefreshError),
    Grant(grants::GrantError),
    Client(ControlPlaneError),
    CachePoisoned,
    ContextChangedDuringBuild,
}

impl fmt::Display for HostedSetupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HostedConfigUnavailable => {
                formatter.write_str("CONVEX_URL is required for daemon sync")
            }
            Self::AccountLoginRequired => formatter.write_str(
                "daemon sync needs a signed-in Bowline account on this host: run `bowline login --headless`. BOWLINE_CONTROL_PLANE_TOKEN authenticates the deployment, not the account, so it cannot stand in for one",
            ),
            Self::DeviceKeys(error) => error.fmt(formatter),
            Self::DeviceTrust(error) => error.fmt(formatter),
            Self::Grant(error) => error.fmt(formatter),
            Self::Client(error) => error.fmt(formatter),
            Self::CachePoisoned => formatter.write_str("hosted context cache lock poisoned"),
            Self::ContextChangedDuringBuild => {
                formatter.write_str("hosted context inputs changed during construction")
            }
        }
    }
}

impl Error for HostedSetupError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::HostedConfigUnavailable
            | Self::AccountLoginRequired
            | Self::CachePoisoned
            | Self::ContextChangedDuringBuild => None,
            Self::DeviceKeys(error) => Some(error),
            Self::DeviceTrust(error) => Some(error),
            Self::Grant(error) => Some(error),
            Self::Client(error) => Some(error),
        }
    }
}

impl From<ControlPlaneError> for HostedSetupError {
    fn from(error: ControlPlaneError) -> Self {
        Self::Client(error)
    }
}

impl From<TrustRefreshError> for HostedSetupError {
    fn from(error: TrustRefreshError) -> Self {
        Self::DeviceTrust(error)
    }
}

impl From<DeviceKeyError> for HostedSetupError {
    fn from(error: DeviceKeyError) -> Self {
        Self::DeviceKeys(error)
    }
}

pub(super) fn workos_access_token(key_store: &dyn DeviceKeyStore) -> Option<String> {
    if let Some(token) = daemon_env_var("BOWLINE_WORKOS_ACCESS_TOKEN")
        && workos_token_is_not_expired(&token)
    {
        return Some(token);
    }
    if let Some(token) = refresh_env_workos_token(key_store) {
        return Some(token);
    }
    let tokens = key_store.load_account_tokens().ok().flatten()?;
    if workos_token_is_not_expired(&tokens.access_token) {
        return Some(tokens.access_token);
    }
    let client_id = daemon_env_var("BOWLINE_WORKOS_CLIENT_ID")
        .unwrap_or_else(|| DEFAULT_WORKOS_CLIENT_ID.to_string());
    workos::refresh_and_store(key_store, &client_id, &tokens.refresh_token)
        .ok()
        .map(|tokens| tokens.access_token)
}

pub(super) fn refresh_env_workos_token(key_store: &dyn DeviceKeyStore) -> Option<String> {
    let client_id = daemon_env_var("BOWLINE_WORKOS_CLIENT_ID")
        .unwrap_or_else(|| DEFAULT_WORKOS_CLIENT_ID.to_string());
    let refresh_token = daemon_env_var("BOWLINE_WORKOS_REFRESH_TOKEN")?;
    workos::refresh_and_store(key_store, &client_id, &refresh_token)
        .ok()
        .map(|tokens| tokens.access_token)
}

pub(super) fn workos_token_is_not_expired(token: &str) -> bool {
    let Some(payload) = token.split('.').nth(1) else {
        return true;
    };
    let Some(bytes) = decode_base64url(payload) else {
        return true;
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return true;
    };
    let Some(exp) = value.get("exp").and_then(|value| value.as_i64()) else {
        return true;
    };
    exp > OffsetDateTime::now_utc().unix_timestamp() + 30
}

pub(super) fn decode_base64url(input: &str) -> Option<Vec<u8>> {
    let mut bits = 0_u32;
    let mut bit_count = 0_u8;
    let mut output = Vec::new();
    for byte in input.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            b'=' => break,
            _ => return None,
        } as u32;
        bits = (bits << 6) | value;
        bit_count += 6;
        if bit_count >= 8 {
            bit_count -= 8;
            output.push(((bits >> bit_count) & 0xff) as u8);
        }
    }
    Some(output)
}

pub(super) fn require_convex_url() -> Result<String, Box<dyn std::error::Error>> {
    Ok(daemon_env_var("CONVEX_URL").unwrap_or_else(|| DEFAULT_CONVEX_URL.to_string()))
}

pub(super) fn key_store() -> Result<Box<dyn DeviceKeyStore>, DeviceKeyError> {
    default_device_key_store()
}

/// A currently valid WorkOS access token, refreshing through the stored refresh
/// token when the cached one has expired. Answers `None` when this host has been
/// signed out, which is the one case a daemon cannot recover from on its own.
fn current_workos_access_token() -> Option<String> {
    workos_access_token(key_store().ok()?.as_ref())
}

pub(super) fn workspace_key_bytes(bytes: &[u8]) -> Result<[u8; 32], Box<dyn std::error::Error>> {
    bytes
        .try_into()
        .map_err(|_| runtime_error("workspace key material must be exactly 32 bytes"))
}

pub(super) fn runtime_error(message: impl Into<String>) -> Box<dyn std::error::Error> {
    Box::new(io::Error::other(message.into()))
}
