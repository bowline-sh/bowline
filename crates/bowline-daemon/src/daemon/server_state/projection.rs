use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bowline_control_plane::DeviceApprovalRequestList;
use bowline_core::devices::display_matching_code;
use bowline_core::ids::DeviceId;
use bowline_core::status::{
    StatusFact, StatusFactScope, StatusItem, StatusItemKind, StatusSubject, StatusSubjectKind,
    status_fact_policy,
};
use bowline_core::wire::generated::DeviceApprovalAffordance;
use bowline_local::metadata::DEFAULT_DATABASE_FILE;

use bowline_daemon::manifest_transport::RefObserverReadiness;
use bowline_daemon::status_projection::StatusProjectionService;
use bowline_daemon::status_projection::{
    DaemonInstanceId, DaemonStatusProjection, ProjectionServiceConfig, SafetyRefreshInterval,
    StatusInputEvent, StatusSource,
};
use bowline_daemon::status_projection::{
    DeviceTrustStatusFacts, LocalStatusProjectionCollector, SharedStatusSourceCollector,
    SharedStatusSourceHandle, StatusSourceCollector, StatusSourceFacts, StatusSourceState,
    StatusSourceStateFacts,
};
use bowline_daemon::status_projection::{
    EngineStatusCollector, ObservationAuthority, ObservationAuthoritySource,
};
use bowline_daemon::watcher_recovery::RecoveryLifecycle;

use crate::daemon::server_state::CachedDaemonStatus;
use crate::daemon::sync::{
    NotificationPollCompletion, PreparedStatusPublish, StatusPublishCompletion,
};
use crate::daemon::{
    DaemonRuntime, DaemonServerState, STATUS_PUBLISH_INTERVAL, current_timestamp, finder_status,
};

pub(super) struct ProjectionSourceHandles {
    pub(super) sync_runtime: SharedStatusSourceHandle,
    pub(super) device_trust: SharedStatusSourceHandle,
    _update_availability: SharedStatusSourceHandle,
    _notification_state: SharedStatusSourceHandle,
    pub(super) service_runtime: SharedStatusSourceHandle,
}

pub(in crate::daemon) struct ProjectionAdapterPoll {
    initial_notification: Option<Arc<DaemonStatusProjection>>,
    latest: Option<Arc<DaemonStatusProjection>>,
    heartbeat: Option<Arc<DaemonStatusProjection>>,
    current: Option<Arc<DaemonStatusProjection>>,
}

impl DaemonServerState {
    pub(in crate::daemon) fn prepare_projection_adapter_poll(&self) -> ProjectionAdapterPoll {
        let initial_notification = self
            .initial_notification_projection
            .lock()
            .ok()
            .and_then(|mut initial| initial.take());
        let latest = self.take_latest_projection();
        if let Some(projection) = latest.as_ref() {
            self.publish_rpc_projection(projection);
        }
        let heartbeat = self
            .take_heartbeat_due()
            .then(|| self.projection.current().ok())
            .flatten();
        if let Some(projection) = heartbeat.as_ref() {
            self.publish_finder_projection_at(projection, &current_timestamp());
        }
        let current = latest
            .as_ref()
            .or(heartbeat.as_ref())
            .cloned()
            .or_else(|| self.projection.current().ok());
        ProjectionAdapterPoll {
            initial_notification,
            latest,
            heartbeat,
            current,
        }
    }

    pub(in crate::daemon) fn poll_projection_adapters(
        &self,
        runtime: &mut DaemonRuntime,
        prepare_hosted_publish: bool,
        poll: ProjectionAdapterPoll,
    ) -> Option<PreparedStatusPublish> {
        let now = Instant::now();
        let mut prepared_publish = None;
        self.observe_runtime_sources_if_due(runtime, now);
        if let Some(initial) = poll.initial_notification {
            runtime.poll_notifications_for_projection(&initial.status, &self.projection_input);
        }
        if let Some(projection) = poll.latest {
            if prepare_hosted_publish {
                prepared_publish = runtime.prepare_projection_status(
                    &projection,
                    false,
                    now,
                    &self.projection_input,
                );
            }
            runtime.poll_notifications_for_projection(&projection.status, &self.projection_input);
        }
        if let Some(current) = poll.heartbeat
            && prepare_hosted_publish
            && prepared_publish.is_none()
        {
            prepared_publish =
                runtime.prepare_projection_status(&current, true, now, &self.projection_input);
        }
        if prepare_hosted_publish
            && prepared_publish.is_none()
            && let Some(current) = poll.current
        {
            prepared_publish =
                runtime.prepare_projection_status(&current, false, now, &self.projection_input);
            if prepared_publish.is_none() {
                prepared_publish = runtime.prepare_projection_status_retry_if_due(
                    &current,
                    now,
                    &self.projection_input,
                );
            }
        }
        prepared_publish
    }

    pub(in crate::daemon) fn forward_projection_input(&self, input: StatusInputEvent) {
        self.send_projection_event(input);
    }

    pub(in crate::daemon) fn complete_notification_poll(
        &self,
        runtime: &mut DaemonRuntime,
        completion: NotificationPollCompletion,
    ) {
        runtime.complete_notification_poll(completion, &self.projection_input);
    }

    pub(in crate::daemon) fn complete_status_publish(
        &self,
        runtime: &mut DaemonRuntime,
        completion: StatusPublishCompletion,
    ) {
        runtime.complete_status_publish(completion, &self.projection_input);
    }

    pub(in crate::daemon) fn shutdown_projection(&self, grace: Duration) -> io::Result<bool> {
        self.projection
            .shutdown_and_join(grace)
            .map_err(projection_io_error)
    }

    pub(in crate::daemon) fn join_projection_after_shutdown(&self) -> io::Result<()> {
        self.projection
            .join_after_shutdown()
            .map_err(projection_io_error)
    }

    fn observe_runtime_sources_if_due(&self, runtime: &DaemonRuntime, now: Instant) {
        let observe_due = self
            .next_source_observation
            .lock()
            .map(|mut next_observation| {
                if now < *next_observation {
                    return false;
                }
                *next_observation = now + Duration::from_secs(1);
                true
            })
            .unwrap_or(false);
        if observe_due {
            self.observe_runtime_sources(runtime);
            self.send_projection_event(StatusInputEvent::SourceChanged(StatusSource::Metadata));
            self.send_projection_event(StatusInputEvent::SourceChanged(StatusSource::Convergence));
        }
    }

    fn observe_runtime_sources(&self, runtime: &DaemonRuntime) {
        let adapters = runtime_adapter_facts(runtime);
        self.update_projection_source(
            &self.projection_sources.sync_runtime,
            StatusSourceFacts::SyncRuntime(adapters.observer),
        );
        self.update_projection_source(
            &self.projection_sources.service_runtime,
            StatusSourceFacts::ServiceRuntime(adapters.watcher),
        );
    }

    pub(super) fn update_projection_source(
        &self,
        handle: &SharedStatusSourceHandle,
        facts: StatusSourceFacts,
    ) {
        let source = facts.source();
        if handle.update(facts) {
            self.send_projection_event(StatusInputEvent::SourceChanged(source));
        }
    }

    pub(super) fn send_projection_event(&self, event: StatusInputEvent) {
        if let Err(error) = self.projection_input.send(event) {
            eprintln!("bowline-daemon status projection input failed: {error}");
        }
    }

    fn take_latest_projection(&self) -> Option<Arc<DaemonStatusProjection>> {
        let receiver = self.projection_updates.lock().ok()?;
        let mut latest = None;
        while let Ok(projection) = receiver.try_recv() {
            latest = Some(projection);
        }
        latest
    }

    fn take_heartbeat_due(&self) -> bool {
        let Ok(receiver) = self.projection_heartbeats.lock() else {
            return false;
        };
        let mut due = false;
        while receiver.try_recv().is_ok() {
            due = true;
        }
        due
    }

    pub(super) fn publish_rpc_projection(&self, projection: &DaemonStatusProjection) {
        let next = CachedDaemonStatus {
            instance_id: projection.instance_id.as_str().to_string(),
            sequence: projection.sequence.get(),
            status: projection.status.clone(),
        };
        if let Ok(mut status) = self.status.lock() {
            *status = next.clone();
        }
        self.projection_input.record_rpc_serialization();
        self.publish_finder_projection(projection);
        if let Ok(subscriptions) = self.subscriptions.lock() {
            for subscription in subscriptions.values() {
                if let Some(scoped) = self.apply_status_scope(next.clone(), subscription.scope()) {
                    subscription.publish(scoped);
                }
            }
        }
    }

    pub(super) fn publish_finder_projection(&self, projection: &DaemonStatusProjection) {
        self.publish_finder_projection_at(projection, projection.generated_at.as_str());
    }

    fn publish_finder_projection_at(
        &self,
        projection: &DaemonStatusProjection,
        delivered_at: &str,
    ) {
        let Some(destination) = self.finder_snapshot_path.as_ref() else {
            return;
        };
        let roots = self
            .sync_options
            .as_ref()
            .map(|args| vec![args.root.clone()])
            .unwrap_or_default();
        match finder_status::write_snapshot(destination, &roots, projection, delivered_at) {
            Ok(()) => self.projection_input.record_finder_snapshot(true),
            Err(error) => {
                self.projection_input.record_finder_snapshot(false);
                eprintln!("bowline-daemon Finder status delivery failed: {error}");
            }
        }
    }
}

pub(super) fn start_projection(
    runtime: &DaemonRuntime,
    instance_id: &str,
) -> io::Result<(StatusProjectionService, ProjectionSourceHandles)> {
    let sync_args = runtime.sync.as_ref().map(|sync| &sync.args);
    let metadata = match sync_args {
        Some(args) => LocalStatusProjectionCollector::new_for_workspace(
            args.state_root.join(DEFAULT_DATABASE_FILE),
            args.root.display().to_string(),
            args.workspace_id.clone(),
        ),
        None => LocalStatusProjectionCollector::new(None, None, false),
    }
    .map_err(|error| io::Error::other(error.to_string()))?;
    let adapters = runtime_adapter_facts(runtime);
    let (sync_runtime, sync_collector) =
        ready_source_collector(StatusSourceFacts::SyncRuntime(adapters.observer));
    let (device_trust, device_collector) = SharedStatusSourceCollector::new(
        StatusSourceFacts::DeviceTrustDetails(DeviceTrustStatusFacts {
            state: ready_source_state(),
            facts: Vec::new(),
            items: Vec::new(),
            approvals: Vec::new(),
        }),
    );
    let (update_availability, update_collector) =
        ready_source_collector(StatusSourceFacts::UpdateAvailability(ready_source_state()));
    let (notification_state, notification_collector) =
        ready_source_collector(StatusSourceFacts::NotificationState(ready_source_state()));
    let (service_runtime, service_collector) =
        ready_source_collector(StatusSourceFacts::ServiceRuntime(adapters.watcher));
    let mut collectors: Vec<Box<dyn StatusSourceCollector>> = vec![Box::new(metadata)];
    // The manifest engine's live snapshot is the convergence source. The handle
    // carries live engine snapshots when the driver is up and a `limited`
    // host-status snapshot while the driver is waiting to (re)build. It is absent
    // only for status-only daemons with no configured workspace, where readiness
    // consumers fail closed on the missing field.
    if let Some(handle) = runtime
        .sync
        .as_ref()
        .map(|sync| sync.manifest_snapshot_handle())
    {
        // Convergence readiness composes engine work with whether the watcher is
        // still delivering. Without the second half an engine that is being told
        // nothing looks exactly like an engine with nothing to do.
        let collector = match runtime.sync.as_ref() {
            Some(sync) => EngineStatusCollector::new(handle).with_observation_authority(Box::new(
                RecoveryObservationAuthority {
                    coordinator: Arc::clone(&sync.recovery_coordinator),
                },
            )),
            None => EngineStatusCollector::new(handle),
        };
        collectors.push(Box::new(collector));
    }
    collectors.push(Box::new(sync_collector));
    collectors.push(Box::new(device_collector));
    collectors.push(Box::new(update_collector));
    collectors.push(Box::new(notification_collector));
    collectors.push(Box::new(service_collector));
    let config =
        ProjectionServiceConfig::new(DaemonInstanceId::new(instance_id), STATUS_PUBLISH_INTERVAL)
            .and_then(|config| {
                SafetyRefreshInterval::new(Duration::from_secs(4 * 60))
                    .map(|interval| config.with_safety_refresh_interval(interval))
            })
            .map_err(projection_io_error)?;
    let service =
        StatusProjectionService::start(config, collectors).map_err(projection_io_error)?;
    Ok((
        service,
        ProjectionSourceHandles {
            sync_runtime,
            device_trust,
            _update_availability: update_availability,
            _notification_state: notification_state,
            service_runtime,
        },
    ))
}

fn ready_source_collector(
    facts: StatusSourceFacts,
) -> (SharedStatusSourceHandle, SharedStatusSourceCollector) {
    SharedStatusSourceCollector::new(facts)
}

fn ready_source_state() -> StatusSourceStateFacts {
    StatusSourceStateFacts {
        state: StatusSourceState::Ready,
        pending_count: 0,
    }
}

pub(super) struct RuntimeAdapterFacts {
    pub(super) observer: StatusSourceStateFacts,
    pub(super) watcher: StatusSourceStateFacts,
}

pub(super) fn runtime_adapter_facts(runtime: &DaemonRuntime) -> RuntimeAdapterFacts {
    let Some(sync) = runtime.sync.as_ref() else {
        return RuntimeAdapterFacts {
            observer: ready_source_state(),
            watcher: ready_source_state(),
        };
    };
    // Remote readiness requires an initial Convex subscription value, not merely
    // a running driver thread. Watcher readiness is owned by the recovery
    // coordinator: an installed kernel is necessary but cannot hide an open or
    // blocked recovery incident.
    RuntimeAdapterFacts {
        observer: adapter_source_state(observer_source_state(sync.manifest_observer_readiness())),
        watcher: adapter_source_state(watcher_source_state(sync)),
    }
}

/// A reconnect clears an ordinary observer failure on its own, so retrying is
/// merely degraded. A refused account session does not clear until this device
/// signs in again: without reporting it unavailable the daemon would sit
/// silently, never learning that another device moved the workspace head.
pub(super) fn observer_source_state(readiness: RefObserverReadiness) -> StatusSourceState {
    match readiness {
        RefObserverReadiness::Live => StatusSourceState::Ready,
        RefObserverReadiness::Retrying => StatusSourceState::Degraded,
        // The credentials are fine and the subscription is alive; what this host
        // cannot do is trust the device that moved the head. Reported
        // unavailable for the same reason a refused session is: no amount of
        // waiting makes those heads readable.
        RefObserverReadiness::Blocked { .. } => StatusSourceState::Unavailable,
    }
}

fn watcher_source_state(sync: &crate::daemon::ContinuousSyncRuntime) -> StatusSourceState {
    if !sync.watcher.is_armed() {
        return StatusSourceState::Degraded;
    }
    match sync.recovery_coordinator.snapshot() {
        Ok(snapshot) => match snapshot.lifecycle() {
            RecoveryLifecycle::Nominal => StatusSourceState::Ready,
            RecoveryLifecycle::Recovering => StatusSourceState::Degraded,
            RecoveryLifecycle::Blocked => StatusSourceState::Unavailable,
        },
        Err(_) => StatusSourceState::Unavailable,
    }
}

fn adapter_source_state(state: StatusSourceState) -> StatusSourceStateFacts {
    StatusSourceStateFacts {
        state,
        // The engine snapshot is the sole queue authority. Runtime adapter
        // health must never overlay a second queued-work count.
        pending_count: 0,
    }
}

pub(super) fn projection_io_error(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

pub(super) fn device_trust_status_facts(
    trust: &DeviceApprovalRequestList,
    workspace_root: Option<&Path>,
) -> DeviceTrustStatusFacts {
    let mut pending = trust
        .pending_requests
        .iter()
        .filter(|request| request.state == bowline_control_plane::DeviceRequestState::Pending)
        .collect::<Vec<_>>();
    pending.sort_by(|left, right| left.request_id.as_str().cmp(right.request_id.as_str()));
    let mut facts = Vec::with_capacity(pending.len());
    let mut items = Vec::with_capacity(pending.len());
    let mut approvals = Vec::with_capacity(pending.len());
    let generated_at = current_timestamp();
    for request in pending {
        let request_id = request.request_id.as_str();
        let device_id = request.device_id.as_str();
        let code = display_matching_code(&request.matching_code);
        let approve_command = workspace_root.map_or_else(String::new, |root| {
            format!(
                "bowline device approve --root {} --code {}",
                bowline_core::shell::quote_word(&root.display().to_string()),
                bowline_core::shell::quote_word(&code),
            )
        });
        approvals.push(DeviceApprovalAffordance {
            request_id: request_id.to_string(),
            device_name: request.device_name.clone(),
            code: code.clone(),
            approve_command,
        });
        items.push(StatusItem {
            kind: StatusItemKind::Device,
            summary: format!("{} is waiting for local approval.", request.device_name),
            subject: Some(StatusSubject {
                kind: StatusSubjectKind::DeviceApprovalRequest,
                id: request_id.to_string(),
                path: None,
            }),
            path: None,
            classification: None,
            mode: None,
            access: Vec::new(),
            event_id: None,
            event_name: None,
            device_id: Some(DeviceId::new(device_id)),
            lease_id: None,
            project_id: None,
            snapshot_id: None,
            policy_version: None,
            env_record_id: None,
        });
        let policy = status_fact_policy("device.approval_requested");
        let mut fact = StatusFact::new(
            format!("device-approval:{request_id}"),
            "device.approval_requested",
            policy.authority,
            StatusFactScope::Device,
            generated_at.clone(),
            format!("device-approval:{request_id}"),
        )
        .with_scope_id(device_id);
        if let Some(action) = fact.action.as_mut() {
            action.target_id = Some(request_id.to_string());
        }
        facts.push(fact);
    }
    DeviceTrustStatusFacts {
        state: StatusSourceStateFacts {
            state: StatusSourceState::Ready,
            pending_count: approvals.len() as u64,
        },
        facts,
        items,
        approvals,
    }
}

/// Observation authority as the watcher-recovery coordinator sees it.
///
/// Only recovery lifecycle is read here, not the callback overflow latch, which
/// lives inside the bridge thread. That is sound only because the bridge holds
/// the invariant that a nominal coordinator implies no asserted overflow
/// request: it reopens recovery when it finds that pairing. If that backstop is
/// ever removed, a stuck latch would become invisible here again.
struct RecoveryObservationAuthority {
    coordinator: Arc<bowline_daemon::watcher_recovery::WatcherRecoveryCoordinator>,
}

// The coordinator is not Debug and should not become Debug just to satisfy a
// collector bound, so name the source without leaking its interior.
impl std::fmt::Debug for RecoveryObservationAuthority {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RecoveryObservationAuthority")
            .finish_non_exhaustive()
    }
}

impl ObservationAuthoritySource for RecoveryObservationAuthority {
    fn authority(&self) -> ObservationAuthority {
        match self.coordinator.snapshot() {
            Ok(snapshot) => ObservationAuthority {
                recovery_open: snapshot.lifecycle() != RecoveryLifecycle::Nominal,
                overflow_pending: false,
            },
            // An unreadable coordinator is not evidence of authority. Fail closed
            // so status cannot claim ready on a recovery state nobody can read.
            Err(_) => ObservationAuthority {
                recovery_open: true,
                overflow_pending: false,
            },
        }
    }

    fn revision(&self) -> u64 {
        self.coordinator
            .snapshot()
            .map(|snapshot| snapshot.snapshot_revision().get())
            .unwrap_or_default()
    }
}
