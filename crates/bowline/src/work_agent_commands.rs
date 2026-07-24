use super::*;
use crate::command_error_classification::print_work_error;

pub(super) fn print_events(args: EventsArgs, json: bool, quiet: bool) -> ExitCode {
    let generated_at = generated_at();
    let options = EventsOptions {
        db_path: metadata_db_path(),
        requested_path: selected_workspace_path(args.selection),
        workspace_scope: false,
        generated_at: generated_at.clone(),
        limit: args.limit,
    };

    match bowline_local::status::compose_events(options) {
        Ok(mut output) if json => {
            abbreviate_events_requested_path(&mut output);
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(output) if quiet => write_human_or_exit(
            CommandName::Events,
            generated_at,
            &render_events_quiet(&output),
        ),
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

pub(super) fn print_work_create(args: work::WorkCreateArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    let project_path = io_helpers::resolve_project_path(args.project_path);
    let args = work::WorkCreateArgs {
        project_path,
        name: args.name,
        from: args.from,
    };
    match work::run_work_create(
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
            print!("{}", work::render_work_create_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => print_work_error(CommandName::WorkCreate, generated_at, &error, json).into(),
    }
}

pub(super) fn print_work(args: work::WorkListArgs, json: bool, quiet: bool) -> ExitCode {
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
        Ok(output) if quiet => {
            write_human_or_exit(CommandName::Work, generated_at, &render_work_quiet(&output))
        }
        Ok(output) => {
            print!("{}", work::render_list_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => print_work_error(CommandName::Work, generated_at, &error, json).into(),
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
        Err(error) => print_work_error(CommandName::Diff, generated_at, &error, json).into(),
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
        Err(error) => print_work_error(CommandName::Review, generated_at, &error, json).into(),
    }
}

pub(super) fn print_work_lifecycle(
    lifecycle: work::WorkLifecycle,
    args: work::WorkSelectorArgs,
    json: bool,
) -> ExitCode {
    let generated_at = generated_at();
    let workspace_id = runtime::active_workspace_id();
    let result = work::run_lifecycle(
        lifecycle,
        args,
        metadata_db_path(),
        runtime::daemon_device_id(&workspace_id),
        generated_at.clone(),
    );
    match result {
        Ok(output) if json => {
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(output) => {
            print!("{}", work::render_lifecycle_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => {
            let command = lifecycle.command_name();
            print_work_error(command, generated_at, &error, json).into()
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
        Err(error) => print_work_error(CommandName::Cleanup, generated_at, &error, json).into(),
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
