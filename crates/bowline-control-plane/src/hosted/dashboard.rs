use crate::{ControlPlaneResult, FakeControlPlaneClient};

use super::{HostedControlPlaneClient, generated};
use generated::{
    DashboardGetDashboardShell, DashboardGetWorkspaceDashboardCurrent,
    DashboardListDashboardDevices, DashboardListDashboardRecoveryEnvelopes,
};

pub type DashboardProviderEnvironment = generated::HostedDashboardProviderEnvironment;
pub type DashboardDeviceState = generated::HostedDashboardDeviceState;
pub type DashboardShellRequest = generated::HostedDashboardGetDashboardShellRequest;
pub type DashboardShellResponse = generated::HostedDashboardGetDashboardShellResponse;
pub type WorkspaceDashboardCurrentRequest =
    generated::HostedDashboardGetWorkspaceDashboardCurrentRequest;
pub type WorkspaceDashboardCurrentResponse =
    generated::HostedDashboardGetWorkspaceDashboardCurrentResponse;
pub type DashboardDevicesRequest = generated::HostedDashboardListDashboardDevicesRequest;
pub type DashboardDevicesResponse = generated::HostedDashboardListDashboardDevicesResponse;
pub type DashboardRecoveryEnvelopesRequest =
    generated::HostedDashboardListDashboardRecoveryEnvelopesRequest;
pub type DashboardRecoveryEnvelopesResponse =
    generated::HostedDashboardListDashboardRecoveryEnvelopesResponse;

pub trait DashboardCurrentStateControlPlaneClient {
    fn get_dashboard_shell(
        &self,
        request: DashboardShellRequest,
    ) -> ControlPlaneResult<DashboardShellResponse>;

    fn get_workspace_dashboard_current(
        &self,
        request: WorkspaceDashboardCurrentRequest,
    ) -> ControlPlaneResult<WorkspaceDashboardCurrentResponse>;

    fn list_dashboard_devices(
        &self,
        request: DashboardDevicesRequest,
    ) -> ControlPlaneResult<DashboardDevicesResponse>;

    fn list_dashboard_recovery_envelopes(
        &self,
        request: DashboardRecoveryEnvelopesRequest,
    ) -> ControlPlaneResult<DashboardRecoveryEnvelopesResponse>;
}

impl DashboardCurrentStateControlPlaneClient for HostedControlPlaneClient {
    fn get_dashboard_shell(
        &self,
        mut request: DashboardShellRequest,
    ) -> ControlPlaneResult<DashboardShellResponse> {
        request.account_session_id = request
            .account_session_id
            .or_else(|| self.account_session_id.clone());
        self.call::<DashboardGetDashboardShell>(&request)
    }

    fn get_workspace_dashboard_current(
        &self,
        mut request: WorkspaceDashboardCurrentRequest,
    ) -> ControlPlaneResult<WorkspaceDashboardCurrentResponse> {
        request.account_session_id = request
            .account_session_id
            .or_else(|| self.account_session_id.clone());
        self.call::<DashboardGetWorkspaceDashboardCurrent>(&request)
    }

    fn list_dashboard_devices(
        &self,
        mut request: DashboardDevicesRequest,
    ) -> ControlPlaneResult<DashboardDevicesResponse> {
        request.account_session_id = request
            .account_session_id
            .or_else(|| self.account_session_id.clone());
        self.call::<DashboardListDashboardDevices>(&request)
    }

    fn list_dashboard_recovery_envelopes(
        &self,
        mut request: DashboardRecoveryEnvelopesRequest,
    ) -> ControlPlaneResult<DashboardRecoveryEnvelopesResponse> {
        request.account_session_id = request
            .account_session_id
            .or_else(|| self.account_session_id.clone());
        self.call::<DashboardListDashboardRecoveryEnvelopes>(&request)
    }
}

impl DashboardCurrentStateControlPlaneClient for FakeControlPlaneClient {
    fn get_dashboard_shell(
        &self,
        request: DashboardShellRequest,
    ) -> ControlPlaneResult<DashboardShellResponse> {
        let account_id = request
            .account_session_id
            .unwrap_or_else(|| "fake-account".to_string());
        Ok(DashboardShellResponse {
            account: generated::HostedDashboardAccount {
                account_id,
                work_os_user_id: "fake-user".to_string(),
                work_os_organization_id: None,
            },
            workspaces: Vec::new(),
            billing: None,
            billing_projection: unavailable_projection_metadata(),
            next_cursor: None,
            has_more: false,
            total: 0,
        })
    }

    fn get_workspace_dashboard_current(
        &self,
        request: WorkspaceDashboardCurrentRequest,
    ) -> ControlPlaneResult<WorkspaceDashboardCurrentResponse> {
        Ok(WorkspaceDashboardCurrentResponse {
            workspace_id: request.workspace_id,
            workspace_ref: None,
            status: None,
            status_projection: unavailable_projection_metadata(),
            devices: None,
            device_projection: unavailable_projection_metadata(),
            recovery: None,
            recovery_projection: unavailable_projection_metadata(),
            recent_activity_count: 0,
            recent_activity_watermark: 0,
            activity_projection: unavailable_projection_metadata(),
            revision: 0,
            generated_at: fake_timestamp(),
        })
    }

    fn list_dashboard_devices(
        &self,
        _request: DashboardDevicesRequest,
    ) -> ControlPlaneResult<DashboardDevicesResponse> {
        Ok(DashboardDevicesResponse {
            rows: Vec::new(),
            next_cursor: None,
            has_more: false,
            total: 0,
            revision: 0,
            projection: unavailable_projection_metadata(),
        })
    }

    fn list_dashboard_recovery_envelopes(
        &self,
        _request: DashboardRecoveryEnvelopesRequest,
    ) -> ControlPlaneResult<DashboardRecoveryEnvelopesResponse> {
        Ok(DashboardRecoveryEnvelopesResponse {
            rows: Vec::new(),
            next_cursor: None,
            has_more: false,
            total: 0,
            revision: 0,
            projection: unavailable_projection_metadata(),
        })
    }
}

fn unavailable_projection_metadata() -> generated::HostedProjectionMetadata {
    generated::HostedProjectionMetadata {
        schema_version: 1,
        version: 0,
        source_watermark: 0,
        updated_at: fake_timestamp(),
        generated_at: fake_timestamp(),
        repair_state: generated::HostedProjectionRepairState::Unavailable,
        failure_reason: Some("projection-not-seeded".to_string()),
    }
}

fn fake_timestamp() -> String {
    "1970-01-01T00:00:00Z".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_current_state_contract<T: DashboardCurrentStateControlPlaneClient>() {}

    #[test]
    fn fake_and_hosted_clients_share_the_generated_current_state_contract() {
        assert_current_state_contract::<FakeControlPlaneClient>();
        assert_current_state_contract::<HostedControlPlaneClient>();
    }

    #[test]
    fn fake_current_state_lists_match_empty_hosted_pages() {
        let fake = FakeControlPlaneClient::default();
        let shell = fake
            .get_dashboard_shell(DashboardShellRequest {
                account_session_id: Some("account-session".to_string()),
                provider_environment: DashboardProviderEnvironment::Sandbox,
                selected_workspace_id: None,
                cursor: None,
                limit: Some(20),
            })
            .expect("fake dashboard shell");
        let current = fake
            .get_workspace_dashboard_current(WorkspaceDashboardCurrentRequest {
                account_session_id: Some("account-session".to_string()),
                workspace_id: "workspace".to_string(),
            })
            .expect("fake workspace current projection");
        let devices = fake
            .list_dashboard_devices(DashboardDevicesRequest {
                account_session_id: Some("account-session".to_string()),
                workspace_id: "workspace".to_string(),
                state: DashboardDeviceState::Authorized,
                cursor: None,
                limit: Some(50),
            })
            .expect("fake device projection");
        let recovery = fake
            .list_dashboard_recovery_envelopes(DashboardRecoveryEnvelopesRequest {
                account_session_id: Some("account-session".to_string()),
                workspace_id: "workspace".to_string(),
                cursor: None,
                limit: Some(50),
            })
            .expect("fake recovery projection");

        assert!(shell.billing.is_none());
        assert_eq!(
            shell.billing_projection.repair_state,
            generated::HostedProjectionRepairState::Unavailable
        );
        assert!(current.status.is_none());
        assert!(current.devices.is_none());
        assert!(current.recovery.is_none());
        assert_eq!(
            current.status_projection.repair_state,
            generated::HostedProjectionRepairState::Unavailable
        );
        assert!(devices.rows.is_empty());
        assert!(!devices.has_more);
        assert_eq!(
            devices.projection.repair_state,
            generated::HostedProjectionRepairState::Unavailable
        );
        assert!(recovery.rows.is_empty());
        assert!(!recovery.has_more);
        assert_eq!(
            recovery.projection.repair_state,
            generated::HostedProjectionRepairState::Unavailable
        );
    }
}
