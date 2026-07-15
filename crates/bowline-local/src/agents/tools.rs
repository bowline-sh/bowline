use super::*;

pub(super) fn resolve_path_tool(
    request: &AgentToolInvokeRequest,
    lease: &AgentLease,
) -> Result<AgentToolResult, AgentError> {
    let Some(path) = request.arguments.get("path").and_then(Value::as_str) else {
        return Ok(denied_result(request, "missing-path"));
    };
    match scoped_read_path(lease, path) {
        Ok(path) => Ok(allowed_payload(
            request,
            "path resolved",
            json!({
                "path": path.display().to_string(),
                "writeTargetMode": lease.write_target_mode,
            }),
        )),
        Err(_) => Ok(denied_result(request, "path-outside-lease")),
    }
}

pub(super) fn diff_tool(
    request: &AgentToolInvokeRequest,
    lease: &AgentLease,
    db_path: PathBuf,
    generated_at: &str,
    checkpoint: &mut dyn FnMut() -> bool,
) -> Result<AgentToolResult, AgentError> {
    if lease.write_target_mode != AgentWriteTargetMode::WorkView {
        return Ok(denied_result(request, "work-view-required"));
    }
    let diff = diff_work_view_with_checkpoint(
        WorkSelectorOptions {
            db_path: Some(db_path),
            selector: lease.work_view_id.as_str().to_string(),
            paths: Vec::new(),
            generated_at: generated_at.to_string(),
        },
        checkpoint,
    )?;
    Ok(allowed_payload(
        request,
        "overlay changes listed",
        json!({"changes": diff.changes}),
    ))
}

pub(super) fn denied_result(request: &AgentToolInvokeRequest, code: &str) -> AgentToolResult {
    AgentToolResult {
        request_id: request.request_id.clone(),
        lease_id: request.lease_id.clone(),
        tool: request.tool,
        outcome: AgentToolResultOutcome::Denied,
        event_id: None,
        receipt_id: None,
        denial: Some(bowline_core::commands::AgentToolDenial {
            code: code.to_string(),
            // Agent-output safe_next_actions are emitted empty (067 handshake).
            safe_next_actions: Vec::new(),
        }),
        summary: format!("tool denied: {code}"),
        payload: None,
    }
}

pub(super) fn allowed_payload(
    request: &AgentToolInvokeRequest,
    summary: &str,
    payload: Value,
) -> AgentToolResult {
    AgentToolResult {
        request_id: request.request_id.clone(),
        lease_id: request.lease_id.clone(),
        tool: request.tool,
        outcome: AgentToolResultOutcome::Allowed,
        event_id: None,
        receipt_id: None,
        denial: None,
        summary: summary.to_string(),
        payload: Some(json_map(payload)),
    }
}

pub(super) fn transport_allowed(authority: &AgentToolAuthority) -> bool {
    match authority.transport {
        AgentToolTransport::LocalDaemon => authority.peer_credential_checked,
        AgentToolTransport::McpAdapter => {
            authority.peer_credential_checked && authority.nonce_presented
        }
    }
}

pub(super) fn lease_is_expired(lease: &AgentLease, generated_at: &str) -> bool {
    expiry_elapsed(&lease.expires_at, generated_at)
}
