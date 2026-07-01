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
    };
    invoke_agent_tool(db_path, request, generated_at)
}

pub(super) fn invoke_agent_tool(
    db_path: Option<PathBuf>,
    request: AgentToolInvokeRequest,
    generated_at: String,
) -> Result<AgentToolResult, AgentError> {
    if !transport_allowed(&request.authority) {
        return Ok(denied_result(&request, "transport-not-authorized"));
    }
    let resolved_db_path = resolve_db_path(db_path)?;
    let mut store = MetadataStore::open(&resolved_db_path)?;
    let mut lease = load_lease(&store, &request.lease_id, &generated_at)?;
    reconcile_materialized_hydration_queue(&store, &lease.workspace_id, &generated_at)?;
    if lease_is_expired(&lease, &generated_at) {
        return audit_tool_result(
            &store,
            &lease,
            denied_result(&request, "lease-expired"),
            &generated_at,
        );
    }
    if !matches!(
        lease.execution_state,
        AgentLeaseExecutionState::Active | AgentLeaseExecutionState::Blocked
    ) {
        return audit_tool_result(
            &store,
            &lease,
            denied_result(&request, "lease-not-active"),
            &generated_at,
        );
    }
    if lease.execution_state == AgentLeaseExecutionState::Blocked
        && !tool_allowed_for_blocked_lease(request.tool)
    {
        return audit_tool_result(
            &store,
            &lease,
            denied_result(&request, "lease-blocked"),
            &generated_at,
        );
    }

    let result = match request.tool {
        AgentToolName::WorkspaceStatus => allowed_payload(
            &request,
            "workspace is available",
            json!({
                "status": WorkspaceStatus::healthy(),
                "lease": lease.id.as_str(),
            }),
        ),
        AgentToolName::ListCapabilities => allowed_payload(
            &request,
            "capabilities listed",
            json!({
                "capabilities": capabilities_for_lease(&lease),
            }),
        ),
        AgentToolName::ResolvePath => resolve_path_tool(&request, &lease),
        AgentToolName::ExplainPathPolicy => allowed_payload(
            &request,
            "path policy explained",
            json!({
                "summary": "Lease tools may read/write only inside the lease work view and persisted scope.",
                "writeScope": lease.scopes.write.roots,
            }),
        ),
        AgentToolName::ListAttentionItems => allowed_payload(
            &request,
            "attention items listed",
            json!({
                "attention": attention_for_lease(&lease),
            }),
        ),
        AgentToolName::ListTreeAtSnapshot => list_tree_tool(&request, &lease),
        AgentToolName::ReadFileAtSnapshot => read_file_tool(&request, &lease),
        AgentToolName::SearchWorkspace => {
            search_workspace_tool(&request, &lease, &resolved_db_path, &generated_at)
        }
        AgentToolName::SymbolLookup => {
            symbol_lookup_tool(&request, &lease, &resolved_db_path, &generated_at)
        }
        AgentToolName::RequestHydration => {
            let path = request
                .arguments
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or(".");
            let Ok(scoped_path) = scoped_read_path(&lease, path) else {
                return audit_tool_result(
                    &store,
                    &lease,
                    denied_result(&request, "path-outside-lease"),
                    &generated_at,
                );
            };
            if !agent_read_allowed(lease_write_target_path(&lease), &scoped_path) {
                return audit_tool_result(
                    &store,
                    &lease,
                    denied_result(&request, "path-not-agent-readable"),
                    &generated_at,
                );
            }
            let requested_content_id = request.arguments.get("contentId").and_then(Value::as_str);
            let (requested_bytes, content_id) =
                match hydration_target(&store, &lease, &scoped_path, requested_content_id) {
                    Ok(target) => target,
                    Err(AgentError::ToolDenied { code }) => {
                        return audit_tool_result(
                            &store,
                            &lease,
                            denied_result(&request, &code),
                            &generated_at,
                        );
                    }
                    Err(error) => return Err(error),
                };
            let queue_path = store
                .workspace_relative_path(&lease.workspace_id, &scoped_path.display().to_string())?;
            if let Some(existing) = store
                .hydration_queue(&lease.workspace_id)?
                .into_iter()
                .find(|record| {
                    record.path == queue_path
                        && record.cause == "agent-lease"
                        && (record.state == "queued" || record.state == "completed")
                })
            {
                if hydration_queue_content_matches(
                    existing.content_id.as_ref(),
                    content_id.as_ref(),
                ) {
                    let budget = lease_budget_status(
                        &store,
                        &lease.workspace_id,
                        &lease.project_id,
                        &lease.id,
                        lease.hydrate_budget_bytes,
                    )?;
                    let (summary, state) = if existing.state == "completed" {
                        ("hydration request already completed", "completed")
                    } else {
                        ("hydration request already queued", "queued")
                    };
                    return audit_tool_result(
                        &store,
                        &lease,
                        allowed_payload(
                            &request,
                            summary,
                            json!({
                                "state": state,
                                "reservationId": null,
                                "budget": budget,
                            }),
                        ),
                        &generated_at,
                    );
                }
                return audit_tool_result(
                    &store,
                    &lease,
                    denied_result(&request, "hydration-already-queued-different-content"),
                    &generated_at,
                );
            }
            let reservation = reserve_lease_bytes(
                &mut store,
                HydrationBudgetReservationRequest {
                    workspace_id: &lease.workspace_id,
                    project_id: &lease.project_id,
                    lease_id: &lease.id,
                    path: &scoped_path.display().to_string(),
                    content_id: content_id.as_ref().map(|id| id.as_str()),
                    cause: "agent-lease",
                    requested_bytes,
                    limit_bytes: lease.hydrate_budget_bytes,
                    now: &generated_at,
                },
            )?;
            if !reservation.accepted {
                return audit_tool_result(
                    &store,
                    &lease,
                    denied_result(&request, "hydration-budget-exhausted"),
                    &generated_at,
                );
            }
            let queue_result = store.enqueue_hydration(&HydrationQueueRecord {
                id: format!("hydrate_{}", reservation.reservation_id),
                workspace_id: lease.workspace_id.clone(),
                project_id: Some(lease.project_id.clone()),
                path: scoped_path.display().to_string(),
                content_id,
                priority: "agent-lease".to_string(),
                state: "queued".to_string(),
                cause: "agent-lease".to_string(),
                updated_at: generated_at.clone(),
            });
            if let Err(error) = queue_result {
                let _ = release_reservation(&store, &reservation.reservation_id, &generated_at);
                return Err(error.into());
            }
            if let Err(error) = append_lease_event(
                &store,
                &lease,
                EventName::LeaseHydrationRequested,
                EventId::new(format!(
                    "evt_hydration_{}_{}",
                    lease.id.as_str(),
                    generated_at
                )),
                &generated_at,
                "Agent requested hydration.",
            ) {
                let _ = release_queued_hydration(
                    &store,
                    &lease.workspace_id,
                    &scoped_path.display().to_string(),
                    "agent-lease",
                    &generated_at,
                );
                return Err(error);
            }
            reconcile_materialized_hydration_queue(&store, &lease.workspace_id, &generated_at)?;
            let budget = lease_budget_status(
                &store,
                &lease.workspace_id,
                &lease.project_id,
                &lease.id,
                lease.hydrate_budget_bytes,
            )?;
            let queue_state = store
                .hydration_queue(&lease.workspace_id)?
                .into_iter()
                .find(|record| {
                    record.path == queue_path
                        && record.cause == "agent-lease"
                        && record.id == format!("hydrate_{}", reservation.reservation_id)
                })
                .map(|record| record.state)
                .unwrap_or_else(|| "queued".to_string());
            allowed_payload(
                &request,
                "hydration request accepted",
                json!({
                    "state": queue_state,
                    "reservationId": reservation.reservation_id,
                    "budget": budget,
                }),
            )
        }
        AgentToolName::GetHydrationStatus => hydration_status_tool(&request, &store, &lease)?,
        AgentToolName::WriteOverlayFile => {
            let previous_lease = lease.clone();
            let (result, effect) = write_overlay_tool(&request, &store, &mut lease, &generated_at)?;
            if let Some(effect) = effect {
                if let Err(error) = store.upsert_agent_lease(&lease) {
                    rollback_agent_write_effect(&store, &previous_lease, &effect);
                    return Err(error.into());
                }
                return match audit_tool_result(&store, &lease, result, &generated_at) {
                    Ok(result) => Ok(result),
                    Err(error) => {
                        rollback_agent_write_effect(&store, &previous_lease, &effect);
                        Err(error)
                    }
                };
            }
            result
        }
        AgentToolName::ListOverlayChanges | AgentToolName::DiffSnapshots => {
            diff_tool(&request, &lease, resolved_db_path.clone())?
        }
        AgentToolName::RunCommandWithReceipt => {
            run_command_tool(&request, &store, &lease, &generated_at)?
        }
        AgentToolName::InspectSetupReceipts => {
            let receipts = store
                .setup_receipts(&lease.workspace_id)?
                .into_iter()
                .filter(|receipt| receipt.project_id.as_ref() == Some(&lease.project_id))
                .map(|receipt| receipt.id)
                .collect::<Vec<_>>();
            allowed_payload(
                &request,
                "setup receipts inspected",
                json!({"receiptIds": receipts}),
            )
        }
        AgentToolName::ProposePolicyChange | AgentToolName::RequestHumanDecision => {
            allowed_payload(
                &request,
                "human attention requested",
                json!({"state": "attention"}),
            )
        }
        AgentToolName::PublishOverlayForReview => {
            publish_for_review(&request, &store, &mut lease, &generated_at)?
        }
        AgentToolName::CompleteTask => complete_task(&request, &store, &mut lease, &generated_at)?,
    };
    audit_tool_result(&store, &lease, result, &generated_at)
}
