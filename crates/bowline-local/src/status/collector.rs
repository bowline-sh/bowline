#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::time::{Duration, Instant, SystemTime};

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct LocalStatusRevision(u64);

impl LocalStatusRevision {
    pub fn new(value: u64) -> Self {
        Self(value)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalStatusFacts {
    pub revision: LocalStatusRevision,
    pub observed_at: String,
    pub output: StatusCommandOutput,
}

#[derive(Debug)]
pub enum LocalStatusCollection {
    Collected(Box<LocalStatusFacts>),
    Unchanged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalStatusSourceRevision {
    database_exists: bool,
    database_len: u64,
    database_modified: Option<SystemTime>,
    database_identity: Option<(u64, u64)>,
    data_version: Option<u64>,
    local_dirty_generation: u64,
    trust_generation: u64,
    update_generation: u64,
    daemon_generation: u64,
}

pub type StatusSourceRevision = LocalStatusSourceRevision;

#[derive(Debug)]
pub enum RevisionedStatus {
    Composed(Box<StatusCommandOutput>),
    Unchanged,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StatusComposerMetrics {
    pub collector_calls: u64,
    pub collector_skips: u64,
    pub full_compositions: u64,
    pub store_opens: u64,
}

#[derive(Debug)]
pub struct LocalStatusFactCollector {
    db_path: PathBuf,
    store: Option<MetadataStore>,
    revision: Option<LocalStatusSourceRevision>,
    last_composed_at: Option<Instant>,
    local_dirty_generation: u64,
    trust_generation: u64,
    update_generation: u64,
    daemon_generation: u64,
    open_database_identity: Option<(u64, u64)>,
    next_fact_revision: u64,
    metrics: StatusComposerMetrics,
    safety_refresh_interval: Duration,
}

pub type RevisionedStatusComposer = LocalStatusFactCollector;

impl LocalStatusFactCollector {
    pub fn new(db_path: Option<PathBuf>) -> Result<Self, LocalStatusError> {
        Ok(Self {
            db_path: resolve_db_path(db_path)?,
            store: None,
            revision: None,
            last_composed_at: None,
            local_dirty_generation: 0,
            trust_generation: 0,
            update_generation: 0,
            daemon_generation: 0,
            open_database_identity: None,
            next_fact_revision: 1,
            metrics: StatusComposerMetrics::default(),
            safety_refresh_interval: STATUS_SAFETY_REFRESH_INTERVAL,
        })
    }

    pub fn mark_local_dirty(&mut self) {
        self.local_dirty_generation = self.local_dirty_generation.saturating_add(1);
    }

    pub fn mark_trust_dirty(&mut self) {
        self.trust_generation = self.trust_generation.saturating_add(1);
    }

    pub fn mark_update_dirty(&mut self) {
        self.update_generation = self.update_generation.saturating_add(1);
    }

    pub fn mark_daemon_dirty(&mut self) {
        self.daemon_generation = self.daemon_generation.saturating_add(1);
    }

    pub fn metrics(&self) -> StatusComposerMetrics {
        self.metrics
    }

    pub fn set_safety_refresh_interval(&mut self, interval: Duration) {
        self.safety_refresh_interval = interval;
    }

    pub fn collect_if_needed(
        &mut self,
        options: StatusOptions,
        now: Instant,
    ) -> Result<LocalStatusCollection, LocalStatusError> {
        self.collect_if_needed_for_workspace(options, None, now)
    }

    pub fn collect_workspace_if_needed(
        &mut self,
        options: StatusOptions,
        workspace_id: &WorkspaceId,
        now: Instant,
    ) -> Result<LocalStatusCollection, LocalStatusError> {
        self.collect_if_needed_for_workspace(options, Some(workspace_id), now)
    }

    fn collect_if_needed_for_workspace(
        &mut self,
        mut options: StatusOptions,
        workspace_id: Option<&WorkspaceId>,
        now: Instant,
    ) -> Result<LocalStatusCollection, LocalStatusError> {
        self.metrics.collector_calls = self.metrics.collector_calls.saturating_add(1);
        let revision = self.source_revision()?;
        let safety_due = self
            .last_composed_at
            .is_none_or(|last| now.duration_since(last) >= self.safety_refresh_interval);
        if self.revision.as_ref() == Some(&revision) && !safety_due {
            self.metrics.collector_skips = self.metrics.collector_skips.saturating_add(1);
            return Ok(LocalStatusCollection::Unchanged);
        }
        options.db_path = Some(self.db_path.clone());
        let observed_at = options.generated_at.clone();
        let output = self.compose_current(options, workspace_id)?;
        let fact_revision = LocalStatusRevision(self.next_fact_revision);
        self.next_fact_revision = self.next_fact_revision.saturating_add(1);
        self.metrics.full_compositions = self.metrics.full_compositions.saturating_add(1);
        self.revision = Some(self.source_revision()?);
        self.last_composed_at = Some(now);
        Ok(LocalStatusCollection::Collected(Box::new(
            LocalStatusFacts {
                revision: fact_revision,
                observed_at,
                output,
            },
        )))
    }

    pub fn compose_if_needed(
        &mut self,
        options: StatusOptions,
        now: Instant,
    ) -> Result<RevisionedStatus, LocalStatusError> {
        match self.collect_if_needed(options, now)? {
            LocalStatusCollection::Collected(facts) => {
                Ok(RevisionedStatus::Composed(Box::new(facts.output)))
            }
            LocalStatusCollection::Unchanged => Ok(RevisionedStatus::Unchanged),
        }
    }

    fn source_revision(&self) -> Result<LocalStatusSourceRevision, LocalStatusError> {
        let metadata = std::fs::metadata(&self.db_path).ok();
        Ok(LocalStatusSourceRevision {
            database_exists: metadata.is_some(),
            database_len: metadata.as_ref().map_or(0, std::fs::Metadata::len),
            database_modified: metadata.as_ref().and_then(|value| value.modified().ok()),
            database_identity: file_identity(metadata.as_ref()),
            data_version: self
                .store
                .as_ref()
                .map(MetadataStore::data_version)
                .transpose()?,
            local_dirty_generation: self.local_dirty_generation,
            trust_generation: self.trust_generation,
            update_generation: self.update_generation,
            daemon_generation: self.daemon_generation,
        })
    }

    fn compose_current(
        &mut self,
        options: StatusOptions,
        workspace_id: Option<&WorkspaceId>,
    ) -> Result<StatusCommandOutput, LocalStatusError> {
        let inspection = MetadataStore::inspect(&self.db_path);
        let current_identity = file_identity(std::fs::metadata(&self.db_path).ok().as_ref());
        if self.open_database_identity.is_some() && self.open_database_identity != current_identity
        {
            self.store = None;
        }
        match inspection.state {
            DatabaseState::Current => {
                if self.store.is_none() {
                    self.store = Some(MetadataStore::open_read_only(
                        &self.db_path,
                        MetadataStore::STATUS_PROJECTION_READER,
                    )?);
                    self.metrics.store_opens = self.metrics.store_opens.saturating_add(1);
                    self.open_database_identity = current_identity;
                }
                let state_root = self
                    .db_path
                    .parent()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("."));
                let Some(store) = self.store.as_ref() else {
                    return Err(LocalStatusError::MetadataState(DatabaseState::Missing));
                };
                compose_from_store_for_workspace(store, options, state_root, workspace_id)
            }
            DatabaseState::Missing | DatabaseState::Empty => {
                self.store = None;
                self.open_database_identity = None;
                Ok(missing_metadata_status(&options))
            }
            state => {
                self.store = None;
                self.open_database_identity = None;
                Ok(limited_metadata_status(&options, &state))
            }
        }
    }
}

#[cfg(unix)]
fn file_identity(metadata: Option<&std::fs::Metadata>) -> Option<(u64, u64)> {
    metadata.map(|value| (value.dev(), value.ino()))
}

#[cfg(not(unix))]
fn file_identity(metadata: Option<&std::fs::Metadata>) -> Option<(u64, u64)> {
    metadata.map(|value| (value.len(), 0))
}
