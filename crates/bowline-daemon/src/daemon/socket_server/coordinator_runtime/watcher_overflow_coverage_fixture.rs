use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};

use crate::daemon::watcher::SyncWatcherCoverageHandle;
use bowline_daemon::watcher_coverage::{
    CoverageWait, WatcherCoverageAdapter, WatcherCoverageError, WatcherCoverageHandoff,
    WatcherCoveragePreparation,
};

pub(super) struct GateSecondPreparation {
    pub(super) inner: SyncWatcherCoverageHandle,
    pub(super) gate_enabled: Arc<AtomicBool>,
    pub(super) gated: bool,
    pub(super) entered: mpsc::SyncSender<()>,
    pub(super) release: mpsc::Receiver<()>,
}

impl WatcherCoverageAdapter for GateSecondPreparation {
    fn begin_recovery(
        &mut self,
        wait: &CoverageWait,
    ) -> Result<WatcherCoveragePreparation, WatcherCoverageError> {
        if self.gate_enabled.load(Ordering::Acquire) && !self.gated {
            self.gated = true;
            self.entered
                .send(())
                .map_err(|_| WatcherCoverageError::CoverageUnavailable)?;
            self.release
                .recv()
                .map_err(|_| WatcherCoverageError::CoverageUnavailable)?;
        }
        self.inner.begin_recovery(wait)
    }

    fn seal_after_scan(
        &mut self,
        preparation: WatcherCoveragePreparation,
        wait: &CoverageWait,
    ) -> Result<WatcherCoverageHandoff, WatcherCoverageError> {
        self.inner.seal_after_scan(preparation, wait)
    }

    fn validate_boundary(
        &self,
        handoff: &WatcherCoverageHandoff,
    ) -> Result<(), WatcherCoverageError> {
        self.inner.validate_boundary(handoff)
    }

    fn shutdown(&mut self) -> Result<(), WatcherCoverageError> {
        self.inner.shutdown()
    }
}
