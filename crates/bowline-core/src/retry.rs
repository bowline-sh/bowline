use std::time::Duration;

/// Doubling stops here so the delay settles at a bounded steady state instead
/// of overflowing toward `maximum` on huge attempt counts.
const MAXIMUM_BACKOFF_EXPONENT: u32 = 5;

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
        let exponent = attempt_count
            .saturating_sub(1)
            .min(MAXIMUM_BACKOFF_EXPONENT);
        // Reserve the jitter window below the cap before clamping. Clamping
        // after adding jitter would discard it entirely once the exponential
        // reaches `maximum`, and every device would then retry on the same
        // wall-clock boundary forever — exactly the stampede jitter exists to
        // prevent, at exactly the moment the queue is longest.
        let jitter_window = Duration::from_secs(self.jitter_seconds);
        let base = self
            .initial
            .saturating_mul(2_u32.pow(exponent))
            .min(self.maximum.saturating_sub(jitter_window));
        base + Duration::from_secs(self.jitter_seconds(retry_key, attempt_count))
    }

    /// Jitter offset in seconds, decorrelated across both operations and
    /// successive attempts of the same operation.
    fn jitter_seconds(self, retry_key: &str, attempt_count: u32) -> u64 {
        let seed = retry_key
            .bytes()
            .chain(attempt_count.to_be_bytes())
            .fold(0_u64, |state, byte| {
                state.wrapping_mul(31).wrapping_add(u64::from(byte))
            });
        seed % self.jitter_seconds.saturating_add(1)
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
    fn offline_retries_stay_decorrelated_after_the_backoff_reaches_its_cap() {
        // Attempt 10 is deep in the clamped regime, where the delay used to
        // collapse to exactly `maximum` for every key and every attempt.
        let first = OFFLINE_SYNC_RETRY_POLICY.delay("operation-a", 10);
        let second = OFFLINE_SYNC_RETRY_POLICY.delay("operation-b", 10);
        let later = OFFLINE_SYNC_RETRY_POLICY.delay("operation-a", 11);

        assert_ne!(
            first, second,
            "distinct operations must not retry in lockstep"
        );
        assert_ne!(
            first, later,
            "successive attempts of one operation must decorrelate"
        );
        assert!(first <= Duration::from_secs(60));
        assert!(second <= Duration::from_secs(60));
        assert!(later <= Duration::from_secs(60));
    }

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
