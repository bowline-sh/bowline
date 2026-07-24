use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use super::{
    collector::{
        StatusSourceCollection, StatusSourceCollector, StatusSourceFacts, StatusSourceFailurePolicy,
    },
    delivery::{DeliveryOutcome, LatestProjectionReceiver, Projection, latest_projection_channel},
    input::{PendingInputBatch, PendingInputState, StatusProjectionInput, take_pending_input},
    reducer::reduce_projection_status,
    retry::RetrySchedule,
    subscriptions::SharedProjectionState,
    types::{
        DaemonStatusProjection, ProjectionBuildReason, ProjectionServiceConfig, SourceFreshness,
        SourceRevision, StatusProjectionError, StatusProjectionMetrics, StatusSequence,
        StatusSource, StatusSourceRevision, StatusTimestamp, semantic_fingerprint,
    },
};

struct StagedCollectionBatch {
    sources: Vec<StatusSource>,
    had_failure: bool,
    successful_sources: BTreeSet<StatusSource>,
    recoverable_failures: BTreeSet<StatusSource>,
    unrecoverable_failures: BTreeSet<StatusSource>,
}

#[derive(Debug)]
pub struct ProjectionSubscription {
    pub initial: Projection,
    pub updates: LatestProjectionReceiver,
}

#[derive(Debug)]
pub struct ProjectionHeartbeatSubscription {
    pub current: Projection,
    pub deadlines: LatestProjectionReceiver,
}

pub struct StatusProjectionService {
    pending: Arc<Mutex<PendingInputState>>,
    wake_sender: SyncSender<()>,
    shared: Arc<Mutex<SharedProjectionState>>,
    worker: Mutex<Option<JoinHandle<()>>>,
    worker_done: Mutex<Option<Receiver<()>>>,
}

impl StatusProjectionService {
    pub fn start(
        config: ProjectionServiceConfig,
        collectors: Vec<Box<dyn StatusSourceCollector>>,
    ) -> Result<Self, StatusProjectionError> {
        let collectors = collector_map(collectors)?;
        let source_count = collectors.len();
        if !collectors.contains_key(&StatusSource::Metadata) {
            return Err(StatusProjectionError::MissingMetadataCollector);
        }
        let (wake_sender, wake_receiver) = mpsc::sync_channel(1);
        let (ready_sender, ready_receiver) = mpsc::channel();
        let (done_sender, done_receiver) = mpsc::sync_channel(1);
        let pending = Arc::new(Mutex::new(PendingInputState::new(source_count)));
        let shared = Arc::new(Mutex::new(SharedProjectionState::new()));
        let worker_pending = Arc::clone(&pending);
        let worker_shared = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("bowline-status-projection".to_string())
            .spawn(move || {
                let mut runtime = ProjectionRuntime::new(config, collectors, worker_shared);
                let initial = runtime.initialize();
                let ready = initial.clone();
                let _ready_result = ready_sender.send(ready);
                if initial.is_ok() {
                    runtime.run(wake_receiver, worker_pending);
                }
                let _receiver_gone = done_sender.send(());
            })
            .map_err(|_| StatusProjectionError::ChannelClosed {
                operation: "spawn worker",
            })?;
        match ready_receiver.recv() {
            Ok(Ok(())) => Ok(Self {
                pending,
                wake_sender,
                shared,
                worker: Mutex::new(Some(worker)),
                worker_done: Mutex::new(Some(done_receiver)),
            }),
            Ok(Err(error)) => {
                let _join_result = worker.join();
                Err(error)
            }
            Err(_) => {
                let _join_result = worker.join();
                Err(StatusProjectionError::ChannelClosed {
                    operation: "await initialization",
                })
            }
        }
    }

    pub fn input(&self) -> StatusProjectionInput {
        StatusProjectionInput::new(
            Arc::clone(&self.pending),
            self.wake_sender.clone(),
            Arc::clone(&self.shared),
        )
    }

    pub fn current(&self) -> Result<Projection, StatusProjectionError> {
        self.shared
            .lock()
            .ok()
            .and_then(|state| state.current.clone())
            .ok_or(StatusProjectionError::ChannelClosed {
                operation: "read current projection",
            })
    }

    pub fn subscribe(&self) -> Result<ProjectionSubscription, StatusProjectionError> {
        let mut shared = self
            .shared
            .lock()
            .map_err(|_| StatusProjectionError::ChannelClosed {
                operation: "subscribe",
            })?;
        shared.prune_disconnected_subscribers();
        let initial = shared
            .current
            .clone()
            .ok_or(StatusProjectionError::ChannelClosed {
                operation: "read subscription snapshot",
            })?;
        let (sender, updates) = latest_projection_channel();
        shared.projection_subscribers.push(sender);
        shared.update_subscriber_gauges();
        Ok(ProjectionSubscription { initial, updates })
    }

    pub fn subscribe_heartbeats(
        &self,
    ) -> Result<ProjectionHeartbeatSubscription, StatusProjectionError> {
        let mut shared = self
            .shared
            .lock()
            .map_err(|_| StatusProjectionError::ChannelClosed {
                operation: "subscribe heartbeats",
            })?;
        shared.prune_disconnected_subscribers();
        let current = shared
            .current
            .clone()
            .ok_or(StatusProjectionError::ChannelClosed {
                operation: "read heartbeat snapshot",
            })?;
        let (sender, deadlines) = latest_projection_channel();
        shared.heartbeat_subscribers.push(sender);
        shared.update_subscriber_gauges();
        Ok(ProjectionHeartbeatSubscription { current, deadlines })
    }

    pub fn metrics(&self) -> Result<StatusProjectionMetrics, StatusProjectionError> {
        self.shared
            .lock()
            .map(|state| state.metrics.clone())
            .map_err(|_| StatusProjectionError::ChannelClosed {
                operation: "read metrics",
            })
    }

    pub fn shutdown_and_join(&self, grace: Duration) -> Result<bool, StatusProjectionError> {
        if let Ok(mut pending) = self.pending.lock() {
            pending.shutdown = true;
        }
        let _already_awake = self.wake_sender.try_send(());
        let timed_out = self
            .worker_done
            .lock()
            .map_err(|_| StatusProjectionError::ChannelClosed {
                operation: "lock worker completion",
            })?
            .take()
            .is_some_and(|done| match done.recv_timeout(grace) {
                Ok(()) | Err(RecvTimeoutError::Disconnected) => false,
                Err(RecvTimeoutError::Timeout) => true,
            });
        if timed_out {
            return Ok(true);
        }
        if let Some(worker) = self
            .worker
            .lock()
            .map_err(|_| StatusProjectionError::ChannelClosed {
                operation: "lock worker join handle",
            })?
            .take()
            && worker.join().is_err()
        {
            return Err(StatusProjectionError::ChannelClosed {
                operation: "join worker",
            });
        }
        self.clear_delivery_state();
        Ok(false)
    }

    pub fn join_after_shutdown(&self) -> Result<(), StatusProjectionError> {
        if let Some(worker) = self
            .worker
            .lock()
            .map_err(|_| StatusProjectionError::ChannelClosed {
                operation: "lock worker join handle",
            })?
            .take()
            && worker.join().is_err()
        {
            return Err(StatusProjectionError::ChannelClosed {
                operation: "join worker",
            });
        }
        self.clear_delivery_state();
        Ok(())
    }

    fn clear_delivery_state(&self) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.projection_subscribers.clear();
            shared.heartbeat_subscribers.clear();
            shared.metrics.active_collector_retries = 0;
            shared.update_subscriber_gauges();
        }
    }
}

impl Drop for StatusProjectionService {
    fn drop(&mut self) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.shutdown = true;
        }
        let _wake_result = self.wake_sender.try_send(());
        let _join_result = self.join_after_shutdown();
    }
}

fn collector_map(
    collectors: Vec<Box<dyn StatusSourceCollector>>,
) -> Result<BTreeMap<StatusSource, Box<dyn StatusSourceCollector>>, StatusProjectionError> {
    let mut mapped = BTreeMap::new();
    for collector in collectors {
        let source = collector.source();
        if mapped.insert(source, collector).is_some() {
            return Err(StatusProjectionError::DuplicateCollector { source });
        }
    }
    Ok(mapped)
}

struct ProjectionRuntime {
    config: ProjectionServiceConfig,
    collectors: BTreeMap<StatusSource, Box<dyn StatusSourceCollector>>,
    sources: BTreeMap<StatusSource, SourceRevision>,
    facts: BTreeMap<StatusSource, StatusSourceFacts>,
    replay_sources: BTreeSet<StatusSource>,
    retry_schedule: RetrySchedule,
    contract_retry_sources: BTreeSet<StatusSource>,
    shared: Arc<Mutex<SharedProjectionState>>,
    next_heartbeat: Instant,
    next_safety_refresh: Instant,
}

impl ProjectionRuntime {
    fn new(
        config: ProjectionServiceConfig,
        collectors: BTreeMap<StatusSource, Box<dyn StatusSourceCollector>>,
        shared: Arc<Mutex<SharedProjectionState>>,
    ) -> Self {
        let next_heartbeat = Instant::now() + config.heartbeat_interval();
        let next_safety_refresh = Instant::now() + config.safety_refresh_interval();
        Self {
            config,
            collectors,
            sources: BTreeMap::new(),
            facts: BTreeMap::new(),
            replay_sources: BTreeSet::new(),
            retry_schedule: RetrySchedule::default(),
            contract_retry_sources: BTreeSet::new(),
            shared,
            next_heartbeat,
            next_safety_refresh,
        }
    }

    fn initialize(&mut self) -> Result<(), StatusProjectionError> {
        let dirty = self.collectors.keys().copied().collect();
        let observed_at = current_timestamp()?;
        let now = Instant::now();
        let batch = self.collect_sources(&dirty, observed_at.clone(), now, true)?;
        let result = self
            .build_projection(
                StatusSequence::INITIAL,
                observed_at,
                ProjectionBuildReason::Initial,
            )
            .and_then(|projection| {
                self.shared
                    .lock()
                    .map(|mut shared| shared.current = Some(Arc::new(projection)))
                    .map_err(|_| StatusProjectionError::ChannelClosed {
                        operation: "store initial projection",
                    })
            });
        if result.is_ok() {
            self.commit_staged_sources(&batch.sources);
            self.update_retry_schedule(&batch, Instant::now());
        } else {
            self.abort_staged_sources(&batch.sources);
        }
        result
    }

    fn run(&mut self, wake_receiver: Receiver<()>, pending: Arc<Mutex<PendingInputState>>) {
        loop {
            if pending.lock().is_ok_and(|pending| pending.shutdown) {
                return;
            }
            let now = Instant::now();
            self.prune_disconnected_subscribers();
            self.process_due_deadlines(now);
            let timeout = self
                .next_deadline()
                .saturating_duration_since(Instant::now());
            match wake_receiver.recv_timeout(timeout) {
                Ok(()) => {
                    let Some(batch) = take_pending_input(&pending) else {
                        return;
                    };
                    if batch.shutdown {
                        return;
                    }
                    self.handle_input_batch(batch);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    fn handle_input_batch(&mut self, batch: PendingInputBatch) {
        let mut dirty = batch.dirty;
        let reason = if batch.refresh_all {
            ProjectionBuildReason::RefreshAll
        } else {
            ProjectionBuildReason::SourceChanged
        };
        if batch.refresh_all {
            dirty.extend(self.collectors.keys().copied());
        } else {
            for source in &dirty {
                if self.retry_schedule.is_scheduled(*source) {
                    increment_source_metric(&self.shared, *source, SourceMetric::RetryAcceleration);
                }
            }
        }
        let mark_dirty = dirty.clone();
        if !dirty.is_empty() {
            dirty.append(&mut self.replay_sources);
        }
        if dirty.is_empty() {
            return;
        }
        self.collect_and_publish(dirty, mark_dirty, reason, BTreeSet::new());
    }

    fn collect_and_publish(
        &mut self,
        dirty: BTreeSet<StatusSource>,
        mark_dirty: BTreeSet<StatusSource>,
        mut reason: ProjectionBuildReason,
        retry_attempts: BTreeSet<StatusSource>,
    ) {
        for source in mark_dirty {
            if let Some(collector) = self.collectors.get_mut(&source) {
                collector.mark_dirty();
            }
        }
        for source in retry_attempts {
            increment_source_metric(&self.shared, source, SourceMetric::RetryAttempt);
            if self.contract_retry_sources.contains(&source) {
                increment_source_metric(&self.shared, source, SourceMetric::ContractRetryAttempt);
            }
        }
        let Ok(observed_at) = current_timestamp() else {
            self.replay_sources.extend(dirty);
            return;
        };
        let now = Instant::now();
        let previous_sources = self.sources.clone();
        let previous_facts = self.facts.clone();
        let batch = match self.collect_sources(&dirty, observed_at.clone(), now, false) {
            Ok(batch) => batch,
            Err(_) => return,
        };
        if batch.had_failure {
            reason = ProjectionBuildReason::SourceFailure;
        }
        if self.publish_if_changed(observed_at, reason).is_ok() {
            self.commit_staged_sources(&batch.sources);
            self.update_retry_schedule(&batch, Instant::now());
        } else {
            self.sources = previous_sources;
            self.facts = previous_facts;
            self.abort_staged_sources(&batch.sources);
            self.replay_sources.extend(batch.sources);
        }
    }

    fn process_due_deadlines(&mut self, now: Instant) {
        if now >= self.next_heartbeat {
            self.next_heartbeat = now + self.config.heartbeat_interval();
            self.emit_heartbeat();
        }
        if now >= self.next_safety_refresh {
            self.next_safety_refresh = now + self.config.safety_refresh_interval();
            if let Ok(mut shared) = self.shared.lock() {
                shared.metrics.safety_refreshes = shared.metrics.safety_refreshes.saturating_add(1);
            }
            let mut dirty = self.collectors.keys().copied().collect::<BTreeSet<_>>();
            let mark_dirty = dirty.clone();
            dirty.append(&mut self.replay_sources);
            self.collect_and_publish(
                dirty,
                mark_dirty,
                ProjectionBuildReason::SafetyRefresh,
                BTreeSet::new(),
            );
        }
        let retry_attempts = self
            .retry_schedule
            .due_sources(Instant::now())
            .into_iter()
            .collect::<BTreeSet<_>>();
        if retry_attempts.is_empty() {
            return;
        }
        let mut dirty = retry_attempts.clone();
        dirty.append(&mut self.replay_sources);
        self.collect_and_publish(
            dirty,
            retry_attempts.clone(),
            ProjectionBuildReason::Retry,
            retry_attempts,
        );
    }

    fn next_deadline(&self) -> Instant {
        self.retry_schedule
            .next_deadline()
            .map_or(self.next_heartbeat.min(self.next_safety_refresh), |retry| {
                retry.min(self.next_heartbeat).min(self.next_safety_refresh)
            })
    }

    fn update_retry_schedule(&mut self, batch: &StagedCollectionBatch, now: Instant) {
        for source in &batch.sources {
            if self.contract_retry_sources.remove(source) {
                increment_source_metric(&self.shared, *source, SourceMetric::ContractRetryRecovery);
            }
        }
        for source in &batch.successful_sources {
            if self.retry_schedule.record_success(*source) {
                increment_source_metric(&self.shared, *source, SourceMetric::RetryRecovery);
            }
        }
        for source in &batch.unrecoverable_failures {
            self.retry_schedule.abandon(*source);
            increment_source_metric(&self.shared, *source, SourceMetric::RetryAbandoned);
        }
        for source in &batch.recoverable_failures {
            self.schedule_retry(*source, now, false);
        }
        self.update_retry_gauges();
    }

    fn schedule_retry(&mut self, source: StatusSource, now: Instant, contract: bool) {
        let scheduled = self
            .retry_schedule
            .record_failure(source, now, self.config.retry_policy());
        increment_source_metric(&self.shared, source, SourceMetric::RetryScheduled);
        if contract {
            self.contract_retry_sources.insert(source);
            increment_source_metric(&self.shared, source, SourceMetric::ContractRetryScheduled);
        }
        if scheduled.capped {
            increment_source_metric(&self.shared, source, SourceMetric::RetryDelayCapped);
            if contract {
                increment_source_metric(
                    &self.shared,
                    source,
                    SourceMetric::ContractRetryDelayCapped,
                );
            }
        }
        if let Ok(mut shared) = self.shared.lock() {
            shared
                .metrics
                .collector_retry_delay_nanos
                .insert(source, scheduled.delay.as_nanos());
        }
        self.update_retry_gauges();
    }

    fn update_retry_gauges(&self) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.metrics.active_collector_retries = self.retry_schedule.len() as u64;
            shared.metrics.max_pending_collector_retries = shared
                .metrics
                .max_pending_collector_retries
                .max(self.retry_schedule.len() as u64);
        }
    }

    fn prune_disconnected_subscribers(&self) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.prune_disconnected_subscribers();
        }
    }

    fn current_projection(&self) -> Result<Projection, StatusProjectionError> {
        self.shared
            .lock()
            .ok()
            .and_then(|state| state.current.clone())
            .ok_or(StatusProjectionError::ChannelClosed {
                operation: "read worker projection",
            })
    }

    fn collect_sources(
        &mut self,
        dirty: &BTreeSet<StatusSource>,
        observed_at: StatusTimestamp,
        now: Instant,
        initializing: bool,
    ) -> Result<StagedCollectionBatch, StatusProjectionError> {
        let mut staged_sources = Vec::new();
        let mut staged_results = Vec::new();
        let mut pending_sources = dirty.iter().copied();
        while let Some(source) = pending_sources.next() {
            let can_accept_unchanged = self.can_accept_unchanged(source);
            let Some(collector) = self.collectors.get_mut(&source) else {
                continue;
            };
            increment_source_metric(&self.shared, source, SourceMetric::Call);
            let policy = collector.failure_policy();
            let result = collector.stage(observed_at.clone(), now);
            let contract_mismatch = match &result {
                Ok(StatusSourceCollection::Updated { facts, .. }) => facts.source() != source,
                Ok(StatusSourceCollection::Unchanged) => !can_accept_unchanged,
                Err(failure) => failure.source != source,
            };
            if contract_mismatch {
                collector.reject_staged();
                self.abort_staged_sources(&staged_sources);
                self.replay_sources.extend(staged_sources);
                self.replay_sources.extend(pending_sources);
                if !initializing {
                    self.schedule_retry(source, now, true);
                }
                return Err(StatusProjectionError::SourceContract { source });
            }
            staged_sources.push(source);
            staged_results.push((source, policy, result));
        }

        let mut next_sources = self.sources.clone();
        let mut next_facts = self.facts.clone();
        let mut had_failure = false;
        let mut successful_sources = BTreeSet::new();
        let mut recoverable_failures = BTreeSet::new();
        let mut unrecoverable_failures = BTreeSet::new();
        for (source, policy, result) in staged_results {
            match result {
                Ok(StatusSourceCollection::Updated {
                    revision,
                    observed_at,
                    facts,
                }) => {
                    next_sources.insert(
                        source,
                        SourceRevision {
                            source,
                            revision,
                            observed_at,
                            freshness: SourceFreshness::Current,
                        },
                    );
                    next_facts.insert(source, facts);
                    successful_sources.insert(source);
                }
                Ok(StatusSourceCollection::Unchanged) => {
                    increment_source_metric(&self.shared, source, SourceMetric::Skip);
                    if let Some(revision) = next_sources.get_mut(&source) {
                        revision.freshness = SourceFreshness::Current;
                        revision.observed_at = observed_at.clone();
                    }
                    successful_sources.insert(source);
                }
                Err(failure) => {
                    had_failure = true;
                    increment_source_metric(&self.shared, source, SourceMetric::Failure);
                    if initializing && source == StatusSource::Metadata {
                        self.abort_staged_sources(&staged_sources);
                        return Err(StatusProjectionError::InitialCollection {
                            source,
                            code: failure.code,
                        });
                    }
                    let has_last_known = next_facts.contains_key(&source);
                    let freshness = match policy {
                        StatusSourceFailurePolicy::RetainLastKnown if has_last_known => {
                            SourceFreshness::Stale
                        }
                        StatusSourceFailurePolicy::RetainLastKnown
                        | StatusSourceFailurePolicy::Discard => {
                            next_facts.remove(&source);
                            SourceFreshness::Failed
                        }
                    };
                    next_sources
                        .entry(source)
                        .and_modify(|revision| revision.freshness = freshness)
                        .or_insert(SourceRevision {
                            source,
                            revision: StatusSourceRevision::new(0),
                            observed_at: StatusTimestamp::new("1970-01-01T00:00:00Z"),
                            freshness,
                        });
                    if failure.code.is_recoverable() {
                        recoverable_failures.insert(source);
                    } else {
                        unrecoverable_failures.insert(source);
                    }
                }
            }
        }
        self.sources = next_sources;
        self.facts = next_facts;
        Ok(StagedCollectionBatch {
            sources: staged_sources,
            had_failure,
            successful_sources,
            recoverable_failures,
            unrecoverable_failures,
        })
    }

    fn can_accept_unchanged(&self, source: StatusSource) -> bool {
        self.sources
            .get(&source)
            .is_some_and(|revision| revision.freshness == SourceFreshness::Current)
            && self
                .facts
                .get(&source)
                .is_some_and(|facts| facts.source() == source)
    }

    fn abort_staged_sources(&mut self, sources: &[StatusSource]) {
        for source in sources {
            if let Some(collector) = self.collectors.get_mut(source) {
                collector.abort_staged();
            }
        }
    }

    fn commit_staged_sources(&mut self, sources: &[StatusSource]) {
        for source in sources {
            if let Some(collector) = self.collectors.get_mut(source) {
                collector.commit_staged();
            }
        }
    }

    fn publish_if_changed(
        &mut self,
        generated_at: StatusTimestamp,
        reason: ProjectionBuildReason,
    ) -> Result<(), StatusProjectionError> {
        let current = self.current_projection()?;
        let mut next = self.build_projection(current.sequence, generated_at, reason)?;
        if next.semantic_fingerprint == current.semantic_fingerprint {
            let mut shared =
                self.shared
                    .lock()
                    .map_err(|_| StatusProjectionError::ChannelClosed {
                        operation: "store no-op projection",
                    })?;
            shared.current = Some(Arc::new(next));
            shared.metrics.no_op_refreshes = shared.metrics.no_op_refreshes.saturating_add(1);
            return Ok(());
        }
        next.sequence = current.sequence.next();
        let next = Arc::new(next);
        let mut shared = self
            .shared
            .lock()
            .map_err(|_| StatusProjectionError::ChannelClosed {
                operation: "store semantic projection",
            })?;
        shared.current = Some(Arc::clone(&next));
        shared.metrics.semantic_changes = shared.metrics.semantic_changes.saturating_add(1);
        let mut delivered = 0_u64;
        let mut coalesced = 0_u64;
        let mut disconnected = 0_u64;
        shared.projection_subscribers.retain(|subscriber| {
            match subscriber.deliver(Arc::clone(&next)) {
                DeliveryOutcome::Delivered => {
                    delivered = delivered.saturating_add(1);
                    true
                }
                DeliveryOutcome::Coalesced => {
                    coalesced = coalesced.saturating_add(1);
                    true
                }
                DeliveryOutcome::Disconnected => {
                    disconnected = disconnected.saturating_add(1);
                    false
                }
            }
        });
        shared.metrics.projection_updates_delivered = shared
            .metrics
            .projection_updates_delivered
            .saturating_add(delivered);
        shared.metrics.projection_updates_coalesced = shared
            .metrics
            .projection_updates_coalesced
            .saturating_add(coalesced);
        shared.metrics.projection_subscribers_disconnected = shared
            .metrics
            .projection_subscribers_disconnected
            .saturating_add(disconnected);
        shared.metrics.broadcasts = shared
            .metrics
            .broadcasts
            .saturating_add(delivered.saturating_add(coalesced));
        shared.update_subscriber_gauges();
        Ok(())
    }

    fn build_projection(
        &self,
        sequence: StatusSequence,
        generated_at: StatusTimestamp,
        reason: ProjectionBuildReason,
    ) -> Result<DaemonStatusProjection, StatusProjectionError> {
        let started_at = Instant::now();
        let metadata_status = self
            .facts
            .get(&StatusSource::Metadata)
            .and_then(StatusSourceFacts::metadata_output)
            .ok_or(StatusProjectionError::MissingMetadataFacts)?;
        let status =
            reduce_projection_status(metadata_status, &self.sources, &self.facts, &generated_at);
        let semantic_fingerprint = semantic_fingerprint(&status, &self.sources, &self.facts)?;
        if let Ok(mut shared) = self.shared.lock() {
            let builds = shared.metrics.builds_by_reason.entry(reason).or_default();
            *builds = builds.saturating_add(1);
            shared.metrics.build_latency_nanos = shared
                .metrics
                .build_latency_nanos
                .saturating_add(started_at.elapsed().as_nanos());
        }
        Ok(DaemonStatusProjection {
            instance_id: self.config.instance_id().clone(),
            sequence,
            semantic_fingerprint,
            generated_at,
            sources: self.sources.clone(),
            source_facts: self.facts.clone(),
            status,
        })
    }

    fn emit_heartbeat(&mut self) {
        let Ok(mut shared) = self.shared.lock() else {
            return;
        };
        let Some(current) = shared.current.clone() else {
            return;
        };
        let mut delivered = 0_u64;
        let mut coalesced = 0_u64;
        let mut disconnected = 0_u64;
        shared.heartbeat_subscribers.retain(|subscriber| {
            match subscriber.deliver(Arc::clone(&current)) {
                DeliveryOutcome::Delivered => {
                    delivered = delivered.saturating_add(1);
                    true
                }
                DeliveryOutcome::Coalesced => {
                    coalesced = coalesced.saturating_add(1);
                    true
                }
                DeliveryOutcome::Disconnected => {
                    disconnected = disconnected.saturating_add(1);
                    false
                }
            }
        });
        shared.metrics.heartbeat_deliveries = shared
            .metrics
            .heartbeat_deliveries
            .saturating_add(delivered);
        shared.metrics.heartbeat_deliveries_coalesced = shared
            .metrics
            .heartbeat_deliveries_coalesced
            .saturating_add(coalesced);
        shared.metrics.heartbeat_subscribers_disconnected = shared
            .metrics
            .heartbeat_subscribers_disconnected
            .saturating_add(disconnected);
        shared.metrics.heartbeats_emitted = shared.metrics.heartbeats_emitted.saturating_add(1);
        shared.update_subscriber_gauges();
    }
}

#[derive(Clone, Copy)]
enum SourceMetric {
    Call,
    Skip,
    Failure,
    RetryScheduled,
    RetryAttempt,
    RetryRecovery,
    RetryAcceleration,
    RetryAbandoned,
    RetryDelayCapped,
    ContractRetryScheduled,
    ContractRetryAttempt,
    ContractRetryRecovery,
    ContractRetryDelayCapped,
}

fn increment_source_metric(
    shared: &Arc<Mutex<SharedProjectionState>>,
    source: StatusSource,
    metric: SourceMetric,
) {
    let Ok(mut shared) = shared.lock() else {
        return;
    };
    let counters = match metric {
        SourceMetric::Call => &mut shared.metrics.collector_calls,
        SourceMetric::Skip => &mut shared.metrics.collector_skips,
        SourceMetric::Failure => &mut shared.metrics.collector_failures,
        SourceMetric::RetryScheduled => &mut shared.metrics.collector_retries_scheduled,
        SourceMetric::RetryAttempt => &mut shared.metrics.collector_retry_attempts,
        SourceMetric::RetryRecovery => &mut shared.metrics.collector_retry_recoveries,
        SourceMetric::RetryAcceleration => &mut shared.metrics.collector_retry_accelerations,
        SourceMetric::RetryAbandoned => &mut shared.metrics.collector_retry_abandoned,
        SourceMetric::RetryDelayCapped => &mut shared.metrics.collector_retry_delays_capped,
        SourceMetric::ContractRetryScheduled => {
            &mut shared.metrics.collector_contract_retries_scheduled
        }
        SourceMetric::ContractRetryAttempt => &mut shared.metrics.collector_contract_retry_attempts,
        SourceMetric::ContractRetryRecovery => {
            &mut shared.metrics.collector_contract_retry_recoveries
        }
        SourceMetric::ContractRetryDelayCapped => {
            &mut shared.metrics.collector_contract_retry_delays_capped
        }
    };
    let counter = counters.entry(source).or_default();
    *counter = counter.saturating_add(1);
}

fn current_timestamp() -> Result<StatusTimestamp, StatusProjectionError> {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map(StatusTimestamp::new)
        .map_err(|_| StatusProjectionError::TimestampFormatting)
}
