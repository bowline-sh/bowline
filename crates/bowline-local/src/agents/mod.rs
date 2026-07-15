use std::{
    error::Error,
    fmt, fs, io,
    path::{Component, Path, PathBuf},
};

use bowline_core::{
    commands::{
        AGENT_LEASE_STATUS_CREATING, AgentCapability, AgentCapabilityState,
        AgentCompleteCommandOutput, AgentContextCommandOutput, AgentContextV1, AgentLease,
        AgentLeaseBase, AgentLeaseCreateCommandOutput, AgentLeaseDispatchState,
        AgentLeaseUpdateCommandOutput, AgentMcpGrant, AgentMcpTokenCommandOutput,
        AgentProjectReadiness, AgentPrompt, AgentPromptCommandOutput, AgentPromptRedaction,
        AgentReadinessSignal, AgentReadinessState, AgentSessionState, AgentStartWork,
        AgentToolAuthority, AgentToolCategory, AgentToolInvokeRequest, AgentToolName,
        AgentToolResult, AgentToolResultOutcome, AgentToolTransport, AgentWriteTargetMode,
        CONTRACT_VERSION, CommandName,
    },
    events::{EventName, EventSeverity, EventSubject, EventSubjectKind, WorkspaceEvent},
    ids::{DeviceId, EventId, LeaseId, PolicyVersion, ProjectId, SnapshotId, WorkViewId},
    status::{
        FreshnessVerdict, RepairCommand, StaleBaseStatus, StatusItem, StatusItemKind, StatusLevel,
        StatusSubject, StatusSubjectKind, WorkspaceStatus,
    },
    workspace_graph::normalize_workspace_path,
};
use serde_json::{Map, Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::{
    events::LocalEventError,
    metadata::{AgentMcpTokenRecord, MetadataError, MetadataStore, default_database_path},
    work_views::{
        WorkCreateOptions, WorkSelectorOptions, WorkViewError, create_work_view_with_id_and_key,
        diff_work_view_with_checkpoint, expand_display_path,
    },
};
mod context;
mod daemon;
mod error;
pub mod handoff;
mod lease;
mod mcp_token;
mod paths;
mod persistence;
mod session;
#[cfg(test)]
mod tests;
mod tools;
mod types;

pub use context::{agent_context, agent_prompt};
pub use daemon::{
    invoke_agent_tool_from_daemon, invoke_agent_tool_from_daemon_with_checkpoint,
    invoke_agent_tool_from_local_daemon,
};
pub use error::AgentError;
pub use lease::{
    DispatchedAgentLeaseIdentity, create_agent_lease, create_dispatched_agent_lease,
    default_device_id,
};
pub use mcp_token::issue_agent_mcp_token;
pub use session::{
    MAX_AGENT_LEASE_EXTENSION_HOURS, cancel_agent_session, complete_agent_session,
    extend_agent_session,
};
pub use types::{
    AgentLeaseCreateOptions, AgentLeaseExtendOptions, AgentLeaseSelectorOptions,
    AgentMcpTokenIssueOptions, DispatchedAgentLeaseCreateOptions,
};

use context::{
    attention_for_lease, capabilities_for_lease, lease_write_target_path, shell_word,
    status_for_attention,
};
#[cfg(test)]
use daemon::invoke_agent_tool;
use paths::{
    agent_work_view_id, display_path_for_agent_work_view, display_path_for_project, json_map,
    lease_work_view_name, redacted_task_label, resolve_db_path, scoped_read_path, stable_token,
};
pub(crate) use persistence::recover_provisional_agent_leases;
use persistence::{
    lease_event, load_lease, recover_provisional_agent_lease_by_id,
    rollback_created_agent_work_view, rollback_provisional_agent_lease,
};
use tools::{
    allowed_payload, denied_result, diff_tool, lease_is_expired, resolve_path_tool,
    transport_allowed,
};
use types::{DEFAULT_DEVICE_ID, DEFAULT_POLICY_VERSION};

fn log_best_effort_metadata_cleanup(context: &str, error: MetadataError) {
    eprintln!("bowline agent metadata cleanup skipped ({context}): {error}");
}

fn expiry_elapsed(expires_at: &str, generated_at: &str) -> bool {
    let expires_at = expires_at.trim();
    let Ok(expires_at) = OffsetDateTime::parse(expires_at, &Rfc3339) else {
        return true;
    };
    let Ok(generated_at) = OffsetDateTime::parse(generated_at, &Rfc3339) else {
        return true;
    };
    expires_at <= generated_at
}
