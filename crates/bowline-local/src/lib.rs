#![deny(unsafe_code)]

use bowline_core::status::WorkspaceStatus;

pub mod account;
pub mod agents;
pub mod bootstrap;
pub mod device_keys;
pub mod env;
pub mod events;
pub mod fakes;
pub(crate) mod fs_access;
pub(crate) mod glob;
pub mod history;
pub mod init;
pub mod lifecycle;
pub mod linux_service;
pub mod macos_service;
pub mod metadata;
pub mod notifications;
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod page_test_support;
pub mod policy;
pub mod scanner;
pub(crate) mod service_runtime;
pub mod setup;
pub mod status;
pub mod sync;
pub mod trust;
pub mod work_views;
pub mod workspace;

pub fn initial_workspace_status() -> WorkspaceStatus {
    WorkspaceStatus::healthy()
}

#[cfg(test)]
mod tests {
    use super::initial_workspace_status;

    #[test]
    fn initial_status_is_healthy() {
        assert!(!initial_workspace_status().needs_attention());
    }
}
