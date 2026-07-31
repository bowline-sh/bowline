use std::{error::Error, fmt};

use bowline_core::ids::{DeviceApprovalRequestId, DeviceId, WorkspaceId};

use crate::StaleWorkspaceRef;

/// Enough digest to compare two builds by eye without printing 64 hex
/// characters into every skew message.
const CONTRACT_DIGEST_PREVIEW: usize = 12;

/// How a caller should react to a control-plane failure. This is the single
/// classification point: retry loops must consume it rather than re-deriving
/// intent by matching the error variants themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retryability {
    /// Transient. The same request may succeed on a later attempt.
    Retryable,
    /// Terminal. Retrying cannot change the outcome.
    Fatal,
    /// Terminal until the caller obtains fresh credentials.
    AuthExpired,
    /// Terminal until the caller learns the device trust the payload was signed
    /// under. Retrying the same call with the same local trust repeats the same
    /// answer; refreshing device trust first can change it.
    TrustRefreshRequired,
}

/// Which half of a hosted endpoint call failed its declared wire contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireContractFailure {
    Request,
    Response,
}

impl WireContractFailure {
    pub const fn message(self) -> &'static str {
        match self {
            Self::Request => "request did not match the declared contract",
            Self::Response => "response did not match the declared contract",
        }
    }
}

impl fmt::Display for WireContractFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompareAndSwapError {
    WorkspaceMissing {
        workspace_id: WorkspaceId,
    },
    StaleRef(StaleWorkspaceRef),
    /// The swap definitely did not apply: it failed before the mutation was
    /// issued, or the server answered with a decisive rejection.
    Rejected(Box<ControlPlaneError>),
    /// The swap may or may not have applied. The caller must re-read the ref
    /// before deciding anything.
    Ambiguous(Box<ControlPlaneError>),
    Unsupported {
        capability: &'static str,
        reason: &'static str,
    },
}

impl CompareAndSwapError {
    /// A swap that failed before the mutation left the client.
    pub fn rejected(error: ControlPlaneError) -> Self {
        Self::Rejected(Box::new(error))
    }

    /// A swap whose outcome the client cannot determine.
    pub fn ambiguous(error: ControlPlaneError) -> Self {
        Self::Ambiguous(Box::new(error))
    }

    pub fn retryability(&self) -> Retryability {
        match self {
            Self::WorkspaceMissing { .. } | Self::Unsupported { .. } => Retryability::Fatal,
            // A stale ref is the expected concurrent-writer outcome: the caller
            // rebases onto the returned current ref and swaps again.
            Self::StaleRef(_) => Retryability::Retryable,
            Self::Rejected(error) | Self::Ambiguous(error) => error.retryability(),
        }
    }
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
            Self::Rejected(error) => {
                write!(formatter, "compare-and-swap was rejected: {error}")
            }
            Self::Ambiguous(error) => write!(
                formatter,
                "compare-and-swap outcome is unknown, re-read the ref: {error}"
            ),
            Self::Unsupported { capability, reason } => {
                write!(formatter, "{capability} is unsupported: {reason}")
            }
        }
    }
}

impl Error for CompareAndSwapError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Rejected(error) | Self::Ambiguous(error) => Some(error.as_ref()),
            Self::WorkspaceMissing { .. } | Self::StaleRef(_) | Self::Unsupported { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectionCode {
    AccountSessionExpired,
    AccountSessionMissing,
    AccountSessionRevoked,
    Conflict,
    DeviceNotTrusted,
    Expired,
    InvalidRequest,
    Unauthorized,
    WorkspaceMembershipRequired,
    WorkspaceOwnerRequired,
    Unknown,
}

impl RejectionCode {
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::AccountSessionExpired => "account_session_expired",
            Self::AccountSessionMissing => "account_session_missing",
            Self::AccountSessionRevoked => "account_session_revoked",
            Self::Conflict => "control_plane/conflict",
            Self::DeviceNotTrusted => "control_plane/device_not_trusted",
            Self::Expired => "control_plane/expired",
            Self::InvalidRequest => "control_plane/invalid_request",
            Self::Unauthorized => "control_plane/unauthorized",
            Self::WorkspaceMembershipRequired => "control_plane/workspace_membership_required",
            Self::WorkspaceOwnerRequired => "control_plane/workspace_owner_required",
            Self::Unknown => "control_plane/unknown",
        }
    }

    pub fn from_wire(code: &str) -> Self {
        match code {
            "account_session_expired" => Self::AccountSessionExpired,
            "account_session_missing" => Self::AccountSessionMissing,
            "account_session_revoked" => Self::AccountSessionRevoked,
            "control_plane/conflict" => Self::Conflict,
            "control_plane/device_not_trusted" => Self::DeviceNotTrusted,
            "control_plane/expired" => Self::Expired,
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
    Unsupported {
        capability: &'static str,
        reason: &'static str,
    },
    Conflict {
        resource: &'static str,
        reason: &'static str,
    },
    /// A payload did not match the generated wire contract for its endpoint.
    /// `field_path` is a structural locator into the payload (`objects[3].key`),
    /// never the offending value, so it is safe to surface.
    ContractViolation {
        endpoint: &'static str,
        function: &'static str,
        failure: WireContractFailure,
        field_path: Option<String>,
    },
    /// A signed payload named a signing device this client holds no
    /// authorization-proof verifier for, so its signature was never checked.
    ///
    /// Deliberately not a `ResponseShape`: nothing about the payload is
    /// malformed, and the answer can change without the peer sending anything
    /// different. It says only that local device trust is behind the workspace's
    /// — the state a second device enrolling into a running workspace creates.
    UnknownSigningDevice {
        workspace_id: WorkspaceId,
        device_id: DeviceId,
    },
    /// A payload satisfied the wire contract but not what the domain requires
    /// (an unmodeled enum value, an unparsable timestamp, a missing proof part).
    ResponseShape {
        reason: &'static str,
        field: Option<&'static str>,
    },
    /// The server function raised an uncaught exception.
    ServerError {
        function: &'static str,
        message: String,
    },
    /// A typed hosted call this client had already validated against its own
    /// generated contract was refused without a typed error. Hosted handlers
    /// throw structured `ConvexError`s, so an unclassified refusal is the
    /// deployment's argument validator disagreeing with the contract this client
    /// was generated from — the one failure a redacted "Server Error" hides.
    ContractSkew {
        endpoint: &'static str,
        function: &'static str,
        client_wire_schema_digest: &'static str,
        detail: String,
    },
    /// A client-local invariant or configuration failed. Never caused by the
    /// peer, and never fixed by retrying.
    Internal {
        reason: &'static str,
    },
}

impl ControlPlaneError {
    pub fn retryability(&self) -> Retryability {
        match self {
            // A server exception may be a transient dependency failure; the
            // caller's backoff decides how many times that is worth believing.
            Self::Timeout { .. } | Self::Transport { .. } | Self::ServerError { .. } => {
                Retryability::Retryable
            }
            Self::Rejected {
                code:
                    RejectionCode::AccountSessionExpired
                    | RejectionCode::AccountSessionMissing
                    | RejectionCode::AccountSessionRevoked
                    | RejectionCode::Unauthorized,
                ..
            } => Retryability::AuthExpired,
            Self::UnknownSigningDevice { .. } => Retryability::TrustRefreshRequired,
            Self::Rejected { .. }
            | Self::WorkspaceMissing { .. }
            | Self::InvalidObjectKey { .. }
            | Self::ObjectMissing { .. }
            | Self::DeviceRequestMissing { .. }
            | Self::Unsupported { .. }
            | Self::Conflict { .. }
            | Self::ContractViolation { .. }
            | Self::ContractSkew { .. }
            | Self::ResponseShape { .. }
            | Self::Internal { .. } => Retryability::Fatal,
            Self::CompareAndSwap(error) => error.retryability(),
        }
    }

    /// The signer a `TrustRefreshRequired` failure is about, so a caller can
    /// refresh trust for that one device instead of matching error variants to
    /// rediscover why the payload could not be verified.
    pub fn unknown_signing_device(&self) -> Option<(&WorkspaceId, &DeviceId)> {
        match self {
            Self::UnknownSigningDevice {
                workspace_id,
                device_id,
            } => Some((workspace_id, device_id)),
            // A CAS carries the ref it could not verify inside its own outcome,
            // so the signer has to survive that wrapping to reach the caller.
            Self::CompareAndSwap(
                CompareAndSwapError::Rejected(error) | CompareAndSwapError::Ambiguous(error),
            ) => error.unknown_signing_device(),
            Self::CompareAndSwap(
                CompareAndSwapError::WorkspaceMissing { .. }
                | CompareAndSwapError::StaleRef(_)
                | CompareAndSwapError::Unsupported { .. },
            )
            | Self::Timeout { .. }
            | Self::Transport { .. }
            | Self::Rejected { .. }
            | Self::WorkspaceMissing { .. }
            | Self::InvalidObjectKey { .. }
            | Self::ObjectMissing { .. }
            | Self::DeviceRequestMissing { .. }
            | Self::Unsupported { .. }
            | Self::Conflict { .. }
            | Self::ContractViolation { .. }
            | Self::ResponseShape { .. }
            | Self::ServerError { .. }
            | Self::ContractSkew { .. }
            | Self::Internal { .. } => None,
        }
    }
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
            Self::Unsupported { capability, reason } => {
                write!(formatter, "{capability} is unsupported: {reason}")
            }
            Self::Conflict { resource, reason } => {
                write!(
                    formatter,
                    "{resource} conflicts with existing metadata: {reason}"
                )
            }
            Self::ContractViolation {
                endpoint,
                function,
                failure,
                field_path,
            } => {
                write!(
                    formatter,
                    "hosted endpoint `{endpoint}` (`{function}`): {failure}"
                )?;
                match field_path {
                    Some(path) => write!(formatter, " (field `{path}`)"),
                    None => Ok(()),
                }
            }
            Self::UnknownSigningDevice {
                workspace_id,
                device_id,
            } => write!(
                formatter,
                "workspace `{}` was signed by device `{}`, which this host holds no device \
                 authorization proof verifier for",
                workspace_id.as_str(),
                device_id.as_str()
            ),
            Self::ResponseShape { reason, field } => match field {
                Some(field) => write!(formatter, "field `{field}`: {reason}"),
                None => formatter.write_str(reason),
            },
            Self::ServerError { function, message } => {
                write!(formatter, "`{function}` failed on the server: {message}")
            }
            Self::ContractSkew {
                endpoint,
                function,
                client_wire_schema_digest,
                detail,
            } => write!(
                formatter,
                "hosted endpoint `{endpoint}` (`{function}`) was refused without a typed error \
                 ({detail}); this client speaks wire contract {}. A control plane generated from \
                 a different contract refuses calls exactly this way — run `bowline update` to \
                 install a client that matches it.",
                &client_wire_schema_digest
                    [..CONTRACT_DIGEST_PREVIEW.min(client_wire_schema_digest.len())]
            ),
            Self::Internal { reason } => {
                write!(formatter, "control-plane client failed locally: {reason}")
            }
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
            | Self::InvalidObjectKey { .. }
            | Self::ObjectMissing { .. }
            | Self::DeviceRequestMissing { .. }
            | Self::Unsupported { .. }
            | Self::Conflict { .. }
            | Self::ContractViolation { .. }
            | Self::UnknownSigningDevice { .. }
            | Self::ResponseShape { .. }
            | Self::ServerError { .. }
            | Self::ContractSkew { .. }
            | Self::Internal { .. } => None,
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
#[cfg(test)]
mod rejection_code_tests {
    use super::RejectionCode;

    #[test]
    fn workspace_access_codes_round_trip_the_canonical_wire_values() {
        for code in [
            RejectionCode::AccountSessionExpired,
            RejectionCode::AccountSessionMissing,
            RejectionCode::AccountSessionRevoked,
            RejectionCode::Expired,
            RejectionCode::WorkspaceMembershipRequired,
            RejectionCode::WorkspaceOwnerRequired,
        ] {
            assert_eq!(RejectionCode::from_wire(code.as_wire()), code);
        }
    }
}
