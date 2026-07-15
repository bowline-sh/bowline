use std::{collections::BTreeMap, time::Instant};

use super::types::{StatusRetryPolicy, StatusSource};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SourceRetryState {
    consecutive_failures: u32,
    due_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScheduledRetry {
    pub(crate) delay: std::time::Duration,
    pub(crate) capped: bool,
}

#[derive(Debug, Default)]
pub(crate) struct RetrySchedule {
    sources: BTreeMap<StatusSource, SourceRetryState>,
}

impl RetrySchedule {
    pub(crate) fn record_failure(
        &mut self,
        source: StatusSource,
        now: Instant,
        policy: StatusRetryPolicy,
    ) -> ScheduledRetry {
        let failure_count = self
            .sources
            .get(&source)
            .map_or(1, |state| state.consecutive_failures.saturating_add(1));
        let exponent = failure_count.saturating_sub(1).min(31);
        let multiplier = 1_u32 << exponent;
        let uncapped = policy.initial_delay().saturating_mul(multiplier);
        let delay = uncapped.min(policy.max_delay());
        self.sources.insert(
            source,
            SourceRetryState {
                consecutive_failures: failure_count,
                due_at: now + delay,
            },
        );
        ScheduledRetry {
            delay,
            capped: uncapped >= policy.max_delay(),
        }
    }

    pub(crate) fn record_success(&mut self, source: StatusSource) -> bool {
        self.sources.remove(&source).is_some()
    }

    pub(crate) fn abandon(&mut self, source: StatusSource) -> bool {
        self.sources.remove(&source).is_some()
    }

    pub(crate) fn is_scheduled(&self, source: StatusSource) -> bool {
        self.sources.contains_key(&source)
    }

    pub(crate) fn due_sources(&self, now: Instant) -> Vec<StatusSource> {
        self.sources
            .iter()
            .filter_map(|(source, state)| (state.due_at <= now).then_some(*source))
            .collect()
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.sources.values().map(|state| state.due_at).min()
    }

    pub(crate) fn len(&self) -> usize {
        self.sources.len()
    }
}
