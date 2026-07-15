use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use bowline_core::{
    commands::{AgentCliCapability, AgentCliName},
    events::{
        EventName, EventRedaction, EventSeverity, EventSubject, EventSubjectKind, WorkspaceEvent,
    },
    ids::EventId,
    status::RepairCommand,
};
use bowline_local::metadata::{
    MetadataStore, SyncOperationKind, SyncOperationRecord, SyncOperationState, SyncResourceKey,
};
use bowline_local::sync::{
    ConflictKind, ConflictRecord, ConflictState, transition_conflict_occurrence_state,
};
use serde::Serialize;

use crate::surface::style::{self, Presentation, Role};

const ENV_STATE_ROOT: &str = "BOWLINE_STATE_ROOT";
const PRIVATE_STATE_ROOT: &str = ".bowline";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveArgs {
    pub project_or_path: String,
    pub copy_prompt: bool,
    pub tui: bool,
    pub diff: Option<String>,
    pub agent: Option<ResolveAgent>,
    pub decision: Option<ResolveDecision>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResolveAction {
    List,
    CopyPrompt,
    Diff,
    Agent,
    Accept,
    Reject,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResolveAgent {
    Codex,
    Claude,
    Cursor,
}

impl ResolveAgent {
    pub fn as_str(self) -> &'static str {
        match self {
            ResolveAgent::Codex => "codex",
            ResolveAgent::Claude => "claude",
            ResolveAgent::Cursor => "cursor",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveDecision {
    Accept(String),
    Reject(String),
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolveCommandOutput {
    pub contract_version: u16,
    pub command: &'static str,
    pub generated_at: String,
    pub project_or_path: String,
    pub action: ResolveAction,
    pub conflicts: Vec<ResolveConflict>,
    pub available_agents: Vec<AvailableAgent>,
    pub available_actions: Vec<ResolveAvailableAction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<ResolvePrompt>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<ResolveDiff>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_agent: Option<ResolveAgent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_conflict_id: Option<String>,
    pub status: ResolveStatus,
    pub next_actions: Vec<RepairCommand>,
    #[serde(skip)]
    pub command_failed: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolveConflict {
    pub id: String,
    pub occurrence_version: u64,
    pub state: String,
    pub bundle_path: String,
    pub conflict_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_root: Option<String>,
    pub reason: String,
    pub affected_files: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spans: Vec<ResolveConflictSpan>,
    pub active_view: String,
    pub has_resolution_overlay: bool,
    pub contains_secrets: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolveConflictSpan {
    pub path: String,
    pub base_start_line: u32,
    pub base_end_line: u32,
    pub local_start_line: u32,
    pub local_end_line: u32,
    pub remote_start_line: u32,
    pub remote_end_line: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_context_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_context_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_context_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AvailableAgent {
    pub name: ResolveAgent,
    pub command: String,
    pub capability: AgentCliCapability,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolveAvailableAction {
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    pub mutates: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvePrompt {
    pub conflict_id: String,
    pub bundle_path: String,
    pub resolution_path: String,
    pub redaction: &'static str,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolveDiff {
    pub conflict_id: String,
    pub bundle_path: String,
    pub redaction: &'static str,
    pub affected_files: Vec<String>,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolveStatus {
    pub level: &'static str,
    pub summary: String,
}

pub fn run(args: ResolveArgs, generated_at: String) -> ResolveCommandOutput {
    let action = action_for(&args);
    let project_or_path = args.project_or_path.clone();
    let mut conflicts = discover_conflicts(Path::new(&args.project_or_path));
    conflicts.sort_by(|left, right| left.id.cmp(&right.id));
    let decision_result = apply_decision(
        Path::new(&args.project_or_path),
        &conflicts,
        args.decision.as_ref(),
        &generated_at,
    );
    if decision_result.is_ok() && args.decision.is_some() {
        conflicts = discover_conflicts(Path::new(&args.project_or_path));
        conflicts.sort_by(|left, right| left.id.cmp(&right.id));
    }

    let available_agents = detect_agents();
    let selected_conflict_id = selected_conflict_id(&args);
    let prompt_conflict = selected_conflict_id
        .as_deref()
        .and_then(|id| conflicts.iter().find(|conflict| conflict.id == id))
        .or_else(|| conflicts.first());
    let prompt = if args.copy_prompt || args.agent.is_some() {
        prompt_conflict.map(build_prompt)
    } else {
        None
    };
    let diff = args
        .diff
        .as_deref()
        .and_then(|id| conflicts.iter().find(|conflict| conflict.id == id))
        .map(build_diff);

    let missing_requested_diff = args.diff.is_some() && diff.is_none();
    let secret_agent_denied = requested_agent_secret_scope_denied(&args, &conflicts);
    let command_failed = (args.decision.is_some() && decision_result.is_err())
        || missing_requested_diff
        || secret_agent_denied;
    let available_actions = available_actions(&project_or_path, &conflicts, &available_agents);
    let status = status_for(
        &args,
        &conflicts,
        &available_agents,
        decision_result.as_ref(),
    );
    let next_actions = next_actions(&project_or_path, &conflicts, &available_agents);

    ResolveCommandOutput {
        contract_version: bowline_core::commands::CONTRACT_VERSION,
        command: "resolve",
        generated_at,
        project_or_path,
        action,
        conflicts,
        available_agents,
        available_actions,
        prompt,
        diff,
        requested_agent: args.agent,
        selected_conflict_id,
        status,
        next_actions,
        command_failed,
    }
}

pub fn render_human(output: &ResolveCommandOutput) -> String {
    let pres = Presentation::detect(false);
    let mut lines = vec![format!(
        "{}  {}",
        style::section("Resolve", &pres),
        output.status.summary
    )];
    if output.conflicts.is_empty() {
        lines.push(format!(
            "  {}",
            style::paint(
                &format!("No unresolved conflicts under {}.", output.project_or_path),
                Role::Label,
                &pres,
            )
        ));
    } else {
        for conflict in &output.conflicts {
            lines.push(style::bullet(
                Role::Attention,
                &format!(
                    "{} at {} ({})",
                    conflict.id, conflict.bundle_path, conflict.active_view
                ),
                &pres,
            ));
            for file in &conflict.affected_files {
                lines.push(format!("      {}", style::paint(file, Role::Label, &pres)));
            }
        }
    }

    if !output.available_agents.is_empty() {
        let agents = output
            .available_agents
            .iter()
            .map(|agent| agent.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(format!("{}  {agents}", style::section("Agents", &pres)));
    }

    if let Some(diff) = &output.diff {
        lines.push(String::new());
        lines.push(diff.text.clone());
    } else if let Some(prompt) = &output.prompt {
        lines.push(String::new());
        lines.push(prompt.text.clone());
    } else if !output.next_actions.is_empty() {
        lines.push(String::new());
        lines.push(style::section("Next", &pres));
        for action in &output.next_actions {
            lines.push(match &action.command {
                Some(command) => style::next_action(command, &action.label, &pres),
                None => format!("  {}", style::paint(&action.label, Role::Label, &pres)),
            });
        }
    }

    lines.push(String::new());
    lines.join("\n")
}

mod actions;
mod apply;
mod discovery;
mod prompt;

#[cfg(test)]
mod tests;

pub(crate) use actions::parse_agent;
use actions::*;
use apply::*;
use discovery::*;
use prompt::*;
