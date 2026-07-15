use bowline_core::commands::LoginCommandOutput;
use bowline_local::account::workos::{self, WorkOsDeviceAuthorization, WorkOsLoginOptions};

use crate::runtime;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginArgs {
    pub no_poll: bool,
    pub headless: bool,
}

pub fn run(args: LoginArgs, generated_at: String) -> Result<LoginCommandOutput, String> {
    let client_id = runtime::hosted_workos_client_id();
    let key_store = runtime::key_store()?;
    let output = workos::login_and_store(
        &*key_store,
        WorkOsLoginOptions {
            client_id,
            generated_at,
            poll: !args.no_poll && !args.headless,
        },
    )
    .map_err(|error| error.to_string())?;
    runtime::ensure_durable_account_session(&*key_store, None)?;
    Ok(output)
}

pub fn start(
    generated_at: String,
) -> Result<(WorkOsDeviceAuthorization, LoginCommandOutput), String> {
    let client_id = runtime::hosted_workos_client_id();
    workos::start_login(WorkOsLoginOptions {
        client_id,
        generated_at,
        poll: false,
    })
    .map_err(|error| error.to_string())
}

pub fn finish(
    authorization: WorkOsDeviceAuthorization,
    generated_at: String,
) -> Result<LoginCommandOutput, String> {
    let client_id = runtime::hosted_workos_client_id();
    let key_store = runtime::key_store()?;
    let output =
        workos::complete_login_and_store(&*key_store, &client_id, authorization, generated_at)
            .map_err(|error| error.to_string())?;
    runtime::ensure_durable_account_session(&*key_store, None)?;
    Ok(output)
}
