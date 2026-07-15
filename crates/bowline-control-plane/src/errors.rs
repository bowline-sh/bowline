use std::{error::Error, fmt};

use bowline_core::ids::{DeviceApprovalRequestId, LeaseId, WorkViewId, WorkspaceId};

use crate::{StaleWorkViewOverlayHead, StaleWorkspaceRef};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Capability {
    WorkspaceRefHistory,
    StorageGc,
    ObjectMetadata,
    WorkViews,
    AgentLeases,
    DeviceBootstrap,
    DeviceTrust,
    RecoveryKey,
}

impl Capability {
    pub const ALL: &'static [Capability] = &[
        Capability::WorkspaceRefHistory,
        Capability::StorageGc,
        Capability::ObjectMetadata,
        Capability::WorkViews,
        Capability::AgentLeases,
        Capability::DeviceBootstrap,
        Capability::DeviceTrust,
        Capability::RecoveryKey,
    ];

    pub fn as_wire(self) -> &'static str {
        match self {
            Self::WorkspaceRefHistory => "workspace-ref-history",
            Self::StorageGc => "storage-gc",
            Self::ObjectMetadata => "object-metadata",
            Self::WorkViews => "work-views",
            Self::AgentLeases => "agent-leases",
            Self::DeviceBootstrap => "device-bootstrap",
            Self::DeviceTrust => "device-trust",
            Self::RecoveryKey => "recovery-key",
        }
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_wire())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompareAndSwapError {
    WorkspaceMissing {
        workspace_id: WorkspaceId,
    },
    StaleRef(StaleWorkspaceRef),
    Storage(String),
    Unsupported {
        capability: &'static str,
        reason: &'static str,
    },
}

impl fmt::Display for CompareAndSwapError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WorkspaceMissing { workspace_id } => {
                write!(
                    formatter,
                    "workspace `{}` does not exist",
                    workspace_id.as_str()
                )
            }
            Self::StaleRef(stale) => write!(
                formatter,
                "workspace `{}` is at version {}, not expected version {}",
                stale.current.workspace_id.as_str(),
                stale.current.version,
                stale.expected_version
            ),
            Self::Storage(error) => write!(formatter, "control-plane storage failed: {error}"),
            Self::Unsupported { capability, reason } => {
                write!(formatter, "{capability} is unsupported: {reason}")
            }
        }
    }
}

impl Error for CompareAndSwapError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectionCode {
    DeviceNotTrusted,
    InvalidRequest,
    Unauthorized,
    WorkspaceMembershipRequired,
    WorkspaceOwnerRequired,
    Unknown,
}

impl RejectionCode {
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::DeviceNotTrusted => "control_plane/device_not_trusted",
            Self::InvalidRequest => "control_plane/invalid_request",
            Self::Unauthorized => "control_plane/unauthorized",
            Self::WorkspaceMembershipRequired => "control_plane/workspace_membership_required",
            Self::WorkspaceOwnerRequired => "control_plane/workspace_owner_required",
            Self::Unknown => "control_plane/unknown",
        }
    }

    pub fn from_wire(code: &str) -> Self {
        match code {
            "control_plane/device_not_trusted" => Self::DeviceNotTrusted,
            "control_plane/invalid_request" => Self::InvalidRequest,
            "control_plane/unauthorized" => Self::Unauthorized,
            "control_plane/workspace_membership_required" => Self::WorkspaceMembershipRequired,
            "control_plane/workspace_owner_required" => Self::WorkspaceOwnerRequired,
            _ => Self::Unknown,
        }
    }
}

impl fmt::Display for RejectionCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_wire())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlPlaneError {
    Timeout {
        capability: &'static str,
    },
    Transport {
        detail: String,
    },
    Rejected {
        code: RejectionCode,
        message: String,
    },
    WorkspaceMissing {
        workspace_id: WorkspaceId,
    },
    WorkViewMissing {
        work_view_id: WorkViewId,
    },
    LeaseMissing {
        lease_id: LeaseId,
    },
    CompareAndSwap(CompareAndSwapError),
    InvalidObjectKey {
        reason: &'static str,
    },
    ObjectMissing {
        object_key: String,
    },
    DeviceRequestMissing {
        request_id: DeviceApprovalRequestId,
    },
    Limited {
        capability: Capability,
        reason: &'static str,
    },
    Unsupported {
        capability: &'static str,
        reason: &'static str,
    },
    Conflict {
        resource: &'static str,
        reason: &'static str,
    },
    Storage(String),
}

impl fmt::Display for ControlPlaneError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout { capability } => {
                write!(formatter, "{capability} request timed out")
            }
            Self::Transport { detail } => {
                write!(formatter, "control-plane transport failed: {detail}")
            }
            Self::Rejected { code, message } => {
                write!(
                    formatter,
                    "control-plane rejected request ({code}): {message}"
                )
            }
            Self::WorkspaceMissing { workspace_id } => {
                write!(
                    formatter,
                    "workspace `{}` does not exist",
                    workspace_id.as_str()
                )
            }
            Self::WorkViewMissing { work_view_id } => {
                write!(
                    formatter,
                    "work view `{}` does not exist",
                    work_view_id.as_str()
                )
            }
            Self::LeaseMissing { lease_id } => {
                write!(formatter, "lease `{}` does not exist", lease_id.as_str())
            }
            Self::CompareAndSwap(error) => error.fmt(formatter),
            Self::InvalidObjectKey { reason } => {
                write!(formatter, "object key is invalid: {reason}")
            }
            Self::ObjectMissing { object_key } => {
                write!(formatter, "object `{object_key}` does not exist")
            }
            Self::DeviceRequestMissing { request_id } => {
                write!(
                    formatter,
                    "device request `{}` does not exist",
                    request_id.as_str()
                )
            }
            Self::Limited { capability, reason } => {
                write!(formatter, "{capability} is limited in this phase: {reason}")
            }
            Self::Unsupported { capability, reason } => {
                write!(formatter, "{capability} is unsupported: {reason}")
            }
            Self::Conflict { resource, reason } => {
                write!(
                    formatter,
                    "{resource} conflicts with existing metadata: {reason}"
                )
            }
            Self::Storage(error) => write!(formatter, "storage failed: {error}"),
        }
    }
}

impl Error for ControlPlaneError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CompareAndSwap(error) => Some(error),
            Self::Timeout { .. }
            | Self::Transport { .. }
            | Self::Rejected { .. }
            | Self::WorkspaceMissing { .. }
            | Self::WorkViewMissing { .. }
            | Self::LeaseMissing { .. }
            | Self::InvalidObjectKey { .. }
            | Self::ObjectMissing { .. }
            | Self::DeviceRequestMissing { .. }
            | Self::Limited { .. }
            | Self::Unsupported { .. }
            | Self::Conflict { .. }
            | Self::Storage(_) => None,
        }
    }
}

#[cfg(test)]
mod rejection_code_tests {
    use super::RejectionCode;

    #[test]
    fn workspace_access_codes_round_trip_the_canonical_wire_values() {
        for code in [
            RejectionCode::WorkspaceMembershipRequired,
            RejectionCode::WorkspaceOwnerRequired,
        ] {
            assert_eq!(RejectionCode::from_wire(code.as_wire()), code);
        }
    }
}

impl From<CompareAndSwapError> for ControlPlaneError {
    fn from(error: CompareAndSwapError) -> Self {
        match error {
            CompareAndSwapError::WorkspaceMissing { workspace_id } => {
                Self::WorkspaceMissing { workspace_id }
            }
            error => Self::CompareAndSwap(error),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkViewUpdateError {
    WorkViewMissing {
        work_view_id: WorkViewId,
    },
    StaleOverlayHead(Box<StaleWorkViewOverlayHead>),
    Storage(String),
    Unsupported {
        capability: Capability,
        reason: &'static str,
    },
}

impl fmt::Display for WorkViewUpdateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WorkViewMissing { work_view_id } => {
                write!(
                    formatter,
                    "work view `{}` does not exist",
                    work_view_id.as_str()
                )
            }
            Self::StaleOverlayHead(stale) => write!(
                formatter,
                "work view `{}` overlay is at version {}, not expected version {}",
                stale.current.work_view_id.as_str(),
                stale.current.overlay_version,
                stale.expected_overlay_version
            ),
            Self::Storage(error) => write!(formatter, "control-plane storage failed: {error}"),
            Self::Unsupported { capability, reason } => {
                write!(formatter, "{capability} is unsupported: {reason}")
            }
        }
    }
}

impl Error for WorkViewUpdateError {}

impl From<ControlPlaneError> for WorkViewUpdateError {
    fn from(error: ControlPlaneError) -> Self {
        match error {
            ControlPlaneError::WorkViewMissing { work_view_id } => {
                Self::WorkViewMissing { work_view_id }
            }
            error => Self::Storage(error.to_string()),
        }
    }
}
