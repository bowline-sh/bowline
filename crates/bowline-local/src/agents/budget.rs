use super::*;

pub fn grant_agent_hydration_budget(
    options: AgentBudgetGrantOptions,
) -> Result<AgentBudgetCommandOutput, AgentError> {
    if options.add_bytes == 0 {
        return Err(AgentError::InvalidLease {
            reason: "hydration budget grant must add at least one byte".to_string(),
        });
    }
    let mut store = MetadataStore::open(resolve_db_path(options.db_path)?)?;
    let mut lease = load_lease(&store, &options.lease_id, &options.generated_at)?;
    let previous_limit_bytes = lease.hydrate_budget_bytes;
    lease.hydrate_budget_bytes = lease.hydrate_budget_bytes.saturating_add(options.add_bytes);
    lease.updated_at = options.generated_at.clone();
    grant_lease_budget_override(&mut store, &lease, options.add_bytes, &options.generated_at)?;
    let budget = lease_budget_status(
        &store,
        &lease.workspace_id,
        &lease.project_id,
        &lease.id,
        lease.hydrate_budget_bytes,
    )?;
    let context = context_for_lease(&store, &lease);
    Ok(AgentBudgetCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::AgentBudget,
        generated_at: options.generated_at,
        workspace_id: lease.workspace_id.clone(),
        project_id: lease.project_id.clone(),
        lease,
        previous_limit_bytes,
        added_bytes: options.add_bytes,
        budget,
        status: context.status,
        next_actions: vec![SafeAction {
            label: "Show agent context".to_string(),
            command: Some(format!(
                "bowline agent context --lease {}",
                options.lease_id.as_str()
            )),
        }],
    })
}
