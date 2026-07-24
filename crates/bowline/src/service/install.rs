use super::*;
use std::{
    fs, io, thread,
    time::{Duration, Instant},
};

const DAEMON_TAKEOVER_STABLE_ABSENCE: Duration = Duration::from_secs(1);
const DAEMON_TAKEOVER_PROBE_INTERVAL: Duration = Duration::from_millis(50);

pub(crate) fn daemon_service_install(socket: &Path) -> Result<DaemonServiceOutcome, String> {
    if linux_service::current_platform_supported() {
        let options = daemon_linux_service_options(socket)?;
        let service_was_active = daemon_service_was_active()?;
        let previous_definition = required_previous_active_service_definition(
            service_was_active,
            &options.unit_dir.join(linux_service::SERVICE_NAME),
            "systemd service has no unit to restore; refusing to stop it",
        )?;
        let previous_enablement = service_was_active
            .then(|| linux_service::service_enablement(&SystemProcessRunner))
            .transpose()
            .map_err(|error| error.to_string())?;
        return install_daemon_service_with_takeover(
            socket,
            service_was_active,
            || {
                linux_service::stop_service(&SystemProcessRunner, &options.unit_dir)
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            },
            || {
                linux_service::install_or_update_service(&SystemProcessRunner, &options)
                    .map(DaemonServiceOutcome::from)
                    .map_err(|error| error.to_string())
            },
            || {
                let Some(definition) = previous_definition.as_deref() else {
                    return Err("previous systemd service is unavailable".to_string());
                };
                let Some(enablement) = previous_enablement else {
                    return Err("previous systemd enablement is unavailable".to_string());
                };
                linux_service::restore_service(
                    &SystemProcessRunner,
                    &options.unit_dir,
                    definition,
                    enablement,
                )
                .map(|_| ())
                .map_err(|error| error.to_string())
            },
        );
    }
    if macos_service::current_platform_supported() {
        let options = daemon_macos_service_options(socket)?;
        let service_was_active = daemon_service_was_active()?;
        let previous_definition = required_previous_active_service_definition(
            service_was_active,
            &options.launch_agents_dir.join(macos_service::PLIST_NAME),
            "launchd service has no plist to restore; refusing to unload it",
        )?;
        return install_daemon_service_with_takeover(
            socket,
            service_was_active,
            || {
                macos_service::stop_service(
                    &SystemProcessRunner,
                    &options.launch_agents_dir,
                    &options.launch_domain,
                )
                .map(|_| ())
                .map_err(|error| error.to_string())
            },
            || {
                macos_service::install_or_update_service(&SystemProcessRunner, &options)
                    .map(DaemonServiceOutcome::from)
                    .map_err(|error| error.to_string())
            },
            || {
                let Some(definition) = previous_definition.as_deref() else {
                    return Err("previous launch agent is unavailable".to_string());
                };
                macos_service::restore_service(
                    &SystemProcessRunner,
                    &options.launch_agents_dir,
                    &options.launch_domain,
                    definition,
                )
                .map(|_| ())
                .map_err(|error| error.to_string())
            },
        );
    }
    Err("daemon service commands are available only on Linux and macOS".to_string())
}

fn required_previous_active_service_definition(
    service_was_active: bool,
    path: &Path,
    missing_definition_error: &'static str,
) -> Result<Option<Vec<u8>>, String> {
    let definition = previous_active_service_definition(service_was_active, path)?;
    if service_was_active && definition.is_none() {
        return Err(missing_definition_error.to_string());
    }
    Ok(definition)
}

pub(crate) fn previous_active_service_definition(
    service_was_active: bool,
    path: &Path,
) -> Result<Option<Vec<u8>>, String> {
    if !service_was_active {
        return Ok(None);
    }
    match fs::read(path) {
        Ok(definition) => Ok(Some(definition)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!(
            "could not preserve active service definition: {error}"
        )),
    }
}

fn daemon_service_was_active() -> Result<bool, String> {
    daemon_service_active_from_status(daemon_service_status(&SystemProcessRunner))
}

pub(crate) fn daemon_service_active_from_status(
    status: Option<DaemonServiceStatus>,
) -> Result<bool, String> {
    let status =
        status.ok_or_else(|| "daemon service state is unavailable on this platform".to_string())?;
    match status.state.as_str() {
        "active" => Ok(true),
        "inactive" => Ok(false),
        state => Err(status.unavailable_because.unwrap_or_else(|| {
            format!("daemon service state is {state}; refusing to replace an uncertain owner")
        })),
    }
}

pub(crate) fn install_daemon_service_with_takeover<T>(
    socket: &Path,
    service_was_active: bool,
    stop_registered_service: impl FnOnce() -> Result<(), String>,
    install_registered_service: impl FnOnce() -> Result<T, String>,
    mut restart_registered_service: impl FnMut() -> Result<(), String>,
) -> Result<T, String> {
    if service_was_active && let Err(error) = stop_registered_service() {
        return Err(restore_active_service_after_failure(
            error,
            true,
            &mut restart_registered_service,
        ));
    }
    if let Err(error) = stop_unmanaged_daemon(socket) {
        return Err(restore_active_service_after_failure(
            error,
            service_was_active,
            &mut restart_registered_service,
        ));
    }
    match install_registered_service() {
        Ok(outcome) => Ok(outcome),
        Err(error) => Err(restore_active_service_after_failure(
            error,
            service_was_active,
            &mut restart_registered_service,
        )),
    }
}

fn restore_active_service_after_failure(
    primary_error: String,
    service_was_active: bool,
    restart_registered_service: &mut impl FnMut() -> Result<(), String>,
) -> String {
    if !service_was_active {
        return primary_error;
    }
    match restart_registered_service() {
        Ok(()) => primary_error,
        Err(restore_error) => format!(
            "{primary_error}; could not restore the previously active daemon service: {restore_error}"
        ),
    }
}

pub(crate) fn stop_unmanaged_daemon(socket: &Path) -> Result<(), String> {
    match request_shutdown(socket) {
        Ok(()) => {
            if !wait_for_daemon_socket_to_stop(socket, Duration::from_secs(3)) {
                return Err("existing unmanaged daemon did not stop within 3 seconds".to_string());
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            require_socket_path_absent(socket)?;
        }
        Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
            remove_stale_daemon_socket_after_connect_error(socket, &error).map_err(
                |remove_error| {
                    format!("could not remove the refused stale daemon socket: {remove_error}")
                },
            )?;
            require_socket_path_absent(socket)?;
        }
        Err(error) => {
            return Err(format!(
                "could not stop the existing unmanaged daemon: {error}"
            ));
        }
    }
    wait_for_stable_socket_absence(
        socket,
        DAEMON_TAKEOVER_STABLE_ABSENCE,
        DAEMON_TAKEOVER_PROBE_INTERVAL,
    )
}

fn require_socket_path_absent(socket: &Path) -> Result<(), String> {
    match fs::symlink_metadata(socket) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err("daemon socket path remains and cannot be safely replaced".to_string()),
        Err(error) => Err(format!("could not inspect the daemon socket path: {error}")),
    }
}

pub(crate) fn wait_for_stable_socket_absence(
    socket: &Path,
    stable_for: Duration,
    probe_interval: Duration,
) -> Result<(), String> {
    let absent_since = Instant::now();
    loop {
        require_socket_path_absent(socket)?;
        if absent_since.elapsed() >= stable_for {
            return Ok(());
        }
        thread::sleep(probe_interval);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_launchd_service_without_plist_blocks_before_unload() {
        let path = std::env::temp_dir().join(format!(
            "bowline-missing-launch-agent-{}.plist",
            std::process::id()
        ));
        assert!(!path.exists());

        let error = required_previous_active_service_definition(
            true,
            &path,
            "launchd service has no plist to restore; refusing to unload it",
        )
        .expect_err("launchd needs rollback bytes before bootout");

        assert!(error.contains("refusing to unload"));
    }

    #[test]
    fn active_systemd_service_without_unit_blocks_before_stop() {
        let path = std::env::temp_dir().join(format!(
            "bowline-missing-systemd-unit-{}.service",
            std::process::id()
        ));
        assert!(!path.exists());

        let error = required_previous_active_service_definition(
            true,
            &path,
            "systemd service has no unit to restore; refusing to stop it",
        )
        .expect_err("systemd needs rollback bytes before stop");

        assert!(error.contains("refusing to stop"));
    }
}
