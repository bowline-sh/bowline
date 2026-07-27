//! What a failed daemon connection actually means.
//!
//! Every caller used to collapse a failed `DaemonClient::connect` into "the
//! daemon is stopped", which is a flat lie when a daemon is running but speaks a
//! different wire generation, or when the socket is present but unresponsive.
//! This is the one classification both the `bowline` CLI and the daemon's own
//! CLI render from.

use std::{fmt, io};

use bowline_core::wire::generated::DaemonRpcErrorCode;

use crate::{
    client::ClientError,
    negotiation::{VersionDimension, VersionWindow},
};

/// Why a daemon RPC call could not be served.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonReachability {
    /// Nothing is listening: no daemon is running, and starting one is correct.
    NotRunning,
    /// A daemon answered the handshake and speaks an incompatible generation.
    /// Starting a second daemon cannot help; the resident one must be replaced.
    VersionSkew(Box<VersionSkew>),
    /// A daemon may well be running; this process could not talk to it. Callers
    /// must not report this as "stopped".
    Unreachable,
}

impl DaemonReachability {
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::NotRunning => "stopped",
            Self::VersionSkew(_) => "version-skew",
            Self::Unreachable => "unreachable",
        }
    }

    /// Whether a caller may treat this as the ordinary "no daemon yet" state.
    #[must_use]
    pub const fn is_absent(&self) -> bool {
        matches!(self, Self::NotRunning)
    }

    #[must_use]
    pub fn version_skew(&self) -> Option<&VersionSkew> {
        match self {
            Self::VersionSkew(skew) => Some(skew),
            Self::NotRunning | Self::Unreachable => None,
        }
    }

    /// The one-line remediation a human or agent should act on.
    #[must_use]
    pub fn remediation(&self) -> &'static str {
        match self {
            Self::NotRunning => "run `bowline daemon start`",
            Self::VersionSkew(_) => "run `bowline daemon restart`",
            Self::Unreachable => "run `bowline daemon status` for the underlying error",
        }
    }
}

impl fmt::Display for DaemonReachability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotRunning => formatter.write_str("no daemon is running"),
            Self::VersionSkew(skew) => write!(formatter, "{skew}"),
            Self::Unreachable => formatter.write_str("the daemon could not be reached"),
        }
    }
}

/// A running daemon whose wire generation does not overlap this build's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionSkew {
    /// The dimension that failed, when the peer told us which one.
    pub dimension: Option<VersionDimension>,
    /// The generation the peer declared, when it reached us.
    pub peer_version: Option<u16>,
    /// This build's compatibility range for that dimension.
    pub window: Option<VersionWindow>,
    /// The build the daemon says a client needs, when it populates the field.
    pub required_client_version: Option<String>,
    /// The peer's own rendering of the failure.
    pub detail: String,
}

impl fmt::Display for VersionSkew {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "a daemon speaking a different wire generation is running: {}",
            self.detail
        )
    }
}

impl ClientError {
    /// Classify this failure for callers that must decide between reporting a
    /// stopped daemon, refusing to start a second one, and surfacing an error.
    #[must_use]
    pub fn reachability(&self) -> DaemonReachability {
        match self {
            Self::Io { source, .. } => io_reachability(source),
            Self::IncompatibleVersion {
                dimension,
                peer_version,
                window,
            } => DaemonReachability::VersionSkew(Box::new(VersionSkew {
                dimension: Some(*dimension),
                peer_version: Some(*peer_version),
                window: Some(*window),
                required_client_version: None,
                detail: self.to_string(),
            })),
            // The daemon rejected our hello: it is running and refusing us.
            Self::Remote(error) if error.code == DaemonRpcErrorCode::UnsupportedVersion => {
                DaemonReachability::VersionSkew(Box::new(VersionSkew {
                    dimension: None,
                    peer_version: None,
                    window: None,
                    required_client_version: error.required_client_version.clone(),
                    detail: error.message.clone(),
                }))
            }
            _ => DaemonReachability::Unreachable,
        }
    }
}

fn io_reachability(source: &io::Error) -> DaemonReachability {
    match source.kind() {
        // No socket file, or a socket with no listener behind it.
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => {
            DaemonReachability::NotRunning
        }
        _ => DaemonReachability::Unreachable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bowline_core::wire::generated::DaemonRpcError;

    #[test]
    fn a_missing_socket_is_a_stopped_daemon() {
        let error = ClientError::Io {
            operation: "connect",
            source: io::Error::from(io::ErrorKind::NotFound),
        };
        assert_eq!(error.reachability(), DaemonReachability::NotRunning);
        assert!(error.reachability().is_absent());
    }

    #[test]
    fn a_permission_error_is_not_a_stopped_daemon() {
        let error = ClientError::Io {
            operation: "connect",
            source: io::Error::from(io::ErrorKind::PermissionDenied),
        };
        assert_eq!(error.reachability(), DaemonReachability::Unreachable);
        assert!(!error.reachability().is_absent());
    }

    #[test]
    fn a_rejected_handshake_is_reported_as_skew_with_its_required_version() {
        let error = ClientError::Remote(Box::new(DaemonRpcError {
            code: DaemonRpcErrorCode::UnsupportedVersion,
            message: "peer machine-contract version 7 is outside the supported range 8..=8"
                .to_string(),
            retryable: false,
            retry_after_ms: None,
            operation_id: None,
            required_client_version: Some("0.9.0".to_string()),
            details: None,
        }));
        let reachability = error.reachability();
        let skew = reachability.version_skew().expect("skew is classified");
        assert_eq!(skew.required_client_version.as_deref(), Some("0.9.0"));
        assert!(!reachability.is_absent());
        assert_eq!(reachability.remediation(), "run `bowline daemon restart`");
    }

    #[test]
    fn a_locally_detected_window_miss_is_skew() {
        let error = ClientError::IncompatibleVersion {
            dimension: VersionDimension::MachineContract,
            peer_version: 4,
            window: VersionWindow {
                minimum: 8,
                supported: 8,
            },
        };
        let reachability = error.reachability();
        let skew = reachability.version_skew().expect("skew is classified");
        assert_eq!(skew.peer_version, Some(4));
        assert_eq!(skew.dimension, Some(VersionDimension::MachineContract));
    }
}
