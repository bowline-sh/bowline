use super::*;

pub(super) fn requested_agent_handoff(args: &BootstrapSshArgs) -> Option<BootstrapAgentHandoff> {
    Some(BootstrapAgentHandoff {
        project: args.project.clone()?,
        task: args.task.clone()?,
        agent: args.agent.clone(),
        lease_id: None,
        write_target_mode: None,
        write_target_path: None,
        work_view_id: None,
        work_view_path: None,
        launched: false,
        accepted: false,
    })
}

pub(super) fn create_agent_handoff_if_requested<R>(
    runner: &R,
    options: &BootstrapSshOptions,
    args: &BootstrapSshArgs,
    steps: &mut Vec<BootstrapStep>,
) -> Option<BootstrapAgentHandoff>
where
    R: ProcessRunner,
{
    let mut handoff = requested_agent_handoff(args)?;
    match ssh::create_remote_agent_lease(runner, options, &handoff.project, &handoff.task) {
        Ok(probe) => match extract_agent_handoff(&probe.stdout) {
            Ok(remote_lease) => {
                handoff.lease_id = Some(remote_lease.lease_id.clone());
                handoff.write_target_mode = Some(remote_lease.write_target_mode);
                handoff.write_target_path = Some(remote_lease.write_target_path.clone());
                handoff.work_view_id = remote_lease.work_view_id.clone();
                handoff.work_view_path = remote_lease.work_view_path.clone();
                steps.push(step(
                    "agent-lease",
                    BootstrapStepState::Completed,
                    format!("Started remote agent work {}.", remote_lease.lease_id),
                ));
                if handoff.agent.as_deref() == Some("codex") {
                    run_requested_remote_codex(runner, options, &mut handoff, steps);
                } else if let Some(agent) = handoff.agent.as_deref() {
                    steps.push(step(
                        "agent-run",
                        BootstrapStepState::Blocked,
                        format!("Remote agent `{agent}` is not launchable by bootstrap yet."),
                    ));
                }
                Some(handoff)
            }
            Err(error) => {
                steps.push(step(
                    "agent-lease",
                    BootstrapStepState::Blocked,
                    format!("Remote agent start output was not valid bowline JSON: {error}"),
                ));
                Some(handoff)
            }
        },
        Err(error) => {
            steps.push(step(
                "agent-lease",
                BootstrapStepState::Blocked,
                format!("Remote agent start failed: {error}"),
            ));
            Some(handoff)
        }
    }
}

pub(super) fn run_requested_remote_codex<R>(
    runner: &R,
    options: &BootstrapSshOptions,
    handoff: &mut BootstrapAgentHandoff,
    steps: &mut Vec<BootstrapStep>,
) where
    R: ProcessRunner,
{
    let Some(lease_id) = handoff.lease_id.as_deref() else {
        return;
    };
    let Some(write_target_mode) = handoff.write_target_mode else {
        return;
    };
    let Some(write_target_path) = handoff.write_target_path.as_deref() else {
        return;
    };
    match ssh::launch_remote_codex_agent(runner, options, lease_id, write_target_path) {
        Ok(_) => steps.push(step(
            "agent-run",
            BootstrapStepState::Completed,
            format!("Codex finished remote lease {lease_id}."),
        )),
        Err(error) => {
            steps.push(step(
                "agent-run",
                BootstrapStepState::Blocked,
                format!("Codex launch failed for remote lease {lease_id}: {error}"),
            ));
            return;
        }
    }
    handoff.launched = true;

    if write_target_mode == AgentWriteTargetMode::Direct {
        match ssh::complete_remote_agent_lease(runner, options, lease_id) {
            Ok(probe) => match completed_direct_lease_summary(&probe.stdout, lease_id) {
                Ok(summary) => {
                    steps.push(step(
                        "agent-complete",
                        BootstrapStepState::Completed,
                        summary,
                    ));
                    handoff.accepted = true;
                }
                Err(summary) => {
                    steps.push(step("agent-complete", BootstrapStepState::Blocked, summary))
                }
            },
            Err(error) => steps.push(step(
                "agent-complete",
                BootstrapStepState::Blocked,
                format!("Remote agent complete failed for {lease_id}: {error}"),
            )),
        }
        return;
    }

    let Some(work_view_id) = handoff.work_view_id.as_deref() else {
        steps.push(step(
            "agent-accept",
            BootstrapStepState::Blocked,
            format!("Remote work-view lease {lease_id} did not include a work view id."),
        ));
        return;
    };
    match ssh::accept_remote_work_view(runner, options, work_view_id) {
        Ok(probe) => match accepted_work_view_summary(&probe.stdout) {
            Ok(summary) => {
                steps.push(step("agent-accept", BootstrapStepState::Completed, summary));
                handoff.accepted = true;
            }
            Err(summary) => steps.push(step("agent-accept", BootstrapStepState::Blocked, summary)),
        },
        Err(error) => steps.push(step(
            "agent-accept",
            BootstrapStepState::Blocked,
            format!("Remote work-view accept failed for {work_view_id}: {error}"),
        )),
    }
}

pub(super) fn completed_direct_lease_summary(
    stdout: &str,
    lease_id: &str,
) -> Result<String, String> {
    let output = serde_json::from_str::<serde_json::Value>(stdout).map_err(|error| {
        format!("Remote agent complete output was not valid bowline JSON: {error}")
    })?;
    match output
        .pointer("/outcome")
        .and_then(|outcome| outcome.as_str())
    {
        Some("allowed") => Ok(format!(
            "Completed direct remote lease {lease_id}; edits remain in the real project path."
        )),
        Some("denied")
            if output
                .pointer("/denial/code")
                .and_then(|code| code.as_str())
                == Some("lease-not-active") =>
        {
            Ok(format!(
                "Direct remote lease {lease_id} was already completed."
            ))
        }
        Some(outcome) => Err(format!(
            "Remote agent complete returned unexpected outcome {outcome}."
        )),
        None => Err("Remote agent complete output did not include an outcome.".to_string()),
    }
}

pub(super) fn accepted_work_view_summary(stdout: &str) -> Result<String, String> {
    let output = serde_json::from_str::<serde_json::Value>(stdout).map_err(|error| {
        format!("Remote work-view accept output was not valid bowline JSON: {error}")
    })?;
    let work_view_id = output
        .pointer("/workView/id")
        .and_then(|id| id.as_str())
        .unwrap_or("unknown");
    match output.pointer("/action").and_then(|action| action.as_str()) {
        Some("accepted") => Ok(format!(
            "Accepted remote work view {work_view_id} into the real project."
        )),
        Some("review-ready") => Err(output
            .pointer("/status/attentionItems/0")
            .and_then(|item| item.as_str())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| {
                format!("Remote work view {work_view_id} still needs review before accepting.")
            })),
        Some(action) => Err(format!(
            "Remote work-view accept returned unexpected action {action}."
        )),
        None => Err("Remote work-view accept output did not include an action.".to_string()),
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
