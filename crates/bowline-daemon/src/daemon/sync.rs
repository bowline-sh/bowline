use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bowline_core::ids::{DeviceId, WorkspaceId};
use bowline_local::notifications::{
    DesktopNotificationSender, NotificationDedupe, NotificationDispatchReport, NotificationSender,
    dispatch_new_notifications_with_checkpoint, pending_device_payloads,
};
use time::OffsetDateTime;

use bowline_daemon::manifest_transport::{
    ReconnectAttempt, RefObserverFailureStage, RefObserverReadiness,
};

use crate::daemon::{
    HostedContext, HostedContextCache, HostedContextResolver, NOTIFICATION_POLL_INTERVAL,
    REMOTE_OBSERVER_REAUTH_RECONNECT_MAX, REMOTE_OBSERVER_RECONNECT_INITIAL,
    REMOTE_OBSERVER_RECONNECT_MAX, REMOTE_OBSERVER_UNTRUSTED_SIGNER_RECONNECT_MAX,
    StatusPublishOutcome, StatusPublishPayload, StatusPublisher, hosted_context_resolver,
    hosted_status_publisher_with_context,
};

mod notification_status;
mod policy_cache;
mod status_publish;
mod watcher_host;
mod workspace_key;

pub(in crate::daemon) use policy_cache::{drain_policy, invalidate_policy_cache_for_path};
pub(in crate::daemon) use watcher_host::WatcherHost;
pub(in crate::daemon) use workspace_key::require_local_workspace_key;

/// The workspace identity and filesystem locations a daemon serves. Every
/// engine-facing surface (driver build, status publish, work-view RPCs) reads
/// from this one struct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SyncArgs {
    pub(super) root: PathBuf,
    pub(super) state_root: PathBuf,
    pub(super) workspace_id: WorkspaceId,
    pub(super) device_id: DeviceId,
}

pub(super) struct DaemonRuntime {
    pub(super) sync: Option<ContinuousSyncRuntime>,
    pub(super) notify_approvals: bool,
    pub(super) notification_dedupe: Arc<Mutex<NotificationDedupe>>,
    pub(super) next_notification_poll: Instant,
    pub(super) pending_notification_status: Option<bowline_core::commands::StatusCommandOutput>,
}

pub(super) struct PreparedNotificationPoll {
    status: bowline_core::commands::StatusCommandOutput,
    dedupe: Arc<Mutex<NotificationDedupe>>,
}

pub(super) struct NotificationPollCompletion {
    status: bowline_core::commands::StatusCommandOutput,
    result: Result<NotificationDispatchReport, String>,
}

pub(super) struct PreparedStatusPublish {
    payload: StatusPublishPayload,
    published_at: Instant,
    publisher: StatusPublisher,
}

pub(super) struct StatusPublishCompletion {
    published_at: Instant,
    result: Result<StatusPublishOutcome, String>,
}

/// The per-workspace sync runtime. The manifest engine owns sync end to end:
/// this host carries the engine driver (or its pending rebuild), the watcher
/// kernel feeding it, and the hosted status publisher.
pub(super) struct ContinuousSyncRuntime {
    pub(super) args: SyncArgs,
    /// The filesystem watcher kernel. A watcher that fails to arm is a
    /// degradation with a retry deadline, never a silent absence — nothing else
    /// in the engine detects local edits.
    pub(super) watcher: WatcherHost,
    pub(super) status_publisher: StatusPublisher,
    pub(super) next_status_publish: Instant,
    pub(super) last_status_publish_fingerprint: Option<String>,
    pub(super) last_status_publish_at: Option<Instant>,
    pub(super) last_status_publish_failed_at: Option<Instant>,
    pub(super) hosted_resolver: HostedContextResolver,
    pub(super) claimant_id: String,
    /// The manifest-sync engine host. Construction leaves it `PendingRebuild`
    /// (surfaced as `limited` status); the scheduler's first engine drive
    /// performs the real build after the control socket is available. There is
    /// no other sync engine to fall back to.
    pub(super) manifest_engine: ManifestEngineHost,
    /// The persistent status slot for this workspace's engine. A driver built at
    /// startup or later publishes into it; while the driver is pending, the daemon
    /// publishes a `limited` host-status snapshot into it. Owning it here (not
    /// inside the driver) is what lets the status projection observe a late-built
    /// driver without being rebuilt.
    pub(super) manifest_snapshot: (
        bowline_daemon::manifest_driver::EngineSnapshotSink,
        bowline_daemon::manifest_driver::EngineSnapshotHandle,
    ),
    /// The persistent engine cost meters for this workspace (Plan 111 Step 5).
    /// Owned here, not inside the driver, so counts accumulate across driver
    /// rebuilds and the daemon metrics RPC reads a stable handle. Threaded into
    /// each rebuilt engine's `EngineContext`.
    pub(super) manifest_counters:
        std::sync::Arc<bowline_local::sync::manifest_engine::EngineCounters>,
}

/// The daemon's ownership state for a workspace's sync engine. Both variants
/// mean "the manifest engine owns sync": `Active` is a running driver,
/// `PendingRebuild` retries the build on a capped backoff while status shows
/// `limited`.
pub(super) enum ManifestEngineHost {
    /// The engine driver is built and running; its snapshot feeds status.
    Active(bowline_daemon::manifest_driver::ManifestDriver),
    /// The driver could not be built yet (missing workspace key or hosted
    /// context, or a spawn failure). The daemon retries on a capped backoff and
    /// surfaces `limited` status meanwhile. The specific unavailability reason is
    /// logged at each failed attempt; it is not stored because every reason
    /// collapses to the same `limited` status.
    PendingRebuild {
        next_attempt: Instant,
        backoff: Option<Duration>,
    },
}

/// Why the manifest engine driver could not be built. Logged verbatim; the status
/// projection collapses every variant to a single `limited` convergence state
/// (there is no separate wire reason code per prerequisite, by design).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ManifestEngineUnavailableReason {
    WorkspaceKeyUnavailable,
    HostedContextUnavailable,
    DriverStartFailed,
}

impl ManifestEngineUnavailableReason {
    fn as_log(self) -> &'static str {
        match self {
            Self::WorkspaceKeyUnavailable => "workspace key unavailable",
            Self::HostedContextUnavailable => "hosted context unavailable",
            Self::DriverStartFailed => "driver failed to start",
        }
    }
}

/// First retry delay after a failed build. Short so a transiently-missing key or
/// hosted context (common right after first-run trust) recovers within seconds.
const MANIFEST_ENGINE_RETRY_INITIAL: Duration = Duration::from_secs(1);
/// Backoff ceiling: past this the daemon retries once per this interval forever.
const MANIFEST_ENGINE_RETRY_MAX: Duration = Duration::from_secs(30);

/// How far the exponential climb runs before the per-stage ceiling takes over.
/// 250ms << 8 clears the re-authentication ceiling, so both stages reach their
/// own ceiling from one curve rather than two hand-tuned schedules.
const REMOTE_OBSERVER_RECONNECT_EXPONENT_CAP: u32 = 8;

/// Shared reconnect backoff for the manifest transport's ref subscription.
pub(in crate::daemon) fn remote_observer_reconnect_delay(attempt: ReconnectAttempt) -> Duration {
    let ceiling = match attempt.stage {
        RefObserverFailureStage::Authentication => REMOTE_OBSERVER_REAUTH_RECONNECT_MAX,
        // A head this host is not allowed to verify does not heal on the
        // transport schedule either: the workspace's device trust has to change
        // first, and the observer re-reads that trust on its own bounded
        // interval rather than on this one.
        RefObserverFailureStage::UntrustedSigner(_) => {
            REMOTE_OBSERVER_UNTRUSTED_SIGNER_RECONNECT_MAX
        }
        RefObserverFailureStage::Start
        | RefObserverFailureStage::InitialValue
        | RefObserverFailureStage::Stream
        // Still learning: the next attempt is expected to succeed as soon as the
        // trust read behind it lands, so this keeps the transport schedule.
        | RefObserverFailureStage::UnknownSigner(_) => REMOTE_OBSERVER_RECONNECT_MAX,
    };
    let exponent = attempt
        .consecutive_failures
        .saturating_sub(1)
        .min(REMOTE_OBSERVER_RECONNECT_EXPONENT_CAP);
    (REMOTE_OBSERVER_RECONNECT_INITIAL * (1_u32 << exponent)).min(ceiling)
}

impl DaemonRuntime {
    /// Retry building the manifest driver if one is pending and due. Returns
    /// `true` when this call brought the driver up (so the caller starts the
    /// watcher bridge).
    pub(super) fn retry_manifest_engine(&mut self, now: Instant) -> bool {
        self.sync
            .as_mut()
            .is_some_and(|sync| sync.retry_manifest_engine(now))
    }

    /// A sender for feeding watcher-derived engine events, or `None` when no
    /// manifest driver is running.
    pub(super) fn manifest_event_sender(
        &self,
    ) -> Option<std::sync::mpsc::Sender<bowline_local::sync::manifest_engine::EngineEvent>> {
        self.sync
            .as_ref()
            .and_then(ContinuousSyncRuntime::manifest_event_sender)
    }

    pub(super) fn prepare_projection_status(
        &mut self,
        projection: &bowline_daemon::status_projection::DaemonStatusProjection,
        heartbeat: bool,
        now: Instant,
        projection_input: &bowline_daemon::status_projection::StatusProjectionInput,
    ) -> Option<PreparedStatusPublish> {
        self.sync
            .as_mut()?
            .prepare_projection_status(projection, heartbeat, now, projection_input)
    }

    pub(super) fn prepare_projection_status_retry_if_due(
        &mut self,
        projection: &bowline_daemon::status_projection::DaemonStatusProjection,
        now: Instant,
        projection_input: &bowline_daemon::status_projection::StatusProjectionInput,
    ) -> Option<PreparedStatusPublish> {
        self.sync.as_mut()?.prepare_projection_status_retry_if_due(
            projection,
            now,
            projection_input,
        )
    }

    pub(super) fn complete_status_publish(
        &mut self,
        completion: StatusPublishCompletion,
        projection_input: &bowline_daemon::status_projection::StatusProjectionInput,
    ) {
        if let Some(sync) = self.sync.as_mut() {
            sync.complete_status_publish(completion, projection_input);
        }
    }

    pub(super) fn prepare_notification_poll(&mut self) -> Option<PreparedNotificationPoll> {
        if !self.notify_approvals {
            return None;
        }
        let now = Instant::now();
        if now < self.next_notification_poll {
            return None;
        }
        let status = self.pending_notification_status.as_ref()?.clone();
        self.next_notification_poll = now + NOTIFICATION_POLL_INTERVAL;
        Some(PreparedNotificationPoll {
            status,
            dedupe: Arc::clone(&self.notification_dedupe),
        })
    }
}

impl PreparedNotificationPoll {
    #[cfg(test)]
    pub(super) fn execute(self) -> NotificationPollCompletion {
        self.execute_cancellable(|| true)
    }

    pub(super) fn execute_cancellable(
        self,
        checkpoint: impl FnMut() -> bool,
    ) -> NotificationPollCompletion {
        let sender = DesktopNotificationSender;
        let result = self.execute_with(&sender, checkpoint);
        NotificationPollCompletion {
            status: self.status,
            result,
        }
    }

    fn execute_with<S>(
        &self,
        sender: &S,
        checkpoint: impl FnMut() -> bool,
    ) -> Result<NotificationDispatchReport, String>
    where
        S: NotificationSender,
    {
        let payloads = pending_device_payloads(&self.status);
        let mut dedupe = self
            .dedupe
            .lock()
            .map_err(|_| "notification dedupe state is unavailable".to_string())?;
        Ok(dispatch_new_notifications_with_checkpoint(
            &payloads,
            &mut dedupe,
            sender,
            checkpoint,
        ))
    }
}

impl ContinuousSyncRuntime {
    pub(super) fn new(args: SyncArgs) -> Self {
        let claimant_id = format!(
            "daemon-{}-{}",
            std::process::id(),
            OffsetDateTime::now_utc().unix_timestamp_nanos()
        );
        let now = Instant::now();
        let mut watcher = WatcherHost::unarmed(now);
        // A failure here logs and schedules a retry; it never leaves the daemon
        // silently blind. The scheduler re-arms on every engine drive.
        watcher.ensure_armed(&args.root, now);
        let hosted_context = Arc::new(HostedContextCache::new());
        let hosted_resolver = hosted_context_resolver(hosted_context);
        let manifest_snapshot = bowline_daemon::manifest_driver::shared_engine_snapshot();
        // Publish the `limited` host-status snapshot immediately so status
        // consumers see a truthful degradation until the driver is built.
        manifest_snapshot
            .0
            .publish(bowline_daemon::manifest_driver::host_status_snapshot());
        Self {
            claimant_id,
            args,
            watcher,
            status_publisher: hosted_status_publisher_with_context(hosted_resolver.clone()),
            next_status_publish: Instant::now(),
            last_status_publish_fingerprint: None,
            last_status_publish_at: None,
            last_status_publish_failed_at: None,
            hosted_resolver,
            manifest_engine: ManifestEngineHost::PendingRebuild {
                next_attempt: Instant::now(),
                // No build has failed yet. If the immediate background attempt
                // fails, it receives the configured initial retry delay.
                backoff: None,
            },
            manifest_snapshot,
            manifest_counters: bowline_local::sync::manifest_engine::EngineCounters::shared(),
        }
    }

    /// A shared handle to this workspace's engine cost meters, for the daemon
    /// metrics RPC. Stable across driver rebuilds.
    pub(super) fn manifest_counters(
        &self,
    ) -> std::sync::Arc<bowline_local::sync::manifest_engine::EngineCounters> {
        std::sync::Arc::clone(&self.manifest_counters)
    }

    /// Retry building the driver if it is not yet built and the backoff deadline
    /// has passed. Returns `true` only when this call transitioned the host to
    /// `Active` (so the caller can start the watcher bridge). A still-failing
    /// build leaves the workspace `limited`.
    pub(super) fn retry_manifest_engine(&mut self, now: Instant) -> bool {
        self.demote_dead_manifest_thread(now);
        let ManifestEngineHost::PendingRebuild {
            next_attempt,
            backoff,
        } = &self.manifest_engine
        else {
            return false;
        };
        if now < *next_attempt {
            return false;
        }
        let next_backoff = backoff.map_or(MANIFEST_ENGINE_RETRY_INITIAL, |delay| {
            (delay * 2).min(MANIFEST_ENGINE_RETRY_MAX)
        });
        let (sink, handle) = &self.manifest_snapshot;
        match build_manifest_driver(
            &self.args,
            &self.hosted_resolver,
            sink.clone(),
            handle.clone(),
            self.manifest_counters.clone(),
        ) {
            Ok(driver) => {
                self.manifest_engine = ManifestEngineHost::Active(driver);
                true
            }
            Err(reason) => {
                eprintln!(
                    "bowline-daemon manifest engine still unavailable ({}); retrying in {:?}",
                    reason.as_log(),
                    next_backoff
                );
                self.manifest_engine = ManifestEngineHost::PendingRebuild {
                    next_attempt: now + next_backoff,
                    backoff: Some(next_backoff),
                };
                false
            }
        }
    }

    /// Demote an `Active` host whose engine thread has exited (a panic or an
    /// unexpected loop return) to `PendingRebuild`, so the retry path rebuilds it
    /// rather than leaving a dead driver wired to status and events. Publishes the
    /// `limited` host-status snapshot like a failed build; `next_attempt = now` so
    /// the very next retry rebuilds.
    fn demote_dead_manifest_thread(&mut self, now: Instant) {
        if let ManifestEngineHost::Active(driver) = &self.manifest_engine
            && driver.has_finished_required_worker()
        {
            eprintln!("bowline-daemon manifest sync worker exited unexpectedly; rebuilding");
            self.manifest_snapshot
                .0
                .publish(bowline_daemon::manifest_driver::host_status_snapshot());
            self.manifest_engine = ManifestEngineHost::PendingRebuild {
                next_attempt: now,
                backoff: None,
            };
        }
    }

    /// Drive the host into `PendingRebuild` as a failed build would, publishing the
    /// `limited` host-status snapshot — without the real build I/O. Used by tests
    /// to exercise the rebuild status path deterministically.
    /// Install an `Active` host whose engine thread has already exited, so tests
    /// can exercise the dead-thread demotion path deterministically.
    #[cfg(test)]
    pub(in crate::daemon) fn simulate_active_manifest_engine_with_exited_thread(&mut self) {
        let driver = bowline_daemon::manifest_driver::ManifestDriver::spawn(|_inbox, _sink| {})
            .expect("spawn stub driver");
        // The stub body returns immediately; wait for the thread to actually exit
        // so `is_thread_finished` observes the dead thread.
        while !driver.is_thread_finished() {
            std::thread::yield_now();
        }
        self.manifest_engine = ManifestEngineHost::Active(driver);
    }

    #[cfg(test)]
    pub(in crate::daemon) fn simulate_manifest_engine_unavailable(&mut self) {
        self.manifest_snapshot
            .0
            .publish(bowline_daemon::manifest_driver::host_status_snapshot());
        self.manifest_engine = ManifestEngineHost::PendingRebuild {
            next_attempt: Instant::now() + MANIFEST_ENGINE_RETRY_INITIAL,
            backoff: Some(MANIFEST_ENGINE_RETRY_INITIAL),
        };
    }

    /// Arm the watcher kernel if it is down and its backoff has elapsed. Returns
    /// `true` when signals are available for a bridge to consume.
    pub(super) fn ensure_watcher_armed(&mut self, now: Instant) -> bool {
        self.watcher.ensure_armed(&self.args.root, now)
    }

    /// The earliest instant at which any degraded part of this runtime wants to
    /// be retried, so the coordinator wakes for it while otherwise idle. The
    /// watcher and the manifest driver both retry forever on capped backoff.
    pub(super) fn next_runtime_retry(&self) -> Option<Instant> {
        match (self.next_manifest_retry(), self.watcher.next_retry()) {
            (Some(engine), Some(watcher)) => Some(engine.min(watcher)),
            (retry, None) | (None, retry) => retry,
        }
    }

    /// When the driver is waiting to rebuild, the instant of the next attempt, so
    /// the coordinator can wake the loop even while otherwise idle.
    pub(super) fn next_manifest_retry(&self) -> Option<Instant> {
        match &self.manifest_engine {
            ManifestEngineHost::PendingRebuild { next_attempt, .. } => Some(*next_attempt),
            // A dead engine thread is due for an immediate rebuild, so the loop
            // wakes even while otherwise idle.
            ManifestEngineHost::Active(driver) if driver.has_finished_required_worker() => {
                Some(Instant::now())
            }
            ManifestEngineHost::Active(_) => None,
        }
    }

    /// The persistent engine snapshot handle for the status projection: live
    /// engine snapshots when the driver is `Active`, a `limited` host-status
    /// snapshot while it is `PendingRebuild`.
    pub(super) fn manifest_snapshot_handle(
        &self,
    ) -> bowline_daemon::manifest_driver::EngineSnapshotHandle {
        self.manifest_snapshot.1.clone()
    }

    /// A sender for feeding watcher-derived engine events, or `None` when no driver
    /// is running.
    pub(super) fn manifest_event_sender(
        &self,
    ) -> Option<std::sync::mpsc::Sender<bowline_local::sync::manifest_engine::EngineEvent>> {
        match &self.manifest_engine {
            ManifestEngineHost::Active(driver) => Some(driver.event_sender()),
            ManifestEngineHost::PendingRebuild { .. } => None,
        }
    }

    /// What the hosted ref subscription currently contributes to status. A
    /// running engine thread alone is not remote observer readiness: `Live`
    /// requires a delivered initial reactive value.
    pub(super) fn manifest_observer_readiness(&self) -> RefObserverReadiness {
        match &self.manifest_engine {
            ManifestEngineHost::Active(driver) => driver.ref_observer_readiness(),
            ManifestEngineHost::PendingRebuild { .. } => RefObserverReadiness::Retrying,
        }
    }
}

/// How the ref observer learns a device that signed a head this host cannot
/// verify. It reads device trust through the same client the observer watches —
/// safe because the observer calls it between stream attempts, never from
/// inside the verifier lookup that failed — and writes the result into the
/// verifier set that client already resolves through.
fn signer_trust_refresh(
    hosted: &Arc<HostedContext>,
) -> bowline_daemon::manifest_transport::SignerTrustRefresh {
    let client = Arc::clone(&hosted.client);
    let trust = Arc::clone(&hosted.trust);
    Arc::new(move |device_id| trust.refresh_unknown_signer(&*client, device_id, Instant::now()))
}

/// Build the manifest-sync engine driver for a workspace. A missing
/// prerequisite (workspace key, hosted context, engine store) returns a typed
/// [`ManifestEngineUnavailableReason`] the caller turns into a retrying,
/// `limited`-status `PendingRebuild`. On success, returns a running driver whose
/// engine thread and ref subscription own the sync loop from here on.
fn build_manifest_driver(
    args: &SyncArgs,
    hosted_resolver: &HostedContextResolver,
    sink: bowline_daemon::manifest_driver::EngineSnapshotSink,
    handle: bowline_daemon::manifest_driver::EngineSnapshotHandle,
    counters: std::sync::Arc<bowline_local::sync::manifest_engine::EngineCounters>,
) -> Result<bowline_daemon::manifest_driver::ManifestDriver, ManifestEngineUnavailableReason> {
    use bowline_daemon::manifest_driver::{
        MANIFEST_ENGINE_DB_FILE, ManifestDriver, ManifestDriverConfig,
    };
    use bowline_local::sync::manifest_engine::{
        EngineConfig, EngineContext, probe_name_folding, probe_timestamp_granularity,
    };

    let workspace_key = match require_local_workspace_key(args) {
        Ok(key) => key,
        Err(error) => {
            eprintln!("bowline-daemon manifest engine key unavailable: {error}");
            return Err(ManifestEngineUnavailableReason::WorkspaceKeyUnavailable);
        }
    };
    let hosted = match hosted_resolver(args) {
        Ok(hosted) => hosted,
        Err(error) => {
            eprintln!("bowline-daemon manifest engine hosted context unavailable: {error}");
            return Err(ManifestEngineUnavailableReason::HostedContextUnavailable);
        }
    };
    let engine_state_dir = args
        .root
        .join(bowline_local::sync::manifest_engine::ENGINE_STATE_DIR);
    let context = EngineContext {
        crypto: workspace_key.workspace_crypto(&args.workspace_id),
        device_id: args.device_id.clone(),
        // Probed here, once, because this is where the daemon accepts the
        // workspace volume; the verdict cannot change while the root stays
        // mounted, and a root replacement rebuilds this context.
        names: probe_name_folding(&engine_state_dir),
        timestamps: probe_timestamp_granularity(&engine_state_dir),
        engine_state_dir,
        workspace_root: args.root.clone(),
        config: EngineConfig::default(),
        project_view: false,
        counters,
    };
    let reconnect_delay: bowline_daemon::manifest_transport::ReconnectDelay =
        Arc::new(remote_observer_reconnect_delay);
    let trust_refresh = signer_trust_refresh(&hosted);
    let config = ManifestDriverConfig {
        store_path: args.state_root.join(MANIFEST_ENGINE_DB_FILE),
        context,
        client: Arc::clone(&hosted.client),
        http: hosted.http.clone(),
        workspace_id: args.workspace_id.clone(),
        device_id: args.device_id.clone(),
        reconnect_delay,
        trust_refresh,
    };
    match ManifestDriver::spawn_production_with_sink(config, sink, handle) {
        Ok(driver) => Ok(driver),
        Err(error) => {
            eprintln!("bowline-daemon manifest engine driver failed to start: {error}");
            Err(ManifestEngineUnavailableReason::DriverStartFailed)
        }
    }
}
