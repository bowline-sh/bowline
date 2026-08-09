use crate::watcher_coverage::{WatcherCoverageBoundary, WatcherCoverageHandoff};

use super::{ActivityWatermark, LossWatermark};

/// A mechanical watcher boundary admitted at one exact coordinator frontier.
///
/// `WatcherCoverageBoundary` remains the sole native-proof authority. This
/// wrapper adds only the coordinator activity watermark captured atomically
/// after the adapter has completed its live handoff.
#[derive(Debug, Clone)]
pub struct AttemptCoverageBoundary {
    activity_watermark: ActivityWatermark,
    loss_watermark: LossWatermark,
    handoff: WatcherCoverageHandoff,
}

impl AttemptCoverageBoundary {
    pub(crate) fn admitted(
        activity_watermark: ActivityWatermark,
        loss_watermark: LossWatermark,
        handoff: WatcherCoverageHandoff,
    ) -> Self {
        Self {
            activity_watermark,
            loss_watermark,
            handoff,
        }
    }

    pub fn activity_watermark(&self) -> ActivityWatermark {
        self.activity_watermark
    }

    /// The loss admissions this attempt's covering scan is answerable for.
    pub fn loss_watermark(&self) -> LossWatermark {
        self.loss_watermark
    }

    pub fn proof(&self) -> WatcherCoverageBoundary {
        self.handoff.boundary()
    }

    pub(crate) fn is_current(&self) -> bool {
        self.handoff.close_guard().is_current()
    }

    #[cfg(test)]
    pub(crate) fn handoff(&self) -> &WatcherCoverageHandoff {
        &self.handoff
    }
}

impl PartialEq for AttemptCoverageBoundary {
    fn eq(&self, other: &Self) -> bool {
        self.activity_watermark == other.activity_watermark
            && self.proof() == other.proof()
            && self.handoff.close_guard().token() == other.handoff.close_guard().token()
    }
}

impl Eq for AttemptCoverageBoundary {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeAdapter {
    Darwin,
    Linux,
}
