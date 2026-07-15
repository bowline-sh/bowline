#![deny(unsafe_code)]

mod client;
mod device_proofs;
mod errors;
mod fake;
mod gc;
mod primitives;
mod types;
mod validation;

#[cfg(feature = "hosted-convex")]
pub mod hosted;
#[cfg(feature = "hosted-convex")]
pub(crate) mod hosted_endpoints;
#[cfg(feature = "hosted-convex")]
pub mod transfer;

pub use client::*;
pub(crate) use device_proofs::verify_device_authorization_proof;
pub use device_proofs::{
    device_authorization_message, device_request_proof_subject, device_revocation_proof_subject,
    recovery_envelope_payload_proof_subject, recovery_envelope_payload_proof_subject_parts,
    recovery_envelope_proof_subject,
};
pub use errors::{
    Capability, CompareAndSwapError, ControlPlaneError, RejectionCode, WorkViewUpdateError,
};
pub use fake::FakeControlPlaneClient;
pub use gc::{ControlPlaneGcSweepReport, sweep_storage_gc};
pub use primitives::{ControlPlaneTimestamp, DeterministicClock, DeterministicIdGenerator};
pub use types::*;
pub use validation::is_opaque_object_key;
pub(crate) use validation::validate_object_key;

#[cfg(feature = "hosted-convex")]
pub use hosted::{
    HostedControlPlaneClient, HostedFunctionCallCount, WorkspaceRefStreamCancellation,
    WorkspaceRefStreamShutdown, hosted_function_call_counts, workspace_ref_stream_shutdown_pair,
};
#[cfg(feature = "hosted-convex")]
pub use transfer::{SignedUrlByteStore, SignedUrlHttpClient};

#[cfg(test)]
mod tests;
