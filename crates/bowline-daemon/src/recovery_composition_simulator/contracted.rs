use std::collections::{BTreeMap, BTreeSet};

const INGRESS_ROOT_CAPACITY: usize = 256;
const PUBLIC_BARRIER_CAPACITY: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RecoveryCause {
    Startup,
    NativeLoss,
    IngressDetailCollapsed,
    RootReplaced,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum ActionRequirement {
    Authentication,
    SignerTrust,
    Integrity,
    MassDeletion,
    EngineDisconnected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BarrierFailure {
    Recovering,
    Syncing,
    ObserverUnavailable,
    RefMismatch,
    Blocked(ActionRequirement),
    ResourceExhausted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ScanFailure {
    ProducerActive,
    EngineBusy,
    UnstableFiles,
    StaleAttempt,
    RescanRequired,
    NoIncident,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CrashPhase {
    Rearming,
    AwaitingCoverage,
    Scanning,
    AwaitingSeal,
    Closing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ExactReceipt {
    pub(super) revision: u64,
    pub(super) remote_head: u64,
    pub(super) incident_id: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ScanAttempt {
    incident_id: u64,
    attempt_id: u64,
    filesystem_revision: u64,
    activity_watermark: u64,
    loss_generation: u64,
    stream_epoch: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RecoveryIncident {
    id: u64,
    cause: RecoveryCause,
    attempts: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NativeAuthority {
    stream_epoch: u64,
    loss_generation: u64,
    activity_watermark: u64,
    callback_generation: u64,
    live: bool,
}

impl NativeAuthority {
    fn new() -> Self {
        Self {
            stream_epoch: 1,
            loss_generation: 0,
            activity_watermark: 0,
            callback_generation: 0,
            live: true,
        }
    }

    fn activity(&mut self) {
        self.activity_watermark = self.activity_watermark.saturating_add(1);
        self.callback_generation = self.callback_generation.saturating_add(1);
    }

    fn loss(&mut self) {
        self.activity();
        self.loss_generation = self.loss_generation.saturating_add(1);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkspaceModel {
    filesystem: BTreeMap<String, u64>,
    filesystem_revision: u64,
    native: NativeAuthority,
    recovery: Option<RecoveryIncident>,
    last_closed_incident: u64,
    current_attempt: Option<ScanAttempt>,
    engine_paused: bool,
    engine_cycle_active: bool,
    producer_active: bool,
    unstable_files: bool,
    accumulator: BTreeSet<String>,
    scan_required: bool,
    dirty: BTreeSet<String>,
    public_barriers: usize,
    observer_live: bool,
    observed_ref: u64,
    applied_ref: u64,
    durable_revision: u64,
    convergence_revision: u64,
    cas_uncertain: bool,
    blocked: BTreeSet<ActionRequirement>,
    running: bool,
}

impl WorkspaceModel {
    fn new() -> Self {
        Self {
            filesystem: BTreeMap::new(),
            filesystem_revision: 0,
            native: NativeAuthority::new(),
            recovery: None,
            last_closed_incident: 0,
            current_attempt: None,
            engine_paused: false,
            engine_cycle_active: false,
            producer_active: false,
            unstable_files: false,
            accumulator: BTreeSet::new(),
            scan_required: false,
            dirty: BTreeSet::new(),
            public_barriers: 0,
            observer_live: true,
            observed_ref: 0,
            applied_ref: 0,
            durable_revision: 0,
            convergence_revision: 0,
            cas_uncertain: false,
            blocked: BTreeSet::new(),
            running: true,
        }
    }

    fn write(&mut self, path: String) {
        self.filesystem_revision = self.filesystem_revision.saturating_add(1);
        self.filesystem
            .insert(path.clone(), self.filesystem_revision);
        self.observe_path(path);
    }

    fn observe_path(&mut self, path: String) {
        self.native.activity();
        if self.accumulator.len() >= INGRESS_ROOT_CAPACITY {
            self.scan_required = true;
            self.open_incident(RecoveryCause::IngressDetailCollapsed);
        } else {
            self.accumulator.insert(path);
        }
    }

    fn observe_loss(&mut self, cause: RecoveryCause) {
        self.native.loss();
        self.scan_required = true;
        self.open_incident(cause);
    }

    fn open_incident(&mut self, cause: RecoveryCause) {
        if self.recovery.is_none() {
            let id = self.last_closed_incident.saturating_add(1);
            self.recovery = Some(RecoveryIncident {
                id,
                cause,
                attempts: 0,
            });
            self.convergence_revision = self.convergence_revision.saturating_add(1);
        }
    }

    fn begin_scan(&mut self) -> Result<ScanAttempt, ScanFailure> {
        if self.producer_active {
            return Err(ScanFailure::ProducerActive);
        }
        if self.engine_cycle_active {
            return Err(ScanFailure::EngineBusy);
        }
        if self.unstable_files {
            return Err(ScanFailure::UnstableFiles);
        }
        let incident = self.recovery.as_mut().ok_or(ScanFailure::NoIncident)?;
        incident.attempts = incident.attempts.saturating_add(1);
        self.engine_paused = true;
        self.accumulator.clear();
        self.dirty.extend(self.filesystem.keys().cloned());
        let attempt = ScanAttempt {
            incident_id: incident.id,
            attempt_id: incident.attempts,
            filesystem_revision: self.filesystem_revision,
            activity_watermark: self.native.activity_watermark,
            loss_generation: self.native.loss_generation,
            stream_epoch: self.native.stream_epoch,
        };
        self.current_attempt = Some(attempt);
        Ok(attempt)
    }

    fn seal_and_close(&mut self, attempt: ScanAttempt) -> Result<u64, ScanFailure> {
        if self.current_attempt != Some(attempt) {
            return Err(ScanFailure::StaleAttempt);
        }
        let invalidated = !self.native.live
            || self.native.stream_epoch != attempt.stream_epoch
            || self.native.loss_generation != attempt.loss_generation
            || self.native.activity_watermark != attempt.activity_watermark
            || self.filesystem_revision != attempt.filesystem_revision;
        if invalidated {
            self.current_attempt = None;
            self.engine_paused = false;
            return Err(ScanFailure::RescanRequired);
        }
        let incident = self.recovery.take().ok_or(ScanFailure::NoIncident)?;
        if incident.id != attempt.incident_id {
            return Err(ScanFailure::StaleAttempt);
        }
        self.last_closed_incident = incident.id;
        self.scan_required = false;
        self.current_attempt = None;
        self.engine_paused = false;
        self.convergence_revision = self.convergence_revision.saturating_add(1);
        Ok(incident.id)
    }

    fn register_barrier(&mut self) -> Result<(), BarrierFailure> {
        if self.public_barriers == PUBLIC_BARRIER_CAPACITY {
            return Err(BarrierFailure::ResourceExhausted);
        }
        self.public_barriers += 1;
        Ok(())
    }

    fn barrier(&self, remote_head: u64) -> Result<ExactReceipt, BarrierFailure> {
        if let Some(blocked) = self.blocked.iter().next().copied() {
            return Err(BarrierFailure::Blocked(blocked));
        }
        if self.recovery.is_some() || self.engine_paused {
            return Err(BarrierFailure::Recovering);
        }
        if !self.observer_live {
            return Err(BarrierFailure::ObserverUnavailable);
        }
        if !self.dirty.is_empty() || self.engine_cycle_active || self.cas_uncertain {
            return Err(BarrierFailure::Syncing);
        }
        if self.observed_ref != remote_head || self.applied_ref != remote_head {
            return Err(BarrierFailure::RefMismatch);
        }
        Ok(ExactReceipt {
            revision: self.convergence_revision,
            remote_head,
            incident_id: self.last_closed_incident,
        })
    }

    fn crash_and_restart(&mut self, _phase: CrashPhase) {
        self.running = false;
        self.engine_paused = false;
        self.engine_cycle_active = false;
        self.current_attempt = None;
        self.recovery = None;
        self.native = NativeAuthority::new();
        self.running = true;
        self.open_incident(RecoveryCause::Startup);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ContractedProductModel {
    source: WorkspaceModel,
    peer: WorkspaceModel,
    remote_files: BTreeMap<String, u64>,
    remote_head: u64,
}

impl ContractedProductModel {
    pub(super) fn new() -> Self {
        Self {
            source: WorkspaceModel::new(),
            peer: WorkspaceModel::new(),
            remote_files: BTreeMap::new(),
            remote_head: 0,
        }
    }

    pub(super) fn write_source(&mut self, path: impl Into<String>) {
        self.source.write(path.into());
    }

    pub(super) fn write_peer(&mut self, path: impl Into<String>) {
        self.peer.write(path.into());
    }

    pub(super) fn source_loss(&mut self, cause: RecoveryCause) {
        self.source.observe_loss(cause);
    }

    pub(super) fn source_scan(&mut self) -> Result<ScanAttempt, ScanFailure> {
        self.source.begin_scan()
    }

    pub(super) fn close_source_scan(&mut self, attempt: ScanAttempt) -> Result<u64, ScanFailure> {
        self.source.seal_and_close(attempt)
    }

    pub(super) fn peer_scan(&mut self) -> Result<ScanAttempt, ScanFailure> {
        self.peer.begin_scan()
    }

    pub(super) fn close_peer_scan(&mut self, attempt: ScanAttempt) -> Result<u64, ScanFailure> {
        self.peer.seal_and_close(attempt)
    }

    pub(super) fn begin_dense_source_producer(&mut self) {
        self.source.producer_active = true;
    }

    pub(super) fn end_dense_source_producer(&mut self) {
        self.source.producer_active = false;
    }

    pub(super) fn publish_source(&mut self) {
        assert!(self.source.recovery.is_none());
        assert!(!self.source.engine_paused);
        self.source.engine_cycle_active = true;
        self.remote_head = self.remote_head.saturating_add(1);
        self.remote_files = self.source.filesystem.clone();
        self.source.applied_ref = self.remote_head;
        self.source.durable_revision = self.source.filesystem_revision;
        self.source.dirty.clear();
        self.source.engine_cycle_active = false;
        self.source.convergence_revision = self.source.convergence_revision.saturating_add(1);
    }

    pub(super) fn advance_source_observer(&mut self) {
        self.source.observed_ref = self.remote_head;
    }

    pub(super) fn apply_peer(&mut self, watcher_paths: usize) {
        self.peer.engine_cycle_active = true;
        self.peer.filesystem = self.remote_files.clone();
        self.peer.filesystem_revision = self.peer.filesystem_revision.saturating_add(1);
        for index in 0..watcher_paths {
            self.peer
                .observe_path(format!("remote/materialized/{index}"));
        }
        self.peer.applied_ref = self.remote_head;
        self.peer.observed_ref = self.remote_head;
        self.peer.durable_revision = self.peer.filesystem_revision;
        self.peer.dirty.clear();
        self.peer.engine_cycle_active = false;
    }

    pub(super) fn source_barrier(&self) -> Result<ExactReceipt, BarrierFailure> {
        self.source.barrier(self.remote_head)
    }

    pub(super) fn peer_barrier(&self) -> Result<ExactReceipt, BarrierFailure> {
        self.peer.barrier(self.remote_head)
    }

    pub(super) fn register_source_barrier(&mut self) -> Result<(), BarrierFailure> {
        self.source.register_barrier()
    }

    pub(super) fn source_recovery_open(&self) -> bool {
        self.source.recovery.is_some()
    }

    pub(super) fn source_incident_attempts(&self) -> u64 {
        self.source.recovery.map_or(0, |incident| incident.attempts)
    }

    pub(super) fn source_incident_cause(&self) -> Option<RecoveryCause> {
        self.source.recovery.map(|incident| incident.cause)
    }

    pub(super) fn source_last_closed_incident(&self) -> u64 {
        self.source.last_closed_incident
    }

    pub(super) fn source_files_equal_remote(&self) -> bool {
        self.source.filesystem == self.remote_files
    }

    pub(super) fn peer_files_equal_remote(&self) -> bool {
        self.peer.filesystem == self.remote_files
    }

    pub(super) fn source_observer_live(&mut self, live: bool) {
        self.source.observer_live = live;
    }

    pub(super) fn mark_source_cas_uncertain(&mut self, uncertain: bool) {
        self.source.cas_uncertain = uncertain;
    }

    pub(super) fn set_source_cycle_active(&mut self, active: bool) {
        self.source.engine_cycle_active = active;
    }

    pub(super) fn set_source_unstable(&mut self, unstable: bool) {
        self.source.unstable_files = unstable;
    }

    pub(super) fn replace_source_root(&mut self) {
        self.source.native.stream_epoch = self.source.native.stream_epoch.saturating_add(1);
        self.source.observe_loss(RecoveryCause::RootReplaced);
    }

    pub(super) fn block_source(&mut self, requirement: ActionRequirement) {
        self.source.blocked.insert(requirement);
    }

    pub(super) fn restore_source(&mut self, requirement: ActionRequirement) {
        self.source.blocked.remove(&requirement);
        if requirement != ActionRequirement::Integrity {
            self.source.open_incident(RecoveryCause::Startup);
        }
    }

    pub(super) fn crash_source(&mut self, phase: CrashPhase) {
        self.source.crash_and_restart(phase);
    }
}
