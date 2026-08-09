//! The engine's in-memory state: the read-only snapshot the daemon publishes,
//! the phase/degradation vocabulary it reports, and the transition signature
//! that decides when a snapshot revision moves.
//!
//! Split from `mod.rs`, which owns the driver's scheduling and cycle logic.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use bowline_core::ids::WorkspaceId;

use super::{
    Clock, EngineConvergenceBarrierId, EngineEndpointGeneration, EngineIo, FullScanReason,
    ManifestEngine, MaterializationRevision, RefObservation, RemoteObjects, RemoteRef, RootFault,
    UnsyncableRecord, WorkspacePath,
};

/// Coarse engine phase for the snapshot. Momentary; the durable facts are the
/// ref/manifest/intents fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnginePhase {
    Starting,
    Idle,
    Syncing,
    BackingOff,
    Stalled,
    Stopped,
}

/// The engine's health, distinct from its phase. Nominal is healthy; the rest
/// are non-fatal (a lost CAS is never here — it is normal). `RootUnavailable`
/// clears only when the root sentinel is satisfied again, and
/// `MassDeletionBlocked` only when a push actually publishes — clearing either
/// one on a timer would mean destroying user data on a guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Degradation {
    Nominal,
    FullScanRequired(FullScanReason),
    OfflineRetrying {
        attempt: u32,
    },
    IntegrityStalled,
    /// The workspace root is gone, is not a directory, or is not the workspace
    /// this device committed its ancestor against. NOTHING is published while
    /// this holds.
    RootUnavailable(RootFault),
    /// A push would have removed an implausible share of the workspace and was
    /// refused. Cleared only by [`ManifestEngine::confirm_mass_deletion`]. The
    /// counts are the arithmetic the guard performed; the paths themselves live
    /// in [`EngineSnapshot::refused_removals`], which no `Copy` state may hold.
    /// The ceiling is not stored beside them: it is exactly
    /// [`mass_deletion_threshold`] of `entries`, and a stored copy could drift.
    MassDeletionBlocked {
        removals: usize,
        entries: usize,
    },
    /// The engine's own store could not be read, so the reported engine state is
    /// unknown rather than the last good value.
    StoreUnavailable,
}

/// Exact identity of the authoritative/applied workspace ref.
///
/// Genesis is a real state, not a missing value. A head always carries both
/// components of its identity: the hosted monotonic version and the opaque
/// physical [`super::ManifestKey`] of the sealed manifest bytes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum EngineRef {
    #[default]
    Genesis,
    Head(RefObservation),
}

impl EngineRef {
    #[must_use]
    pub fn from_observation(observation: Option<RefObservation>) -> Self {
        observation.map_or(Self::Genesis, Self::Head)
    }
}

/// Opaque identity of the daemon process incarnation issuing a receipt.
///
/// Barrier and endpoint counters restart after a daemon restart; binding them
/// to this separately minted identity prevents cross-process aliasing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineProcessIdentity {
    boot_id: String,
    session_id: String,
    started_at: String,
}

impl EngineProcessIdentity {
    #[must_use]
    pub fn current() -> Self {
        use std::sync::OnceLock;
        use time::{OffsetDateTime, format_description::well_known::Rfc3339};

        static PROCESS_IDENTITY: OnceLock<EngineProcessIdentity> = OnceLock::new();
        PROCESS_IDENTITY
            .get_or_init(|| {
                let started = OffsetDateTime::now_utc();
                let started_at = started
                    .format(&Rfc3339)
                    .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string());
                let nonce = format!("{}:{}", std::process::id(), started.unix_timestamp_nanos());
                Self::from_parts(
                    opaque_process_identity("boot", nonce.as_bytes()),
                    opaque_process_identity("session", nonce.as_bytes()),
                    started_at,
                )
            })
            .clone()
    }

    #[must_use]
    pub fn from_parts(boot_id: String, session_id: String, started_at: String) -> Self {
        Self {
            boot_id,
            session_id,
            started_at,
        }
    }

    #[must_use]
    pub fn boot_id(&self) -> &str {
        &self.boot_id
    }

    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    #[must_use]
    pub fn started_at(&self) -> &str {
        &self.started_at
    }
}

fn opaque_process_identity(label: &str, nonce: &[u8]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"bowline.engine-process-identity.v2\0");
    hasher.update(label.as_bytes());
    hasher.update(&[0]);
    hasher.update(nonce);
    format!("{label}_{}", hasher.finalize().to_hex())
}

/// A read-only snapshot of engine state: in-memory facts only, no JSON method,
/// no queue fiction. `revision` bumps ONLY on a state transition, so a status
/// consumer that polls an idle engine sees a stable revision (Plan 109 Step 7 /
/// review Change 14).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineSnapshot {
    pub revision: u64,
    pub phase: EnginePhase,
    pub observed_ref: Option<RefObservation>,
    pub applied_ref: EngineRef,
    pub materialization_revision: MaterializationRevision,
    pub pending_intents: usize,
    /// Dirty paths queued for the next push. Exposed so the daemon status
    /// projection can report a truthful outbound queue count (Plan 111 Step 1c)
    /// without a second `dirty_paths()` round-trip.
    pub dirty: usize,
    /// Attributed work retained for project-scoped status. The workspace-wide
    /// counters above remain the canonical global projection.
    pub dirty_paths: Arc<BTreeSet<WorkspacePath>>,
    pub dirty_subtree_paths: Arc<BTreeSet<WorkspacePath>>,
    pub pending_intent_paths: Arc<BTreeSet<WorkspacePath>>,
    /// A pending full scan makes every project scope conservatively
    /// non-ready because the attributed sets may still be incomplete.
    pub scan_required: bool,
    /// A remote wake has scheduled a pull whose paths are not known yet, or a
    /// cycle is currently able to discover such paths.
    pub unattributed_pull_pending: bool,
    pub cycle_active: bool,
    pub last_success_at: Option<u64>,
    pub degradation: Degradation,
    /// Paths this device cannot sync, with the reason and remedy for each. A
    /// non-empty set is an actionable status item, never a silent omission from
    /// a product whose invariant is "Everything Syncs".
    pub unsyncable: Arc<BTreeMap<WorkspacePath, UnsyncableRecord>>,
    /// Exactly the removals the deletion breaker refused, so an operator can
    /// read what a confirmation would publish before authorising it. Non-empty
    /// if and only if `degradation` is [`Degradation::MassDeletionBlocked`].
    pub refused_removals: Arc<BTreeSet<WorkspacePath>>,
}

/// Proof that one engine endpoint reached an exact, durable convergence
/// frontier for one internal barrier request.
///
/// Fields are private so callers cannot manufacture a receipt from a merely
/// idle-looking snapshot. [`EngineSnapshot::convergence_receipt`] is the sole
/// constructor and rejects every pending or hidden engine condition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineConvergenceReceipt {
    process_identity: EngineProcessIdentity,
    workspace_identity: WorkspaceId,
    endpoint_generation: EngineEndpointGeneration,
    barrier_id: EngineConvergenceBarrierId,
    engine_revision: u64,
    observed_ref: EngineRef,
    applied_ref: EngineRef,
    materialization_revision: MaterializationRevision,
}

impl EngineConvergenceReceipt {
    #[must_use]
    pub const fn process_identity(&self) -> &EngineProcessIdentity {
        &self.process_identity
    }

    #[must_use]
    pub const fn workspace_identity(&self) -> &WorkspaceId {
        &self.workspace_identity
    }

    #[must_use]
    pub const fn endpoint_generation(&self) -> EngineEndpointGeneration {
        self.endpoint_generation
    }

    #[must_use]
    pub const fn barrier_id(&self) -> EngineConvergenceBarrierId {
        self.barrier_id
    }

    #[must_use]
    pub const fn engine_revision(&self) -> u64 {
        self.engine_revision
    }

    #[must_use]
    pub const fn observed_ref(&self) -> &EngineRef {
        &self.observed_ref
    }

    #[must_use]
    pub const fn applied_ref(&self) -> &EngineRef {
        &self.applied_ref
    }

    #[must_use]
    pub const fn materialization_revision(&self) -> MaterializationRevision {
        self.materialization_revision
    }
}

impl EngineSnapshot {
    #[must_use]
    pub fn exact_observed_ref(&self) -> EngineRef {
        EngineRef::from_observation(self.observed_ref.clone())
    }

    /// Whether every engine-owned condition needed by an exact boundary is
    /// settled at this snapshot.
    #[must_use]
    pub fn is_exactly_converged(&self) -> bool {
        let observed_ref = self.exact_observed_ref();
        self.is_work_drained()
            && self.degradation == Degradation::Nominal
            && observed_ref == self.applied_ref
            && self.unsyncable.is_empty()
            && self.refused_removals.is_empty()
    }

    /// Whether the engine has drained every actionable unit of work.
    ///
    /// This is deliberately weaker than exact convergence: a terminally
    /// represented workspace fact such as an unsyncable object can be fully
    /// drained without being safe to authorize as `Ready`. Internal simulation
    /// harnesses use this distinction to stop driving a stable blocked state;
    /// exact barriers must continue to use [`Self::is_exactly_converged`].
    #[must_use]
    pub(crate) fn is_work_drained(&self) -> bool {
        self.phase == EnginePhase::Idle
            && self.pending_intents == 0
            && self.dirty == 0
            && self.dirty_paths.is_empty()
            && self.dirty_subtree_paths.is_empty()
            && self.pending_intent_paths.is_empty()
            && !self.scan_required
            && !self.unattributed_pull_pending
            && !self.cycle_active
    }

    /// Build a typed receipt only from a fully settled engine frontier.
    #[must_use]
    pub(crate) fn convergence_receipt(
        &self,
        process_identity: EngineProcessIdentity,
        workspace_identity: WorkspaceId,
        endpoint_generation: EngineEndpointGeneration,
        barrier_id: EngineConvergenceBarrierId,
    ) -> Option<EngineConvergenceReceipt> {
        if !self.is_exactly_converged() {
            return None;
        }
        let observed_ref = self.exact_observed_ref();
        Some(EngineConvergenceReceipt {
            process_identity,
            workspace_identity,
            endpoint_generation,
            barrier_id,
            engine_revision: self.revision,
            observed_ref,
            applied_ref: self.applied_ref.clone(),
            materialization_revision: self.materialization_revision,
        })
    }
}

impl Default for EngineSnapshot {
    fn default() -> Self {
        Self {
            revision: 0,
            phase: EnginePhase::Starting,
            observed_ref: None,
            applied_ref: EngineRef::Genesis,
            materialization_revision: MaterializationRevision::INITIAL,
            pending_intents: 0,
            dirty: 0,
            dirty_paths: Arc::new(BTreeSet::new()),
            dirty_subtree_paths: Arc::new(BTreeSet::new()),
            pending_intent_paths: Arc::new(BTreeSet::new()),
            scan_required: false,
            unattributed_pull_pending: false,
            cycle_active: false,
            last_success_at: None,
            degradation: Degradation::Nominal,
            unsyncable: Arc::new(BTreeMap::new()),
            refused_removals: Arc::new(BTreeSet::new()),
        }
    }
}

/// The transition signature: the fields whose change is a state transition (and
/// so bumps `revision`). Deliberately excludes wall-clock time and scheduling
/// deadlines, so an idle poll never advances the revision.
#[derive(Clone, PartialEq, Eq)]
pub(super) struct StateSig {
    phase: EnginePhase,
    degradation: Degradation,
    applied_ref: EngineRef,
    materialization_revision: MaterializationRevision,
    observed_ref: EngineRef,
    pending_intents: usize,
    pending_intent_paths: Arc<BTreeSet<WorkspacePath>>,
    dirty_paths: Arc<BTreeSet<WorkspacePath>>,
    dirty_subtree_paths: Arc<BTreeSet<WorkspacePath>>,
    scan_required: bool,
    unattributed_pull_pending: bool,
    cycle_active: bool,
    unsyncable: Arc<BTreeMap<WorkspacePath, UnsyncableRecord>>,
    refused_removals: Arc<BTreeSet<WorkspacePath>>,
}

impl ManifestEngine {
    /// The one writer of `degradation`.
    ///
    /// Every transition out of [`Degradation::MassDeletionBlocked`] must drop the
    /// refused removal set with it: a snapshot that still lists paths nothing is
    /// refusing would offer an operator a confirmation for a push that no longer
    /// exists. Routing every write through here is what makes that invariant a
    /// property of the type rather than of remembering.
    pub(super) fn set_degradation(&mut self, degradation: Degradation) {
        if !matches!(degradation, Degradation::MassDeletionBlocked { .. })
            && !self.refused_removals.is_empty()
        {
            self.refused_removals = Arc::new(BTreeSet::new());
        }
        self.degradation = degradation;
    }

    /// Record a refused removal batch: the counts status reports, and the paths
    /// the operator surface lists.
    pub(super) fn block_mass_deletion(
        &mut self,
        removals: BTreeSet<WorkspacePath>,
        entries: usize,
    ) {
        self.set_degradation(Degradation::MassDeletionBlocked {
            removals: removals.len(),
            entries,
        });
        self.refused_removals = Arc::new(removals);
    }

    pub(super) fn idle(&self) -> bool {
        self.dirty.is_empty()
            && self.dirty_subtrees.is_empty()
            && !self.scan_required
            && !self.pull_needed
    }

    /// Whether a successful cycle clears this degradation. `RootUnavailable` is
    /// absent on purpose: only the root sentinel may declare the root trustworthy
    /// again, and it does so with a full rescan. `StoreUnavailable` is likewise
    /// owned by the store read in `refresh_and_bump`.
    pub(super) fn degradation_is_transient(&self) -> bool {
        matches!(
            self.degradation,
            Degradation::OfflineRetrying { .. }
                | Degradation::FullScanRequired(_)
                | Degradation::IntegrityStalled
                // A push that published is proof the batch is no longer refused,
                // whether the user confirmed it or restored the missing files.
                | Degradation::MassDeletionBlocked { .. }
        )
    }

    /// Re-read the durable engine facts the snapshot reports.
    ///
    /// A read failure MUST NOT be swallowed: replaying the last good values makes
    /// status report a healthy, unchanged engine while its database is failing,
    /// and the unchanged signature means the revision does not even move, so no
    /// consumer learns anything is wrong. Record it as a degradation instead.
    pub(super) fn refresh_and_bump<O: RemoteObjects, R: RemoteRef, C: Clock>(
        &mut self,
        _io: &EngineIo<'_, O, R, C>,
    ) {
        self.refresh_durable_state();
        self.bump_revision_if_changed();
    }

    pub(super) fn refresh_durable_state(&mut self) {
        let mut store_failed = false;
        match self.store.engine_state() {
            Ok(state) => match state.applied_ref() {
                Ok(applied_ref) => {
                    self.applied_ref = applied_ref;
                    self.materialization_revision = state.materialization_revision;
                }
                Err(_) => store_failed = true,
            },
            Err(_) => store_failed = true,
        }
        match self.store.pending_intents() {
            Ok(intents) => {
                self.pending_intents = intents.len();
                self.pending_intent_paths = Arc::new(
                    intents
                        .into_iter()
                        .map(|intent| intent.path)
                        .collect::<BTreeSet<_>>(),
                );
            }
            Err(_) => store_failed = true,
        }
        match self.store.unsyncable() {
            Ok(entries) => self.unsyncable = Arc::new(entries),
            Err(_) => store_failed = true,
        }
        if store_failed {
            self.set_degradation(Degradation::StoreUnavailable);
        } else if self.degradation == Degradation::StoreUnavailable {
            self.set_degradation(Degradation::Nominal);
        }
    }

    pub(super) fn bump_revision_if_changed(&mut self) {
        let sig = StateSig {
            phase: self.phase,
            degradation: self.degradation,
            applied_ref: self.applied_ref.clone(),
            materialization_revision: self.materialization_revision,
            observed_ref: EngineRef::from_observation(self.head_ref.clone()),
            pending_intents: self.pending_intents,
            pending_intent_paths: self.pending_intent_paths.clone(),
            dirty_paths: self.dirty.clone(),
            dirty_subtree_paths: self.dirty_subtrees.clone(),
            scan_required: self.scan_required,
            unattributed_pull_pending: self.unattributed_pull_pending,
            cycle_active: self.cycle_active,
            unsyncable: self.unsyncable.clone(),
            refused_removals: self.refused_removals.clone(),
        };
        if self.last_sig.as_ref() != Some(&sig) {
            self.revision += 1;
            self.last_sig = Some(sig);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::manifest_engine::{
        EngineConvergenceBarrierId, EngineEndpointGeneration, ManifestKey, UnsyncableReason,
    };

    fn head(version: u64, key: &str) -> RefObservation {
        RefObservation {
            version,
            manifest_key: ManifestKey::new(key),
        }
    }

    fn converged_snapshot() -> EngineSnapshot {
        let observed = head(7, "manifest-a");
        EngineSnapshot {
            revision: 19,
            phase: EnginePhase::Idle,
            observed_ref: Some(observed.clone()),
            applied_ref: EngineRef::Head(observed),
            materialization_revision: MaterializationRevision::from_stored(11),
            pending_intents: 0,
            dirty: 0,
            dirty_paths: Arc::new(BTreeSet::new()),
            dirty_subtree_paths: Arc::new(BTreeSet::new()),
            pending_intent_paths: Arc::new(BTreeSet::new()),
            scan_required: false,
            unattributed_pull_pending: false,
            cycle_active: false,
            last_success_at: Some(1),
            degradation: Degradation::Nominal,
            unsyncable: Arc::new(BTreeMap::new()),
            refused_removals: Arc::new(BTreeSet::new()),
        }
    }

    fn receipt_identities() -> (EngineProcessIdentity, WorkspaceId) {
        (
            EngineProcessIdentity::from_parts(
                "boot_test".to_string(),
                "session_test".to_string(),
                "2026-08-03T00:00:00Z".to_string(),
            ),
            WorkspaceId::new("ws-test"),
        )
    }

    #[test]
    fn exact_receipt_binds_endpoint_barrier_refs_and_materialization() {
        let (process_identity, workspace_identity) = receipt_identities();
        let receipt = converged_snapshot()
            .convergence_receipt(
                process_identity.clone(),
                workspace_identity.clone(),
                EngineEndpointGeneration(23),
                EngineConvergenceBarrierId(29),
            )
            .expect("exact frontier produces a receipt");

        assert_eq!(receipt.process_identity(), &process_identity);
        assert_eq!(receipt.workspace_identity(), &workspace_identity);
        assert_eq!(receipt.endpoint_generation(), EngineEndpointGeneration(23));
        assert_eq!(receipt.barrier_id(), EngineConvergenceBarrierId(29));
        assert_eq!(receipt.engine_revision(), 19);
        assert_eq!(receipt.observed_ref(), receipt.applied_ref());
        assert_eq!(receipt.materialization_revision().get(), 11);
    }

    #[test]
    fn genesis_is_an_explicit_exact_frontier() {
        let mut snapshot = converged_snapshot();
        snapshot.observed_ref = None;
        snapshot.applied_ref = EngineRef::Genesis;
        let (process_identity, workspace_identity) = receipt_identities();
        assert!(
            snapshot
                .convergence_receipt(
                    process_identity,
                    workspace_identity,
                    EngineEndpointGeneration(1),
                    EngineConvergenceBarrierId(1),
                )
                .is_some()
        );
    }

    #[test]
    fn same_version_different_key_withholds_convergence() {
        let mut snapshot = converged_snapshot();
        snapshot.observed_ref = Some(head(7, "manifest-b"));
        assert!(!snapshot.is_exactly_converged());
    }

    #[test]
    fn drained_work_is_not_exact_authorization_when_a_blocker_remains() {
        let mut snapshot = converged_snapshot();
        snapshot.unsyncable = Arc::new(BTreeMap::from([(
            WorkspacePath::new("queue.fifo"),
            UnsyncableRecord::new(UnsyncableReason::UnsupportedKind, None, 1),
        )]));

        assert!(snapshot.is_work_drained());
        assert!(!snapshot.is_exactly_converged());
        let (process_identity, workspace_identity) = receipt_identities();
        assert!(
            snapshot
                .convergence_receipt(
                    process_identity,
                    workspace_identity,
                    EngineEndpointGeneration(1),
                    EngineConvergenceBarrierId(1),
                )
                .is_none()
        );
    }

    #[test]
    fn every_hidden_or_pending_engine_fact_withholds_convergence() {
        let base = converged_snapshot();
        let mut blocked = Vec::new();

        let mut snapshot = base.clone();
        snapshot.pending_intents = 1;
        blocked.push(snapshot);
        let mut snapshot = base.clone();
        snapshot.dirty = 1;
        snapshot.dirty_paths = Arc::new(BTreeSet::from([WorkspacePath::new("dirty")]));
        blocked.push(snapshot);
        let mut snapshot = base.clone();
        snapshot.scan_required = true;
        blocked.push(snapshot);
        let mut snapshot = base.clone();
        snapshot.unattributed_pull_pending = true;
        blocked.push(snapshot);
        let mut snapshot = base.clone();
        snapshot.cycle_active = true;
        blocked.push(snapshot);
        let mut snapshot = base.clone();
        snapshot.degradation = Degradation::IntegrityStalled;
        blocked.push(snapshot);
        let mut snapshot = base.clone();
        snapshot.unsyncable = Arc::new(BTreeMap::from([(
            WorkspacePath::new("opaque"),
            UnsyncableRecord::new(UnsyncableReason::ReadFailed, None, 1),
        )]));
        blocked.push(snapshot);
        let mut snapshot = base;
        snapshot.refused_removals = Arc::new(BTreeSet::from([WorkspacePath::new("refused")]));
        blocked.push(snapshot);

        for snapshot in blocked {
            assert!(!snapshot.is_exactly_converged());
        }
    }
}
