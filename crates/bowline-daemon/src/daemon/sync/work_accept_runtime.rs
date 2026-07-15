use super::*;

use bowline_local::metadata::{
    ClaimedWorkViewAcceptOperation, WorkViewAcceptClaimTransition, WorkViewAcceptFailureReason,
    WorkViewAcceptReviewReason,
};
#[cfg(test)]
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
};

#[cfg(test)]
pub(in crate::daemon) struct WorkAcceptLeaseSupervisor {
    stop_tx: mpsc::Sender<()>,
    lost: Arc<AtomicBool>,
    thread: thread::JoinHandle<()>,
}

#[cfg(test)]
impl WorkAcceptLeaseSupervisor {
    fn start_with(interval: Duration, mut renew: impl FnMut() -> bool + Send + 'static) -> Self {
        let (stop_tx, stop_rx) = mpsc::channel();
        let lost = Arc::new(AtomicBool::new(false));
        let worker_lost = lost.clone();
        let thread = thread::spawn(move || {
            loop {
                match stop_rx.recv_timeout(interval) {
                    Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
                if !renew() {
                    worker_lost.store(true, Ordering::Release);
                    break;
                }
            }
        });
        Self {
            stop_tx,
            lost,
            thread,
        }
    }

    pub(in crate::daemon) fn stop(self) -> ClaimOwnership {
        let _ = self.stop_tx.send(());
        if self.thread.join().is_err() || self.lost.load(Ordering::Acquire) {
            ClaimOwnership::Lost
        } else {
            ClaimOwnership::Owned
        }
    }
}

pub(in crate::daemon) enum WorkAcceptRunResult {
    Completed {
        snapshot_id: SnapshotId,
        result_json: String,
    },
    Review {
        reason: WorkViewAcceptReviewReason,
        result_json: String,
    },
    Cancelled,
    OwnershipLost,
}

impl ContinuousSyncRuntime {
    pub(in crate::daemon) fn finish_work_view_accept(
        &mut self,
        claimed: &ClaimedWorkViewAcceptOperation,
        result: Result<WorkAcceptRunResult, SyncOnceError>,
    ) {
        let now_time = OffsetDateTime::now_utc();
        let now = format_timestamp(now_time);
        let transition =
            self.metadata_store_for_write("metadata_store(finish_work_view_accept)", |store| {
                match &result {
                    Ok(WorkAcceptRunResult::Completed {
                        snapshot_id,
                        result_json,
                    }) => store.complete_work_view_accept(
                        &claimed.claim,
                        snapshot_id,
                        result_json,
                        &now,
                    ),
                    Ok(WorkAcceptRunResult::Review {
                        reason,
                        result_json,
                    }) => store.mark_work_view_accept_review(
                        &claimed.claim,
                        *reason,
                        result_json,
                        &now,
                    ),
                    Ok(WorkAcceptRunResult::Cancelled) => store.cancel_claimed_work_view_accept(
                        &claimed.claim,
                        r#"{"outcome":"cancelled"}"#,
                        &now,
                    ),
                    Ok(WorkAcceptRunResult::OwnershipLost) => {
                        Ok(WorkViewAcceptClaimTransition::OwnershipLost)
                    }
                    Err(_error) if claimed.operation.attempt_count < MAX_SYNC_RETRY_ATTEMPTS => {
                        eprintln!("bowline-daemon work-view accept transient failure");
                        let delay = retry_delay_seconds(
                            claimed.claim.operation_id(),
                            claimed.operation.attempt_count,
                        );
                        let next_attempt_at =
                            format_timestamp(now_time + time::Duration::seconds(delay));
                        store.retry_work_view_accept(
                            &claimed.claim,
                            "transient work-view accept failure",
                            &next_attempt_at,
                            &now,
                        )
                    }
                    Err(_error) => {
                        eprintln!("bowline-daemon work-view accept retry limit reached");
                        store.fail_work_view_accept(
                            &claimed.claim,
                            WorkViewAcceptFailureReason::Transient,
                            "work-view accept retry limit reached",
                            &now,
                        )
                    }
                }
            });
        if transition == Some(WorkViewAcceptClaimTransition::OwnershipLost) {
            self.record_sync_ownership_lost();
        }
    }
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::daemon) enum HostedAcceptAttempt {
    Advanced {
        snapshot_id: SnapshotId,
        result_json: String,
    },
    Stale,
    Review {
        reason: bowline_local::metadata::WorkViewAcceptReviewReason,
        result_json: String,
    },
    OwnershipLost,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::daemon) enum HostedAcceptOutcome {
    Completed {
        snapshot_id: SnapshotId,
        result_json: String,
    },
    Review {
        reason: bowline_local::metadata::WorkViewAcceptReviewReason,
        result_json: String,
    },
    OwnershipLost,
    RetryExhausted,
}

/// Re-resolve canonical main before every merge attempt. A stale hosted CAS
/// discards the prepared result and loops without authorizing local publish.
#[cfg(test)]
pub(in crate::daemon) fn run_hosted_accept_retry_loop<E>(
    max_attempts: u32,
    mut claim_owned: impl FnMut() -> Result<bool, E>,
    mut reconcile_current_main: impl FnMut() -> Result<(), E>,
    mut attempt_upload_and_cas: impl FnMut() -> Result<HostedAcceptAttempt, E>,
    mut publish_local: impl FnMut(&SnapshotId) -> Result<bool, E>,
) -> Result<HostedAcceptOutcome, E> {
    for _ in 0..max_attempts {
        if !claim_owned()? {
            return Ok(HostedAcceptOutcome::OwnershipLost);
        }
        reconcile_current_main()?;
        match attempt_upload_and_cas()? {
            HostedAcceptAttempt::Stale => continue,
            HostedAcceptAttempt::OwnershipLost => {
                return Ok(HostedAcceptOutcome::OwnershipLost);
            }
            HostedAcceptAttempt::Review {
                reason,
                result_json,
            } => {
                return Ok(HostedAcceptOutcome::Review {
                    reason,
                    result_json,
                });
            }
            HostedAcceptAttempt::Advanced {
                snapshot_id,
                result_json,
            } => {
                if !claim_owned()? || !publish_local(&snapshot_id)? {
                    return Ok(HostedAcceptOutcome::OwnershipLost);
                }
                return Ok(HostedAcceptOutcome::Completed {
                    snapshot_id,
                    result_json,
                });
            }
        }
    }
    Ok(HostedAcceptOutcome::RetryExhausted)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::sync::atomic::AtomicUsize;

    use super::*;

    #[test]
    fn stale_cas_reconciles_again_before_single_local_publish() {
        let reconciles = Cell::new(0_u32);
        let attempts = Cell::new(0_u32);
        let publishes = Cell::new(0_u32);

        let outcome = run_hosted_accept_retry_loop(
            3,
            || Ok::<_, ()>(true),
            || {
                reconciles.set(reconciles.get() + 1);
                Ok(())
            },
            || {
                attempts.set(attempts.get() + 1);
                if attempts.get() == 1 {
                    Ok(HostedAcceptAttempt::Stale)
                } else {
                    Ok(HostedAcceptAttempt::Advanced {
                        snapshot_id: SnapshotId::new("snap_advanced"),
                        result_json: "{}".to_string(),
                    })
                }
            },
            |snapshot_id| {
                assert_eq!(snapshot_id.as_str(), "snap_advanced");
                publishes.set(publishes.get() + 1);
                Ok(true)
            },
        )
        .expect("retry loop");

        assert!(matches!(outcome, HostedAcceptOutcome::Completed { .. }));
        assert_eq!(reconciles.get(), 2);
        assert_eq!(attempts.get(), 2);
        assert_eq!(publishes.get(), 1);
    }

    #[test]
    fn ownership_loss_after_hosted_advance_blocks_local_publish() {
        let checks = Cell::new(0_u32);
        let publishes = Cell::new(0_u32);
        let outcome = run_hosted_accept_retry_loop(
            1,
            || {
                checks.set(checks.get() + 1);
                Ok::<_, ()>(checks.get() == 1)
            },
            || Ok(()),
            || {
                Ok(HostedAcceptAttempt::Advanced {
                    snapshot_id: SnapshotId::new("snap_advanced"),
                    result_json: "{}".to_string(),
                })
            },
            |_| {
                publishes.set(publishes.get() + 1);
                Ok(true)
            },
        )
        .expect("ownership result");

        assert_eq!(outcome, HostedAcceptOutcome::OwnershipLost);
        assert_eq!(publishes.get(), 0);
    }

    #[test]
    fn terminal_attempt_outcomes_never_publish_local_main() {
        let publishes = Cell::new(0_u32);
        let review = run_hosted_accept_retry_loop(
            1,
            || Ok::<_, ()>(true),
            || Ok(()),
            || {
                Ok(HostedAcceptAttempt::Review {
                    reason: bowline_local::metadata::WorkViewAcceptReviewReason::PolicyDrift,
                    result_json: "{}".to_string(),
                })
            },
            |_| {
                publishes.set(publishes.get() + 1);
                Ok(true)
            },
        )
        .expect("review outcome");
        assert!(matches!(review, HostedAcceptOutcome::Review { .. }));

        let ownership_lost = run_hosted_accept_retry_loop(
            1,
            || Ok::<_, ()>(true),
            || Ok(()),
            || Ok(HostedAcceptAttempt::OwnershipLost),
            |_| {
                publishes.set(publishes.get() + 1);
                Ok(true)
            },
        )
        .expect("ownership outcome");
        assert_eq!(ownership_lost, HostedAcceptOutcome::OwnershipLost);
        assert_eq!(publishes.get(), 0);
    }

    #[test]
    fn lease_supervisor_reports_renewal_ownership_loss() {
        let renewals = Arc::new(AtomicUsize::new(0));
        let renewals_for_worker = renewals.clone();
        let supervisor =
            WorkAcceptLeaseSupervisor::start_with(Duration::from_millis(1), move || {
                renewals_for_worker.fetch_add(1, Ordering::SeqCst);
                false
            });
        for _ in 0..100 {
            if renewals.load(Ordering::SeqCst) > 0 {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }

        assert_eq!(supervisor.stop(), ClaimOwnership::Lost);
        assert!(renewals.load(Ordering::SeqCst) > 0);
    }

    #[test]
    fn post_cas_local_edit_is_reconciled_before_retry_publish() {
        let local_edit_synced = Cell::new(false);
        let first = run_hosted_accept_retry_loop(
            1,
            || Ok::<_, ()>(true),
            || Ok(()),
            || {
                Ok(HostedAcceptAttempt::Advanced {
                    snapshot_id: SnapshotId::new("snap_first"),
                    result_json: "{}".to_string(),
                })
            },
            |_| Ok(false),
        )
        .expect("first attempt");
        assert_eq!(first, HostedAcceptOutcome::OwnershipLost);

        let second = run_hosted_accept_retry_loop(
            1,
            || Ok::<_, ()>(true),
            || {
                local_edit_synced.set(true);
                Ok(())
            },
            || {
                assert!(local_edit_synced.get());
                Ok(HostedAcceptAttempt::Advanced {
                    snapshot_id: SnapshotId::new("snap_remerged"),
                    result_json: "{}".to_string(),
                })
            },
            |_| Ok(true),
        )
        .expect("recovery attempt");

        assert!(matches!(second, HostedAcceptOutcome::Completed { .. }));
    }
}
