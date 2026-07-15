use std::{
    env, fs,
    io::{self, Read},
    path::{Path, PathBuf},
    process::ExitCode,
    time::{SystemTime, UNIX_EPOCH},
};

use bowline_core::status::RepairCommand;
use bowline_local::agents::handoff::{
    HandoffDiscovery, HandoffDiscoveryOptions, HandoffSelectError, SelectedHandoff, bundle,
    discover_sessions, select_handoff, tmux, transfer,
};
use bowline_local::bootstrap::{
    process::SystemProcessRunner,
    ssh::{self, BootstrapSshOptions},
};

use crate::{
    EXIT_RUNTIME, EXIT_USAGE, HandoffArgs, confirm_return, generated_at,
    handoff_trust::{handoff_ssh_options, trust_error_for_target},
    print_json, print_runtime_error, print_usage_error, resolve_explicit_path, shell_word,
};
use bowline_core::commands::{
    CONTRACT_VERSION, CommandName, HandoffAgent, HandoffCandidate, HandoffCommandOutput,
    HandoffError, HandoffOutcome, HandoffPlan, HandoffReceipt, HandoffSessionMode,
    HandoffTransferPlan,
};

const ENV_FAKE_REMOTE_ROOT: &str = "BOWLINE_HANDOFF_FAKE_REMOTE_ROOT";
const ENV_TRANSFER_KEY: &str = "BOWLINE_HANDOFF_TRANSFER_KEY";

pub(super) fn print_handoff(args: HandoffArgs, json: bool, dry_run: bool) -> ExitCode {
    let generated_at = generated_at();
    let project_path = args
        .project
        .clone()
        .map(resolve_explicit_path)
        .or_else(|| {
            env::current_dir()
                .ok()
                .map(|path| path.display().to_string())
        })
        .unwrap_or_else(|| ".".to_string());

    let fake_remote_root = env::var_os(ENV_FAKE_REMOTE_ROOT).map(PathBuf::from);
    if let Some(error) =
        trust_error_for_target(&args.target, &project_path, fake_remote_root.is_some())
    {
        let output = handoff_error_output(
            generated_at,
            args.target,
            project_path,
            Vec::new(),
            "target_not_trusted",
            &error,
        );
        print_handoff_output(output, json);
        return ExitCode::from(EXIT_RUNTIME);
    }

    let prompt = match prompt_text(&args) {
        Ok(prompt) => prompt,
        Err(message) => {
            let output = handoff_error_output(
                generated_at,
                args.target,
                project_path,
                Vec::new(),
                "prompt_unavailable",
                &message,
            );
            print_handoff_output(output, json);
            return ExitCode::from(EXIT_RUNTIME);
        }
    };

    let discovery_options = HandoffDiscoveryOptions::from_project(PathBuf::from(&project_path));
    let discovery = discover_sessions(&discovery_options);
    let require_confirmation = args.agent.is_none() && args.session.is_none();
    let selected = match select_handoff(
        &discovery,
        args.agent,
        args.session.as_deref(),
        prompt.clone(),
        require_confirmation,
    ) {
        Ok(selected) => selected,
        Err(HandoffSelectError::ConfirmationRequired { default_agent }) if !json => {
            let prompt_message =
                confirmation_prompt(&discovery, default_agent, &args.target, &project_path);
            if !confirm_return(&prompt_message) {
                let output = confirmation_required_output(
                    generated_at,
                    &args,
                    &project_path,
                    &discovery,
                    default_agent,
                );
                print_handoff_output(output, json);
                return ExitCode::from(EXIT_USAGE);
            }
            match select_handoff(
                &discovery,
                args.agent,
                args.session.as_deref(),
                prompt,
                false,
            ) {
                Ok(selected) => selected,
                Err(error) => {
                    let code = select_error_code(&error);
                    let output = handoff_error_output(
                        generated_at,
                        args.target,
                        project_path,
                        command_candidates(&discovery, None),
                        code,
                        &error.to_string(),
                    );
                    print_handoff_output(output, json);
                    return ExitCode::from(EXIT_RUNTIME);
                }
            }
        }
        Err(HandoffSelectError::ConfirmationRequired { default_agent }) => {
            let output = confirmation_required_output(
                generated_at,
                &args,
                &project_path,
                &discovery,
                default_agent,
            );
            print_handoff_output(output, json);
            return ExitCode::from(EXIT_USAGE);
        }
        Err(error) => {
            let code = select_error_code(&error);
            let output = handoff_error_output(
                generated_at,
                args.target,
                project_path,
                command_candidates(&discovery, None),
                code,
                &error.to_string(),
            );
            print_handoff_output(output, json);
            return ExitCode::from(EXIT_RUNTIME);
        }
    };

    let plan = render_plan(&args.target, &project_path, &selected);
    if dry_run {
        let candidates = command_candidates(
            &discovery,
            selected
                .session
                .as_ref()
                .map(|item| (item.agent, item.session_id.as_str())),
        );
        let output = HandoffCommandOutput {
            contract_version: CONTRACT_VERSION,
            command: CommandName::Handoff,
            generated_at,
            outcome: HandoffOutcome::DryRun,
            target: args.target,
            project_path,
            candidates,
            selected: selected_candidate(&selected),
            plan: Some(plan),
            receipt: None,
            error: None,
            next_actions: Vec::new(),
        };
        print_handoff_output(output, json);
        return ExitCode::SUCCESS;
    }

    let handoff_result = if let Some(fake_remote_root) = fake_remote_root.as_deref() {
        run_fake_remote_handoff(&args.target, &project_path, &selected, fake_remote_root)
    } else {
        run_real_remote_handoff(&args.target, &project_path, &selected)
    };

    match handoff_result {
        Ok(receipt) => {
            let output = HandoffCommandOutput {
                contract_version: CONTRACT_VERSION,
                command: CommandName::Handoff,
                generated_at,
                outcome: HandoffOutcome::Receipt,
                target: args.target,
                project_path,
                candidates: command_candidates(
                    &discovery,
                    selected
                        .session
                        .as_ref()
                        .map(|item| (item.agent, item.session_id.as_str())),
                ),
                selected: selected_candidate(&selected),
                plan: Some(plan),
                receipt: Some(receipt),
                error: None,
                next_actions: Vec::new(),
            };
            print_handoff_output(output, json);
            ExitCode::SUCCESS
        }
        Err(error) => {
            let output = handoff_error_output(
                generated_at,
                args.target,
                project_path,
                command_candidates(&discovery, None),
                error.code,
                &error.message,
            );
            print_handoff_output(output, json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

pub(super) fn print_handoff_install_bundle(json: bool) -> ExitCode {
    if !json {
        print_usage_error(
            CommandName::Handoff,
            "usage_error",
            "bowline internal handoff install-bundle requires --json",
            false,
        );
        return ExitCode::from(EXIT_USAGE);
    }
    let Ok(target) = env::var("BOWLINE_HANDOFF_TARGET") else {
        print_runtime_error(
            CommandName::Handoff,
            generated_at(),
            "internal handoff install-bundle requires BOWLINE_HANDOFF_TARGET",
            json,
        );
        return ExitCode::from(EXIT_RUNTIME);
    };
    let key = match transfer_key() {
        Ok(key) => key,
        Err(error) => {
            print_runtime_error(CommandName::Handoff, generated_at(), &error.message, json);
            return ExitCode::from(EXIT_RUNTIME);
        }
    };
    let mut stdin = String::new();
    if let Err(error) = io::stdin().read_to_string(&mut stdin) {
        print_runtime_error(
            CommandName::Handoff,
            generated_at(),
            &error.to_string(),
            json,
        );
        return ExitCode::from(EXIT_RUNTIME);
    }
    let envelope = match transfer::envelope_from_json(&stdin) {
        Ok(envelope) => envelope,
        Err(_) => {
            print_runtime_error(
                CommandName::Handoff,
                generated_at(),
                "handoff bundle decrypt failed before install",
                json,
            );
            return ExitCode::from(EXIT_RUNTIME);
        }
    };
    let bundle = match transfer::decrypt_bundle(&envelope, &target, &key) {
        Ok(bundle) => bundle,
        Err(_) => {
            print_runtime_error(
                CommandName::Handoff,
                generated_at(),
                "handoff bundle decrypt failed before install",
                json,
            );
            return ExitCode::from(EXIT_RUNTIME);
        }
    };
    let temp_root = env::var_os("BOWLINE_HANDOFF_TEMP")
        .map(PathBuf::from)
        .unwrap_or_else(|| env::temp_dir().join("bowline-handoff"));
    if let Err(error) = claim_handoff_envelope(&temp_root, &envelope.nonce) {
        print_runtime_error(
            CommandName::Handoff,
            generated_at(),
            &format!("handoff bundle replay check failed: {error}"),
            json,
        );
        return ExitCode::from(EXIT_RUNTIME);
    }
    let agent_home = env::var_os("BOWLINE_HANDOFF_AGENT_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| default_agent_home(bundle.manifest.agent));
    match bundle::install_bundle(&bundle, &agent_home, &temp_root) {
        Ok(receipt) => {
            if json {
                print_json(&receipt);
            } else {
                println!("installed handoff bundle");
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error(
                CommandName::Handoff,
                generated_at(),
                &error.to_string(),
                json,
            );
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

fn run_fake_remote_handoff(
    target: &str,
    project_path: &str,
    selected: &SelectedHandoff,
    fake_remote_root: &Path,
) -> Result<HandoffReceipt, HandoffRuntimeError> {
    let remote_project_path = PathBuf::from(project_path);
    let handoff_bundle = bundle::build_bundle(selected, target, remote_project_path.clone())
        .map_err(|error| HandoffRuntimeError::new("bundle_unavailable", error.to_string()))?;
    let key = transfer_key()?;
    let envelope = transfer::encrypt_bundle(&handoff_bundle, target, &key)
        .map_err(|error| HandoffRuntimeError::new("handoff_transfer_failed", error.to_string()))?;
    let envelope_json = transfer::envelope_json(&envelope)
        .map_err(|error| HandoffRuntimeError::new("handoff_transfer_failed", error.to_string()))?;
    let decrypted = transfer::envelope_from_json(&envelope_json)
        .and_then(|parsed| transfer::decrypt_bundle(&parsed, target, &key))
        .map_err(|error| HandoffRuntimeError::new("handoff_transfer_failed", error.to_string()))?;
    let agent_home = fake_remote_root.join(agent_home_fragment(selected.agent));
    let temp_root = fake_remote_root.join("tmp/handoff");
    let install = bundle::install_bundle(&decrypted, &agent_home, &temp_root)
        .map_err(|error| HandoffRuntimeError::new("handoff_install_failed", error.to_string()))?;
    let launch = tmux::render_launch(&tmux::TmuxLaunchRequest {
        target: target.to_string(),
        agent: selected.agent,
        session_mode: selected.mode,
        session_id: install.session_id.clone(),
        project_path: install.remote_project_path.clone(),
        prompt_file: install.prompt_file.clone(),
        unique_suffix: unique_handoff_suffix(),
    });
    if env::var_os("BOWLINE_HANDOFF_FAKE_NO_TMUX").is_some() {
        return Err(cleanup_fake_prompt_on_launch_error(
            install.prompt_file.as_deref(),
            "tmux_missing",
            "tmux is not available on the target",
        ));
    }
    if let Err(error) = fs::create_dir_all(fake_remote_root.join("tmux")) {
        return Err(cleanup_fake_prompt_on_launch_error(
            install.prompt_file.as_deref(),
            "tmux_missing",
            error.to_string(),
        ));
    }
    if let Err(error) = fs::write(
        fake_remote_root.join("tmux").join(&launch.session_name),
        &launch.launch_command,
    ) {
        return Err(cleanup_fake_prompt_on_launch_error(
            install.prompt_file.as_deref(),
            "tmux_missing",
            error.to_string(),
        ));
    }

    Ok(HandoffReceipt {
        agent: selected.agent,
        target: target.to_string(),
        remote_project_path: install.remote_project_path.display().to_string(),
        tmux_session: launch.session_name,
        attach_command: launch.attach_command,
        monitoring: false,
        workspace_lock: false,
        same_session_concurrency_risk: selected.mode == HandoffSessionMode::ResumeExisting,
        session_mode: selected.mode,
        agent_runtime_verified: false,
        note: "Bowline is not monitoring this session and did not verify agent runtime/auth state."
            .to_string(),
    })
}

fn run_real_remote_handoff(
    target: &str,
    project_path: &str,
    selected: &SelectedHandoff,
) -> Result<HandoffReceipt, HandoffRuntimeError> {
    let remote_project_path = PathBuf::from(project_path);
    let handoff_bundle = bundle::build_bundle(selected, target, remote_project_path.clone())
        .map_err(|error| HandoffRuntimeError::new("bundle_unavailable", error.to_string()))?;
    let key = transfer::generate_transfer_key()
        .map_err(|error| HandoffRuntimeError::new("handoff_transfer_failed", error.to_string()))?;
    let envelope = transfer::encrypt_bundle(&handoff_bundle, target, key.as_bytes())
        .map_err(|error| HandoffRuntimeError::new("handoff_transfer_failed", error.to_string()))?;
    let envelope_json = transfer::envelope_json(&envelope)
        .map_err(|error| HandoffRuntimeError::new("handoff_transfer_failed", error.to_string()))?;
    let runner = SystemProcessRunner;
    let options = handoff_ssh_options(target, project_path);
    let install_probe =
        ssh::install_handoff_bundle(&runner, &options, target, &key, &envelope_json)
            .map_err(remote_error("handoff_install_failed"))?;
    let install = serde_json::from_str::<bundle::HandoffInstallReceipt>(&install_probe.stdout)
        .map_err(|error| HandoffRuntimeError::new("handoff_install_failed", error.to_string()))?;
    let launch = tmux::render_launch(&tmux::TmuxLaunchRequest {
        target: target.to_string(),
        agent: selected.agent,
        session_mode: selected.mode,
        session_id: install.session_id.clone(),
        project_path: install.remote_project_path.clone(),
        prompt_file: install.prompt_file.clone(),
        unique_suffix: unique_handoff_suffix(),
    });
    if let Err(error) = ssh::launch_handoff_tmux(
        &runner,
        &options,
        &launch.launch_command,
        &launch.has_session_command,
    ) {
        let cleanup_error = cleanup_remote_prompt_after_launch_error(
            &runner,
            &options,
            install.prompt_file.as_deref(),
        );
        let mut error = remote_error("tmux_missing")(error);
        if let Some(cleanup_error) = cleanup_error {
            error.message = format!("{}; prompt cleanup failed: {cleanup_error}", error.message);
        }
        return Err(error);
    }

    Ok(HandoffReceipt {
        agent: selected.agent,
        target: target.to_string(),
        remote_project_path: install.remote_project_path.display().to_string(),
        tmux_session: launch.session_name,
        attach_command: launch.attach_command,
        monitoring: false,
        workspace_lock: false,
        same_session_concurrency_risk: selected.mode == HandoffSessionMode::ResumeExisting,
        session_mode: selected.mode,
        agent_runtime_verified: false,
        note: "Bowline is not monitoring this session and did not verify agent runtime/auth state."
            .to_string(),
    })
}

fn remote_error(code: &'static str) -> impl FnOnce(ssh::BootstrapSshError) -> HandoffRuntimeError {
    move |error| HandoffRuntimeError::new(code, error.to_string())
}

fn cleanup_fake_prompt_on_launch_error(
    prompt_file: Option<&Path>,
    code: &'static str,
    message: impl Into<String>,
) -> HandoffRuntimeError {
    let message = message.into();
    match remove_local_prompt_file(prompt_file) {
        Ok(()) => HandoffRuntimeError::new(code, message),
        Err(error) => {
            HandoffRuntimeError::new(code, format!("{message}; prompt cleanup failed: {error}"))
        }
    }
}

fn remove_local_prompt_file(prompt_file: Option<&Path>) -> io::Result<()> {
    let Some(path) = prompt_file else {
        return Ok(());
    };
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn cleanup_remote_prompt_after_launch_error(
    runner: &SystemProcessRunner,
    options: &BootstrapSshOptions,
    prompt_file: Option<&Path>,
) -> Option<ssh::BootstrapSshError> {
    let path = prompt_file?;
    ssh::remove_handoff_prompt_file(runner, options, path).err()
}

struct HandoffRuntimeError {
    code: &'static str,
    message: String,
}

impl HandoffRuntimeError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

fn render_plan(target: &str, project_path: &str, selected: &SelectedHandoff) -> HandoffPlan {
    let launch = tmux::render_launch(&tmux::TmuxLaunchRequest {
        target: target.to_string(),
        agent: selected.agent,
        session_mode: selected.mode,
        session_id: selected
            .session
            .as_ref()
            .map(|session| session.session_id.clone()),
        project_path: PathBuf::from(project_path),
        prompt_file: Some(PathBuf::from("protected prompt staging file")),
        unique_suffix: generated_at(),
    });
    HandoffPlan {
        target: target.to_string(),
        agent: selected.agent,
        session_mode: selected.mode,
        project_path: project_path.to_string(),
        remote_project_path: project_path.to_string(),
        tmux_session: launch.session_name,
        launch_command: launch.launch_command,
        transfer: HandoffTransferPlan {
            encrypted: true,
            durable_cloud_storage: false,
            installs_byte_exact_session_files: true,
            remote_installer_command: "bowline internal handoff install-bundle --json".to_string(),
        },
    }
}

fn command_candidates(
    discovery: &HandoffDiscovery,
    selected: Option<(HandoffAgent, &str)>,
) -> Vec<HandoffCandidate> {
    discovery
        .candidates
        .iter()
        .map(|candidate| HandoffCandidate {
            agent: candidate.agent,
            session_id: candidate.session_id.clone(),
            source_path: candidate.source_path.display().to_string(),
            project_path: candidate
                .project_path
                .as_ref()
                .map(|path| path.display().to_string()),
            modified_at_unix_seconds: candidate.modified_at_unix_seconds,
            selected: selected.is_some_and(|(agent, session_id)| {
                agent == candidate.agent && session_id == candidate.session_id
            }),
            skipped_reason: None,
        })
        .collect()
}

fn selected_candidate(selected: &SelectedHandoff) -> Option<HandoffCandidate> {
    selected.session.as_ref().map(|candidate| HandoffCandidate {
        agent: candidate.agent,
        session_id: candidate.session_id.clone(),
        source_path: candidate.source_path.display().to_string(),
        project_path: candidate
            .project_path
            .as_ref()
            .map(|path| path.display().to_string()),
        modified_at_unix_seconds: candidate.modified_at_unix_seconds,
        selected: true,
        skipped_reason: None,
    })
}

fn confirmation_required_output(
    generated_at: String,
    args: &HandoffArgs,
    project_path: &str,
    discovery: &HandoffDiscovery,
    default_agent: HandoffAgent,
) -> HandoffCommandOutput {
    HandoffCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::Handoff,
        generated_at,
        outcome: HandoffOutcome::ConfirmationRequired,
        target: args.target.clone(),
        project_path: project_path.to_string(),
        candidates: command_candidates(discovery, None),
        selected: None,
        plan: None,
        receipt: None,
        error: Some(HandoffError {
            code: "confirmation_required".to_string(),
            message: confirmation_prompt(discovery, default_agent, &args.target, project_path),
            recoverability: "user-action".to_string(),
        }),
        next_actions: vec![RepairCommand::mutating(
            format!("Confirm latest {} handoff", agent_label(default_agent)),
            Some(format!(
                "bowline handoff {} --agent {}",
                shell_word(&args.target),
                agent_label(default_agent)
            )),
        )],
    }
}

fn confirmation_prompt(
    discovery: &HandoffDiscovery,
    default_agent: HandoffAgent,
    target: &str,
    project_path: &str,
) -> String {
    let default = discovery
        .candidates
        .iter()
        .filter(|candidate| candidate.agent == default_agent)
        .max_by_key(|candidate| candidate.modified_at_unix_seconds);
    match default {
        Some(candidate) => format!(
            "Codex and Claude sessions detected. Handoff latest: {} {} to {} from {}?",
            agent_label(candidate.agent),
            short_session_id(&candidate.session_id),
            target,
            candidate
                .project_path
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| project_path.to_string())
        ),
        None => format!(
            "Codex and Claude sessions detected. Handoff latest ({}) to {} from {}?",
            agent_label(default_agent),
            target,
            project_path
        ),
    }
}

fn short_session_id(session_id: &str) -> String {
    session_id.chars().take(12).collect()
}

fn prompt_text(args: &HandoffArgs) -> Result<Option<String>, String> {
    if let Some(prompt) = args.prompt.as_ref() {
        return Ok(Some(prompt.clone()));
    }
    let Some(prompt_file) = args.prompt_file.as_ref() else {
        return Ok(None);
    };
    fs::read_to_string(resolve_explicit_path(prompt_file.clone()))
        .map(Some)
        .map_err(|error| format!("could not read prompt file: {error}"))
}

fn handoff_error_output(
    generated_at: String,
    target: String,
    project_path: String,
    candidates: Vec<HandoffCandidate>,
    code: &str,
    message: &str,
) -> HandoffCommandOutput {
    HandoffCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::Handoff,
        generated_at,
        outcome: HandoffOutcome::Error,
        target,
        project_path,
        candidates,
        selected: None,
        plan: None,
        receipt: None,
        error: Some(HandoffError {
            code: code.to_string(),
            message: message.to_string(),
            recoverability: "user-action".to_string(),
        }),
        next_actions: vec![RepairCommand::inspect(
            "Inspect handoff help",
            Some("bowline help handoff".to_string()),
        )],
    }
}

fn print_handoff_output(output: HandoffCommandOutput, json: bool) {
    if json {
        print_json(&output);
        return;
    }
    match output.outcome {
        HandoffOutcome::DryRun => {
            println!(
                "handoff dry run: {} -> {}",
                output
                    .selected
                    .as_ref()
                    .map(|candidate| agent_label(candidate.agent))
                    .unwrap_or("agent"),
                output.target
            );
            if let Some(plan) = output.plan {
                println!("tmux session: {}", plan.tmux_session);
                println!(
                    "remote installer: {}",
                    plan.transfer.remote_installer_command
                );
            }
        }
        HandoffOutcome::Receipt => {
            if let Some(receipt) = output.receipt {
                println!("handoff started in tmux: {}", receipt.tmux_session);
                println!("attach: {}", receipt.attach_command);
                println!("{}", receipt.note);
            }
        }
        HandoffOutcome::ConfirmationRequired | HandoffOutcome::Error => {
            if let Some(error) = output.error {
                eprintln!("bowline handoff: {}", error.message);
            }
        }
    }
}

fn select_error_code(error: &HandoffSelectError) -> &'static str {
    match error {
        HandoffSelectError::NoSupportedSession => "no_supported_session",
        HandoffSelectError::NoMatchingSession(_) => "no_matching_session",
        HandoffSelectError::AmbiguousSession(_) => "ambiguous_session",
        HandoffSelectError::ConfirmationRequired { .. } => "confirmation_required",
    }
}

fn transfer_key() -> Result<Vec<u8>, HandoffRuntimeError> {
    let key = env::var(ENV_TRANSFER_KEY).map_err(|_| {
        HandoffRuntimeError::new(
            "transfer_key_unavailable",
            "handoff transfer key is unavailable",
        )
    })?;
    if key.is_empty() {
        return Err(HandoffRuntimeError::new(
            "transfer_key_unavailable",
            "handoff transfer key is empty",
        ));
    }
    Ok(key.into_bytes())
}

fn unique_handoff_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("{}-{nanos}", std::process::id())
}

fn claim_handoff_envelope(temp_root: &Path, nonce: &str) -> io::Result<()> {
    let claim_dir = temp_root.join("consumed");
    create_private_dir(&claim_dir)?;
    let claim_path = claim_dir.join(safe_claim_file(nonce));
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(claim_path)?;
    }
    #[cfg(not(unix))]
    {
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(claim_path)?;
    }
    Ok(())
}

fn create_private_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700).create(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path)?;
    }
    Ok(())
}

fn safe_claim_file(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

fn default_agent_home(agent: HandoffAgent) -> PathBuf {
    let home = env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from);
    home.join(agent_home_fragment(agent))
}

fn agent_home_fragment(agent: HandoffAgent) -> &'static str {
    match agent {
        HandoffAgent::Codex => ".codex",
        HandoffAgent::Claude => ".claude",
    }
}

fn agent_label(agent: HandoffAgent) -> &'static str {
    match agent {
        HandoffAgent::Codex => "codex",
        HandoffAgent::Claude => "claude",
    }
}
