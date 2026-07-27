//! The one contract every OS service supervisor implements for the bowline
//! daemon.
//!
//! launchd and systemd differ only in the file they write and the escaping that
//! file needs. Everything above that — the states, the outcome, the config, the
//! takeover/rollback sequence — is the same product behaviour, so it is declared
//! once here and a behaviour change lands once.

use std::fmt;
use std::path::PathBuf;

use bowline_core::introspection::ServiceManager;

use crate::bootstrap::process::ProcessRunner;
use crate::linux_service::SystemdUserService;
use crate::macos_service::LaunchdUserAgent;
use crate::service_runtime::ServiceError;

/// Everything a supervisor needs to run the daemon. Identical on every
/// platform; only the rendering of it differs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceConfig {
    pub daemon: PathBuf,
    pub root: PathBuf,
    pub state_root: PathBuf,
    pub socket: PathBuf,
    pub workspace_id: String,
    pub device_id: String,
}

/// What the supervisor reports about the daemon, or what a mutation just did to
/// it. `Installed`/`Restarted`/`Uninstalled` are only ever mutation results;
/// `Active`/`Inactive` are only ever query results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceState {
    Installed,
    Restarted,
    Uninstalled,
    Active,
    Inactive,
    /// A supervisor label this build does not model, preserved verbatim.
    Unknown(String),
}

impl fmt::Display for ServiceState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Installed => "installed",
            Self::Restarted => "restarted",
            Self::Uninstalled => "uninstalled",
            Self::Active => "active",
            Self::Inactive => "inactive",
            Self::Unknown(state) => state,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceOutcome {
    pub service_name: String,
    pub unit_path: PathBuf,
    pub state: ServiceState,
}

/// Whether the supervisor starts the daemon on its own, captured before a
/// takeover so a rollback restores what was there rather than a default.
/// launchd keeps no registration apart from the plist, so it reports `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceRegistration {
    Enabled,
    /// Enabled until the next boot only (systemd `enabled-runtime`).
    EnabledUntilReboot,
    Disabled,
}

pub trait ServiceSupervisor {
    /// The supervisor's own name for the daemon service. Known before any
    /// directory resolution, so an unavailable supervisor can still be named.
    fn service_name(&self) -> &'static str;

    /// Where the supervisor's definition file lives.
    fn definition_path(&self) -> PathBuf;

    /// The message to fail a takeover with when an active service has no
    /// definition to roll back to.
    fn missing_definition_error(&self) -> &'static str;

    /// Render the definition the supervisor would install for `config`. Exposed
    /// so callers can diff or preview without touching the filesystem.
    fn render_definition(&self, config: &ServiceConfig) -> String;

    fn install_or_update(&self, config: &ServiceConfig) -> Result<ServiceOutcome, ServiceError>;

    fn restart(&self) -> Result<ServiceOutcome, ServiceError>;

    fn stop(&self) -> Result<ServiceOutcome, ServiceError>;

    fn uninstall(&self) -> Result<ServiceOutcome, ServiceError>;

    fn status(&self) -> Result<ServiceOutcome, ServiceError>;

    fn registration(&self) -> Result<Option<ServiceRegistration>, ServiceError>;

    /// Put `definition` back and re-register the daemon exactly as
    /// `registration` described it.
    fn restore(
        &self,
        definition: &[u8],
        registration: Option<ServiceRegistration>,
    ) -> Result<ServiceOutcome, ServiceError>;
}

/// The only place that decides which supervisor owns this host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformService {
    Systemd,
    Launchd,
}

impl PlatformService {
    pub fn current() -> Option<Self> {
        if cfg!(target_os = "linux") {
            return Some(Self::Systemd);
        }
        if cfg!(target_os = "macos") {
            return Some(Self::Launchd);
        }
        None
    }

    pub fn service_name(self) -> &'static str {
        match self {
            Self::Systemd => crate::linux_service::SERVICE_NAME,
            Self::Launchd => crate::macos_service::PLIST_NAME,
        }
    }

    pub fn manager(self) -> ServiceManager {
        match self {
            Self::Systemd => ServiceManager::Systemd,
            Self::Launchd => ServiceManager::Launchd,
        }
    }

    /// Bind the supervisor to a process runner. Fails when the supervisor exists
    /// but its user session does not — no HOME, no GUI launch domain.
    pub fn supervisor<'a, R>(
        self,
        runner: &'a R,
    ) -> Result<Box<dyn ServiceSupervisor + 'a>, ServiceError>
    where
        R: ProcessRunner,
    {
        match self {
            Self::Systemd => Ok(Box::new(SystemdUserService::for_current_user(runner)?)),
            Self::Launchd => Ok(Box::new(LaunchdUserAgent::for_current_user(runner)?)),
        }
    }
}

/// The supervisor for this host, ready to run commands. `None` means no
/// supported supervisor exists here.
pub fn current_platform_supervisor<R>(
    runner: &R,
) -> Option<Result<Box<dyn ServiceSupervisor + '_>, ServiceError>>
where
    R: ProcessRunner,
{
    PlatformService::current().map(|platform| platform.supervisor(runner))
}
