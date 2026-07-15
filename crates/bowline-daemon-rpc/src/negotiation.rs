use std::{error::Error, fmt};

use bowline_core::wire::generated::{
    DaemonClientHello, DaemonServerHello, MACHINE_CONTRACT_VERSION, WIRE_SCHEMA_HASH,
};

use crate::{DAEMON_RPC_PROTOCOL, DAEMON_RPC_PROTOCOL_VERSION};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerNegotiation {
    pub daemon_version: String,
    pub capabilities: Vec<String>,
    pub instance_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NegotiationError {
    InvalidProtocol {
        received: String,
    },
    ProtocolVersionMismatch {
        received: u16,
        supported: u16,
    },
    ContractVersionMismatch {
        received: u16,
        supported: u16,
    },
    SchemaHashMismatch {
        received: String,
        supported: &'static str,
    },
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
            Self::ProtocolVersionMismatch {
                received,
                supported,
            } => write!(
                formatter,
                "daemon RPC protocol version {received} does not equal required version {supported}"
            ),
            Self::ContractVersionMismatch {
                received,
                supported,
            } => write!(
                formatter,
                "machine contract version {received} does not equal required version {supported}"
            ),
            Self::SchemaHashMismatch {
                received,
                supported,
            } => write!(
                formatter,
                "wire schema hash {received} does not equal required hash {supported}"
            ),
        }
    }
}

impl Error for NegotiationError {}

pub fn negotiate(
    client: &DaemonClientHello,
    server: &ServerNegotiation,
) -> Result<DaemonServerHello, NegotiationError> {
    if client.protocol != DAEMON_RPC_PROTOCOL {
        return Err(NegotiationError::InvalidProtocol {
            received: client.protocol.clone(),
        });
    }
    if client.protocol_version != DAEMON_RPC_PROTOCOL_VERSION {
        return Err(NegotiationError::ProtocolVersionMismatch {
            received: client.protocol_version,
            supported: DAEMON_RPC_PROTOCOL_VERSION,
        });
    }
    if client.contract_version != MACHINE_CONTRACT_VERSION {
        return Err(NegotiationError::ContractVersionMismatch {
            received: client.contract_version,
            supported: MACHINE_CONTRACT_VERSION,
        });
    }
    if client.schema_hash != WIRE_SCHEMA_HASH {
        return Err(NegotiationError::SchemaHashMismatch {
            received: client.schema_hash.clone(),
            supported: WIRE_SCHEMA_HASH,
        });
    }
    Ok(DaemonServerHello {
        protocol_version: DAEMON_RPC_PROTOCOL_VERSION,
        contract_version: MACHINE_CONTRACT_VERSION,
        schema_hash: WIRE_SCHEMA_HASH.to_string(),
        daemon_version: server.daemon_version.clone(),
        capabilities: server.capabilities.clone(),
        instance_id: server.instance_id.clone(),
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
            capabilities: vec!["status.snapshot".to_string()],
            instance_id: "daemon-1".to_string(),
        }
    }

    #[test]
    fn accepts_the_exact_current_contract() {
        let selected = negotiate(&client(), &server()).expect("versions match");
        assert_eq!(selected.protocol_version, DAEMON_RPC_PROTOCOL_VERSION);
        assert_eq!(selected.contract_version, MACHINE_CONTRACT_VERSION);
        assert_eq!(selected.schema_hash, WIRE_SCHEMA_HASH);
    }

    #[test]
    fn protocol_version_mismatch_is_explicit() {
        let mut client = client();
        client.protocol_version += 1;
        assert!(matches!(
            negotiate(&client, &server()),
            Err(NegotiationError::ProtocolVersionMismatch { .. })
        ));
    }

    #[test]
    fn contract_version_mismatch_is_explicit() {
        let mut client = client();
        client.contract_version -= 1;
        assert!(matches!(
            negotiate(&client, &server()),
            Err(NegotiationError::ContractVersionMismatch { .. })
        ));
    }

    #[test]
    fn schema_hash_mismatch_is_explicit() {
        let mut client = client();
        client.schema_hash = "different-schema".to_string();
        assert!(matches!(
            negotiate(&client, &server()),
            Err(NegotiationError::SchemaHashMismatch { .. })
        ));
    }
}
