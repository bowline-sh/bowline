//! Machine-readable introspection primitives shared by the CLI's `version`,
//! `status`, and `sync wait` surfaces.
//!
//! These types give test harnesses and the release captain a stable, semantic
//! view of device and workspace readiness so they never infer state from log
//! lines, file names, or sleeps. Every value here is *derived* from live
//! account, device-trust, and daemon-sync state and serialized at the edge; it
//! is never persisted or sent to the hosted service.

use serde::{Deserialize, Serialize};

use crate::status::SyncQueueStatus;

/// The OS-level service supervisor that owns the Bowline daemon on this host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ServiceManager {
    Launchd,
    Systemd,
    None,
}

impl ServiceManager {
    pub const ALL: &'static [Self] = &[Self::Launchd, Self::Systemd, Self::None];

    pub const fn token(self) -> &'static str {
        match self {
            Self::Launchd => "launchd",
            Self::Systemd => "systemd",
            Self::None => "none",
        }
    }

    pub fn from_token(token: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|value| value.token() == token)
    }
}

/// Compact daemon-service view for `bowline status --json`. `state` is the raw
/// supervisor state string (`running`, `stopped`, `unavailable`, …) owned by the
/// service layer; `manager` names which supervisor produced it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceIntrospection {
    pub state: String,
    pub manager: ServiceManager,
}

/// Where this device sits on the account/device authentication ladder.
///
/// This is a reduction of account-login and device-trust state onto the single
/// semantic axis harnesses wait on. Ordering is ladder order: a device that is
/// `Authenticated` is at or past `ApprovalPending`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthenticationState {
    Unauthenticated,
    ApprovalPending,
    Authenticated,
}

impl AuthenticationState {
    pub const ALL: &'static [Self] = &[
        Self::Unauthenticated,
        Self::ApprovalPending,
        Self::Authenticated,
    ];

    pub const fn token(self) -> &'static str {
        match self {
            Self::Unauthenticated => "unauthenticated",
            Self::ApprovalPending => "approval-pending",
            Self::Authenticated => "authenticated",
        }
    }

    pub fn from_token(token: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|value| value.token() == token)
    }
}

/// Compact authentication view for `bowline status --json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthenticationIntrospection {
    pub state: AuthenticationState,
}

/// Live sync activity for this workspace on this device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SyncActivityState {
    Idle,
    Syncing,
    Offline,
    Attention,
}

impl SyncActivityState {
    pub const ALL: &'static [Self] = &[Self::Idle, Self::Syncing, Self::Offline, Self::Attention];

    pub const fn token(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Syncing => "syncing",
            Self::Offline => "offline",
            Self::Attention => "attention",
        }
    }

    pub fn from_token(token: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|value| value.token() == token)
    }

    /// `Idle` is the only state in which the workspace is fully caught up; every
    /// other state means work is in flight, blocked, or needs a human.
    pub fn is_settled(self) -> bool {
        matches!(self, Self::Idle)
    }
}

/// Compact sync view for `bowline status --json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncIntrospection {
    pub state: SyncActivityState,
    pub pending_uploads: u64,
    pub pending_downloads: u64,
}

impl SyncIntrospection {
    /// Project the daemon's operation queue onto the direction-split shape
    /// consumers expect. The queue tracks *operations*, not a per-direction
    /// split, so outbound push lanes (queued/claimed/retry/offline) count as
    /// uploads and the inbound reconciliation lane counts as downloads.
    pub fn from_queue(queue: &SyncQueueStatus) -> Self {
        let pending_uploads =
            queue.queued + queue.claimed + queue.waiting_retry + queue.blocked_offline;
        let pending_downloads = queue.reconciliation_required;
        let state = if queue.blocked_offline > 0 {
            SyncActivityState::Offline
        } else if queue.attention > 0 {
            SyncActivityState::Attention
        } else if pending_uploads > 0 || pending_downloads > 0 {
            SyncActivityState::Syncing
        } else {
            SyncActivityState::Idle
        };
        Self {
            state,
            pending_uploads,
            pending_downloads,
        }
    }

    pub fn is_settled(&self) -> bool {
        self.state.is_settled() && self.pending_uploads == 0 && self.pending_downloads == 0
    }
}

/// Semantic readiness ladder the daemon trust-and-sync flow climbs, exposed for
/// `bowline sync wait`. Ordering is ladder order: a device at or past the
/// requested rung satisfies a wait for that rung.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkspaceReadiness {
    Unauthenticated,
    ApprovalPending,
    Authenticated,
    Ready,
}

impl WorkspaceReadiness {
    pub const ALL: &'static [Self] = &[
        Self::Unauthenticated,
        Self::ApprovalPending,
        Self::Authenticated,
        Self::Ready,
    ];

    pub const fn token(self) -> &'static str {
        match self {
            Self::Unauthenticated => "unauthenticated",
            Self::ApprovalPending => "approval-pending",
            Self::Authenticated => "authenticated",
            Self::Ready => "ready",
        }
    }

    pub fn from_token(token: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|value| value.token() == token)
    }

    /// Compose the authentication ladder with sync settledness into overall
    /// readiness. A device is `Ready` only once it is authenticated *and* the
    /// daemon reports the workspace fully caught up.
    pub fn derive(auth: AuthenticationState, sync_settled: bool) -> Self {
        match auth {
            AuthenticationState::Unauthenticated => Self::Unauthenticated,
            AuthenticationState::ApprovalPending => Self::ApprovalPending,
            AuthenticationState::Authenticated if sync_settled => Self::Ready,
            AuthenticationState::Authenticated => Self::Authenticated,
        }
    }

    /// A wait for `target` is satisfied once the observed rung is at or past it.
    pub fn satisfies(self, target: Self) -> bool {
        self >= target
    }
}

/// Host platform label in the vocabulary contract consumers expect
/// (`macos`/`linux`), with an honest passthrough for any other host.
pub fn platform_label() -> &'static str {
    match std::env::consts::OS {
        "macos" => "macos",
        "linux" => "linux",
        other => other,
    }
}

/// Host CPU architecture label (`aarch64`, `x86_64`, …).
pub fn architecture_label() -> &'static str {
    std::env::consts::ARCH
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readiness_tokens_round_trip() {
        for readiness in WorkspaceReadiness::ALL.iter().copied() {
            assert_eq!(
                WorkspaceReadiness::from_token(readiness.token()),
                Some(readiness)
            );
        }
        assert_eq!(WorkspaceReadiness::from_token("bogus"), None);
    }

    #[test]
    fn readiness_ladder_is_ordered() {
        assert!(WorkspaceReadiness::Ready > WorkspaceReadiness::Authenticated);
        assert!(WorkspaceReadiness::Authenticated > WorkspaceReadiness::ApprovalPending);
        assert!(WorkspaceReadiness::ApprovalPending > WorkspaceReadiness::Unauthenticated);
    }

    #[test]
    fn satisfies_is_at_or_past_target() {
        assert!(WorkspaceReadiness::Ready.satisfies(WorkspaceReadiness::Authenticated));
        assert!(WorkspaceReadiness::Authenticated.satisfies(WorkspaceReadiness::Authenticated));
        assert!(!WorkspaceReadiness::ApprovalPending.satisfies(WorkspaceReadiness::Authenticated));
    }

    #[test]
    fn derive_maps_auth_and_sync_onto_the_ladder() {
        assert_eq!(
            WorkspaceReadiness::derive(AuthenticationState::Unauthenticated, true),
            WorkspaceReadiness::Unauthenticated
        );
        assert_eq!(
            WorkspaceReadiness::derive(AuthenticationState::ApprovalPending, true),
            WorkspaceReadiness::ApprovalPending
        );
        assert_eq!(
            WorkspaceReadiness::derive(AuthenticationState::Authenticated, false),
            WorkspaceReadiness::Authenticated
        );
        assert_eq!(
            WorkspaceReadiness::derive(AuthenticationState::Authenticated, true),
            WorkspaceReadiness::Ready
        );
    }

    #[test]
    fn sync_introspection_splits_queue_direction() {
        let queue = SyncQueueStatus {
            queued: 2,
            claimed: 1,
            waiting_retry: 1,
            blocked_offline: 0,
            reconciliation_required: 3,
            attention: 0,
            completed: 5,
        };
        let sync = SyncIntrospection::from_queue(&queue);
        assert_eq!(sync.pending_uploads, 4);
        assert_eq!(sync.pending_downloads, 3);
        assert_eq!(sync.state, SyncActivityState::Syncing);
        assert!(!sync.is_settled());
    }

    #[test]
    fn empty_queue_is_idle_and_settled() {
        let sync = SyncIntrospection::from_queue(&SyncQueueStatus::default());
        assert_eq!(sync.state, SyncActivityState::Idle);
        assert!(sync.is_settled());
    }

    #[test]
    fn offline_lane_wins_over_syncing() {
        let queue = SyncQueueStatus {
            blocked_offline: 1,
            ..SyncQueueStatus::default()
        };
        let sync = SyncIntrospection::from_queue(&queue);
        assert_eq!(sync.state, SyncActivityState::Offline);
        assert_eq!(sync.pending_uploads, 1);
    }

    #[test]
    fn attention_lane_surfaces_over_plain_syncing() {
        let queue = SyncQueueStatus {
            attention: 1,
            ..SyncQueueStatus::default()
        };
        assert_eq!(
            SyncIntrospection::from_queue(&queue).state,
            SyncActivityState::Attention
        );
    }

    #[test]
    fn service_manager_tokens_round_trip() {
        for manager in ServiceManager::ALL.iter().copied() {
            assert_eq!(ServiceManager::from_token(manager.token()), Some(manager));
        }
    }
}
