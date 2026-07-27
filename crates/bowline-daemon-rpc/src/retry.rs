//! Whether repeating an identical daemon RPC could answer differently.
//!
//! A daemon that has just been installed or restarted binds its control socket
//! before its subsystems attach, so the first calls after a restart fail with
//! conditions that clear on their own. Callers with a deadline — every `wait`
//! command — must sit through those and fail fast on everything else. This is
//! the one place that judgement is made; it reads the structured error code the
//! daemon sends, never the human message attached to it.

use std::io;

use bowline_core::wire::generated::DaemonRpcErrorCode;

use crate::client::ClientError;

/// What a failed daemon RPC means for a caller that is willing to wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryDisposition {
    /// The daemon cannot answer this call *yet*: nothing is bound to the socket,
    /// a restart is swapping the listener, or a live daemon's subsystem has not
    /// attached. An identical call can succeed once startup finishes.
    RetryWhileStarting,
    /// The call reached a live daemon and consumed a deadline. Retrying re-arms
    /// the same wait; it does not change what is being waited for.
    DeadlineElapsed,
    /// The daemon answered definitively, or the two builds cannot talk to each
    /// other. No amount of waiting changes the answer.
    Terminal,
}

impl ClientError {
    /// Classify this failure for callers that hold a deadline and must decide
    /// between waiting and reporting.
    #[must_use]
    pub fn retry_disposition(&self) -> RetryDisposition {
        match self {
            Self::Io { source, .. } => io_disposition(source.kind()),
            Self::Timeout { .. } => RetryDisposition::DeadlineElapsed,
            // A daemon replacing itself drops live connections; from this side
            // that is indistinguishable from a restart in progress.
            Self::ConnectionClosed { .. } => RetryDisposition::RetryWhileStarting,
            Self::Remote(error) => remote_disposition(&error.code),
            Self::Codec(_)
            | Self::SerializeParams(_)
            | Self::DeserializeResult(_)
            | Self::InvalidHandshake(_)
            | Self::IncompatibleVersion { .. }
            | Self::InvalidResponse { .. }
            | Self::InternalState(_) => RetryDisposition::Terminal,
        }
    }
}

fn io_disposition(kind: io::ErrorKind) -> RetryDisposition {
    match kind {
        // Nothing is listening yet, or the listener went away mid-call: exactly
        // what a daemon between service start and socket bind looks like.
        io::ErrorKind::NotFound
        | io::ErrorKind::ConnectionRefused
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::ConnectionAborted
        | io::ErrorKind::BrokenPipe
        | io::ErrorKind::AddrNotAvailable
        | io::ErrorKind::Interrupted => RetryDisposition::RetryWhileStarting,
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => RetryDisposition::DeadlineElapsed,
        // A socket this process may not open, or a path that is not a socket at
        // all, stays broken however long the caller waits.
        _ => RetryDisposition::Terminal,
    }
}

fn remote_disposition(code: &DaemonRpcErrorCode) -> RetryDisposition {
    match code {
        // The daemon is up, and the subsystem behind this method is not serving
        // yet or is briefly refusing work.
        DaemonRpcErrorCode::Unavailable
        | DaemonRpcErrorCode::Overloaded
        | DaemonRpcErrorCode::ResourceExhausted => RetryDisposition::RetryWhileStarting,
        DaemonRpcErrorCode::DeadlineExceeded | DaemonRpcErrorCode::Cancelled => {
            RetryDisposition::DeadlineElapsed
        }
        // Every remaining code is the daemon's considered answer. An unknown
        // code from a newer build joins them: a client that cannot name a
        // condition must not decide it is temporary.
        DaemonRpcErrorCode::InvalidRequest
        | DaemonRpcErrorCode::UnsupportedVersion
        | DaemonRpcErrorCode::MethodNotFound
        | DaemonRpcErrorCode::MalformedFrame
        | DaemonRpcErrorCode::FrameTooLarge
        | DaemonRpcErrorCode::Conflict
        | DaemonRpcErrorCode::NotFound
        | DaemonRpcErrorCode::PermissionDenied
        | DaemonRpcErrorCode::Internal
        | DaemonRpcErrorCode::Unknown(_) => RetryDisposition::Terminal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bowline_core::wire::generated::DaemonRpcError;

    fn remote(code: DaemonRpcErrorCode, retryable: bool) -> ClientError {
        ClientError::Remote(Box::new(DaemonRpcError {
            code,
            message: "engine is not attached".to_string(),
            retryable,
            retry_after_ms: None,
            operation_id: None,
            required_client_version: None,
            details: None,
        }))
    }

    /// The reproduction: a daemon that has bound its socket but not yet attached
    /// its sync engine answers `unavailable`, and a wait must sit through it.
    #[test]
    fn an_unavailable_subsystem_is_worth_waiting_for() {
        assert_eq!(
            remote(DaemonRpcErrorCode::Unavailable, true).retry_disposition(),
            RetryDisposition::RetryWhileStarting
        );
    }

    #[test]
    fn a_refused_request_is_terminal_however_long_the_caller_waits() {
        for code in [
            DaemonRpcErrorCode::InvalidRequest,
            DaemonRpcErrorCode::NotFound,
            DaemonRpcErrorCode::PermissionDenied,
            DaemonRpcErrorCode::MethodNotFound,
            DaemonRpcErrorCode::Internal,
        ] {
            assert_eq!(
                remote(code, false).retry_disposition(),
                RetryDisposition::Terminal
            );
        }
    }

    /// The daemon's own `retryable` hint must not promote a refusal: the code is
    /// the contract, the hint is advisory.
    #[test]
    fn a_terminal_code_stays_terminal_when_the_daemon_flags_it_retryable() {
        assert_eq!(
            remote(DaemonRpcErrorCode::InvalidRequest, true).retry_disposition(),
            RetryDisposition::Terminal
        );
    }

    #[test]
    fn an_unrecognised_code_is_never_guessed_to_be_temporary() {
        assert_eq!(
            remote(
                DaemonRpcErrorCode::Unknown("future_state".to_string()),
                true
            )
            .retry_disposition(),
            RetryDisposition::Terminal
        );
    }

    #[test]
    fn a_deadline_is_distinguished_from_a_starting_daemon() {
        assert_eq!(
            remote(DaemonRpcErrorCode::DeadlineExceeded, true).retry_disposition(),
            RetryDisposition::DeadlineElapsed
        );
        assert_eq!(
            ClientError::Timeout {
                request_id: "request-1".to_string(),
                timeout: std::time::Duration::from_secs(1),
            }
            .retry_disposition(),
            RetryDisposition::DeadlineElapsed
        );
    }

    #[test]
    fn an_unbound_socket_is_a_daemon_still_starting() {
        for kind in [
            io::ErrorKind::NotFound,
            io::ErrorKind::ConnectionRefused,
            io::ErrorKind::ConnectionReset,
        ] {
            assert_eq!(
                ClientError::Io {
                    operation: "connect",
                    source: io::Error::from(kind),
                }
                .retry_disposition(),
                RetryDisposition::RetryWhileStarting
            );
        }
    }

    #[test]
    fn a_socket_this_process_may_not_open_is_terminal() {
        assert_eq!(
            ClientError::Io {
                operation: "connect",
                source: io::Error::from(io::ErrorKind::PermissionDenied),
            }
            .retry_disposition(),
            RetryDisposition::Terminal
        );
    }

    #[test]
    fn a_build_that_cannot_talk_to_the_daemon_never_waits() {
        assert_eq!(
            ClientError::IncompatibleVersion {
                dimension: crate::negotiation::VersionDimension::MachineContract,
                peer_version: 6,
                window: crate::negotiation::VersionWindow {
                    minimum: 8,
                    supported: 8,
                },
            }
            .retry_disposition(),
            RetryDisposition::Terminal
        );
    }
}
