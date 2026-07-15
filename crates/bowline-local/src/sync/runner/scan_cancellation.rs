use std::sync::{
    Mutex,
    atomic::{AtomicU8, AtomicU64, Ordering},
};

use bowline_core::namespace_snapshot::NamespaceCancellation;

use super::{SyncRunner, SyncRunnerError};
use crate::{
    metadata::{MetadataStore, SyncClaimCheck, SyncClaimHandle, SyncOperationState},
    sync::CoalesceError,
};

const SCAN_NAMESPACE_CLAIM_POLL_INTERVAL_CHECKS: u64 = 64;
const CLAIM_OWNED: u8 = 0;
const CLAIM_CANCELLATION_REQUESTED: u8 = 1;
const CLAIM_OWNERSHIP_LOST: u8 = 2;

pub(super) struct ScanNamespaceCancellation {
    store: Mutex<MetadataStore>,
    claim: SyncClaimHandle,
    reconciliation_required: bool,
    checks: AtomicU64,
    state: AtomicU8,
}

impl ScanNamespaceCancellation {
    fn open(runner: &SyncRunner<'_>) -> Result<Option<Self>, SyncRunnerError> {
        let Some(claim) = runner.options.sync_claim.clone() else {
            return Ok(None);
        };
        let cancellation = Self {
            store: Mutex::new(MetadataStore::open(runner.metadata_db_path())?),
            reconciliation_required: claim.claimed_from_state()
                == SyncOperationState::ReconciliationRequired,
            claim,
            checks: AtomicU64::new(1),
            state: AtomicU8::new(CLAIM_OWNED),
        };
        cancellation.check_now()?;
        Ok(Some(cancellation))
    }

    fn check_now(&self) -> Result<(), SyncRunnerError> {
        let check = self.store.lock().ok().and_then(|store| {
            if self.reconciliation_required {
                store
                    .renew_sync_operation_reconciliation_boundary(&self.claim)
                    .ok()
            } else {
                store.authorize_sync_operation_boundary(&self.claim).ok()
            }
        });
        let Some(check) = check else {
            self.state.store(CLAIM_OWNERSHIP_LOST, Ordering::Release);
            return Err(SyncRunnerError::SyncClaimOwnershipLost);
        };
        let state = match check {
            SyncClaimCheck::Owned => CLAIM_OWNED,
            SyncClaimCheck::CancellationRequested if self.reconciliation_required => CLAIM_OWNED,
            SyncClaimCheck::CancellationRequested => CLAIM_CANCELLATION_REQUESTED,
            SyncClaimCheck::OwnershipLost => CLAIM_OWNERSHIP_LOST,
        };
        self.state.store(state, Ordering::Release);
        self.latched_error().map_or(Ok(()), Err)
    }

    fn latched_error(&self) -> Option<SyncRunnerError> {
        match self.state.load(Ordering::Acquire) {
            CLAIM_OWNED => None,
            CLAIM_CANCELLATION_REQUESTED => {
                Some(SyncRunnerError::SyncOperationCancellationRequested)
            }
            _ => Some(SyncRunnerError::SyncClaimOwnershipLost),
        }
    }
}

impl NamespaceCancellation for ScanNamespaceCancellation {
    fn is_cancelled(&self) -> bool {
        if self.latched_error().is_some() {
            return true;
        }
        let check = self.checks.fetch_add(1, Ordering::Relaxed);
        if !claim_poll_due(check) {
            return false;
        }
        self.check_now().is_err()
    }
}

fn claim_poll_due(check: u64) -> bool {
    check.is_multiple_of(SCAN_NAMESPACE_CLAIM_POLL_INTERVAL_CHECKS)
}

impl SyncRunner<'_> {
    pub(super) fn scan_namespace_cancellation(
        &self,
    ) -> Result<Option<ScanNamespaceCancellation>, SyncRunnerError> {
        ScanNamespaceCancellation::open(self)
    }

    pub(super) fn finish_namespace_scan<T>(
        &self,
        cancellation: Option<&ScanNamespaceCancellation>,
        result: Result<T, CoalesceError>,
    ) -> Result<T, SyncRunnerError> {
        if let Some(cancellation) = cancellation {
            if let Some(error) = cancellation.latched_error() {
                return Err(error);
            }
            cancellation.check_now()?;
        }
        result.map_err(Into::into)
    }

    pub(super) fn finish_claim_backed_namespace_operation<T, E>(
        &self,
        cancellation: Option<&ScanNamespaceCancellation>,
        result: Result<T, E>,
    ) -> Result<T, SyncRunnerError>
    where
        E: Into<SyncRunnerError>,
    {
        if let Some(cancellation) = cancellation {
            if let Some(error) = cancellation.latched_error() {
                return Err(error);
            }
            cancellation.check_now()?;
        }
        result.map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespace_claim_polling_has_a_named_bounded_cadence() {
        assert!(claim_poll_due(0));
        assert!((1..SCAN_NAMESPACE_CLAIM_POLL_INTERVAL_CHECKS).all(|check| !claim_poll_due(check)));
        assert!(claim_poll_due(SCAN_NAMESPACE_CLAIM_POLL_INTERVAL_CHECKS));
    }
}
