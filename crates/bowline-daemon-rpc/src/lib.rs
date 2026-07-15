//! Shared, bounded transport for Bowline's local daemon RPC protocol.
//!
//! This crate owns transport mechanics and client correlation only. Daemon
//! handlers, status composition, durable operations, and socket discovery stay
//! with their domain owners.

mod client;
mod codec;
mod negotiation;

pub use client::{ClientError, ClientOptions, DaemonClient, EventReceiver};
pub use codec::{
    CONNECTION_MAGIC, CodecError, CodecPhase, DEFAULT_MAX_FRAME_BYTES, FrameCodec,
    IncrementalFrameDecoder,
};
pub use negotiation::{NegotiationError, ServerNegotiation, negotiate};

pub const DAEMON_RPC_PROTOCOL: &str = "bowline-daemon-v2";
pub const DAEMON_RPC_PROTOCOL_VERSION: u16 = 2;
