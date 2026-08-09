use std::time::{Duration, Instant};

use bowline_daemon::manifest_driver::SyncBarrierError;
use bowline_daemon::watcher_recovery::RecoveryLifecycle;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use super::DaemonServerState;

/// Receipt for the public workspace boundary, combining engine and observer
/// convergence with the watcher-recovery frontier that was linearized with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::daemon) struct DaemonWorkspaceExactBarrierReceipt {
    engine_observer: bowline_daemon::manifest_driver::EngineObserverConvergenceReceipt,
    recovery: bowline_daemon::watcher_recovery::RecoveryAttestation,
    linearized_at: String,
}

impl DaemonWorkspaceExactBarrierReceipt {
    pub(in crate::daemon) fn engine_observer(
        &self,
    ) -> &bowline_daemon::manifest_driver::EngineObserverConvergenceReceipt {
        &self.engine_observer
    }

    pub(in crate::daemon) fn recovery(
        &self,
    ) -> &bowline_daemon::watcher_recovery::RecoveryAttestation {
        &self.recovery
    }

    pub(in crate::daemon) fn linearized_at(&self) -> &str {
        &self.linearized_at
    }
}

impl DaemonServerState {
    /// Wait for one exact engine, observer, and recovery boundary. Recovery
    /// changes restart the engine wait inside the caller's original deadline.
    pub(in crate::daemon) fn request_sync_barrier(
        &self,
        timeout: Duration,
        mut cancelled: impl FnMut() -> bool,
    ) -> Result<DaemonWorkspaceExactBarrierReceipt, SyncBarrierError> {
        let recovery = self
            .recovery_coordinator
            .as_ref()
            .ok_or(SyncBarrierError::WorkspaceNotServed)?;
        let engine = self
            .manifest_snapshot
            .as_ref()
            .ok_or(SyncBarrierError::WorkspaceNotServed)?;
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(SyncBarrierError::TimedOut)?;

        loop {
            if cancelled() {
                return Err(SyncBarrierError::Cancelled);
            }
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .filter(|remaining| !remaining.is_zero())
                .ok_or(SyncBarrierError::TimedOut)?;
            let recovery_snapshot =
                recovery
                    .snapshot()
                    .map_err(|_| SyncBarrierError::Unavailable {
                        reason: "watcher recovery state is unavailable",
                    })?;
            match recovery_snapshot.lifecycle() {
                RecoveryLifecycle::Blocked => {
                    let failure =
                        recovery_snapshot
                            .failure()
                            .ok_or(SyncBarrierError::Unavailable {
                                reason: "watcher recovery failure state is unavailable",
                            })?;
                    return Err(SyncBarrierError::RecoveryBlocked {
                        class: failure.class(),
                        code: *failure.code(),
                    });
                }
                RecoveryLifecycle::Recovering => {
                    std::thread::sleep(remaining.min(Duration::from_millis(25)));
                    continue;
                }
                RecoveryLifecycle::Nominal => {}
            }

            let recovery_frontier = match recovery.capture_nominal_frontier() {
                Ok(frontier) => frontier,
                Err(_) => continue,
            };
            let waiter = match engine.request_sync_barrier() {
                Ok(waiter) => waiter,
                Err(SyncBarrierError::Unavailable { .. } | SyncBarrierError::EngineStopped) => {
                    std::thread::sleep(remaining.min(Duration::from_millis(25)));
                    continue;
                }
                Err(error) => return Err(error),
            };
            let mut user_cancelled = false;
            let engine_observer = match waiter.wait(remaining, || {
                user_cancelled = cancelled();
                user_cancelled || recovery.linearize_nominal(recovery_frontier).is_err()
            }) {
                Ok(receipt) => receipt,
                Err(SyncBarrierError::Cancelled) if !user_cancelled => continue,
                Err(error) => return Err(error),
            };
            let Ok(recovery) = recovery.linearize_nominal(recovery_frontier) else {
                continue;
            };
            if !receipt_process_identity_matches(&engine_observer, &recovery) {
                return Err(SyncBarrierError::FatalContract {
                    reason: "workspace barrier component provenance does not match",
                });
            }
            let linearized_at = OffsetDateTime::now_utc().format(&Rfc3339).map_err(|_| {
                SyncBarrierError::Unavailable {
                    reason: "workspace barrier timestamp is unavailable",
                }
            })?;
            return Ok(DaemonWorkspaceExactBarrierReceipt {
                engine_observer,
                recovery,
                linearized_at,
            });
        }
    }
}

fn receipt_process_identity_matches(
    engine_observer: &bowline_daemon::manifest_driver::EngineObserverConvergenceReceipt,
    recovery: &bowline_daemon::watcher_recovery::RecoveryAttestation,
) -> bool {
    let engine = engine_observer.engine().process_identity();
    let recovery = recovery.source_identity().process_identity();
    let Ok(started_at) = recovery.started_at().to_rfc3339() else {
        return false;
    };
    engine.boot_id() == recovery.boot_id().to_string()
        && engine.session_id() == recovery.session_id().to_string()
        && engine.started_at() == started_at
}
