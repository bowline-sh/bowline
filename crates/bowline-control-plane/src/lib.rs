#![deny(unsafe_code)]

mod client;
mod errors;
mod fake;
mod primitives;
mod types;
mod validation;

#[cfg(feature = "hosted-convex")]
pub mod hosted;
#[cfg(feature = "hosted-convex")]
pub mod transfer;

pub use client::*;
pub use errors::*;
pub use fake::FakeControlPlaneClient;
pub use primitives::{ControlPlaneTimestamp, DeterministicClock, DeterministicIdGenerator};
pub use types::*;
pub use validation::is_opaque_object_key;
pub(crate) use validation::validate_object_key;

#[cfg(feature = "hosted-convex")]
pub use hosted::{HostedControlPlaneClient, HostedFunctionCallCount, hosted_function_call_counts};
#[cfg(feature = "hosted-convex")]
pub use transfer::SignedUrlByteStore;

#[cfg(test)]
mod tests;
