use bowline_core::{
    commands::{CONTRACT_VERSION, CommandName, LogoutCommandOutput},
    status::SafeAction,
};
use std::process::ExitCode;

use crate::{
    EXIT_RUNTIME, generated_at, print_json, print_runtime_error, render_logout_human, runtime,
};

pub fn run(generated_at: String) -> Result<LogoutCommandOutput, String> {
    let key_store = runtime::key_store()?;
    let signed_out = key_store
        .clear_account_tokens()
        .map_err(|error| error.to_string())?;
    Ok(LogoutCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::Logout,
        generated_at,
        signed_out,
        next_actions: vec![SafeAction {
            label: "Sign in again".to_string(),
            command: Some("bowline login".to_string()),
        }],
    })
}

pub(super) fn print_logout(json: bool) -> ExitCode {
    let generated_at = generated_at();
    match run(generated_at.clone()) {
        Ok(output) if json => {
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(output) => {
            print!("{}", render_logout_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error(CommandName::Logout, generated_at, &error, json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}
