use std::env;

use bowline_control_plane::AuthorizedDeviceRecord;
use bowline_core::{
    commands::DevicesCommandOutput,
    devices::{DeviceRecord, DeviceTrustState},
    ids::WorkspaceId,
};
use bowline_local::bootstrap::{
    process::SystemProcessRunner,
    ssh::{self, BootstrapSshOptions},
};

use crate::runtime;

const ENV_TRUSTED_TARGETS: &str = "BOWLINE_HANDOFF_TRUSTED_TARGETS";

pub(crate) fn trust_error_for_target(
    target: &str,
    project_path: &str,
    fake_remote: bool,
) -> Option<String> {
    if fake_remote {
        return fake_trust_error(target);
    }

    let runner = SystemProcessRunner;
    let options = handoff_ssh_options(target, project_path);
    let probe = match ssh::list_remote_devices(&runner, &options) {
        Ok(probe) => probe,
        Err(error) => {
            return Some(format!("Target trust could not be verified: {error}"));
        }
    };
    let output = match serde_json::from_str::<DevicesCommandOutput>(&probe.stdout) {
        Ok(output) => output,
        Err(error) => {
            return Some(format!(
                "Target trust output was not valid Bowline JSON: {error}"
            ));
        }
    };
    let workspace_id = runtime::workspace_id_for_root(project_path)
        .unwrap_or_else(|_| runtime::active_workspace_id());
    let control_plane = match runtime::control_plane() {
        Ok(control_plane) => control_plane,
        Err(error) => {
            return Some(format!("Local trust could not be verified: {error}"));
        }
    };
    let trust = match control_plane.list_device_trust(&workspace_id) {
        Ok(trust) => trust,
        Err(error) => {
            return Some(format!("Local trust could not be verified: {error}"));
        }
    };

    remote_trust_error(&output, &trust.authorized_devices, &workspace_id)
}

fn fake_trust_error(target: &str) -> Option<String> {
    let Ok(trusted) = env::var(ENV_TRUSTED_TARGETS) else {
        return Some("Target is not trusted for handoff.".to_string());
    };
    let matches = trusted
        .split(',')
        .map(str::trim)
        .any(|candidate| candidate == target);
    (!matches).then_some("Target is not trusted for handoff.".to_string())
}

pub(crate) fn remote_trust_error(
    output: &DevicesCommandOutput,
    authorized_devices: &[AuthorizedDeviceRecord],
    workspace_id: &WorkspaceId,
) -> Option<String> {
    if output.workspace_id.as_ref() != Some(workspace_id) {
        return Some("Target belongs to a different Bowline workspace.".to_string());
    }
    let Some(remote_device) = remote_current_device(output) else {
        return Some("Target did not report a current Bowline device.".to_string());
    };
    if &remote_device.workspace_id != workspace_id {
        return Some("Target belongs to a different Bowline workspace.".to_string());
    }
    if remote_device.trust_state != DeviceTrustState::Trusted {
        return Some("Target is not trusted for handoff.".to_string());
    }
    let locally_trusted = authorized_devices.iter().any(|device| {
        device.workspace_id == workspace_id.as_str()
            && device.device_id == remote_device.id.as_str()
            && device.device_fingerprint == remote_device.device_fingerprint.as_str()
            && device.revoked_at.is_none()
    });
    (!locally_trusted).then_some("Target is not trusted for handoff.".to_string())
}

fn remote_current_device(output: &DevicesCommandOutput) -> Option<&DeviceRecord> {
    output
        .local_device
        .as_ref()
        .filter(|device| device.is_current_device)
        .or_else(|| {
            output
                .devices
                .iter()
                .find(|device| device.is_current_device)
        })
}

pub(crate) fn handoff_ssh_options(target: &str, project_path: &str) -> BootstrapSshOptions {
    BootstrapSshOptions {
        host: target.to_string(),
        root: project_path.to_string(),
        remote_binary: None,
        remote_platform: None,
        remote_workspace_id: None,
        remote_env: Vec::new(),
        remote_secret_env: Vec::new(),
        bootstrap_token: None,
    }
}
