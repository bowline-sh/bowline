use super::*;

pub(super) fn wait_for_remote_daemon<R>(
    runner: &R,
    options: &BootstrapSshOptions,
) -> Result<ssh::RemoteBootstrapProbe, ssh::BootstrapSshError>
where
    R: ProcessRunner,
{
    let mut last_probe = None;
    for attempt in 0..20 {
        let probe = ssh::daemon_status_remote(runner, options)?;
        if remote_daemon_sync_is_ready(&probe.stdout) {
            return Ok(probe);
        }
        last_probe = Some(probe);
        if attempt < 19 {
            thread::sleep(Duration::from_millis(250));
        }
    }
    Ok(last_probe.expect("readiness loop always performs at least one probe"))
}

pub(super) fn existing_trusted_remote_device<R>(
    runner: &R,
    options: &BootstrapSshOptions,
    workspace_id: &bowline_core::ids::WorkspaceId,
) -> Option<DeviceRecord>
where
    R: ProcessRunner,
{
    let mut options = options.clone();
    options.bootstrap_token = None;
    let probe = ssh::list_remote_devices(runner, &options).ok()?;
    let output = serde_json::from_str::<DevicesCommandOutput>(&probe.stdout).ok()?;
    output.devices.into_iter().find(|device| {
        device.is_current_device
            && device.trust_state == DeviceTrustState::Trusted
            && device.workspace_id.as_str() == workspace_id.as_str()
    })
}

pub(super) fn remote_workspace_key_available<R>(
    runner: &R,
    options: &BootstrapSshOptions,
    workspace_id: &bowline_core::ids::WorkspaceId,
) -> bool
where
    R: ProcessRunner,
{
    ssh::server_local_workspace_key_available(runner, options, workspace_id.as_str())
        .unwrap_or(false)
}

pub(super) fn set_remote_device_id(options: &mut BootstrapSshOptions, device_id: String) {
    if let Some((_, value)) = options
        .remote_env
        .iter_mut()
        .find(|(key, _)| key == "BOWLINE_DEVICE_ID")
    {
        *value = device_id;
        return;
    }
    options
        .remote_env
        .push(("BOWLINE_DEVICE_ID".to_string(), device_id));
}

pub(super) fn request_matches_cloud(
    remote: &DeviceApprovalRequest,
    cloud: &bowline_control_plane::DeviceRequest,
) -> bool {
    remote.request_id.as_str() == cloud.request_id
        && remote.workspace_id.as_str() == cloud.workspace_id
        && remote.requester_device_id.as_str() == cloud.device_id
        && remote.device_public_key.as_str() == cloud.device_public_key
        && remote.device_fingerprint.as_str() == cloud.device_fingerprint
        && remote.matching_code == cloud.matching_code
}

pub(super) fn remote_bootstrap_env(host: &str) -> Vec<(String, String)> {
    let mut values = Vec::new();
    if let Some(convex_url) = runtime::hosted_convex_url() {
        values.push(("CONVEX_URL".to_string(), convex_url));
    }
    values.push((
        "BOWLINE_WORKOS_CLIENT_ID".to_string(),
        runtime::hosted_workos_client_id(),
    ));
    values.push((
        "BOWLINE_WORKSPACE_ID".to_string(),
        runtime::active_workspace_id().as_str().to_string(),
    ));
    values.push(("BOWLINE_DEVICE_ID".to_string(), remote_device_id(host)));
    values.push(("BOWLINE_DEVICE_NAME".to_string(), remote_device_name(host)));
    values.push((
        "BOWLINE_SECRET_STORE".to_string(),
        "server-local".to_string(),
    ));
    values
}

pub(super) fn remote_device_id(host: &str) -> String {
    format!("device_{}", sanitize_remote_device_id_part(host))
}

pub(super) fn remote_rebootstrap_device_id(host: &str, seed: &str) -> String {
    let suffix = blake3::hash(seed.as_bytes()).to_hex()[..8].to_string();
    format!("device_{}_{}", sanitize_remote_device_id_part(host), suffix)
}

pub(super) fn sanitize_remote_device_id_part(value: &str) -> String {
    let mut id = String::new();
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            id.push(character.to_ascii_lowercase());
        } else {
            id.push('_');
        }
    }
    while id.contains("__") {
        id = id.replace("__", "_");
    }
    id.trim_matches('_').to_string()
}

pub(super) fn remote_device_name(host: &str) -> String {
    format!("bowline-remote-{}", sanitize_remote_device_id_part(host))
}

pub(super) fn remote_bootstrap_secret_env() -> Vec<(String, String)> {
    let account_session_id = runtime::key_store().ok().and_then(|store| {
        let _ =
            runtime::ensure_durable_account_session(&*store, Some(&runtime::active_workspace_id()));
        runtime::account_session_id(&*store)
    });
    let control_plane_token = env::var("BOWLINE_CONTROL_PLANE_TOKEN")
        .ok()
        .filter(|value| !value.is_empty());
    remote_bootstrap_secret_env_from(account_session_id, control_plane_token)
}

pub(super) fn remote_bootstrap_secret_env_from(
    account_session_id: Option<String>,
    control_plane_token: Option<String>,
) -> Vec<(String, String)> {
    let mut values = Vec::new();
    if let Some(session_id) = account_session_id {
        values.push(("BOWLINE_ACCOUNT_SESSION_ID".to_string(), session_id));
    }
    if let Some(token) = control_plane_token {
        values.push(("BOWLINE_CONTROL_PLANE_TOKEN".to_string(), token));
    }
    if values.is_empty() {
        values.push((
            "BOWLINE_REMOTE_AUTH_ERROR".to_string(),
            "missing-durable-account-session".to_string(),
        ));
    }
    values
}

pub(super) fn remote_bootstrap_auth_error(values: &[(String, String)]) -> bool {
    values
        .iter()
        .any(|(key, value)| key == "BOWLINE_REMOTE_AUTH_ERROR" && !value.is_empty())
}

pub(super) fn verify_remote_device_trust(
    control_plane: &dyn bowline_control_plane::ControlPlaneClient,
    remote_request: &DeviceApprovalRequest,
) -> Result<DeviceRecord, String> {
    let trust = control_plane
        .list_device_trust(remote_request.workspace_id.as_str())
        .map_err(|error| format!("Could not verify remote device trust: {error}"))?;
    let Some(device) = trust.authorized_devices.into_iter().find(|device| {
        device.device_id == remote_request.requester_device_id.as_str()
            && device.device_fingerprint == remote_request.device_fingerprint.as_str()
    }) else {
        return Err(format!(
            "Remote device {} accepted its grant but is not authorized in the control plane.",
            remote_request.device_name
        ));
    };

    Ok(DeviceRecord {
        id: DeviceId::new(device.device_id),
        name: device.device_name,
        workspace_id: remote_request.workspace_id.clone(),
        platform: platform_from_str(&device.platform),
        trust_state: DeviceTrustState::Trusted,
        device_fingerprint: DeviceFingerprint::new(device.device_fingerprint),
        authorized_at: Some(device.authorized_at.to_string()),
        updated_at: device.authorized_at.to_string(),
        is_current_device: false,
        limitation_reason: None,
    })
}

pub(super) fn remote_sync_is_ready(status: &WorkspaceStatus) -> bool {
    !status.needs_attention()
}

pub(super) fn remote_daemon_is_running(stdout: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(stdout).is_ok_and(|value| {
        value
            .pointer("/daemon/state")
            .and_then(|state| state.as_str())
            == Some("running")
    })
}

pub(super) fn remote_daemon_sync_is_ready(stdout: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(stdout).is_ok_and(|value| {
        let sync = &value["sync"];
        let local_head = &sync["localHead"];
        let remote_head = &sync["remoteHead"];
        sync["state"].as_str() == Some("idle")
            && local_head["workspaceId"].as_str().is_some()
            && local_head["snapshotId"].as_str().is_some()
            && local_head["version"].as_u64().is_some()
            && local_head["workspaceId"] == remote_head["workspaceId"]
            && local_head["snapshotId"] == remote_head["snapshotId"]
            && local_head["version"] == remote_head["version"]
    })
}

pub(super) fn remote_daemon_status_summary(stdout: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(stdout) {
        Ok(value) => {
            let state = value
                .pointer("/daemon/state")
                .and_then(|state| state.as_str())
                .unwrap_or("unknown");
            format!("Remote daemon is {state}, not running.")
        }
        Err(error) => format!("Remote daemon status output was not valid bowline JSON: {error}"),
    }
}

pub(super) fn platform_from_str(value: &str) -> bowline_core::devices::DevicePlatform {
    match value {
        "macos" | "darwin" => bowline_core::devices::DevicePlatform::Macos,
        "linux" => bowline_core::devices::DevicePlatform::Linux,
        _ => bowline_core::devices::DevicePlatform::Unknown,
    }
}

pub(super) fn remote_status_attention_summary(status: &WorkspaceStatus) -> String {
    status
        .attention_items
        .first()
        .cloned()
        .unwrap_or_else(|| "Remote status is not healthy.".to_string())
}
