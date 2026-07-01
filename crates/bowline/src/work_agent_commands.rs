use super::*;

pub(super) fn print_explain(args: ExplainArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    let options = ExplainOptions {
        db_path: metadata_db_path(),
        requested_path: resolve_explicit_path(args.path),
        generated_at: generated_at.clone(),
    };

    match bowline_local::explain::compose_explain(options) {
        Ok(output) if json => {
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(output) => {
            print!("{}", bowline_local::explain::render_explain_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error(CommandName::Explain, generated_at, &error.to_string(), json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

pub(super) fn print_events(args: EventsArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    let options = EventsOptions {
        db_path: metadata_db_path(),
        requested_path: requested_path(args.path),
        workspace_scope: args.workspace,
        generated_at: generated_at.clone(),
        limit: args.limit,
    };

    match bowline_local::status::compose_events(options) {
        Ok(mut output) if json => {
            abbreviate_events_requested_path(&mut output);
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(mut output) => {
            abbreviate_events_requested_path(&mut output);
            print!("{}", bowline_local::status::render_events_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error(CommandName::Events, generated_at, &error.to_string(), json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

pub(super) fn print_workon(args: work::WorkonArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    let project_path = resolve_explicit_path(args.project_path);
    let args = work::WorkonArgs {
        project_path,
        name: args.name,
    };
    match work::run_workon(
        args,
        metadata_db_path(),
        runtime::device_id(),
        generated_at.clone(),
    ) {
        Ok(output) if json => {
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(output) => {
            print!("{}", work::render_workon_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error(CommandName::Workon, generated_at, &error.to_string(), json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

pub(super) fn print_work(args: work::WorkListArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    match work::run_list(
        args,
        metadata_db_path(),
        runtime::device_id(),
        generated_at.clone(),
    ) {
        Ok(output) if json => {
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(output) => {
            print!("{}", work::render_list_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error(CommandName::Work, generated_at, &error.to_string(), json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

pub(super) fn print_work_diff(args: work::WorkSelectorArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    match work::run_diff(args, metadata_db_path(), generated_at.clone()) {
        Ok(output) if json => {
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(output) => {
            print!("{}", work::render_diff_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error(CommandName::Diff, generated_at, &error.to_string(), json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

pub(super) fn print_work_review(args: work::WorkSelectorArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    match work::run_diff(args, metadata_db_path(), generated_at.clone()) {
        Ok(mut output) if json => {
            output.command = CommandName::Review;
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(mut output) => {
            output.command = CommandName::Review;
            print!("{}", work::render_diff_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error(CommandName::Review, generated_at, &error.to_string(), json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

pub(super) fn print_work_lifecycle(
    command: CommandName,
    args: work::WorkSelectorArgs,
    json: bool,
) -> ExitCode {
    let generated_at = generated_at();
    match work::run_lifecycle(command, args, metadata_db_path(), generated_at.clone()) {
        Ok(output) if json => {
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(output) => {
            print!("{}", work::render_lifecycle_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error(command, generated_at, &error.to_string(), json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

pub(super) fn print_work_cleanup(args: work::WorkCleanupArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    match work::run_cleanup(args, metadata_db_path(), generated_at.clone()) {
        Ok(output) if json => {
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(output) => {
            print!("{}", work::render_cleanup_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error(CommandName::Cleanup, generated_at, &error.to_string(), json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

pub(super) fn print_agent_lease_create(args: agent::AgentLeaseCreateArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    let args = agent::AgentLeaseCreateArgs {
        project_path: resolve_explicit_path(args.project_path),
        task: args.task,
        base: args.base,
        hydrate_budget_bytes: args.hydrate_budget_bytes,
        work_view: args.work_view,
    };
    match agent::run_lease_create(
        args,
        metadata_db_path(),
        runtime::device_id(),
        generated_at.clone(),
    ) {
        Ok(output) if json => {
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(output) => {
            print!("{}", agent::render_lease_create_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error(
                CommandName::AgentStart,
                generated_at,
                &error.to_string(),
                json,
            );
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

pub(super) fn print_agent_context(args: agent::AgentLeaseSelectorArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    match agent::run_context(args, metadata_db_path(), generated_at.clone()) {
        Ok(output) if json => {
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(output) => {
            print!("{}", agent::render_context_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error(
                CommandName::AgentContext,
                generated_at,
                &error.to_string(),
                json,
            );
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

pub(super) fn print_agent_prompt(args: agent::AgentLeaseSelectorArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    match agent::run_prompt(args, metadata_db_path(), generated_at.clone()) {
        Ok(output) if json => {
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(output) => {
            print!("{}", agent::render_prompt_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error(
                CommandName::AgentPrompt,
                generated_at,
                &error.to_string(),
                json,
            );
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

pub(super) fn print_agent_tool_action(
    command: CommandName,
    args: agent::AgentLeaseSelectorArgs,
    json: bool,
) -> ExitCode {
    let generated_at = generated_at();
    let result = match command {
        CommandName::AgentPublish => {
            agent::run_publish(args, metadata_db_path(), generated_at.clone())
        }
        CommandName::AgentComplete => {
            agent::run_complete(args, metadata_db_path(), generated_at.clone())
        }
        _ => unreachable!("unsupported agent tool command"),
    };
    match result {
        Ok(output) if json => {
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(output) => {
            print!("{}", agent::render_tool_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error(command, generated_at, &error.to_string(), json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

pub(super) fn print_agent_budget(args: agent::AgentBudgetArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    match agent::run_budget(args, metadata_db_path(), generated_at.clone()) {
        Ok(output) if json => {
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(output) => {
            print!("{}", agent::render_budget_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error(
                CommandName::AgentBudget,
                generated_at,
                &error.to_string(),
                json,
            );
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

pub(super) fn print_bootstrap_ssh(args: bootstrap::BootstrapSshArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    let output = bootstrap::run(args, generated_at);
    let success = bootstrap_ssh_succeeded(&output);
    if json {
        print_json(&output);
    } else {
        print!("{}", render_bootstrap_ssh_human(&output));
    }
    if success {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(EXIT_RUNTIME)
    }
}

pub(super) fn bootstrap_ssh_succeeded(
    output: &bowline_core::commands::BootstrapSshCommandOutput,
) -> bool {
    output.trusted
        && output
            .steps
            .iter()
            .all(|step| step.state != bowline_core::commands::BootstrapStepState::Blocked)
}
