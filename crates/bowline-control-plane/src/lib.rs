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
    FETCH_DEVICE_GRANT_ACTION, device_authorization_message, device_public_key_proof_subject,
    device_request_proof_subject, device_revocation_proof_subject,
    key_regrant_accept_proof_subject, key_regrant_offer_digest, key_regrant_offer_proof_subject,
    key_regrant_work_proof_subject, recovery_envelope_payload_proof_subject,
    recovery_envelope_payload_proof_subject_parts, recovery_envelope_proof_subject,
};
pub use errors::{
    CompareAndSwapError, ControlPlaneError, RejectionCode, Retryability, WireContractFailure,
};
pub use fake::FakeControlPlaneClient;
pub use gc::{
    ControlPlaneGcMetadataFailure, ControlPlaneGcSweep, ControlPlaneGcSweepReport,
    StorageGcSweepVerdict, sweep_storage_gc, sweep_storage_gc_until_converged,
};
pub use primitives::{ControlPlaneTimestamp, DeterministicClock, DeterministicIdGenerator};
pub use types::*;
pub use validation::is_opaque_object_key;
pub(crate) use validation::validate_object_key;

#[cfg(feature = "hosted-convex")]
pub use hosted::{
    HostedControlPlaneClient, MISSING_ACCOUNT_SESSION_MESSAGE, WorkspaceRefStreamCancellation,
    WorkspaceRefStreamConnectionState, WorkspaceRefStreamEvent, WorkspaceRefStreamShutdown,
    workspace_ref_stream_shutdown_pair,
};
#[cfg(feature = "hosted-convex")]
pub use transfer::{PutOutcome, SignedUrlByteStore, SignedUrlHttpClient};

#[cfg(test)]
mod tests;
