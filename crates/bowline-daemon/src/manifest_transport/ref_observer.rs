//! The reactive workspace-ref observer.
//!
//! Bridges the hosted ref subscription into engine wakeups and owns everything
//! that decides how a failed attempt is answered: the lifecycle a status
//! surface reads, the reconnect schedule's inputs, and the trust refresh that
//! lets a running daemon learn a device trusted after it started.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use bowline_control_plane::{
    ControlPlaneError, HostedControlPlaneClient, Retryability, WorkspaceRefStreamConnectionState,
    WorkspaceRefStreamEvent, WorkspaceRefStreamShutdown, workspace_ref_stream_shutdown_pair,
};
use bowline_core::ids::DeviceId;
use bowline_local::sync::manifest_engine::EngineEvent;

use super::head_observation;
use crate::device_trust::TrustRefreshOutcome;

/// What the reconnect schedule gets to reason about. The stage is part of it
/// because a refused credential and a dropped websocket deserve different
/// patience: reopening recovers the second on its own, and only burns another
/// account-session registration on the first.
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

/// Lifecycle of the reactive hosted-ref observer. `Live` is reached only after
/// Convex has delivered the subscription's initial value; owning a worker thread
/// is not sufficient evidence that remote changes can reach the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefObserverState {
    Connecting,
    Live,
    Retrying,
    Stopped,
}

/// The operation that ended the most recent observer attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefObserverFailureStage {
    Start,
    InitialValue,
    Stream,
    /// The control plane refused this device's account credentials. Distinct
    /// from `Stream` because reconnecting alone cannot clear it: the client has
    /// already replaced the session once and been refused again.
    Authentication,
    /// The workspace head was signed by a device this host has not learned yet.
    /// The named device is what the bridge refreshes trust for; the ordinary
    /// case is a second device that enrolled while this daemon was running.
    UnknownSigner(DeviceId),
    /// The workspace head was signed by a device the control plane does not
    /// authorize in this workspace. Distinct from `UnknownSigner` because the
    /// question has been asked and answered: no reconnect and no further trust
    /// read can make that head verifiable here.
    UntrustedSigner(DeviceId),
}

/// Which observer condition the status surface should report. `Live` is the only
/// healthy answer. `Unauthenticated` is separated from `Retrying` because it is
/// the one condition a reconnect cannot clear, and a daemon that silently stops
/// receiving remote heads is the failure this whole path exists to prevent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefObserverReadiness {
    Live,
    Retrying,
    /// The remote head is signed by a device this host is not allowed to trust.
    /// Like `Unauthenticated`, retrying cannot clear it; unlike it, this host's
    /// own credentials are fine and the workspace's device trust is what has to
    /// change.
    UntrustedSigner,
    Unauthenticated,
}

/// Structured failure retained for diagnostics and rate-limited logging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefObserverFailure {
    pub stage: RefObserverFailureStage,
    pub message: String,
}

/// Current observer health. The revision changes on every lifecycle transition
/// so connection loss is visible even while the engine remains idle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefObserverHealth {
    pub revision: u64,
    pub state: RefObserverState,
    pub consecutive_failures: u32,
    pub reconnects: u64,
    pub last_failure: Option<RefObserverFailure>,
}

impl RefObserverHealth {
    /// The last failure outlives the retry that follows it, so a credential
    /// refusal stays reported while the bridge is between attempts instead of
    /// flickering back to a neutral "connecting" every backoff cycle.
    pub fn readiness(&self) -> RefObserverReadiness {
        if self.state == RefObserverState::Live {
            return RefObserverReadiness::Live;
        }
        match self.last_failure.as_ref().map(|failure| &failure.stage) {
            Some(RefObserverFailureStage::Authentication) => RefObserverReadiness::Unauthenticated,
            Some(RefObserverFailureStage::UntrustedSigner(_)) => {
                RefObserverReadiness::UntrustedSigner
            }
            Some(
                RefObserverFailureStage::Start
                | RefObserverFailureStage::InitialValue
                | RefObserverFailureStage::Stream
                // Not yet answered: the bridge is still trying to learn this
                // signer, which a retry can complete.
                | RefObserverFailureStage::UnknownSigner(_),
            )
            | None => RefObserverReadiness::Retrying,
        }
    }
}

impl Default for RefObserverHealth {
    fn default() -> Self {
        Self {
            revision: 0,
            state: RefObserverState::Connecting,
            consecutive_failures: 0,
            reconnects: 0,
            last_failure: None,
        }
    }
}

/// Cloneable, lock-bounded view of the observer lifecycle.
#[derive(Clone, Debug)]
pub struct RefObserverHealthHandle(Arc<Mutex<RefObserverHealth>>);

impl RefObserverHealthHandle {
    pub(super) fn new() -> Self {
        Self(Arc::new(Mutex::new(RefObserverHealth::default())))
    }

    pub fn current(&self) -> RefObserverHealth {
        self.0
            .lock()
            .map(|health| health.clone())
            .unwrap_or_default()
    }

    pub fn readiness(&self) -> RefObserverReadiness {
        self.current().readiness()
    }

    pub(super) fn connecting(&self, consecutive_failures: u32) {
        let last_failure = self.current().last_failure;
        self.transition(
            RefObserverState::Connecting,
            consecutive_failures,
            false,
            last_failure,
        );
    }

    pub(super) fn transition(
        &self,
        state: RefObserverState,
        consecutive_failures: u32,
        reconnect: bool,
        last_failure: Option<RefObserverFailure>,
    ) {
        if let Ok(mut health) = self.0.lock() {
            health.revision = health.revision.saturating_add(1);
            health.state = state;
            health.consecutive_failures = consecutive_failures;
            health.reconnects = health.reconnects.saturating_add(u64::from(reconnect));
            health.last_failure = last_failure;
        }
    }
}

/// Bridges the hosted workspace-ref subscription into engine wakeups. A
/// signature-verified real head received during a live subscription is carried
/// as a freshness-checked pull hint. The first value after startup or reconnect
/// remains a payload-free wakeup so the driver synchronously re-establishes
/// authority. Reconnects on stream loss with the injected backoff. Dropping the
/// handle stops the worker.
pub struct RefChangeSubscription {
    shutdown: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    health: RefObserverHealthHandle,
}

impl RefChangeSubscription {
    /// Subscribe over a hosted control-plane client.
    pub fn spawn(
        client: Arc<HostedControlPlaneClient>,
        workspace_id: String,
        events: Sender<EngineEvent>,
        reconnect_delay: ReconnectDelay,
        trust_refresh: SignerTrustRefresh,
    ) -> Self {
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
        Self::spawn_with_starter(starter, events, reconnect_delay, trust_refresh)
    }

    pub(super) fn spawn_with_starter(
        mut starter: StreamStarter,
        events: Sender<EngineEvent>,
        reconnect_delay: ReconnectDelay,
        trust_refresh: SignerTrustRefresh,
    ) -> Self {
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = Arc::clone(&shutdown);
        let health = RefObserverHealthHandle::new();
        let worker_health = health.clone();
        let worker = thread::Builder::new()
            .name("bowline-manifest-ref-bridge".to_string())
            .spawn(move || {
                run_ref_bridge(RefBridge {
                    starter: &mut starter,
                    events: &events,
                    reconnect_delay: &reconnect_delay,
                    trust_refresh: &trust_refresh,
                    shutdown: &worker_shutdown,
                    health: &worker_health,
                })
            })
            .expect("ref-change subscription bridge thread spawns");
        Self {
            shutdown,
            worker: Some(worker),
            health,
        }
    }

    pub fn health_handle(&self) -> RefObserverHealthHandle {
        self.health.clone()
    }

    pub fn is_finished(&self) -> bool {
        self.worker
            .as_ref()
            .is_none_or(std::thread::JoinHandle::is_finished)
    }
}

impl Drop for RefChangeSubscription {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        self.health
            .transition(RefObserverState::Stopped, 0, false, None);
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
    events: &'a Sender<EngineEvent>,
    reconnect_delay: &'a ReconnectDelay,
    trust_refresh: &'a SignerTrustRefresh,
    shutdown: &'a AtomicBool,
    health: &'a RefObserverHealthHandle,
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
}

fn run_ref_bridge(bridge: RefBridge<'_>) {
    let RefBridge {
        starter,
        events,
        reconnect_delay,
        trust_refresh,
        shutdown,
        health,
    } = bridge;
    let mut history = AttemptHistory::default();
    while !shutdown.load(Ordering::SeqCst) {
        let (stream_tx, stream_rx) = mpsc::channel();
        let attempt = match starter(stream_tx) {
            Ok(attempt) => attempt,
            Err(error) => {
                history.failures = history.failures.saturating_add(1);
                let failure = RefObserverFailure {
                    stage: RefObserverFailureStage::Start,
                    message: error.to_string(),
                };
                if !back_off_after_failure(
                    &failure,
                    &mut history,
                    reconnect_delay,
                    shutdown,
                    health,
                ) {
                    break;
                }
                continue;
            }
        };
        health.connecting(history.failures);
        let outcome = drain_stream(
            &stream_rx,
            events,
            shutdown,
            health,
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
                        health.transition(RefObserverState::Connecting, 0, true, None);
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
                            health,
                        ) {
                            break;
                        }
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
        return FailureRecovery::BackOff(failure);
    };
    let outcome = trust_refresh(device_id);
    if outcome.learned() {
        if history.immediate_retries >= MAX_IMMEDIATE_TRUST_RETRIES {
            return FailureRecovery::BackOff(RefObserverFailure {
                stage: failure.stage.clone(),
                message: format!(
                    "{}: trust for this device is installed and the head is still unverifiable",
                    failure.message
                ),
            });
        }
        eprintln!(
            "bowline-daemon reactive ref observer learned device trust for {}; resuming",
            device_id.as_str()
        );
        return FailureRecovery::RetryNow;
    }
    let stage = if outcome.refused() {
        RefObserverFailureStage::UntrustedSigner(device_id.clone())
    } else {
        failure.stage.clone()
    };
    FailureRecovery::BackOff(RefObserverFailure {
        stage,
        message: format!("{}: {outcome}", failure.message),
    })
}

/// Record one failed observer attempt and wait out its backoff. Returns `false`
/// when shutdown fired during the wait, which the bridge answers by stopping.
fn back_off_after_failure(
    failure: &RefObserverFailure,
    history: &mut AttemptHistory,
    reconnect_delay: &ReconnectDelay,
    shutdown: &AtomicBool,
    health: &RefObserverHealthHandle,
) -> bool {
    health.transition(
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
    health.connecting(history.failures);
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
    events: &Sender<EngineEvent>,
    shutdown: &AtomicBool,
    health: &RefObserverHealthHandle,
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
                health.connecting(health.current().consecutive_failures);
            }
            Ok(WorkspaceRefStreamEvent::ConnectionState(
                WorkspaceRefStreamConnectionState::Connecting,
            )) => health.connecting(health.current().consecutive_failures),
            Ok(WorkspaceRefStreamEvent::ConnectionState(
                WorkspaceRefStreamConnectionState::Connected,
            )) => websocket_connected = true,
            Ok(WorkspaceRefStreamEvent::Ref(Ok(workspace_ref))) => {
                let requires_authoritative_read = !received_initial_value;
                if !received_initial_value {
                    health.transition(RefObserverState::Live, 0, false, None);
                }
                received_any_value = true;
                received_initial_value = true;
                let event = if requires_authoritative_read {
                    EngineEvent::RefChanged
                } else {
                    workspace_ref
                        .and_then(head_observation)
                        .map_or(EngineEvent::RefChanged, EngineEvent::RefObserved)
                };
                if events.send(event).is_err() {
                    return DrainOutcome::DriverGone;
                }
            }
            Ok(WorkspaceRefStreamEvent::Ref(Err(error))) => {
                return DrainOutcome::Reconnect {
                    received_value: received_any_value,
                    failure: RefObserverFailure {
                        stage: observer_failure_stage(&error),
                        message: error.to_string(),
                    },
                };
            }
            Err(RecvTimeoutError::Timeout) => {
                if !received_initial_value && value_wait_started.elapsed() >= first_value_timeout {
                    return DrainOutcome::Reconnect {
                        received_value: received_any_value,
                        failure: RefObserverFailure {
                            stage: RefObserverFailureStage::InitialValue,
                            message: format!(
                                "no initial subscription value within {:?}",
                                first_value_timeout
                            ),
                        },
                    };
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                return DrainOutcome::Reconnect {
                    received_value: received_any_value,
                    failure: RefObserverFailure {
                        stage: RefObserverFailureStage::Stream,
                        message: "workspace-ref subscription ended".to_string(),
                    },
                };
            }
        }
    }
}

/// The control plane has already replaced the account session once before it
/// surfaces `AuthExpired` here, so this is a refusal of the identity rather than
/// a stream fault. Classified through `retryability` because that is the single
/// classification point for control-plane failures.
fn observer_failure_stage(error: &ControlPlaneError) -> RefObserverFailureStage {
    match error.retryability() {
        Retryability::AuthExpired => RefObserverFailureStage::Authentication,
        Retryability::TrustRefreshRequired => error
            .unknown_signing_device()
            .map_or(RefObserverFailureStage::Stream, |(_, device_id)| {
                RefObserverFailureStage::UnknownSigner(device_id.clone())
            }),
        Retryability::Retryable | Retryability::Fatal => RefObserverFailureStage::Stream,
    }
}

fn log_observer_failure(failure: &RefObserverFailure, consecutive_failures: u32) {
    eprintln!(
        "bowline-daemon reactive ref observer {:?} failure #{consecutive_failures}: {}",
        failure.stage, failure.message
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
