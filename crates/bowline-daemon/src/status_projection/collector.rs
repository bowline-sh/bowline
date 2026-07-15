use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Instant,
};

use bowline_core::{
    commands::StatusCommandOutput,
    ids::WorkspaceId,
    status::{StatusFact, StatusItem},
    wire::generated::DeviceApprovalAffordance,
};
use bowline_local::status::{
    LocalStatusCollection, LocalStatusFactCollector, LocalStatusFacts, StatusOptions,
};
use serde::Serialize;

use super::types::{StatusSource, StatusSourceRevision, StatusTimestamp};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusSourceFailurePolicy {
    RetainLastKnown,
    Discard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusCollectorFailureCode {
    LocalStatusRecoverable,
    LocalStatusUnrecoverable,
    InjectedFailure,
}

impl StatusCollectorFailureCode {
    pub fn is_recoverable(self) -> bool {
        match self {
            Self::LocalStatusRecoverable | Self::InjectedFailure => true,
            Self::LocalStatusUnrecoverable => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusCollectorFailure {
    pub source: StatusSource,
    pub code: StatusCollectorFailureCode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum StatusSourceState {
    Ready,
    Degraded,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatusSourceStateFacts {
    pub state: StatusSourceState,
    pub pending_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceTrustStatusFacts {
    pub state: StatusSourceStateFacts,
    pub facts: Vec<StatusFact>,
    pub items: Vec<StatusItem>,
    pub approvals: Vec<DeviceApprovalAffordance>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusSourceFacts {
    Metadata(Box<LocalStatusFacts>),
    SyncRuntime(StatusSourceStateFacts),
    StoreHealth(StatusSourceStateFacts),
    DeviceTrust(StatusSourceStateFacts),
    DeviceTrustDetails(DeviceTrustStatusFacts),
    UpdateAvailability(StatusSourceStateFacts),
    NotificationState(StatusSourceStateFacts),
    ServiceRuntime(StatusSourceStateFacts),
}

#[derive(Debug)]
struct SharedStatusSourceState {
    revision: StatusSourceRevision,
    facts: StatusSourceFacts,
}

#[derive(Debug, Clone)]
pub struct SharedStatusSourceHandle {
    source: StatusSource,
    shared: Arc<Mutex<SharedStatusSourceState>>,
}

impl SharedStatusSourceHandle {
    pub fn current(&self) -> Option<StatusSourceFacts> {
        self.shared.lock().ok().map(|shared| shared.facts.clone())
    }

    pub fn update(&self, facts: StatusSourceFacts) -> bool {
        if facts.source() != self.source {
            return false;
        }
        let Ok(mut shared) = self.shared.lock() else {
            return false;
        };
        if shared.facts.semantically_eq(&facts) {
            return false;
        }
        shared.revision = StatusSourceRevision::new(shared.revision.get().saturating_add(1));
        shared.facts = facts;
        true
    }
}

#[derive(Debug)]
pub struct SharedStatusSourceCollector {
    source: StatusSource,
    shared: Arc<Mutex<SharedStatusSourceState>>,
    committed_revision: Option<StatusSourceRevision>,
    staged: Option<StatusSourceCollection>,
}

impl SharedStatusSourceCollector {
    pub fn new(facts: StatusSourceFacts) -> (SharedStatusSourceHandle, Self) {
        let source = facts.source();
        let shared = Arc::new(Mutex::new(SharedStatusSourceState {
            revision: StatusSourceRevision::new(1),
            facts,
        }));
        (
            SharedStatusSourceHandle {
                source,
                shared: Arc::clone(&shared),
            },
            Self {
                source,
                shared,
                committed_revision: None,
                staged: None,
            },
        )
    }
}

impl StatusSourceCollector for SharedStatusSourceCollector {
    fn source(&self) -> StatusSource {
        self.source
    }

    fn failure_policy(&self) -> StatusSourceFailurePolicy {
        StatusSourceFailurePolicy::RetainLastKnown
    }

    fn mark_dirty(&mut self) {}

    fn stage(
        &mut self,
        observed_at: StatusTimestamp,
        _now: Instant,
    ) -> Result<StatusSourceCollection, StatusCollectorFailure> {
        if let Some(staged) = self.staged.as_ref() {
            return Ok(staged.clone());
        }
        let shared = self.shared.lock().map_err(|_| StatusCollectorFailure {
            source: self.source,
            code: StatusCollectorFailureCode::InjectedFailure,
        })?;
        if self.committed_revision == Some(shared.revision) {
            return Ok(StatusSourceCollection::Unchanged);
        }
        let staged = StatusSourceCollection::Updated {
            revision: shared.revision,
            observed_at,
            facts: shared.facts.clone(),
        };
        self.staged = Some(staged.clone());
        Ok(staged)
    }

    fn commit_staged(&mut self) {
        if let Some(StatusSourceCollection::Updated { revision, .. }) = self.staged.take() {
            self.committed_revision = Some(revision);
        }
    }

    fn abort_staged(&mut self) {}

    fn reject_staged(&mut self) {
        self.staged = None;
    }
}

impl StatusSourceFacts {
    pub fn source(&self) -> StatusSource {
        match self {
            Self::Metadata(_) => StatusSource::Metadata,
            Self::SyncRuntime(_) => StatusSource::SyncRuntime,
            Self::StoreHealth(_) => StatusSource::StoreHealth,
            Self::DeviceTrust(_) | Self::DeviceTrustDetails(_) => StatusSource::DeviceTrust,
            Self::UpdateAvailability(_) => StatusSource::UpdateAvailability,
            Self::NotificationState(_) => StatusSource::NotificationState,
            Self::ServiceRuntime(_) => StatusSource::ServiceRuntime,
        }
    }

    pub fn metadata_output(&self) -> Option<&StatusCommandOutput> {
        match self {
            Self::Metadata(facts) => Some(&facts.output),
            _ => None,
        }
    }

    pub(crate) fn state_facts(&self) -> Option<&StatusSourceStateFacts> {
        match self {
            Self::Metadata(_) => None,
            Self::SyncRuntime(facts)
            | Self::StoreHealth(facts)
            | Self::UpdateAvailability(facts)
            | Self::NotificationState(facts)
            | Self::ServiceRuntime(facts) => Some(facts),
            Self::DeviceTrust(facts) => Some(facts),
            Self::DeviceTrustDetails(facts) => Some(&facts.state),
        }
    }

    fn semantically_eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::DeviceTrustDetails(left), Self::DeviceTrustDetails(right)) => {
                left.state == right.state
                    && left.items == right.items
                    && left.approvals == right.approvals
                    && normalized_device_facts(&left.facts) == normalized_device_facts(&right.facts)
            }
            _ => self == other,
        }
    }
}

fn normalized_device_facts(facts: &[StatusFact]) -> Vec<StatusFact> {
    facts
        .iter()
        .cloned()
        .map(|mut fact| {
            fact.observed_at.clear();
            fact
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusSourceCollection {
    Updated {
        revision: StatusSourceRevision,
        observed_at: StatusTimestamp,
        facts: StatusSourceFacts,
    },
    Unchanged,
}

pub trait StatusSourceCollector: Send {
    fn source(&self) -> StatusSource;
    fn failure_policy(&self) -> StatusSourceFailurePolicy;
    fn mark_dirty(&mut self);
    /// Stage one observation without consuming it. Repeated calls after an
    /// abort must return the same observation until it is committed or rejected.
    fn stage(
        &mut self,
        observed_at: StatusTimestamp,
        now: Instant,
    ) -> Result<StatusSourceCollection, StatusCollectorFailure>;
    /// Acknowledge that the staged observation is now in the published projection.
    fn commit_staged(&mut self);
    /// Retain a valid staged observation for the next recovery batch.
    fn abort_staged(&mut self);
    /// Discard a malformed staged observation so the collector can recover.
    fn reject_staged(&mut self);
}

#[derive(Debug)]
pub struct LocalStatusProjectionCollector {
    collector: LocalStatusFactCollector,
    requested_path: Option<String>,
    workspace_scope: bool,
    configured_workspace_id: Option<WorkspaceId>,
    staged: Option<Result<StatusSourceCollection, StatusCollectorFailure>>,
}

impl LocalStatusProjectionCollector {
    pub fn new(
        db_path: Option<PathBuf>,
        requested_path: Option<String>,
        workspace_scope: bool,
    ) -> Result<Self, bowline_local::status::LocalStatusError> {
        Ok(Self {
            collector: LocalStatusFactCollector::new(db_path)?,
            requested_path,
            workspace_scope,
            configured_workspace_id: None,
            staged: None,
        })
    }

    pub fn new_for_workspace(
        db_path: PathBuf,
        requested_path: String,
        workspace_id: WorkspaceId,
    ) -> Result<Self, bowline_local::status::LocalStatusError> {
        Ok(Self {
            collector: LocalStatusFactCollector::new(Some(db_path))?,
            requested_path: Some(requested_path),
            workspace_scope: true,
            configured_workspace_id: Some(workspace_id),
            staged: None,
        })
    }

    pub fn metrics(&self) -> bowline_local::status::StatusComposerMetrics {
        self.collector.metrics()
    }
}

impl StatusSourceCollector for LocalStatusProjectionCollector {
    fn source(&self) -> StatusSource {
        StatusSource::Metadata
    }

    fn failure_policy(&self) -> StatusSourceFailurePolicy {
        StatusSourceFailurePolicy::RetainLastKnown
    }

    fn mark_dirty(&mut self) {
        // Source-change events are revision hints. The collector still checks
        // durable revisions, so an unchanged scheduler observation never forces
        // an expensive full composition.
    }

    fn stage(
        &mut self,
        observed_at: StatusTimestamp,
        now: Instant,
    ) -> Result<StatusSourceCollection, StatusCollectorFailure> {
        if let Some(staged) = self.staged.as_ref() {
            return staged.clone();
        }
        let options = StatusOptions {
            db_path: None,
            requested_path: self.requested_path.clone(),
            workspace_scope: self.workspace_scope,
            generated_at: observed_at.as_str().to_string(),
        };
        let collection = match self.configured_workspace_id.as_ref() {
            Some(workspace_id) => {
                self.collector
                    .collect_workspace_if_needed(options, workspace_id, now)
            }
            None => self.collector.collect_if_needed(options, now),
        };
        let staged = match collection {
            Ok(LocalStatusCollection::Collected(facts)) => Ok(StatusSourceCollection::Updated {
                revision: StatusSourceRevision::new(facts.revision.get()),
                observed_at,
                facts: StatusSourceFacts::Metadata(facts),
            }),
            Ok(LocalStatusCollection::Unchanged) => Ok(StatusSourceCollection::Unchanged),
            Err(error) => Err(StatusCollectorFailure {
                source: StatusSource::Metadata,
                code: if error.is_recoverable() {
                    StatusCollectorFailureCode::LocalStatusRecoverable
                } else {
                    StatusCollectorFailureCode::LocalStatusUnrecoverable
                },
            }),
        };
        self.staged = Some(staged.clone());
        staged
    }

    fn commit_staged(&mut self) {
        self.staged = None;
    }

    fn abort_staged(&mut self) {}

    fn reject_staged(&mut self) {
        self.staged = None;
    }
}
