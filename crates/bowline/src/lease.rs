use std::path::PathBuf;

use bowline_core::{
    commands::{
        AgentLease, AgentWriteTargetMode, CONTRACT_VERSION, CommandName, DeviceCommandAction,
        DevicesCommandOutput,
    },
    devices::{DeviceRecord, DeviceTrustState, RecoveryKeyState},
    ids::{DeviceApprovalRequestId, LeaseId, ProjectId, WorkspaceId},
    status::RepairCommand,
};
use bowline_local::{
    init::InitOptions,
    metadata::{MetadataStore, SetupReceiptRecord},
    trust::{self, DeviceRequestOptions},
};

use crate::{
    default_database_path, generated_at, io_helpers, metadata_db_path, resolve_explicit_path,
    runtime,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseJoinArgs {
    pub root: String,
    pub lease_id: Option<String>,
    pub runtime: Option<String>,
    pub request_id: Option<String>,
    pub token_env: Option<String>,
    pub lease_json_env: String,
}

pub fn run_join(args: LeaseJoinArgs, generated_at: String) -> Result<DevicesCommandOutput, String> {
    let root = resolve_explicit_path(args.root);
    let workspace_id = runtime::active_workspace_id();
    bowline_local::init::initialize_root_with_workspace(
        InitOptions {
            db_path: metadata_db_path(),
            requested_root: Some(root.clone()),
            generated_at: generated_at.clone(),
        },
        workspace_id.clone(),
    )
    .map_err(|error| error.to_string())?;

    let control_plane = runtime::control_plane_with_bootstrap_token(bootstrap_token_from_env(
        args.token_env.as_deref(),
    )?)?;
    let key_store = runtime::key_store()?;
    if let Some(request_id) = args.request_id {
        let grant = trust::accept_device_grant(
            &*control_plane,
            &*key_store,
            &workspace_id,
            &DeviceApprovalRequestId::new(request_id),
            &runtime::device_id(),
        )
        .map_err(|error| error.to_string())?;
        let identity = key_store
            .load_or_create_device_identity()
            .map_err(|error| error.to_string())?;
        let local_device = DeviceRecord {
            id: runtime::device_id(),
            name: runtime::device_name(),
            workspace_id: workspace_id.clone(),
            platform: runtime::platform(),
            trust_state: DeviceTrustState::Trusted,
            device_fingerprint: identity.fingerprint,
            authorized_at: grant.accepted_at.clone().or(Some(grant.created_at.clone())),
            updated_at: grant.accepted_at.unwrap_or(grant.created_at),
            is_current_device: true,
            limitation_reason: None,
        };
        // The joined device holds a materialized handoff record, not a runnable
        // lease command; the human drives it through the normal work-view verbs.
        let next_actions = Vec::new();
        return Ok(DevicesCommandOutput {
            contract_version: CONTRACT_VERSION,
            command: CommandName::LeaseJoin,
            generated_at,
            action: DeviceCommandAction::Accept,
            workspace_id: Some(workspace_id),
            local_device: Some(local_device.clone()),
            devices: vec![local_device.clone()],
            revoked_devices: Vec::new(),
            pending_requests: Vec::new(),
            created_request: None,
            approved_device: Some(local_device),
            denied_request: None,
            revoked_device: None,
            recovery_key: Some(RecoveryKeyState::missing()),
            next_actions,
        });
    }

    let request = trust::create_device_request(
        &*control_plane,
        &*key_store,
        DeviceRequestOptions {
            workspace_id: workspace_id.clone(),
            device_id: runtime::device_id(),
            device_name: runtime::device_name(),
            platform: runtime::platform(),
            host: args.runtime.clone(),
            lease_id: args.lease_id.clone(),
            root: Some(root.clone()),
            runtime: args.runtime.clone(),
            generated_at: generated_at.clone(),
        },
    )
    .map_err(|error| error.to_string())?;
    let imported_lease = import_lease_from_env(
        &args.lease_json_env,
        args.lease_id.as_deref(),
        &workspace_id,
        &root,
        request.lease_handoff_digest.as_deref(),
    )?;
    if let Some(lease) = imported_lease.as_ref() {
        import_setup_receipts_from_env(
            "BOWLINE_AGENT_RECEIPTS_JSON",
            &lease.workspace_id,
            &lease.project_id,
            request.setup_receipts_digest.as_deref(),
        )?;
    }
    let approve_root = std::env::var("BOWLINE_APPROVER_ROOT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "<trusted-workspace-root>".to_string());
    let approve_command = format!(
        "bowline device approve --root {} --request {} --yes --json",
        io_helpers::shell_word(&approve_root),
        io_helpers::shell_word(request.request_id.as_str())
    );
    let continuation_env = bootstrap_continuation_env(&workspace_id, request.request_id.as_str())?;
    let lease_arg = imported_lease
        .as_ref()
        .map(|lease| format!(" --lease {}", io_helpers::shell_word(lease.id.as_str())))
        .unwrap_or_default();
    let accept_command = format!(
        "{}bowline lease join --root {} --request {}{} --json",
        continuation_env,
        io_helpers::shell_word(&root),
        io_helpers::shell_word(request.request_id.as_str()),
        lease_arg
    );
    Ok(DevicesCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::LeaseJoin,
        generated_at,
        action: DeviceCommandAction::Request,
        workspace_id: Some(workspace_id),
        local_device: None,
        devices: Vec::new(),
        revoked_devices: Vec::new(),
        pending_requests: Vec::new(),
        created_request: Some(request),
        approved_device: None,
        denied_request: None,
        revoked_device: None,
        recovery_key: Some(RecoveryKeyState::missing()),
        next_actions: vec![
            RepairCommand::mutating(
                "Approve this lease device from a trusted machine".to_string(),
                Some(approve_command),
            ),
            RepairCommand::mutating(
                "Accept the encrypted grant on this sandbox".to_string(),
                Some(accept_command),
            ),
        ],
    })
}

fn bootstrap_token_from_env(env_name: Option<&str>) -> Result<Option<String>, String> {
    let explicit_env = env_name.is_some();
    let env_name = env_name.unwrap_or("BOWLINE_BOOTSTRAP_TOKEN");
    if env_name.trim().is_empty() {
        return Err("bootstrap token env name must not be empty".to_string());
    }
    let token = std::env::var(env_name)
        .ok()
        .filter(|value| !value.trim().is_empty());
    if !explicit_env {
        return Ok(token);
    }
    token
        .map(Some)
        .ok_or_else(|| format!("bootstrap token env `{env_name}` is not set"))
}

fn bootstrap_continuation_env(
    workspace_id: &bowline_core::ids::WorkspaceId,
    request_id: &str,
) -> Result<String, String> {
    let Some(token) = std::env::var("BOWLINE_BOOTSTRAP_TOKEN")
        .ok()
        .filter(|value| !value.trim().is_empty())
    else {
        return Ok(String::new());
    };
    let token_file = write_bootstrap_token_file(request_id, &token)?;
    Ok(format!(
        "BOWLINE_WORKSPACE_ID={}; BOWLINE_BOOTSTRAP_TOKEN=\"$(cat {})\"; rm -f {}; export BOWLINE_WORKSPACE_ID BOWLINE_BOOTSTRAP_TOKEN; ",
        io_helpers::shell_word(workspace_id.as_str()),
        io_helpers::shell_word(&token_file.display().to_string()),
        io_helpers::shell_word(&token_file.display().to_string())
    ))
}

fn write_bootstrap_token_file(request_id: &str, token: &str) -> Result<PathBuf, String> {
    let state_dir = default_database_path()
        .map_err(|error| error.to_string())?
        .parent()
        .ok_or_else(|| "metadata database path has no parent".to_string())?
        .join("bootstrap");
    std::fs::create_dir_all(&state_dir).map_err(|error| error.to_string())?;
    let token_path = state_dir.join(format!("{request_id}.token"));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    use std::io::Write;
    let mut file = options
        .open(&token_path)
        .map_err(|error| error.to_string())?;
    if let Err(error) = file
        .write_all(token.as_bytes())
        .map_err(|error| error.to_string())
    {
        let _ = std::fs::remove_file(&token_path);
        return Err(error);
    }
    Ok(token_path)
}

pub fn print_join(args: LeaseJoinArgs, json: bool) -> std::process::ExitCode {
    let generated_at = generated_at();
    match run_join(args, generated_at.clone()) {
        Ok(output) if json => {
            crate::print_json(&output);
            std::process::ExitCode::SUCCESS
        }
        Ok(output) => {
            print!("{}", crate::render_devices_human(&output));
            std::process::ExitCode::SUCCESS
        }
        Err(error) => {
            crate::print_runtime_error(CommandName::LeaseJoin, generated_at, &error, json);
            std::process::ExitCode::from(crate::EXIT_RUNTIME)
        }
    }
}

fn import_lease_from_env(
    env_name: &str,
    expected_lease_id: Option<&str>,
    workspace_id: &WorkspaceId,
    root: &str,
    expected_digest: Option<&str>,
) -> Result<Option<AgentLease>, String> {
    let Some(raw) = std::env::var(env_name)
        .ok()
        .filter(|value| !value.trim().is_empty())
    else {
        return Ok(None);
    };
    let mut lease: AgentLease = serde_json::from_str(&raw).map_err(|error| error.to_string())?;
    let Some(expected_digest) = expected_digest else {
        return Err("lease handoff is missing trusted digest".to_string());
    };
    let actual_digest = lease_handoff_digest(&raw);
    if actual_digest != expected_digest {
        return Err("lease handoff digest mismatch".to_string());
    }
    let Some(expected_lease_id) = expected_lease_id else {
        return Err("lease handoff requires --lease <id>".to_string());
    };
    if lease.id.as_str() != expected_lease_id {
        return Err("lease handoff does not match bootstrap lease".to_string());
    }
    lease.id = LeaseId::new(expected_lease_id.to_string());
    lease.workspace_id = workspace_id.clone();
    lease.device_id = runtime::device_id();
    lease.write_target_mode = AgentWriteTargetMode::WorkView;
    lease.write_target_path = root.to_string();
    lease.work_view_path = root.to_string();
    let store = MetadataStore::open(
        metadata_db_path().ok_or_else(|| "metadata database path is missing".to_string())?,
    )
    .map_err(|error| error.to_string())?;
    store
        .upsert_agent_lease(&lease)
        .map_err(|error| error.to_string())?;
    Ok(Some(lease))
}

fn lease_handoff_digest(lease_json: &str) -> String {
    format!(
        "lease_handoff_blake3:{}",
        blake3::hash(lease_json.as_bytes()).to_hex()
    )
}

fn import_setup_receipts_from_env(
    env_name: &str,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
    expected_digest: Option<&str>,
) -> Result<(), String> {
    let Some(raw) = std::env::var(env_name)
        .ok()
        .filter(|value| !value.trim().is_empty())
    else {
        return Ok(());
    };
    let Some(expected_digest) = expected_digest else {
        return Err("setup receipt handoff is missing trusted digest".to_string());
    };
    let actual_digest = setup_receipts_digest(&raw);
    if actual_digest != expected_digest {
        return Err("setup receipt handoff digest mismatch".to_string());
    }
    let receipts: Vec<SetupReceiptRecord> =
        serde_json::from_str(&raw).map_err(|error| error.to_string())?;
    let store = MetadataStore::open(
        metadata_db_path().ok_or_else(|| "metadata database path is missing".to_string())?,
    )
    .map_err(|error| error.to_string())?;
    for receipt in receipts.into_iter().filter(|receipt| {
        receipt.workspace_id == *workspace_id
            && receipt.project_id.as_ref() == Some(project_id)
            && matches!(receipt.state.as_str(), "completed" | "approved")
    }) {
        store
            .upsert_setup_receipt(&receipt)
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn setup_receipts_digest(receipts_json: &str) -> String {
    format!(
        "setup_receipts_blake3:{}",
        blake3::hash(receipts_json.as_bytes()).to_hex()
    )
}
