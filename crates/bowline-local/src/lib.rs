#![deny(unsafe_code)]

use bowline_core::status::WorkspaceStatus;

pub mod account;
pub mod agents;
pub mod bootstrap;
pub mod device_keys;
pub mod env;
pub mod events;
pub mod explain;
pub mod fakes;
pub mod hydration_budget;
pub mod indexed;
pub mod init;
pub mod linux_service;
pub mod macos_service;
pub mod metadata;
pub mod notifications;
pub mod policy;
pub mod scanner;
pub mod search;
pub mod setup;
pub mod status;
pub mod symbols;
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
