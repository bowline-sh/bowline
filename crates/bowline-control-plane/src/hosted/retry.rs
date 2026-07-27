use std::time::Duration;

use crate::{ControlPlaneError, Retryability};

/// Total attempts (including the first) for an idempotent control-plane call.
const RPC_ATTEMPTS: u32 = 3;
const RPC_BASE_DELAY: Duration = Duration::from_millis(250);
const RPC_MAX_DELAY: Duration = Duration::from_secs(5);

/// Retry budget for one control-plane call.
///
/// Full jitter matters more than the exponential curve here: a fleet of agent
/// hosts waking together would otherwise retry in lockstep and re-form the
/// thundering herd that caused the first failure.
pub(super) struct RpcAttempt {
    attempts_left: u32,
    exponent: u32,
}

impl RpcAttempt {
    /// `idempotent` is false for mutations and actions, which must never be
    /// re-issued: the client cannot tell a lost response from a lost request.
    pub(super) fn first(idempotent: bool) -> Self {
        Self {
            attempts_left: if idempotent { RPC_ATTEMPTS - 1 } else { 0 },
            exponent: 0,
        }
    }

    /// How long to wait before the next attempt, or `None` when the error is
    /// terminal or the budget is spent.
    pub(super) fn backoff_after(&mut self, error: &ControlPlaneError) -> Option<Duration> {
        if self.attempts_left == 0 || error.retryability() != Retryability::Retryable {
            return None;
        }
        self.attempts_left -= 1;
        let ceiling = RPC_BASE_DELAY
            .saturating_mul(1_u32 << self.exponent.min(16))
            .min(RPC_MAX_DELAY);
        self.exponent += 1;
        Some(full_jitter(ceiling))
    }
}

/// Uniform draw over `[0, ceiling]`. A failed randomness draw degrades to the
/// undithered ceiling rather than to no wait at all.
fn full_jitter(ceiling: Duration) -> Duration {
    let mut bytes = [0_u8; 8];
    if getrandom::fill(&mut bytes).is_err() {
        return ceiling;
    }
    let fraction = u64::from_le_bytes(bytes) as f64 / u64::MAX as f64;
    ceiling.mul_f64(fraction)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RejectionCode;

    fn transport() -> ControlPlaneError {
        ControlPlaneError::Transport {
            detail: "reset".to_string(),
        }
    }

    #[test]
    fn non_idempotent_calls_never_retry() {
        assert!(
            RpcAttempt::first(false)
                .backoff_after(&transport())
                .is_none()
        );
    }

    #[test]
    fn idempotent_calls_retry_within_budget_and_stay_bounded() {
        let mut attempt = RpcAttempt::first(true);
        let mut delays = Vec::new();
        while let Some(delay) = attempt.backoff_after(&transport()) {
            assert!(delay <= RPC_MAX_DELAY);
            delays.push(delay);
        }
        assert_eq!(delays.len() as u32, RPC_ATTEMPTS - 1);
    }

    #[test]
    fn fatal_errors_are_not_retried_even_with_budget_left() {
        let fatal = ControlPlaneError::Rejected {
            code: RejectionCode::DeviceNotTrusted,
            message: "revoked".to_string(),
        };
        assert!(RpcAttempt::first(true).backoff_after(&fatal).is_none());
    }

    #[test]
    fn expired_sessions_are_not_retried() {
        let expired = ControlPlaneError::Rejected {
            code: RejectionCode::Unauthorized,
            message: "session expired".to_string(),
        };
        assert!(RpcAttempt::first(true).backoff_after(&expired).is_none());
    }
}
