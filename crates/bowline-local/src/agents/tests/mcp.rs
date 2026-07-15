use super::*;

#[test]
fn mcp_token_allows_read_tools_for_same_user_transport() {
    let (temp, db_path) = seeded_store("agent-lease-mcp-read");
    let project_path = temp.root().join("Code/apps/web");
    let lease = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        task: "mcp read".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        work_view: true,
        force_stale: false,
        device_id: DeviceId::new("device-test"),
        generated_at: now(),
    })
    .expect("lease created")
    .lease;
    let token = issue_agent_mcp_token(AgentMcpTokenIssueOptions {
        db_path: Some(db_path.clone()),
        lease_id: lease.id.clone(),
        grants: vec![],
        generated_at: now(),
    })
    .expect("mcp token");

    for tool in [
        AgentToolName::WorkspaceStatus,
        AgentToolName::ListCapabilities,
    ] {
        let result = invoke_agent_tool(
            Some(db_path.clone()),
            mcp_tool_request(&lease, tool, serde_json::json!({}), &token.token_file),
            now(),
        )
        .expect("tool result");

        assert_eq!(result.outcome, AgentToolResultOutcome::Allowed, "{tool:?}");
    }
}

#[test]
fn mcp_grant_table_keeps_bridge_tools_read_only() {
    for tool in [
        AgentToolName::WorkspaceStatus,
        AgentToolName::ListCapabilities,
        AgentToolName::ResolvePath,
        AgentToolName::ListOverlayChanges,
    ] {
        assert_eq!(
            mcp_token::grant_for_tool(tool),
            AgentMcpGrant::Read,
            "{tool:?}"
        );
    }
}

#[test]
fn mcp_token_file_reads_are_bounded_to_issued_regular_files() {
    let (temp, db_path) = seeded_store("agent-lease-mcp-token-file");
    let project_path = temp.root().join("Code/apps/web");
    let lease = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        task: "mcp token file".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        work_view: true,
        force_stale: false,
        device_id: DeviceId::new("device-test"),
        generated_at: now(),
    })
    .expect("lease created")
    .lease;
    let token = issue_agent_mcp_token(AgentMcpTokenIssueOptions {
        db_path: Some(db_path.clone()),
        lease_id: lease.id.clone(),
        grants: vec![],
        generated_at: now(),
    })
    .expect("mcp token");
    fs::write(&token.token_file, "x".repeat(2048)).expect("oversized token file");

    let result = invoke_agent_tool(
        Some(db_path),
        mcp_tool_request(
            &lease,
            AgentToolName::ListCapabilities,
            serde_json::json!({}),
            &token.token_file,
        ),
        now(),
    )
    .expect("tool result");

    assert_eq!(result.outcome, AgentToolResultOutcome::Denied);
    assert_eq!(
        result.denial.expect("denial").code,
        "mcp-token-file-too-large"
    );
}

#[test]
fn expired_mcp_token_call_revokes_lease_tokens() {
    let (temp, db_path) = seeded_store("agent-lease-mcp-expired");
    let project_path = temp.root().join("Code/apps/web");
    let mut lease = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        task: "mcp expired".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        work_view: true,
        force_stale: false,
        device_id: DeviceId::new("device-test"),
        generated_at: now(),
    })
    .expect("lease created")
    .lease;
    lease.expires_at = "2026-06-25T13:00:00Z".to_string();
    MetadataStore::open(&db_path)
        .expect("store")
        .upsert_agent_lease(&lease)
        .expect("lease expiry");
    let token = issue_agent_mcp_token(AgentMcpTokenIssueOptions {
        db_path: Some(db_path.clone()),
        lease_id: lease.id.clone(),
        grants: vec![],
        generated_at: now(),
    })
    .expect("mcp token");

    let result = invoke_agent_tool(
        Some(db_path.clone()),
        mcp_tool_request(
            &lease,
            AgentToolName::ListCapabilities,
            serde_json::json!({}),
            &token.token_file,
        ),
        "2026-06-25T14:00:00Z".to_string(),
    )
    .expect("tool result");

    assert_eq!(result.outcome, AgentToolResultOutcome::Denied);
    assert_eq!(result.denial.expect("denial").code, "mcp-token-expired");
    let token_contents = fs::read_to_string(&token.token_file).expect("token");
    let token_hash = blake3::hash(token_contents.trim().as_bytes())
        .to_hex()
        .to_string();
    let stored = MetadataStore::open(&db_path)
        .expect("store")
        .agent_mcp_token_by_hash(&token_hash)
        .expect("token lookup")
        .expect("token stored");
    assert_eq!(stored.revoked_at.as_deref(), Some("2026-06-25T14:00:00Z"));
}
