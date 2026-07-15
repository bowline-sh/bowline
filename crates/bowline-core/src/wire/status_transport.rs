use serde::{Serialize, de::DeserializeOwned};

use crate::commands::{StatusCommandOutput, WatchFrame};

use super::generated::{WireStatusCommandOutput, WireWatchFrame};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusTransportError {
    SerializeDomain,
    DeserializeWire,
    SerializeWire,
    DeserializeDomain,
}

impl std::fmt::Display for StatusTransportError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::SerializeDomain => "status domain value could not be serialized",
            Self::DeserializeWire => "status wire value did not match the generated contract",
            Self::SerializeWire => "generated status wire value could not be serialized",
            Self::DeserializeDomain => "status wire value did not match the domain contract",
        })
    }
}

impl std::error::Error for StatusTransportError {}

fn domain_to_wire<TDomain: Serialize, TWire: DeserializeOwned>(
    domain: &TDomain,
) -> Result<TWire, StatusTransportError> {
    let value = serde_json::to_value(domain).map_err(|_| StatusTransportError::SerializeDomain)?;
    serde_json::from_value(value).map_err(|_| StatusTransportError::DeserializeWire)
}

fn wire_to_domain<TWire: Serialize, TDomain: DeserializeOwned>(
    wire: TWire,
) -> Result<TDomain, StatusTransportError> {
    let value = serde_json::to_value(wire).map_err(|_| StatusTransportError::SerializeWire)?;
    serde_json::from_value(value).map_err(|_| StatusTransportError::DeserializeDomain)
}

pub fn status_command_to_wire(
    status: &StatusCommandOutput,
) -> Result<WireStatusCommandOutput, StatusTransportError> {
    domain_to_wire(status)
}

pub fn status_command_from_wire(
    status: WireStatusCommandOutput,
) -> Result<StatusCommandOutput, StatusTransportError> {
    wire_to_domain(status)
}

pub fn watch_frame_to_wire(frame: &WatchFrame) -> Result<WireWatchFrame, StatusTransportError> {
    domain_to_wire(frame)
}

pub fn watch_frame_from_wire(frame: WireWatchFrame) -> Result<WatchFrame, StatusTransportError> {
    wire_to_domain(frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(path: &str) -> serde_json::Value {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let source = std::fs::read_to_string(root.join(path)).expect("shared fixture is readable");
        serde_json::from_str(&source).expect("shared fixture is valid JSON")
    }

    #[test]
    fn status_fixture_has_byte_equivalent_domain_and_generated_shapes() {
        let value = fixture("tests/contracts/status/healthy.json");
        let domain: StatusCommandOutput =
            serde_json::from_value(value.clone()).expect("domain status fixture");
        let wire = status_command_to_wire(&domain).expect("generated status conversion");
        assert_eq!(serde_json::to_value(&wire).expect("wire JSON"), value);
        assert_eq!(
            status_command_from_wire(wire).expect("domain conversion"),
            domain
        );
    }

    #[test]
    fn watch_stream_has_equivalent_domain_and_generated_frames() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let source =
            std::fs::read_to_string(root.join("tests/contracts/streams/status-watch.ndjson"))
                .expect("watch fixture is readable");
        for line in source.lines().filter(|line| !line.trim().is_empty()) {
            let value: serde_json::Value = serde_json::from_str(line).expect("watch fixture JSON");
            let domain: WatchFrame =
                serde_json::from_value(value.clone()).expect("domain watch frame");
            let wire = watch_frame_to_wire(&domain).expect("generated watch conversion");
            assert_eq!(serde_json::to_value(&wire).expect("wire JSON"), value);
            assert_eq!(
                watch_frame_from_wire(wire).expect("domain conversion"),
                domain
            );
        }
    }
}
