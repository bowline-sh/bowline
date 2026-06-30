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
};
use bowline_local::metadata::{MetadataStore, SyncOperationRecord};
use serde::Serialize;
use serde_json::Value;

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
    pub next_actions: Vec<ResolveAvailableAction>,
    #[serde(skip)]
    pub command_failed: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolveConflict {
    pub id: String,
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
    let mut lines = Vec::new();
    lines.push(format!("Resolve: {}", output.status.summary));
    if output.conflicts.is_empty() {
        lines.push(format!(
            "No unresolved conflicts under {}.",
            output.project_or_path
        ));
    } else {
        for conflict in &output.conflicts {
            lines.push(format!(
                "- {} at {} ({})",
                conflict.id, conflict.bundle_path, conflict.active_view
            ));
            for file in &conflict.affected_files {
                lines.push(format!("  {}", file));
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
        lines.push(format!("Available agents: {agents}"));
    }

    if let Some(diff) = &output.diff {
        lines.push(String::new());
        lines.push(diff.text.clone());
    } else if let Some(prompt) = &output.prompt {
        lines.push(String::new());
        lines.push(prompt.text.clone());
    } else if !output.next_actions.is_empty() {
        lines.push(String::new());
        lines.push("Actions:".to_string());
        for action in &output.next_actions {
            match &action.command {
                Some(command) => lines.push(format!("- {}: {}", action.label, command)),
                None => lines.push(format!("- {}", action.label)),
            }
        }
    }

    lines.push(String::new());
    lines.join("\n")
}

fn action_for(args: &ResolveArgs) -> ResolveAction {
    match &args.decision {
        Some(ResolveDecision::Accept(_)) => ResolveAction::Accept,
        Some(ResolveDecision::Reject(_)) => ResolveAction::Reject,
        None if args.diff.is_some() => ResolveAction::Diff,
        None if args.agent.is_some() => ResolveAction::Agent,
        None if args.copy_prompt => ResolveAction::CopyPrompt,
        None => ResolveAction::List,
    }
}

fn selected_conflict_id(args: &ResolveArgs) -> Option<String> {
    match &args.decision {
        Some(ResolveDecision::Accept(id)) | Some(ResolveDecision::Reject(id)) => Some(id.clone()),
        None if args.diff.is_some() => args.diff.clone(),
        None => None,
    }
}

fn status_for(
    args: &ResolveArgs,
    conflicts: &[ResolveConflict],
    available_agents: &[AvailableAgent],
    decision_result: Result<&ResolveDecisionApplied, &ResolveError>,
) -> ResolveStatus {
    if let Err(error) = decision_result {
        return ResolveStatus {
            level: "attention",
            summary: error.to_string(),
        };
    }

    if let Some(id) = &args.diff
        && !conflicts.iter().any(|conflict| conflict.id == *id)
    {
        return ResolveStatus {
            level: "attention",
            summary: format!("conflict `{id}` was not found"),
        };
    }

    if let Some(agent) = args.agent
        && !available_agents
            .iter()
            .any(|available| available.name == agent)
    {
        return ResolveStatus {
            level: "limited",
            summary: format!("{} is not available on PATH.", agent.as_str()),
        };
    }

    if requested_agent_secret_scope_denied(args, conflicts) {
        return ResolveStatus {
            level: "attention",
            summary: "secret-bearing conflict requires explicit agent secret-read scope; use --copy-prompt for a redacted handoff".to_string(),
        };
    }

    if let Ok(applied) = decision_result
        && args.decision.is_some()
    {
        if !conflicts.is_empty() {
            return ResolveStatus {
                level: "attention",
                summary: format!(
                    "{}; {} unresolved conflict{} remain",
                    applied.summary,
                    conflicts.len(),
                    if conflicts.len() == 1 { "" } else { "s" }
                ),
            };
        }
        return ResolveStatus {
            level: "healthy",
            summary: applied.summary.clone(),
        };
    }

    if conflicts.is_empty() {
        ResolveStatus {
            level: "healthy",
            summary: "no unresolved conflict bundles found".to_string(),
        }
    } else {
        ResolveStatus {
            level: "attention",
            summary: format!("{} unresolved conflict bundle(s) found", conflicts.len()),
        }
    }
}

fn requested_agent_secret_scope_denied(args: &ResolveArgs, conflicts: &[ResolveConflict]) -> bool {
    if !matches!(action_for(args), ResolveAction::Agent) || agent_secret_scope_allowed() {
        return false;
    }
    let selected = selected_conflict_id(args)
        .as_deref()
        .and_then(|id| conflicts.iter().find(|conflict| conflict.id == id))
        .or_else(|| conflicts.first());
    selected.is_some_and(|conflict| conflict.contains_secrets)
}

fn agent_secret_scope_allowed() -> bool {
    env::var("BOWLINE_ALLOW_SECRET_CONFLICT_AGENT")
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn next_actions(
    project_or_path: &str,
    conflicts: &[ResolveConflict],
    available_agents: &[AvailableAgent],
) -> Vec<ResolveAvailableAction> {
    if conflicts.is_empty() {
        return vec![ResolveAvailableAction {
            label: "Check workspace status".to_string(),
            command: Some(format!("bowline status {}", shell_word(project_or_path))),
        }];
    }

    available_actions(project_or_path, conflicts, available_agents)
}

fn available_actions(
    project_or_path: &str,
    conflicts: &[ResolveConflict],
    available_agents: &[AvailableAgent],
) -> Vec<ResolveAvailableAction> {
    let mut actions = Vec::new();
    if !conflicts.is_empty() {
        actions.push(ResolveAvailableAction {
            label: "Print repair prompt".to_string(),
            command: Some(format!(
                "bowline resolve {} --copy-prompt",
                shell_word(project_or_path)
            )),
        });
        for agent in available_agents {
            actions.push(ResolveAvailableAction {
                label: format!("Prepare {} repair prompt", agent.name.as_str()),
                command: Some(format!(
                    "bowline resolve {} --agent {}",
                    shell_word(project_or_path),
                    agent.name.as_str()
                )),
            });
        }
        for conflict in conflicts {
            actions.push(ResolveAvailableAction {
                label: format!("Open diff {}", conflict.id),
                command: Some(format!(
                    "bowline resolve {} --diff {}",
                    shell_word(project_or_path),
                    shell_word(&conflict.id)
                )),
            });
            actions.push(ResolveAvailableAction {
                label: format!("Accept {}", conflict.id),
                command: Some(format!(
                    "bowline resolve {} --accept {}",
                    shell_word(project_or_path),
                    shell_word(&conflict.id)
                )),
            });
            actions.push(ResolveAvailableAction {
                label: format!("Reject {}", conflict.id),
                command: Some(format!(
                    "bowline resolve {} --reject {}",
                    shell_word(project_or_path),
                    shell_word(&conflict.id)
                )),
            });
        }
    }
    actions
}

fn shell_word(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| matches!(byte, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'/' | b'.' | b'_' | b'-' | b':' | b'+' | b'=' | b'@' | b'%'))
    {
        return value.to_string();
    }

    format!("'{}'", value.replace('\'', "'\\''"))
}

fn detect_agents() -> Vec<AvailableAgent> {
    crate::agent_adapters::detect_agent_cli_capabilities()
        .into_iter()
        .filter_map(|capability| {
            if !capability.available {
                return None;
            }
            let name = resolve_agent_for_cli_name(capability.name)?;
            let command = capability.command.clone()?;
            Some(AvailableAgent {
                name,
                command,
                capability,
            })
        })
        .collect()
}

fn resolve_agent_for_cli_name(name: AgentCliName) -> Option<ResolveAgent> {
    match name {
        AgentCliName::Codex => Some(ResolveAgent::Codex),
        AgentCliName::Claude => Some(ResolveAgent::Claude),
        AgentCliName::Cursor => Some(ResolveAgent::Cursor),
    }
}

pub fn parse_agent(value: &str) -> Option<ResolveAgent> {
    crate::agent_adapters::parse_cli_name(value).and_then(resolve_agent_for_cli_name)
}

fn discover_conflicts(path: &Path) -> Vec<ResolveConflict> {
    let mut conflicts = Vec::new();
    if let Some(conflict) = read_bundle(path) {
        conflicts.push(conflict);
        return conflicts;
    }

    for root in conflict_roots_for(path) {
        if let Ok(entries) = fs::read_dir(root) {
            for entry in entries.flatten() {
                if let Some(conflict) = read_bundle(&entry.path()) {
                    conflicts.push(conflict);
                }
            }
        }
    }
    conflicts
}

fn conflict_roots_for(path: &Path) -> Vec<PathBuf> {
    let mut roots = vec![path.join(PRIVATE_STATE_ROOT).join("conflicts")];
    if let Some(state_root) = state_root_for_conflicts() {
        roots.push(state_root.join("conflicts"));
    }
    roots
}

fn state_root_for_conflicts() -> Option<PathBuf> {
    if let Some(path) = env::var_os(ENV_STATE_ROOT).map(PathBuf::from) {
        return Some(path);
    }
    bowline_local::metadata::default_database_path()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
}

fn read_bundle(path: &Path) -> Option<ResolveConflict> {
    let manifest_path = path.join("manifest.json");
    if !manifest_path.is_file()
        || !path.join("base").is_dir()
        || !path.join("local").is_dir()
        || !path.join("remote").is_dir()
        || !path.join("resolution").is_dir()
    {
        return None;
    }

    let manifest = fs::read_to_string(&manifest_path).ok()?;
    let manifest: Value = serde_json::from_str(&manifest).ok()?;
    let id = string_field(&manifest, &["conflictId", "id"])
        .unwrap_or_else(|| fallback_conflict_id(path));
    let affected_files =
        string_array_field(&manifest, &["affectedFiles", "affectedPaths", "paths"]);
    let active_view =
        string_field(&manifest, &["activeView"]).unwrap_or_else(|| "local".to_string());
    let state = string_field(&manifest, &["state"]).unwrap_or_else(|| "unresolved".to_string());
    if state != "unresolved" {
        return None;
    }
    let reason = string_field(&manifest, &["reason"]).unwrap_or_default();
    let conflict_kind =
        string_field(&manifest, &["conflictKind"]).unwrap_or_else(|| infer_conflict_kind(&reason));
    let contains_secrets = bool_field(&manifest, &["containsSecrets", "secretBearing"]);
    let workspace_root = string_field(&manifest, &["workspaceRoot"]);
    let spans = spans_field(&manifest);

    Some(ResolveConflict {
        id,
        state,
        bundle_path: path.display().to_string(),
        conflict_kind,
        workspace_root,
        reason,
        affected_files,
        spans,
        active_view,
        has_resolution_overlay: path.join("resolution").is_dir(),
        contains_secrets,
    })
}

fn infer_conflict_kind(reason: &str) -> String {
    match reason {
        "delete-versus-edit conflict" => "delete-edit",
        "path kind conflict" => "path-shape",
        "opaque Git state conflict" => "opaque-git",
        "structured text merge did not validate" => "structured-text",
        _ => "text",
    }
    .to_string()
}

fn spans_field(manifest: &Value) -> Vec<ResolveConflictSpan> {
    manifest
        .get("spans")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|span| {
            Some(ResolveConflictSpan {
                path: string_field(span, &["path"])?,
                base_start_line: u32_field(span, &["baseStartLine"])?,
                base_end_line: u32_field(span, &["baseEndLine"])?,
                local_start_line: u32_field(span, &["localStartLine"])?,
                local_end_line: u32_field(span, &["localEndLine"])?,
                remote_start_line: u32_field(span, &["remoteStartLine"])?,
                remote_end_line: u32_field(span, &["remoteEndLine"])?,
                base_context_hash: string_field(span, &["baseContextHash"]),
                local_context_hash: string_field(span, &["localContextHash"]),
                remote_context_hash: string_field(span, &["remoteContextHash"]),
            })
        })
        .collect()
}

#[derive(Debug)]
struct ResolveDecisionApplied {
    summary: String,
}

#[derive(Debug)]
enum ResolveError {
    ConflictNotFound(String),
    MissingResolution(String),
    UnsafePath(String),
    Io(io::Error),
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConflictNotFound(id) => write!(formatter, "conflict `{id}` was not found"),
            Self::MissingResolution(path) => {
                write!(formatter, "resolution overlay is missing `{path}`")
            }
            Self::UnsafePath(path) => write!(formatter, "resolution path `{path}` is unsafe"),
            Self::Io(error) => write!(formatter, "resolve action failed: {error}"),
        }
    }
}

impl From<io::Error> for ResolveError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

fn apply_decision(
    requested_path: &Path,
    conflicts: &[ResolveConflict],
    decision: Option<&ResolveDecision>,
    generated_at: &str,
) -> Result<ResolveDecisionApplied, ResolveError> {
    let Some(decision) = decision else {
        return Ok(ResolveDecisionApplied {
            summary: String::new(),
        });
    };
    let (target_id, accepting) = match decision {
        ResolveDecision::Accept(id) => (id, true),
        ResolveDecision::Reject(id) => (id, false),
    };
    let conflict = conflicts
        .iter()
        .find(|conflict| conflict.id == *target_id)
        .ok_or_else(|| ResolveError::ConflictNotFound(target_id.clone()))?;
    let bundle = PathBuf::from(&conflict.bundle_path);
    if accepting {
        let project_root = project_root_for_bundle(requested_path, &bundle, conflict)?;
        let resolution_root = bundle.join("resolution");
        let mut staged = Vec::with_capacity(conflict.affected_files.len());
        for affected in &conflict.affected_files {
            validate_relative_path(affected)?;
            let source = resolution_root.join(affected);
            let destination = project_root.join(affected);
            staged.push(stage_resolution_file(
                &resolution_root,
                &project_root,
                conflict,
                affected,
                &source,
                &destination,
            )?);
        }
        apply_staged_resolutions(&project_root, staged)?;
        mark_bundle_state(&bundle, "accepted", generated_at)?;
        enqueue_resolution_sync(&project_root, conflict, "accept", generated_at);
        return Ok(ResolveDecisionApplied {
            summary: format!("accepted resolution for conflict `{}`", conflict.id),
        });
    }
    let project_root = project_root_for_bundle(requested_path, &bundle, conflict)?;
    let remote_root = bundle.join("remote");
    let mut staged = Vec::with_capacity(conflict.affected_files.len());
    for affected in &conflict.affected_files {
        validate_relative_path(affected)?;
        let source = remote_root.join(affected);
        let destination = project_root.join(affected);
        let missing_policy = if conflict.reason == "delete-versus-edit conflict" {
            MissingSidePolicy::DeleteDestination
        } else {
            MissingSidePolicy::Error
        };
        staged.push(stage_bundle_side_file(
            &remote_root,
            &project_root,
            affected,
            &source,
            &destination,
            missing_policy,
        )?);
    }
    apply_staged_resolutions(&project_root, staged)?;
    mark_bundle_state(&bundle, "rejected", generated_at)?;
    enqueue_resolution_sync(&project_root, conflict, "reject", generated_at);
    Ok(ResolveDecisionApplied {
        summary: format!("rejected resolution for conflict `{}`", conflict.id),
    })
}

fn enqueue_resolution_sync(
    project_root: &Path,
    conflict: &ResolveConflict,
    decision: &str,
    generated_at: &str,
) {
    let Ok(db_path) = bowline_local::metadata::default_database_path() else {
        return;
    };
    if !db_path.exists() {
        return;
    }
    let Ok(store) = MetadataStore::open(&db_path) else {
        return;
    };
    let Ok(Some(workspace)) = store.current_workspace() else {
        return;
    };
    let Ok(roots) = store.accepted_roots(&workspace.id) else {
        return;
    };
    if !roots
        .iter()
        .any(|root| path_starts_with(project_root, Path::new(root)))
    {
        return;
    }
    let Ok(base) = store.workspace_sync_head(&workspace.id) else {
        return;
    };
    let base = base.map(|head| head.workspace_ref);
    let payload = serde_json::json!({
        "source": "resolve",
        "decision": decision,
        "conflictId": conflict.id,
        "affectedFiles": conflict.affected_files,
    });
    append_resolution_event(
        &store,
        &workspace.id,
        project_root,
        conflict,
        decision,
        generated_at,
    );
    let _ = store.enqueue_sync_operation(&SyncOperationRecord {
        id: format!(
            "resolve:{}:{}:{}",
            conflict.id,
            decision,
            stable_suffix(generated_at)
        ),
        workspace_id: workspace.id.clone(),
        kind: "upload".to_string(),
        state: "queued".to_string(),
        idempotency_key: format!(
            "resolve:{}:{}:{}",
            conflict.id,
            decision,
            base.as_ref()
                .map(|workspace_ref| workspace_ref.version.to_string())
                .unwrap_or_else(|| "no-head".to_string())
        ),
        base_version: base.as_ref().map(|workspace_ref| workspace_ref.version),
        base_snapshot_id: base
            .as_ref()
            .map(|workspace_ref| workspace_ref.snapshot_id.clone()),
        target_snapshot_id: None,
        device_id: None,
        payload_json: payload.to_string(),
        attempt_count: 0,
        claimed_by: None,
        heartbeat_at: None,
        next_attempt_at: None,
        last_error: None,
        created_at: generated_at.to_string(),
        updated_at: generated_at.to_string(),
    });
}

fn append_resolution_event(
    store: &MetadataStore,
    workspace_id: &bowline_core::ids::WorkspaceId,
    project_root: &Path,
    conflict: &ResolveConflict,
    decision: &str,
    generated_at: &str,
) {
    let (name, summary) = match decision {
        "accept" => (
            EventName::ConflictResolutionAccepted,
            format!("Accepted resolution for conflict `{}`.", conflict.id),
        ),
        "reject" => (
            EventName::ConflictResolutionRejected,
            format!("Rejected resolution for conflict `{}`.", conflict.id),
        ),
        _ => return,
    };
    let mut event = WorkspaceEvent::new(
        resolution_event_id(name, &conflict.id, generated_at),
        name,
        generated_at,
        EventSeverity::Info,
        summary,
        workspace_id.clone(),
    );
    let affected_path = conflict
        .affected_files
        .first()
        .cloned()
        .unwrap_or_else(|| project_root.display().to_string());
    event.project_id = store
        .current_project_by_path(&affected_path)
        .ok()
        .flatten()
        .map(|project| project.id);
    event.path = Some(affected_path.clone());
    event.subject = Some(EventSubject {
        kind: EventSubjectKind::Conflict,
        id: conflict.id.clone(),
        path: Some(affected_path),
    });
    event.payload.insert(
        "decision".to_string(),
        serde_json::Value::String(decision.to_string()),
    );
    event.payload.insert(
        "conflictId".to_string(),
        serde_json::Value::String(conflict.id.clone()),
    );
    event.payload.insert(
        "affectedFiles".to_string(),
        serde_json::Value::Array(
            conflict
                .affected_files
                .iter()
                .map(|path| serde_json::Value::String(path.clone()))
                .collect(),
        ),
    );
    event.redaction = EventRedaction::applied(["secret-values-not-included"]);
    let _ = store.append_event(event);
}

fn resolution_event_id(name: EventName, conflict_id: &str, generated_at: &str) -> EventId {
    let input = format!("{name:?}:{conflict_id}:{generated_at}");
    EventId::new(format!(
        "evt_resolve_{}",
        &blake3::hash(input.as_bytes()).to_hex()[..16]
    ))
}

fn stable_suffix(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' => byte as char,
            _ => '_',
        })
        .collect()
}

#[derive(Debug)]
struct StagedResolution {
    affected: String,
    destination: PathBuf,
    action: StagedResolutionAction,
}

#[derive(Debug)]
enum StagedResolutionAction {
    Write(Vec<u8>),
    Delete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MissingSidePolicy {
    Error,
    DeleteDestination,
}

fn stage_resolution_file(
    resolution_root: &Path,
    project_root: &Path,
    conflict: &ResolveConflict,
    affected: &str,
    source: &Path,
    destination: &Path,
) -> Result<StagedResolution, ResolveError> {
    let missing_policy = if conflict.reason == "delete-versus-edit conflict" {
        MissingSidePolicy::DeleteDestination
    } else {
        MissingSidePolicy::Error
    };
    stage_bundle_side_file(
        resolution_root,
        project_root,
        affected,
        source,
        destination,
        missing_policy,
    )
}

fn stage_bundle_side_file(
    source_root: &Path,
    project_root: &Path,
    affected: &str,
    source: &Path,
    destination: &Path,
    missing_policy: MissingSidePolicy,
) -> Result<StagedResolution, ResolveError> {
    reject_existing_symlink_components(source_root, affected)?;
    let metadata = match fs::symlink_metadata(source) {
        Ok(metadata) => metadata,
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                && missing_policy == MissingSidePolicy::DeleteDestination =>
        {
            reject_existing_symlink_components(project_root, affected)?;
            return Ok(StagedResolution {
                affected: affected.to_string(),
                destination: destination.to_path_buf(),
                action: StagedResolutionAction::Delete,
            });
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(ResolveError::MissingResolution(affected.to_string()));
        }
        Err(error) => return Err(ResolveError::Io(error)),
    };
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        return Err(ResolveError::UnsafePath(affected.to_string()));
    }
    if !file_type.is_file() {
        return Err(ResolveError::MissingResolution(affected.to_string()));
    }

    reject_existing_symlink_components(project_root, affected)?;
    let bytes = fs::read(source)?;
    Ok(StagedResolution {
        affected: affected.to_string(),
        destination: destination.to_path_buf(),
        action: StagedResolutionAction::Write(bytes),
    })
}

fn apply_staged_resolutions(
    project_root: &Path,
    staged: Vec<StagedResolution>,
) -> Result<(), ResolveError> {
    for staged_file in &staged {
        preflight_destination(project_root, staged_file)?;
    }

    let mut temp_paths = Vec::with_capacity(staged.len());
    for (index, staged_file) in staged.iter().enumerate() {
        let StagedResolutionAction::Write(bytes) = &staged_file.action else {
            temp_paths.push(None);
            continue;
        };
        let parent = staged_file
            .destination
            .parent()
            .ok_or_else(|| ResolveError::UnsafePath(staged_file.affected.clone()))?;
        let temp_path = parent.join(format!(
            ".bowline-resolve-{}-{index}.tmp",
            std::process::id()
        ));
        if let Err(error) = write_private_temp_file(&temp_path, bytes) {
            cleanup_temp_paths(temp_paths.iter().filter_map(Option::as_ref));
            return Err(error.into());
        }
        temp_paths.push(Some(temp_path));
    }

    for (staged_file, temp_path) in staged.iter().zip(&temp_paths) {
        reject_existing_symlink_components(project_root, &staged_file.affected)?;
        match (&staged_file.action, temp_path) {
            (StagedResolutionAction::Write(_), Some(temp_path)) => {
                if let Err(error) = fs::rename(temp_path, &staged_file.destination) {
                    cleanup_temp_paths(temp_paths.iter().filter_map(Option::as_ref));
                    return Err(error.into());
                }
            }
            (StagedResolutionAction::Delete, None) => {
                if let Err(error) = remove_destination_file(&staged_file.destination) {
                    cleanup_temp_paths(temp_paths.iter().filter_map(Option::as_ref));
                    return Err(error.into());
                }
            }
            _ => return Err(ResolveError::UnsafePath(staged_file.affected.clone())),
        }
    }
    Ok(())
}

fn preflight_destination(
    project_root: &Path,
    staged_file: &StagedResolution,
) -> Result<(), ResolveError> {
    reject_existing_symlink_components(project_root, &staged_file.affected)?;
    let parent = staged_file
        .destination
        .parent()
        .ok_or_else(|| ResolveError::UnsafePath(staged_file.affected.clone()))?;
    fs::create_dir_all(parent)?;
    reject_existing_symlink_components(project_root, &staged_file.affected)?;
    match fs::symlink_metadata(&staged_file.destination) {
        Ok(metadata)
            if metadata.file_type().is_file()
                && matches!(staged_file.action, StagedResolutionAction::Write(_)) =>
        {
            Ok(())
        }
        Ok(metadata)
            if metadata.file_type().is_file()
                && matches!(staged_file.action, StagedResolutionAction::Delete) =>
        {
            Ok(())
        }
        Ok(_) => Err(ResolveError::UnsafePath(staged_file.affected.clone())),
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                && matches!(staged_file.action, StagedResolutionAction::Write(_)) =>
        {
            Ok(())
        }
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                && matches!(staged_file.action, StagedResolutionAction::Delete) =>
        {
            Ok(())
        }
        Err(error) => Err(ResolveError::Io(error)),
    }
}

fn write_private_temp_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)
}

fn cleanup_temp_paths<'a>(paths: impl IntoIterator<Item = &'a PathBuf>) {
    for path in paths {
        let _ = fs::remove_file(path);
    }
}

fn remove_destination_file(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn reject_existing_symlink_components(
    root: &Path,
    relative_path: &str,
) -> Result<(), ResolveError> {
    let mut current = root.to_path_buf();
    for component in Path::new(relative_path).components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(ResolveError::UnsafePath(relative_path.to_string()));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(ResolveError::Io(error)),
        }
    }
    Ok(())
}

fn project_root_for_bundle(
    requested_path: &Path,
    bundle: &Path,
    conflict: &ResolveConflict,
) -> Result<PathBuf, ResolveError> {
    if !is_bundle_path(requested_path) {
        if bundle_is_under_project_conflicts(requested_path, bundle) {
            return Ok(requested_path.to_path_buf());
        }
        if let Some(workspace_root) = conflict.workspace_root.as_deref() {
            let workspace_root = PathBuf::from(workspace_root);
            if same_path(&workspace_root, requested_path)
                || request_covers_all_affected_paths(requested_path, &workspace_root, conflict)?
            {
                return Ok(workspace_root);
            }
            return Err(ResolveError::UnsafePath(format!(
                "state-root conflict bundle belongs to `{}`; requested `{}`",
                workspace_root.display(),
                requested_path.display()
            )));
        }
        if is_state_root_bundle(bundle) {
            return Err(ResolveError::UnsafePath(
                "state-root conflict bundle is missing trusted workspace root metadata".to_string(),
            ));
        }
        return Ok(requested_path.to_path_buf());
    }
    let components = bundle
        .components()
        .map(|component| component.as_os_str().to_string_lossy().to_string())
        .collect::<Vec<_>>();
    if let Some(index) = components
        .iter()
        .position(|component| component == PRIVATE_STATE_ROOT)
    {
        let mut root = PathBuf::new();
        for component in &components[..index] {
            root.push(component);
        }
        return Ok(root);
    }
    Err(ResolveError::UnsafePath(
        "state-root conflict bundles must be accepted from the workspace root, not by direct bundle path"
            .to_string(),
    ))
}

fn request_covers_all_affected_paths(
    requested_path: &Path,
    workspace_root: &Path,
    conflict: &ResolveConflict,
) -> Result<bool, ResolveError> {
    let requested_path =
        fs::canonicalize(requested_path).unwrap_or_else(|_| requested_path.to_path_buf());
    let workspace_root =
        fs::canonicalize(workspace_root).unwrap_or_else(|_| workspace_root.to_path_buf());
    if !requested_path.starts_with(&workspace_root) {
        return Ok(false);
    }
    for affected in &conflict.affected_files {
        validate_relative_path(affected)?;
        let affected_path = workspace_root.join(affected);
        if !affected_path.starts_with(&requested_path) {
            return Ok(false);
        }
    }
    Ok(!conflict.affected_files.is_empty())
}

fn bundle_is_under_project_conflicts(project_root: &Path, bundle: &Path) -> bool {
    path_starts_with(
        bundle,
        &project_root.join(PRIVATE_STATE_ROOT).join("conflicts"),
    )
}

fn is_state_root_bundle(bundle: &Path) -> bool {
    state_root_for_conflicts()
        .map(|state_root| path_starts_with(bundle, &state_root.join("conflicts")))
        .unwrap_or(false)
}

fn path_starts_with(path: &Path, root: &Path) -> bool {
    let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    path.starts_with(root)
}

fn same_path(left: &Path, right: &Path) -> bool {
    let left = fs::canonicalize(left).unwrap_or_else(|_| left.to_path_buf());
    let right = fs::canonicalize(right).unwrap_or_else(|_| right.to_path_buf());
    left == right
}

fn is_bundle_path(path: &Path) -> bool {
    path.join("manifest.json").is_file()
}

fn validate_relative_path(path: &str) -> Result<(), ResolveError> {
    let normalized = bowline_core::workspace_graph::normalize_workspace_path(path);
    if normalized != path
        || normalized.is_empty()
        || normalized.starts_with("../")
        || normalized.contains("/../")
        || normalized == PRIVATE_STATE_ROOT
        || normalized.starts_with(&format!("{PRIVATE_STATE_ROOT}/"))
    {
        return Err(ResolveError::UnsafePath(path.to_string()));
    }
    Ok(())
}

fn mark_bundle_state(bundle: &Path, state: &str, generated_at: &str) -> Result<(), ResolveError> {
    let manifest_path = bundle.join("manifest.json");
    let mut manifest: Value = serde_json::from_str(&fs::read_to_string(&manifest_path)?)
        .map_err(|error| ResolveError::Io(io::Error::new(io::ErrorKind::InvalidData, error)))?;
    let object = manifest
        .as_object_mut()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "manifest must be object"))?;
    object.insert("state".to_string(), Value::String(state.to_string()));
    object.insert(
        format!("{state}At"),
        Value::String(generated_at.to_string()),
    );
    let temp = manifest_path.with_extension("json.tmp");
    match fs::remove_file(&temp) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(ResolveError::Io(error)),
    }
    let bytes = serde_json::to_vec_pretty(&manifest)
        .map_err(|error| ResolveError::Io(io::Error::new(io::ErrorKind::InvalidData, error)))?;
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
    }
    fs::rename(temp, manifest_path)?;
    Ok(())
}

fn build_prompt(conflict: &ResolveConflict) -> ResolvePrompt {
    let resolution_path = Path::new(&conflict.bundle_path)
        .join("resolution")
        .display()
        .to_string();
    let affected = if conflict.affected_files.is_empty() {
        "No affected file list is available in manifest.json.".to_string()
    } else {
        conflict.affected_files.join(", ")
    };
    let secret_note = if conflict.contains_secrets {
        "The bundle is marked secret-bearing. Do not print file contents or environment values."
    } else {
        "Do not print secrets, environment values, tokens, or private file contents."
    };
    let span_note = if conflict.spans.is_empty() {
        "No precise conflict spans were recorded; inspect each affected file as an opaque conflict."
            .to_string()
    } else {
        format!("Conflict spans:\n{}", format_spans(&conflict.spans))
    };
    let kind_note = prompt_kind_note(conflict);
    let text = format!(
        "Repair bowline conflict `{}`.\n\nConflict kind: {}\n{}\nBundle path: {}\nAffected files: {}\n{}\n\nLayout:\n- base/ contains the common ancestor bytes.\n- local/ contains this device's version.\n- remote/ contains the workspace-head version from the other device.\n- resolution/ is the only place you may write repaired files.\n\nRules:\n- Do not use Git, mutate Git state, create branches, stage files, commit, push, or publish.\n- Do not write outside the resolution overlay.\n- Do not modify base/, local/, remote/, manifest.json, or the live project path.\n- Write the final repaired file contents under resolution/ using the same relative paths.\n- {}\n",
        conflict.id,
        conflict.conflict_kind,
        kind_note,
        conflict.bundle_path,
        affected,
        span_note,
        secret_note
    );

    ResolvePrompt {
        conflict_id: conflict.id.clone(),
        bundle_path: conflict.bundle_path.clone(),
        resolution_path,
        redaction: "applied",
        text,
    }
}

fn prompt_kind_note(conflict: &ResolveConflict) -> &'static str {
    match conflict.conflict_kind.as_str() {
        "structured-text" => {
            "The previous automatic merge failed structured validation; keep the final file parseable."
        }
        "opaque-git" => {
            "This is opaque Git state. Do not run Git repair commands; preserve the intended bytes under resolution/."
        }
        "delete-edit" => "One side deleted the path while the other edited it.",
        "path-shape" => {
            "The path shape differs between sides; do not replace directories or symlinks blindly."
        }
        "env-key" => {
            "This is an env key conflict. Do not copy secret values into the prompt or response."
        }
        _ => "Resolve only the unsafe overlap; preserve unrelated safe edits.",
    }
}

fn format_spans(spans: &[ResolveConflictSpan]) -> String {
    spans
        .iter()
        .map(|span| {
            let anchor = match (
                span.base_context_hash.as_deref(),
                span.local_context_hash.as_deref(),
                span.remote_context_hash.as_deref(),
            ) {
                (Some(base), Some(local), Some(remote)) => {
                    format!(" anchors base:{base} local:{local} remote:{remote}")
                }
                _ => String::new(),
            };
            format!(
                "- {} base:{}-{} local:{}-{} remote:{}-{}{}",
                span.path,
                span.base_start_line,
                span.base_end_line,
                span.local_start_line,
                span.local_end_line,
                span.remote_start_line,
                span.remote_end_line,
                anchor,
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn build_diff(conflict: &ResolveConflict) -> ResolveDiff {
    let bundle = Path::new(&conflict.bundle_path);
    let affected = if conflict.affected_files.is_empty() {
        "No affected file list is available in manifest.json.".to_string()
    } else {
        conflict.affected_files.join(", ")
    };
    let text = format!(
        "Conflict diff for `{}`\n\nBundle path: {}\nAffected files: {}\n\nReview paths:\n- base: {}\n- local: {}\n- remote: {}\n- resolution: {}\n\nRedaction: file contents are not printed here. Open these bundle paths locally to inspect bytes, or use copy-prompt to hand the bundle to an agent.\n",
        conflict.id,
        conflict.bundle_path,
        affected,
        bundle.join("base").display(),
        bundle.join("local").display(),
        bundle.join("remote").display(),
        bundle.join("resolution").display()
    );

    ResolveDiff {
        conflict_id: conflict.id.clone(),
        bundle_path: conflict.bundle_path.clone(),
        redaction: "contents-not-printed",
        affected_files: conflict.affected_files.clone(),
        text,
    }
}

fn string_field(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .map(ToString::to_string)
}

fn string_array_field(value: &Value, keys: &[&str]) -> Vec<String> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_array))
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn u32_field(value: &Value, keys: &[&str]) -> Option<u32> {
    keys.iter()
        .find_map(|key| value.get(*key))
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
}

fn bool_field(value: &Value, keys: &[&str]) -> bool {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_bool))
        .unwrap_or(false)
}

fn fallback_conflict_id(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("conflict")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::{ResolveAgent, parse_agent};

    #[test]
    fn parses_known_agents_only() {
        assert_eq!(parse_agent("codex"), Some(ResolveAgent::Codex));
        assert_eq!(parse_agent("claude"), Some(ResolveAgent::Claude));
        assert_eq!(parse_agent("cursor"), Some(ResolveAgent::Cursor));
        assert_eq!(parse_agent("git"), None);
    }
}
