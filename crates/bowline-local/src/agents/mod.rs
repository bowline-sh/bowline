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

mod budget;
mod context;
mod daemon;
mod error;
mod hydration;
mod lease;
mod paths;
mod persistence;
#[cfg(test)]
mod tests;
mod tools;
mod types;

pub use budget::grant_agent_hydration_budget;
pub use context::{agent_context, agent_prompt};
pub use daemon::invoke_agent_tool_from_local_daemon;
pub use error::AgentError;
pub use lease::{create_agent_lease, default_device_id};
pub use types::{AgentBudgetGrantOptions, AgentLeaseCreateOptions, AgentLeaseSelectorOptions};

use context::{
    attention_for_lease, capabilities_for_lease, context_for_lease, default_env_profile,
    default_scopes, lease_write_target_path, shell_word,
};
#[cfg(test)]
use daemon::invoke_agent_tool;
use hydration::{
    hydration_queue_content_matches, hydration_target, lease_index_identity, lease_relative_filter,
};
use paths::{
    agent_path_decision, agent_read_allowed, agent_work_view_id, agent_write_allowed_decision,
    create_parent_dirs_without_symlinks, display_path_for_agent_work_view,
    display_path_for_project, ensure_no_symlink_components, json_map, lease_work_view_name,
    redacted_task_label, resolve_db_path, scoped_path, scoped_read_path, scoped_write_path,
    stable_token,
};
pub(crate) use persistence::recover_provisional_agent_leases;
use persistence::{
    append_lease_event, audit_tool_result, load_lease, persist_created_agent_lease,
    recover_provisional_agent_lease_by_id, rollback_created_agent_work_view,
    rollback_provisional_agent_lease,
};
use tools::{
    allowed_payload, complete_task, degraded_bounds, denied_result, diff_tool,
    hydration_status_tool, lease_is_expired, list_tree_tool, publish_for_review, read_file_tool,
    resolve_path_tool, rollback_agent_write_effect, run_command_tool, search_workspace_tool,
    symbol_lookup_tool, tool_allowed_for_blocked_lease, transport_allowed, write_overlay_tool,
};
use types::{
    AgentWriteEffect, DEFAULT_DEVICE_ID, DEFAULT_POLICY_VERSION, MAX_READ_BYTES, MAX_TREE_DEPTH,
    MAX_TREE_FILES,
};
