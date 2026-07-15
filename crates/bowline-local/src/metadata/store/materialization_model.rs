#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaterializationFailureKind {
    PathFenceNotCurrent,
    ContentMissing,
    TransportUnavailable,
    RemoteTimeout,
    RemoteServiceUnavailable,
    RemoteRateLimited,
    AuthorizationRequired,
    ContentIntegrityFailed,
    HydrationFailed,
    LocalIoFailed,
    InvalidHydrationMetadata,
    UnsupportedHydration,
    WorkspaceMutationFailed,
    RetryBudgetExhausted,
}

impl MaterializationFailureKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PathFenceNotCurrent => "path-fence-not-current",
            Self::ContentMissing => "content-missing",
            Self::TransportUnavailable => "transport-unavailable",
            Self::RemoteTimeout => "remote-timeout",
            Self::RemoteServiceUnavailable => "remote-service-unavailable",
            Self::RemoteRateLimited => "remote-rate-limited",
            Self::AuthorizationRequired => "authorization-required",
            Self::ContentIntegrityFailed => "content-integrity-failed",
            Self::HydrationFailed => "hydration-failed",
            Self::LocalIoFailed => "local-io-failed",
            Self::InvalidHydrationMetadata => "invalid-hydration-metadata",
            Self::UnsupportedHydration => "unsupported-hydration",
            Self::WorkspaceMutationFailed => "workspace-mutation-failed",
            Self::RetryBudgetExhausted => "retry-budget-exhausted",
        }
    }

    pub(super) fn from_wire(value: &str) -> Option<Self> {
        match value {
            "path-fence-not-current" => Some(Self::PathFenceNotCurrent),
            "content-missing" => Some(Self::ContentMissing),
            "transport-unavailable" => Some(Self::TransportUnavailable),
            "remote-timeout" => Some(Self::RemoteTimeout),
            "remote-service-unavailable" => Some(Self::RemoteServiceUnavailable),
            "remote-rate-limited" => Some(Self::RemoteRateLimited),
            "authorization-required" => Some(Self::AuthorizationRequired),
            "content-integrity-failed" => Some(Self::ContentIntegrityFailed),
            "hydration-failed" => Some(Self::HydrationFailed),
            "local-io-failed" => Some(Self::LocalIoFailed),
            "invalid-hydration-metadata" => Some(Self::InvalidHydrationMetadata),
            "unsupported-hydration" => Some(Self::UnsupportedHydration),
            "workspace-mutation-failed" => Some(Self::WorkspaceMutationFailed),
            "retry-budget-exhausted" => Some(Self::RetryBudgetExhausted),
            _ => None,
        }
    }
}

pub const MATERIALIZATION_TASK_LEASE_SECONDS: i64 = 60;
pub const MATERIALIZATION_TASK_HEARTBEAT_SECONDS: u64 = 15;
