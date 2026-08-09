use std::path::Path;

use notify::{RecommendedWatcher, RecursiveMode, Watcher};

use super::{
    CoverageWait, NativeEventHandler, WatcherCoverageAdapter, WatcherCoverageBoundary,
    WatcherCoverageError, WatcherCoverageHandoff, WatcherCoverageIds, WatcherCoveragePreparation,
};

/// Event-only fallback that cannot claim a native coverage boundary.
pub struct NativeWatcherCoverageAdapter {
    watcher: Option<RecommendedWatcher>,
}

impl NativeWatcherCoverageAdapter {
    pub(super) fn start(
        root: &Path,
        event_handler: NativeEventHandler,
        _ids: WatcherCoverageIds,
        _wait: &CoverageWait,
    ) -> Result<Self, WatcherCoverageError> {
        let mut watcher = notify::recommended_watcher(move |event| {
            event_handler(super::WatcherStreamEpoch::fallback(), event);
        })
        .map_err(|_| WatcherCoverageError::CoverageUnavailable)?;
        watcher
            .watch(root, RecursiveMode::Recursive)
            .map_err(|_| WatcherCoverageError::CoverageUnavailable)?;
        Ok(Self {
            watcher: Some(watcher),
        })
    }
}

impl WatcherCoverageAdapter for NativeWatcherCoverageAdapter {
    fn begin_recovery(
        &mut self,
        _wait: &CoverageWait,
    ) -> Result<WatcherCoveragePreparation, WatcherCoverageError> {
        Err(WatcherCoverageError::CoverageUnavailable)
    }

    fn seal_after_scan(
        &mut self,
        _preparation: WatcherCoveragePreparation,
        _wait: &CoverageWait,
    ) -> Result<WatcherCoverageHandoff, WatcherCoverageError> {
        Err(WatcherCoverageError::CoverageUnavailable)
    }

    fn validate_boundary(
        &self,
        _handoff: &WatcherCoverageHandoff,
    ) -> Result<(), WatcherCoverageError> {
        Err(WatcherCoverageError::CoverageUnavailable)
    }

    fn shutdown(&mut self) -> Result<(), WatcherCoverageError> {
        self.watcher = None;
        Ok(())
    }
}
