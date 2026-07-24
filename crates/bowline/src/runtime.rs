use std::{
    collections::BTreeMap,
    env,
    path::{Path, PathBuf},
};

use bowline_control_plane::{
    AuthorizedDeviceRecord, ControlPlaneClient, ControlPlaneError, FakeControlPlaneClient,
    HostedControlPlaneClient,
};
use bowline_core::{
    devices::DevicePlatform,
    hosted::{DEFAULT_CONVEX_URL, DEFAULT_WORKOS_CLIENT_ID},
    ids::{DeviceId, WorkspaceId},
};
use bowline_local::{
    account::workos,
    device_keys::{AccountTokens, DeviceIdentity, DeviceKeyStore, default_device_key_store},
    metadata::{MetadataStore, default_database_path},
    trust::grants,
};

mod account_session;
#[cfg(test)]
mod account_session_tests;
pub use account_session::{
    AccountSessionRevocation, account_session_id, clear_persisted_account_session,
    ensure_durable_account_session, environment_account_session_revocation,
    persisted_account_session_revocation, revoke_account_session,
    stored_account_session_revocation,
};

#[cfg(test)]
fn tempfile_dir(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("temp dir");
    path
}

pub fn control_plane() -> Result<Box<dyn ControlPlaneClient>, String> {
    let bootstrap_token = env::var("BOWLINE_BOOTSTRAP_TOKEN")
        .ok()
        .filter(|value| !value.is_empty());
    control_plane_with_bootstrap_token(bootstrap_token)
}

pub fn control_plane_with_bootstrap_token(
    bootstrap_token: Option<String>,
) -> Result<Box<dyn ControlPlaneClient>, String> {
    let convex_url = hosted_convex_url();
    if let Some(convex_url) = convex_url {
        if let Some(bootstrap_token) = bootstrap_token {
            return Ok(Box::new(
                HostedControlPlaneClient::try_new_with_bootstrap_token(convex_url, bootstrap_token)
                    .map_err(|error| error.to_string())?
                    .with_device_id(device_id().as_str()),
            ));
        }

        let store = key_store()?;
        let control_plane_token = env::var("BOWLINE_CONTROL_PLANE_TOKEN")
            .ok()
            .filter(|value| !value.is_empty());
        let account_session_id = account_session_id(&*store).or_else(|| {
            ensure_durable_account_session(&*store, Some(&active_workspace_id()))
                .ok()
                .flatten()
        });
        let workos_access_token = if account_session_id.is_some() {
            None
        } else {
            workos_access_token(&*store)
        };
        let has_control_plane_token = control_plane_token.is_some();
        let has_stored_account = store.load_account_tokens().ok().flatten().is_some();
        if has_control_plane_token
            || account_session_id.is_some()
            || workos_access_token.is_some()
            || has_stored_account
            || explicit_workspace_id_configured()
            || local_accepted_workspace_id().is_some()
        {
            let mut client = hosted_client_with_device_proof(
                convex_url,
                control_plane_token.unwrap_or_default(),
                &*store,
            )?;
            if let Some(access_token) = workos_access_token {
                client = client.with_workos_access_token(access_token);
            }
            if let Some(session_id) = account_session_id {
                client = client.with_account_session_id(session_id);
            }
            return Ok(Box::new(client));
        }
    }

    if fake_control_plane_enabled() {
        return Ok(Box::new(FakeControlPlaneClient::default()));
    }

    Err(
        "control-plane configuration is missing; run `bowline setup --root <path>` or set CONVEX_URL and BOWLINE_CONTROL_PLANE_TOKEN"
            .to_string(),
    )
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
                (
                    verifier.workspace_id.as_str().to_string(),
                    verifier.device_id.as_str().to_string(),
                ),
                verifier.proof_verifier,
            )
        })
        .collect::<BTreeMap<_, _>>();
    verifier_cache.insert(
        (String::new(), verifier_device_id.as_str().to_string()),
        grants::device_authorization_proof_verifier(&verifier_identity)
            .map_err(|error| error.to_string())?,
    );
    HostedControlPlaneClient::try_new_with_token(convex_url, control_plane_token)
        .map_err(|error| error.to_string())
        .map(|client| {
            client
                .with_device_id(device_id.as_str())
                .with_device_proof_signer(move |workspace_id, proof_device_id, action, subject| {
                    if proof_device_id != signer_device_id.as_str() {
                        return Err(ControlPlaneError::Storage(
                            "hosted client refused to sign for a different device id".to_string(),
                        ));
                    }
                    grants::device_authorization_proof(
                        &identity,
                        &WorkspaceId::new(workspace_id.to_string()),
                        &signer_device_id,
                        action,
                        subject,
                    )
                    .map_err(|error| ControlPlaneError::Storage(error.to_string()))
                })
                .with_device_proof_verifier_resolver(move |_workspace_id, proof_device_id| {
                    Ok(verifier_cache
                        .get(&(_workspace_id.to_string(), proof_device_id.to_string()))
                        .or_else(|| {
                            verifier_cache.get(&(String::new(), proof_device_id.to_string()))
                        })
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

pub fn key_store() -> Result<Box<dyn DeviceKeyStore>, String> {
    default_device_key_store().map_err(|error| error.to_string())
}

pub fn passive_secret_store_probe_allowed() -> bool {
    !cfg!(test)
}

fn nonempty_env_value(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.is_empty())
}

pub fn workos_access_token(store: &dyn DeviceKeyStore) -> Option<String> {
    if let Some(token) = nonempty_env_value(env::var("BOWLINE_WORKOS_ACCESS_TOKEN").ok())
        && workos_token_is_not_expired(&token)
    {
        return Some(token);
    }
    if let Some(token) = refresh_env_workos_token(store) {
        return Some(token);
    }
    let tokens = store.load_account_tokens().ok().flatten()?;
    if workos_token_is_not_expired(&tokens.access_token) {
        return Some(tokens.access_token);
    }
    refresh_workos_tokens(store, &tokens.refresh_token).map(|tokens| tokens.access_token)
}

fn refresh_env_workos_token(store: &dyn DeviceKeyStore) -> Option<String> {
    let client_id = hosted_workos_client_id();
    let refresh_token = nonempty_env_value(env::var("BOWLINE_WORKOS_REFRESH_TOKEN").ok())?;
    refresh_workos_tokens_with_client(store, &client_id, &refresh_token)
        .map(|tokens| tokens.access_token)
}

fn refresh_workos_tokens(store: &dyn DeviceKeyStore, refresh_token: &str) -> Option<AccountTokens> {
    let client_id = hosted_workos_client_id();
    refresh_workos_tokens_with_client(store, &client_id, refresh_token)
}

fn refresh_workos_tokens_with_client(
    store: &dyn DeviceKeyStore,
    client_id: &str,
    refresh_token: &str,
) -> Option<AccountTokens> {
    workos::refresh_and_store(store, client_id, refresh_token).ok()
}

pub fn hosted_convex_url() -> Option<String> {
    Some(
        nonempty_env_value(env::var("CONVEX_URL").ok())
            .unwrap_or_else(|| DEFAULT_CONVEX_URL.to_string()),
    )
}

pub fn hosted_workos_client_id() -> String {
    nonempty_env_value(env::var("BOWLINE_WORKOS_CLIENT_ID").ok())
        .unwrap_or_else(|| DEFAULT_WORKOS_CLIENT_ID.to_string())
}

fn workos_token_is_not_expired(token: &str) -> bool {
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
    exp > time::OffsetDateTime::now_utc().unix_timestamp() + 30
}

fn workos_account_id_from_access_token(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let bytes = decode_base64url(payload)?;
    let value = serde_json::from_slice::<serde_json::Value>(&bytes).ok()?;
    value
        .get("sub")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn decode_base64url(input: &str) -> Option<Vec<u8>> {
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

#[cfg(test)]
fn keychain_probe_value_allowed(value: Option<&str>) -> bool {
    matches!(value, Some("1") | Some("true") | Some("yes"))
}

#[cfg(test)]
fn keychain_secret_store_allowed_from(store: Option<&str>, probe: Option<&str>) -> bool {
    store == Some("keychain") && keychain_probe_value_allowed(probe)
}

pub fn active_workspace_id() -> WorkspaceId {
    workspace_id_with_probes(true, true)
}

pub fn active_workspace_id_without_local_metadata_probe() -> WorkspaceId {
    workspace_id_with_probes(true, false)
}

fn workspace_id_with_probes(
    allow_account_probe: bool,
    allow_local_metadata_probe: bool,
) -> WorkspaceId {
    if let Ok(workspace_id) = env::var("BOWLINE_WORKSPACE_ID")
        && !workspace_id.is_empty()
    {
        return workspace_id_from_sources(WorkspaceIdSources {
            explicit_workspace_id: Some(workspace_id),
            ..WorkspaceIdSources::default()
        });
    }
    let hosted_control_plane = hosted_control_plane_configured();
    if hosted_control_plane
        && allow_account_probe
        && let Ok(store) = key_store()
    {
        if let Ok(Some(tokens)) = store.load_account_tokens() {
            return workspace_id_from_sources(WorkspaceIdSources {
                hosted_control_plane,
                allow_account_probe,
                allow_local_metadata_probe,
                account_id: Some(tokens.account_id.as_str()),
                ..WorkspaceIdSources::default()
            });
        }
        if let Some(access_token) = workos_access_token(&*store)
            && let Some(account_id) = workos_account_id_from_access_token(&access_token)
        {
            return workspace_id_from_sources(WorkspaceIdSources {
                hosted_control_plane,
                allow_account_probe,
                allow_local_metadata_probe,
                account_id: Some(&account_id),
                ..WorkspaceIdSources::default()
            });
        }
    }
    if allow_local_metadata_probe && let Some(workspace_id) = local_accepted_workspace_id() {
        return workspace_id_from_sources(WorkspaceIdSources {
            hosted_control_plane,
            allow_account_probe,
            allow_local_metadata_probe,
            local_workspace_id: Some(workspace_id),
            ..WorkspaceIdSources::default()
        });
    }
    workspace_id_from_sources(WorkspaceIdSources {
        hosted_control_plane,
        allow_account_probe,
        allow_local_metadata_probe,
        ..WorkspaceIdSources::default()
    })
}

/// Inputs for workspace-id resolution, applied in precedence order: explicit
/// id, account-scoped hosted id, locally accepted workspace, then the default.
#[derive(Default)]
struct WorkspaceIdSources<'a> {
    explicit_workspace_id: Option<String>,
    hosted_control_plane: bool,
    allow_account_probe: bool,
    allow_local_metadata_probe: bool,
    account_id: Option<&'a str>,
    local_workspace_id: Option<WorkspaceId>,
}

fn workspace_id_from_sources(sources: WorkspaceIdSources<'_>) -> WorkspaceId {
    if let Some(workspace_id) = sources
        .explicit_workspace_id
        .filter(|value| !value.is_empty())
    {
        return WorkspaceId::new(workspace_id);
    }
    if sources.hosted_control_plane
        && sources.allow_account_probe
        && let Some(account_id) = sources.account_id
    {
        return WorkspaceId::new(account_scoped_workspace_id(account_id));
    }
    if sources.allow_local_metadata_probe
        && let Some(workspace_id) = sources.local_workspace_id
    {
        return workspace_id;
    }
    WorkspaceId::new("ws_code")
}

pub fn selected_metadata_database_path() -> Option<PathBuf> {
    env::var_os("BOWLINE_METADATA_DB")
        .map(PathBuf::from)
        .or_else(|| default_database_path().ok())
}

pub(crate) fn metadata_state_root(db_path: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(db_path)
        .ok()
        .as_deref()
        .unwrap_or(db_path)
        .parent()
        .map(Path::to_path_buf)
}

fn local_accepted_workspace_id() -> Option<WorkspaceId> {
    let store = MetadataStore::open(selected_metadata_database_path()?).ok()?;
    let workspace = store.current_workspace().ok().flatten()?;
    if store.accepted_roots(&workspace.id).ok()?.is_empty() {
        return None;
    }
    Some(workspace.id)
}

pub fn active_workspace_root() -> Option<String> {
    let store = MetadataStore::open(selected_metadata_database_path()?).ok()?;
    store.current_workspace_root().ok().flatten()
}

pub fn workspace_id_for_root(root: &str) -> Result<WorkspaceId, String> {
    let db_path = selected_metadata_database_path()
        .ok_or_else(|| format!("no local metadata database; run `bowline setup --root {root}`"))?;
    let store = MetadataStore::open(db_path).map_err(|error| error.to_string())?;
    store
        .workspace_by_accepted_root(root)
        .map_err(|error| error.to_string())?
        .map(|workspace| workspace.id)
        .ok_or_else(|| {
            format!("workspace root is not initialized; run `bowline setup --root {root}`")
        })
}

fn hosted_control_plane_configured() -> bool {
    hosted_convex_url().is_some()
}

fn account_scoped_workspace_id(account_id: &str) -> String {
    let seed = format!("bowline:default-code-workspace:{account_id}");
    let suffix = blake3::hash(seed.as_bytes()).to_hex()[..16].to_string();
    format!("ws_code_{suffix}")
}

pub fn device_id() -> DeviceId {
    configured_device_id()
        .map(DeviceId::new)
        .unwrap_or_else(cryptographic_device_id)
}

pub fn daemon_device_id(workspace_id: &WorkspaceId) -> DeviceId {
    if let Ok(device_id) = env::var("BOWLINE_DEVICE_ID")
        && !device_id.trim().is_empty()
    {
        return DeviceId::new(device_id);
    }

    let persisted_device_id = selected_metadata_database_path()
        .as_deref()
        .and_then(metadata_state_root)
        .and_then(|state_root| persisted_daemon_device_id_for_workspace(&state_root, workspace_id));
    trusted_device_id_for_local_identity(workspace_id)
        .or(persisted_device_id)
        .map(DeviceId::new)
        .unwrap_or_else(cryptographic_device_id)
}

pub fn device_name() -> String {
    env::var("BOWLINE_DEVICE_NAME").unwrap_or_else(|_| default_device_name())
}

pub fn platform() -> DevicePlatform {
    match env::consts::OS {
        "macos" => DevicePlatform::Macos,
        "linux" => DevicePlatform::Linux,
        _ => DevicePlatform::Unknown,
    }
}

fn default_device_name() -> String {
    env::var("HOSTNAME")
        .or_else(|_| env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "Bowline device".to_string())
}

fn cryptographic_device_id() -> DeviceId {
    let store = key_store().expect("device identity store must be available for device selection");
    let identity = store
        .load_or_create_device_identity()
        .expect("persisted cryptographic device identity must be available for device selection");
    device_id_for_identity(&identity)
}

fn device_id_for_identity(identity: &DeviceIdentity) -> DeviceId {
    let hash = blake3::hash(identity.public_key.as_str().as_bytes());
    DeviceId::new(format!("device_{}", &hash.to_hex()[..32]))
}

fn configured_device_id() -> Option<String> {
    configured_device_id_from(
        nonempty_env_value(env::var("BOWLINE_DEVICE_ID").ok()),
        persisted_daemon_device_id(),
    )
}

fn configured_device_id_from(
    explicit_device_id: Option<String>,
    persisted_device_id: Option<String>,
) -> Option<String> {
    explicit_device_id.or(persisted_device_id)
}

fn persisted_daemon_device_id() -> Option<String> {
    let db_path = selected_metadata_database_path()?;
    let state_root = metadata_state_root(&db_path)?;
    let workspace_id = active_workspace_id_for_persisted_daemon_device()?;
    persisted_daemon_device_id_for_workspace(&state_root, &workspace_id)
}

pub(crate) fn persisted_daemon_device_id_for_workspace(
    state_root: &Path,
    workspace_id: &WorkspaceId,
) -> Option<String> {
    let persisted_workspace_id = persisted_daemon_env_value(state_root, "BOWLINE_WORKSPACE_ID")?;
    if persisted_workspace_id != workspace_id.as_str() {
        return None;
    }
    persisted_daemon_env_value(state_root, "BOWLINE_DEVICE_ID")
}

fn active_workspace_id_for_persisted_daemon_device() -> Option<WorkspaceId> {
    nonempty_env_value(env::var("BOWLINE_WORKSPACE_ID").ok())
        .map(WorkspaceId::new)
        .or_else(local_accepted_workspace_id)
}

fn persisted_daemon_env_value(state_root: &Path, name: &str) -> Option<String> {
    let contents = std::fs::read_to_string(state_root.join("daemon.env")).ok()?;
    contents.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        (key == name)
            .then(|| value.trim().to_string())
            .filter(|value| !value.is_empty())
    })
}

fn trusted_device_id_for_local_identity(workspace_id: &WorkspaceId) -> Option<String> {
    let store = key_store().ok()?;
    let identity = store.load_or_create_device_identity().ok()?;
    let trust = control_plane().ok()?.list_device_trust(workspace_id).ok()?;
    select_authorized_device_for_identity(
        &trust.authorized_devices,
        identity.fingerprint.as_str(),
        platform_label(),
    )
    .map(ToOwned::to_owned)
}

fn select_authorized_device_for_identity<'a>(
    devices: &'a [AuthorizedDeviceRecord],
    fingerprint: &str,
    platform: &str,
) -> Option<&'a str> {
    devices
        .iter()
        .find(|device| {
            device.device_fingerprint == fingerprint
                && device.platform == platform
                && device.revoked_at.is_none()
        })
        .or_else(|| {
            devices.iter().find(|device| {
                device.device_fingerprint == fingerprint && device.revoked_at.is_none()
            })
        })
        .map(|device| device.device_id.as_str())
}

fn platform_label() -> &'static str {
    match platform() {
        DevicePlatform::Macos => "macos",
        DevicePlatform::Linux => "linux",
        DevicePlatform::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use bowline_control_plane::{AuthorizedDeviceRecord, ControlPlaneTimestamp};
    use bowline_core::ids::{DeviceId, WorkspaceId};
    use bowline_local::device_keys::{DeviceKeyStore, ServerLocalSecretStore};

    use super::{
        WorkspaceIdSources, account_scoped_workspace_id, configured_device_id_from,
        device_id_for_identity, keychain_secret_store_allowed_from, nonempty_env_value,
        passive_secret_store_probe_allowed, persisted_daemon_device_id_for_workspace,
        persisted_daemon_env_value, select_authorized_device_for_identity, tempfile_dir,
        workos_account_id_from_access_token, workos_token_is_not_expired,
        workspace_id_from_sources,
    };

    #[test]
    fn unit_tests_never_probe_the_ambient_secret_store() {
        assert!(!passive_secret_store_probe_allowed());
    }

    #[test]
    fn fallback_device_id_is_stable_and_unique_per_cryptographic_identity() {
        let temp = tempfile_dir("bowline-runtime-device-identity");
        let first_store = ServerLocalSecretStore::new(temp.join("first.json"));
        let reloaded_first_store = ServerLocalSecretStore::new(temp.join("first.json"));
        let second_store = ServerLocalSecretStore::new(temp.join("second.json"));
        let first = first_store
            .load_or_create_device_identity()
            .expect("first identity");
        let reloaded_first = reloaded_first_store
            .load_or_create_device_identity()
            .expect("reloaded first identity");
        let second = second_store
            .load_or_create_device_identity()
            .expect("second identity");

        assert_eq!(
            device_id_for_identity(&first),
            device_id_for_identity(&reloaded_first)
        );
        assert_ne!(
            device_id_for_identity(&first),
            device_id_for_identity(&second)
        );
        assert!(
            device_id_for_identity(&first)
                .as_str()
                .starts_with("device_")
        );
        std::fs::remove_dir_all(temp).expect("remove temp dir");
    }

    #[test]
    fn account_scoped_workspace_ids_keep_default_code_unique_per_account() {
        let first = account_scoped_workspace_id("account_first");
        let second = account_scoped_workspace_id("account_second");

        assert!(first.starts_with("ws_code_"));
        assert!(second.starts_with("ws_code_"));
        assert_ne!(first, second);
        assert_eq!(first.len(), "ws_code_".len() + 16);
    }

    #[test]
    fn empty_secret_store_path_is_not_configured() {
        assert_eq!(nonempty_env_value(None), None);
        assert_eq!(nonempty_env_value(Some(String::new())), None);
        assert_eq!(
            nonempty_env_value(Some("/tmp/bowline-secrets".to_string())).as_deref(),
            Some("/tmp/bowline-secrets")
        );
    }

    #[test]
    fn device_id_prefers_explicit_then_persisted_daemon_env() {
        assert_eq!(
            configured_device_id_from(Some("device_env".to_string()), None).as_deref(),
            Some("device_env")
        );
        assert_eq!(
            configured_device_id_from(
                Some("device_env".to_string()),
                Some("device_persisted".to_string())
            )
            .as_deref(),
            Some("device_env")
        );
        assert_eq!(
            configured_device_id_from(None, Some("device_persisted".to_string())).as_deref(),
            Some("device_persisted")
        );
        assert_eq!(configured_device_id_from(None, None), None);
    }

    #[test]
    fn persisted_daemon_env_value_reads_device_id() {
        let temp = tempfile_dir("bowline-runtime-daemon-env");
        std::fs::write(
            temp.join("daemon.env"),
            "BOWLINE_DEVICE_ID=device_remote\nBOWLINE_DEVICE_NAME=remote\n",
        )
        .expect("daemon env");

        assert_eq!(
            persisted_daemon_env_value(&temp, "BOWLINE_DEVICE_ID").as_deref(),
            Some("device_remote")
        );
        assert_eq!(persisted_daemon_env_value(&temp, "MISSING"), None);
        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn keychain_store_requires_explicit_probe_opt_in() {
        assert!(!keychain_secret_store_allowed_from(Some("keychain"), None));
        assert!(!keychain_secret_store_allowed_from(
            Some("keychain"),
            Some("0")
        ));
        assert!(keychain_secret_store_allowed_from(
            Some("keychain"),
            Some("1")
        ));
        assert!(keychain_secret_store_allowed_from(
            Some("keychain"),
            Some("true")
        ));
        assert!(!keychain_secret_store_allowed_from(
            Some("server-local"),
            Some("1")
        ));
    }

    #[test]
    fn active_workspace_id_can_scope_account_without_local_metadata_probe() {
        assert_eq!(
            workspace_id_from_sources(WorkspaceIdSources {
                hosted_control_plane: true,
                account_id: Some("account_active"),
                ..WorkspaceIdSources::default()
            })
            .as_str(),
            "ws_code"
        );
        assert_eq!(
            workspace_id_from_sources(WorkspaceIdSources {
                hosted_control_plane: true,
                allow_account_probe: true,
                account_id: Some("account_active"),
                ..WorkspaceIdSources::default()
            })
            .as_str(),
            account_scoped_workspace_id("account_active")
        );
        assert_eq!(
            workspace_id_from_sources(WorkspaceIdSources {
                explicit_workspace_id: Some("ws_explicit".to_string()),
                hosted_control_plane: true,
                allow_account_probe: true,
                account_id: Some("account_active"),
                ..WorkspaceIdSources::default()
            })
            .as_str(),
            "ws_explicit"
        );
    }

    #[test]
    fn workos_access_token_sub_can_scope_default_workspace() {
        let token = "eyJhbGciOiJub25lIn0.eyJzdWIiOiJhY2NvdW50X2FjdGl2ZSJ9.";
        let account_id =
            workos_account_id_from_access_token(token).expect("token sub should parse");

        assert_eq!(
            workspace_id_from_sources(WorkspaceIdSources {
                hosted_control_plane: true,
                allow_account_probe: true,
                allow_local_metadata_probe: true,
                account_id: Some(&account_id),
                ..WorkspaceIdSources::default()
            })
            .as_str(),
            account_scoped_workspace_id("account_active")
        );
    }

    #[test]
    fn authenticated_hosted_workspace_id_ignores_stale_local_workspace() {
        assert_eq!(
            workspace_id_from_sources(WorkspaceIdSources {
                hosted_control_plane: true,
                allow_account_probe: true,
                allow_local_metadata_probe: true,
                account_id: Some("account_active"),
                local_workspace_id: Some(bowline_core::ids::WorkspaceId::new("ws_code")),
                ..WorkspaceIdSources::default()
            })
            .as_str(),
            account_scoped_workspace_id("account_active")
        );
        assert_eq!(
            workspace_id_from_sources(WorkspaceIdSources {
                allow_account_probe: true,
                allow_local_metadata_probe: true,
                account_id: Some("account_active"),
                local_workspace_id: Some(bowline_core::ids::WorkspaceId::new("ws_code")),
                ..WorkspaceIdSources::default()
            })
            .as_str(),
            "ws_code"
        );
    }

    #[test]
    fn expired_workos_jwt_is_not_usable() {
        let expired = "eyJhbGciOiJub25lIn0.eyJleHAiOjF9.";

        assert!(!workos_token_is_not_expired(expired));
    }

    #[test]
    fn opaque_workos_token_is_left_to_hosted_verification() {
        assert!(workos_token_is_not_expired("not-a-jwt"));
    }

    #[test]
    fn daemon_device_selection_prefers_matching_fingerprint_and_platform() {
        let devices = vec![
            authorized_device("device_linux", "fp_local", "linux", false),
            authorized_device("device_mac", "fp_local", "macos", false),
            authorized_device("device_old", "fp_local", "macos", true),
        ];

        assert_eq!(
            select_authorized_device_for_identity(&devices, "fp_local", "macos"),
            Some("device_mac")
        );
        assert_eq!(
            select_authorized_device_for_identity(&devices, "fp_local", "windows"),
            Some("device_linux")
        );
        assert_eq!(
            select_authorized_device_for_identity(&devices, "fp_missing", "macos"),
            None
        );
    }

    #[test]
    fn persisted_daemon_device_id_is_scoped_to_requested_workspace() {
        let state_root = tempfile_dir("runtime-daemon-workspace-scope");
        std::fs::write(
            state_root.join("daemon.env"),
            "BOWLINE_WORKSPACE_ID=workspace_a\nBOWLINE_DEVICE_ID=device_a\n",
        )
        .expect("write daemon environment");

        assert_eq!(
            persisted_daemon_device_id_for_workspace(&state_root, &WorkspaceId::new("workspace_a"))
                .as_deref(),
            Some("device_a")
        );
        assert_eq!(
            persisted_daemon_device_id_for_workspace(&state_root, &WorkspaceId::new("workspace_b")),
            None
        );

        std::fs::remove_dir_all(state_root).expect("remove temp dir");
    }

    fn authorized_device(
        device_id: &str,
        fingerprint: &str,
        platform: &str,
        revoked: bool,
    ) -> AuthorizedDeviceRecord {
        AuthorizedDeviceRecord {
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new(device_id),
            device_name: device_id.to_string(),
            platform: platform.to_string(),
            device_fingerprint: fingerprint.to_string(),
            authorized_at: ControlPlaneTimestamp { tick: 1 },
            authorized_by_device_id: None,
            device_authorization_proof_verifier: None,
            revoked_at: revoked.then_some(ControlPlaneTimestamp { tick: 2 }),
        }
    }
}
