use super::*;

const OWNED: u8 = 0;
const UNCERTAIN: u8 = 1;
const LOST: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::daemon) enum ClaimOwnership {
    Owned,
    Uncertain,
    Lost,
}

#[derive(Debug, Clone, Copy)]
pub(in crate::daemon) struct ClaimLeasePolicy {
    pub(in crate::daemon) heartbeat_interval: Duration,
    pub(in crate::daemon) lease_duration: Duration,
}

impl Default for ClaimLeasePolicy {
    fn default() -> Self {
        Self {
            heartbeat_interval: Duration::from_secs(15),
            lease_duration: Duration::from_secs(SYNC_CLAIM_TIMEOUT_SECONDS as u64),
        }
    }
}

pub(in crate::daemon) struct ClaimLeaseSupervisor {
    stop: mpsc::SyncSender<()>,
    ownership: Arc<AtomicUsize>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl ClaimLeaseSupervisor {
    pub(in crate::daemon) fn start(
        state_root: PathBuf,
        claim: SyncClaimHandle,
        policy: ClaimLeasePolicy,
    ) -> io::Result<Self> {
        let (stop, stop_rx) = mpsc::sync_channel(1);
        let ownership = Arc::new(AtomicUsize::new(OWNED as usize));
        let worker_ownership = Arc::clone(&ownership);
        let worker = std::thread::Builder::new()
            .name("bowline-sync-claim-lease".to_string())
            .spawn(move || {
                supervise_claim_lease(&state_root, &claim, policy, &stop_rx, &worker_ownership);
            })?;
        Ok(Self {
            stop,
            ownership,
            worker: Some(worker),
        })
    }

    pub(in crate::daemon) fn stop(mut self) -> ClaimOwnership {
        let _ = self.stop.try_send(());
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            return ClaimOwnership::Lost;
        }
        decode_ownership(self.ownership.load(Ordering::Acquire))
    }
}

impl Drop for ClaimLeaseSupervisor {
    fn drop(&mut self) {
        let _ = self.stop.try_send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn supervise_claim_lease(
    state_root: &Path,
    claim: &SyncClaimHandle,
    policy: ClaimLeasePolicy,
    stop: &Receiver<()>,
    ownership: &AtomicUsize,
) {
    loop {
        let now = OffsetDateTime::now_utc();
        let renewed =
            MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).and_then(|store| {
                store.renew_sync_operation_claim(
                    claim,
                    &format_timestamp(now),
                    &format_timestamp(
                        now + time::Duration::try_from(policy.lease_duration)
                            .unwrap_or(time::Duration::seconds(SYNC_CLAIM_TIMEOUT_SECONDS)),
                    ),
                )
            });
        match renewed {
            Ok(SyncClaimTransition::Applied) => {
                ownership.store(OWNED as usize, Ordering::Release);
            }
            Ok(SyncClaimTransition::OwnershipLost) => {
                ownership.store(LOST as usize, Ordering::Release);
                return;
            }
            Err(error) => {
                ownership.store(UNCERTAIN as usize, Ordering::Release);
                eprintln!("bowline-daemon sync claim renewal unavailable: {error}");
            }
        }
        match stop.recv_timeout(policy.heartbeat_interval) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

fn decode_ownership(value: usize) -> ClaimOwnership {
    match value as u8 {
        OWNED => ClaimOwnership::Owned,
        UNCERTAIN => ClaimOwnership::Uncertain,
        _ => ClaimOwnership::Lost,
    }
}
