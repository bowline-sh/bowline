pub mod generated;
pub mod generated_daemon_rpc;
pub mod generated_status;
pub mod generated_status_fact_authorities;
pub mod generated_status_output;
pub mod generated_status_output_support;
pub mod generated_status_transport;
pub mod generated_watch;
mod status_transport;

pub use generated::{
    DeviceApprovalAffordance, EventName, KNOWN_EVENT_NAMES, MACHINE_CONTRACT_VERSION,
    WIRE_SCHEMA_HASH,
};
pub use status_transport::{
    StatusTransportError, status_command_from_wire, status_command_to_wire, watch_frame_from_wire,
    watch_frame_to_wire,
};
