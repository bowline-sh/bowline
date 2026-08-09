//! The daemon's ownership state for a workspace's filesystem watcher kernel.
//!
//! There is no periodic rescan fallback anywhere in the sync engine: a watcher
//! that never arms means the daemon never observes a local edit. So a failed
//! `notify` watch is not an absence, it is a degradation that must be logged,
//! surfaced through `ServiceRuntime` status, and retried forever on a capped
//! backoff — the same shape the manifest driver's `PendingRebuild` already uses.

#[cfg(test)]
use std::fs;
use std::path::Path;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use crate::daemon::WatcherSignal;
use crate::daemon::start_sync_watcher_with_recovery;
use crate::daemon::sync::RecoveryClock;
use crate::daemon::watcher::{SyncWatcher, SyncWatcherCoverageHandle};
use bowline_daemon::watcher_coverage::{NativeCoverageObservationReceiver, WatcherCoverageIds};
use bowline_daemon::watcher_recovery::WatcherRecoveryCoordinator;
use std::sync::Arc;

/// First retry delay after a failed watcher arm. Short because the common causes
/// (workspace root not yet materialized, a transient descriptor shortage) clear
/// within seconds of first-run trust.
const WATCHER_RETRY_INITIAL: Duration = Duration::from_secs(1);
/// Backoff ceiling: past this the daemon retries once per this interval forever.
/// A watcher is never abandoned, because abandoning it silently stops sync.
const WATCHER_RETRY_MAX: Duration = Duration::from_secs(30);

/// Whether the workspace watcher kernel is armed, and if not, when to retry it.
pub(in crate::daemon) enum WatcherHost {
    /// The native watch is installed. [`WatcherSignals`] says who currently owns
    /// the receiver it produces.
    Armed {
        /// RAII guard for the installed native watch: held solely so the watch
        /// stays registered, dropped to tear it down. `None` only in bridge
        /// tests, which drive a hand-written signal channel with no native
        /// watch behind it.
        _installed_watch: Option<Box<SyncWatcher>>,
        signals: WatcherSignals,
        ids: WatcherCoverageIds,
    },
    /// The native watch could not be installed. Status reports `ServiceRuntime`
    /// degraded (workspace `limited`) until a retry succeeds.
    Down {
        next_attempt: Instant,
        backoff: Option<Duration>,
        ids: WatcherCoverageIds,
    },
}

/// Who holds the armed kernel's signal receiver.
///
/// Both variants describe a healthy, installed watch — the distinction only
/// tells [`WatcherHost::ensure_armed`] whether it still has a receiver to hand
/// out or must rebuild the kernel to make a new one. It is deliberately NOT an
/// `Option`: a bare `None` reads as "the receiver is gone" and invited the
/// reading that an armed kernel whose bridge holds the receiver is a fault due
/// for immediate repair, which is the ordinary steady state.
pub(in crate::daemon) enum WatcherSignals {
    /// The receiver is here, waiting for a bridge worker to take it.
    Available(Receiver<WatcherSignal>),
    /// A bridge worker owns the receiver. Whether that worker is still alive is
    /// the coordinator's knowledge, not this host's.
    HeldByBridge,
}

impl WatcherHost {
    /// A host that has not attempted its first arm yet.
    pub(in crate::daemon) fn unarmed(now: Instant) -> Self {
        Self::unarmed_with_ids(now, WatcherCoverageIds::new())
    }

    /// A host in an existing workspace-lifecycle identity domain.
    pub(in crate::daemon) fn unarmed_with_ids(now: Instant, ids: WatcherCoverageIds) -> Self {
        Self::Down {
            next_attempt: now,
            backoff: None,
            ids,
        }
    }

    /// A host whose signals come from a hand-driven channel, for bridge tests.
    #[cfg(test)]
    pub(in crate::daemon) fn armed_with_signals(signals: Receiver<WatcherSignal>) -> Self {
        Self::Armed {
            _installed_watch: None,
            signals: WatcherSignals::Available(signals),
            ids: WatcherCoverageIds::new(),
        }
    }

    /// A host that owns a real installed native watch but hands the bridge a
    /// hand-driven signal channel. `recovery_inputs()` yields `Some`, so bridge
    /// tests exercise the production recovery path — authentic native coverage
    /// seals — while signal delivery stays deterministic.
    #[cfg(test)]
    pub(in crate::daemon) fn armed_with_native_watch(
        watch: SyncWatcher,
        signals: Receiver<WatcherSignal>,
        ids: WatcherCoverageIds,
    ) -> Self {
        Self::Armed {
            _installed_watch: Some(Box::new(watch)),
            signals: WatcherSignals::Available(signals),
            ids,
        }
    }

    /// Whether the native watch is currently installed. This is the
    /// `ServiceRuntime` status fact: `false` means no local change can be seen.
    pub(in crate::daemon) fn is_armed(&self) -> bool {
        matches!(self, Self::Armed { .. })
    }

    /// Hand the signal receiver to a bridge worker. `None` when the watcher is
    /// down or a previous bridge already took it.
    pub(in crate::daemon) fn take_signals(&mut self) -> Option<Receiver<WatcherSignal>> {
        let Self::Armed { signals, .. } = self else {
            return None;
        };
        match std::mem::replace(signals, WatcherSignals::HeldByBridge) {
            WatcherSignals::Available(receiver) => Some(receiver),
            WatcherSignals::HeldByBridge => None,
        }
    }

    /// Clone the native coverage authority and the lifecycle's single outward
    /// loss lane for the bridge worker. The callback invalidates close authority
    /// before publishing into this lane, so a coalesced notification cannot
    /// authorize a stale recovery close.
    pub(in crate::daemon) fn recovery_inputs(
        &self,
    ) -> Option<(SyncWatcherCoverageHandle, NativeCoverageObservationReceiver)> {
        let Self::Armed {
            _installed_watch: Some(installed_watch),
            ids,
            ..
        } = self
        else {
            return None;
        };
        Some((
            installed_watch.coverage_handle(),
            ids.observation_receiver(),
        ))
    }

    /// Return a receiver whose hand-off to the bridge worker failed.
    pub(in crate::daemon) fn restore_signals(&mut self, restored: Receiver<WatcherSignal>) {
        if let Self::Armed { signals, .. } = self {
            *signals = WatcherSignals::Available(restored);
        }
    }

    /// Tear the native watch down (shutdown, or before a rebuild). The host is
    /// left due for an immediate re-arm.
    pub(in crate::daemon) fn disarm(&mut self, now: Instant) {
        let ids = match self {
            Self::Armed { ids, .. } | Self::Down { ids, .. } => ids.clone(),
        };
        *self = Self::unarmed_with_ids(now, ids);
    }

    /// When the watcher is down, the instant of the next arm attempt, so the
    /// coordinator wakes for it even while otherwise idle.
    ///
    /// An armed kernel is never due: there is nothing to retry, including while
    /// a bridge worker holds the receiver. That state used to report "due now",
    /// and because the coordinator's engine-retry deadline is armed by the same
    /// drive its handler runs, a healthy daemon rescheduled a zero-delay
    /// deadline forever — a busy loop that burned a core in the status
    /// projection. Replacing a bridge worker that exited is the coordinator's
    /// job, on its own bounded supervision deadline.
    pub(in crate::daemon) fn next_retry(&self) -> Option<Instant> {
        match self {
            Self::Down { next_attempt, .. } => Some(*next_attempt),
            Self::Armed { .. } => None,
        }
    }

    /// Arm the watcher if it is not already producing signals and the backoff
    /// deadline has passed. Returns `true` when signals are available afterwards.
    pub(in crate::daemon) fn ensure_armed(
        &mut self,
        root: &Path,
        now: Instant,
        recovery: Arc<WatcherRecoveryCoordinator>,
        recovery_clock: Arc<RecoveryClock>,
    ) -> bool {
        let (backoff, ids) = match self {
            Self::Armed {
                signals: WatcherSignals::Available(_),
                ..
            } => return true,
            Self::Down { next_attempt, .. } if now < *next_attempt => return false,
            Self::Down { backoff, ids, .. } => (*backoff, ids.clone()),
            // A bridge worker owns the receiver, and the caller only asks for a
            // re-arm once it has decided that worker must be replaced: rebuild
            // from scratch rather than waiting on a kernel nothing is reading.
            Self::Armed {
                signals: WatcherSignals::HeldByBridge,
                ids,
                ..
            } => (None, ids.clone()),
        };
        match start_sync_watcher_with_recovery(root, ids.clone(), recovery, recovery_clock) {
            Ok((installed_watch, signals)) => {
                *self = Self::Armed {
                    _installed_watch: Some(Box::new(installed_watch)),
                    signals: WatcherSignals::Available(signals),
                    ids,
                };
                true
            }
            Err(error) => {
                let next_backoff = backoff.map_or(WATCHER_RETRY_INITIAL, |delay| {
                    (delay * 2).min(WATCHER_RETRY_MAX)
                });
                eprintln!(
                    "bowline-daemon filesystem watcher for {} is unavailable ({error}); local changes are not observed, retrying in {next_backoff:?}",
                    root.display()
                );
                *self = Self::Down {
                    next_attempt: now + next_backoff,
                    backoff: Some(next_backoff),
                    ids,
                };
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bowline_core::ids::WorkspaceId;
    use bowline_daemon::watcher_coverage::{
        CoverageCancellation, CoverageWait, WatcherCoverageAdapter,
    };
    use bowline_daemon::watcher_recovery::{
        BackoffPolicy, RecoveryProcessBootId, RecoveryProcessIdentity, RecoveryProcessSessionId,
        RecoverySourceIdentity, RecoveryTimestamp,
    };
    use time::OffsetDateTime;

    fn ensure_armed(host: &mut WatcherHost, root: &Path, now: Instant) -> bool {
        let recovery_clock = Arc::new(RecoveryClock::new());
        let timestamp = RecoveryTimestamp::from_datetime(OffsetDateTime::now_utc());
        let recovery = Arc::new(WatcherRecoveryCoordinator::startup_reconciliation(
            RecoverySourceIdentity::new(
                RecoveryProcessIdentity::new(
                    RecoveryProcessBootId::new(1).expect("test boot identity is nonzero"),
                    RecoveryProcessSessionId::new(1).expect("test session identity is nonzero"),
                    timestamp,
                ),
                WorkspaceId::new("ws_watcher_host_test"),
            ),
            recovery_clock.now(),
            BackoffPolicy::standard(),
        ));
        host.ensure_armed(root, now, recovery, recovery_clock)
    }

    fn coverage_wait() -> CoverageWait {
        CoverageWait::new(
            Instant::now() + Duration::from_secs(10),
            CoverageCancellation::new(),
        )
    }

    #[test]
    fn a_failed_arm_backs_off_and_stays_retryable() {
        let root = std::path::PathBuf::from("/bowline/does/not/exist/for/watcher/tests");
        let start = Instant::now();
        let mut host = WatcherHost::unarmed(start);
        assert!(!ensure_armed(&mut host, &root, start));
        assert!(!host.is_armed());
        let first_retry = host
            .next_retry()
            .expect("a down watcher is always retryable");
        assert!(first_retry > start);
        // Before the deadline the host must not burn a syscall on every drive.
        assert!(!ensure_armed(&mut host, &root, start));
        assert_eq!(host.next_retry(), Some(first_retry));
        // After the deadline the retry runs and the backoff grows.
        assert!(!ensure_armed(&mut host, &root, first_retry));
        let second_retry = host
            .next_retry()
            .expect("a down watcher is always retryable");
        assert!(second_retry.duration_since(first_retry) > first_retry.duration_since(start));
    }

    #[test]
    fn arming_a_real_root_yields_signals_once() {
        let root = crate::daemon::tests::unique_temp_dir("bowline-watcher-host-arm");
        fs::create_dir_all(&root).expect("watcher host test root exists");
        let now = Instant::now();
        let mut host = WatcherHost::unarmed(now);
        assert!(ensure_armed(&mut host, &root, now));
        assert!(host.is_armed());
        assert_eq!(host.next_retry(), None);
        assert!(host.take_signals().is_some());
        // A second hand-off finds nothing: the bridge worker owns the receiver.
        assert!(host.take_signals().is_none());
        // The kernel must be rebuilt to make another receiver, and `ensure_armed`
        // does exactly that when the coordinator asks for it.
        assert!(ensure_armed(&mut host, &root, now));
        assert!(host.take_signals().is_some());
        let _cleanup = fs::remove_dir_all(&root);
    }

    #[test]
    fn an_armed_host_is_never_due_for_a_retry() {
        let root = crate::daemon::tests::unique_temp_dir("bowline-watcher-host-not-due");
        fs::create_dir_all(&root).expect("watcher host test root exists");
        let now = Instant::now();
        let mut host = WatcherHost::unarmed(now);
        assert!(ensure_armed(&mut host, &root, now));
        assert_eq!(host.next_retry(), None);
        // The steady state: a bridge worker owns the receiver. Reporting a
        // deadline here spins the coordinator, because the drive that reads this
        // is the same drive the deadline runs.
        assert!(host.take_signals().is_some());
        assert_eq!(host.next_retry(), None);
        let _cleanup = fs::remove_dir_all(&root);
    }

    #[test]
    fn host_reconstruction_preserves_monotonic_native_identities() {
        let root = crate::daemon::tests::unique_temp_dir("bowline-watcher-host-identities");
        fs::create_dir_all(&root).expect("watcher host test root exists");
        let now = Instant::now();
        let mut host = WatcherHost::unarmed(now);
        assert!(ensure_armed(&mut host, &root, now));
        let first = match &mut host {
            WatcherHost::Armed {
                _installed_watch: Some(watcher),
                ..
            } => {
                let wait = coverage_wait();
                let preparation = watcher
                    .begin_recovery(&wait)
                    .expect("first live native preparation");
                watcher
                    .seal_after_scan(preparation, &wait)
                    .expect("first post-scan native seal")
            }
            _ => panic!("watcher host must own its native adapter"),
        };

        assert!(host.take_signals().is_some());
        assert!(ensure_armed(&mut host, &root, now));
        let second = match &mut host {
            WatcherHost::Armed {
                _installed_watch: Some(watcher),
                ..
            } => {
                let wait = coverage_wait();
                let preparation = watcher
                    .begin_recovery(&wait)
                    .expect("reconstructed live native preparation");
                watcher
                    .seal_after_scan(preparation, &wait)
                    .expect("reconstructed post-scan native seal")
            }
            _ => panic!("rebuilt watcher host must own its native adapter"),
        };
        assert!(second.boundary_id() > first.boundary_id());
        assert!(second.live_stream_epoch() > first.live_stream_epoch());
        let _cleanup = fs::remove_dir_all(&root);
    }
}
