use super::*;

// Creates the cross-device handoff lease on the trusted remote host, then stops.
// Bootstrap no longer launches, completes, or accepts the agent: the host's
// materialize path makes the workspace appear on arrival, and the human/agent
// runtime drives the work from there.
pub(super) fn create_agent_handoff_if_requested<R>(
    runner: &R,
    options: &BootstrapSshOptions,
    args: &BootstrapSshArgs,
    steps: &mut Vec<BootstrapStep>,
) where
    R: ProcessRunner,
{
    let (Some(project), Some(task)) = (args.project.as_deref(), args.task.as_deref()) else {
        return;
    };
    match ssh::create_remote_agent_lease(runner, options, project, task) {
        Ok(probe) => match extract_agent_handoff(&probe.stdout) {
            Ok(remote_lease) => {
                let RemoteAgentHandoffLease {
                    lease_id,
                    write_target_mode,
                    write_target_path,
                    work_view_id,
                    work_view_path,
                } = remote_lease;
                let target = match write_target_mode {
                    AgentWriteTargetMode::WorkView => format!(
                        "work view {} at {}",
                        work_view_id.as_deref().unwrap_or("unknown"),
                        work_view_path
                            .as_deref()
                            .unwrap_or(write_target_path.as_str())
                    ),
                    AgentWriteTargetMode::Direct => format!("project path {write_target_path}"),
                };
                steps.push(step(
                    BootstrapStepName::AgentLease,
                    BootstrapStepState::Completed,
                    format!(
                        "Prepared remote agent work {lease_id} targeting {target}; the trusted host materializes the workspace."
                    ),
                ));
            }
            Err(error) => {
                steps.push(step(
                    BootstrapStepName::AgentLease,
                    BootstrapStepState::Blocked,
                    format!("Remote agent start output was not valid bowline JSON: {error}"),
                ));
            }
        },
        Err(error) => {
            steps.push(step(
                BootstrapStepName::AgentLease,
                BootstrapStepState::Blocked,
                format!("Remote agent start failed: {error}"),
            ));
        }
    }
}

pub(super) fn extract_agent_handoff(stdout: &str) -> Result<RemoteAgentHandoffLease, String> {
    let value =
        serde_json::from_str::<serde_json::Value>(stdout).map_err(|error| error.to_string())?;
    let lease_id = value
        .pointer("/lease/id")
        .and_then(|id| id.as_str())
        .filter(|id| !id.is_empty())
        .ok_or_else(|| "missing lease.id".to_string())?
        .to_string();
    let work_view_id = value
        .pointer("/lease/workViewId")
        .and_then(|id| id.as_str())
        .filter(|id| !id.is_empty())
        .map(ToOwned::to_owned);
    let write_target_mode = match value
        .pointer("/lease/writeTargetMode")
        .and_then(|mode| mode.as_str())
    {
        Some("direct") => AgentWriteTargetMode::Direct,
        Some("work-view") => AgentWriteTargetMode::WorkView,
        Some(mode) => return Err(format!("unsupported lease.writeTargetMode {mode}")),
        None if work_view_id.is_some() => AgentWriteTargetMode::WorkView,
        None => AgentWriteTargetMode::Direct,
    };
    let work_view_path = value
        .pointer("/lease/workViewPath")
        .and_then(|path| path.as_str())
        .filter(|path| !path.is_empty())
        .map(ToOwned::to_owned);
    let write_target_path = value
        .pointer("/lease/writeTargetPath")
        .and_then(|path| path.as_str())
        .filter(|path| !path.is_empty())
        .or_else(|| {
            value
                .pointer("/lease/outputTarget/path")
                .and_then(|path| path.as_str())
                .filter(|path| !path.is_empty())
        })
        .or(work_view_path.as_deref())
        .ok_or_else(|| "missing lease.writeTargetPath".to_string())?
        .to_string();
    if write_target_mode == AgentWriteTargetMode::WorkView && work_view_id.is_none() {
        return Err("missing lease.workViewId for work-view lease".to_string());
    }
    Ok(RemoteAgentHandoffLease {
        lease_id,
        write_target_mode,
        write_target_path,
        work_view_id,
        work_view_path,
    })
}
