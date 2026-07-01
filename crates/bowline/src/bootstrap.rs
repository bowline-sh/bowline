use std::{env, thread, time::Duration};

use bowline_control_plane::{BootstrapSessionInput, ControlPlaneClient};
use bowline_core::{
    commands::{
        AgentWriteTargetMode, BootstrapSecretStore, BootstrapSshCommandOutput, BootstrapStep,
        BootstrapStepState, BootstrapSyncState, CONTRACT_VERSION, DevicesCommandOutput,
        StatusCommandOutput,
    },
    devices::{DeviceApprovalRequest, DeviceFingerprint, DeviceRecord, DeviceTrustState},
    ids::DeviceId,
    status::{SafeAction, StatusLevel, WorkspaceStatus},
};
use bowline_local::bootstrap::{
    install::{self, BootstrapInstallOptions, RemoteBowlineInstall},
    process::{ProcessRunner, SystemProcessRunner},
    ssh::{self, BootstrapSshOptions},
};
use bowline_local::device_keys::DeviceKeyStore;

use crate::runtime;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapSshArgs {
    pub host: String,
    pub root: String,
    pub artifact: Option<String>,
    pub project: Option<String>,
    pub task: Option<String>,
    pub agent: Option<String>,
}

struct BootstrapOutputBase {
    host: String,
    root: String,
    generated_at: String,
    steps: Vec<BootstrapStep>,
    agent_handoff: Option<BootstrapAgentHandoff>,
}

struct BootstrapAgentHandoff {
    project: String,
    task: String,
    agent: Option<String>,
    lease_id: Option<String>,
    write_target_mode: Option<AgentWriteTargetMode>,
    write_target_path: Option<String>,
    work_view_id: Option<String>,
    work_view_path: Option<String>,
    launched: bool,
    accepted: bool,
}

struct RemoteAgentHandoffLease {
    lease_id: String,
    write_target_mode: AgentWriteTargetMode,
    write_target_path: String,
    work_view_id: Option<String>,
    work_view_path: Option<String>,
}

pub fn run(args: BootstrapSshArgs, generated_at: String) -> BootstrapSshCommandOutput {
    let args = normalize_remote_root(args);
    let runner = SystemProcessRunner;
    let mut steps = Vec::new();
    let install = match install::install_or_update_bowline(
        &runner,
        &BootstrapInstallOptions {
            host: args.host.clone(),
            root: args.root.clone(),
            artifact: args.artifact.clone().map(Into::into),
        },
    ) {
        Ok(install) => {
            steps.push(step(
                "install",
                BootstrapStepState::Completed,
                format!(
                    "Installed bowline and bowline-daemon for {} with artifacts {} / {}.",
                    install.platform.label(),
                    &install.artifact_sha256[..16],
                    &install.daemon_artifact_sha256[..16]
                ),
            ));
            install
        }
        Err(error) => {
            steps.push(step(
                "install",
                BootstrapStepState::Blocked,
                format!("Remote install failed: {error}"),
            ));
            return bootstrap_output(
                output_base(&args, &generated_at, steps),
                None,
                None,
                false,
                None,
            );
        }
    };
    let control_plane = match runtime::control_plane() {
        Ok(control_plane) => control_plane,
        Err(error) => {
            steps.push(step(
                "control-plane",
                BootstrapStepState::Blocked,
                format!("Local control-plane client unavailable: {error}"),
            ));
            return bootstrap_output(
                output_base(&args, &generated_at, steps),
                None,
                None,
                false,
                None,
            );
        }
    };
    let key_store = match runtime::key_store() {
        Ok(key_store) => key_store,
        Err(error) => {
            steps.push(step(
                "approve",
                BootstrapStepState::Blocked,
                format!("Local secret store unavailable: {error}"),
            ));
            return bootstrap_output(
                output_base(&args, &generated_at, steps),
                None,
                None,
                false,
                None,
            );
        }
    };
    let workspace_id = runtime::active_workspace_id();
    let approving_device_id = runtime::daemon_device_id(&workspace_id);
    run_after_install(
        &runner,
        args,
        generated_at,
        steps,
        install,
        &*control_plane,
        &*key_store,
        workspace_id,
        approving_device_id,
        remote_bootstrap_secret_env(),
    )
}

fn normalize_remote_root(mut args: BootstrapSshArgs) -> BootstrapSshArgs {
    if let Ok(home) = env::var("HOME") {
        args.root = normalize_remote_root_for_home(&args.root, &home);
    }
    args
}

fn normalize_remote_root_for_home(root: &str, home: &str) -> String {
    if root == home {
        return "~".to_string();
    }
    root.strip_prefix(&format!("{home}/"))
        .map(|rest| format!("~/{rest}"))
        .unwrap_or_else(|| root.to_string())
}

#[allow(clippy::too_many_arguments)]
mod after_install;
mod agent_handoff;
mod output;
mod remote;

#[cfg(test)]
mod tests;

use after_install::*;
use agent_handoff::*;
use output::*;
use remote::*;
