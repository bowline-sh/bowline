use std::{error::Error, fmt, thread, time::Duration};

use bowline_core::{
    commands::{CONTRACT_VERSION, LoginCommandOutput},
    devices::{AccountLoginState, AccountLoginStatus},
    ids::{AccountId, WorkOsOrganizationId, WorkOsUserId},
    status::RepairCommand,
};
use reqwest::blocking::Client;
use serde::Deserialize;

use crate::device_keys::{AccountTokens, DeviceKeyError, DeviceKeyStore};

const AUTHORIZE_DEVICE_URL: &str = "https://api.workos.com/user_management/authorize/device";
const AUTHENTICATE_URL: &str = "https://api.workos.com/user_management/authenticate";
const DEVICE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
const WORKOS_HTTP_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_DEVICE_LOGIN_POLL: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkOsLoginOptions {
    pub client_id: String,
    pub generated_at: String,
    pub poll: bool,
}

#[derive(Clone, PartialEq, Eq)]
pub struct WorkOsDeviceAuthorization {
    device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: String,
    pub expires_in: u64,
    pub interval: u64,
}

impl fmt::Debug for WorkOsDeviceAuthorization {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkOsDeviceAuthorization")
            .field("device_code", &"[redacted]")
            .field("user_code", &self.user_code)
            .field("verification_uri", &self.verification_uri)
            .field("verification_uri_complete", &self.verification_uri_complete)
            .field("expires_in", &self.expires_in)
            .field("interval", &self.interval)
            .finish()
    }
}

#[derive(Debug)]
pub enum WorkOsLoginError {
    Http(reqwest::Error),
    DeviceAuthorizationFailed(String),
    AccessDenied,
    Expired,
    KeyStore(DeviceKeyError),
}

pub fn start_login(
    options: WorkOsLoginOptions,
) -> Result<(WorkOsDeviceAuthorization, LoginCommandOutput), WorkOsLoginError> {
    let client = workos_http_client()?;
    let authorization = request_device_authorization(&client, &options.client_id)?;
    let output = LoginCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: bowline_core::commands::CommandName::Login,
        generated_at: options.generated_at,
        account: AccountLoginState {
            status: AccountLoginStatus::LoginPending,
            account_id: None,
            work_os_user_id: None,
            work_os_organization_id: None,
            user_code: Some(authorization.user_code.clone()),
            verification_uri: Some(authorization.verification_uri.clone()),
            verification_uri_complete: Some(authorization.verification_uri_complete.clone()),
            poll_interval_seconds: Some(authorization.interval.min(u16::MAX as u64) as u16),
            expires_at: None,
            authenticated_at: None,
        },
        local_device: None,
        next_actions: vec![RepairCommand::inspect(
            "Open the verification URL and confirm the code".to_string(),
            None,
        )],
    };
    Ok((authorization, output))
}

pub fn login_and_store<K>(
    key_store: &K,
    options: WorkOsLoginOptions,
) -> Result<LoginCommandOutput, WorkOsLoginError>
where
    K: DeviceKeyStore + ?Sized,
{
    let client = workos_http_client()?;
    let authorization = request_device_authorization(&client, &options.client_id)?;
    if !options.poll {
        return Ok(LoginCommandOutput {
            contract_version: CONTRACT_VERSION,
            command: bowline_core::commands::CommandName::Login,
            generated_at: options.generated_at,
            account: AccountLoginState {
                status: AccountLoginStatus::LoginPending,
                account_id: None,
                work_os_user_id: None,
                work_os_organization_id: None,
                user_code: Some(authorization.user_code),
                verification_uri: Some(authorization.verification_uri),
                verification_uri_complete: Some(authorization.verification_uri_complete),
                poll_interval_seconds: Some(authorization.interval.min(u16::MAX as u64) as u16),
                expires_at: None,
                authenticated_at: None,
            },
            local_device: None,
            next_actions: vec![RepairCommand::inspect(
                "Open the verification URL and confirm the code".to_string(),
                None,
            )],
        });
    }

    complete_login_and_store(
        key_store,
        &options.client_id,
        authorization,
        options.generated_at,
    )
}

pub fn complete_login_and_store<K>(
    key_store: &K,
    client_id: &str,
    authorization: WorkOsDeviceAuthorization,
    generated_at: String,
) -> Result<LoginCommandOutput, WorkOsLoginError>
where
    K: DeviceKeyStore + ?Sized,
{
    let client = workos_http_client()?;
    let token = poll_for_tokens(&client, client_id, authorization)?;
    let user_id = token.user.id.clone();
    let organization_id = token.organization_id.clone();
    let tokens = account_tokens_from_response(token);
    let account_id = tokens.account_id.clone();
    key_store.store_account_tokens(tokens)?;
    Ok(LoginCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: bowline_core::commands::CommandName::Login,
        generated_at: generated_at.clone(),
        account: AccountLoginState {
            status: AccountLoginStatus::AccountAuthenticated,
            account_id: Some(account_id),
            work_os_user_id: Some(WorkOsUserId::new(user_id)),
            work_os_organization_id: organization_id.map(WorkOsOrganizationId::new),
            user_code: None,
            verification_uri: None,
            verification_uri_complete: None,
            poll_interval_seconds: None,
            expires_at: None,
            authenticated_at: Some(generated_at),
        },
        local_device: None,
        next_actions: vec![RepairCommand::inspect(
            "Choose and trust the workspace root".to_string(),
            Some("bowline setup".to_string()),
        )],
    })
}

pub fn refresh_tokens(
    client_id: &str,
    refresh_token: &str,
) -> Result<AccountTokens, WorkOsLoginError> {
    let client = workos_http_client()?;
    let token = client
        .post(AUTHENTICATE_URL)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client_id),
        ])
        .send()?
        .error_for_status()?
        .json::<TokenResponse>()?;
    Ok(account_tokens_from_response(token))
}

pub fn refresh_and_store<K>(
    key_store: &K,
    client_id: &str,
    refresh_token: &str,
) -> Result<AccountTokens, WorkOsLoginError>
where
    K: DeviceKeyStore + ?Sized,
{
    let mut tokens = refresh_tokens(client_id, refresh_token)?;
    if let Some(existing) = key_store.load_account_tokens()? {
        preserve_existing_account_session(&existing, &mut tokens);
    }
    key_store.store_account_tokens(tokens.clone())?;
    Ok(tokens)
}

fn preserve_existing_account_session(existing: &AccountTokens, refreshed: &mut AccountTokens) {
    if existing.account_id == refreshed.account_id && refreshed.account_session.is_none() {
        refreshed.account_session = existing.account_session.clone();
    }
}

fn account_tokens_from_response(token: TokenResponse) -> AccountTokens {
    AccountTokens {
        account_id: AccountId::new(token.user.id),
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        expires_at: token.expires_at.unwrap_or_else(|| "unknown".to_string()),
        account_session: None,
    }
}

fn request_device_authorization(
    client: &Client,
    client_id: &str,
) -> Result<WorkOsDeviceAuthorization, WorkOsLoginError> {
    let response = client
        .post(AUTHORIZE_DEVICE_URL)
        .form(&[("client_id", client_id)])
        .send()?
        .error_for_status()?
        .json::<DeviceAuthorizationResponse>()?;
    Ok(WorkOsDeviceAuthorization {
        device_code: response.device_code,
        user_code: response.user_code,
        verification_uri: response.verification_uri,
        verification_uri_complete: response.verification_uri_complete,
        expires_in: response.expires_in,
        interval: response.interval.max(1),
    })
}

fn poll_for_tokens(
    client: &Client,
    client_id: &str,
    authorization: WorkOsDeviceAuthorization,
) -> Result<TokenResponse, WorkOsLoginError> {
    let mut interval = authorization.interval.max(1);
    let max_poll_seconds = authorization
        .expires_in
        .min(MAX_DEVICE_LOGIN_POLL.as_secs())
        .max(interval);
    let max_attempts = (max_poll_seconds / interval).max(1);
    for _ in 0..max_attempts {
        let response = client
            .post(AUTHENTICATE_URL)
            .form(&[
                ("grant_type", DEVICE_GRANT_TYPE),
                ("device_code", &authorization.device_code),
                ("client_id", client_id),
            ])
            .send()?;
        if response.status().is_success() {
            return response.json::<TokenResponse>().map_err(Into::into);
        }
        let error = response.json::<TokenErrorResponse>()?;
        match error.error.as_str() {
            "authorization_pending" => thread::sleep(Duration::from_secs(interval)),
            "slow_down" => {
                interval += 1;
                thread::sleep(Duration::from_secs(interval));
            }
            "access_denied" => return Err(WorkOsLoginError::AccessDenied),
            "expired_token" => return Err(WorkOsLoginError::Expired),
            other => {
                return Err(WorkOsLoginError::DeviceAuthorizationFailed(
                    other.to_string(),
                ));
            }
        }
    }
    Err(WorkOsLoginError::Expired)
}

fn workos_http_client() -> Result<Client, WorkOsLoginError> {
    Ok(Client::builder().timeout(WORKOS_HTTP_TIMEOUT).build()?)
}

#[derive(Debug, Deserialize)]
struct DeviceAuthorizationResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: String,
    expires_in: u64,
    interval: u64,
}

#[derive(Debug, Deserialize)]
struct TokenErrorResponse {
    error: String,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    user: WorkOsUser,
    organization_id: Option<String>,
    access_token: String,
    refresh_token: String,
    expires_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WorkOsUser {
    id: String,
}

impl fmt::Display for WorkOsLoginError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Http(error) => write!(formatter, "WorkOS login request failed: {error}"),
            Self::DeviceAuthorizationFailed(error) => {
                write!(formatter, "WorkOS device authorization failed: {error}")
            }
            Self::AccessDenied => write!(formatter, "WorkOS login was denied"),
            Self::Expired => write!(formatter, "WorkOS login expired"),
            Self::KeyStore(error) => error.fmt(formatter),
        }
    }
}

impl Error for WorkOsLoginError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Http(error) => Some(error),
            Self::KeyStore(error) => Some(error),
            _ => None,
        }
    }
}

impl From<reqwest::Error> for WorkOsLoginError {
    fn from(error: reqwest::Error) -> Self {
        Self::Http(error)
    }
}

impl From<DeviceKeyError> for WorkOsLoginError {
    fn from(error: DeviceKeyError) -> Self {
        Self::KeyStore(error)
    }
}

#[cfg(test)]
mod tests {
    use bowline_core::ids::AccountId;

    use crate::{
        account::workos::preserve_existing_account_session,
        device_keys::{AccountSessionCredentials, AccountTokens},
    };

    #[test]
    fn refreshed_tokens_preserve_existing_durable_account_session_for_same_account() {
        let existing = AccountTokens {
            account_id: AccountId::new("user_123"),
            access_token: "old-access".to_string(),
            refresh_token: "old-refresh".to_string(),
            expires_at: "old-expiry".to_string(),
            account_session: Some(AccountSessionCredentials {
                session_id: "bowline_session_existing".to_string(),
                revocation_token: "bowline_revoke_existing".to_string(),
            }),
        };
        let mut refreshed = AccountTokens {
            account_id: AccountId::new("user_123"),
            access_token: "new-access".to_string(),
            refresh_token: "new-refresh".to_string(),
            expires_at: "new-expiry".to_string(),
            account_session: None,
        };

        preserve_existing_account_session(&existing, &mut refreshed);

        assert_eq!(
            refreshed.account_session,
            Some(AccountSessionCredentials {
                session_id: "bowline_session_existing".to_string(),
                revocation_token: "bowline_revoke_existing".to_string(),
            })
        );
    }
}
