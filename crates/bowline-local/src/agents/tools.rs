use super::*;

pub(super) fn resolve_path_tool(
    request: &AgentToolInvokeRequest,
    lease: &AgentLease,
) -> AgentToolResult {
    let Some(path) = request.arguments.get("path").and_then(Value::as_str) else {
        return denied_result(request, "missing-path");
    };
    match scoped_read_path(lease, path) {
        Ok(path) => allowed_payload(
            request,
            "path resolved",
            json!({"path": path.display().to_string()}),
        ),
        Err(_) => denied_result(request, "path-outside-lease"),
    }
}

pub(super) fn list_tree_tool(
    request: &AgentToolInvokeRequest,
    lease: &AgentLease,
) -> AgentToolResult {
    let path = request
        .arguments
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or(".");
    let Ok(root) = scoped_read_path(lease, path) else {
        return denied_result(request, "path-outside-lease");
    };
    if !agent_read_allowed(lease_write_target_path(lease), &root) {
        return denied_result(request, "path-not-agent-readable");
    }
    let mut entries = Vec::new();
    let max_files = effective_max_files(&lease.scopes.read);
    let max_depth = effective_max_depth(&lease.scopes.read);
    let mut stack = vec![(root, 0_u64)];
    while let Some((dir, depth)) = stack.pop() {
        if depth > max_depth || entries.len() as u64 >= max_files {
            break;
        }
        let Ok(read_dir) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in read_dir.flatten() {
            let path = entry.path();
            if !agent_read_allowed(lease_write_target_path(lease), &path) {
                continue;
            }
            entries.push(path.display().to_string());
            if entries.len() as u64 >= max_files {
                break;
            }
            if entry.file_type().is_ok_and(|file_type| file_type.is_dir()) {
                stack.push((path, depth + 1));
            }
        }
    }
    allowed_payload(
        request,
        "bounded tree listed",
        json!({
            "entries": entries,
            "bounds": degraded_bounds_for_scope(&lease.scopes.read),
        }),
    )
}

pub(super) fn read_file_tool(
    request: &AgentToolInvokeRequest,
    lease: &AgentLease,
) -> AgentToolResult {
    let Some(path) = request.arguments.get("path").and_then(Value::as_str) else {
        return denied_result(request, "missing-path");
    };
    let Ok(path) = scoped_read_path(lease, path) else {
        return denied_result(request, "path-outside-lease");
    };
    let Ok(metadata) = fs::metadata(&path) else {
        return denied_result(request, "missing-file");
    };
    if !agent_read_allowed(lease_write_target_path(lease), &path) {
        return denied_result(request, "path-not-agent-readable");
    }
    let max_read_bytes = effective_max_read_bytes(&lease.scopes.read);
    if metadata.len() > max_read_bytes {
        return AgentToolResult {
            request_id: request.request_id.clone(),
            lease_id: request.lease_id.clone(),
            tool: request.tool,
            outcome: AgentToolResultOutcome::Degraded,
            event_id: None,
            receipt_id: None,
            denial: None,
            degraded: Some(degraded_bounds_for_scope(&lease.scopes.read)),
            summary: "file exceeds per-call read bounds".to_string(),
            payload: None,
        };
    }
    match fs::read_to_string(&path) {
        Ok(contents) => allowed_payload(request, "file read", json!({"contents": contents})),
        Err(_) => denied_result(request, "file-not-text"),
    }
}

pub(super) fn search_workspace_tool(
    request: &AgentToolInvokeRequest,
    lease: &AgentLease,
    db_path: &Path,
    generated_at: &str,
) -> AgentToolResult {
    let Some(query) = request
        .arguments
        .get("query")
        .or_else(|| request.arguments.get("q"))
        .and_then(Value::as_str)
    else {
        return denied_result(request, "missing-query");
    };
    let requested_path = request
        .arguments
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or(".");
    let Ok(scoped_path) = scoped_read_path(lease, requested_path) else {
        return denied_result(request, "path-outside-lease");
    };
    if !agent_read_allowed(lease_write_target_path(lease), &scoped_path) {
        return denied_result(request, "path-not-agent-readable");
    }
    let scope_prefix = lease_relative_filter(lease, &scoped_path);
    match crate::search::search_workspace(crate::search::SearchCommandOptions {
        db_path: Some(db_path.to_path_buf()),
        query: query.to_string(),
        requested_path: Some(scoped_path.display().to_string()),
        path_prefix: None,
        generated_at: generated_at.to_string(),
        limit: effective_max_files(&lease.scopes.read) as usize,
        project_identity: Some(lease_index_identity(
            lease,
            scope_prefix.clone(),
            effective_max_files(&lease.scopes.read) as usize,
        )),
    }) {
        Ok(mut output) => {
            prefix_search_result_paths(&mut output, scope_prefix.as_deref());
            allowed_payload(
                request,
                "index-backed search returned",
                serde_json::to_value(output).expect("search output serializes"),
            )
        }
        Err(_) => AgentToolResult {
            request_id: request.request_id.clone(),
            lease_id: request.lease_id.clone(),
            tool: request.tool,
            outcome: AgentToolResultOutcome::Degraded,
            event_id: None,
            receipt_id: None,
            denial: None,
            degraded: Some(degraded_bounds_for_scope(&lease.scopes.read)),
            summary: "search index is degraded for this lease scope".to_string(),
            payload: None,
        },
    }
}

pub(super) fn symbol_lookup_tool(
    request: &AgentToolInvokeRequest,
    lease: &AgentLease,
    db_path: &Path,
    generated_at: &str,
) -> AgentToolResult {
    let Some(query) = request
        .arguments
        .get("name")
        .or_else(|| request.arguments.get("query"))
        .and_then(Value::as_str)
    else {
        return denied_result(request, "missing-symbol-name");
    };
    let requested_path = request
        .arguments
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or(".");
    let Ok(scoped_path) = scoped_read_path(lease, requested_path) else {
        return denied_result(request, "path-outside-lease");
    };
    if !agent_read_allowed(lease_write_target_path(lease), &scoped_path) {
        return denied_result(request, "path-not-agent-readable");
    }
    let scope_prefix = lease_relative_filter(lease, &scoped_path);
    match crate::symbols::lookup_symbols(crate::symbols::SymbolCommandOptions {
        db_path: Some(db_path.to_path_buf()),
        query: query.to_string(),
        requested_path: Some(scoped_path.display().to_string()),
        path_prefix: None,
        generated_at: generated_at.to_string(),
        limit: effective_max_files(&lease.scopes.read) as usize,
        project_identity: Some(lease_index_identity(
            lease,
            scope_prefix.clone(),
            effective_max_files(&lease.scopes.read) as usize,
        )),
    }) {
        Ok(mut output) => {
            prefix_symbol_result_paths(&mut output, scope_prefix.as_deref());
            allowed_payload(
                request,
                "index-backed symbol lookup returned",
                serde_json::to_value(output).expect("symbol output serializes"),
            )
        }
        Err(_) => AgentToolResult {
            request_id: request.request_id.clone(),
            lease_id: request.lease_id.clone(),
            tool: request.tool,
            outcome: AgentToolResultOutcome::Degraded,
            event_id: None,
            receipt_id: None,
            denial: None,
            degraded: Some(degraded_bounds_for_scope(&lease.scopes.read)),
            summary: "symbol index is degraded for this lease scope".to_string(),
            payload: None,
        },
    }
}

pub(super) fn hydration_status_tool(
    request: &AgentToolInvokeRequest,
    store: &MetadataStore,
    lease: &AgentLease,
) -> Result<AgentToolResult, AgentError> {
    let budget = lease_budget_status(
        store,
        &lease.workspace_id,
        &lease.project_id,
        &lease.id,
        lease.hydrate_budget_bytes,
    )?;
    Ok(allowed_payload(
        request,
        "hydration status returned",
        json!({
            "state": "ready",
            "hydrateBudgetBytes": lease.hydrate_budget_bytes,
            "budget": budget,
        }),
    ))
}

pub(super) fn prefix_search_result_paths(
    output: &mut bowline_core::commands::SearchCommandOutput,
    prefix: Option<&str>,
) {
    let Some(prefix) = prefix else {
        return;
    };
    let prefix = normalize_workspace_path(prefix);
    for result in &mut output.results {
        result.path = prefixed_lease_relative_path(&prefix, &result.path);
    }
}

pub(super) fn prefix_symbol_result_paths(
    output: &mut bowline_core::commands::SymbolCommandOutput,
    prefix: Option<&str>,
) {
    let Some(prefix) = prefix else {
        return;
    };
    let prefix = normalize_workspace_path(prefix);
    for result in &mut output.symbols {
        result.path = prefixed_lease_relative_path(&prefix, &result.path);
    }
}

pub(super) fn prefixed_lease_relative_path(prefix: &str, path: &str) -> String {
    let path = normalize_workspace_path(path);
    let prefix = prefix.trim_end_matches('/');
    if path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|suffix| suffix.starts_with('/'))
    {
        return path;
    }
    format!("{}/{}", prefix, path.trim_start_matches('/'))
}

pub(super) fn write_overlay_tool(
    request: &AgentToolInvokeRequest,
    store: &MetadataStore,
    lease: &mut AgentLease,
    generated_at: &str,
) -> Result<(AgentToolResult, Option<AgentWriteEffect>), AgentError> {
    let Some(path) = request.arguments.get("path").and_then(Value::as_str) else {
        return Ok((denied_result(request, "missing-path"), None));
    };
    let Some(contents) = request.arguments.get("contents").and_then(Value::as_str) else {
        return Ok((denied_result(request, "missing-contents"), None));
    };
    if contents.len() as u64 > effective_max_write_bytes(lease) {
        return Ok((denied_result(request, "write-exceeds-lease-bounds"), None));
    }
    let path = match scoped_write_path(lease, path) {
        Ok(path) => path,
        Err(_) => return Ok((denied_result(request, "path-outside-lease"), None)),
    };
    let Some(path_policy) = agent_path_decision(lease_write_target_path(lease), &path) else {
        return Ok((denied_result(request, "path-not-agent-writable"), None));
    };
    if !agent_write_allowed_decision(&path_policy) {
        return Ok((denied_result(request, "path-not-agent-writable"), None));
    }
    let lease_root = expand_display_path(lease_write_target_path(lease));
    if let Some(parent) = path.parent() {
        match create_parent_dirs_without_symlinks(&lease_root, parent) {
            Ok(()) => {}
            Err(AgentError::ToolDenied { .. }) => {
                return Ok((denied_result(request, "path-outside-lease"), None));
            }
            Err(error) => return Err(error),
        }
    }
    ensure_no_symlink_components(&lease_root, &path)?;
    let previous_contents = if path.exists() {
        Some(fs::read(&path)?)
    } else {
        None
    };
    let operation = if previous_contents.is_some() {
        "modify"
    } else {
        "create"
    };
    fs::write(&path, contents)?;
    let write_log_id = format!(
        "write_agent_{}_{}_{}",
        lease.id.as_str(),
        stable_token(&path.display().to_string()),
        stable_token(generated_at)
    );
    let log_result = store.append_local_write_log(&LocalWriteLogRecord {
        id: write_log_id.clone(),
        workspace_id: lease.workspace_id.clone(),
        device_id: lease.device_id.clone(),
        project_id: Some(lease.project_id.clone()),
        path: path.display().to_string(),
        source_path: None,
        operation: operation.to_string(),
        staged_content_id: None,
        policy_classification: path_policy.classification,
        causation_id: request.request_id.clone(),
        settled_at: generated_at.to_string(),
        created_at: generated_at.to_string(),
    });
    if let Err(error) = log_result {
        restore_agent_write_path(&path, previous_contents.as_deref());
        return Err(error.into());
    }
    lease.output_state = AgentLeaseOutputState::Dirty;
    lease.status_summary = "dirty".to_string();
    lease.updated_at = generated_at.to_string();
    Ok((
        allowed_payload(
            request,
            "overlay file written",
            json!({
                "path": path.display().to_string(),
                "bytes": contents.len(),
            }),
        ),
        Some(AgentWriteEffect {
            path,
            previous_contents,
            write_log_id,
        }),
    ))
}

pub(super) fn diff_tool(
    request: &AgentToolInvokeRequest,
    lease: &AgentLease,
    db_path: PathBuf,
) -> Result<AgentToolResult, AgentError> {
    if lease.write_target_mode != AgentWriteTargetMode::WorkView {
        return Ok(denied_result(request, "work-view-required"));
    }
    let diff = diff_work_view(WorkSelectorOptions {
        db_path: Some(db_path),
        selector: lease.work_view_id.as_str().to_string(),
        generated_at: "2026-06-25T00:00:00Z".to_string(),
    })?;
    Ok(allowed_payload(
        request,
        "overlay changes listed",
        json!({"changes": diff.changes}),
    ))
}

pub(super) fn rollback_agent_write_effect(
    store: &MetadataStore,
    previous_lease: &AgentLease,
    effect: &AgentWriteEffect,
) {
    restore_agent_write_path(&effect.path, effect.previous_contents.as_deref());
    let _ = store.connection().execute(
        "DELETE FROM local_write_log WHERE id = ?1",
        rusqlite::params![effect.write_log_id.as_str()],
    );
    let _ = store.upsert_agent_lease(previous_lease);
}

pub(super) fn restore_agent_write_path(path: &Path, previous_contents: Option<&[u8]>) {
    match previous_contents {
        Some(contents) => {
            let _ = fs::write(path, contents);
        }
        None => {
            if path.exists() {
                let _ = fs::remove_file(path);
            }
        }
    }
}

pub(super) fn publish_for_review(
    request: &AgentToolInvokeRequest,
    store: &MetadataStore,
    lease: &mut AgentLease,
    generated_at: &str,
) -> Result<AgentToolResult, AgentError> {
    if lease.write_target_mode != AgentWriteTargetMode::WorkView {
        return Ok(denied_result(request, "work-view-required"));
    }
    let Some(mut work_view) = store.work_view_by_id(&lease.workspace_id, &lease.work_view_id)?
    else {
        return Err(AgentError::MissingWorkView {
            id: lease.work_view_id.as_str().to_string(),
        });
    };
    let previous_work_view = work_view.clone();
    let previous_lease = lease.clone();
    work_view.lifecycle = WorkViewLifecycle::ReviewReady;
    work_view.sync_state = WorkViewSyncState::Attention;
    work_view.attention = vec!["Agent output is ready for review.".to_string()];
    work_view.updated_at = generated_at.to_string();
    store.upsert_work_view(&work_view)?;
    lease.output_state = AgentLeaseOutputState::ReviewReady;
    lease.status_summary = "review-ready".to_string();
    lease.updated_at = generated_at.to_string();
    store.upsert_agent_lease(lease)?;
    if let Err(error) = append_lease_event(
        store,
        lease,
        EventName::LeaseReviewReady,
        EventId::new(format!(
            "evt_review_ready_{}_{}",
            lease.id.as_str(),
            generated_at
        )),
        generated_at,
        "Agent output is ready for review.",
    ) {
        let _ = store.upsert_work_view(&previous_work_view);
        let _ = store.upsert_agent_lease(&previous_lease);
        *lease = previous_lease;
        return Err(error);
    }
    Ok(allowed_payload(
        request,
        "overlay published for review",
        json!({"state": "review-ready"}),
    ))
}

pub(super) fn complete_task(
    request: &AgentToolInvokeRequest,
    store: &MetadataStore,
    lease: &mut AgentLease,
    generated_at: &str,
) -> Result<AgentToolResult, AgentError> {
    let allow_no_review = request
        .arguments
        .get("allowNoReview")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if lease.write_target_mode == AgentWriteTargetMode::WorkView
        && lease.output_state == AgentLeaseOutputState::Dirty
        && !allow_no_review
    {
        return Ok(denied_result(request, "publish-before-complete"));
    }
    let previous_lease = lease.clone();
    lease.execution_state = AgentLeaseExecutionState::Completed;
    lease.cleanup_state = AgentLeaseCleanupState::Retained;
    lease.status_summary = "completed".to_string();
    lease.updated_at = generated_at.to_string();
    store.upsert_agent_lease(lease)?;
    if let Err(error) = append_lease_event(
        store,
        lease,
        EventName::LeaseCompleted,
        EventId::new(format!(
            "evt_completed_{}_{}",
            lease.id.as_str(),
            generated_at
        )),
        generated_at,
        "Agent task completed.",
    ) {
        let _ = store.upsert_agent_lease(&previous_lease);
        *lease = previous_lease;
        return Err(error);
    }
    Ok(allowed_payload(
        request,
        "task completed",
        json!({"executionState": "completed"}),
    ))
}

pub(super) fn run_command_tool(
    request: &AgentToolInvokeRequest,
    store: &MetadataStore,
    lease: &AgentLease,
    generated_at: &str,
) -> Result<AgentToolResult, AgentError> {
    let Some(command) = request.arguments.get("command").and_then(Value::as_str) else {
        return Ok(denied_result(request, "missing-command"));
    };
    let allowed = store
        .setup_receipts(&lease.workspace_id)?
        .iter()
        .any(|receipt| {
            receipt.project_id.as_ref() == Some(&lease.project_id)
                && receipt.command == command
                && matches!(receipt.state.as_str(), "completed" | "approved")
        });
    if !allowed {
        return Ok(denied_result(request, "command-not-declared"));
    }
    let cwd = match scoped_path(lease_write_target_path(lease), ".") {
        Ok(path) => path,
        Err(_) => return Ok(denied_result(request, "path-outside-lease")),
    };
    let output = Command::new("sh")
        .arg("-lc")
        .arg(command)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .output()?;
    let receipt_id = format!(
        "receipt_{}_{}",
        lease.id.as_str(),
        stable_token(generated_at)
    );
    Ok(AgentToolResult {
        request_id: request.request_id.clone(),
        lease_id: lease.id.clone(),
        tool: request.tool,
        outcome: AgentToolResultOutcome::Allowed,
        event_id: None,
        receipt_id: Some(receipt_id),
        denial: None,
        degraded: None,
        summary: "command ran with stripped ambient env".to_string(),
        payload: Some(json_map(json!({
            "exitCode": output.status.code(),
            "stdoutBytes": output.stdout.len(),
            "stderrBytes": output.stderr.len(),
        }))),
    })
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
            safe_next_actions: vec![SafeAction {
                label: "Inspect lease context".to_string(),
                command: Some(format!(
                    "bowline agent context --lease {}",
                    request.lease_id.as_str()
                )),
            }],
        }),
        degraded: None,
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
        degraded: None,
        summary: summary.to_string(),
        payload: Some(json_map(payload)),
    }
}

pub(super) fn degraded_bounds() -> DegradedExplorationBounds {
    degraded_bounds_for_scope(&AgentLeaseScope {
        roots: Vec::new(),
        classifications: Vec::new(),
        max_bytes_per_read: Some(MAX_READ_BYTES),
        max_files_per_request: Some(MAX_TREE_FILES),
        max_depth: Some(MAX_TREE_DEPTH),
    })
}

pub(super) fn degraded_bounds_for_scope(scope: &AgentLeaseScope) -> DegradedExplorationBounds {
    DegradedExplorationBounds {
        max_bytes: effective_max_read_bytes(scope),
        max_files: effective_max_files(scope),
        max_depth: effective_max_depth(scope),
        truncation_reason: "bounded-phase-10-exploration".to_string(),
        continuation: None,
        safe_next_action: SafeAction {
            label: "Use bounded file reads".to_string(),
            command: Some("read_file_at_snapshot".to_string()),
        },
        index_backed_search_unavailable: true,
    }
}

pub(super) fn effective_max_read_bytes(scope: &AgentLeaseScope) -> u64 {
    scope
        .max_bytes_per_read
        .unwrap_or(MAX_READ_BYTES)
        .min(MAX_READ_BYTES)
}

pub(super) fn effective_max_files(scope: &AgentLeaseScope) -> u64 {
    scope
        .max_files_per_request
        .unwrap_or(MAX_TREE_FILES)
        .min(MAX_TREE_FILES)
}

pub(super) fn effective_max_depth(scope: &AgentLeaseScope) -> u64 {
    scope
        .max_depth
        .unwrap_or(MAX_TREE_DEPTH)
        .min(MAX_TREE_DEPTH)
}

pub(super) fn effective_max_write_bytes(lease: &AgentLease) -> u64 {
    effective_max_read_bytes(&lease.scopes.write).min(lease.hydrate_budget_bytes)
}

pub(super) fn transport_allowed(authority: &AgentToolAuthority) -> bool {
    match authority.transport {
        AgentToolTransport::LocalDaemon => authority.peer_credential_checked,
        AgentToolTransport::McpAdapter => false,
    }
}

pub(super) fn tool_allowed_for_blocked_lease(tool: AgentToolName) -> bool {
    matches!(
        tool,
        AgentToolName::WorkspaceStatus
            | AgentToolName::ListCapabilities
            | AgentToolName::ResolvePath
            | AgentToolName::ExplainPathPolicy
            | AgentToolName::ListAttentionItems
            | AgentToolName::ListTreeAtSnapshot
            | AgentToolName::ReadFileAtSnapshot
            | AgentToolName::SearchWorkspace
            | AgentToolName::SymbolLookup
            | AgentToolName::GetHydrationStatus
            | AgentToolName::ListOverlayChanges
            | AgentToolName::DiffSnapshots
            | AgentToolName::InspectSetupReceipts
            | AgentToolName::ProposePolicyChange
            | AgentToolName::RequestHumanDecision
    )
}

pub(super) fn lease_is_expired(lease: &AgentLease, generated_at: &str) -> bool {
    let expires_at = lease.expires_at.trim();
    if expires_at.is_empty() || expires_at == "never" {
        return false;
    }
    let Ok(expires_at) = OffsetDateTime::parse(expires_at, &Rfc3339) else {
        return true;
    };
    let Ok(generated_at) = OffsetDateTime::parse(generated_at, &Rfc3339) else {
        return true;
    };
    expires_at <= generated_at
}
