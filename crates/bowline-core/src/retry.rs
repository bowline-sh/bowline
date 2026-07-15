use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryBackoffPolicy {
    initial: Duration,
    maximum: Duration,
    jitter_seconds: u64,
    maximum_attempts: Option<u32>,
}

impl RetryBackoffPolicy {
    pub const fn new(
        initial: Duration,
        maximum: Duration,
        jitter_seconds: u64,
        maximum_attempts: Option<u32>,
    ) -> Self {
        Self {
            initial,
            maximum,
            jitter_seconds,
            maximum_attempts,
        }
    }

    pub fn delay(self, retry_key: &str, attempt_count: u32) -> Duration {
        let exponent = attempt_count.saturating_sub(1).min(5);
        let base = self
            .initial
            .saturating_mul(2_u32.pow(exponent))
            .min(self.maximum);
        let jitter = retry_key.bytes().fold(0_u64, |state, byte| {
            state.wrapping_mul(31).wrapping_add(u64::from(byte))
        }) % self.jitter_seconds.saturating_add(1);
        (base + Duration::from_secs(jitter)).min(self.maximum)
    }

    pub fn is_exhausted(self, attempt_count: u32) -> bool {
        self.maximum_attempts
            .is_some_and(|maximum| attempt_count >= maximum)
    }
}

pub const BOUNDED_SYNC_RETRY_POLICY: RetryBackoffPolicy =
    RetryBackoffPolicy::new(Duration::from_secs(2), Duration::from_secs(60), 3, Some(8));

pub const OFFLINE_SYNC_RETRY_POLICY: RetryBackoffPolicy =
    RetryBackoffPolicy::new(Duration::from_secs(2), Duration::from_secs(60), 3, None);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_retry_policy_is_deterministic_bounded_and_budgeted() {
        let first = BOUNDED_SYNC_RETRY_POLICY.delay("operation", 1);
        assert_eq!(first, BOUNDED_SYNC_RETRY_POLICY.delay("operation", 1));
        assert!(BOUNDED_SYNC_RETRY_POLICY.delay("operation", 2) >= first);
        assert!(BOUNDED_SYNC_RETRY_POLICY.delay("operation", u32::MAX) <= Duration::from_secs(60));
        assert!(!BOUNDED_SYNC_RETRY_POLICY.is_exhausted(7));
        assert!(BOUNDED_SYNC_RETRY_POLICY.is_exhausted(8));
        assert!(!OFFLINE_SYNC_RETRY_POLICY.is_exhausted(u32::MAX));
    }
}
