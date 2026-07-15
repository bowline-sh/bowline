use std::time::{Duration, Instant};

#[cfg(test)]
use std::{
    sync::Arc,
    sync::atomic::{AtomicU64, Ordering},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(in crate::daemon) struct CoordinatorInstant(u64);

impl CoordinatorInstant {
    #[cfg(test)]
    pub(in crate::daemon) const ZERO: Self = Self(0);

    pub(in crate::daemon) fn from_duration(duration: Duration) -> Self {
        Self(u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX))
    }

    pub(in crate::daemon) fn add(self, duration: Duration) -> Self {
        Self(
            self.0
                .saturating_add(u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)),
        )
    }

    pub(in crate::daemon) fn saturating_duration_since(self, earlier: Self) -> Duration {
        Duration::from_nanos(self.0.saturating_sub(earlier.0))
    }
}

pub(in crate::daemon) trait CoordinatorClock: Send + Sync + 'static {
    fn now(&self) -> CoordinatorInstant;
}

#[derive(Debug, Clone)]
pub(in crate::daemon) struct SystemCoordinatorClock {
    started_at: Instant,
}

impl SystemCoordinatorClock {
    pub(in crate::daemon) fn new() -> Self {
        Self {
            started_at: Instant::now(),
        }
    }
}

impl Default for SystemCoordinatorClock {
    fn default() -> Self {
        Self::new()
    }
}

impl CoordinatorClock for SystemCoordinatorClock {
    fn now(&self) -> CoordinatorInstant {
        CoordinatorInstant::from_duration(self.started_at.elapsed())
    }
}

#[cfg(test)]
#[derive(Debug, Clone, Default)]
pub(super) struct FakeCoordinatorClock {
    nanos: Arc<AtomicU64>,
}

#[cfg(test)]
impl FakeCoordinatorClock {
    pub(super) fn advance(&self, duration: Duration) {
        self.nanos.fetch_add(
            u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX),
            Ordering::AcqRel,
        );
    }
}

#[cfg(test)]
impl CoordinatorClock for FakeCoordinatorClock {
    fn now(&self) -> CoordinatorInstant {
        CoordinatorInstant(self.nanos.load(Ordering::Acquire))
    }
}
