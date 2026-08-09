//! The reactive workspace-ref observer.
//!
//! Bridges the hosted ref subscription into engine wakeups and owns everything
//! that decides how a failed attempt is answered: the lifecycle a status
//! surface reads, the reconnect schedule's inputs, and the trust refresh that
//! lets a running daemon learn a device trusted after it started.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::Sender as EngineEventSender;

use bowline_control_plane::{
    ControlPlaneError, DependencyFailureClass, HostedControlPlaneClient,
    WorkspaceRefStreamConnectionState, WorkspaceRefStreamEvent, WorkspaceRefStreamShutdown,
    workspace_ref_stream_shutdown_pair,
};
use bowline_core::ids::{DeviceId, WorkspaceId};
use bowline_local::sync::manifest_engine::{EngineEvent, EngineProcessIdentity, RefObservation};

use super::head_observation;
use crate::device_trust::{TrustRefreshError, TrustRefreshOutcome};

/// What the reconnect schedule gets to reason about. Only retryable failures
/// reach it; terminal stages remain representable so schedule implementations
/// stay exhaustive and fail safely if a future caller violates that boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconnectAttempt {
    pub consecutive_failures: u32,
    pub stage: RefObserverFailureStage,
}

/// Learn trust for a device whose signed head this host cannot verify, and
/// report what the control plane said about it.
///
/// Injected rather than performed here: the verifier lookup that failed runs
/// inside the client's own response parsing, so the client must never be called
/// again from that path. The bridge calls this between stream attempts, when
/// nothing of its own is in flight. Implementations are expected to bound
/// themselves — see [`crate::device_trust::WorkspaceDeviceTrust`].
pub type SignerTrustRefresh = Arc<dyn Fn(&DeviceId) -> TrustRefreshOutcome + Send + Sync>;

/// A reconnect backoff schedule. The daemon driver injects the shared observer
/// schedule; keeping it a parameter avoids a second copy of the backoff formula
/// and lets tests drive fast reconnects.
pub type ReconnectDelay = Arc<dyn Fn(ReconnectAttempt) -> Duration + Send + Sync>;

// The ref values themselves wake this receiver immediately. This bound only
// limits how long local shutdown and websocket-state transitions wait to be
// observed; it is not a remote polling interval.
const REF_SUBSCRIPTION_DRAIN_INTERVAL: Duration = Duration::from_millis(100);
const REF_SUBSCRIPTION_SHUTDOWN_POLL: Duration = Duration::from_millis(50);
const REF_SUBSCRIPTION_FIRST_VALUE_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) type StreamEventSender = Sender<WorkspaceRefStreamEvent>;

pub(super) struct StreamAttempt {
    pub(super) shutdown: WorkspaceRefStreamShutdown,
    pub(super) worker: JoinHandle<()>,
}

pub(super) type StreamStarter =
    Box<dyn FnMut(StreamEventSender) -> std::io::Result<StreamAttempt> + Send>;

#[path = "ref_observer/frontier.rs"]
mod frontier;

pub use frontier::{
    RefObserverAuthoritySource, RefObserverEndpointGeneration, RefObserverFailure,
    RefObserverFailureCode, RefObserverFailureStage, RefObserverFrontier, RefObserverHealth,
    RefObserverHealthHandle, RefObserverLifecycleRevision, RefObserverProcessIdentity,
    RefObserverReadiness, RefObserverRemediation, RefObserverRemediationKind, RefObserverSnapshot,
    RefObserverSnapshotHandle, RefObserverState, VerifiedWorkspaceRef, VerifiedWorkspaceRefView,
};

/// Bridges the hosted workspace-ref subscription into engine wakeups. A
/// signature-verified real head received during a live subscription is carried
/// as a freshness-checked pull hint. The first value after startup or reconnect
/// remains a payload-free wakeup so the driver synchronously re-establishes
/// authority. Reconnects on stream loss with the injected backoff. Dropping the
/// handle stops the worker.
pub struct RefChangeSubscription {
    shutdown: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    snapshot: RefObserverSnapshotHandle,
}

impl RefChangeSubscription {
    /// Subscribe for one exact engine endpoint. Endpoint reconstruction creates
    /// a new subscription and handle; an old worker can therefore never publish
    /// into a replacement endpoint's frontier.
    pub(crate) fn spawn_for_endpoint(
        client: Arc<HostedControlPlaneClient>,
        workspace_id: String,
        process_identity: EngineProcessIdentity,
        events: EngineEventSender<EngineEvent>,
        reconnect_delay: ReconnectDelay,
        trust_refresh: SignerTrustRefresh,
        endpoint_generation: RefObserverEndpointGeneration,
    ) -> Self {
        let authority_source = RefObserverAuthoritySource::issue(
            process_identity,
            WorkspaceId::new(workspace_id.clone()),
            endpoint_generation,
        );
        let starter: StreamStarter = Box::new(move |stream_tx| {
            let (shutdown, cancellation) = workspace_ref_stream_shutdown_pair();
            let client = Arc::clone(&client);
            let workspace_id = workspace_id.clone();
            let terminal_tx = stream_tx.clone();
            let worker = thread::Builder::new()
                .name("bowline-manifest-ref-stream".to_string())
                .spawn(move || {
                    if let Err(error) = client.stream_workspace_ref_events_until(
                        &workspace_id,
                        stream_tx,
                        cancellation,
                    ) {
                        let _receiver_gone =
                            terminal_tx.send(WorkspaceRefStreamEvent::Ref(Err(error)));
                    }
                })?;
            Ok(StreamAttempt { shutdown, worker })
        });
        Self::spawn_with_starter_for_source(
            starter,
            events,
            reconnect_delay,
            trust_refresh,
            authority_source,
        )
    }

    #[cfg(test)]
    pub(super) fn spawn_with_starter(
        starter: StreamStarter,
        events: EngineEventSender<EngineEvent>,
        reconnect_delay: ReconnectDelay,
        trust_refresh: SignerTrustRefresh,
    ) -> Self {
        Self::spawn_with_starter_for_endpoint(
            starter,
            events,
            reconnect_delay,
            trust_refresh,
            RefObserverEndpointGeneration::new(1),
        )
    }

    #[cfg(test)]
    pub(super) fn spawn_with_starter_for_endpoint(
        starter: StreamStarter,
        events: EngineEventSender<EngineEvent>,
        reconnect_delay: ReconnectDelay,
        trust_refresh: SignerTrustRefresh,
        endpoint_generation: RefObserverEndpointGeneration,
    ) -> Self {
        Self::spawn_with_starter_for_source(
            starter,
            events,
            reconnect_delay,
            trust_refresh,
            RefObserverAuthoritySource::issue(
                EngineProcessIdentity::current(),
                WorkspaceId::new("ws_ref_observer_test"),
                endpoint_generation,
            ),
        )
    }

    fn spawn_with_starter_for_source(
        mut starter: StreamStarter,
        events: EngineEventSender<EngineEvent>,
        reconnect_delay: ReconnectDelay,
        trust_refresh: SignerTrustRefresh,
        authority_source: RefObserverAuthoritySource,
    ) -> Self {
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = Arc::clone(&shutdown);
        let snapshot = RefObserverSnapshotHandle::for_source(authority_source);
        let worker_snapshot = snapshot.clone();
        let worker = thread::Builder::new()
            .name("bowline-manifest-ref-bridge".to_string())
            .spawn(move || {
                let _lifecycle = RefObserverWorkerLifecycle {
                    snapshot: worker_snapshot.clone(),
                };
                run_ref_bridge(RefBridge {
                    starter: &mut starter,
                    events: &events,
                    reconnect_delay: &reconnect_delay,
                    trust_refresh: &trust_refresh,
                    shutdown: &worker_shutdown,
                    snapshot: &worker_snapshot,
                })
            })
            .expect("ref-change subscription bridge thread spawns");
        Self {
            shutdown,
            worker: Some(worker),
            snapshot,
        }
    }

    pub fn health_handle(&self) -> RefObserverHealthHandle {
        self.snapshot.clone()
    }

    pub fn snapshot_handle(&self) -> RefObserverSnapshotHandle {
        self.snapshot.clone()
    }

    pub fn is_finished(&self) -> bool {
        self.worker
            .as_ref()
            .is_none_or(std::thread::JoinHandle::is_finished)
    }
}

struct RefObserverWorkerLifecycle {
    snapshot: RefObserverSnapshotHandle,
}

impl Drop for RefObserverWorkerLifecycle {
    fn drop(&mut self) {
        self.snapshot.stopped();
    }
}

impl Drop for RefChangeSubscription {
    fn drop(&mut self) {
        self.snapshot.request_shutdown(&self.shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Outcome of draining one stream attempt.
pub(super) enum DrainOutcome {
    /// The driver's receiver was dropped; stop the bridge entirely.
    DriverGone,
    /// The stream ended or errored; reconnect (with backoff unless a value was
    /// seen, which resets the failure count).
    Reconnect {
        received_value: bool,
        failure: RefObserverFailure,
    },
}

/// Everything one run of the bridge borrows for its whole life. A params struct
/// because the loop, the recovery step and the backoff all need the same set.
struct RefBridge<'a> {
    starter: &'a mut StreamStarter,
    events: &'a EngineEventSender<EngineEvent>,
    reconnect_delay: &'a ReconnectDelay,
    trust_refresh: &'a SignerTrustRefresh,
    shutdown: &'a AtomicBool,
    snapshot: &'a RefObserverSnapshotHandle,
}

/// How many attempts have failed in a row, what was last written to the log, and
/// how many reopens learned trust without the subscription getting anywhere.
/// Held across attempts so a repeating failure is reported without one line per
/// reconnect for as long as the daemon runs.
#[derive(Default)]
pub(super) struct AttemptHistory {
    pub(super) failures: u32,
    pub(super) last_logged: Option<RefObserverFailure>,
    immediate_retries: u32,
}

/// How many times learning a signer may skip the backoff before the bridge
/// stops calling it progress. Trust that was just learned makes the next attempt
/// worth trying at once, but only a delivered value proves it: without this
/// bound, a resolver that keeps disagreeing with the trust handle would reopen
/// the subscription in a tight loop.
const MAX_IMMEDIATE_TRUST_RETRIES: u32 = 3;

/// What the bridge does about a failed attempt.
enum FailureRecovery {
    /// Trust for the signer was just learned, so the same subscription is worth
    /// reopening at once: the failure was local staleness, now resolved.
    RetryNow,
    /// Wait out the backoff for this failure, as reclassified by the recovery
    /// attempt.
    BackOff(RefObserverFailure),
    /// Authority, integrity, or contract failure. Time cannot repair it.
    Blocked(RefObserverFailure),
}

fn run_ref_bridge(bridge: RefBridge<'_>) {
    let RefBridge {
        starter,
        events,
        reconnect_delay,
        trust_refresh,
        shutdown,
        snapshot,
    } = bridge;
    let mut history = AttemptHistory::default();
    while !shutdown.load(Ordering::SeqCst) {
        let (stream_tx, stream_rx) = mpsc::channel();
        let attempt = match starter(stream_tx) {
            Ok(attempt) => attempt,
            Err(_error) => {
                history.failures = history.failures.saturating_add(1);
                let failure = RefObserverFailure {
                    stage: RefObserverFailureStage::Start,
                    class: DependencyFailureClass::Retryable,
                    code: RefObserverFailureCode::StartUnavailable,
                };
                if !back_off_after_failure(
                    &failure,
                    &mut history,
                    reconnect_delay,
                    shutdown,
                    snapshot,
                ) {
                    break;
                }
                continue;
            }
        };
        snapshot.connecting(history.failures);
        let outcome = drain_stream(
            &stream_rx,
            events,
            shutdown,
            snapshot,
            REF_SUBSCRIPTION_FIRST_VALUE_TIMEOUT,
        );
        drop(attempt.shutdown);
        let _ = attempt.worker.join();
        match outcome {
            DrainOutcome::DriverGone => break,
            DrainOutcome::Reconnect {
                received_value,
                failure,
            } => {
                if shutdown.load(Ordering::SeqCst) {
                    break;
                }
                match recover_from_failure(failure, trust_refresh, &history) {
                    FailureRecovery::RetryNow => {
                        history.failures = 0;
                        history.immediate_retries = history.immediate_retries.saturating_add(1);
                        snapshot.transition(RefObserverState::Connecting, 0, true, None);
                    }
                    FailureRecovery::BackOff(failure) => {
                        history.immediate_retries = 0;
                        history.failures = if received_value {
                            1
                        } else {
                            history.failures.saturating_add(1)
                        };
                        if !back_off_after_failure(
                            &failure,
                            &mut history,
                            reconnect_delay,
                            shutdown,
                            snapshot,
                        ) {
                            break;
                        }
                    }
                    FailureRecovery::Blocked(failure) => {
                        history.failures = history.failures.saturating_add(1);
                        if should_log_observer_failure(&failure, &history) {
                            log_observer_failure(&failure, history.failures);
                            history.last_logged = Some(failure.clone());
                        }
                        snapshot.blocked(failure, history.failures);
                        if !snapshot.wait_for_authority_restore(shutdown) {
                            break;
                        }
                        history = AttemptHistory::default();
                    }
                }
            }
        }
    }
}

/// Answer a failed attempt before waiting on it.
///
/// A head signed by a device this host does not know is the one failure the
/// bridge can act on rather than merely survive: enrolling a second device
/// leaves every already-running daemon holding trust that predates it. The
/// refresh happens here, between attempts, because the lookup that failed ran
/// inside the client's response parsing and must not call the client again.
fn recover_from_failure(
    failure: RefObserverFailure,
    trust_refresh: &SignerTrustRefresh,
    history: &AttemptHistory,
) -> FailureRecovery {
    let RefObserverFailureStage::UnknownSigner(device_id) = &failure.stage else {
        return if failure.class == DependencyFailureClass::Retryable {
            FailureRecovery::BackOff(failure)
        } else {
            FailureRecovery::Blocked(failure)
        };
    };
    let outcome = trust_refresh(device_id);
    if outcome.learned() {
        if history.immediate_retries >= MAX_IMMEDIATE_TRUST_RETRIES {
            return FailureRecovery::BackOff(RefObserverFailure {
                stage: failure.stage.clone(),
                class: DependencyFailureClass::Retryable,
                code: RefObserverFailureCode::UnknownSigner,
            });
        }
        eprintln!(
            "bowline-daemon reactive ref observer learned device trust for {}; resuming",
            device_id.as_str()
        );
        return FailureRecovery::RetryNow;
    }
    if outcome.refused() {
        return FailureRecovery::Blocked(RefObserverFailure {
            stage: RefObserverFailureStage::UntrustedSigner(device_id.clone()),
            class: DependencyFailureClass::AuthorizationLost,
            code: RefObserverFailureCode::AuthorizationLost,
        });
    }
    if matches!(&outcome, TrustRefreshOutcome::RateLimited) {
        return FailureRecovery::BackOff(RefObserverFailure {
            stage: failure.stage,
            class: DependencyFailureClass::Retryable,
            code: RefObserverFailureCode::UnknownSigner,
        });
    }
    if let TrustRefreshOutcome::Unavailable(error) = outcome {
        let failure = match error {
            TrustRefreshError::ControlPlane(error) => observer_failure(&error),
            TrustRefreshError::Persist(_) | TrustRefreshError::CachePoisoned { .. } => {
                RefObserverFailure {
                    stage: RefObserverFailureStage::FatalContract,
                    class: DependencyFailureClass::FatalContract,
                    code: RefObserverFailureCode::FatalContract,
                }
            }
        };
        return if failure.class == DependencyFailureClass::Retryable {
            FailureRecovery::BackOff(failure)
        } else {
            FailureRecovery::Blocked(failure)
        };
    }
    FailureRecovery::BackOff(RefObserverFailure {
        stage: failure.stage,
        class: DependencyFailureClass::Retryable,
        code: RefObserverFailureCode::UnknownSigner,
    })
}

/// Record one failed observer attempt and wait out its backoff. Returns `false`
/// when shutdown fired during the wait, which the bridge answers by stopping.
fn back_off_after_failure(
    failure: &RefObserverFailure,
    history: &mut AttemptHistory,
    reconnect_delay: &ReconnectDelay,
    shutdown: &AtomicBool,
    snapshot: &RefObserverSnapshotHandle,
) -> bool {
    snapshot.transition(
        RefObserverState::Retrying,
        history.failures,
        true,
        Some(failure.clone()),
    );
    if should_log_observer_failure(failure, history) {
        log_observer_failure(failure, history.failures);
        history.last_logged = Some(failure.clone());
    }
    let delay = reconnect_delay(ReconnectAttempt {
        consecutive_failures: history.failures,
        stage: failure.stage.clone(),
    });
    if !sleep_until_shutdown(delay, shutdown) {
        return false;
    }
    snapshot.connecting(history.failures);
    true
}

/// Report a failure the first time it is seen, again whenever it changes, and
/// then on a thinning sample of the repeats.
///
/// A condition no reconnect can clear — a signer this host is not allowed to
/// verify — otherwise writes one line per reconnect for as long as the daemon
/// runs. It still has to be visible in the log, so the repeats thin out rather
/// than stop: the gaps double with the count, which turned the 1,853 identical
/// lines this was built for into 11.
pub(super) fn should_log_observer_failure(
    failure: &RefObserverFailure,
    history: &AttemptHistory,
) -> bool {
    history
        .last_logged
        .as_ref()
        .is_none_or(|last| last != failure)
        || history.failures.is_power_of_two()
}

pub(super) fn drain_stream(
    stream_rx: &Receiver<WorkspaceRefStreamEvent>,
    events: &EngineEventSender<EngineEvent>,
    shutdown: &AtomicBool,
    snapshot: &RefObserverSnapshotHandle,
    first_value_timeout: Duration,
) -> DrainOutcome {
    let mut received_any_value = false;
    let mut received_initial_value = false;
    let mut websocket_connected = false;
    let mut value_wait_started = Instant::now();
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return DrainOutcome::DriverGone;
        }
        let drain_interval = if received_initial_value {
            REF_SUBSCRIPTION_DRAIN_INTERVAL
        } else {
            REF_SUBSCRIPTION_DRAIN_INTERVAL
                .min(first_value_timeout.saturating_sub(value_wait_started.elapsed()))
        };
        match stream_rx.recv_timeout(drain_interval) {
            Ok(WorkspaceRefStreamEvent::ConnectionState(
                WorkspaceRefStreamConnectionState::Connecting,
            )) if websocket_connected => {
                // Convex owns transport reconnection and keeps the query
                // subscription registered across it. Destroying the client here
                // fights that recovery loop and can chain first-value timeouts.
                // Mark readiness degraded until this same subscription pushes a
                // fresh value, then return to Live without polling or rebuilding.
                websocket_connected = false;
                received_initial_value = false;
                value_wait_started = Instant::now();
                snapshot.connecting(snapshot.current().consecutive_failures);
            }
            Ok(WorkspaceRefStreamEvent::ConnectionState(
                WorkspaceRefStreamConnectionState::Connecting,
            )) => snapshot.connecting(snapshot.current().consecutive_failures),
            Ok(WorkspaceRefStreamEvent::ConnectionState(
                WorkspaceRefStreamConnectionState::Connected,
            )) => websocket_connected = true,
            Ok(WorkspaceRefStreamEvent::Ref(Ok(workspace_ref))) => {
                let requires_authoritative_read = !received_initial_value;
                let (verified_ref, observation) = match verified_stream_ref(workspace_ref) {
                    Ok(verified) => verified,
                    Err(failure) => {
                        return DrainOutcome::Reconnect {
                            received_value: received_any_value,
                            failure,
                        };
                    }
                };
                // This is the observer's linearization point: liveness and the
                // initial verified authority become visible in one immutable
                // snapshot. No separate health event can be paired with a ref
                // from another endpoint or lifecycle.
                snapshot.live_with_ref(verified_ref);
                received_any_value = true;
                received_initial_value = true;
                let event = if requires_authoritative_read {
                    EngineEvent::RefChanged
                } else {
                    observation.map_or(EngineEvent::RefChanged, EngineEvent::RefObserved)
                };
                if events.send(event).is_err() {
                    return DrainOutcome::DriverGone;
                }
            }
            Ok(WorkspaceRefStreamEvent::Ref(Err(error))) => {
                return DrainOutcome::Reconnect {
                    received_value: received_any_value,
                    failure: observer_failure(&error),
                };
            }
            Err(RecvTimeoutError::Timeout) => {
                if !received_initial_value && value_wait_started.elapsed() >= first_value_timeout {
                    return DrainOutcome::Reconnect {
                        received_value: received_any_value,
                        failure: RefObserverFailure {
                            stage: RefObserverFailureStage::InitialValue,
                            class: DependencyFailureClass::Retryable,
                            code: RefObserverFailureCode::InitialValueTimeout,
                        },
                    };
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                return DrainOutcome::Reconnect {
                    received_value: received_any_value,
                    failure: RefObserverFailure {
                        stage: RefObserverFailureStage::Stream,
                        class: DependencyFailureClass::Retryable,
                        code: RefObserverFailureCode::StreamUnavailable,
                    },
                };
            }
        }
    }
}

/// The control plane has already replaced the account session once before it
/// surfaces `AuthExpired` here, so this is a refusal of the identity rather than
/// a stream fault. The common dependency class is the single classification
/// point; this adapter adds only the observer-specific signer-refresh stage.
fn observer_failure(error: &ControlPlaneError) -> RefObserverFailure {
    if let Some((_, device_id)) = error.unknown_signing_device() {
        return RefObserverFailure {
            stage: RefObserverFailureStage::UnknownSigner(device_id.clone()),
            // The bridge performs one explicit authority refresh before this
            // class becomes terminal or retryable.
            class: DependencyFailureClass::AuthorizationLost,
            code: RefObserverFailureCode::UnknownSigner,
        };
    }
    let class = error.dependency_failure_class();
    let (stage, code) = match class {
        DependencyFailureClass::Retryable => (
            RefObserverFailureStage::Stream,
            RefObserverFailureCode::StreamUnavailable,
        ),
        DependencyFailureClass::AuthenticationRequired => (
            RefObserverFailureStage::Authentication,
            RefObserverFailureCode::AuthenticationRequired,
        ),
        DependencyFailureClass::AuthorizationLost => (
            RefObserverFailureStage::Authorization,
            RefObserverFailureCode::AuthorizationLost,
        ),
        DependencyFailureClass::Integrity => (
            RefObserverFailureStage::Integrity,
            RefObserverFailureCode::Integrity,
        ),
        DependencyFailureClass::FatalContract => (
            RefObserverFailureStage::FatalContract,
            RefObserverFailureCode::FatalContract,
        ),
    };
    RefObserverFailure { stage, class, code }
}

fn verified_stream_ref(
    workspace_ref: Option<bowline_control_plane::WorkspaceRef>,
) -> Result<(VerifiedWorkspaceRef, Option<RefObservation>), RefObserverFailure> {
    let Some(workspace_ref) = workspace_ref else {
        return Ok((VerifiedWorkspaceRef::genesis(), None));
    };
    if workspace_ref.version == 0 && workspace_ref.snapshot_id.is_none() {
        return Ok((VerifiedWorkspaceRef::genesis(), None));
    }
    let observation = head_observation(workspace_ref).ok_or(RefObserverFailure {
        stage: RefObserverFailureStage::Integrity,
        class: DependencyFailureClass::Integrity,
        code: RefObserverFailureCode::Integrity,
    })?;
    Ok((
        VerifiedWorkspaceRef::from_observation(observation.clone()),
        Some(observation),
    ))
}

fn log_observer_failure(failure: &RefObserverFailure, consecutive_failures: u32) {
    eprintln!(
        "bowline-daemon reactive ref observer {:?} failure #{consecutive_failures} ({:?})",
        failure.stage, failure.code
    );
}

/// Sleep for `delay`, waking early if shutdown is requested. Returns `false` when
/// shutdown fired (the caller must stop).
fn sleep_until_shutdown(delay: Duration, shutdown: &AtomicBool) -> bool {
    let deadline = Instant::now() + delay;
    while Instant::now() < deadline {
        if shutdown.load(Ordering::SeqCst) {
            return false;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        thread::sleep(REF_SUBSCRIPTION_SHUTDOWN_POLL.min(remaining));
    }
    !shutdown.load(Ordering::SeqCst)
}
