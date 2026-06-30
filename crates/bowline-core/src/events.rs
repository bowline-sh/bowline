use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::ids::{DeviceId, EventId, LeaseId, ProjectId, WorkspaceId};

pub const EVENT_SCHEMA_VERSION: u16 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventName {
    #[serde(rename = "namespace.created")]
    NamespaceCreated,
    #[serde(rename = "namespace.moved")]
    NamespaceMoved,
    #[serde(rename = "namespace.deleted_or_archived")]
    NamespaceDeletedOrArchived,
    #[serde(rename = "hydration.started")]
    HydrationStarted,
    #[serde(rename = "hydration.completed")]
    HydrationCompleted,
    #[serde(rename = "hydration.blocked")]
    HydrationBlocked,
    #[serde(rename = "hydration.budget_reserved")]
    HydrationBudgetReserved,
    #[serde(rename = "hydration.budget_committed")]
    HydrationBudgetCommitted,
    #[serde(rename = "hydration.budget_released")]
    HydrationBudgetReleased,
    #[serde(rename = "hydration.budget_denied")]
    HydrationBudgetDenied,
    #[serde(rename = "hydration.budget_override_granted")]
    HydrationBudgetOverrideGranted,
    #[serde(rename = "policy.classified")]
    PolicyClassified,
    #[serde(rename = "policy.needs_approval")]
    PolicyNeedsApproval,
    #[serde(rename = "policy.changed")]
    PolicyChanged,
    #[serde(rename = "env.imported")]
    EnvImported,
    #[serde(rename = "env.materialized")]
    EnvMaterialized,
    #[serde(rename = "env.revoked")]
    EnvRevoked,
    #[serde(rename = "setup.started")]
    SetupStarted,
    #[serde(rename = "setup.completed")]
    SetupCompleted,
    #[serde(rename = "setup.blocked")]
    SetupBlocked,
    #[serde(rename = "source.stale")]
    SourceStale,
    #[serde(rename = "work.created")]
    WorkCreated,
    #[serde(rename = "work.updated")]
    WorkUpdated,
    #[serde(rename = "work.review_ready")]
    WorkReviewReady,
    #[serde(rename = "work.accepted")]
    WorkAccepted,
    #[serde(rename = "work.discarded")]
    WorkDiscarded,
    #[serde(rename = "work.restored")]
    WorkRestored,
    #[serde(rename = "work.expired")]
    WorkExpired,
    #[serde(rename = "work.archived")]
    WorkArchived,
    #[serde(rename = "work.cleanup_previewed")]
    WorkCleanupPreviewed,
    #[serde(rename = "work.cleanup_completed")]
    WorkCleanupCompleted,
    #[serde(rename = "lease.created")]
    LeaseCreated,
    #[serde(rename = "lease.updated")]
    LeaseUpdated,
    #[serde(rename = "lease.expired")]
    LeaseExpired,
    #[serde(rename = "lease.completed")]
    LeaseCompleted,
    #[serde(rename = "lease.blocked")]
    LeaseBlocked,
    #[serde(rename = "lease.revoked")]
    LeaseRevoked,
    #[serde(rename = "lease.review_ready")]
    LeaseReviewReady,
    #[serde(rename = "lease.tool_invoked")]
    LeaseToolInvoked,
    #[serde(rename = "lease.tool_denied")]
    LeaseToolDenied,
    #[serde(rename = "lease.hydration_requested")]
    LeaseHydrationRequested,
    #[serde(rename = "lease.cleanup_completed")]
    LeaseCleanupCompleted,
    #[serde(rename = "overlay.changed")]
    OverlayChanged,
    #[serde(rename = "publish.requested")]
    PublishRequested,
    #[serde(rename = "conflict.created")]
    ConflictCreated,
    #[serde(rename = "conflict.bundle_created")]
    ConflictBundleCreated,
    #[serde(rename = "conflict.resolution_proposed")]
    ConflictResolutionProposed,
    #[serde(rename = "conflict.resolution_accepted")]
    ConflictResolutionAccepted,
    #[serde(rename = "conflict.resolution_rejected")]
    ConflictResolutionRejected,
    #[serde(rename = "daemon.degraded")]
    DaemonDegraded,
    #[serde(rename = "daemon.recovered")]
    DaemonRecovered,
    #[serde(rename = "device.approval_requested")]
    DeviceApprovalRequested,
    #[serde(rename = "device.approved")]
    DeviceApproved,
    #[serde(rename = "device.denied")]
    DeviceDenied,
    #[serde(rename = "device.revoked")]
    DeviceRevoked,
    #[serde(rename = "recovery_key.created")]
    RecoveryKeyCreated,
    #[serde(rename = "recovery_key.verified")]
    RecoveryKeyVerified,
    #[serde(rename = "recovery_key.rotated")]
    RecoveryKeyRotated,
    #[serde(rename = "recovery_key.revoked")]
    RecoveryKeyRevoked,
    #[serde(rename = "auth.login_started")]
    AuthLoginStarted,
    #[serde(rename = "auth.login_completed")]
    AuthLoginCompleted,
    #[serde(rename = "index.updated")]
    IndexUpdated,
    #[serde(rename = "index.degraded")]
    IndexDegraded,
    #[serde(rename = "sync.started")]
    SyncStarted,
    #[serde(rename = "sync.completed")]
    SyncCompleted,
    #[serde(rename = "sync.limited")]
    SyncLimited,
    #[serde(rename = "sync.degraded")]
    SyncDegraded,
    #[serde(rename = "sync.recovered")]
    SyncRecovered,
    #[serde(rename = "watcher.degraded")]
    WatcherDegraded,
    #[serde(rename = "watcher.recovered")]
    WatcherRecovered,
    #[serde(rename = "network.offline")]
    NetworkOffline,
    #[serde(rename = "network.recovered")]
    NetworkRecovered,
    #[serde(rename = "metadata.corrupt")]
    MetadataCorrupt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EventSeverity {
    Info,
    Attention,
    Limited,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EventSubjectKind {
    Workspace,
    Root,
    Project,
    Path,
    Snapshot,
    Content,
    Pack,
    Policy,
    EnvRecord,
    SetupReceipt,
    Conflict,
    WorkView,
    Lease,
    Overlay,
    Index,
    Device,
    Metadata,
    Component,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventSubject {
    pub kind: EventSubjectKind,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EventActorKind {
    System,
    Daemon,
    Device,
    Agent,
    User,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventActor {
    pub kind: EventActorKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventRedaction {
    pub status: EventRedactionStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EventRedactionStatus {
    NotNeeded,
    Applied,
}

impl EventRedaction {
    pub fn not_needed() -> Self {
        Self {
            status: EventRedactionStatus::NotNeeded,
            rules: Vec::new(),
        }
    }

    pub fn applied(rules: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            status: EventRedactionStatus::Applied,
            rules: rules.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceEvent {
    #[serde(default = "event_schema_version")]
    pub schema_version: u16,
    pub id: EventId,
    pub name: EventName,
    pub occurred_at: String,
    pub severity: EventSeverity,
    pub summary: String,
    pub workspace_id: WorkspaceId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_id: Option<LeaseId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<DeviceId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<EventSubject>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<EventActor>,
    #[serde(default, skip_serializing_if = "empty_payload")]
    pub payload: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causation_id: Option<EventId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<EventId>,
    #[serde(default = "EventRedaction::not_needed")]
    pub redaction: EventRedaction,
}

impl WorkspaceEvent {
    pub fn new(
        id: EventId,
        name: EventName,
        occurred_at: impl Into<String>,
        severity: EventSeverity,
        summary: impl Into<String>,
        workspace_id: WorkspaceId,
    ) -> Self {
        Self {
            schema_version: EVENT_SCHEMA_VERSION,
            id,
            name,
            occurred_at: occurred_at.into(),
            severity,
            summary: summary.into(),
            workspace_id,
            project_id: None,
            path: None,
            lease_id: None,
            device_id: None,
            subject: None,
            actor: None,
            payload: Map::new(),
            causation_id: None,
            correlation_id: None,
            redaction: EventRedaction::not_needed(),
        }
    }
}

fn event_schema_version() -> u16 {
    EVENT_SCHEMA_VERSION
}

fn empty_payload(payload: &Map<String, Value>) -> bool {
    payload.is_empty()
}
