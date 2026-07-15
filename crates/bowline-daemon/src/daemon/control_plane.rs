use super::*;

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
}

pub(super) fn resolve_daemon_credentials(
    key_store: &dyn DeviceKeyStore,
    workspace_id: &WorkspaceId,
) -> Result<DaemonCredentials, HostedSetupError> {
    let deployment_url =
        require_convex_url().map_err(|_| HostedSetupError::HostedConfigUnavailable)?;
    let control_plane_token = daemon_env_var("BOWLINE_CONTROL_PLANE_TOKEN");
    let has_control_plane_token = control_plane_token.is_some();
    let account_session_id = account_session_id(key_store).or_else(|| {
        ensure_durable_account_session(key_store, workspace_id)
            .ok()
            .flatten()
    });
    let workos_access_token = if has_control_plane_token || account_session_id.is_some() {
        None
    } else {
        workos_access_token(key_store)
    };
    if control_plane_token.is_none()
        && account_session_id.is_none()
        && workos_access_token.is_none()
    {
        return Err(HostedSetupError::CredentialsMissing);
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
    Ok(build_hosted_control_plane(key_store, workspace_id, device_id, resolved)?.client)
}

pub(super) fn build_hosted_control_plane(
    key_store: &dyn DeviceKeyStore,
    workspace_id: WorkspaceId,
    device_id: DeviceId,
    resolved: ResolvedHostedContext,
) -> Result<BuiltHostedControlPlane, HostedSetupError> {
    let ResolvedHostedContext {
        credentials,
        identity,
        verifiers,
    } = resolved;
    let has_control_plane_token = credentials.control_plane_token.is_some();
    let signer_device_id = device_id.clone();
    let signer_workspace_id = workspace_id.clone();
    let verifier_identity = identity.clone();
    let verifier_device_id = device_id.clone();
    let mut verifier_cache = verifiers
        .into_iter()
        .map(|verifier| {
            (
                (
                    verifier.workspace_id.as_str().to_string(),
                    verifier.device_id.as_str().to_string(),
                ),
                verifier.proof_verifier,
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    verifier_cache.insert(
        (
            workspace_id.as_str().to_string(),
            verifier_device_id.as_str().to_string(),
        ),
        grants::device_authorization_proof_verifier(&verifier_identity)
            .map_err(HostedSetupError::Grant)?,
    );
    let mut client = HostedControlPlaneClient::try_new_with_token(
        credentials.deployment_url,
        credentials.control_plane_token.unwrap_or_default(),
    )?
    .with_device_id(device_id.as_str())
    .with_device_proof_signer(move |workspace_id, proof_device_id, action, subject| {
        if workspace_id != signer_workspace_id.as_str() {
            return Err(ControlPlaneError::Storage(
                "daemon refused to sign for a different workspace".to_string(),
            ));
        }
        if proof_device_id != signer_device_id.as_str() {
            return Err(ControlPlaneError::Storage(
                "daemon refused to sign for a different device id".to_string(),
            ));
        }
        grants::device_authorization_proof(
            &identity,
            &signer_workspace_id,
            &signer_device_id,
            action,
            subject,
        )
        .map_err(|error| ControlPlaneError::Storage(error.to_string()))
    });
    if !has_control_plane_token && let Some(access_token) = credentials.workos_access_token.as_ref()
    {
        client = client.with_workos_access_token(access_token.clone());
    }
    if let Some(session_id) = credentials.account_session_id.as_ref() {
        client = client.with_account_session_id(session_id.clone());
    }
    let authoritative_verifiers =
        refresh_device_proof_verifiers(key_store, &client, &workspace_id, &mut verifier_cache)?;
    let installed_verifiers: Vec<DeviceProofVerifier> = verifier_cache
        .iter()
        .map(
            |((workspace_id, device_id), proof_verifier)| DeviceProofVerifier {
                workspace_id: WorkspaceId::new(workspace_id.clone()),
                device_id: DeviceId::new(device_id.clone()),
                proof_verifier: proof_verifier.clone(),
            },
        )
        .collect();
    let client = client.with_device_proof_verifier_resolver(
        move |resolver_workspace_id, proof_device_id| {
            Ok(verifier_cache
                .get(&(
                    resolver_workspace_id.to_string(),
                    proof_device_id.to_string(),
                ))
                .cloned())
        },
    );
    debug_assert!(authoritative_verifiers.iter().all(|verifier| {
        installed_verifiers
            .iter()
            .any(|installed| installed == verifier)
    }));
    Ok(BuiltHostedControlPlane {
        client,
        installed_verifiers,
    })
}

fn refresh_device_proof_verifiers(
    key_store: &dyn DeviceKeyStore,
    client: &HostedControlPlaneClient,
    workspace_id: &WorkspaceId,
    verifier_cache: &mut std::collections::BTreeMap<(String, String), String>,
) -> Result<Vec<DeviceProofVerifier>, HostedSetupError> {
    let authorized_devices = client.list_device_trust(workspace_id)?.authorized_devices;
    let authoritative = authorized_device_proof_verifiers(workspace_id, authorized_devices);
    key_store.replace_device_proof_verifiers_for_workspace(workspace_id, authoritative.clone())?;
    verifier_cache
        .retain(|(cached_workspace_id, _), _| cached_workspace_id != workspace_id.as_str());
    for verifier in &authoritative {
        verifier_cache.insert(
            (
                verifier.workspace_id.as_str().to_string(),
                verifier.device_id.as_str().to_string(),
            ),
            verifier.proof_verifier.clone(),
        );
    }
    Ok(authoritative)
}

fn authorized_device_proof_verifiers(
    workspace_id: &WorkspaceId,
    devices: Vec<AuthorizedDeviceRecord>,
) -> Vec<DeviceProofVerifier> {
    let mut verifiers = devices
        .into_iter()
        .filter_map(|device| {
            device
                .device_authorization_proof_verifier
                .map(|proof_verifier| DeviceProofVerifier {
                    workspace_id: workspace_id.clone(),
                    device_id: device.device_id,
                    proof_verifier,
                })
        })
        .collect::<Vec<_>>();
    verifiers.sort_by(|left, right| left.device_id.cmp(&right.device_id));
    verifiers
}

#[cfg(test)]
mod tests {
    use super::*;
    use bowline_local::fakes::FakeKeychain;

    #[test]
    fn authoritative_verifier_replacement_persists_replacement_and_revocation() {
        let key_store = FakeKeychain::default();
        let workspace_id = WorkspaceId::new("workspace_test");
        let sibling_device_id = DeviceId::new("device_sibling");
        key_store
            .store_device_proof_verifier(DeviceProofVerifier {
                workspace_id: workspace_id.clone(),
                device_id: sibling_device_id.clone(),
                proof_verifier: "dapv_old".to_string(),
            })
            .expect("seed verifier");
        let replacement = authorized_device_proof_verifiers(
            &workspace_id,
            vec![AuthorizedDeviceRecord {
                workspace_id: workspace_id.clone(),
                device_id: sibling_device_id.clone(),
                device_name: "Sibling".to_string(),
                platform: "linux".to_string(),
                device_fingerprint: "fingerprint_sibling".to_string(),
                authorized_at: ControlPlaneTimestamp { tick: 1 },
                authorized_by_device_id: Some(DeviceId::new("device_approver")),
                device_authorization_proof_verifier: Some("dapv_p256_v1_sibling".to_string()),
                revoked_at: None,
            }],
        );
        key_store
            .replace_device_proof_verifiers_for_workspace(&workspace_id, replacement.clone())
            .expect("replace verifier");
        assert_eq!(
            key_store
                .load_device_proof_verifiers()
                .expect("replacement persisted"),
            replacement
        );

        key_store
            .replace_device_proof_verifiers_for_workspace(&workspace_id, Vec::new())
            .expect("revoke verifier");
        assert!(
            key_store
                .load_device_proof_verifiers()
                .expect("revocation persisted")
                .is_empty()
        );
    }
}

#[derive(Debug)]
pub(super) enum HostedSetupError {
    HostedConfigUnavailable,
    CredentialsMissing,
    DeviceKeys(DeviceKeyError),
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
            Self::CredentialsMissing => formatter.write_str(
                "daemon sync requires account session credentials, BOWLINE_CONTROL_PLANE_TOKEN, or a stored account session",
            ),
            Self::DeviceKeys(error) => error.fmt(formatter),
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
            | Self::CredentialsMissing
            | Self::CachePoisoned
            | Self::ContextChangedDuringBuild => None,
            Self::DeviceKeys(error) => Some(error),
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

impl From<DeviceKeyError> for HostedSetupError {
    fn from(error: DeviceKeyError) -> Self {
        Self::DeviceKeys(error)
    }
}

pub(super) fn account_session_id(key_store: &dyn DeviceKeyStore) -> Option<String> {
    daemon_env_var("BOWLINE_ACCOUNT_SESSION_ID")
        .filter(|session_id| durable_account_session_id(session_id))
        .filter(|_| {
            daemon_env_var("BOWLINE_ACCOUNT_SESSION_REVOCATION_TOKEN")
                .is_some_and(|token| token.starts_with("bowline_revoke_"))
        })
        .or_else(|| {
            key_store
                .load_account_tokens()
                .ok()
                .flatten()
                .and_then(|tokens| tokens.account_session.map(|session| session.session_id))
                .filter(|session_id| durable_account_session_id(session_id))
        })
}

pub(super) fn durable_account_session_id(session_id: &str) -> bool {
    session_id.starts_with("bowline_session_")
}

pub(super) fn ensure_durable_account_session(
    key_store: &dyn DeviceKeyStore,
    workspace_id: &WorkspaceId,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    if let Some(session_id) = account_session_id(key_store) {
        return Ok(Some(session_id));
    }
    if key_store.load_account_tokens()?.is_none() {
        return Ok(None);
    }
    let Some(access_token) = workos_access_token(key_store) else {
        return Ok(None);
    };
    let mut tokens = match key_store.load_account_tokens()? {
        Some(tokens) => tokens,
        None => return Ok(None),
    };
    let client =
        HostedControlPlaneClient::try_new_with_token(require_convex_url()?, String::new())?;
    let registration =
        client.register_account_session(access_token, Some(workspace_id.as_str()))?;
    tokens.account_session = Some(bowline_local::device_keys::AccountSessionCredentials {
        session_id: registration.session_id.clone(),
        revocation_token: registration.revocation_token,
    });
    key_store.store_account_tokens(tokens)?;
    Ok(Some(registration.session_id))
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

pub(super) fn workspace_key_bytes(bytes: &[u8]) -> Result<[u8; 32], Box<dyn std::error::Error>> {
    bytes
        .try_into()
        .map_err(|_| runtime_error("workspace key material must be exactly 32 bytes"))
}

pub(super) fn runtime_error(message: impl Into<String>) -> Box<dyn std::error::Error> {
    Box::new(io::Error::other(message.into()))
}
