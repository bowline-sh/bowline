use std::path::PathBuf;

use bowline_core::{
    commands::{
        AgentCompleteCommandOutput, AgentContextCommandOutput, AgentLeaseBase,
        AgentLeaseCreateCommandOutput, AgentLeaseUpdateCommandOutput, AgentMcpGrant,
        AgentMcpTokenCommandOutput, AgentPromptCommandOutput,
    },
    ids::{DeviceId, LeaseId},
};
use bowline_local::agents::{
    AgentError, AgentLeaseCreateOptions, AgentLeaseExtendOptions, AgentLeaseSelectorOptions,
    AgentMcpTokenIssueOptions, agent_context, agent_prompt, cancel_agent_session,
    complete_agent_session, create_agent_lease, extend_agent_session, issue_agent_mcp_token,
};

use crate::surface::style::{self, Presentation, Role};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentLeaseCreateArgs {
    pub project_path: String,
    pub task: String,
    pub base: AgentLeaseBase,
    pub work_view: bool,
    pub force_stale: bool,
    pub on_device: Option<String>,
    pub remote_runtime: Option<String>,
    pub remote_root: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentLeaseSelectorArgs {
    pub lease_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentLeaseExtendArgs {
    pub lease_id: String,
    pub hours: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentMcpTokenArgs {
    pub lease_id: String,
    pub grants: Vec<AgentMcpGrant>,
}

pub fn parse_base(value: &str) -> Option<AgentLeaseBase> {
    match value {
        "latest-workspace" => Some(AgentLeaseBase::LatestWorkspace),
        "latest:main" => Some(AgentLeaseBase::LatestMain),
        _ => None,
    }
}

pub fn run_lease_create(
    args: AgentLeaseCreateArgs,
    db_path: Option<PathBuf>,
    device_id: DeviceId,
    generated_at: String,
) -> Result<AgentLeaseCreateCommandOutput, AgentError> {
    create_agent_lease(AgentLeaseCreateOptions {
        db_path,
        project_path: args.project_path,
        task: args.task,
        base: args.base,
        work_view: args.work_view,
        force_stale: args.force_stale,
        device_id,
        generated_at,
    })
}

pub fn run_context(
    args: AgentLeaseSelectorArgs,
    db_path: Option<PathBuf>,
    generated_at: String,
) -> Result<AgentContextCommandOutput, AgentError> {
    let mut output = agent_context(AgentLeaseSelectorOptions {
        db_path,
        lease_id: LeaseId::new(args.lease_id),
        generated_at,
    })?;
    output.context.adapter_capabilities = crate::agent_adapters::detect_agent_cli_capabilities();
    Ok(output)
}

pub fn run_prompt(
    args: AgentLeaseSelectorArgs,
    db_path: Option<PathBuf>,
    generated_at: String,
) -> Result<AgentPromptCommandOutput, AgentError> {
    let mut output = agent_prompt(AgentLeaseSelectorOptions {
        db_path,
        lease_id: LeaseId::new(args.lease_id),
        generated_at,
    })?;
    output.prompt.adapter_capabilities = crate::agent_adapters::detect_agent_cli_capabilities();
    Ok(output)
}

pub fn run_complete(
    args: AgentLeaseSelectorArgs,
    db_path: Option<PathBuf>,
    generated_at: String,
) -> Result<AgentCompleteCommandOutput, AgentError> {
    complete_agent_session(AgentLeaseSelectorOptions {
        db_path,
        lease_id: LeaseId::new(args.lease_id),
        generated_at,
    })
}

pub fn run_cancel(
    args: AgentLeaseSelectorArgs,
    db_path: Option<PathBuf>,
    generated_at: String,
) -> Result<AgentLeaseUpdateCommandOutput, AgentError> {
    cancel_agent_session(AgentLeaseSelectorOptions {
        db_path,
        lease_id: LeaseId::new(args.lease_id),
        generated_at,
    })
}

pub fn run_extend(
    args: AgentLeaseExtendArgs,
    db_path: Option<PathBuf>,
    generated_at: String,
) -> Result<AgentLeaseUpdateCommandOutput, AgentError> {
    extend_agent_session(AgentLeaseExtendOptions {
        db_path,
        lease_id: LeaseId::new(args.lease_id),
        hours: args.hours,
        generated_at,
    })
}

pub fn run_mcp_token(
    args: AgentMcpTokenArgs,
    db_path: Option<PathBuf>,
    generated_at: String,
) -> Result<AgentMcpTokenCommandOutput, AgentError> {
    issue_agent_mcp_token(AgentMcpTokenIssueOptions {
        db_path,
        lease_id: LeaseId::new(args.lease_id),
        grants: args.grants,
        generated_at,
    })
}

pub fn render_lease_create_human(output: &AgentLeaseCreateCommandOutput) -> String {
    let pres = Presentation::detect(false);
    let target_label = match output.lease.write_target_mode {
        bowline_core::commands::AgentWriteTargetMode::Direct => "Project",
        bowline_core::commands::AgentWriteTargetMode::WorkView => "Work view",
    };
    let mut rendered = format!(
        "{}  {}\n{}  {}\n{}  {}\n\n",
        style::section("Agent lease", &pres),
        style::paint(output.lease.id.as_str(), Role::Strong, &pres),
        style::section(target_label, &pres),
        output.lease.write_target_path,
        style::section("State", &pres),
        style::paint("active", Role::Ready, &pres),
    );
    if !output.next_actions.is_empty() {
        rendered.push_str(
            &output
                .next_actions
                .iter()
                .map(|action| {
                    action.command.as_ref().map_or_else(
                        || format!("{}  {}\n", style::section("Next", &pres), action.label),
                        |command| {
                            format!(
                                "{}  {}\n  {}\n",
                                style::section("Next", &pres),
                                action.label,
                                command
                            )
                        },
                    )
                })
                .collect::<String>(),
        );
        rendered.push('\n');
    }
    rendered
}

pub fn render_context_human(output: &AgentContextCommandOutput) -> String {
    let pres = Presentation::detect(false);
    let capabilities = output
        .context
        .capabilities
        .iter()
        .map(|capability| style::kebab(&capability.name))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "{}  {}\n{}  {}\n{}  {}\n{}  {}\n{}  {}\n{}  {}\n\n",
        style::section("Agent lease", &pres),
        style::paint(output.context.lease.id.as_str(), Role::Strong, &pres),
        style::section("Target", &pres),
        output.context.lease.write_target_path,
        style::section("State", &pres),
        style::kebab(&output.context.lease.session_state),
        style::section("Freshness", &pres),
        style::kebab(&output.context.freshness),
        style::section("Readiness", &pres),
        style::kebab(&output.context.readiness.state),
        style::section("Capabilities", &pres),
        capabilities,
    )
}

pub fn render_complete_human(output: &AgentCompleteCommandOutput) -> String {
    let pres = Presentation::detect(false);
    let mut rendered = format!(
        "{}  {}\n{}  {}\n",
        style::section("Agent lease", &pres),
        style::paint(output.lease.id.as_str(), Role::Strong, &pres),
        style::section("State", &pres),
        style::paint("completed", Role::Ready, &pres),
    );
    for action in &output.next_actions {
        rendered.push_str(&format!(
            "{}  {}\n",
            style::section("Next", &pres),
            action.label
        ));
        if let Some(command) = action.command.as_deref() {
            rendered.push_str(&format!("  {command}\n"));
        }
    }
    rendered.push('\n');
    rendered
}

pub fn render_lease_update_human(output: &AgentLeaseUpdateCommandOutput) -> String {
    let pres = Presentation::detect(false);
    let state = style::kebab(&output.lease.session_state);
    format!(
        "{}  {}\n{}  {}\n{}  {}\n\n",
        style::section("Agent lease", &pres),
        style::paint(output.lease.id.as_str(), Role::Strong, &pres),
        style::section("State", &pres),
        style::paint(&state, Role::Ready, &pres),
        style::section("Expires", &pres),
        output.lease.expires_at,
    )
}

pub fn render_prompt_human(output: &AgentPromptCommandOutput) -> String {
    format!("{}\n", output.prompt.text)
}
