use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::ids::{DeviceId, EventId, LeaseId, ProjectId, WorkspaceId};
pub use crate::wire::EventName;

pub const EVENT_SCHEMA_VERSION: u16 = 3;

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
