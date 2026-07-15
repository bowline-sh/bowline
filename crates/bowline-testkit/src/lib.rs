#![deny(unsafe_code)]

pub mod cost;
pub mod hosted_byte_store;
pub mod invariants;
pub mod scenario;
pub mod snapshot_fixture;

pub use cost::{CostBudget, CostBudgetError, CostReport};
pub use hosted_byte_store::FakeHostedByteStore;
pub use invariants::{
    DegradedEvidence, InvariantError, RenderedStatus, assert_local_head_supported,
    assert_object_before_ref, assert_status_not_hiding_degraded,
};
pub use scenario::{ScenarioError, SyncScenario, TwoDeviceSyncScenario};
pub use snapshot_fixture::persist_project_snapshot_fixture;
