use std::{error::Error, fmt};

use crate::{StaleWorkViewOverlayHead, StaleWorkspaceRef};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompareAndSwapError {
    WorkspaceMissing {
        workspace_id: String,
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
                write!(formatter, "workspace `{workspace_id}` does not exist")
            }
            Self::StaleRef(stale) => write!(
                formatter,
                "workspace `{}` is at version {}, not expected version {}",
                stale.current.workspace_id, stale.current.version, stale.expected_version
            ),
            Self::Storage(error) => write!(formatter, "control-plane storage failed: {error}"),
            Self::Unsupported { capability, reason } => {
                write!(formatter, "{capability} is unsupported: {reason}")
            }
        }
    }
}

impl Error for CompareAndSwapError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlPlaneError {
    WorkspaceMissing {
        workspace_id: String,
    },
    WorkViewMissing {
        work_view_id: String,
    },
    LeaseMissing {
        lease_id: String,
    },
    CompareAndSwap(CompareAndSwapError),
    InvalidObjectKey {
        reason: &'static str,
    },
    ObjectMissing {
        object_key: String,
    },
    DeviceRequestMissing {
        request_id: String,
    },
    Limited {
        capability: &'static str,
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
            Self::WorkspaceMissing { workspace_id } => {
                write!(formatter, "workspace `{workspace_id}` does not exist")
            }
            Self::WorkViewMissing { work_view_id } => {
                write!(formatter, "work view `{work_view_id}` does not exist")
            }
            Self::LeaseMissing { lease_id } => {
                write!(formatter, "lease `{lease_id}` does not exist")
            }
            Self::CompareAndSwap(error) => error.fmt(formatter),
            Self::InvalidObjectKey { reason } => {
                write!(formatter, "object key is invalid: {reason}")
            }
            Self::ObjectMissing { object_key } => {
                write!(formatter, "object `{object_key}` does not exist")
            }
            Self::DeviceRequestMissing { request_id } => {
                write!(formatter, "device request `{request_id}` does not exist")
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
            _ => None,
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
        work_view_id: String,
    },
    StaleOverlayHead(Box<StaleWorkViewOverlayHead>),
    Storage(String),
    Unsupported {
        capability: &'static str,
        reason: &'static str,
    },
}

impl fmt::Display for WorkViewUpdateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WorkViewMissing { work_view_id } => {
                write!(formatter, "work view `{work_view_id}` does not exist")
            }
            Self::StaleOverlayHead(stale) => write!(
                formatter,
                "work view `{}` overlay is at version {}, not expected version {}",
                stale.current.work_view_id,
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
