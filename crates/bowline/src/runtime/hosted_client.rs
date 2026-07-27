use std::{collections::BTreeMap, env};

use bowline_control_plane::{
    ControlPlaneClient, ControlPlaneError, FakeControlPlaneClient, HostedControlPlaneClient,
};
use bowline_local::{device_keys::DeviceKeyStore, trust::grants};

use super::{
    account_session_id, active_workspace_id, device_id, ensure_durable_account_session,
    environment_account_session_revocation, hosted_convex_url, key_store,
    local_accepted_workspace_id, nonempty_env_value, workos_access_token,
};

const MISSING_CONFIGURATION: &str = "control-plane configuration is missing; run `bowline setup --root <path>` or set CONVEX_URL and BOWLINE_CONTROL_PLANE_TOKEN";

pub fn control_plane() -> Result<Box<dyn ControlPlaneClient>, String> {
    let bootstrap_token = nonempty_env_value(env::var("BOWLINE_BOOTSTRAP_TOKEN").ok());
    if let Some(convex_url) = hosted_convex_url() {
        // A half-configured account-session pair has a name; letting it fall
        // through to "configuration is missing" sends the operator hunting for
        // the wrong thing on a host whose session was almost right.
        environment_account_session_revocation()?;
        let store = key_store()?;
        let credentials = HostedCredentials::resolve(bootstrap_token, &*store);
        if credentials.is_configured() {
            return Ok(Box::new(credentials.into_client(convex_url, &*store)?));
        }
    }

    if fake_control_plane_enabled() {
        return Ok(Box::new(FakeControlPlaneClient::default()));
    }

    Err(MISSING_CONFIGURATION.to_string())
}

/// Every credential this process can present to the hosted control plane.
///
/// The bootstrap token and the account session authenticate different halves of
/// device enrolment, so they are additive rather than exclusive. Enrolment calls
/// (`createPendingDevice`, `getEncryptedGrant`, `confirmGrantAccepted`) have a
/// bootstrap-token form; the account-scoped reads around them — `listDeviceTrust`
/// above all — do not. `bowline device accept` makes both kinds in one command,
/// so a client that dropped either one strands the remote mid-enrolment.
struct HostedCredentials {
    bootstrap_token: Option<String>,
    control_plane_token: Option<String>,
    account_session_id: Option<String>,
    workos_access_token: Option<String>,
    has_stored_account: bool,
}

impl HostedCredentials {
    fn resolve(bootstrap_token: Option<String>, store: &dyn DeviceKeyStore) -> Self {
        let account_session_id = account_session_id(store).or_else(|| {
            ensure_durable_account_session(store, Some(&active_workspace_id()))
                .ok()
                .flatten()
        });
        // A durable session already authenticates every account call; asking the
        // client to also carry a WorkOS token would only add a second credential
        // for it to choose between.
        let workos_access_token = if account_session_id.is_some() {
            None
        } else {
            workos_access_token(store)
        };
        Self {
            bootstrap_token,
            control_plane_token: nonempty_env_value(env::var("BOWLINE_CONTROL_PLANE_TOKEN").ok()),
            account_session_id,
            workos_access_token,
            has_stored_account: store.load_account_tokens().ok().flatten().is_some(),
        }
    }

    fn is_configured(&self) -> bool {
        self.bootstrap_token.is_some()
            || self.control_plane_token.is_some()
            || self.account_session_id.is_some()
            || self.workos_access_token.is_some()
            || self.has_stored_account
            || explicit_workspace_id_configured()
            || local_accepted_workspace_id().is_some()
    }

    fn into_client(
        self,
        convex_url: String,
        store: &dyn DeviceKeyStore,
    ) -> Result<HostedControlPlaneClient, String> {
        let mut client = hosted_client_with_device_proof(
            convex_url,
            self.control_plane_token.unwrap_or_default(),
            store,
        )?;
        if let Some(bootstrap_token) = self.bootstrap_token {
            client = client.with_bootstrap_token(bootstrap_token);
        }
        if let Some(access_token) = self.workos_access_token {
            client = client.with_workos_access_token(access_token);
        }
        if let Some(session_id) = self.account_session_id {
            client = client.with_account_session_id(session_id);
        }
        Ok(client)
    }
}

fn hosted_client_with_device_proof(
    convex_url: String,
    control_plane_token: String,
    store: &dyn DeviceKeyStore,
) -> Result<HostedControlPlaneClient, String> {
    let device_id = device_id();
    let identity = store
        .load_or_create_device_identity()
        .map_err(|error| error.to_string())?;
    let signer_device_id = device_id.clone();
    let verifier_device_id = device_id.clone();
    let verifier_identity = identity.clone();
    let mut verifier_cache = store
        .load_device_proof_verifiers()
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|verifier| {
            (
                (Some(verifier.workspace_id), verifier.device_id),
                verifier.proof_verifier,
            )
        })
        .collect::<BTreeMap<_, _>>();
    verifier_cache.insert(
        (None, verifier_device_id),
        grants::device_authorization_proof_verifier(&verifier_identity)
            .map_err(|error| error.to_string())?,
    );
    HostedControlPlaneClient::try_new_with_token(convex_url, control_plane_token)
        .map_err(|error| error.to_string())
        .map(|client| {
            client
                .with_device_id(device_id.clone())
                .with_device_proof_signer(move |workspace_id, proof_device_id, action, subject| {
                    if proof_device_id != &signer_device_id {
                        return Err(ControlPlaneError::Internal {
                            reason: "hosted client refused to sign for a different device id",
                        });
                    }
                    grants::device_authorization_proof(
                        &identity,
                        workspace_id,
                        &signer_device_id,
                        action,
                        subject,
                    )
                    .map_err(|_| ControlPlaneError::Internal {
                        reason: "device authorization proof generation failed",
                    })
                })
                .with_device_proof_verifier_resolver(move |workspace_id, proof_device_id| {
                    Ok(verifier_cache
                        .get(&(Some(workspace_id.clone()), proof_device_id.clone()))
                        // The locally generated verifier is workspace-agnostic:
                        // it is keyed under `None` so it answers for a device
                        // before any workspace-scoped verifier is published.
                        .or_else(|| verifier_cache.get(&(None, proof_device_id.clone())))
                        .cloned())
                })
        })
}

fn fake_control_plane_enabled() -> bool {
    matches!(
        env::var("BOWLINE_USE_FAKE_CONTROL_PLANE").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

fn explicit_workspace_id_configured() -> bool {
    env::var("BOWLINE_WORKSPACE_ID")
        .ok()
        .is_some_and(|workspace_id| !workspace_id.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bowline_local::fakes::FakeKeychain;

    fn bootstrapped_remote_credentials() -> HostedCredentials {
        HostedCredentials {
            bootstrap_token: Some("scoped-bootstrap-token".to_string()),
            control_plane_token: None,
            account_session_id: Some("bowline_session_fixture".to_string()),
            workos_access_token: None,
            has_stored_account: false,
        }
    }

    /// `bowline connect` ships a bootstrap token *and* an account session to the
    /// remote. `device accept` reads device trust between fetching its grant and
    /// confirming it, and that read has no bootstrap-token form, so a client that
    /// let the bootstrap token displace the session stranded the remote with an
    /// uploaded grant it could never accept.
    #[test]
    fn a_bootstrap_token_never_displaces_the_account_session() {
        let store = FakeKeychain::default();

        let client = bootstrapped_remote_credentials()
            .into_client("https://example.convex.cloud".to_string(), &store)
            .expect("hosted client builds from bootstrap credentials");

        assert!(
            client.can_authenticate_account_calls(),
            "a bootstrapped remote must still authenticate device-trust reads"
        );
    }

    #[test]
    fn a_bootstrap_token_alone_is_enough_to_reach_the_control_plane() {
        let credentials = HostedCredentials {
            account_session_id: None,
            ..bootstrapped_remote_credentials()
        };

        assert!(credentials.is_configured());
    }
}
