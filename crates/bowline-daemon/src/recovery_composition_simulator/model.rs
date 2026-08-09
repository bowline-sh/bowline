use std::collections::{BTreeMap, BTreeSet, VecDeque};

const INCREMENTAL_ROOT_BUDGET: usize = 32;
const LEGACY_TRANSFER_WORKERS: usize = 8;
const TRANSFER_WAVE_BUDGET: usize = 8;
const OBJECT_BATCH_SIZE: usize = 64;
const CONTRACTED_TRANSFER_WORKERS: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct SimMillis(u64);

impl SimMillis {
    const ZERO: Self = Self(0);

    pub(super) const fn from_millis(value: u64) -> Self {
        Self(value)
    }

    fn advance(&mut self, delta: u64) {
        self.0 = self.0.saturating_add(delta);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DaemonId {
    Source,
    Peer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Milestone {
    FilesystemMutation,
    NativeBoundary,
    AuthoritativeScan,
    RecoveryClosed,
    CallbackDelivered,
    ControlRejected,
    IncrementalWakeAdmitted,
    PublishStarted,
    PublishCompleted,
    ObserverAdvanced,
    PeerMaterialized,
    IntegrityMismatch,
    ReleaseCircuitOpened,
    SuccessorCampaignOpened,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct TraceEvent {
    pub(super) at: SimMillis,
    pub(super) daemon: DaemonId,
    pub(super) milestone: Milestone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SeamFailure {
    LateCallbackOmitted,
    RecoveryControlStarved,
    UnboundedDataWorkItem,
    MaterializationMisclassified,
    TransferWaveBudgetExceeded,
    ImmutableObjectOverwritten,
    ReleaseCircuitReset,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NativeDelivery {
    Immediate,
    DelayedUntilAfterClose,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EngineCycleKind {
    LocalPublish,
    RemoteApply,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EngineQueueEntry {
    IncrementalPaths { roots: usize },
    RecoveryControl,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SimFileSystem {
    files: BTreeMap<String, Vec<u8>>,
    revision: u64,
}

impl SimFileSystem {
    fn new() -> Self {
        Self {
            files: BTreeMap::new(),
            revision: 0,
        }
    }

    fn write(&mut self, path: &str, bytes: &[u8]) -> u64 {
        self.revision = self.revision.saturating_add(1);
        self.files.insert(path.to_owned(), bytes.to_vec());
        self.revision
    }

    fn snapshot(&self) -> FileSystemSnapshot {
        FileSystemSnapshot {
            files: self.files.clone(),
            revision: self.revision,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileSystemSnapshot {
    files: BTreeMap<String, Vec<u8>>,
    revision: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NativeCallback {
    filesystem_revision: u64,
    delivery: NativeDelivery,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FakeNativeCoverage {
    stream_epoch: u64,
    cursor: u64,
    callback_generation: u64,
    pending: VecDeque<NativeCallback>,
}

impl FakeNativeCoverage {
    fn new() -> Self {
        Self {
            stream_epoch: 1,
            cursor: 0,
            callback_generation: 0,
            pending: VecDeque::new(),
        }
    }

    fn observe(&mut self, filesystem_revision: u64, delivery: NativeDelivery) {
        self.cursor = self.cursor.saturating_add(1);
        self.pending.push_back(NativeCallback {
            filesystem_revision,
            delivery,
        });
    }

    fn pre_scan_boundary(&self) -> NativeBoundary {
        NativeBoundary {
            stream_epoch: self.stream_epoch,
            cursor: self.cursor,
            callback_generation: self.callback_generation,
        }
    }

    fn deliver_immediate(&mut self) -> Vec<NativeCallback> {
        self.deliver_matching(NativeDelivery::Immediate)
    }

    fn deliver_delayed(&mut self) -> Vec<NativeCallback> {
        self.deliver_matching(NativeDelivery::DelayedUntilAfterClose)
    }

    fn deliver_matching(&mut self, delivery: NativeDelivery) -> Vec<NativeCallback> {
        let mut delivered = Vec::new();
        let mut retained = VecDeque::new();
        while let Some(callback) = self.pending.pop_front() {
            if callback.delivery == delivery {
                self.callback_generation = self.callback_generation.saturating_add(1);
                delivered.push(callback);
            } else {
                retained.push_back(callback);
            }
        }
        self.pending = retained;
        delivered
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NativeBoundary {
    stream_epoch: u64,
    cursor: u64,
    callback_generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SimEngine {
    queue_capacity: usize,
    queue: VecDeque<EngineQueueEntry>,
    dirty: BTreeSet<String>,
    active_cycle: Option<EngineCycleKind>,
}

impl SimEngine {
    fn legacy() -> Self {
        Self {
            queue_capacity: 1,
            queue: VecDeque::new(),
            dirty: BTreeSet::new(),
            active_cycle: None,
        }
    }

    fn admit(&mut self, entry: EngineQueueEntry) -> bool {
        if self.queue.len() >= self.queue_capacity {
            return false;
        }
        self.queue.push_back(entry);
        true
    }

    fn classify_remote_materialization(&self) -> bool {
        self.active_cycle.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SimObjectStore {
    objects: BTreeMap<String, Vec<u8>>,
}

impl SimObjectStore {
    fn new() -> Self {
        Self {
            objects: BTreeMap::new(),
        }
    }

    fn legacy_put(&mut self, key: &str, bytes: &[u8]) -> bool {
        let overwritten = self.objects.get(key).is_some_and(|value| value != bytes);
        self.objects.insert(key.to_owned(), bytes.to_vec());
        overwritten
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SimObserver {
    visible_version: u64,
    lag_millis: u64,
}

impl SimObserver {
    fn new() -> Self {
        Self {
            visible_version: 0,
            lag_millis: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SimDaemon {
    filesystem: SimFileSystem,
    native: FakeNativeCoverage,
    engine: SimEngine,
    applied_version: u64,
}

impl SimDaemon {
    fn new() -> Self {
        Self {
            filesystem: SimFileSystem::new(),
            native: FakeNativeCoverage::new(),
            engine: SimEngine::legacy(),
            applied_version: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct OperationCounts {
    pub(super) tree_node_reads: usize,
    pub(super) tree_node_writes: usize,
    pub(super) reserve_calls: usize,
    pub(super) commit_calls: usize,
    pub(super) transfer_waves: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ContractedOperationCounts {
    pub(super) affected_tree_nodes: usize,
    pub(super) tree_node_reads: usize,
    pub(super) tree_node_writes: usize,
    pub(super) reserve_batches: usize,
    pub(super) commit_batches: usize,
    pub(super) file_put_waves: usize,
    pub(super) file_get_waves: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ContractedTransportProof {
    pub(super) operations: ContractedOperationCounts,
    pub(super) modeled_p95_millis: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ScenarioResult {
    pub(super) failure: SeamFailure,
    pub(super) trace: Vec<TraceEvent>,
    pub(super) operations: Option<OperationCounts>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReleaseObjective {
    failure_signature_count: u8,
    circuit_open: bool,
}

impl ReleaseObjective {
    fn new() -> Self {
        Self {
            failure_signature_count: 0,
            circuit_open: false,
        }
    }

    fn record_failure(&mut self) {
        self.failure_signature_count = self.failure_signature_count.saturating_add(1);
        self.circuit_open = self.failure_signature_count >= 2;
    }
}

pub(super) struct DeterministicCompositionSimulator {
    source: SimDaemon,
    peer: SimDaemon,
    store: SimObjectStore,
    observer: SimObserver,
    clock: SimMillis,
    trace: Vec<TraceEvent>,
}

impl DeterministicCompositionSimulator {
    pub(super) fn new() -> Self {
        Self {
            source: SimDaemon::new(),
            peer: SimDaemon::new(),
            store: SimObjectStore::new(),
            observer: SimObserver::new(),
            clock: SimMillis::ZERO,
            trace: Vec::new(),
        }
    }

    pub(super) fn reproduce_pre_scan_boundary_race(mut self) -> ScenarioResult {
        let first_revision = self.source.filesystem.write("src/a", b"first");
        self.record(DaemonId::Source, Milestone::FilesystemMutation);
        self.source
            .native
            .observe(first_revision, NativeDelivery::Immediate);
        let boundary = self.source.native.pre_scan_boundary();
        self.record(DaemonId::Source, Milestone::NativeBoundary);
        let _ = self.source.native.deliver_immediate();
        let scan = self.source.filesystem.snapshot();
        self.record(DaemonId::Source, Milestone::AuthoritativeScan);

        let late_revision = self.source.filesystem.write("src/b", b"late");
        self.record(DaemonId::Source, Milestone::FilesystemMutation);
        self.source
            .native
            .observe(late_revision, NativeDelivery::DelayedUntilAfterClose);
        self.record(DaemonId::Source, Milestone::RecoveryClosed);
        let delivered = self.source.native.deliver_delayed();
        self.record(DaemonId::Source, Milestone::CallbackDelivered);

        assert_eq!(boundary.callback_generation, 0);
        assert!(delivered.iter().any(|callback| {
            callback.filesystem_revision > scan.revision
                && callback.filesystem_revision == late_revision
        }));
        self.result(SeamFailure::LateCallbackOmitted, None)
    }

    pub(super) fn reproduce_shared_inbox_starvation(mut self) -> ScenarioResult {
        assert!(
            self.source
                .engine
                .admit(EngineQueueEntry::IncrementalPaths { roots: 32 })
        );
        self.record(DaemonId::Source, Milestone::IncrementalWakeAdmitted);
        assert!(!self.source.engine.admit(EngineQueueEntry::RecoveryControl));
        self.record(DaemonId::Source, Milestone::ControlRejected);
        self.result(SeamFailure::RecoveryControlStarved, None)
    }

    pub(super) fn reproduce_unbounded_work_item(mut self) -> ScenarioResult {
        let roots = 256;
        assert!(
            self.source
                .engine
                .admit(EngineQueueEntry::IncrementalPaths { roots })
        );
        self.record(DaemonId::Source, Milestone::IncrementalWakeAdmitted);
        assert!(roots > INCREMENTAL_ROOT_BUDGET);
        self.result(SeamFailure::UnboundedDataWorkItem, None)
    }

    pub(super) fn reproduce_materialization_inference(mut self) -> ScenarioResult {
        self.source.engine.active_cycle = Some(EngineCycleKind::LocalPublish);
        self.record(DaemonId::Source, Milestone::PublishStarted);
        assert!(self.source.engine.classify_remote_materialization());
        self.result(SeamFailure::MaterializationMisclassified, None)
    }

    pub(super) fn reproduce_transport_budget(mut self) -> ScenarioResult {
        let changed_paths: usize = 256;
        let tree_depth = 4;
        let waves = changed_paths.div_ceil(LEGACY_TRANSFER_WORKERS);
        let operations = OperationCounts {
            tree_node_reads: changed_paths * tree_depth,
            tree_node_writes: changed_paths * tree_depth,
            reserve_calls: changed_paths,
            commit_calls: changed_paths,
            transfer_waves: waves,
        };
        self.record(DaemonId::Source, Milestone::PublishStarted);
        self.clock.advance(u64::try_from(waves).unwrap_or(u64::MAX));
        self.record(DaemonId::Source, Milestone::PublishCompleted);
        assert!(operations.transfer_waves > TRANSFER_WAVE_BUDGET);
        self.result(SeamFailure::TransferWaveBudgetExceeded, Some(operations))
    }

    pub(super) fn prove_contracted_transport_budget(&self) -> ContractedTransportProof {
        let changed_files = 256_usize;
        // Thirty-two changed leaf directories plus the shared root. Grouped
        // mutation opens and rewrites each one once rather than once per path.
        let affected_tree_nodes = 33_usize;
        let file_batches = changed_files.div_ceil(OBJECT_BATCH_SIZE);
        let manifest_batches = affected_tree_nodes.div_ceil(OBJECT_BATCH_SIZE);
        let transfer_waves = changed_files.div_ceil(CONTRACTED_TRANSFER_WORKERS);
        let operations = ContractedOperationCounts {
            affected_tree_nodes,
            tree_node_reads: affected_tree_nodes,
            tree_node_writes: affected_tree_nodes,
            reserve_batches: file_batches + manifest_batches,
            commit_batches: file_batches + manifest_batches,
            file_put_waves: transfer_waves,
            file_get_waves: transfer_waves,
        };

        // Deterministic p95 component ceilings in milliseconds. These are
        // deliberately pessimistic operation budgets, not a wall-clock sleep:
        // scan 1.5s; producer metadata 1.2s; file PUT waves 4.8s; manifest-node
        // waves 0.8s; CAS 0.25s; observer 0.8s; peer GET waves 4.8s;
        // verification/apply 1.8s; exact barrier 0.45s.
        let modeled_p95_millis = 1_500 + 1_200 + 4_800 + 800 + 250 + 800 + 4_800 + 1_800 + 450;
        ContractedTransportProof {
            operations,
            modeled_p95_millis,
        }
    }

    pub(super) fn reproduce_integrity_overwrite(mut self) -> ScenarioResult {
        assert!(!self.store.legacy_put("b_identity", b"original"));
        assert!(self.store.legacy_put("b_identity", b"replacement"));
        self.record(DaemonId::Source, Milestone::IntegrityMismatch);
        self.result(SeamFailure::ImmutableObjectOverwritten, None)
    }

    pub(super) fn reproduce_release_circuit_reset(mut self) -> ScenarioResult {
        let mut first_campaign = ReleaseObjective::new();
        first_campaign.record_failure();
        first_campaign.record_failure();
        assert!(first_campaign.circuit_open);
        self.record(DaemonId::Source, Milestone::ReleaseCircuitOpened);

        let successor_campaign = ReleaseObjective::new();
        assert!(!successor_campaign.circuit_open);
        self.record(DaemonId::Source, Milestone::SuccessorCampaignOpened);
        self.result(SeamFailure::ReleaseCircuitReset, None)
    }

    pub(super) fn observer_and_peer_are_explicit_authorities(&mut self) {
        self.observer.visible_version = 1;
        self.observer.lag_millis = 25;
        self.peer.engine.active_cycle = Some(EngineCycleKind::RemoteApply);
        self.peer.applied_version = 1;
        self.clock.advance(self.observer.lag_millis);
        self.record(DaemonId::Source, Milestone::ObserverAdvanced);
        self.record(DaemonId::Peer, Milestone::PeerMaterialized);
        self.peer.engine.active_cycle = None;
    }

    pub(super) fn trace(&self) -> &[TraceEvent] {
        &self.trace
    }

    fn record(&mut self, daemon: DaemonId, milestone: Milestone) {
        self.trace.push(TraceEvent {
            at: self.clock,
            daemon,
            milestone,
        });
        self.clock.advance(1);
    }

    fn result(self, failure: SeamFailure, operations: Option<OperationCounts>) -> ScenarioResult {
        ScenarioResult {
            failure,
            trace: self.trace,
            operations,
        }
    }
}
