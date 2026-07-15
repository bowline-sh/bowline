use super::*;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

const MCP_TOKEN_BYTES: usize = 32;
const MCP_TOKEN_FILE_MAX_BYTES: u64 = 1024;

pub fn issue_agent_mcp_token(
    options: AgentMcpTokenIssueOptions,
) -> Result<AgentMcpTokenCommandOutput, AgentError> {
    let db_path = resolve_db_path(options.db_path)?;
    let store = MetadataStore::open(&db_path)?;
    let lease = load_lease(&store, &options.lease_id, &options.generated_at)?;
    if lease_is_expired(&lease, &options.generated_at) {
        store.revoke_agent_mcp_tokens_for_lease(&lease.id, &options.generated_at)?;
        return Err(AgentError::ToolDenied {
            code: "lease-expired".to_string(),
        });
    }
    if !matches!(
        lease.session_state,
        AgentSessionState::Open | AgentSessionState::Provisional
    ) {
        return Err(AgentError::ToolDenied {
            code: "lease-not-open".to_string(),
        });
    }

    let token = random_token()?;
    let token_hash = token_hash(&token);
    let record_id = format!(
        "mcp_{}",
        stable_token(&format!(
            "{}:{}:{}",
            lease.id.as_str(),
            token_hash,
            options.generated_at
        ))
    );
    let token_dir = db_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("mcp-tokens");
    fs::create_dir_all(&token_dir)?;
    let token_file = token_dir.join(format!(
        "{}-{}.token",
        lease.id.as_str(),
        record_id.as_str()
    ));
    write_owner_token_file(&token_file, &token)?;

    let grants = normalize_grants(options.grants);
    let record = AgentMcpTokenRecord {
        id: record_id,
        workspace_id: lease.workspace_id.clone(),
        project_id: lease.project_id.clone(),
        lease_id: lease.id.clone(),
        token_hash,
        token_file: token_file.display().to_string(),
        grants_json: serde_json::to_string(&grants)?,
        expires_at: lease.expires_at.clone(),
        revoked_at: None,
        created_at: options.generated_at.clone(),
        last_used_at: None,
    };
    store.insert_agent_mcp_token(&record)?;

    Ok(AgentMcpTokenCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::AgentMcpToken,
        generated_at: options.generated_at,
        workspace_id: lease.workspace_id,
        project_id: lease.project_id,
        lease_id: lease.id,
        token_file: token_file.display().to_string(),
        grants,
        expires_at: record.expires_at,
    })
}

pub(super) fn verify_mcp_authority(
    store: &MetadataStore,
    lease: &AgentLease,
    request: &AgentToolInvokeRequest,
    generated_at: &str,
) -> Result<(), String> {
    if request.authority.transport != AgentToolTransport::McpAdapter {
        return Ok(());
    }
    let token_file = request
        .authority
        .mcp_token_file
        .as_deref()
        .ok_or_else(|| "mcp-token-file-required".to_string())?;
    let record = store
        .agent_mcp_token_by_file(token_file)
        .map_err(|_| "mcp-token-lookup-failed".to_string())?
        .ok_or_else(|| "mcp-token-file-not-found".to_string())?;
    if record.lease_id != lease.id {
        return Err("mcp-token-wrong-lease".to_string());
    }
    if record.revoked_at.is_some() {
        return Err("mcp-token-revoked".to_string());
    }
    if token_expired(&record.expires_at, generated_at) {
        store
            .revoke_agent_mcp_tokens_for_lease(&lease.id, generated_at)
            .map_err(|_| "mcp-token-revoke-failed".to_string())?;
        return Err("mcp-token-expired".to_string());
    }
    let token = read_token_file(token_file)?;
    if token.is_empty() {
        return Err("mcp-token-empty".to_string());
    }
    let token_hash = token_hash(&token);
    if token_hash != record.token_hash {
        return Err("mcp-token-mismatch".to_string());
    }
    let grants: Vec<AgentMcpGrant> = serde_json::from_str(&record.grants_json)
        .map_err(|_| "mcp-token-grants-invalid".to_string())?;
    let required = grant_for_tool(request.tool);
    if !grants.contains(&required) {
        return Err("mcp-tool-grant-missing".to_string());
    }
    store
        .mark_agent_mcp_token_used(&token_hash, generated_at)
        .map_err(|_| "mcp-token-use-record-failed".to_string())?;
    Ok(())
}

fn read_token_file(token_file: &str) -> Result<String, String> {
    let path = Path::new(token_file);
    let metadata = fs::symlink_metadata(path).map_err(|_| "mcp-token-file-unreadable")?;
    if !metadata.file_type().is_file() {
        return Err("mcp-token-file-not-regular".to_string());
    }
    if metadata.len() > MCP_TOKEN_FILE_MAX_BYTES {
        return Err("mcp-token-file-too-large".to_string());
    }
    let token = fs::read_to_string(path).map_err(|_| "mcp-token-file-unreadable")?;
    Ok(token.trim().to_string())
}

fn normalize_grants(mut grants: Vec<AgentMcpGrant>) -> Vec<AgentMcpGrant> {
    if grants.is_empty() {
        grants.push(AgentMcpGrant::Read);
    }
    grants.sort();
    grants.dedup();
    grants
}

fn random_token() -> Result<String, AgentError> {
    let mut bytes = [0_u8; MCP_TOKEN_BYTES];
    getrandom::fill(&mut bytes)
        .map_err(|error| io::Error::other(format!("random token generation failed: {error}")))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn token_hash(token: &str) -> String {
    blake3::hash(token.as_bytes()).to_hex().to_string()
}

fn write_owner_token_file(path: &Path, token: &str) -> Result<(), AgentError> {
    let mut options = fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    use std::io::Write as _;
    writeln!(file, "{token}")?;
    Ok(())
}

pub(super) fn grant_for_tool(tool: AgentToolName) -> AgentMcpGrant {
    // Every surviving MCP bridge tool is a read-only reader, so the grant model
    // collapses to a single Read scope (packet 065, Decision 2).
    match tool {
        AgentToolName::WorkspaceStatus
        | AgentToolName::ListCapabilities
        | AgentToolName::ResolvePath
        | AgentToolName::ListOverlayChanges => AgentMcpGrant::Read,
    }
}

fn token_expired(expires_at: &str, generated_at: &str) -> bool {
    expiry_elapsed(expires_at, generated_at)
}
