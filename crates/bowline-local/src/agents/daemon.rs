use super::*;

pub fn invoke_agent_tool_from_local_daemon(
    db_path: Option<PathBuf>,
    mut request: AgentToolInvokeRequest,
    peer_credential_checked: bool,
    generated_at: String,
) -> Result<AgentToolResult, AgentError> {
    request.authority = AgentToolAuthority {
        transport: AgentToolTransport::LocalDaemon,
        peer_credential_checked,
        nonce_presented: false,
        mcp_token_file: None,
    };
    invoke_agent_tool(db_path, request, generated_at)
}

pub fn invoke_agent_tool_from_daemon(
    db_path: Option<PathBuf>,
    request: AgentToolInvokeRequest,
    peer_credential_checked: bool,
    generated_at: String,
) -> Result<AgentToolResult, AgentError> {
    invoke_agent_tool_from_daemon_with_checkpoint(
        db_path,
        request,
        peer_credential_checked,
        generated_at,
        || true,
    )
}

pub fn invoke_agent_tool_from_daemon_with_checkpoint(
    db_path: Option<PathBuf>,
    mut request: AgentToolInvokeRequest,
    peer_credential_checked: bool,
    generated_at: String,
    mut checkpoint: impl FnMut() -> bool,
) -> Result<AgentToolResult, AgentError> {
    request.authority.peer_credential_checked = peer_credential_checked;
    if request.authority.transport == AgentToolTransport::LocalDaemon {
        request.authority.nonce_presented = false;
        request.authority.mcp_token_file = None;
    }
    invoke_agent_tool_with_checkpoint(db_path, request, generated_at, &mut checkpoint)
}

pub(super) fn invoke_agent_tool(
    db_path: Option<PathBuf>,
    request: AgentToolInvokeRequest,
    generated_at: String,
) -> Result<AgentToolResult, AgentError> {
    invoke_agent_tool_with_checkpoint(db_path, request, generated_at, &mut || true)
}

fn invoke_agent_tool_with_checkpoint(
    db_path: Option<PathBuf>,
    request: AgentToolInvokeRequest,
    generated_at: String,
    checkpoint: &mut dyn FnMut() -> bool,
) -> Result<AgentToolResult, AgentError> {
    if !transport_allowed(&request.authority) {
        return Ok(denied_result(&request, "transport-not-authorized"));
    }
    let resolved_db_path = resolve_db_path(db_path)?;
    let store = MetadataStore::open(&resolved_db_path)?;
    let lease = load_lease(&store, &request.lease_id, &generated_at)?;
    if let Err(code) =
        super::mcp_token::verify_mcp_authority(&store, &lease, &request, &generated_at)
    {
        return Ok(denied_result(&request, &code));
    }
    if lease_is_expired(&lease, &generated_at) {
        store.revoke_agent_mcp_tokens_for_lease(&lease.id, &generated_at)?;
        return Ok(denied_result(&request, "lease-expired"));
    }
    if !matches!(
        lease.session_state,
        AgentSessionState::Open | AgentSessionState::Provisional
    ) {
        return Ok(denied_result(&request, "lease-not-open"));
    }
    let context = AgentToolInvokeContext {
        request: &request,
        resolved_db_path: &resolved_db_path,
        generated_at: &generated_at,
    };
    match dispatch_agent_tool(&context, &lease, checkpoint)? {
        AgentToolDispatch::Unaudited(result) => Ok(result),
    }
}

struct AgentToolInvokeContext<'a> {
    request: &'a AgentToolInvokeRequest,
    resolved_db_path: &'a Path,
    generated_at: &'a str,
}

enum AgentToolDispatch {
    Unaudited(AgentToolResult),
}

fn dispatch_agent_tool(
    context: &AgentToolInvokeContext<'_>,
    lease: &AgentLease,
    checkpoint: &mut dyn FnMut() -> bool,
) -> Result<AgentToolDispatch, AgentError> {
    Ok(match context.request.tool {
        AgentToolName::WorkspaceStatus => handle_workspace_status(context, lease),
        AgentToolName::ListCapabilities => handle_list_capabilities(context, lease),
        AgentToolName::ResolvePath => handle_resolve_path(context, lease)?,
        AgentToolName::ListOverlayChanges => {
            handle_list_overlay_changes(context, lease, checkpoint)?
        }
    })
}

fn unaudited(result: AgentToolResult) -> AgentToolDispatch {
    AgentToolDispatch::Unaudited(result)
}

fn handle_workspace_status(
    context: &AgentToolInvokeContext<'_>,
    lease: &AgentLease,
) -> AgentToolDispatch {
    let attention = attention_for_lease(lease);
    unaudited(allowed_payload(
        context.request,
        "workspace is available",
        json!({
            "writeTargetMode": lease.write_target_mode,
            "status": status_for_attention(&attention),
            "lease": lease.id.as_str(),
            // Absolute workspace location for this lease, so an orchestrator can
            // discover where the materialized workspace lives without a separate
            // resolve_path call.
            "workspacePath": lease_write_target_path(lease),
            "hostReadiness": host_readiness(lease),
        }),
    ))
}

// Host-readiness reader for the orchestrator bridge (packet 065, Decision 2).
// State is derived from the lease session state: a lease this daemon can serve
// implies the workspace is materialized on this host. The concrete
// "materialized on host" handoff acknowledgement is layered in by plan 070; this
// reader is the surface it enriches rather than a parallel materialization
// signal.
fn host_readiness(lease: &AgentLease) -> Value {
    let state = match lease.session_state {
        AgentSessionState::Open => "ready",
        AgentSessionState::Provisional => "provisional",
        AgentSessionState::Completed => "review-ready",
        AgentSessionState::Cancelled => "cancelled",
    };
    json!({
        "state": state,
        "summary": lease.status_summary,
    })
}

fn handle_list_capabilities(
    context: &AgentToolInvokeContext<'_>,
    lease: &AgentLease,
) -> AgentToolDispatch {
    unaudited(allowed_payload(
        context.request,
        "capabilities listed",
        json!({
            "writeTargetMode": lease.write_target_mode,
            "capabilities": capabilities_for_lease(lease),
        }),
    ))
}

fn handle_resolve_path(
    context: &AgentToolInvokeContext<'_>,
    lease: &AgentLease,
) -> Result<AgentToolDispatch, AgentError> {
    Ok(unaudited(resolve_path_tool(context.request, lease)?))
}

fn handle_list_overlay_changes(
    context: &AgentToolInvokeContext<'_>,
    lease: &AgentLease,
    checkpoint: &mut dyn FnMut() -> bool,
) -> Result<AgentToolDispatch, AgentError> {
    Ok(unaudited(diff_tool(
        context.request,
        lease,
        context.resolved_db_path.to_path_buf(),
        context.generated_at,
        checkpoint,
    )?))
}
