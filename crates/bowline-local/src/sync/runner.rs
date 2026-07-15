use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt, fs, io,
    path::{Path, PathBuf},
};

use super::conflicts::{set_conflict_bundle_object, unresolved_conflict_upload_overrides};
use super::upload::{UploadConflictBundleRequest, upload_conflict_bundle_object};
use super::{
    CandidateBase, CoalesceContext, CoalesceError, ConflictBundleError, ConflictFile,
    ConflictRecord, DownloadError, FullScanReason, MergeError, ObservationWriteScope, ScanScope,
    SnapshotContent, StatCacheSession, UploadError, UploadOutcome, import_snapshot_by_id,
    prepare_pending_conflict_occurrence_operations, unresolved_conflict_paths,
    upload_snapshot_candidate_with_checkpoints,
};
use crate::env::import::{
    EnvImportError, PreparedEnvImport, apply_prepared_env_records, prepare_env_records_from_scan,
    records_for_env_bytes,
};
use crate::metadata::{
    CurrentNamespaceEntryRecord, DEFAULT_DATABASE_FILE, MetadataCacheRecord, MetadataCacheState,
    MetadataError, MetadataLogicalId, MetadataObjectBindingRecord, MetadataObjectKey,
    MetadataRecordKind as LocalMetadataRecordKind, MetadataRecordRef, MetadataStore,
    MetadataVerificationState, ObservedLocalPath, PreparationLeaseState, ProjectUpsert,
    ProjectionSlice, SnapshotRecord, SyncClaimCheck, SyncClaimHandle, SyncClaimTransition,
    SyncOperationCheckpointRecord, SyncOperationKind, SyncOperationRecord, SyncOperationState,
    WorkViewAcceptClaimCheck, WorkViewAcceptClaimHandle, WorkspaceRelativePath,
    WorkspaceSyncHeadRecord,
};
use crate::sync::merge_plugins::{
    MergePluginAuditRecord, MergePluginConfigError, ProjectMergePluginRegistry,
};
use crate::work_views::WorkViewOverlaySyncError;
use bowline_control_plane::{
    ControlPlaneClient, ControlPlaneError, WorkspaceRef, WorkspaceRefHistoryRecord,
};
#[cfg(test)]
use bowline_core::workspace_graph::workspace_content_id;
use bowline_core::{
    events::{EventName, EventSeverity, EventSubject, EventSubjectKind, WorkspaceEvent},
    git_worktree_link::worktree_link_file,
    ids::{ContentId, DeviceId, EventId, ProjectId, SnapshotId, WorkspaceId},
    namespace_snapshot::{
        EntryVisitor, NamespaceOperationBudget, NamespaceOperationContext, NamespaceReadError,
        NamespaceVisitControl,
    },
    policy::{MaterializationMode, PathClassification},
    workspace_graph::{
        HydrationState, NamespaceEntry, NamespaceEntryKind, RefKind, SnapshotKind,
        SnapshotManifest, WorkspaceRef as SnapshotRef,
    },
};
use bowline_storage::{ByteStore, CacheError, StorageKey};

mod base_locators;
mod cancellation;
mod error;
mod helpers;
mod import;
mod materialization_guard;
mod materialization_plan;
#[cfg(test)]
mod materialization_tests;
mod page_persistence;
#[cfg(test)]
mod permission_reconcile;
mod permissions;
mod persistence;
mod plan;
mod preparation;
mod reason_code;
mod scan_cancellation;
mod stale_merge;
mod stat_cache_integration;
#[cfg(test)]
mod tests;
mod work_view_accept;
mod worktree_registration;

use base_locators::BaseLocatorSource;
pub use error::{SyncRunnerError, SyncRunnerFailureSource};
use plan::{ObservedSnapshotIds, SyncAction, SyncDecisionFacts, plan_sync_action};
// `plan::SnapshotId` is referenced fully-qualified; the bare `SnapshotId` name is
// already bound to `bowline_core::ids::SnapshotId` above.
use helpers::*;
use materialization_guard::*;
use materialization_plan::*;
#[cfg(test)]
use permission_reconcile::*;
use reason_code::CheckpointReasonCode;
pub use reason_code::SyncExternalFailureCode;
use stat_cache_integration::FillBytesScope;

#[derive(Debug, Clone)]
pub struct SyncRunnerOptions {
    pub root: PathBuf,
    pub state_root: PathBuf,
    pub workspace_id: WorkspaceId,
    pub device_id: DeviceId,
    pub workspace_content_key: [u8; 32],
    pub storage_key: StorageKey,
    pub key_epoch: u32,
    pub generated_at: String,
    pub sync_claim: Option<SyncClaimHandle>,
    pub scan_scope: ScanScope,
}

#[derive(Debug, Clone)]
pub struct WorkViewAcceptExecutionInput {
    pub operation_id: String,
    pub work_view_id: bowline_core::ids::WorkViewId,
    pub selected_paths: Vec<String>,
    pub claim: WorkViewAcceptClaimHandle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkViewAcceptExecutionOutcome {
    Completed {
        workspace_ref: WorkspaceRef,
        snapshot_id: SnapshotId,
        cancelled_late: bool,
    },
    Cancelled,
    ReviewRequired {
        reason: crate::metadata::WorkViewAcceptReviewReason,
        result_json: String,
    },
    RetryStale {
        workspace_ref: WorkspaceRef,
    },
    OwnershipLost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LongOperationCancellationPoint {
    BeforeStart,
    BeforeExternalCall,
    BetweenChunks,
    BeforeStagePublish,
    BeforeCommitFence,
    BetweenMaterializationTasks,
    BeforeMaterializationMutation,
}

impl LongOperationCancellationPoint {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BeforeStart => "before-start",
            Self::BeforeExternalCall => "before-external-call",
            Self::BetweenChunks => "between-chunks",
            Self::BeforeStagePublish => "before-stage-publish",
            Self::BeforeCommitFence => "before-commit-fence",
            Self::BetweenMaterializationTasks => "between-materialization-tasks",
            Self::BeforeMaterializationMutation => "before-materialization-mutation",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncTickOutcome {
    NoWorkspaceRef,
    NoChanges,
    Imported(WorkspaceRef),
    Uploaded(Box<UploadOutcome>),
    Merged(Box<UploadOutcome>),
    Conflicted(Vec<ConflictRecord>),
}

pub struct SyncRunner<'a> {
    control_plane: &'a dyn ControlPlaneClient,
    byte_store: &'a dyn ByteStore,
    options: SyncRunnerOptions,
    work_view_accept_claim: Option<WorkViewAcceptClaimHandle>,
    observed_base_ref: Option<WorkspaceRef>,
    store: RefCell<Option<MetadataStore>>,
    last_scan_scope: RefCell<ScanScope>,
    last_scan_stats: RefCell<super::ScanStats>,
    remote_domain_committed: Cell<bool>,
    local_materialization_committed: Cell<bool>,
    cancellation_requested_after_commit: Cell<bool>,
    last_cancellation_point: Cell<Option<LongOperationCancellationPoint>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ImportedHydrationSelection {
    AllFiles,
    #[cfg(test)]
    RequiredFiles,
    Paths(BTreeSet<String>),
}

enum LocalHeadMetadataUpdate<'a> {
    CommittedScan {
        candidate: &'a super::SnapshotCandidate,
        bound_snapshot: Option<&'a SnapshotContent>,
    },
    FreshScan {
        bound_snapshot: Option<&'a SnapshotContent>,
    },
}

struct PreparedScanMetadata {
    report: crate::scanner::ScanReport,
    full_observation: bool,
}

enum PreparedLocalHeadMetadataUpdate<'a> {
    CommittedScan {
        candidate: &'a super::SnapshotCandidate,
        scan: Option<PreparedScanMetadata>,
        bound_snapshot: Option<&'a SnapshotContent>,
    },
    FreshScan {
        candidate: Box<super::SnapshotCandidate>,
        scan: Option<PreparedScanMetadata>,
        bound_snapshot: Option<&'a SnapshotContent>,
    },
}

/// Everything a sync tick observes before it decides an action: the refs, the
/// freshly coalesced candidate, the conflict-derived inputs execution needs, and
/// the cheap `SyncDecisionFacts` the pure planner consumes. Plain owned struct
/// (no store lifetime) so it can move from `observe` into `execute_sync_action`.
struct SyncObservation {
    base_ref: WorkspaceRef,
    candidate_base_ref: WorkspaceRef,
    candidate: super::SnapshotCandidate,
    excluded_paths: BTreeSet<String>,
    preserved_exception_entries: Vec<NamespaceEntry>,
    conflict_upload_overrides: BTreeMap<String, Vec<u8>>,
    facts: SyncDecisionFacts,
}

/// Build the planner's cheap facts from the observed refs and candidate. Snapshot
/// ids cross into the pure planner as `plan::SnapshotId`, not raw domain strings.
fn decision_facts_for(
    base_ref: &WorkspaceRef,
    candidate_base_ref: &WorkspaceRef,
    local_head: Option<&WorkspaceRef>,
    candidate: &super::SnapshotCandidate,
) -> SyncDecisionFacts {
    SyncDecisionFacts::new(
        ObservedSnapshotIds::new(
            plan::SnapshotId::new(base_ref.snapshot_id.clone()),
            plan::SnapshotId::new(candidate_base_ref.snapshot_id.clone()),
            plan::SnapshotId::new(candidate.snapshot.manifest.snapshot_id.as_str()),
        ),
        local_head.is_some(),
        candidate.snapshot.manifest.entry_count == 0,
    )
}

impl<'a> SyncRunner<'a> {
    pub fn new(
        control_plane: &'a dyn ControlPlaneClient,
        byte_store: &'a dyn ByteStore,
        options: SyncRunnerOptions,
    ) -> Self {
        Self {
            control_plane,
            byte_store,
            options,
            work_view_accept_claim: None,
            observed_base_ref: None,
            store: RefCell::new(None),
            last_scan_scope: RefCell::new(ScanScope::default()),
            last_scan_stats: RefCell::new(super::ScanStats::default()),
            remote_domain_committed: Cell::new(false),
            local_materialization_committed: Cell::new(false),
            cancellation_requested_after_commit: Cell::new(false),
            last_cancellation_point: Cell::new(None),
        }
    }

    pub fn new_with_base_ref(
        control_plane: &'a dyn ControlPlaneClient,
        byte_store: &'a dyn ByteStore,
        options: SyncRunnerOptions,
        base_ref: WorkspaceRef,
    ) -> Self {
        Self {
            control_plane,
            byte_store,
            options,
            work_view_accept_claim: None,
            observed_base_ref: Some(base_ref),
            store: RefCell::new(None),
            last_scan_scope: RefCell::new(ScanScope::default()),
            last_scan_stats: RefCell::new(super::ScanStats::default()),
            remote_domain_committed: Cell::new(false),
            local_materialization_committed: Cell::new(false),
            cancellation_requested_after_commit: Cell::new(false),
            last_cancellation_point: Cell::new(None),
        }
    }

    pub fn with_work_view_accept_claim(mut self, claim: WorkViewAcceptClaimHandle) -> Self {
        self.work_view_accept_claim = Some(claim);
        self
    }

    pub fn last_scan_scope(&self) -> ScanScope {
        self.last_scan_scope.borrow().clone()
    }

    pub fn last_scan_stats(&self) -> super::ScanStats {
        self.last_scan_stats.borrow().clone()
    }

    pub fn cancellation_requested_after_commit(&self) -> bool {
        self.cancellation_requested_after_commit.get()
    }

    pub fn last_cancellation_point(&self) -> Option<LongOperationCancellationPoint> {
        self.last_cancellation_point.get()
    }

    /// Observe the remote ref, read local head, coalesce the workspace scan, and
    /// package everything the decision + execution stages need. Returns `None`
    /// only for the empty-workspace early-out (no remote ref yet) so
    /// `SyncObservation.base_ref` stays non-`Option`. Every checkpoint, its
    /// ordering, and the `last_scan_scope`/`last_scan_stats` cache writes are
    /// preserved exactly as the pre-extraction `tick` prologue performed them.
    fn observe(&self) -> Result<Option<SyncObservation>, SyncRunnerError> {
        let base_ref = match &self.observed_base_ref {
            Some(base_ref) => base_ref.clone(),
            None => {
                let Some(base_ref) = self
                    .control_plane
                    .get_workspace_ref(&self.options.workspace_id)
                    .map_err(UploadError::ControlPlane)?
                else {
                    return Ok(None);
                };
                base_ref
            }
        };
        self.record_sync_checkpoint(
            "remote-ref-observed",
            "completed",
            &checkpoint_payload(&SnapshotVersionPayload {
                snapshot_id: &base_ref.snapshot_id,
                version: base_ref.version,
            })?,
        )?;
        self.import_remote_ref_history(&base_ref.snapshot_id)?;
        self.enqueue_pending_conflict_occurrences()?;
        let local_head = self.read_local_head()?;
        let candidate_base_ref = match &local_head {
            Some(head) if head.snapshot_id != base_ref.snapshot_id => head.clone(),
            None if base_ref.snapshot_id != EMPTY_SNAPSHOT_ID => {
                empty_workspace_ref(self.options.workspace_id.clone())
            }
            _ => base_ref.clone(),
        };
        let excluded_paths = unresolved_conflict_paths(&self.options.state_root)?;
        let conflict_upload_overrides =
            unresolved_conflict_upload_overrides(&self.options.state_root, &self.options.root)?;
        let local_head_snapshot = if matches!(self.options.scan_scope, ScanScope::Full(_)) {
            None
        } else if let Some(head) = local_head.as_ref() {
            let snapshot_id = SnapshotId::new(head.snapshot_id.clone());
            self.with_store_sync(|store| {
                store
                    .snapshot(&self.options.workspace_id, &snapshot_id)?
                    .map(|record| crate::sync::load_cached_snapshot(store, &record))
                    .transpose()
                    .map_err(|error| SyncRunnerError::StateIo(io::Error::other(error)))
            })?
        } else {
            None
        };
        let scan_scope =
            self.effective_scan_scope(local_head.as_ref(), local_head_snapshot.as_ref())?;
        *self.last_scan_scope.borrow_mut() = scan_scope.clone();
        let preserved_exception_entries =
            self.preserved_exception_entries(&candidate_base_ref, &excluded_paths)?;
        let base_reuse = self.load_base_reuse_locators(
            &candidate_base_ref,
            local_head.as_ref(),
            local_head_snapshot.as_ref(),
        );
        if let BaseLocatorSource::Unavailable(reason) = base_reuse.source {
            self.record_sync_checkpoint(
                "source-pack-reuse-unavailable",
                "limited",
                &checkpoint_payload(&ReasonPayload {
                    reason: reason.as_str(),
                })?,
            )?;
        }
        let mut stat_cache = self.load_stat_cache_session(&scan_scope)?;
        let preparation_root = self.options.state_root.join("preparations");
        let scan_cancellation = self.scan_namespace_cancellation()?;
        let candidate_result = super::coalescer::coalesce_workspace_scan_cached(
            super::coalescer::CoalesceScanRequest {
                root: &self.options.root,
                workspace_id: self.options.workspace_id.clone(),
                base_ref: &candidate_base_ref,
                device_id: self.options.device_id.clone(),
                workspace_content_key: self.options.workspace_content_key,
                created_at: self.options.generated_at.clone(),
                context: CoalesceContext {
                    paths: &excluded_paths,
                    prior_snapshot: local_head_snapshot.as_ref(),
                    namespace_cancellation: scan_cancellation.as_ref().map(|cancellation| {
                        cancellation as &dyn bowline_core::namespace_snapshot::NamespaceCancellation
                    }),
                    preserved_entries: &preserved_exception_entries,
                    file_overrides: &conflict_upload_overrides,
                    base_locators: &base_reuse.locators,
                    preparation_root: Some(&preparation_root),
                },
                stat_cache: Some(&mut stat_cache),
                scan_scope,
            },
        );
        let mut candidate =
            self.finish_namespace_scan(scan_cancellation.as_ref(), candidate_result)?;
        self.adopt_existing_preparation(&mut candidate)?;
        *self.last_scan_stats.borrow_mut() = candidate.scan_stats.clone();
        self.fail_if_candidate_has_stat_cache_divergence(&candidate)?;
        self.record_sync_checkpoint(
            "snapshot-candidate-built",
            "completed",
            &checkpoint_payload(&SnapshotFileCountPayload {
                snapshot_id: candidate.snapshot.manifest.snapshot_id.as_str(),
                file_count: candidate.snapshot.manifest.entry_count as usize,
            })?,
        )?;
        self.record_manifest_identity_checkpoint(&candidate)?;
        let facts = decision_facts_for(
            &base_ref,
            &candidate_base_ref,
            local_head.as_ref(),
            &candidate,
        );
        Ok(Some(SyncObservation {
            base_ref,
            candidate_base_ref,
            candidate,
            excluded_paths,
            preserved_exception_entries,
            conflict_upload_overrides,
            facts,
        }))
    }

    pub fn tick(&self) -> Result<SyncTickOutcome, SyncRunnerError> {
        let Some(obs) = self.observe()? else {
            return Ok(SyncTickOutcome::NoWorkspaceRef);
        };
        let action = if self.has_pending_materialization_retry(&obs.base_ref.snapshot_id)? {
            SyncAction::Import
        } else {
            plan_sync_action(&obs.facts)
        };
        self.execute_sync_action(action, obs)
    }

    fn has_pending_materialization_retry(
        &self,
        snapshot_id: &SnapshotId,
    ) -> Result<bool, SyncRunnerError> {
        self.with_store(|store| {
            store.has_pending_materialization_retry(&self.options.workspace_id, snapshot_id)
        })
    }

    fn complete_current_materialization(
        &self,
        workspace_ref: &WorkspaceRef,
    ) -> Result<(), SyncRunnerError> {
        self.with_store(|store| {
            store.with_committed(|store| {
                store.complete_materialization_snapshot(
                    &self.options.workspace_id,
                    &workspace_ref.snapshot_id,
                    &self.options.generated_at,
                )?;
                store.promote_ready_current_namespace_hydration(
                    &self.options.workspace_id,
                    &workspace_ref.snapshot_id,
                    &self.options.generated_at,
                )?;
                Ok::<(), MetadataError>(())
            })
        })
    }

    /// The only IO/CAS/persistence site of a sync tick. Each arm is the verbatim
    /// side-effect body of the pre-split `tick` cascade, dispatched by the pure
    /// `SyncAction` the planner chose. Helpers are unchanged; the public
    /// `SyncTickOutcome` contract is preserved exactly.
    fn execute_sync_action(
        &self,
        action: SyncAction,
        mut obs: SyncObservation,
    ) -> Result<SyncTickOutcome, SyncRunnerError> {
        match action {
            SyncAction::NoChanges => {
                self.persist_candidate_preparation(&mut obs.candidate)?;
                self.complete_local_head(
                    &obs.base_ref,
                    LocalHeadMetadataUpdate::CommittedScan {
                        candidate: &obs.candidate,
                        bound_snapshot: None,
                    },
                )?;
                self.complete_current_materialization(&obs.base_ref)?;
                self.finish_candidate_preparation(
                    &obs.candidate,
                    PreparationLeaseState::Abandoned,
                )?;
                Ok(SyncTickOutcome::NoChanges)
            }
            SyncAction::Import => {
                self.persist_candidate_preparation(&mut obs.candidate)?;
                self.import_remote_structure(&obs.base_ref, Some(&obs.candidate_base_ref))?;
                self.complete_local_head(
                    &obs.base_ref,
                    LocalHeadMetadataUpdate::FreshScan {
                        bound_snapshot: None,
                    },
                )?;
                self.complete_current_materialization(&obs.base_ref)?;
                self.finish_candidate_preparation(
                    &obs.candidate,
                    PreparationLeaseState::Abandoned,
                )?;
                Ok(SyncTickOutcome::Imported(obs.base_ref))
            }
            SyncAction::Materialize => {
                self.persist_candidate_preparation(&mut obs.candidate)?;
                self.import_and_materialize_remote(&obs.base_ref, None)?;
                self.complete_local_head(
                    &obs.base_ref,
                    LocalHeadMetadataUpdate::FreshScan {
                        bound_snapshot: None,
                    },
                )?;
                self.complete_current_materialization(&obs.base_ref)?;
                self.finish_candidate_preparation(
                    &obs.candidate,
                    PreparationLeaseState::Abandoned,
                )?;
                Ok(SyncTickOutcome::Imported(obs.base_ref))
            }
            SyncAction::StaleMerge => {
                self.fill_candidate_bytes(&mut obs.candidate, FillBytesScope::AllHits)?;
                self.persist_candidate_preparation(&mut obs.candidate)?;
                let cleanup_candidate = obs.candidate.clone();
                let result = self.resolve_stale_candidate(obs.candidate, obs.base_ref);
                self.finish_candidate_preparation(
                    &cleanup_candidate,
                    PreparationLeaseState::Abandoned,
                )?;
                result
            }
            SyncAction::Upload => {
                let verify_shard =
                    crate::sync::stat_cache::verify_shard_for_timestamp(&self.options.generated_at);
                self.fill_candidate_bytes(
                    &mut obs.candidate,
                    FillBytesScope::UploadShardSampled { verify_shard },
                )?;
                self.persist_candidate_preparation(&mut obs.candidate)?;
                self.reference_candidate_preparation(&obs.candidate)?;
                let (mut uploaded_candidate, outcome) = self.upload_candidate_with_reuse_fallback(
                    obs.candidate,
                    &obs.candidate_base_ref,
                    &obs.excluded_paths,
                    &obs.preserved_exception_entries,
                    &obs.conflict_upload_overrides,
                )?;
                match outcome {
                    UploadOutcome::Advanced {
                        ref workspace_ref, ..
                    } => {
                        let bound_snapshot = outcome.bound_snapshot();
                        self.complete_local_head(
                            workspace_ref,
                            LocalHeadMetadataUpdate::CommittedScan {
                                candidate: &uploaded_candidate,
                                bound_snapshot,
                            },
                        )?;
                        self.finish_candidate_preparation(
                            &uploaded_candidate,
                            PreparationLeaseState::Committed,
                        )?;
                        Ok(SyncTickOutcome::Uploaded(Box::new(outcome)))
                    }
                    UploadOutcome::Stale { stale, .. } => {
                        // Upload sampling leaves out-of-shard hit paths byte-less;
                        // merge's three-way tail has no locator escape.
                        self.fill_candidate_bytes(
                            &mut uploaded_candidate,
                            FillBytesScope::AllHits,
                        )?;
                        let cleanup_candidate = uploaded_candidate.clone();
                        let result =
                            self.resolve_stale_candidate(uploaded_candidate, stale.current);
                        self.finish_candidate_preparation(
                            &cleanup_candidate,
                            PreparationLeaseState::Abandoned,
                        )?;
                        result
                    }
                }
            }
        }
    }

    fn complete_local_head(
        &self,
        workspace_ref: &WorkspaceRef,
        metadata_update: LocalHeadMetadataUpdate<'_>,
    ) -> Result<(), SyncRunnerError> {
        let prepared = self.prepare_local_head_metadata_update(workspace_ref, metadata_update)?;
        self.commit_local_head_metadata(workspace_ref, prepared)?;
        self.enqueue_pending_conflict_occurrences()?;
        self.enqueue_work_view_overlay_sync(workspace_ref)?;
        Ok(())
    }

    fn enqueue_work_view_overlay_sync(
        &self,
        workspace_ref: &WorkspaceRef,
    ) -> Result<(), SyncRunnerError> {
        let operation = super::work_view_overlay_sync_operation(
            workspace_ref,
            &self.options.device_id,
            &self.options.generated_at,
        )?;
        self.with_store(|store| {
            store.enqueue_sync_operation(&operation)?;
            Ok(())
        })
    }

    fn upload_candidate_with_reuse_fallback(
        &self,
        candidate: super::SnapshotCandidate,
        candidate_base_ref: &WorkspaceRef,
        excluded_paths: &BTreeSet<String>,
        preserved_exception_entries: &[NamespaceEntry],
        file_overrides: &BTreeMap<String, Vec<u8>>,
    ) -> Result<(super::SnapshotCandidate, UploadOutcome), SyncRunnerError> {
        match self.upload_candidate_with_checkpoints(&candidate) {
            Ok(outcome) => Ok((candidate, outcome)),
            Err(SyncRunnerError::Upload(UploadError::ReusedPackMissing { pack_id })) => {
                self.record_pack_reuse_disabled(&pack_id)?;
                self.finish_candidate_preparation(&candidate, PreparationLeaseState::Abandoned)?;
                let preparation_root = self.options.state_root.join("preparations");
                let scan_cancellation = self.scan_namespace_cancellation()?;
                let rebuilt_result = super::coalescer::coalesce_workspace_scan_excluding(
                    &self.options.root,
                    self.options.workspace_id.clone(),
                    candidate_base_ref,
                    self.options.device_id.clone(),
                    self.options.workspace_content_key,
                    self.options.generated_at.clone(),
                    CoalesceContext {
                        paths: excluded_paths,
                        prior_snapshot: None,
                        namespace_cancellation: scan_cancellation.as_ref().map(|cancellation| {
                            cancellation
                                as &dyn bowline_core::namespace_snapshot::NamespaceCancellation
                        }),
                        preserved_entries: preserved_exception_entries,
                        file_overrides,
                        base_locators: &BTreeMap::new(),
                        preparation_root: Some(&preparation_root),
                    },
                );
                let mut rebuilt =
                    self.finish_namespace_scan(scan_cancellation.as_ref(), rebuilt_result)?;
                self.persist_candidate_preparation(&mut rebuilt)?;
                self.reference_candidate_preparation(&rebuilt)?;
                let outcome = self.upload_candidate_with_checkpoints(&rebuilt)?;
                Ok((rebuilt, outcome))
            }
            Err(error) => Err(error),
        }
    }

    fn project_merge_plugins(&self) -> Result<ProjectMergePluginRegistry, SyncRunnerError> {
        let approvals =
            self.with_store(|store| store.merge_plugin_approvals(&self.options.workspace_id))?;
        match crate::sync::merge_plugins::MergePluginRegistry::load_project(
            &self.options.root,
            &self.options.workspace_id,
            &approvals,
        ) {
            Ok(registry) => Ok(registry),
            Err(_error) => {
                // Redacted: a merge-plugin config error can carry the config
                // file path, so the checkpoint carries only the fixed code.
                self.record_sync_checkpoint(
                    "merge-plugin-config-invalid",
                    "limited",
                    &checkpoint_payload(&ReasonPayload {
                        reason: CheckpointReasonCode::MergePluginConfigInvalid.as_code(),
                    })?,
                )?;
                Ok(ProjectMergePluginRegistry {
                    registry: crate::sync::merge_plugins::MergePluginRegistry::built_in(),
                    approval_requests: Vec::new(),
                    config_path: self.options.root.join(".bowlinemerge.toml"),
                })
            }
        }
    }

    fn append_merge_plugin_approval_events(&self, plugins: &ProjectMergePluginRegistry) {
        if plugins.approval_requests.is_empty() {
            return;
        }
        if let Err(error) = self.with_store(|store| {
            for request in &plugins.approval_requests {
                let mut event = WorkspaceEvent::new(
                    merge_plugin_event_id(
                        EventName::PolicyNeedsApproval,
                        &request.plugin.stable_key(),
                        &self.options.generated_at,
                    ),
                    EventName::PolicyNeedsApproval,
                    self.options.generated_at.clone(),
                    EventSeverity::Attention,
                    format!(
                        "Merge plugin `{}` {} needs approval before automatic merge.",
                        request.plugin.id, request.plugin.version
                    ),
                    self.options.workspace_id.clone(),
                );
                event.path = Some(plugins.config_path.display().to_string());
                event.device_id = Some(self.options.device_id.clone());
                event.subject = Some(EventSubject {
                    kind: EventSubjectKind::Policy,
                    id: request.plugin.stable_key(),
                    path: Some(plugins.config_path.display().to_string()),
                });
                event.payload = merge_plugin_approval_payload(request);
                store
                    .append_event(event)
                    .map_err(|error| MetadataError::InvalidStorageMetadata(error.to_string()))?;
            }
            Ok(())
        }) {
            report_event_append_failure("merge plugin approval event append", &error);
        }
    }

    fn append_merge_plugin_applied_events(
        &self,
        records: &[MergePluginAuditRecord],
        remote_ref: &WorkspaceRef,
    ) {
        if records.is_empty() {
            return;
        }
        if let Err(error) = self.with_store(|store| {
            for record in records {
                let mut event = WorkspaceEvent::new(
                    merge_plugin_event_id(
                        EventName::MergePluginApplied,
                        &format!("{}:{}", record.plugin.stable_key(), record.path),
                        &self.options.generated_at,
                    ),
                    EventName::MergePluginApplied,
                    self.options.generated_at.clone(),
                    EventSeverity::Info,
                    format!(
                        "Merge plugin `{}` {} produced a validated automatic merge for `{}`.",
                        record.plugin.id, record.plugin.version, record.path
                    ),
                    self.options.workspace_id.clone(),
                );
                event.path = Some(record.path.clone());
                event.device_id = Some(self.options.device_id.clone());
                event.subject = Some(EventSubject {
                    kind: EventSubjectKind::Path,
                    id: record.path.clone(),
                    path: Some(record.path.clone()),
                });
                event.payload = merge_plugin_applied_payload(record, remote_ref);
                store
                    .append_event(event)
                    .map_err(|error| MetadataError::InvalidStorageMetadata(error.to_string()))?;
            }
            Ok(())
        }) {
            report_event_append_failure("merge plugin applied event append", &error);
        }
    }

    pub(super) fn ensure_conflict_bundle_object(
        &self,
        conflict: &mut ConflictRecord,
        files: &[ConflictFile],
    ) -> Result<(), SyncRunnerError> {
        if conflict.bundle_object.is_some() {
            return Ok(());
        }
        let pointer = upload_conflict_bundle_object(UploadConflictBundleRequest {
            record: conflict,
            files,
            workspace_id: &self.options.workspace_id,
            device_id: &self.options.device_id,
            control_plane: self.control_plane,
            byte_store: self.byte_store,
            storage_key: self.options.storage_key,
            key_epoch: self.options.key_epoch,
        })?;
        if !set_conflict_bundle_object(conflict, pointer.clone())? {
            return Err(ConflictBundleError::OccurrenceSuperseded {
                conflict_id: conflict.id.clone(),
                occurrence_version: conflict.occurrence_version,
            }
            .into());
        }
        conflict.bundle_object = Some(pointer);
        Ok(())
    }

    fn enqueue_pending_conflict_occurrences(&self) -> Result<(), SyncRunnerError> {
        let operations = prepare_pending_conflict_occurrence_operations(
            &self.options.state_root,
            &self.options.workspace_id,
            &self.options.device_id,
            &self.options.generated_at,
            |record, files| self.ensure_conflict_bundle_object(record, files),
        )?;
        self.with_store(|store| {
            for operation in &operations {
                store.enqueue_sync_operation(operation)?;
            }
            Ok(())
        })
    }
}
