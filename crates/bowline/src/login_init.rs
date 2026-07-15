use super::*;

pub(super) fn print_login(mut args: login::LoginArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    args = login_args_for_output(args, json);
    if !json && !args.no_poll && !args.headless {
        return print_polling_login(generated_at);
    }
    match login::run(args, generated_at.clone()) {
        Ok(output) if json => {
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(output) => {
            print!("{}", render_login_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error(CommandName::Login, generated_at, &error, json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

pub(super) fn print_polling_login(generated_at: String) -> ExitCode {
    let (authorization, pending_output) = match login::start(generated_at.clone()) {
        Ok(started) => started,
        Err(error) => {
            print_runtime_error(CommandName::Login, generated_at, &error, false);
            return ExitCode::from(EXIT_RUNTIME);
        }
    };

    print!("{}", render_login_human(&pending_output));
    let _ = io::stdout().flush();

    match login::finish(authorization, generated_at.clone()) {
        Ok(_) => print_machine_setup_output(
            Some("~/Code".to_string()),
            generated_at,
            MachineSetupPrintOptions {
                format: OutputFormat::Human,
                login_poll: LoginPollMode::Poll,
            },
        ),
        Err(error) => {
            print_runtime_error(CommandName::Login, generated_at, &error, false);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputFormat {
    Human,
    Json,
}

pub(super) fn login_args_for_output(mut args: login::LoginArgs, json: bool) -> login::LoginArgs {
    if json {
        args.no_poll = true;
    }
    args
}

pub(super) fn print_setup(args: SetupArgs, json: bool, socket: &Path) -> ExitCode {
    let generated_at = generated_at();
    match args.mode {
        SetupMode::Machine { root } => print_machine_setup(root, generated_at, json, socket),
        SetupMode::Project { project_path, yes } => {
            print_project_setup(project_path, yes, generated_at, json)
        }
    }
}

fn print_project_setup(
    project_path: String,
    yes: bool,
    generated_at: String,
    json: bool,
) -> ExitCode {
    let mut approve_setup = yes;

    loop {
        let outcome = run_project_setup(ProjectSetupOptions {
            db_path: metadata_db_path(),
            project_path: resolve_explicit_path(project_path.clone()),
            approve_setup,
            trigger: if approve_setup {
                "cli-approved-setup".to_string()
            } else {
                "cli-setup".to_string()
            },
            generated_at: generated_at.clone(),
        });

        match outcome {
            Ok(outcome)
                if !json && !approve_setup && outcome.state == ProjectSetupState::SetupBlocked =>
            {
                println!("Setup needs approval: {}", outcome.redacted_summary);
                if !confirm_return("Approve setup?") {
                    return ExitCode::SUCCESS;
                }
                approve_setup = true;
            }
            Ok(outcome) if json => {
                print_json(&setup_project_output(
                    CommandName::Setup,
                    generated_at,
                    outcome,
                ));
                return ExitCode::SUCCESS;
            }
            Ok(outcome) => {
                println!("Setup {:?}: {}", outcome.state, outcome.redacted_summary);
                return ExitCode::SUCCESS;
            }
            Err(error) => {
                print_runtime_error(CommandName::Setup, generated_at, &error.to_string(), json);
                return ExitCode::from(EXIT_RUNTIME);
            }
        }
    }
}

fn setup_project_output(
    command: CommandName,
    generated_at: String,
    outcome: bowline_local::setup::ProjectSetupOutcome,
) -> SetupProjectOutput {
    SetupProjectOutput {
        contract_version: CONTRACT_VERSION,
        command,
        generated_at,
        outcome: SetupProjectOutcome {
            workspace_id: outcome.workspace_id,
            project_id: outcome.project_id,
            project_path: outcome.project_path,
            state: setup_project_state(outcome.state),
            receipt_ids: outcome.receipt_ids,
            redacted_summary: outcome.redacted_summary,
        },
    }
}

fn setup_project_state(state: bowline_local::setup::ProjectSetupState) -> SetupProjectState {
    match state {
        bowline_local::setup::ProjectSetupState::Hot => SetupProjectState::Hot,
        bowline_local::setup::ProjectSetupState::SetupBlocked => SetupProjectState::SetupBlocked,
        bowline_local::setup::ProjectSetupState::NoSetupNeeded => SetupProjectState::NoSetupNeeded,
    }
}

fn print_machine_setup(
    root: Option<String>,
    generated_at: String,
    json: bool,
    socket: &Path,
) -> ExitCode {
    let interactive = !json && io::stdin().is_terminal() && io::stdout().is_terminal();
    if interactive {
        return print_interactive_machine_setup(root, generated_at, socket);
    }
    print_machine_setup_output(
        root,
        generated_at,
        MachineSetupPrintOptions {
            format: if json {
                OutputFormat::Json
            } else {
                OutputFormat::Human
            },
            login_poll: LoginPollMode::Skip,
        },
    )
}

fn print_interactive_machine_setup(
    root: Option<String>,
    generated_at: String,
    socket: &Path,
) -> ExitCode {
    let default_root = root
        .clone()
        .or_else(runtime::active_workspace_root)
        .unwrap_or_else(|| "~/Code".to_string());
    match surface::tui::run_onboarding_app(surface::tui::OnboardingModel::new(default_root)) {
        Ok(Some(result)) => {
            let code = print_machine_setup_output(
                result.root.or(root),
                generated_at.clone(),
                MachineSetupPrintOptions {
                    format: OutputFormat::Human,
                    login_poll: LoginPollMode::Poll,
                },
            );
            if code == ExitCode::SUCCESS
                && let Some(command) = result.connect_command
            {
                return run_confirmed_tui_command(&command, socket);
            }
            code
        }
        Ok(None) => ExitCode::SUCCESS,
        Err(error) => {
            print_runtime_error(CommandName::Setup, generated_at, &error.to_string(), false);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

fn print_machine_setup_output(
    root: Option<String>,
    generated_at: String,
    print_options: MachineSetupPrintOptions,
) -> ExitCode {
    match run_machine_setup(
        root,
        generated_at.clone(),
        print_options.login_poll == LoginPollMode::Poll,
    ) {
        Ok(outcome) if print_options.format == OutputFormat::Json => {
            print_json(&outcome.output);
            ExitCode::SUCCESS
        }
        Ok(outcome) => {
            let workspace_id = outcome.output.workspace_id.clone();
            print!("{}", render_setup_human(&outcome.output));
            if print_options.login_poll == LoginPollMode::Poll
                && let Some(request_id) = outcome.device_trust_attachment.pending_request_id()
            {
                return wait_for_device_grant(
                    CommandName::Setup,
                    workspace_id,
                    request_id,
                    generated_at,
                );
            }
            ExitCode::SUCCESS
        }
        Err(MachineSetupError::AmbiguousRoot(candidates)) => {
            print_ambiguous_setup_root(
                candidates,
                generated_at,
                print_options.format == OutputFormat::Json,
            );
            ExitCode::from(EXIT_USAGE)
        }
        Err(MachineSetupError::Runtime(error)) => {
            print_runtime_error(
                CommandName::Setup,
                generated_at,
                &error,
                print_options.format == OutputFormat::Json,
            );
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoginPollMode {
    Poll,
    Skip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MachineSetupPrintOptions {
    format: OutputFormat,
    login_poll: LoginPollMode,
}

#[derive(Debug)]
enum MachineSetupError {
    AmbiguousRoot(Vec<PathBuf>),
    Runtime(String),
}

#[derive(Debug)]
struct MachineSetupOutcome {
    output: SetupCommandOutput,
    device_trust_attachment: DeviceTrustAttachment,
}

fn run_machine_setup(
    root: Option<String>,
    generated_at: String,
    poll_login: bool,
) -> Result<MachineSetupOutcome, MachineSetupError> {
    if setup_login_should_run_before_root_init() {
        return run_pending_login_setup(root, generated_at, poll_login);
    }

    let mut init = initialize_setup_root(root, generated_at.clone())?;
    let login = setup_login_state(&generated_at, poll_login)?;
    let device_trust_attachment = advance_device_trust(&mut init, &generated_at);
    let mut next_actions = init.next_actions.clone();
    next_actions.extend(login.next_actions.clone());
    append_setup_status_actions(&mut next_actions, &init.root, &generated_at);
    Ok(MachineSetupOutcome {
        output: setup_command_output(SetupCommandParts {
            generated_at,
            workspace_id: init.workspace_id,
            root: init.root,
            root_choice: init.root_choice,
            login: login.account,
            next_actions,
        }),
        device_trust_attachment,
    })
}

fn run_pending_login_setup(
    root: Option<String>,
    generated_at: String,
    poll_login: bool,
) -> Result<MachineSetupOutcome, MachineSetupError> {
    let requested_root = setup_requested_root(root);
    let selection = bowline_local::init::select_or_create_root(requested_root.as_deref())
        .map_err(map_setup_root_error)?;
    let login = setup_login_state(&generated_at, poll_login)?;
    if matches!(
        login.account.status,
        AccountLoginStatus::AccountAuthenticated
    ) {
        let mut init = initialize_setup_root(requested_root, generated_at.clone())?;
        let device_trust_attachment = advance_device_trust(&mut init, &generated_at);
        let mut next_actions = init.next_actions.clone();
        next_actions.extend(login.next_actions.clone());
        append_setup_status_actions(&mut next_actions, &init.root, &generated_at);
        return Ok(MachineSetupOutcome {
            output: setup_command_output(SetupCommandParts {
                generated_at,
                workspace_id: init.workspace_id,
                root: init.root,
                root_choice: init.root_choice,
                login: login.account,
                next_actions,
            }),
            device_trust_attachment,
        });
    }

    let mut next_actions = login.next_actions.clone();
    next_actions.push(RepairCommand::mutating(
        "Complete account login and set up this root".to_string(),
        Some(root_command(
            "bowline setup --root",
            &selection.display_root,
        )),
    ));
    Ok(MachineSetupOutcome {
        output: setup_command_output(SetupCommandParts {
            generated_at,
            workspace_id: runtime::active_workspace_id_without_local_metadata_probe(),
            root: selection.display_root,
            root_choice: selection.root_choice,
            login: login.account,
            next_actions,
        }),
        device_trust_attachment: DeviceTrustAttachment::NoPendingApproval,
    })
}

struct SetupCommandParts {
    generated_at: String,
    workspace_id: WorkspaceId,
    root: String,
    root_choice: bowline_core::commands::RootChoiceState,
    login: AccountLoginState,
    next_actions: Vec<RepairCommand>,
}

fn setup_command_output(parts: SetupCommandParts) -> SetupCommandOutput {
    SetupCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::Setup,
        generated_at: parts.generated_at,
        workspace_id: parts.workspace_id,
        root: parts.root,
        root_choice: parts.root_choice,
        login: parts.login,
        next_actions: dedupe_actions(parts.next_actions),
        connected_host: None,
    }
}

fn setup_login_should_run_before_root_init() -> bool {
    !fake_control_plane_enabled()
        && !stored_account_tokens_available()
        && !setup_env_credentials_available()
}

fn stored_account_tokens_available() -> bool {
    runtime::key_store()
        .ok()
        .and_then(|store| store.load_account_tokens().ok().flatten())
        .is_some()
}

fn setup_env_credentials_available() -> bool {
    env_account_session_present()
        || env_workos_access_token_present()
        || env_control_plane_token_present()
        || env_bootstrap_token_present()
}

fn setup_login_state(
    generated_at: &str,
    poll_login: bool,
) -> Result<bowline_core::commands::LoginCommandOutput, MachineSetupError> {
    if let Ok(key_store) = runtime::key_store()
        && let Ok(Some(tokens)) = key_store.load_account_tokens()
    {
        let mut account = empty_login_state(AccountLoginStatus::AccountAuthenticated);
        account.account_id = Some(tokens.account_id);
        account.expires_at = Some(tokens.expires_at);
        return Ok(bowline_core::commands::LoginCommandOutput {
            contract_version: CONTRACT_VERSION,
            command: CommandName::Login,
            generated_at: generated_at.to_string(),
            account,
            local_device: None,
            next_actions: Vec::new(),
        });
    }
    if fake_control_plane_enabled() {
        return Ok(bowline_core::commands::LoginCommandOutput {
            contract_version: CONTRACT_VERSION,
            command: CommandName::Login,
            generated_at: generated_at.to_string(),
            account: empty_login_state(AccountLoginStatus::NotLoggedIn),
            local_device: None,
            next_actions: vec![RepairCommand::inspect(
                "Log in before enabling workspace sync".to_string(),
                Some("bowline login".to_string()),
            )],
        });
    }
    if setup_env_credentials_available() {
        return Ok(bowline_core::commands::LoginCommandOutput {
            contract_version: CONTRACT_VERSION,
            command: CommandName::Login,
            generated_at: generated_at.to_string(),
            account: empty_login_state(AccountLoginStatus::NotLoggedIn),
            local_device: None,
            next_actions: vec![RepairCommand::inspect(
                "Log in to persist account credentials on this machine".to_string(),
                Some("bowline login".to_string()),
            )],
        });
    }
    if poll_login {
        let (authorization, pending_output) =
            login::start(generated_at.to_string()).map_err(MachineSetupError::Runtime)?;
        print!("{}", render_login_human(&pending_output));
        let _ = io::stdout().flush();
        return login::finish(authorization, generated_at.to_string())
            .map(clear_completed_setup_login_actions)
            .map_err(MachineSetupError::Runtime);
    }
    login::run(
        login::LoginArgs {
            no_poll: true,
            headless: false,
        },
        generated_at.to_string(),
    )
    .map_err(MachineSetupError::Runtime)
}

fn clear_completed_setup_login_actions(
    mut output: bowline_core::commands::LoginCommandOutput,
) -> bowline_core::commands::LoginCommandOutput {
    if output.account.status == AccountLoginStatus::AccountAuthenticated {
        output.next_actions.clear();
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_clears_login_retry_action_after_account_authenticates() {
        let output = clear_completed_setup_login_actions(login_output_with_status(
            AccountLoginStatus::AccountAuthenticated,
        ));

        assert!(output.next_actions.is_empty());
    }

    #[test]
    fn setup_keeps_login_action_while_account_login_is_pending() {
        let output = clear_completed_setup_login_actions(login_output_with_status(
            AccountLoginStatus::LoginPending,
        ));

        assert_eq!(output.next_actions.len(), 1);
        assert_eq!(
            output.next_actions[0].command.as_deref(),
            Some("bowline login")
        );
    }

    fn login_output_with_status(
        status: AccountLoginStatus,
    ) -> bowline_core::commands::LoginCommandOutput {
        bowline_core::commands::LoginCommandOutput {
            contract_version: CONTRACT_VERSION,
            command: CommandName::Login,
            generated_at: "2026-07-03T12:00:00Z".to_string(),
            account: empty_login_state(status),
            local_device: None,
            next_actions: vec![RepairCommand::inspect(
                "Log in".to_string(),
                Some("bowline login".to_string()),
            )],
        }
    }
}

fn empty_login_state(status: AccountLoginStatus) -> AccountLoginState {
    AccountLoginState {
        status,
        account_id: None,
        work_os_user_id: None,
        work_os_organization_id: None,
        user_code: None,
        verification_uri: None,
        verification_uri_complete: None,
        poll_interval_seconds: None,
        expires_at: None,
        authenticated_at: None,
    }
}

fn fake_control_plane_enabled() -> bool {
    matches!(
        env::var("BOWLINE_USE_FAKE_CONTROL_PLANE").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

fn initialize_setup_root(
    root: Option<String>,
    generated_at: String,
) -> Result<bowline_core::commands::RootInitOutput, MachineSetupError> {
    let options = InitOptions {
        db_path: metadata_db_path(),
        requested_root: setup_requested_root(root),
        generated_at: generated_at.clone(),
    };
    bowline_local::init::initialize_root_with_workspace(options, runtime::active_workspace_id())
        .map_err(map_setup_root_error)
}

fn setup_requested_root(root: Option<String>) -> Option<String> {
    root.or_else(runtime::active_workspace_root)
        .map(resolve_explicit_path)
}

fn map_setup_root_error(error: LocalInitError) -> MachineSetupError {
    match error {
        LocalInitError::AmbiguousDefaultRoot(candidates) => {
            MachineSetupError::AmbiguousRoot(candidates)
        }
        error => MachineSetupError::Runtime(error.to_string()),
    }
}

fn append_setup_status_actions(actions: &mut Vec<RepairCommand>, root: &str, generated_at: &str) {
    let Ok(status) = compose_status_for_cli(StatusOptions {
        db_path: metadata_db_path(),
        requested_path: Some(root.to_string()),
        workspace_scope: false,
        generated_at: generated_at.to_string(),
    }) else {
        return;
    };
    actions.extend(status.next_actions);
}

fn dedupe_actions(actions: Vec<RepairCommand>) -> Vec<RepairCommand> {
    let mut deduped = Vec::new();
    for action in actions {
        if !deduped.iter().any(|existing: &RepairCommand| {
            existing.command.is_some() && existing.command == action.command
                || existing.label == action.label && existing.command == action.command
        }) {
            deduped.push(action);
        }
    }
    deduped
}

mod trust;
use trust::{DeviceTrustAttachment, root_command};
pub(super) use trust::{advance_device_trust, wait_for_device_grant};

pub(super) fn env_workos_access_token_present() -> bool {
    env::var("BOWLINE_WORKOS_ACCESS_TOKEN")
        .ok()
        .is_some_and(|value| !value.is_empty())
}

pub(super) fn env_account_session_present() -> bool {
    [
        "BOWLINE_ACCOUNT_SESSION_ID",
        "BOWLINE_ACCOUNT_SESSION_REVOCATION_TOKEN",
    ]
    .into_iter()
    .all(|name| env::var(name).ok().is_some_and(|value| !value.is_empty()))
}

pub(super) fn env_control_plane_token_present() -> bool {
    env::var("BOWLINE_CONTROL_PLANE_TOKEN")
        .ok()
        .is_some_and(|value| !value.is_empty())
}

pub(super) fn env_bootstrap_token_present() -> bool {
    env::var("BOWLINE_BOOTSTRAP_TOKEN")
        .ok()
        .is_some_and(|value| !value.is_empty())
}
