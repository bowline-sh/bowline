use super::*;

pub(super) fn hosted_control_plane(
    key_store: &dyn DeviceKeyStore,
    workspace_id: WorkspaceId,
    device_id: DeviceId,
) -> Result<HostedControlPlaneClient, Box<dyn std::error::Error>> {
    let convex_url = require_convex_url()?;
    let control_plane_token = env::var("BOWLINE_CONTROL_PLANE_TOKEN")
        .ok()
        .filter(|value| !value.is_empty());
    let has_control_plane_token = control_plane_token.is_some();
    let account_session_id = account_session_id(key_store).or_else(|| {
        ensure_durable_account_session(key_store, &workspace_id)
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
        return Err(runtime_error(
            "daemon sync requires BOWLINE_ACCOUNT_SESSION_ID, BOWLINE_CONTROL_PLANE_TOKEN, or a stored account session",
        ));
    }

    let identity = key_store.load_or_create_device_identity()?;
    let signer_device_id = device_id.clone();
    let signer_workspace_id = workspace_id.clone();
    let mut client = HostedControlPlaneClient::try_new_with_token(
        convex_url,
        control_plane_token.unwrap_or_default(),
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
        Ok(grants::device_authorization_proof(
            &identity,
            &signer_workspace_id,
            &signer_device_id,
            action,
            subject,
        ))
    });
    if !has_control_plane_token && let Some(access_token) = workos_access_token {
        client = client.with_workos_access_token(access_token);
    }
    if let Some(session_id) = account_session_id {
        client = client.with_account_session_id(session_id);
    }
    Ok(client)
}

pub(super) fn account_session_id(key_store: &dyn DeviceKeyStore) -> Option<String> {
    nonempty_env_value(env::var("BOWLINE_ACCOUNT_SESSION_ID").ok())
        .filter(|session_id| durable_account_session_id(session_id))
        .or_else(|| {
            key_store
                .load_account_tokens()
                .ok()
                .flatten()
                .and_then(|tokens| tokens.account_session_id)
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
    let session_id =
        client.register_account_session_id(access_token, Some(workspace_id.as_str()))?;
    tokens.account_session_id = Some(session_id.clone());
    key_store.store_account_tokens(tokens)?;
    Ok(Some(session_id))
}

pub(super) fn workos_access_token(key_store: &dyn DeviceKeyStore) -> Option<String> {
    if let Some(token) = nonempty_env_value(env::var("BOWLINE_WORKOS_ACCESS_TOKEN").ok())
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
    let client_id = nonempty_env_value(env::var("BOWLINE_WORKOS_CLIENT_ID").ok())
        .unwrap_or_else(|| DEFAULT_WORKOS_CLIENT_ID.to_string());
    workos::refresh_and_store(key_store, &client_id, &tokens.refresh_token)
        .ok()
        .map(|tokens| tokens.access_token)
}

pub(super) fn refresh_env_workos_token(key_store: &dyn DeviceKeyStore) -> Option<String> {
    let client_id = nonempty_env_value(env::var("BOWLINE_WORKOS_CLIENT_ID").ok())
        .unwrap_or_else(|| DEFAULT_WORKOS_CLIENT_ID.to_string());
    let refresh_token = nonempty_env_value(env::var("BOWLINE_WORKOS_REFRESH_TOKEN").ok())?;
    workos::refresh_and_store(key_store, &client_id, &refresh_token)
        .ok()
        .map(|tokens| tokens.access_token)
}

pub(super) fn nonempty_env_value(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.is_empty())
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
    Ok(env::var("CONVEX_URL")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_CONVEX_URL.to_string()))
}

pub(super) fn key_store() -> Result<Box<dyn DeviceKeyStore>, Box<dyn std::error::Error>> {
    if let Ok(path) = env::var("BOWLINE_SECRET_STORE_PATH")
        && !path.is_empty()
    {
        return Ok(Box::new(ServerLocalSecretStore::new(path)));
    }
    if keychain_secret_store_allowed() {
        return Ok(Box::new(KeyringDeviceKeyStore::new("default")));
    }
    Ok(Box::new(ServerLocalSecretStore::new(
        ServerLocalSecretStore::default_path()?,
    )))
}

pub(super) fn keychain_secret_store_allowed() -> bool {
    env::var("BOWLINE_SECRET_STORE").as_deref() == Ok("keychain")
        && matches!(
            env::var("BOWLINE_ALLOW_KEYCHAIN_PROBE").as_deref(),
            Ok("1") | Ok("true") | Ok("yes")
        )
}

pub(super) fn workspace_key_bytes(bytes: &[u8]) -> Result<[u8; 32], Box<dyn std::error::Error>> {
    bytes
        .try_into()
        .map_err(|_| runtime_error("workspace key material must be exactly 32 bytes"))
}

pub(super) fn runtime_error(message: impl Into<String>) -> Box<dyn std::error::Error> {
    Box::new(io::Error::other(message.into()))
}
