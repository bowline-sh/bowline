use std::{error::Error, fmt};

use bowline_core::wire::generated::{
    DaemonClientHello, DaemonServerHello, MACHINE_CONTRACT_VERSION, WIRE_SCHEMA_HASH,
};

use crate::{DAEMON_RPC_PROTOCOL, DAEMON_RPC_PROTOCOL_VERSION};

/// A peer's compatibility range for one version dimension: the newest generation
/// this build speaks and the oldest it still serves.
///
/// The hello frame carries only each peer's `supported` value, so negotiation
/// selects the lower of the two and each side independently checks that the
/// selection is not older than its own `minimum`. That is what lets a fleet of
/// devices and agent hosts be upgraded one machine at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VersionWindow {
    pub minimum: u16,
    pub supported: u16,
}

impl VersionWindow {
    /// The generation both peers can speak, or `None` when the peer is outside
    /// this window. A peer newer than `supported` is fine — it downgrades to the
    /// selection — as long as this build's own `minimum` is met.
    #[must_use]
    pub fn select(self, peer_supported: u16) -> Option<u16> {
        let selected = self.supported.min(peer_supported);
        (selected >= self.minimum).then_some(selected)
    }
}

impl fmt::Display for VersionWindow {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}..={}", self.minimum, self.supported)
    }
}

/// The framing and dispatch generation. `minimum` equals `supported` because a
/// protocol-version bump changes the frame layout itself; there is nothing to
/// downgrade to.
pub const DAEMON_RPC_PROTOCOL_WINDOW: VersionWindow = VersionWindow {
    minimum: DAEMON_RPC_PROTOCOL_VERSION,
    supported: DAEMON_RPC_PROTOCOL_VERSION,
};

/// The machine-contract generation. Bumping `MACHINE_CONTRACT_VERSION` opens a
/// compatibility window rather than bricking every resident daemon; raising
/// `minimum` is the separate, deliberate act of closing one.
pub const MACHINE_CONTRACT_WINDOW: VersionWindow = VersionWindow {
    minimum: 8,
    supported: MACHINE_CONTRACT_VERSION,
};

/// Which version dimension a peer failed to meet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionDimension {
    Protocol,
    MachineContract,
}

impl VersionDimension {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Protocol => "protocol",
            Self::MachineContract => "machine-contract",
        }
    }

    #[must_use]
    pub const fn window(self) -> VersionWindow {
        match self {
            Self::Protocol => DAEMON_RPC_PROTOCOL_WINDOW,
            Self::MachineContract => MACHINE_CONTRACT_WINDOW,
        }
    }
}

impl fmt::Display for VersionDimension {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerNegotiation {
    pub daemon_version: String,
    pub capabilities: Vec<String>,
    pub instance_id: String,
}

/// The versions and capabilities both peers agreed on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NegotiatedSession {
    pub hello: DaemonServerHello,
    pub protocol_version: u16,
    pub contract_version: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NegotiationError {
    InvalidProtocol {
        received: String,
    },
    IncompatibleVersion {
        dimension: VersionDimension,
        received: u16,
        window: VersionWindow,
    },
}

impl NegotiationError {
    #[must_use]
    pub const fn dimension(&self) -> Option<VersionDimension> {
        match self {
            Self::InvalidProtocol { .. } => None,
            Self::IncompatibleVersion { dimension, .. } => Some(*dimension),
        }
    }
}

impl fmt::Display for NegotiationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidProtocol { received } => {
                write!(
                    formatter,
                    "unsupported daemon RPC protocol marker `{received}`"
                )
            }
            Self::IncompatibleVersion {
                dimension,
                received,
                window,
            } => write!(
                formatter,
                "peer {dimension} version {received} is outside the supported range {window}"
            ),
        }
    }
}

impl Error for NegotiationError {}

/// Accept a client hello if its versions fall inside this build's compatibility
/// windows.
///
/// `schema_hash` is deliberately not a gate. It is the digest of the entire
/// contract corpus, including hosted-only documents that the daemon RPC surface
/// never reads, so requiring equality made an unrelated dashboard field edit
/// brick every resident daemon. The hash is still exchanged so both peers can
/// report which contract revision they were built from.
pub fn negotiate(
    client: &DaemonClientHello,
    server: &ServerNegotiation,
) -> Result<NegotiatedSession, NegotiationError> {
    if client.protocol != DAEMON_RPC_PROTOCOL {
        return Err(NegotiationError::InvalidProtocol {
            received: client.protocol.clone(),
        });
    }
    let protocol_version = select_version(VersionDimension::Protocol, client.protocol_version)?;
    let contract_version =
        select_version(VersionDimension::MachineContract, client.contract_version)?;
    Ok(NegotiatedSession {
        hello: DaemonServerHello {
            protocol_version,
            contract_version,
            schema_hash: WIRE_SCHEMA_HASH.to_string(),
            daemon_version: server.daemon_version.clone(),
            capabilities: server.capabilities.clone(),
            instance_id: server.instance_id.clone(),
        },
        protocol_version,
        contract_version,
    })
}

fn select_version(dimension: VersionDimension, received: u16) -> Result<u16, NegotiationError> {
    dimension
        .window()
        .select(received)
        .ok_or(NegotiationError::IncompatibleVersion {
            dimension,
            received,
            window: dimension.window(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> DaemonClientHello {
        DaemonClientHello {
            protocol: DAEMON_RPC_PROTOCOL.to_string(),
            protocol_version: DAEMON_RPC_PROTOCOL_VERSION,
            contract_version: MACHINE_CONTRACT_VERSION,
            schema_hash: WIRE_SCHEMA_HASH.to_string(),
            client_kind: "test".to_string(),
            client_version: "1".to_string(),
            capabilities: Vec::new(),
        }
    }

    fn server() -> ServerNegotiation {
        ServerNegotiation {
            daemon_version: "1".to_string(),
            capabilities: vec!["status.getSnapshot".to_string()],
            instance_id: "daemon-1".to_string(),
        }
    }

    #[test]
    fn accepts_the_current_contract() {
        let session = negotiate(&client(), &server()).expect("versions match");
        assert_eq!(session.protocol_version, DAEMON_RPC_PROTOCOL_VERSION);
        assert_eq!(session.contract_version, MACHINE_CONTRACT_VERSION);
        assert_eq!(session.hello.schema_hash, WIRE_SCHEMA_HASH);
    }

    #[test]
    fn a_different_schema_hash_is_not_fatal() {
        let mut client = client();
        client.schema_hash = "a-hosted-only-document-changed".to_string();
        let session = negotiate(&client, &server()).expect("hosted edits do not break local RPC");
        assert_eq!(session.contract_version, MACHINE_CONTRACT_VERSION);
    }

    #[test]
    fn a_newer_client_contract_downgrades_to_this_build() {
        let mut client = client();
        client.contract_version = MACHINE_CONTRACT_VERSION + 3;
        let session = negotiate(&client, &server()).expect("newer clients downgrade");
        assert_eq!(session.contract_version, MACHINE_CONTRACT_VERSION);
    }

    #[test]
    fn a_client_below_the_window_is_rejected_with_its_dimension() {
        let mut client = client();
        client.contract_version = MACHINE_CONTRACT_WINDOW.minimum - 1;
        let error = negotiate(&client, &server()).expect_err("stale clients are rejected");
        assert_eq!(error.dimension(), Some(VersionDimension::MachineContract));
    }

    #[test]
    fn a_client_below_the_protocol_window_names_the_protocol_dimension() {
        let mut client = client();
        client.protocol_version = DAEMON_RPC_PROTOCOL_VERSION - 1;
        let error = negotiate(&client, &server()).expect_err("older frame layouts are rejected");
        assert_eq!(error.dimension(), Some(VersionDimension::Protocol));
    }

    #[test]
    fn an_unknown_protocol_marker_is_rejected() {
        let mut client = client();
        client.protocol = "bowline-daemon-v9".to_string();
        let error = negotiate(&client, &server()).expect_err("frame markers must match exactly");
        assert_eq!(error.dimension(), None);
    }

    #[test]
    fn selection_takes_the_lower_supported_version() {
        let window = VersionWindow {
            minimum: 4,
            supported: 8,
        };
        assert_eq!(window.select(6), Some(6));
        assert_eq!(window.select(12), Some(8));
        assert_eq!(window.select(4), Some(4));
        assert_eq!(window.select(3), None);
    }
}
