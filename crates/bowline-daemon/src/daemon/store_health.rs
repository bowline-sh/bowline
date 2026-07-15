use std::cell::Cell;
use std::fmt;

pub(super) struct StoreHealth {
    consecutive_failures: Cell<u32>,
    total_failures: Cell<u64>,
    degraded_status_visible: Cell<bool>,
}

impl StoreHealth {
    pub(super) fn new() -> Self {
        Self {
            consecutive_failures: Cell::new(0),
            total_failures: Cell::new(0),
            degraded_status_visible: Cell::new(false),
        }
    }

    pub(super) fn record<T, E: fmt::Display>(
        &self,
        context: &'static str,
        result: Result<T, E>,
    ) -> Option<T> {
        match result {
            Ok(value) => {
                // A failure may clear only after "degraded" was durably visible locally.
                if self.degraded_status_visible.replace(false) && self.is_degraded() {
                    self.consecutive_failures.set(0);
                }
                Some(value)
            }
            Err(error) => {
                self.consecutive_failures
                    .set(self.consecutive_failures.get().saturating_add(1));
                self.total_failures
                    .set(self.total_failures.get().saturating_add(1));
                self.degraded_status_visible.set(false);
                eprintln!("bowline-daemon store write failed ({context}): {error}");
                None
            }
        }
    }

    pub(super) fn mark_degraded_status_written(&self) {
        if self.is_degraded() {
            self.degraded_status_visible.set(true);
        }
    }

    pub(super) fn is_degraded(&self) -> bool {
        self.consecutive_failures.get() > 0
    }

    /// Monotonic count of every recorded failure (never reset by recovery).
    /// Callers snapshot this around a store-access closure to detect failures
    /// the closure swallowed (recorded but not propagated).
    pub(super) fn total_failure_count(&self) -> u64 {
        self.total_failures.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovers_after_degraded_status_written_and_store_write_succeeds() {
        let health = StoreHealth::new();

        let failed: Option<()> = health.record("test", Err("locked"));
        assert_eq!(failed, None);
        assert!(health.is_degraded());

        health.mark_degraded_status_written();
        assert!(health.is_degraded());

        assert_eq!(health.record::<_, &str>("test", Ok(7)), Some(7));
        assert!(!health.is_degraded());
    }

    #[test]
    fn success_before_visibility_does_not_recover() {
        let health = StoreHealth::new();

        let failed: Option<()> = health.record("test", Err("locked"));
        assert_eq!(failed, None);
        assert!(health.is_degraded());

        assert_eq!(health.record::<_, &str>("test", Ok(7)), Some(7));
        assert!(health.is_degraded());

        health.mark_degraded_status_written();
        assert_eq!(health.record::<_, &str>("test", Ok(8)), Some(8));
        assert!(!health.is_degraded());
    }

    #[test]
    fn new_failure_rearms_visibility_requirement() {
        let health = StoreHealth::new();

        let first_failed: Option<()> = health.record("test", Err("locked"));
        assert_eq!(first_failed, None);
        health.mark_degraded_status_written();

        let second_failed: Option<()> = health.record("test", Err("still locked"));
        assert_eq!(second_failed, None);
        assert!(health.is_degraded());

        assert_eq!(health.record::<_, &str>("test", Ok(7)), Some(7));
        assert!(health.is_degraded());

        health.mark_degraded_status_written();
        assert_eq!(health.record::<_, &str>("test", Ok(8)), Some(8));
        assert!(!health.is_degraded());
    }
}
