use std::{
    fmt,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use bowline_core::wire::generated::DaemonRpcErrorCode;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(super) struct RpcConnectionId(u64);

impl RpcConnectionId {
    pub(super) const fn new(value: u64) -> Self {
        Self(value)
    }

    pub(super) const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct RpcRequestId(String);

impl RpcRequestId {
    pub(super) fn new(value: String) -> Self {
        Self(value)
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CorrelationId(String);

impl CorrelationId {
    fn for_request(connection_id: RpcConnectionId, sequence: u64) -> Self {
        Self(format!("rpc-{}-{sequence}", connection_id.get()))
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CancellationReason {
    Cancelled,
    DeadlineExceeded,
    Disconnected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CancellationDisposition {
    TerminalNow,
    DeferredUntilCompletion,
}

#[derive(Debug)]
struct CancellationState {
    lifecycle: Mutex<RequestLifecycle>,
}

#[derive(Debug, Default)]
struct RequestLifecycle {
    reason: Option<CancellationReason>,
    requested_at: Option<Instant>,
    commit_fence_started: bool,
}

#[derive(Debug, Clone)]
pub(super) struct CancellationToken {
    state: Arc<CancellationState>,
}

impl CancellationToken {
    pub(super) fn new() -> Self {
        Self {
            state: Arc::new(CancellationState {
                lifecycle: Mutex::new(RequestLifecycle::default()),
            }),
        }
    }

    pub(super) fn cancel(&self, reason: CancellationReason) -> bool {
        let mut lifecycle = self
            .state
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if lifecycle.reason.is_some() {
            return false;
        }
        lifecycle.requested_at = Some(Instant::now());
        lifecycle.reason = Some(reason);
        true
    }

    pub(super) fn reason(&self) -> Option<CancellationReason> {
        self.state
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .reason
    }

    pub(super) fn cancellation_age(&self) -> Option<Duration> {
        self.state
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .requested_at
            .map(|requested_at| requested_at.elapsed())
    }

    pub(super) fn same_request(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CancellationPoint {
    HandlerStart,
    BeforeProjectionRead,
    BeforeDatabaseRead,
    BeforeDatabaseMutation,
    BeforeDurableEnqueue,
    BeforeExternalCall,
    BetweenChunks,
    BeforeCommitFence,
}

impl CancellationPoint {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::HandlerStart => "handler-start",
            Self::BeforeProjectionRead => "before-projection-read",
            Self::BeforeDatabaseRead => "before-database-read",
            Self::BeforeDatabaseMutation => "before-database-mutation",
            Self::BeforeDurableEnqueue => "before-durable-enqueue",
            Self::BeforeExternalCall => "before-external-call",
            Self::BetweenChunks => "between-chunks",
            Self::BeforeCommitFence => "before-commit-fence",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RequestContextError {
    reason: CancellationReason,
    point: CancellationPoint,
}

impl RequestContextError {
    pub(super) fn code(self) -> DaemonRpcErrorCode {
        match self.reason {
            CancellationReason::DeadlineExceeded => DaemonRpcErrorCode::DeadlineExceeded,
            CancellationReason::Cancelled | CancellationReason::Disconnected => {
                DaemonRpcErrorCode::Cancelled
            }
        }
    }

    pub(super) const fn message(self) -> &'static str {
        match self.reason {
            CancellationReason::DeadlineExceeded => "the daemon request deadline elapsed",
            CancellationReason::Cancelled => "the daemon request was cancelled",
            CancellationReason::Disconnected => "the daemon request connection closed",
        }
    }

    pub(super) const fn point(self) -> CancellationPoint {
        self.point
    }
}

impl fmt::Display for RequestContextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message())
    }
}

#[derive(Debug, Clone)]
pub(super) struct RequestContext {
    request_id: RpcRequestId,
    deadline: Option<Instant>,
    cancellation: CancellationToken,
    correlation_id: CorrelationId,
}

impl RequestContext {
    pub(super) fn new(
        connection_id: RpcConnectionId,
        correlation_sequence: u64,
        request_id: RpcRequestId,
        deadline: Option<Instant>,
    ) -> Self {
        Self {
            correlation_id: CorrelationId::for_request(connection_id, correlation_sequence),
            request_id,
            deadline,
            cancellation: CancellationToken::new(),
        }
    }

    pub(super) fn request_id(&self) -> &RpcRequestId {
        &self.request_id
    }

    pub(super) fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    pub(super) fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    pub(super) fn correlation_id(&self) -> &CorrelationId {
        &self.correlation_id
    }

    pub(super) fn checkpoint(&self, point: CancellationPoint) -> Result<(), RequestContextError> {
        let mut lifecycle = self
            .cancellation
            .state
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if lifecycle.commit_fence_started {
            return Ok(());
        }
        if lifecycle.reason.is_none()
            && self
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
        {
            lifecycle.requested_at = Some(Instant::now());
            lifecycle.reason = Some(CancellationReason::DeadlineExceeded);
        }
        lifecycle
            .reason
            .map_or(Ok(()), |reason| Err(RequestContextError { reason, point }))
    }

    pub(super) fn begin_commit_fence(&self) -> Result<(), RequestContextError> {
        let mut lifecycle = self
            .cancellation
            .state
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if lifecycle.reason.is_none()
            && self
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
        {
            lifecycle.requested_at = Some(Instant::now());
            lifecycle.reason = Some(CancellationReason::DeadlineExceeded);
        }
        if let Some(reason) = lifecycle.reason {
            return Err(RequestContextError {
                reason,
                point: CancellationPoint::BeforeCommitFence,
            });
        }
        lifecycle.commit_fence_started = true;
        Ok(())
    }

    pub(super) fn commit_fence_started(&self) -> bool {
        self.cancellation
            .state
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .commit_fence_started
    }

    pub(super) fn request_cancellation(
        &self,
        reason: CancellationReason,
    ) -> CancellationDisposition {
        let mut lifecycle = self
            .cancellation
            .state
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if lifecycle.reason.is_none() {
            lifecycle.requested_at = Some(Instant::now());
            lifecycle.reason = Some(reason);
        }
        if lifecycle.commit_fence_started {
            CancellationDisposition::DeferredUntilCompletion
        } else {
            CancellationDisposition::TerminalNow
        }
    }
}
