use std::{
    error::Error,
    fmt, fs, io,
    path::{Component, Path, PathBuf},
    process::Command,
};

use bowline_core::{
    commands::{
        AgentAuditPointer, AgentBudgetCommandOutput, AgentCapability, AgentCapabilityState,
        AgentContextCommandOutput, AgentContextV1, AgentEnvMaterialization, AgentEnvProfile,
        AgentLease, AgentLeaseBase, AgentLeaseCleanupState, AgentLeaseCreateCommandOutput,
        AgentLeaseExecutionState, AgentLeaseOutputState, AgentLeaseScope, AgentLeaseScopes,
        AgentOutputTarget, AgentOutputTargetKind, AgentProjectReadiness, AgentPrompt,
        AgentPromptCommandOutput, AgentPromptRedaction, AgentReadinessSignal, AgentReadinessState,
        AgentStartWork, AgentToolAuthority, AgentToolCategory, AgentToolInvokeRequest,
        AgentToolName, AgentToolResult, AgentToolResultOutcome, AgentToolTransport,
        AgentWriteTargetMode, CONTRACT_VERSION, CommandName, DegradedExplorationBounds,
    },
    events::{EventName, EventSeverity, EventSubject, EventSubjectKind, WorkspaceEvent},
    ids::{ContentId, DeviceId, EventId, LeaseId, PolicyVersion, ProjectId, WorkViewId},
    policy::{AccessFlag, PathClassification},
    status::{
        SafeAction, StatusItem, StatusItemKind, StatusLevel, StatusSubject, StatusSubjectKind,
        WorkspaceStatus,
    },
    work_views::{WorkViewLifecycle, WorkViewSyncState},
    workspace_graph::NamespaceEntryKind,
    workspace_graph::normalize_workspace_path,
};
use serde_json::{Map, Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::{
    events::LocalEventError,
    hydration_budget::{
        HydrationBudgetReservationRequest, grant_lease_budget_override, lease_budget_status,
        reconcile_materialized_hydration_queue, release_queued_hydration, release_reservation,
        reserve_lease_bytes,
    },
    indexed::IndexedProjectIdentity,
    metadata::{
        HydrationQueueRecord, LocalWriteLogRecord, MetadataError, MetadataStore,
        default_database_path,
    },
    policy::{PathFacts, UserPolicy, classify_path},
    work_views::{
        WorkSelectorOptions, WorkViewError, WorkonOptions, create_work_view, diff_work_view,
        expand_display_path,
    },
};

const DEFAULT_DEVICE_ID: &str = "device-local-agent";
const DEFAULT_POLICY_VERSION: &str = "policy-v1";
const MAX_READ_BYTES: u64 = 256 * 1024;
const MAX_TREE_FILES: u64 = 200;
const MAX_TREE_DEPTH: u64 = 4;

#[derive(Debug, Clone)]
pub struct AgentLeaseCreateOptions {
    pub db_path: Option<PathBuf>,
    pub project_path: String,
    pub task: String,
    pub base: AgentLeaseBase,
    pub hydrate_budget_bytes: u64,
    pub work_view: bool,
    pub device_id: DeviceId,
    pub generated_at: String,
}

#[derive(Debug, Clone)]
pub struct AgentLeaseSelectorOptions {
    pub db_path: Option<PathBuf>,
    pub lease_id: LeaseId,
    pub generated_at: String,
}

#[derive(Debug, Clone)]
pub struct AgentBudgetGrantOptions {
    pub db_path: Option<PathBuf>,
    pub lease_id: LeaseId,
    pub add_bytes: u64,
    pub generated_at: String,
}

struct AgentWriteEffect {
    path: PathBuf,
    previous_contents: Option<Vec<u8>>,
    write_log_id: String,
}

#[derive(Debug)]
pub enum AgentError {
    MissingWorkspace,
    MissingProject { path: String },
    MissingLease { lease_id: LeaseId },
    MissingWorkView { id: String },
    InvalidLease { reason: String },
    ToolDenied { code: String },
    Metadata(MetadataError),
    WorkView(WorkViewError),
    Event(LocalEventError),
    Io(io::Error),
    Json(serde_json::Error),
}

impl fmt::Display for AgentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingWorkspace => write!(formatter, "no bowline workspace is initialized"),
            Self::MissingProject { path } => {
                write!(formatter, "no tracked project was found for `{path}`")
            }
            Self::MissingLease { lease_id } => write!(
                formatter,
                "agent lease `{}` was not found",
                lease_id.as_str()
            ),
            Self::MissingWorkView { id } => {
                write!(formatter, "lease work view `{id}` was not found")
            }
            Self::InvalidLease { reason } => write!(formatter, "agent lease is invalid: {reason}"),
            Self::ToolDenied { code } => write!(formatter, "agent tool denied: {code}"),
            Self::Metadata(error) => error.fmt(formatter),
            Self::WorkView(error) => error.fmt(formatter),
            Self::Event(error) => error.fmt(formatter),
            Self::Io(error) => write!(formatter, "agent file operation failed: {error}"),
            Self::Json(error) => write!(formatter, "agent JSON operation failed: {error}"),
        }
    }
}

impl Error for AgentError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Metadata(error) => Some(error),
            Self::WorkView(error) => Some(error),
            Self::Event(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
            _ => None,
        }
    }
}

impl From<MetadataError> for AgentError {
    fn from(error: MetadataError) -> Self {
        Self::Metadata(error)
    }
}

impl From<WorkViewError> for AgentError {
    fn from(error: WorkViewError) -> Self {
        Self::WorkView(error)
    }
}

impl From<LocalEventError> for AgentError {
    fn from(error: LocalEventError) -> Self {
        Self::Event(error)
    }
}

impl From<io::Error> for AgentError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for AgentError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

pub fn create_agent_lease(
    options: AgentLeaseCreateOptions,
) -> Result<AgentLeaseCreateCommandOutput, AgentError> {
    let db_path = resolve_db_path(options.db_path)?;
    let store = MetadataStore::open(&db_path)?;
    let workspace = store
        .current_workspace()?
        .ok_or(AgentError::MissingWorkspace)?;
    let project = match store.current_project_by_path(&options.project_path)? {
        Some(project) => project,
        None => adopt_materialized_project(&store, &workspace.id, &options.project_path)?
            .ok_or_else(|| AgentError::MissingProject {
                path: options.project_path.clone(),
            })?,
    };
    if options.base == AgentLeaseBase::LatestMain {
        return Err(AgentError::InvalidLease {
            reason: "latest:main base is unavailable until read-only Git observer freshness is available"
                .to_string(),
        });
    }
    let root = store
        .current_workspace_root()?
        .ok_or_else(|| AgentError::InvalidLease {
            reason: "workspace root is missing".to_string(),
        })?;
    let base_snapshot_id = store
        .project_latest_snapshot_id(&workspace.id, &project.id)?
        .ok_or_else(|| AgentError::InvalidLease {
            reason: format!(
                "project `{}` needs a fresh snapshot before an agent lease can start",
                project.path
            ),
        })?;
    let lease_name = lease_work_view_name(&options.task, &options.generated_at);
    let lease_token = stable_token(&format!(
        "{}:{}:{}:{}",
        workspace.id.as_str(),
        project.id.as_str(),
        lease_name,
        options.generated_at
    ));
    let lease_id = LeaseId::new(format!("lease_{lease_token}"));
    let work_view_id = agent_work_view_id(workspace.id.as_str(), project.id.as_str(), &lease_name);
    let project_target_path = display_path_for_project(&root, &project.path);
    let work_view_path = display_path_for_agent_work_view(&root, &project.path, &lease_name);
    let write_target_mode = if options.work_view {
        AgentWriteTargetMode::WorkView
    } else {
        AgentWriteTargetMode::Direct
    };
    let write_target_path = if options.work_view {
        work_view_path.clone()
    } else {
        project_target_path.clone()
    };
    let event_id = EventId::new(format!(
        "evt_lease_created_{}",
        stable_token(lease_id.as_str())
    ));
    let mut lease = AgentLease {
        id: lease_id,
        workspace_id: workspace.id.clone(),
        project_id: project.id.clone(),
        device_id: options.device_id.clone(),
        write_target_mode,
        write_target_path: write_target_path.clone(),
        work_view_id: work_view_id.clone(),
        work_view_path: write_target_path.clone(),
        task: redacted_task_label(&options.task),
        base: options.base,
        base_snapshot_id,
        execution_state: AgentLeaseExecutionState::Blocked,
        output_state: AgentLeaseOutputState::Empty,
        scopes: default_scopes(&write_target_path),
        hydrate_budget_bytes: options.hydrate_budget_bytes,
        env_profile: default_env_profile(write_target_mode),
        env_restrictions: Vec::new(),
        output_target: AgentOutputTarget {
            kind: if options.work_view {
                AgentOutputTargetKind::WorkView
            } else {
                AgentOutputTargetKind::RealProject
            },
            work_view_id: options.work_view.then_some(work_view_id.clone()),
            path: write_target_path.clone(),
        },
        audit: AgentAuditPointer {
            local_event_id: event_id.clone(),
            local_receipt_id: None,
            encrypted_object_pointer: None,
        },
        cleanup_state: AgentLeaseCleanupState::Current,
        status_summary: "creating".to_string(),
        expires_at: "never".to_string(),
        created_at: options.generated_at.clone(),
        updated_at: options.generated_at.clone(),
    };
    recover_provisional_agent_lease_by_id(&store, &lease.id, &options.generated_at)?;
    if store.agent_lease_by_id(&lease.id)?.is_some() {
        return Err(AgentError::InvalidLease {
            reason: "agent lease already exists".to_string(),
        });
    }
    if options.work_view
        && store
            .work_view_by_id(&workspace.id, &lease.work_view_id)?
            .is_some()
    {
        return Err(AgentError::InvalidLease {
            reason: "agent lease work view already exists".to_string(),
        });
    }
    store.upsert_agent_lease(&lease)?;
    if !options.work_view {
        lease.execution_state = AgentLeaseExecutionState::Active;
        lease.status_summary = "active".to_string();
        if let Err(error) =
            persist_created_agent_lease(&store, &lease, event_id, &options.generated_at)
        {
            rollback_provisional_agent_lease(&store, &lease);
            return Err(error);
        }
        return Ok(AgentLeaseCreateCommandOutput {
            contract_version: CONTRACT_VERSION,
            command: CommandName::AgentStart,
            generated_at: options.generated_at,
            workspace_id: workspace.id,
            project_id: project.id,
            lease,
            status: WorkspaceStatus::healthy(),
            next_actions: vec![SafeAction {
                label: "Open the project".to_string(),
                command: Some(format!("cd {}", shell_word(&write_target_path))),
            }],
        });
    }
    drop(store);

    let work_output = create_work_view(WorkonOptions {
        db_path: Some(db_path.clone()),
        project_path: options.project_path.clone(),
        name: lease_name,
        owner_device_id: Some(options.device_id.clone()),
        generated_at: options.generated_at.clone(),
    });
    let store = MetadataStore::open(&db_path)?;
    let work_output = match work_output {
        Ok(output) => output,
        Err(error) => {
            rollback_provisional_agent_lease(&store, &lease);
            return Err(error.into());
        }
    };
    debug_assert_eq!(work_output.work_view.id, lease.work_view_id);
    debug_assert_eq!(work_output.work_view.visible_path, lease.work_view_path);
    lease.execution_state = AgentLeaseExecutionState::Active;
    lease.status_summary = "active".to_string();
    if let Err(error) = persist_created_agent_lease(&store, &lease, event_id, &options.generated_at)
    {
        rollback_created_agent_work_view(&store, &lease);
        return Err(error);
    }

    Ok(AgentLeaseCreateCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::AgentStart,
        generated_at: options.generated_at,
        workspace_id: workspace.id,
        project_id: project.id,
        lease,
        status: WorkspaceStatus::healthy(),
        next_actions: vec![SafeAction {
            label: "Open the agent work view".to_string(),
            command: Some("cd <lease work view>".to_string()),
        }],
    })
}

fn adopt_materialized_project(
    store: &MetadataStore,
    workspace_id: &bowline_core::ids::WorkspaceId,
    requested_path: &str,
) -> Result<Option<crate::metadata::ProjectRecord>, AgentError> {
    let Some(root) = store.current_workspace_root()? else {
        return Ok(None);
    };
    let root_path = expand_display_path(&root);
    let requested = expand_display_path(requested_path);
    let absolute_project = if requested.is_absolute() {
        requested
    } else {
        root_path.join(requested)
    };
    if !absolute_project.is_dir() {
        return Ok(None);
    }
    let Ok(relative) = absolute_project.strip_prefix(&root_path) else {
        return Ok(None);
    };
    let project_path = normalize_workspace_path(&relative.to_string_lossy());
    if project_path.is_empty() {
        return Ok(None);
    }
    let project_id = materialized_project_id_for_path(workspace_id, &project_path);
    let root_id = store
        .accepted_root_id_for_path(workspace_id, &root)?
        .ok_or_else(|| AgentError::InvalidLease {
            reason: "workspace root metadata is missing".to_string(),
        })?;
    store.insert_project(&project_id, workspace_id, &root_id, &project_path, "")?;
    if let Some(head) = store.workspace_sync_head(workspace_id)? {
        store.set_project_latest_snapshot_id(
            workspace_id,
            &project_id,
            &bowline_core::ids::SnapshotId::new(head.workspace_ref.snapshot_id),
        )?;
    }
    store
        .current_project_by_path(&project_path)
        .map_err(Into::into)
}

fn materialized_project_id_for_path(
    workspace_id: &bowline_core::ids::WorkspaceId,
    path: &str,
) -> ProjectId {
    let workspace_hash = blake3::hash(workspace_id.as_str().as_bytes()).to_hex()[..12].to_string();
    if path.is_empty() {
        return ProjectId::new(format!("proj_{workspace_hash}_root"));
    }
    let mut id = format!("proj_{workspace_hash}_");
    for character in path.chars() {
        match character {
            character if character.is_ascii_alphanumeric() => {
                id.push(character.to_ascii_lowercase());
            }
            '/' => id.push('_'),
            '-' => id.push_str("_dash_"),
            '_' => id.push_str("_us_"),
            '.' => id.push_str("_dot_"),
            character => id.push_str(&format!("_x{:x}_", character as u32)),
        }
    }
    while id.contains("__") {
        id = id.replace("__", "_");
    }
    ProjectId::new(id.trim_matches('_').to_string())
}

pub fn agent_context(
    options: AgentLeaseSelectorOptions,
) -> Result<AgentContextCommandOutput, AgentError> {
    let store = MetadataStore::open(resolve_db_path(options.db_path)?)?;
    let lease = load_lease(&store, &options.lease_id, &options.generated_at)?;
    reconcile_materialized_hydration_queue(&store, &lease.workspace_id, &options.generated_at)?;
    let context = context_for_lease(&store, &lease);
    Ok(AgentContextCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::AgentContext,
        generated_at: options.generated_at,
        workspace_id: lease.workspace_id.clone(),
        project_id: lease.project_id.clone(),
        context,
    })
}

pub fn agent_prompt(
    options: AgentLeaseSelectorOptions,
) -> Result<AgentPromptCommandOutput, AgentError> {
    let store = MetadataStore::open(resolve_db_path(options.db_path)?)?;
    let lease = load_lease(&store, &options.lease_id, &options.generated_at)?;
    reconcile_materialized_hydration_queue(&store, &lease.workspace_id, &options.generated_at)?;
    let context = context_for_lease(&store, &lease);
    let allowed_tools = capabilities_for_lease(&lease)
        .into_iter()
        .filter(|capability| capability.state != AgentCapabilityState::Unavailable)
        .map(|capability| capability.name)
        .collect::<Vec<_>>();
    let target_label = match lease.write_target_mode {
        AgentWriteTargetMode::Direct => "Project",
        AgentWriteTargetMode::WorkView => "Work view",
    };
    let target_path = lease_write_target_path(&lease).to_string();
    let review_instructions = if lease.write_target_mode == AgentWriteTargetMode::WorkView {
        format!(
            "When output is ready, run these from the work view:\n1. `bowline agent publish --lease {}`\n2. `bowline agent complete --lease {}`\n\nPublish for review instead of applying changes to the main workspace yourself.",
            lease.id.as_str(),
            lease.id.as_str()
        )
    } else {
        format!(
            "When output is ready, run `bowline agent complete --lease {}` from the project. Your normal filesystem edits are synced by bowline; do not use Git remotes, commits, branches, staging, or pushes as bowline's sync path.",
            lease.id.as_str()
        )
    };
    let prompt = AgentPrompt {
        recipe_id: "default-agent-lease".to_string(),
        recipe_version: 1,
        redaction: AgentPromptRedaction::Applied,
        text: format!(
            "You are helping inside a bowline agent task.\n\nTask: {}\n{}: {}\n\nWork only inside this lease target. Do not commit, push, branch, stage files, or mutate source-control refs on bowline's behalf.\n\n{}",
            lease.task, target_label, target_path, review_instructions
        ),
        allowed_tools,
        output_target: lease.output_target.clone(),
        adapter_capabilities: Vec::new(),
        instructions: context.instructions.clone(),
    };
    Ok(AgentPromptCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::AgentPrompt,
        generated_at: options.generated_at,
        workspace_id: lease.workspace_id.clone(),
        project_id: lease.project_id.clone(),
        lease,
        prompt,
        status: context.status.clone(),
        next_actions: vec![SafeAction {
            label: "Open the lease target".to_string(),
            command: Some(format!("cd {}", shell_word(&target_path))),
        }],
    })
}

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

fn invoke_agent_tool(
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

pub fn default_device_id() -> DeviceId {
    DeviceId::new(DEFAULT_DEVICE_ID)
}

fn context_for_lease(store: &MetadataStore, lease: &AgentLease) -> AgentContextV1 {
    let setup_receipts: Vec<String> = store
        .setup_receipts(&lease.workspace_id)
        .unwrap_or_default()
        .into_iter()
        .filter(|receipt| receipt.project_id.as_ref() == Some(&lease.project_id))
        .map(|receipt| receipt.id)
        .collect();
    let attention = attention_for_lease(lease);
    let status = status_for_attention(&attention);
    let readiness = readiness_for_lease(lease, &attention, setup_receipts.len());
    let target_path = lease_write_target_path(lease).to_string();
    let target_label = lease_target_label(lease);
    AgentContextV1 {
        workspace_id: lease.workspace_id.clone(),
        project_id: lease.project_id.clone(),
        lease: lease.clone(),
        policy_version: PolicyVersion::new(DEFAULT_POLICY_VERSION),
        status,
        write_target_path: target_path.clone(),
        work_view_path: target_path.clone(),
        attention,
        capabilities: capabilities_for_lease(lease),
        index: crate::indexed::build_project_index(
            None,
            Some(target_path.clone()),
            &lease.updated_at,
        )
        .ok()
        .map(|project| project.index_status),
        hydration_budget: lease_budget_status(
            store,
            &lease.workspace_id,
            &lease.project_id,
            &lease.id,
            lease.hydrate_budget_bytes,
        )
        .ok(),
        setup_receipts,
        env: lease.env_profile.clone(),
        scopes: lease.scopes.clone(),
        readiness,
        start_work: AgentStartWork {
            cwd: target_path.clone(),
            context_command: format!("bowline agent context --lease {}", lease.id.as_str()),
            prompt_command: format!("bowline agent prompt --lease {}", lease.id.as_str()),
            safe_next_actions: vec![
                SafeAction {
                    label: format!("Open {target_label}"),
                    command: Some(format!("cd {}", shell_word(&target_path))),
                },
                SafeAction {
                    label: "Read agent context".to_string(),
                    command: Some(format!(
                        "bowline agent context --lease {}",
                        lease.id.as_str()
                    )),
                },
                SafeAction {
                    label: "Render agent prompt".to_string(),
                    command: Some(format!(
                        "bowline agent prompt --lease {}",
                        lease.id.as_str()
                    )),
                },
            ],
        },
        adapter_capabilities: Vec::new(),
        instructions: lease_instructions(lease),
    }
}

fn readiness_for_lease(
    lease: &AgentLease,
    attention: &[StatusItem],
    setup_receipt_count: usize,
) -> AgentProjectReadiness {
    let lease_state = if lease.execution_state == AgentLeaseExecutionState::Active {
        AgentReadinessState::Ready
    } else {
        AgentReadinessState::Blocked
    };
    let output_state = match lease.output_state {
        AgentLeaseOutputState::Empty | AgentLeaseOutputState::Dirty => AgentReadinessState::Ready,
        AgentLeaseOutputState::ReviewReady | AgentLeaseOutputState::Retained => {
            AgentReadinessState::Attention
        }
        AgentLeaseOutputState::Conflicted => AgentReadinessState::Blocked,
        AgentLeaseOutputState::Accepted | AgentLeaseOutputState::Discarded => {
            AgentReadinessState::Limited
        }
    };
    let state = if attention.is_empty()
        && lease_state == AgentReadinessState::Ready
        && output_state == AgentReadinessState::Ready
    {
        AgentReadinessState::Ready
    } else if lease_state == AgentReadinessState::Blocked
        || output_state == AgentReadinessState::Blocked
    {
        AgentReadinessState::Blocked
    } else {
        AgentReadinessState::Attention
    };

    let target_path = lease_write_target_path(lease);
    let target_name = match lease.write_target_mode {
        AgentWriteTargetMode::Direct => "project",
        AgentWriteTargetMode::WorkView => "work-view",
    };
    let target_summary = match lease.write_target_mode {
        AgentWriteTargetMode::Direct => {
            "Agent writes use the real project directory and normal bowline sync."
        }
        AgentWriteTargetMode::WorkView => "Agent writes are isolated to the lease work view.",
    };
    AgentProjectReadiness {
        state,
        signals: vec![
            AgentReadinessSignal {
                name: "lease".to_string(),
                state: lease_state,
                summary: lease.status_summary.clone(),
                next_action: if lease_state == AgentReadinessState::Ready {
                    None
                } else {
                    Some(SafeAction {
                        label: "Inspect lease context".to_string(),
                        command: Some(format!(
                            "bowline agent context --lease {}",
                            lease.id.as_str()
                        )),
                    })
                },
            },
            AgentReadinessSignal {
                name: target_name.to_string(),
                state: AgentReadinessState::Ready,
                summary: target_summary.to_string(),
                next_action: Some(SafeAction {
                    label: format!("Open {}", lease_target_label(lease)),
                    command: Some(format!("cd {}", shell_word(target_path))),
                }),
            },
            AgentReadinessSignal {
                name: "setup-receipts".to_string(),
                state: AgentReadinessState::Ready,
                summary: if setup_receipt_count == 0 {
                    "No setup receipts are required or recorded for this lease.".to_string()
                } else {
                    format!("{setup_receipt_count} setup receipt(s) are visible to this lease.")
                },
                next_action: Some(SafeAction {
                    label: "Inspect setup receipts".to_string(),
                    command: Some(format!(
                        "bowline agent context --lease {}",
                        lease.id.as_str()
                    )),
                }),
            },
            AgentReadinessSignal {
                name: "output".to_string(),
                state: output_state,
                summary: format!("Agent output state is {:?}.", lease.output_state),
                next_action: if output_state == AgentReadinessState::Ready
                    || lease.write_target_mode == AgentWriteTargetMode::Direct
                {
                    None
                } else {
                    Some(SafeAction {
                        label: "Inspect work view diff".to_string(),
                        command: Some(format!("bowline review {}", lease.work_view_id.as_str())),
                    })
                },
            },
        ],
    }
}

fn lease_write_target_path(lease: &AgentLease) -> &str {
    &lease.write_target_path
}

fn lease_target_label(lease: &AgentLease) -> &'static str {
    match lease.write_target_mode {
        AgentWriteTargetMode::Direct => "project",
        AgentWriteTargetMode::WorkView => "work view",
    }
}

fn lease_instructions(lease: &AgentLease) -> Vec<String> {
    let mut instructions = vec![
        "Work only inside the lease target.".to_string(),
        "Use primitive bowline tools for inspection, bounded reads, writes, review, and completion.".to_string(),
        "Do not commit, push, branch, stage files, or mutate source-control refs on bowline's behalf.".to_string(),
    ];
    match lease.write_target_mode {
        AgentWriteTargetMode::Direct => instructions
            .push("Direct lease edits go through normal bowline real-directory sync.".to_string()),
        AgentWriteTargetMode::WorkView => instructions.push(
            "Publish overlay output for review instead of applying it to the main workspace."
                .to_string(),
        ),
    }
    instructions
}

fn capabilities() -> Vec<AgentCapability> {
    let degraded = degraded_bounds();
    [
        (
            AgentToolName::WorkspaceStatus,
            AgentToolCategory::Inspection,
            AgentCapabilityState::Available,
            None,
        ),
        (
            AgentToolName::ListCapabilities,
            AgentToolCategory::Inspection,
            AgentCapabilityState::Available,
            None,
        ),
        (
            AgentToolName::ResolvePath,
            AgentToolCategory::Inspection,
            AgentCapabilityState::Available,
            None,
        ),
        (
            AgentToolName::ExplainPathPolicy,
            AgentToolCategory::Inspection,
            AgentCapabilityState::Available,
            None,
        ),
        (
            AgentToolName::ListAttentionItems,
            AgentToolCategory::Inspection,
            AgentCapabilityState::Available,
            None,
        ),
        (
            AgentToolName::ListTreeAtSnapshot,
            AgentToolCategory::Exploration,
            AgentCapabilityState::Available,
            Some(degraded.clone()),
        ),
        (
            AgentToolName::ReadFileAtSnapshot,
            AgentToolCategory::Exploration,
            AgentCapabilityState::Available,
            Some(degraded.clone()),
        ),
        (
            AgentToolName::SearchWorkspace,
            AgentToolCategory::Exploration,
            AgentCapabilityState::Available,
            None,
        ),
        (
            AgentToolName::SymbolLookup,
            AgentToolCategory::Exploration,
            AgentCapabilityState::Available,
            None,
        ),
        (
            AgentToolName::RequestHydration,
            AgentToolCategory::Hydration,
            AgentCapabilityState::Available,
            None,
        ),
        (
            AgentToolName::GetHydrationStatus,
            AgentToolCategory::Hydration,
            AgentCapabilityState::Available,
            None,
        ),
        (
            AgentToolName::WriteOverlayFile,
            AgentToolCategory::Write,
            AgentCapabilityState::Available,
            None,
        ),
        (
            AgentToolName::ListOverlayChanges,
            AgentToolCategory::Write,
            AgentCapabilityState::Available,
            None,
        ),
        (
            AgentToolName::DiffSnapshots,
            AgentToolCategory::Write,
            AgentCapabilityState::Available,
            None,
        ),
        (
            AgentToolName::RunCommandWithReceipt,
            AgentToolCategory::Execution,
            AgentCapabilityState::Available,
            None,
        ),
        (
            AgentToolName::InspectSetupReceipts,
            AgentToolCategory::Execution,
            AgentCapabilityState::Available,
            None,
        ),
        (
            AgentToolName::ProposePolicyChange,
            AgentToolCategory::Review,
            AgentCapabilityState::Available,
            None,
        ),
        (
            AgentToolName::RequestHumanDecision,
            AgentToolCategory::Review,
            AgentCapabilityState::Available,
            None,
        ),
        (
            AgentToolName::PublishOverlayForReview,
            AgentToolCategory::Review,
            AgentCapabilityState::Available,
            None,
        ),
        (
            AgentToolName::CompleteTask,
            AgentToolCategory::Review,
            AgentCapabilityState::Available,
            None,
        ),
    ]
    .into_iter()
    .map(|(name, category, state, bounds)| AgentCapability {
        name,
        category,
        state,
        bounds,
    })
    .collect()
}

fn capabilities_for_lease(lease: &AgentLease) -> Vec<AgentCapability> {
    capabilities()
        .into_iter()
        .filter(|capability| {
            lease.write_target_mode == AgentWriteTargetMode::WorkView
                || !matches!(
                    capability.name,
                    AgentToolName::PublishOverlayForReview
                        | AgentToolName::ListOverlayChanges
                        | AgentToolName::DiffSnapshots
                )
        })
        .collect()
}

fn default_scopes(path: &str) -> AgentLeaseScopes {
    let scope = AgentLeaseScope {
        roots: vec![path.to_string()],
        classifications: Vec::new(),
        max_bytes_per_read: Some(MAX_READ_BYTES),
        max_files_per_request: Some(MAX_TREE_FILES),
        max_depth: Some(MAX_TREE_DEPTH),
    };
    AgentLeaseScopes {
        read: scope.clone(),
        write: scope,
    }
}

fn default_env_profile(write_target_mode: AgentWriteTargetMode) -> AgentEnvProfile {
    AgentEnvProfile {
        name: "default".to_string(),
        materialization: match write_target_mode {
            AgentWriteTargetMode::Direct => AgentEnvMaterialization::ProjectPath,
            AgentWriteTargetMode::WorkView => AgentEnvMaterialization::LeaseWorkView,
        },
        available_keys: Vec::new(),
        restrictions: Vec::new(),
        grant_ids: Vec::new(),
    }
}

fn shell_word(value: &str) -> String {
    if !value.is_empty()
        && value.bytes().all(|byte| {
            matches!(
                byte,
                b'a'..=b'z'
                    | b'A'..=b'Z'
                    | b'0'..=b'9'
                    | b'/'
                    | b'.'
                    | b'_'
                    | b'-'
                    | b':'
                    | b'+'
                    | b'='
                    | b'@'
                    | b'%'
                    | b'~'
            )
        })
    {
        return value.to_string();
    }

    format!("'{}'", value.replace('\'', "'\\''"))
}

fn attention_for_lease(lease: &AgentLease) -> Vec<StatusItem> {
    let Some((summary, event_name)) = lease_attention_summary(lease) else {
        return Vec::new();
    };
    vec![StatusItem {
        kind: StatusItemKind::Lease,
        summary: summary.to_string(),
        subject: Some(StatusSubject {
            kind: StatusSubjectKind::Lease,
            id: lease.id.as_str().to_string(),
            path: Some(lease_write_target_path(lease).to_string()),
        }),
        path: Some(lease_write_target_path(lease).to_string()),
        classification: None,
        mode: None,
        access: Vec::new(),
        event_id: None,
        event_name: Some(event_name),
        device_id: Some(lease.device_id.clone()),
        lease_id: Some(lease.id.clone()),
        project_id: Some(lease.project_id.clone()),
        snapshot_id: Some(lease.base_snapshot_id.clone()),
        policy_version: Some(PolicyVersion::new(DEFAULT_POLICY_VERSION)),
        env_record_id: None,
    }]
}

fn lease_attention_summary(lease: &AgentLease) -> Option<(&'static str, EventName)> {
    if lease.output_state == AgentLeaseOutputState::ReviewReady {
        return Some((
            "Agent output is ready for review.",
            EventName::LeaseReviewReady,
        ));
    }
    if lease.output_state == AgentLeaseOutputState::Conflicted {
        return Some(("Agent output has conflicts.", EventName::LeaseBlocked));
    }
    if lease.execution_state == AgentLeaseExecutionState::Blocked {
        return Some(("Agent lease is blocked.", EventName::LeaseBlocked));
    }
    None
}

fn status_for_attention(attention: &[StatusItem]) -> WorkspaceStatus {
    if attention.is_empty() {
        return WorkspaceStatus::healthy();
    }
    WorkspaceStatus {
        level: StatusLevel::Attention,
        attention_items: attention.iter().map(|item| item.summary.clone()).collect(),
    }
}

fn resolve_path_tool(request: &AgentToolInvokeRequest, lease: &AgentLease) -> AgentToolResult {
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

fn list_tree_tool(request: &AgentToolInvokeRequest, lease: &AgentLease) -> AgentToolResult {
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

fn read_file_tool(request: &AgentToolInvokeRequest, lease: &AgentLease) -> AgentToolResult {
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

fn search_workspace_tool(
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

fn symbol_lookup_tool(
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

fn hydration_status_tool(
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

fn prefix_search_result_paths(
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

fn prefix_symbol_result_paths(
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

fn prefixed_lease_relative_path(prefix: &str, path: &str) -> String {
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

fn write_overlay_tool(
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

fn diff_tool(
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

fn rollback_agent_write_effect(
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

fn restore_agent_write_path(path: &Path, previous_contents: Option<&[u8]>) {
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

fn publish_for_review(
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

fn complete_task(
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

fn run_command_tool(
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

fn denied_result(request: &AgentToolInvokeRequest, code: &str) -> AgentToolResult {
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

fn audit_tool_result(
    store: &MetadataStore,
    lease: &AgentLease,
    mut result: AgentToolResult,
    generated_at: &str,
) -> Result<AgentToolResult, AgentError> {
    if result.outcome == AgentToolResultOutcome::Allowed {
        if let Some((event_name, summary)) = success_event_for_tool(result.tool) {
            let event_id = EventId::new(format!(
                "evt_tool_invoked_{}_{}",
                lease.id.as_str(),
                stable_token(&format!(
                    "{}:{}:{}:{}",
                    result.request_id,
                    serde_json::to_string(&result.tool).unwrap_or_default(),
                    generated_at,
                    result.summary,
                ))
            ));
            append_lease_event(
                store,
                lease,
                event_name,
                event_id.clone(),
                generated_at,
                summary,
            )?;
            result.event_id = Some(event_id);
        }
        return Ok(result);
    }
    if result.outcome != AgentToolResultOutcome::Denied {
        return Ok(result);
    }
    let event_id = EventId::new(format!(
        "evt_tool_denied_{}_{}",
        lease.id.as_str(),
        stable_token(&format!(
            "{}:{}:{}",
            result.request_id,
            serde_json::to_string(&result.tool).unwrap_or_default(),
            generated_at
        ))
    ));
    append_lease_event(
        store,
        lease,
        EventName::LeaseToolDenied,
        event_id.clone(),
        generated_at,
        "Agent tool request was denied.",
    )?;
    result.event_id = Some(event_id);
    Ok(result)
}

fn success_event_for_tool(tool: AgentToolName) -> Option<(EventName, &'static str)> {
    match tool {
        AgentToolName::WriteOverlayFile => {
            Some((EventName::OverlayChanged, "Agent overlay changed."))
        }
        AgentToolName::RunCommandWithReceipt => {
            Some((EventName::LeaseToolInvoked, "Agent command tool completed."))
        }
        AgentToolName::RequestHydration => Some((
            EventName::LeaseToolInvoked,
            "Agent hydration tool completed.",
        )),
        AgentToolName::PublishOverlayForReview => {
            Some((EventName::PublishRequested, "Agent publish requested."))
        }
        AgentToolName::CompleteTask => Some((
            EventName::LeaseToolInvoked,
            "Agent completion tool completed.",
        )),
        _ => None,
    }
}

fn allowed_payload(
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

fn degraded_bounds() -> DegradedExplorationBounds {
    degraded_bounds_for_scope(&AgentLeaseScope {
        roots: Vec::new(),
        classifications: Vec::new(),
        max_bytes_per_read: Some(MAX_READ_BYTES),
        max_files_per_request: Some(MAX_TREE_FILES),
        max_depth: Some(MAX_TREE_DEPTH),
    })
}

fn degraded_bounds_for_scope(scope: &AgentLeaseScope) -> DegradedExplorationBounds {
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

fn effective_max_read_bytes(scope: &AgentLeaseScope) -> u64 {
    scope
        .max_bytes_per_read
        .unwrap_or(MAX_READ_BYTES)
        .min(MAX_READ_BYTES)
}

fn effective_max_files(scope: &AgentLeaseScope) -> u64 {
    scope
        .max_files_per_request
        .unwrap_or(MAX_TREE_FILES)
        .min(MAX_TREE_FILES)
}

fn effective_max_depth(scope: &AgentLeaseScope) -> u64 {
    scope
        .max_depth
        .unwrap_or(MAX_TREE_DEPTH)
        .min(MAX_TREE_DEPTH)
}

fn effective_max_write_bytes(lease: &AgentLease) -> u64 {
    effective_max_read_bytes(&lease.scopes.write).min(lease.hydrate_budget_bytes)
}

fn transport_allowed(authority: &AgentToolAuthority) -> bool {
    match authority.transport {
        AgentToolTransport::LocalDaemon => authority.peer_credential_checked,
        AgentToolTransport::McpAdapter => false,
    }
}

fn tool_allowed_for_blocked_lease(tool: AgentToolName) -> bool {
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

fn lease_is_expired(lease: &AgentLease, generated_at: &str) -> bool {
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

fn append_lease_event(
    store: &MetadataStore,
    lease: &AgentLease,
    name: EventName,
    event_id: EventId,
    generated_at: &str,
    summary: &str,
) -> Result<(), AgentError> {
    store.append_event(lease_event(lease, name, event_id, generated_at, summary))?;
    Ok(())
}

fn persist_created_agent_lease(
    store: &MetadataStore,
    lease: &AgentLease,
    event_id: EventId,
    generated_at: &str,
) -> Result<(), AgentError> {
    store
        .connection()
        .execute("BEGIN IMMEDIATE", [])
        .map_err(MetadataError::from)?;
    let result = (|| {
        store.upsert_agent_lease(lease)?;
        store.append_event(lease_event(
            lease,
            EventName::LeaseCreated,
            event_id,
            generated_at,
            "Agent lease created.",
        ))?;
        Ok::<(), AgentError>(())
    })();
    match result {
        Ok(()) => {
            store
                .connection()
                .execute("COMMIT", [])
                .map_err(MetadataError::from)?;
            Ok(())
        }
        Err(error) => {
            let _ = store.connection().execute("ROLLBACK", []);
            Err(error)
        }
    }
}

fn rollback_created_agent_work_view(store: &MetadataStore, lease: &AgentLease) {
    let _ = store.connection().execute(
        "DELETE FROM leases WHERE id = ?1",
        rusqlite::params![lease.id.as_str()],
    );
    let _ = store.connection().execute(
        "DELETE FROM work_view_base_files WHERE workspace_id = ?1 AND work_view_id = ?2",
        rusqlite::params![lease.workspace_id.as_str(), lease.work_view_id.as_str()],
    );
    let _ = store.connection().execute(
        "DELETE FROM work_views WHERE workspace_id = ?1 AND id = ?2",
        rusqlite::params![lease.workspace_id.as_str(), lease.work_view_id.as_str()],
    );
    let work_view_path = expand_display_path(&lease.work_view_path);
    if work_view_path.exists() {
        let _ = fs::remove_dir_all(work_view_path);
    }
}

fn rollback_provisional_agent_lease(store: &MetadataStore, lease: &AgentLease) {
    let _ = store.connection().execute(
        "DELETE FROM leases WHERE id = ?1",
        rusqlite::params![lease.id.as_str()],
    );
}

pub(crate) fn recover_provisional_agent_leases(
    store: &MetadataStore,
    workspace_id: &bowline_core::ids::WorkspaceId,
    generated_at: &str,
) -> Result<(), AgentError> {
    for lease in store.agent_leases(workspace_id)? {
        recover_provisional_agent_lease(store, lease, generated_at)?;
    }
    Ok(())
}

fn recover_provisional_agent_lease_by_id(
    store: &MetadataStore,
    lease_id: &LeaseId,
    generated_at: &str,
) -> Result<(), AgentError> {
    if let Some(lease) = store.agent_lease_by_id(lease_id)? {
        recover_provisional_agent_lease(store, lease, generated_at)?;
    }
    Ok(())
}

fn recover_provisional_agent_lease(
    store: &MetadataStore,
    mut lease: AgentLease,
    generated_at: &str,
) -> Result<(), AgentError> {
    if !is_provisional_agent_lease(&lease) {
        return Ok(());
    }

    let Some(work_view) = store.work_view_by_id(&lease.workspace_id, &lease.work_view_id)? else {
        rollback_provisional_agent_lease(store, &lease);
        return Ok(());
    };

    if !expand_display_path(&work_view.visible_path).is_dir() {
        rollback_created_agent_work_view(store, &lease);
        return Ok(());
    }

    lease.execution_state = AgentLeaseExecutionState::Active;
    lease.status_summary = "active".to_string();
    lease.updated_at = generated_at.to_string();
    persist_created_agent_lease(
        store,
        &lease,
        lease.audit.local_event_id.clone(),
        generated_at,
    )
}

fn is_provisional_agent_lease(lease: &AgentLease) -> bool {
    lease.execution_state == AgentLeaseExecutionState::Blocked && lease.status_summary == "creating"
}

fn lease_event(
    lease: &AgentLease,
    name: EventName,
    event_id: EventId,
    generated_at: &str,
    summary: &str,
) -> WorkspaceEvent {
    let mut event = WorkspaceEvent::new(
        event_id,
        name,
        generated_at,
        EventSeverity::Info,
        summary,
        lease.workspace_id.clone(),
    );
    event.project_id = Some(lease.project_id.clone());
    event.lease_id = Some(lease.id.clone());
    event.device_id = Some(lease.device_id.clone());
    event.subject = Some(EventSubject {
        kind: EventSubjectKind::Lease,
        id: lease.id.as_str().to_string(),
        path: Some(lease_write_target_path(lease).to_string()),
    });
    event
}

fn load_lease(
    store: &MetadataStore,
    lease_id: &LeaseId,
    generated_at: &str,
) -> Result<AgentLease, AgentError> {
    recover_provisional_agent_lease_by_id(store, lease_id, generated_at)?;
    store
        .agent_lease_by_id(lease_id)?
        .ok_or_else(|| AgentError::MissingLease {
            lease_id: lease_id.clone(),
        })
}

fn resolve_db_path(db_path: Option<PathBuf>) -> Result<PathBuf, AgentError> {
    db_path
        .map(Ok)
        .unwrap_or_else(default_database_path)
        .map_err(Into::into)
}

fn scoped_path(root: &str, requested: &str) -> Result<PathBuf, AgentError> {
    let root = expand_display_path(root);
    let mut candidate = root.clone();
    for component in Path::new(requested).components() {
        match component {
            Component::Normal(part) => candidate.push(part),
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) | Component::ParentDir => {
                return Err(AgentError::ToolDenied {
                    code: "path-outside-lease".to_string(),
                });
            }
        }
    }
    if candidate.starts_with(&root) {
        ensure_no_symlink_components(&root, &candidate)?;
        Ok(candidate)
    } else {
        Err(AgentError::ToolDenied {
            code: "path-outside-lease".to_string(),
        })
    }
}

fn scoped_read_path(lease: &AgentLease, requested: &str) -> Result<PathBuf, AgentError> {
    let path = scoped_path(lease_write_target_path(lease), requested)?;
    ensure_path_in_scope(
        lease_write_target_path(lease),
        &path,
        &lease.scopes.read.roots,
    )?;
    Ok(path)
}

fn scoped_write_path(lease: &AgentLease, requested: &str) -> Result<PathBuf, AgentError> {
    let path = scoped_path(lease_write_target_path(lease), requested)?;
    ensure_path_in_scope(
        lease_write_target_path(lease),
        &path,
        &lease.scopes.write.roots,
    )?;
    Ok(path)
}

fn ensure_path_in_scope(
    lease_root: &str,
    path: &Path,
    scope_roots: &[String],
) -> Result<(), AgentError> {
    if scope_roots
        .iter()
        .map(|root| scope_root_path(lease_root, root))
        .any(|root| path.starts_with(root))
    {
        Ok(())
    } else {
        Err(AgentError::ToolDenied {
            code: "path-outside-lease".to_string(),
        })
    }
}

fn scope_root_path(lease_root: &str, root: &str) -> PathBuf {
    let expanded = expand_display_path(root);
    if expanded.is_absolute() || root == "~" || root.starts_with("~/") {
        expanded
    } else {
        expand_display_path(lease_root).join(expanded)
    }
}

fn agent_read_allowed(lease_root: &str, path: &Path) -> bool {
    let Some(decision) = agent_path_decision(lease_root, path) else {
        return false;
    };
    !matches!(
        decision.classification,
        PathClassification::ProjectEnv
            | PathClassification::SecretLooking
            | PathClassification::LocalOnly
            | PathClassification::Blocked
    ) && decision.access.contains(&AccessFlag::AgentReadable)
        && !decision.access.contains(&AccessFlag::AgentHidden)
}

fn agent_write_allowed_decision(decision: &crate::policy::PathPolicyDecision) -> bool {
    matches!(
        decision.classification,
        PathClassification::WorkspaceSync | PathClassification::LargeFile
    ) && !decision.access.contains(&AccessFlag::AgentHidden)
}

fn agent_path_decision(lease_root: &str, path: &Path) -> Option<crate::policy::PathPolicyDecision> {
    let root = expand_display_path(lease_root);
    let relative = path.strip_prefix(&root).ok()?;
    let relative_path = relative
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/");
    let metadata = fs::symlink_metadata(path).ok();
    let policy =
        UserPolicy::load_for_path(&root, &relative_path).unwrap_or_else(|_| UserPolicy::empty());
    Some(classify_path(
        &PathFacts {
            relative_path,
            is_dir: metadata.as_ref().is_some_and(|metadata| metadata.is_dir()),
            byte_len: metadata.as_ref().map(|metadata| metadata.len()),
        },
        &policy,
    ))
}

fn hydration_target(
    store: &MetadataStore,
    lease: &AgentLease,
    path: &Path,
    requested_content_id: Option<&str>,
) -> Result<(u64, Option<ContentId>), AgentError> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => {
            let metadata_path = lease_project_metadata_path(store, lease, path)?;
            let projected = store.projected_node_by_path(&lease.workspace_id, &metadata_path)?;
            if let Some(projected) = projected {
                if projected.kind != NamespaceEntryKind::File {
                    return Err(AgentError::ToolDenied {
                        code: "hydration-requires-file".to_string(),
                    });
                }
                if let Some(requested) = requested_content_id {
                    let Some(projected_content_id) = projected.content_id.as_ref() else {
                        return Err(AgentError::ToolDenied {
                            code: "content-id-mismatch".to_string(),
                        });
                    };
                    if projected_content_id.as_str() != requested {
                        return Err(AgentError::ToolDenied {
                            code: "content-id-mismatch".to_string(),
                        });
                    }
                }
                return Ok((metadata.len(), projected.content_id));
            }
            if requested_content_id.is_some() {
                return Err(AgentError::ToolDenied {
                    code: "content-id-unverified".to_string(),
                });
            }
            return Ok((metadata.len(), None));
        }
        Ok(_) => {
            return Err(AgentError::ToolDenied {
                code: "hydration-requires-file".to_string(),
            });
        }
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error.into()),
        Err(_) => {}
    }

    let metadata_path = lease_project_metadata_path(store, lease, path)?;
    let projected = store
        .projected_node_by_path(&lease.workspace_id, &metadata_path)?
        .ok_or_else(|| AgentError::ToolDenied {
            code: "missing-file".to_string(),
        })?;
    if projected.kind != NamespaceEntryKind::File {
        return Err(AgentError::ToolDenied {
            code: "hydration-requires-file".to_string(),
        });
    }
    let content_id = projected.content_id.ok_or_else(|| AgentError::ToolDenied {
        code: "hydration-size-unknown".to_string(),
    })?;
    if requested_content_id.is_some_and(|requested| content_id.as_str() != requested) {
        return Err(AgentError::ToolDenied {
            code: "content-id-mismatch".to_string(),
        });
    }
    let locator = store
        .content_locator(&lease.workspace_id, &content_id)?
        .ok_or_else(|| AgentError::ToolDenied {
            code: "hydration-size-unknown".to_string(),
        })?;
    Ok((locator.locator.raw_size, Some(content_id)))
}

fn lease_index_identity(
    lease: &AgentLease,
    policy_path_prefix: Option<String>,
    max_scan_files: usize,
) -> IndexedProjectIdentity {
    IndexedProjectIdentity {
        workspace_id: lease.workspace_id.clone(),
        project_id: lease.project_id.clone(),
        snapshot_id: Some(lease.base_snapshot_id.clone()),
        policy_path_prefix,
        max_scan_files: Some(max_scan_files),
    }
}

fn lease_project_metadata_path(
    store: &MetadataStore,
    lease: &AgentLease,
    path: &Path,
) -> Result<String, AgentError> {
    let relative = lease_relative_filter(lease, path).ok_or_else(|| AgentError::ToolDenied {
        code: "path-outside-lease".to_string(),
    })?;
    let project = store
        .project_by_id(&lease.workspace_id, &lease.project_id)?
        .ok_or_else(|| AgentError::MissingProject {
            path: lease.project_id.as_str().to_string(),
        })?;
    Ok(format!(
        "{}/{}",
        normalize_workspace_path(&project.path).trim_end_matches('/'),
        normalize_workspace_path(&relative).trim_start_matches('/')
    ))
}

fn hydration_queue_content_matches(
    existing: Option<&ContentId>,
    requested: Option<&ContentId>,
) -> bool {
    match (existing, requested) {
        (Some(existing), Some(requested)) => existing == requested,
        (None, None) => true,
        (Some(_), None) | (None, Some(_)) => false,
    }
}

fn lease_relative_filter(lease: &AgentLease, path: &Path) -> Option<String> {
    let root = expand_display_path(lease_write_target_path(lease));
    let relative = path.strip_prefix(root).ok()?;
    let relative = relative
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/");
    (!relative.is_empty()).then_some(relative)
}

fn ensure_no_symlink_components(root: &Path, path: &Path) -> Result<(), AgentError> {
    if fs::symlink_metadata(root)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(AgentError::ToolDenied {
            code: "path-outside-lease".to_string(),
        });
    }
    let relative = path
        .strip_prefix(root)
        .map_err(|_| AgentError::ToolDenied {
            code: "path-outside-lease".to_string(),
        })?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(AgentError::ToolDenied {
                    code: "path-outside-lease".to_string(),
                });
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => break,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn create_parent_dirs_without_symlinks(root: &Path, parent: &Path) -> Result<(), AgentError> {
    if fs::symlink_metadata(root)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(AgentError::ToolDenied {
            code: "path-outside-lease".to_string(),
        });
    }
    let relative = parent
        .strip_prefix(root)
        .map_err(|_| AgentError::ToolDenied {
            code: "path-outside-lease".to_string(),
        })?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        match component {
            Component::Normal(part) => current.push(part),
            Component::CurDir => continue,
            Component::RootDir | Component::Prefix(_) | Component::ParentDir => {
                return Err(AgentError::ToolDenied {
                    code: "path-outside-lease".to_string(),
                });
            }
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(AgentError::ToolDenied {
                    code: "path-outside-lease".to_string(),
                });
            }
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "path component is not a directory",
                )
                .into());
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&current)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn lease_work_view_name(task: &str, generated_at: &str) -> String {
    let words = task
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .take(3)
        .map(|part| part.to_ascii_lowercase())
        .collect::<Vec<_>>();
    let prefix = if words.is_empty() {
        "agent".to_string()
    } else {
        words.join("-")
    };
    format!("agent-{}-{}", prefix, stable_token(generated_at))
}

fn agent_work_view_id(workspace_id: &str, project_id: &str, name: &str) -> WorkViewId {
    let input = format!("{workspace_id}:{project_id}:{name}");
    WorkViewId::new(format!(
        "work_{}",
        &blake3::hash(input.as_bytes()).to_hex()[..16]
    ))
}

fn display_path_for_agent_work_view(root: &str, project_path: &str, name: &str) -> String {
    let path = expand_display_path(root)
        .join(".work")
        .join(normalize_workspace_path(project_path))
        .join(name);
    display_path(&path)
}

fn display_path_for_project(root: &str, project_path: &str) -> String {
    let normalized = normalize_workspace_path(project_path);
    let root = expand_display_path(root);
    let path = if normalized.is_empty() {
        root
    } else {
        root.join(normalized)
    };
    display_path(&path)
}

fn display_path(path: &Path) -> String {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return path.display().to_string();
    };
    let Ok(relative) = path.strip_prefix(&home) else {
        return path.display().to_string();
    };
    if relative.as_os_str().is_empty() {
        "~".to_string()
    } else {
        format!("~/{}", relative.display())
    }
}

fn stable_token(value: &str) -> String {
    let hash = blake3::hash(value.as_bytes()).to_hex().to_string();
    hash.chars().take(12).collect()
}

fn redacted_task_label(task: &str) -> String {
    let redacted = crate::setup::redact_setup_text(task).text;
    let task = redacted.trim();
    if task.is_empty() {
        "agent task".to_string()
    } else {
        task.to_string()
    }
}

fn json_map(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        other => {
            let mut map = Map::new();
            map.insert("value".to_string(), other);
            map
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use bowline_core::{
        commands::{AgentToolAuthority, AgentToolInvokeRequest, AgentToolTransport},
        ids::{ContentId, DeviceId, ProjectId, SnapshotId, WorkspaceId},
        work_views::WorkViewLifecycle,
        workspace_graph::{ContentLocator, ContentStorage, HydrationState, NamespaceEntryKind},
    };
    use serde_json::Map;

    use crate::{
        metadata::{MetadataStore, ProjectedNodeRecord, SetupReceiptRecord},
        status::{StatusOptions, compose_status},
        workspace::TempWorkspace,
    };

    use super::*;

    #[test]
    fn default_lease_binds_directly_to_real_project_without_work_view() {
        let (temp, db_path) = seeded_store("agent-lease-direct");
        let project_path = temp.root().join("Code/apps/web");

        let output = create_agent_lease(AgentLeaseCreateOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            task: "fix auth routing".to_string(),
            base: AgentLeaseBase::LatestWorkspace,
            hydrate_budget_bytes: 1024 * 1024,
            work_view: false,
            device_id: DeviceId::new("device-test"),
            generated_at: now(),
        })
        .expect("direct lease created");

        assert_eq!(output.lease.write_target_mode, AgentWriteTargetMode::Direct);
        assert_eq!(
            output.lease.output_target.kind,
            AgentOutputTargetKind::RealProject
        );
        assert_eq!(output.lease.output_target.work_view_id, None);
        assert_eq!(
            output.lease.write_target_path,
            project_path.display().to_string()
        );
        assert_eq!(
            output.lease.work_view_path,
            project_path.display().to_string()
        );
        assert!(
            !temp.root().join("Code/.work").exists(),
            "direct lease must not create a work-view tree"
        );

        let context = agent_context(AgentLeaseSelectorOptions {
            db_path: Some(db_path.clone()),
            lease_id: output.lease.id.clone(),
            generated_at: now(),
        })
        .expect("direct context");
        assert_eq!(
            context.context.write_target_path,
            project_path.display().to_string()
        );
        assert_eq!(
            context.context.start_work.cwd,
            project_path.display().to_string()
        );
        for work_view_only_tool in [
            AgentToolName::PublishOverlayForReview,
            AgentToolName::ListOverlayChanges,
            AgentToolName::DiffSnapshots,
        ] {
            assert!(
                !context
                    .context
                    .capabilities
                    .iter()
                    .any(|capability| capability.name == work_view_only_tool),
                "direct agent context must not advertise {work_view_only_tool:?}"
            );
        }

        let listed_capabilities = invoke_agent_tool(
            Some(db_path.clone()),
            tool_request(
                &output.lease,
                AgentToolName::ListCapabilities,
                serde_json::json!({}),
            ),
            now(),
        )
        .expect("direct list capabilities");
        assert_eq!(listed_capabilities.outcome, AgentToolResultOutcome::Allowed);
        let listed_capabilities = listed_capabilities
            .payload
            .expect("list capabilities payload")
            .remove("capabilities")
            .expect("capabilities field");
        let listed_capabilities = listed_capabilities.as_array().expect("capabilities array");
        for work_view_only_tool in [
            AgentToolName::PublishOverlayForReview,
            AgentToolName::ListOverlayChanges,
            AgentToolName::DiffSnapshots,
        ] {
            let serialized_name =
                serde_json::to_value(work_view_only_tool).expect("tool name serializes");
            assert!(
                !listed_capabilities
                    .iter()
                    .any(|capability| capability.get("name") == Some(&serialized_name)),
                "direct list_capabilities must not advertise {work_view_only_tool:?}"
            );
        }

        let write = invoke_agent_tool(
            Some(db_path.clone()),
            tool_request(
                &output.lease,
                AgentToolName::WriteOverlayFile,
                serde_json::json!({"path": "README.md", "contents": "direct edit\n"}),
            ),
            now(),
        )
        .expect("direct write");
        assert_eq!(write.outcome, AgentToolResultOutcome::Allowed);
        assert_eq!(
            fs::read_to_string(project_path.join("README.md")).expect("direct file"),
            "direct edit\n"
        );

        let complete = invoke_agent_tool(
            Some(db_path),
            tool_request(
                &output.lease,
                AgentToolName::CompleteTask,
                serde_json::json!({}),
            ),
            now(),
        )
        .expect("direct complete");
        assert_eq!(complete.outcome, AgentToolResultOutcome::Allowed);
    }

    #[test]
    fn hydration_budget_grant_unblocks_exhausted_agent_lease() {
        let (temp, db_path) = seeded_store("agent-budget-grant");
        let project_path = temp.root().join("Code/apps/web");
        let lease = create_agent_lease(AgentLeaseCreateOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            task: "inspect cold files".to_string(),
            base: AgentLeaseBase::LatestWorkspace,
            hydrate_budget_bytes: 1,
            work_view: false,
            device_id: DeviceId::new("device-test"),
            generated_at: now(),
        })
        .expect("lease created")
        .lease;

        let mut store = MetadataStore::open(&db_path).expect("store");
        let denied = reserve_lease_bytes(
            &mut store,
            HydrationBudgetReservationRequest {
                workspace_id: &lease.workspace_id,
                project_id: &lease.project_id,
                lease_id: &lease.id,
                path: "src/cold.ts",
                content_id: Some("cid_cold"),
                cause: "agent-read",
                requested_bytes: 2,
                limit_bytes: lease.hydrate_budget_bytes,
                now: &now(),
            },
        )
        .expect("budget reservation");
        assert!(!denied.accepted);
        assert_eq!(
            denied.status.state,
            bowline_core::commands::HydrationBudgetState::Exhausted
        );
        assert_eq!(
            denied.status.next_action.and_then(|action| action.command),
            Some(format!(
                "bowline agent budget --lease {} --add 64MiB",
                lease.id.as_str()
            ))
        );
        drop(store);

        let grant = grant_agent_hydration_budget(AgentBudgetGrantOptions {
            db_path: Some(db_path.clone()),
            lease_id: lease.id.clone(),
            add_bytes: 4,
            generated_at: "2026-06-25T12:00:01Z".to_string(),
        })
        .expect("budget grant");
        assert_eq!(grant.previous_limit_bytes, 1);
        assert_eq!(grant.budget.limit_bytes, 5);
        assert_eq!(grant.budget.remaining_bytes, 5);

        let mut store = MetadataStore::open(&db_path).expect("store");
        let accepted = reserve_lease_bytes(
            &mut store,
            HydrationBudgetReservationRequest {
                workspace_id: &lease.workspace_id,
                project_id: &lease.project_id,
                lease_id: &lease.id,
                path: "src/cold.ts",
                content_id: Some("cid_cold"),
                cause: "agent-read",
                requested_bytes: 2,
                limit_bytes: grant.budget.limit_bytes,
                now: "2026-06-25T12:00:02Z",
            },
        )
        .expect("budget reservation after grant");
        assert!(accepted.accepted);

        let events = store.list_events(20).expect("events");
        assert!(events.iter().any(|event| {
            event.name == EventName::HydrationBudgetDenied
                && event.lease_id == Some(lease.id.clone())
        }));
        assert!(events.iter().any(|event| {
            event.name == EventName::HydrationBudgetOverrideGranted
                && event.lease_id == Some(lease.id.clone())
        }));
    }

    #[test]
    fn lease_create_binds_to_work_view_and_context_is_secret_free() {
        let (temp, db_path) = seeded_store("agent-lease-create");
        let project_path = temp.root().join("Code/apps/web");
        let task_marker = "task-tail-marker-20260627T090102Z";

        let output = create_agent_lease(AgentLeaseCreateOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            task: format!(
                "fix auth token in the remote agent task prompt without truncating the exact requested work item {task_marker} OPENAI_API_KEY=sk_live_abcdefghijklmnopqrstuvwxyz"
            ),
            base: AgentLeaseBase::LatestWorkspace,
            hydrate_budget_bytes: 1024 * 1024,
            work_view: true,
            device_id: DeviceId::new("device-test"),
            generated_at: now(),
        })
        .expect("lease created");

        assert_eq!(
            output.lease.execution_state,
            AgentLeaseExecutionState::Active
        );
        assert_eq!(output.lease.output_state, AgentLeaseOutputState::Empty);
        assert!(output.lease.task.contains(task_marker));
        assert!(
            !output
                .lease
                .task
                .contains("sk_live_abcdefghijklmnopqrstuvwxyz")
        );
        assert!(
            output
                .lease
                .work_view_path
                .contains(".work/apps/web/agent-fix-auth-token")
        );
        assert!(
            fs::metadata(&output.lease.work_view_path)
                .expect("work view")
                .is_dir()
        );

        let context = agent_context(AgentLeaseSelectorOptions {
            db_path: Some(db_path.clone()),
            lease_id: output.lease.id.clone(),
            generated_at: now(),
        })
        .expect("context");
        let context_json = serde_json::to_string(&context).expect("context serializes");
        assert!(!context_json.contains("nonce"));
        assert!(!context_json.contains("secret"));
        assert!(!context_json.contains("sk_live_abcdefghijklmnopqrstuvwxyz"));
        assert!(context_json.contains("OPENAI_API_KEY=[redacted]"));
        assert!(context_json.contains(task_marker));
        assert_eq!(context.context.work_view_path, output.lease.work_view_path);
        assert!(context.context.capabilities.iter().any(|capability| {
            capability.name == AgentToolName::WriteOverlayFile
                && capability.state == AgentCapabilityState::Available
        }));

        let prompt = agent_prompt(AgentLeaseSelectorOptions {
            db_path: Some(db_path),
            lease_id: output.lease.id.clone(),
            generated_at: now(),
        })
        .expect("prompt");
        assert!(prompt.prompt.text.contains(task_marker));
        assert!(prompt.prompt.text.contains("bowline agent publish --lease"));
        assert!(!prompt.prompt.text.contains("~/.local/bin/bowline"));
        assert!(
            !prompt
                .prompt
                .text
                .contains("sk_live_abcdefghijklmnopqrstuvwxyz")
        );
    }

    #[test]
    fn lease_create_rolls_back_work_view_when_creation_event_fails() {
        let (temp, db_path) = seeded_store("agent-lease-create-rollback");
        let project_path = temp.root().join("Code/apps/web");
        let store = MetadataStore::open(&db_path).expect("store");
        store
            .connection()
            .execute(
                "CREATE TRIGGER fail_agent_lease_event
                 BEFORE INSERT ON events
                 BEGIN
                   SELECT RAISE(FAIL, 'forced event failure');
                 END",
                [],
            )
            .expect("event failure trigger");
        drop(store);

        let error = create_agent_lease(AgentLeaseCreateOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            task: "rollback".to_string(),
            base: AgentLeaseBase::LatestWorkspace,
            hydrate_budget_bytes: 1024 * 1024,
            work_view: true,
            device_id: DeviceId::new("device-test"),
            generated_at: now(),
        })
        .expect_err("lease creation should fail when creation audit event fails");
        assert!(matches!(error, AgentError::Event(_)));

        let store = MetadataStore::open(&db_path).expect("store");
        assert!(
            store
                .agent_leases(&WorkspaceId::new("ws_code"))
                .expect("leases")
                .is_empty()
        );
        assert!(
            store
                .work_views(&WorkspaceId::new("ws_code"), true, None)
                .expect("work views")
                .is_empty()
        );
        let work_namespace = temp.root().join("Code/.work/apps/web");
        assert!(
            !work_namespace.exists()
                || fs::read_dir(work_namespace)
                    .expect("work namespace")
                    .next()
                    .is_none()
        );
    }

    #[test]
    fn status_recovers_provisional_lease_with_materialized_work_view() {
        let (temp, db_path) = seeded_store("agent-lease-provisional-finalize");
        let project_path = temp.root().join("Code/apps/web");
        let mut lease = create_agent_lease(AgentLeaseCreateOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            task: "recover finalized lease".to_string(),
            base: AgentLeaseBase::LatestWorkspace,
            hydrate_budget_bytes: 1024 * 1024,
            work_view: true,
            device_id: DeviceId::new("device-test"),
            generated_at: now(),
        })
        .expect("lease created")
        .lease;

        lease.execution_state = AgentLeaseExecutionState::Blocked;
        lease.status_summary = "creating".to_string();
        let store = MetadataStore::open(&db_path).expect("store");
        store
            .connection()
            .execute(
                "DELETE FROM events WHERE id = ?1",
                rusqlite::params![lease.audit.local_event_id.as_str()],
            )
            .expect("delete creation event");
        store.upsert_agent_lease(&lease).expect("provisional lease");
        drop(store);

        let status = compose_status(StatusOptions {
            db_path: Some(db_path.clone()),
            requested_path: Some(project_path.display().to_string()),
            workspace_scope: false,
            generated_at: now(),
        })
        .expect("status recovers provisional lease");
        assert!(
            status
                .items
                .iter()
                .any(|item| item.lease_id == Some(lease.id.clone()))
        );

        let stored = MetadataStore::open(&db_path)
            .expect("store")
            .agent_lease_by_id(&lease.id)
            .expect("lease query")
            .expect("lease retained");
        assert_eq!(stored.execution_state, AgentLeaseExecutionState::Active);
        assert_eq!(stored.status_summary, "active");
    }

    #[test]
    fn status_removes_orphaned_provisional_lease_without_work_view() {
        let (temp, db_path) = seeded_store("agent-lease-provisional-orphan");
        let project_path = temp.root().join("Code/apps/web");
        let mut lease = create_agent_lease(AgentLeaseCreateOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            task: "recover orphaned lease".to_string(),
            base: AgentLeaseBase::LatestWorkspace,
            hydrate_budget_bytes: 1024 * 1024,
            work_view: true,
            device_id: DeviceId::new("device-test"),
            generated_at: now(),
        })
        .expect("lease created")
        .lease;
        lease.execution_state = AgentLeaseExecutionState::Blocked;
        lease.status_summary = "creating".to_string();

        let store = MetadataStore::open(&db_path).expect("store");
        store.upsert_agent_lease(&lease).expect("provisional lease");
        store
            .connection()
            .execute(
                "DELETE FROM work_view_base_files WHERE workspace_id = ?1 AND work_view_id = ?2",
                rusqlite::params![lease.workspace_id.as_str(), lease.work_view_id.as_str()],
            )
            .expect("delete base files");
        store
            .connection()
            .execute(
                "DELETE FROM work_views WHERE workspace_id = ?1 AND id = ?2",
                rusqlite::params![lease.workspace_id.as_str(), lease.work_view_id.as_str()],
            )
            .expect("delete work view");
        let work_view_path = lease.work_view_path.clone();
        fs::remove_dir_all(&work_view_path).expect("remove materialization");
        drop(store);

        let status = compose_status(StatusOptions {
            db_path: Some(db_path.clone()),
            requested_path: Some(project_path.display().to_string()),
            workspace_scope: false,
            generated_at: now(),
        })
        .expect("status removes orphaned provisional lease");
        assert!(
            !status
                .items
                .iter()
                .any(|item| item.lease_id == Some(lease.id.clone()))
        );
        assert!(
            MetadataStore::open(&db_path)
                .expect("store")
                .agent_lease_by_id(&lease.id)
                .expect("lease query")
                .is_none()
        );
        assert!(!Path::new(&work_view_path).exists());
    }

    #[test]
    fn tool_write_publish_and_complete_update_lease_and_status() {
        let (temp, db_path) = seeded_store("agent-lease-tools");
        let project_path = temp.root().join("Code/apps/web");
        let lease = create_agent_lease(AgentLeaseCreateOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            task: "add readme".to_string(),
            base: AgentLeaseBase::LatestWorkspace,
            hydrate_budget_bytes: 1024 * 1024,
            work_view: true,
            device_id: DeviceId::new("device-test"),
            generated_at: now(),
        })
        .expect("lease created")
        .lease;

        let write = invoke_agent_tool(
            Some(db_path.clone()),
            tool_request(
                &lease,
                AgentToolName::WriteOverlayFile,
                serde_json::json!({"path": "README.md", "contents": "# Hello\n"}),
            ),
            now(),
        )
        .expect("write");
        assert_eq!(write.outcome, AgentToolResultOutcome::Allowed);
        assert!(write.event_id.is_some());
        assert!(Path::new(&lease.work_view_path).join("README.md").is_file());
        let events = MetadataStore::open(&db_path)
            .expect("store")
            .list_events(20)
            .expect("events");
        assert!(events.iter().any(|event| {
            event.name == EventName::OverlayChanged && event.lease_id == Some(lease.id.clone())
        }));
        let writes = MetadataStore::open(&db_path)
            .expect("store")
            .local_write_log(&lease.workspace_id)
            .expect("write log");
        assert!(writes.iter().any(|write| {
            write.project_id.as_ref() == Some(&lease.project_id)
                && write.path.ends_with("README.md")
                && write.operation == "create"
                && write.causation_id.starts_with("req_WriteOverlayFile_")
        }));

        let publish = invoke_agent_tool(
            Some(db_path.clone()),
            tool_request(
                &lease,
                AgentToolName::PublishOverlayForReview,
                serde_json::json!({}),
            ),
            now(),
        )
        .expect("publish");
        assert_eq!(publish.outcome, AgentToolResultOutcome::Allowed);

        let store = MetadataStore::open(&db_path).expect("store");
        let stored = store
            .agent_lease_by_id(&lease.id)
            .expect("lease query")
            .expect("lease stored");
        assert_eq!(stored.output_state, AgentLeaseOutputState::ReviewReady);
        let work_view = store
            .work_view_by_id(&lease.workspace_id, &lease.work_view_id)
            .expect("work view query")
            .expect("work view stored");
        assert_eq!(work_view.lifecycle, WorkViewLifecycle::ReviewReady);
        drop(store);

        let status = compose_status(StatusOptions {
            db_path: Some(db_path.clone()),
            requested_path: Some(project_path.display().to_string()),
            workspace_scope: false,
            generated_at: now(),
        })
        .expect("status");
        assert!(
            status
                .items
                .iter()
                .any(|item| item.lease_id == Some(lease.id.clone()))
        );
        let context = agent_context(AgentLeaseSelectorOptions {
            db_path: Some(db_path.clone()),
            lease_id: lease.id.clone(),
            generated_at: now(),
        })
        .expect("context");
        assert_eq!(context.context.status.level, StatusLevel::Attention);
        assert!(context.context.status.needs_attention());
        assert!(
            context
                .context
                .attention
                .iter()
                .any(|item| item.event_name == Some(EventName::LeaseReviewReady))
        );

        let complete = invoke_agent_tool(
            Some(db_path.clone()),
            tool_request(&lease, AgentToolName::CompleteTask, serde_json::json!({})),
            now(),
        )
        .expect("complete");
        assert_eq!(complete.outcome, AgentToolResultOutcome::Allowed);
    }

    #[test]
    fn agent_journey_uses_setup_prompt_overlay_and_review_without_touching_project() {
        let (temp, db_path) = seeded_store("agent-lease-journey");
        let project_path = temp.root().join("Code/apps/web");
        let project_readme = project_path.join("README.md");
        fs::write(&project_readme, "# Original\n").expect("project readme");

        let lease = create_agent_lease(AgentLeaseCreateOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            task: "update README without leaking API_KEY=secret-token".to_string(),
            base: AgentLeaseBase::LatestWorkspace,
            hydrate_budget_bytes: 1024 * 1024,
            work_view: true,
            device_id: DeviceId::new("device-test"),
            generated_at: now(),
        })
        .expect("lease created")
        .lease;

        MetadataStore::open(&db_path)
            .expect("store")
            .upsert_setup_receipt(&setup_receipt(&lease, "echo ok", "approved"))
            .expect("approved receipt");

        let context = agent_context(AgentLeaseSelectorOptions {
            db_path: Some(db_path.clone()),
            lease_id: lease.id.clone(),
            generated_at: now(),
        })
        .expect("context");
        assert_eq!(context.context.start_work.cwd, lease.work_view_path);
        assert_eq!(context.context.setup_receipts.len(), 1);
        let setup_signal = context
            .context
            .readiness
            .signals
            .iter()
            .find(|signal| signal.name == "setup-receipts")
            .expect("setup readiness signal");
        assert_eq!(setup_signal.state, AgentReadinessState::Ready);
        assert!(setup_signal.summary.contains("1 setup receipt"));
        let context_json = serde_json::to_string(&context).expect("context serializes");
        assert!(!context_json.contains("secret-token"));

        let prompt = agent_prompt(AgentLeaseSelectorOptions {
            db_path: Some(db_path.clone()),
            lease_id: lease.id.clone(),
            generated_at: now(),
        })
        .expect("prompt");
        assert_eq!(prompt.prompt.redaction, AgentPromptRedaction::Applied);
        assert!(prompt.prompt.text.contains("bowline agent task"));
        assert!(prompt.prompt.text.contains(&lease.work_view_path));
        assert!(
            prompt
                .prompt
                .allowed_tools
                .contains(&AgentToolName::PublishOverlayForReview)
        );

        let setup_run = invoke_agent_tool(
            Some(db_path.clone()),
            tool_request(
                &lease,
                AgentToolName::RunCommandWithReceipt,
                serde_json::json!({"command": "echo ok"}),
            ),
            now(),
        )
        .expect("setup command");
        assert_eq!(setup_run.outcome, AgentToolResultOutcome::Allowed);
        assert!(setup_run.receipt_id.is_some());

        let write = invoke_agent_tool(
            Some(db_path.clone()),
            tool_request(
                &lease,
                AgentToolName::WriteOverlayFile,
                serde_json::json!({"path": "README.md", "contents": "# Agent edit\n"}),
            ),
            now(),
        )
        .expect("write");
        assert_eq!(write.outcome, AgentToolResultOutcome::Allowed);
        assert_eq!(
            fs::read_to_string(&project_readme).expect("main project readme"),
            "# Original\n"
        );
        assert_eq!(
            fs::read_to_string(Path::new(&lease.work_view_path).join("README.md"))
                .expect("work view readme"),
            "# Agent edit\n"
        );

        let publish = invoke_agent_tool(
            Some(db_path.clone()),
            tool_request(
                &lease,
                AgentToolName::PublishOverlayForReview,
                serde_json::json!({}),
            ),
            now(),
        )
        .expect("publish");
        assert_eq!(publish.outcome, AgentToolResultOutcome::Allowed);

        let status = compose_status(StatusOptions {
            db_path: Some(db_path.clone()),
            requested_path: Some(project_path.display().to_string()),
            workspace_scope: false,
            generated_at: now(),
        })
        .expect("status");
        assert!(
            status
                .items
                .iter()
                .any(|item| item.event_name == Some(EventName::LeaseReviewReady))
        );

        let store = MetadataStore::open(&db_path).expect("store");
        let stored = store
            .agent_lease_by_id(&lease.id)
            .expect("lease query")
            .expect("lease stored");
        assert_eq!(stored.output_state, AgentLeaseOutputState::ReviewReady);
        let writes = store
            .local_write_log(&lease.workspace_id)
            .expect("write log");
        assert!(writes.iter().any(|write| {
            write.project_id.as_ref() == Some(&lease.project_id)
                && write.path.ends_with("README.md")
                && matches!(write.operation.as_str(), "create" | "modify" | "update")
                && write.causation_id.starts_with("req_WriteOverlayFile_")
        }));
    }

    #[test]
    fn run_command_requires_completed_or_approved_setup_receipt() {
        let (temp, db_path) = seeded_store("agent-lease-command-receipt-state");
        let project_path = temp.root().join("Code/apps/web");
        let lease = create_agent_lease(AgentLeaseCreateOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            task: "run setup command".to_string(),
            base: AgentLeaseBase::LatestWorkspace,
            hydrate_budget_bytes: 1024 * 1024,
            work_view: true,
            device_id: DeviceId::new("device-test"),
            generated_at: now(),
        })
        .expect("lease created")
        .lease;
        let store = MetadataStore::open(&db_path).expect("store");
        store
            .upsert_setup_receipt(&setup_receipt(&lease, "echo ok", "failed"))
            .expect("failed receipt");
        drop(store);

        let denied = invoke_agent_tool(
            Some(db_path.clone()),
            tool_request(
                &lease,
                AgentToolName::RunCommandWithReceipt,
                serde_json::json!({"command": "echo ok"}),
            ),
            now(),
        )
        .expect("command denied");
        assert_eq!(denied.denial.expect("denial").code, "command-not-declared");

        MetadataStore::open(&db_path)
            .expect("store")
            .upsert_setup_receipt(&setup_receipt(&lease, "echo ok", "completed"))
            .expect("completed receipt");
        let allowed = invoke_agent_tool(
            Some(db_path),
            tool_request(
                &lease,
                AgentToolName::RunCommandWithReceipt,
                serde_json::json!({"command": "echo ok"}),
            ),
            now(),
        )
        .expect("command allowed");
        assert_eq!(allowed.outcome, AgentToolResultOutcome::Allowed);
    }

    #[test]
    fn write_overlay_rolls_back_file_log_and_lease_when_audit_event_fails() {
        let (temp, db_path) = seeded_store("agent-lease-write-audit-rollback");
        let project_path = temp.root().join("Code/apps/web");
        let lease = create_agent_lease(AgentLeaseCreateOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            task: "write rollback".to_string(),
            base: AgentLeaseBase::LatestWorkspace,
            hydrate_budget_bytes: 1024 * 1024,
            work_view: true,
            device_id: DeviceId::new("device-test"),
            generated_at: now(),
        })
        .expect("lease created")
        .lease;
        let store = MetadataStore::open(&db_path).expect("store");
        store
            .connection()
            .execute(
                "CREATE TRIGGER fail_overlay_audit
                 BEFORE INSERT ON events
                 WHEN NEW.name = 'lease.tool_invoked' OR NEW.name = 'overlay.changed'
                 BEGIN
                   SELECT RAISE(FAIL, 'forced overlay audit failure');
                 END",
                [],
            )
            .expect("event failure trigger");
        drop(store);

        let error = invoke_agent_tool(
            Some(db_path.clone()),
            tool_request(
                &lease,
                AgentToolName::WriteOverlayFile,
                serde_json::json!({"path": "ROLLBACK.md", "contents": "do not keep"}),
            ),
            now(),
        )
        .expect_err("audit failure should fail the write");
        assert!(matches!(error, AgentError::Event(_)));
        assert!(
            !Path::new(&lease.work_view_path)
                .join("ROLLBACK.md")
                .exists()
        );

        let store = MetadataStore::open(&db_path).expect("store");
        assert!(
            store
                .local_write_log(&lease.workspace_id)
                .expect("write log")
                .iter()
                .all(|write| !write.path.ends_with("ROLLBACK.md"))
        );
        let stored = store
            .agent_lease_by_id(&lease.id)
            .expect("lease query")
            .expect("lease");
        assert_eq!(stored.output_state, AgentLeaseOutputState::Empty);
    }

    #[test]
    fn publish_for_review_rolls_back_when_event_append_fails() {
        let (temp, db_path) = seeded_store("agent-lease-publish-rollback");
        let project_path = temp.root().join("Code/apps/web");
        let lease = create_agent_lease(AgentLeaseCreateOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            task: "publish rollback".to_string(),
            base: AgentLeaseBase::LatestWorkspace,
            hydrate_budget_bytes: 1024 * 1024,
            work_view: true,
            device_id: DeviceId::new("device-test"),
            generated_at: now(),
        })
        .expect("lease created")
        .lease;
        let store = MetadataStore::open(&db_path).expect("store");
        store
            .connection()
            .execute(
                "CREATE TRIGGER fail_publish_event
                 BEFORE INSERT ON events
                 BEGIN
                   SELECT RAISE(FAIL, 'forced publish event failure');
                 END",
                [],
            )
            .expect("event failure trigger");
        drop(store);

        let error = invoke_agent_tool(
            Some(db_path.clone()),
            tool_request(
                &lease,
                AgentToolName::PublishOverlayForReview,
                serde_json::json!({}),
            ),
            now(),
        )
        .expect_err("publish event failure should be reported");
        assert!(matches!(error, AgentError::Event(_)));

        let store = MetadataStore::open(&db_path).expect("store");
        let stored_lease = store
            .agent_lease_by_id(&lease.id)
            .expect("lease query")
            .expect("lease");
        assert_eq!(stored_lease.output_state, AgentLeaseOutputState::Empty);
        let work_view = store
            .work_view_by_id(&lease.workspace_id, &lease.work_view_id)
            .expect("work view query")
            .expect("work view");
        assert_eq!(work_view.lifecycle, WorkViewLifecycle::Active);
    }

    #[test]
    fn search_workspace_returns_index_backed_payload_for_lease_scope() {
        let (temp, db_path) = seeded_store("agent-lease-degraded");
        let project_path = temp.root().join("Code/apps/web");
        let lease = create_agent_lease(AgentLeaseCreateOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            task: "search".to_string(),
            base: AgentLeaseBase::LatestWorkspace,
            hydrate_budget_bytes: 1024 * 1024,
            work_view: true,
            device_id: DeviceId::new("device-test"),
            generated_at: now(),
        })
        .expect("lease created")
        .lease;

        let result = invoke_agent_tool(
            Some(db_path.clone()),
            tool_request(
                &lease,
                AgentToolName::SearchWorkspace,
                serde_json::json!({"query": "auth"}),
            ),
            now(),
        )
        .expect("search");
        assert_eq!(result.outcome, AgentToolResultOutcome::Allowed);
        let payload = result.payload.expect("search payload");
        assert_eq!(
            payload.get("command").and_then(serde_json::Value::as_str),
            Some("search")
        );
        assert_eq!(
            payload.get("projectId").and_then(serde_json::Value::as_str),
            Some(lease.project_id.as_str())
        );
        assert_eq!(
            payload
                .get("workspaceId")
                .and_then(serde_json::Value::as_str),
            Some(lease.workspace_id.as_str())
        );
        assert!(payload.get("results").is_some());
    }

    #[test]
    fn search_workspace_subpath_returns_lease_relative_paths() {
        let (temp, db_path) = seeded_store("agent-lease-search-subpath");
        let project_path = temp.root().join("Code/apps/web");
        let lease = create_agent_lease(AgentLeaseCreateOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            task: "search subpath".to_string(),
            base: AgentLeaseBase::LatestWorkspace,
            hydrate_budget_bytes: 1024 * 1024,
            work_view: true,
            device_id: DeviceId::new("device-test"),
            generated_at: now(),
        })
        .expect("lease created")
        .lease;
        fs::create_dir_all(Path::new(&lease.work_view_path).join("src")).expect("src dir");
        fs::write(
            Path::new(&lease.work_view_path).join("src/auth.ts"),
            "export function authCallback() {}\n",
        )
        .expect("source");

        let result = invoke_agent_tool(
            Some(db_path.clone()),
            tool_request(
                &lease,
                AgentToolName::SearchWorkspace,
                serde_json::json!({"query": "authCallback", "path": "src"}),
            ),
            now(),
        )
        .expect("search");

        let payload = result.payload.expect("search payload");
        assert_eq!(payload["results"][0]["path"].as_str(), Some("src/auth.ts"));
    }

    #[test]
    fn search_workspace_subpath_keeps_project_root_policy_for_work_view_files() {
        let (temp, db_path) = seeded_store("agent-lease-search-subpath-policy");
        let project_path = temp.root().join("Code/apps/web");
        fs::write(project_path.join(".bowlineignore"), b"private/**\n").expect("policy");
        let lease = create_agent_lease(AgentLeaseCreateOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            task: "search private subpath".to_string(),
            base: AgentLeaseBase::LatestWorkspace,
            hydrate_budget_bytes: 1024 * 1024,
            work_view: true,
            device_id: DeviceId::new("device-test"),
            generated_at: now(),
        })
        .expect("lease created")
        .lease;
        fs::create_dir_all(Path::new(&lease.work_view_path).join("private")).expect("private dir");
        fs::write(
            Path::new(&lease.work_view_path).join("private/token.txt"),
            "hiddenNeedle\n",
        )
        .expect("hidden file");

        let result = invoke_agent_tool(
            Some(db_path.clone()),
            tool_request(
                &lease,
                AgentToolName::SearchWorkspace,
                serde_json::json!({"query": "hiddenNeedle", "path": "private"}),
            ),
            now(),
        )
        .expect("search");

        assert_eq!(result.outcome, AgentToolResultOutcome::Allowed);
        let payload = result.payload.expect("search payload");
        assert_eq!(payload["results"].as_array().expect("results").len(), 0);
    }

    #[test]
    fn search_workspace_respects_lease_file_bound_before_indexing() {
        let (temp, db_path) = seeded_store("agent-lease-search-file-bound");
        let project_path = temp.root().join("Code/apps/web");
        let mut lease = create_agent_lease(AgentLeaseCreateOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            task: "bounded search".to_string(),
            base: AgentLeaseBase::LatestWorkspace,
            hydrate_budget_bytes: 1024 * 1024,
            work_view: true,
            device_id: DeviceId::new("device-test"),
            generated_at: now(),
        })
        .expect("lease created")
        .lease;
        lease.scopes.read.max_files_per_request = Some(1);
        let store = MetadataStore::open(&db_path).expect("store");
        store.upsert_agent_lease(&lease).expect("lease update");
        drop(store);
        fs::create_dir_all(Path::new(&lease.work_view_path).join("src")).expect("src dir");
        fs::write(
            Path::new(&lease.work_view_path).join("src/a.ts"),
            "export const boundedNeedle = 1;\n",
        )
        .expect("source a");
        fs::write(
            Path::new(&lease.work_view_path).join("src/b.ts"),
            "export const boundedNeedle = 2;\n",
        )
        .expect("source b");

        let result = invoke_agent_tool(
            Some(db_path.clone()),
            tool_request(
                &lease,
                AgentToolName::SearchWorkspace,
                serde_json::json!({"query": "boundedNeedle"}),
            ),
            now(),
        )
        .expect("search");

        assert_eq!(result.outcome, AgentToolResultOutcome::Allowed);
        let payload = result.payload.expect("search payload");
        assert_eq!(payload["results"].as_array().expect("results").len(), 1);
        assert_eq!(payload["index"]["state"].as_str(), Some("stale"));
    }

    #[test]
    fn request_hydration_records_queue_entry_and_budget_reservation() {
        let (temp, db_path) = seeded_store("agent-lease-hydration-queue");
        let project_path = temp.root().join("Code/apps/web");
        let lease = create_agent_lease(AgentLeaseCreateOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            task: "hydrate".to_string(),
            base: AgentLeaseBase::LatestWorkspace,
            hydrate_budget_bytes: 1024,
            work_view: true,
            device_id: DeviceId::new("device-test"),
            generated_at: now(),
        })
        .expect("lease created")
        .lease;
        let cold_path = Path::new(&lease.work_view_path).join("cold.rs");
        fs::write(&cold_path, "fn cold() {}\n").expect("fixture file");
        let cold_len = fs::metadata(&cold_path).expect("metadata").len();

        let denied = invoke_agent_tool(
            Some(db_path.clone()),
            tool_request(
                &lease,
                AgentToolName::RequestHydration,
                serde_json::json!({"path": "cold.rs", "bytes": 0, "contentId": "cid_cold"}),
            ),
            now(),
        )
        .expect("hydration denial");
        assert_eq!(denied.outcome, AgentToolResultOutcome::Denied);
        assert_eq!(
            denied.denial.as_ref().map(|denial| denial.code.as_str()),
            Some("content-id-unverified")
        );

        let result = invoke_agent_tool(
            Some(db_path.clone()),
            tool_request(
                &lease,
                AgentToolName::RequestHydration,
                serde_json::json!({"path": "cold.rs", "bytes": 0}),
            ),
            now(),
        )
        .expect("hydration request");

        assert_eq!(result.outcome, AgentToolResultOutcome::Allowed);
        assert_eq!(
            result.payload.as_ref().unwrap()["state"].as_str(),
            Some("completed")
        );
        let store = MetadataStore::open(&db_path).expect("store");
        let queue = store.hydration_queue(&lease.workspace_id).expect("queue");
        assert_eq!(queue.len(), 1);
        assert!(queue[0].path.ends_with("/cold.rs"));
        assert_eq!(queue[0].priority, "agent-lease");
        assert_eq!(queue[0].state, "completed");
        assert_eq!(queue[0].content_id, None);
        let budget = crate::hydration_budget::lease_budget_status(
            &store,
            &lease.workspace_id,
            &lease.project_id,
            &lease.id,
            lease.hydrate_budget_bytes,
        )
        .expect("budget");
        assert_eq!(budget.used_bytes, cold_len);
        assert_eq!(budget.reserved_bytes, 0);
        drop(store);

        let duplicate = invoke_agent_tool(
            Some(db_path.clone()),
            tool_request(
                &lease,
                AgentToolName::RequestHydration,
                serde_json::json!({"path": "cold.rs", "bytes": 0}),
            ),
            now(),
        )
        .expect("duplicate hydration request");
        assert_eq!(duplicate.outcome, AgentToolResultOutcome::Allowed);
        assert_eq!(duplicate.summary, "hydration request already completed");
        assert!(duplicate.event_id.is_some());

        let store = MetadataStore::open(&db_path).expect("store");
        let queue = store.hydration_queue(&lease.workspace_id).expect("queue");
        assert_eq!(queue.len(), 1);
        let budget = crate::hydration_budget::lease_budget_status(
            &store,
            &lease.workspace_id,
            &lease.project_id,
            &lease.id,
            lease.hydrate_budget_bytes,
        )
        .expect("budget");
        assert_eq!(budget.used_bytes, cold_len);
        assert_eq!(budget.reserved_bytes, 0);
    }

    #[test]
    fn request_hydration_event_failure_releases_budget_and_fails_queue() {
        let (temp, db_path) = seeded_store("agent-lease-hydration-event-failure");
        let project_path = temp.root().join("Code/apps/web");
        let lease = create_agent_lease(AgentLeaseCreateOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            task: "hydrate event failure".to_string(),
            base: AgentLeaseBase::LatestWorkspace,
            hydrate_budget_bytes: 1024,
            work_view: true,
            device_id: DeviceId::new("device-test"),
            generated_at: now(),
        })
        .expect("lease created")
        .lease;
        let cold_path = Path::new(&lease.work_view_path).join("cold.rs");
        fs::write(&cold_path, "fn cold() {}\n").expect("fixture file");
        let store = MetadataStore::open(&db_path).expect("store");
        store
            .connection()
            .execute(
                "CREATE TRIGGER fail_hydration_event
                 BEFORE INSERT ON events
                 WHEN NEW.name = 'lease.hydration_requested'
                 BEGIN
                   SELECT RAISE(FAIL, 'forced hydration event failure');
                 END",
                [],
            )
            .expect("event failure trigger");
        drop(store);

        let error = invoke_agent_tool(
            Some(db_path.clone()),
            tool_request(
                &lease,
                AgentToolName::RequestHydration,
                serde_json::json!({"path": "cold.rs", "bytes": 0}),
            ),
            now(),
        )
        .expect_err("hydration event failure should fail the request");
        assert!(matches!(error, AgentError::Event(_)));

        let store = MetadataStore::open(&db_path).expect("store");
        let queue = store.hydration_queue(&lease.workspace_id).expect("queue");
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].state, "failed");
        let budget = crate::hydration_budget::lease_budget_status(
            &store,
            &lease.workspace_id,
            &lease.project_id,
            &lease.id,
            lease.hydrate_budget_bytes,
        )
        .expect("budget");
        assert_eq!(budget.reserved_bytes, 0);
    }

    #[test]
    fn request_hydration_queues_cold_projected_file_without_local_bytes() {
        let (temp, db_path) = seeded_store("agent-lease-cold-hydration");
        let project_path = temp.root().join("Code/apps/web");
        let lease = create_agent_lease(AgentLeaseCreateOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            task: "hydrate cold".to_string(),
            base: AgentLeaseBase::LatestWorkspace,
            hydrate_budget_bytes: 2048,
            work_view: true,
            device_id: DeviceId::new("device-test"),
            generated_at: now(),
        })
        .expect("lease created")
        .lease;
        let content_id = ContentId::new("cid_cold_remote");
        let projected_path = project_path.join("cold-remote.rs");
        let store = MetadataStore::open(&db_path).expect("store");
        store
            .put_content_locator(
                &lease.workspace_id,
                &ContentLocator {
                    content_id: content_id.clone(),
                    storage: ContentStorage::Inline,
                    raw_size: 777,
                    pack_id: None,
                    offset: None,
                    length: None,
                    chunk_ids: Vec::new(),
                },
                &now(),
            )
            .expect("locator");
        store
            .upsert_projected_node(&ProjectedNodeRecord {
                workspace_id: lease.workspace_id.clone(),
                node_id: "node_cold_remote".to_string(),
                project_id: Some(lease.project_id.clone()),
                parent_node_id: None,
                path: projected_path.display().to_string(),
                kind: NamespaceEntryKind::File,
                content_id: Some(content_id.clone()),
                hydration_state: HydrationState::Cold,
                updated_at: now(),
            })
            .expect("projected node");
        drop(store);

        let denied = invoke_agent_tool(
            Some(db_path.clone()),
            tool_request(
                &lease,
                AgentToolName::RequestHydration,
                serde_json::json!({"path": "cold-remote.rs", "bytes": 0, "contentId": "cid_old"}),
            ),
            now(),
        )
        .expect("hydration denial");
        assert_eq!(denied.outcome, AgentToolResultOutcome::Denied);
        assert_eq!(
            denied.denial.as_ref().map(|denial| denial.code.as_str()),
            Some("content-id-mismatch")
        );

        let result = invoke_agent_tool(
            Some(db_path.clone()),
            tool_request(
                &lease,
                AgentToolName::RequestHydration,
                serde_json::json!({
                    "path": "cold-remote.rs",
                    "bytes": 0,
                    "contentId": content_id.as_str()
                }),
            ),
            now(),
        )
        .expect("hydration request");

        assert_eq!(result.outcome, AgentToolResultOutcome::Allowed);
        let store = MetadataStore::open(&db_path).expect("store");
        let queue = store.hydration_queue(&lease.workspace_id).expect("queue");
        assert_eq!(queue.len(), 1);
        assert!(queue[0].path.ends_with("/cold-remote.rs"));
        assert_eq!(queue[0].content_id, Some(content_id));
        let budget = crate::hydration_budget::lease_budget_status(
            &store,
            &lease.workspace_id,
            &lease.project_id,
            &lease.id,
            lease.hydrate_budget_bytes,
        )
        .expect("budget");
        assert_eq!(budget.reserved_bytes, 777);
    }

    #[test]
    fn local_daemon_wrapper_ignores_caller_supplied_authority() {
        let (temp, db_path) = seeded_store("agent-lease-mcp-authority");
        let project_path = temp.root().join("Code/apps/web");
        let lease = create_agent_lease(AgentLeaseCreateOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            task: "mcp".to_string(),
            base: AgentLeaseBase::LatestWorkspace,
            hydrate_budget_bytes: 1024 * 1024,
            work_view: true,
            device_id: DeviceId::new("device-test"),
            generated_at: now(),
        })
        .expect("lease created")
        .lease;
        let mut request = tool_request(
            &lease,
            AgentToolName::WriteOverlayFile,
            serde_json::json!({"path": "README.md", "contents": "nope"}),
        );
        request.authority.transport = AgentToolTransport::LocalDaemon;
        request.authority.peer_credential_checked = true;
        request.authority.nonce_presented = true;

        let result = invoke_agent_tool_from_local_daemon(Some(db_path), request, false, now())
            .expect("tool result");

        assert_eq!(result.outcome, AgentToolResultOutcome::Denied);
        assert_eq!(
            result.denial.expect("denial").code,
            "transport-not-authorized"
        );
        assert!(!Path::new(&lease.work_view_path).join("README.md").exists());
    }

    #[test]
    fn scoped_path_expands_home_relative_roots() {
        let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
            return;
        };
        let root = home.join(format!(".bowline-agent-scope-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("root");
        let display_root = format!(
            "~/{}",
            root.strip_prefix(&home).expect("root under home").display()
        );

        let path = scoped_path(&display_root, "out.txt").expect("scoped path");
        let _ = fs::remove_dir_all(&root);

        assert_eq!(path, root.join("out.txt"));
    }

    #[cfg(unix)]
    #[test]
    fn tool_paths_reject_symlink_escapes() {
        use std::os::unix::fs::symlink;

        let (temp, db_path) = seeded_store("agent-lease-symlink");
        let project_path = temp.root().join("Code/apps/web");
        let lease = create_agent_lease(AgentLeaseCreateOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            task: "symlink".to_string(),
            base: AgentLeaseBase::LatestWorkspace,
            hydrate_budget_bytes: 1024 * 1024,
            work_view: true,
            device_id: DeviceId::new("device-test"),
            generated_at: now(),
        })
        .expect("lease created")
        .lease;
        let outside = temp.root().join("outside");
        fs::create_dir_all(&outside).expect("outside dir");
        fs::write(outside.join("secret.txt"), "secret").expect("outside file");
        symlink(&outside, Path::new(&lease.work_view_path).join("escape")).expect("symlink");

        let read = invoke_agent_tool(
            Some(db_path.clone()),
            tool_request(
                &lease,
                AgentToolName::ReadFileAtSnapshot,
                serde_json::json!({"path": "escape/secret.txt"}),
            ),
            now(),
        )
        .expect("read");
        assert_eq!(read.outcome, AgentToolResultOutcome::Denied);

        let write = invoke_agent_tool(
            Some(db_path.clone()),
            tool_request(
                &lease,
                AgentToolName::WriteOverlayFile,
                serde_json::json!({"path": "escape/pwned.txt", "contents": "nope"}),
            ),
            now(),
        )
        .expect("write");
        assert_eq!(write.outcome, AgentToolResultOutcome::Denied);
        assert!(!outside.join("pwned.txt").exists());

        let nested_write = invoke_agent_tool(
            Some(db_path),
            tool_request(
                &lease,
                AgentToolName::WriteOverlayFile,
                serde_json::json!({"path": "escape/new/pwned.txt", "contents": "nope"}),
            ),
            now(),
        )
        .expect("nested write");
        assert_eq!(nested_write.outcome, AgentToolResultOutcome::Denied);
        assert!(!outside.join("new").exists());
    }

    #[test]
    fn write_tool_respects_persisted_write_scope_roots() {
        let (temp, db_path) = seeded_store("agent-lease-write-scope");
        let project_path = temp.root().join("Code/apps/web");
        let mut lease = create_agent_lease(AgentLeaseCreateOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            task: "scope".to_string(),
            base: AgentLeaseBase::LatestWorkspace,
            hydrate_budget_bytes: 1024 * 1024,
            work_view: true,
            device_id: DeviceId::new("device-test"),
            generated_at: now(),
        })
        .expect("lease created")
        .lease;
        lease.scopes.write.roots = vec![
            Path::new(&lease.work_view_path)
                .join("src")
                .display()
                .to_string(),
        ];
        MetadataStore::open(&db_path)
            .expect("store")
            .upsert_agent_lease(&lease)
            .expect("lease update");

        let denied = invoke_agent_tool(
            Some(db_path.clone()),
            tool_request(
                &lease,
                AgentToolName::WriteOverlayFile,
                serde_json::json!({"path": "README.md", "contents": "nope"}),
            ),
            now(),
        )
        .expect("denied write");
        assert_eq!(denied.outcome, AgentToolResultOutcome::Denied);

        let allowed = invoke_agent_tool(
            Some(db_path),
            tool_request(
                &lease,
                AgentToolName::WriteOverlayFile,
                serde_json::json!({"path": "src/index.ts", "contents": "ok"}),
            ),
            now(),
        )
        .expect("allowed write");
        assert_eq!(allowed.outcome, AgentToolResultOutcome::Allowed);
    }

    #[test]
    fn expired_lease_denies_tool_execution() {
        let (temp, db_path) = seeded_store("agent-lease-expired");
        let project_path = temp.root().join("Code/apps/web");
        let mut lease = create_agent_lease(AgentLeaseCreateOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            task: "expired".to_string(),
            base: AgentLeaseBase::LatestWorkspace,
            hydrate_budget_bytes: 1024 * 1024,
            work_view: true,
            device_id: DeviceId::new("device-test"),
            generated_at: "2026-06-25T10:00:00Z".to_string(),
        })
        .expect("lease created")
        .lease;
        lease.expires_at = "2026-06-25T11:00:00Z".to_string();
        MetadataStore::open(&db_path)
            .expect("store")
            .upsert_agent_lease(&lease)
            .expect("lease update");

        let result = invoke_agent_tool(
            Some(db_path),
            tool_request(
                &lease,
                AgentToolName::WriteOverlayFile,
                serde_json::json!({"path": "README.md", "contents": "nope"}),
            ),
            "2026-06-25T12:00:00Z".to_string(),
        )
        .expect("tool result");

        assert_eq!(result.outcome, AgentToolResultOutcome::Denied);
        assert_eq!(result.denial.expect("denial").code, "lease-expired");
        assert!(!Path::new(&lease.work_view_path).join("README.md").exists());
    }

    #[test]
    fn write_tool_respects_lease_byte_budget() {
        let (temp, db_path) = seeded_store("agent-lease-write-budget");
        let project_path = temp.root().join("Code/apps/web");
        let lease = create_agent_lease(AgentLeaseCreateOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            task: "budget".to_string(),
            base: AgentLeaseBase::LatestWorkspace,
            hydrate_budget_bytes: 4,
            work_view: true,
            device_id: DeviceId::new("device-test"),
            generated_at: now(),
        })
        .expect("lease created")
        .lease;

        let result = invoke_agent_tool(
            Some(db_path),
            tool_request(
                &lease,
                AgentToolName::WriteOverlayFile,
                serde_json::json!({"path": "README.md", "contents": "hello"}),
            ),
            now(),
        )
        .expect("tool result");

        assert_eq!(result.outcome, AgentToolResultOutcome::Denied);
        assert_eq!(
            result.denial.expect("denial").code,
            "write-exceeds-lease-bounds"
        );
        assert!(!Path::new(&lease.work_view_path).join("README.md").exists());
    }

    #[test]
    fn blocked_lease_allows_inspection_but_denies_mutation() {
        let (temp, db_path) = seeded_store("agent-lease-blocked");
        let project_path = temp.root().join("Code/apps/web");
        let mut lease = create_agent_lease(AgentLeaseCreateOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            task: "blocked".to_string(),
            base: AgentLeaseBase::LatestWorkspace,
            hydrate_budget_bytes: 1024 * 1024,
            work_view: true,
            device_id: DeviceId::new("device-test"),
            generated_at: now(),
        })
        .expect("lease created")
        .lease;
        lease.execution_state = AgentLeaseExecutionState::Blocked;
        MetadataStore::open(&db_path)
            .expect("store")
            .upsert_agent_lease(&lease)
            .expect("lease update");

        let inspect = invoke_agent_tool(
            Some(db_path.clone()),
            tool_request(
                &lease,
                AgentToolName::ListCapabilities,
                serde_json::json!({}),
            ),
            now(),
        )
        .expect("inspect result");
        assert_eq!(inspect.outcome, AgentToolResultOutcome::Allowed);

        let write = invoke_agent_tool(
            Some(db_path),
            tool_request(
                &lease,
                AgentToolName::WriteOverlayFile,
                serde_json::json!({"path": "README.md", "contents": "nope"}),
            ),
            now(),
        )
        .expect("write result");
        assert_eq!(write.outcome, AgentToolResultOutcome::Denied);
        assert_eq!(write.denial.expect("denial").code, "lease-blocked");
        assert!(!Path::new(&lease.work_view_path).join("README.md").exists());
    }

    #[test]
    fn denied_tool_reports_event_append_failure() {
        let (temp, db_path) = seeded_store("agent-lease-denial-event-fail");
        let project_path = temp.root().join("Code/apps/web");
        let mut lease = create_agent_lease(AgentLeaseCreateOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            task: "denial".to_string(),
            base: AgentLeaseBase::LatestWorkspace,
            hydrate_budget_bytes: 1024 * 1024,
            work_view: true,
            device_id: DeviceId::new("device-test"),
            generated_at: now(),
        })
        .expect("lease created")
        .lease;
        lease.expires_at = "2026-06-25T11:00:00Z".to_string();
        let store = MetadataStore::open(&db_path).expect("store");
        store.upsert_agent_lease(&lease).expect("lease update");
        store
            .connection()
            .execute(
                "CREATE TRIGGER fail_denial_event
                 BEFORE INSERT ON events
                 BEGIN
                   SELECT RAISE(FAIL, 'forced denial event failure');
                 END",
                [],
            )
            .expect("event failure trigger");
        drop(store);

        let error = invoke_agent_tool(
            Some(db_path),
            tool_request(
                &lease,
                AgentToolName::WriteOverlayFile,
                serde_json::json!({"path": "README.md", "contents": "nope"}),
            ),
            "2026-06-25T12:00:00Z".to_string(),
        )
        .expect_err("denial event failure should be reported");

        assert!(matches!(error, AgentError::Event(_)));
        assert!(!Path::new(&lease.work_view_path).join("README.md").exists());
    }

    #[test]
    fn read_tools_respect_persisted_read_scope_bounds() {
        let (temp, db_path) = seeded_store("agent-lease-read-bounds");
        let project_path = temp.root().join("Code/apps/web");
        let mut lease = create_agent_lease(AgentLeaseCreateOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            task: "bounds".to_string(),
            base: AgentLeaseBase::LatestWorkspace,
            hydrate_budget_bytes: 1024 * 1024,
            work_view: true,
            device_id: DeviceId::new("device-test"),
            generated_at: now(),
        })
        .expect("lease created")
        .lease;
        let work_view_path = Path::new(&lease.work_view_path);
        fs::write(work_view_path.join("README.md"), "hello").expect("file");
        fs::create_dir_all(work_view_path.join("src")).expect("src dir");
        fs::write(work_view_path.join("src/index.ts"), "console.log('ok');").expect("nested file");
        lease.scopes.read.max_bytes_per_read = Some(4);
        lease.scopes.read.max_files_per_request = Some(1);
        lease.scopes.read.max_depth = Some(0);
        MetadataStore::open(&db_path)
            .expect("store")
            .upsert_agent_lease(&lease)
            .expect("lease update");

        let read = invoke_agent_tool(
            Some(db_path.clone()),
            tool_request(
                &lease,
                AgentToolName::ReadFileAtSnapshot,
                serde_json::json!({"path": "README.md"}),
            ),
            now(),
        )
        .expect("read");
        assert_eq!(read.outcome, AgentToolResultOutcome::Degraded);
        assert!(read.payload.is_none());
        let degraded = read.degraded.expect("read bounds");
        assert_eq!(degraded.max_bytes, 4);
        assert_eq!(degraded.max_files, 1);
        assert_eq!(degraded.max_depth, 0);

        let tree = invoke_agent_tool(
            Some(db_path),
            tool_request(
                &lease,
                AgentToolName::ListTreeAtSnapshot,
                serde_json::json!({"path": "."}),
            ),
            now(),
        )
        .expect("tree");
        assert_eq!(tree.outcome, AgentToolResultOutcome::Allowed);
        let payload = tree.payload.expect("tree payload");
        let entries = payload
            .get("entries")
            .and_then(serde_json::Value::as_array)
            .expect("entries");
        assert!(entries.len() <= 1);
        let bounds = payload.get("bounds").expect("bounds");
        assert_eq!(bounds["maxBytes"].as_u64(), Some(4));
        assert_eq!(bounds["maxFiles"].as_u64(), Some(1));
        assert_eq!(bounds["maxDepth"].as_u64(), Some(0));
    }

    #[test]
    fn latest_main_lease_requires_git_observer_base() {
        let (temp, db_path) = seeded_store("agent-lease-latest-main");
        let project_path = temp.root().join("Code/apps/web");

        let error = create_agent_lease(AgentLeaseCreateOptions {
            db_path: Some(db_path),
            project_path: project_path.display().to_string(),
            task: "main".to_string(),
            base: AgentLeaseBase::LatestMain,
            hydrate_budget_bytes: 1024 * 1024,
            work_view: true,
            device_id: DeviceId::new("device-test"),
            generated_at: now(),
        })
        .expect_err("latest:main should fail closed without observer state");

        assert!(
            error
                .to_string()
                .contains("latest:main base is unavailable")
        );
    }

    #[test]
    fn read_tool_denies_project_env_contents_and_audits_denial() {
        let (temp, db_path) = seeded_store("agent-lease-env-read");
        let project_path = temp.root().join("Code/apps/web");
        let lease = create_agent_lease(AgentLeaseCreateOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            task: "env".to_string(),
            base: AgentLeaseBase::LatestWorkspace,
            hydrate_budget_bytes: 1024 * 1024,
            work_view: true,
            device_id: DeviceId::new("device-test"),
            generated_at: now(),
        })
        .expect("lease created")
        .lease;
        fs::write(
            Path::new(&lease.work_view_path).join(".env.local"),
            "OPENAI_API_KEY=sk-test\n",
        )
        .expect("env file");

        let result = invoke_agent_tool(
            Some(db_path.clone()),
            tool_request(
                &lease,
                AgentToolName::ReadFileAtSnapshot,
                serde_json::json!({"path": ".env.local"}),
            ),
            now(),
        )
        .expect("read");

        assert_eq!(result.outcome, AgentToolResultOutcome::Denied);
        assert!(result.payload.is_none());
        assert!(result.event_id.is_some());
        let tree = invoke_agent_tool(
            Some(db_path.clone()),
            tool_request(
                &lease,
                AgentToolName::ListTreeAtSnapshot,
                serde_json::json!({"path": "."}),
            ),
            now(),
        )
        .expect("tree");
        assert_eq!(tree.outcome, AgentToolResultOutcome::Allowed);
        assert!(
            !serde_json::to_string(&tree.payload)
                .expect("tree payload")
                .contains(".env.local")
        );

        let write = invoke_agent_tool(
            Some(db_path.clone()),
            tool_request(
                &lease,
                AgentToolName::WriteOverlayFile,
                serde_json::json!({"path": ".env.agent", "contents": "TOKEN=secret"}),
            ),
            now(),
        )
        .expect("write");
        assert_eq!(write.outcome, AgentToolResultOutcome::Denied);
        assert!(!Path::new(&lease.work_view_path).join(".env.agent").exists());

        let events = MetadataStore::open(&db_path)
            .expect("store")
            .list_events(20)
            .expect("events");
        assert!(events.iter().any(|event| {
            event.name == EventName::LeaseToolDenied && event.lease_id == Some(lease.id.clone())
        }));
    }

    fn tool_request(
        lease: &AgentLease,
        tool: AgentToolName,
        arguments: serde_json::Value,
    ) -> AgentToolInvokeRequest {
        let request_suffix =
            stable_token(&serde_json::to_string(&arguments).unwrap_or_else(|_| "{}".to_string()));
        AgentToolInvokeRequest {
            message_type: "agent.tool.invoke".to_string(),
            protocol_version: CONTRACT_VERSION,
            request_id: format!("req_{tool:?}_{request_suffix}"),
            lease_id: lease.id.clone(),
            tool,
            authority: AgentToolAuthority {
                transport: AgentToolTransport::LocalDaemon,
                peer_credential_checked: true,
                nonce_presented: false,
            },
            arguments: match arguments {
                serde_json::Value::Object(map) => map,
                _ => Map::new(),
            },
        }
    }

    fn setup_receipt(lease: &AgentLease, command: &str, state: &str) -> SetupReceiptRecord {
        SetupReceiptRecord {
            id: format!(
                "setup_{}_{}",
                state,
                stable_token(&format!("{}:{command}", lease.id.as_str()))
            ),
            workspace_id: lease.workspace_id.clone(),
            project_id: Some(lease.project_id.clone()),
            command: command.to_string(),
            state: state.to_string(),
            recipe_hash: stable_token(command),
            approval_state: "approved".to_string(),
            trigger: "agent-test".to_string(),
            cwd: lease.write_target_path.clone(),
            os: "macos".to_string(),
            arch: "aarch64".to_string(),
            env_profile: "default".to_string(),
            output_path: None,
            redacted_summary: "redacted setup receipt".to_string(),
            receipt_json: "{}".to_string(),
            updated_at: now(),
        }
    }

    fn seeded_store(name: &str) -> (TempWorkspace, std::path::PathBuf) {
        let temp = TempWorkspace::new(name).expect("temp workspace");
        let code_root = temp.root().join("Code");
        fs::create_dir_all(code_root.join("apps/web")).expect("project dir");
        let db_path = temp.root().join(".state/local.sqlite3");
        let store = MetadataStore::open(&db_path).expect("metadata");
        let workspace_id = WorkspaceId::new("ws_code");
        let project_id = ProjectId::new("proj_web");
        store
            .insert_workspace(&workspace_id, "User Code", "2026-06-25T00:00:00Z")
            .expect("workspace");
        store
            .insert_root(
                "root_code",
                &workspace_id,
                &code_root.display().to_string(),
                "2026-06-25T00:00:00Z",
            )
            .expect("root");
        store
            .insert_project(
                &project_id,
                &workspace_id,
                "root_code",
                "apps/web",
                "2026-06-25T00:00:00Z",
            )
            .expect("project");
        store
            .set_project_latest_snapshot_id(
                &workspace_id,
                &project_id,
                &SnapshotId::new("snap_project_base"),
            )
            .expect("snapshot");
        (temp, db_path)
    }

    fn now() -> String {
        "2026-06-25T12:00:00Z".to_string()
    }
}
