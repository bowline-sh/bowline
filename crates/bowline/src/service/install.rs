use super::*;
use std::{
    fs, io, thread,
    time::{Duration, Instant},
};

const DAEMON_TAKEOVER_STABLE_ABSENCE: Duration = Duration::from_secs(1);
const DAEMON_TAKEOVER_PROBE_INTERVAL: Duration = Duration::from_millis(50);

pub(crate) fn daemon_service_install(socket: &Path) -> Result<DaemonServiceOutcome, String> {
    let supervisor = platform_supervisor()?;
    let config = daemon_service_config(socket)?;
    let service_was_active = daemon_service_was_active()?;
    let previous_definition = required_previous_active_service_definition(
        service_was_active,
        &supervisor.definition_path(),
        supervisor.missing_definition_error(),
    )?;
    // The registration has to be read before the takeover stops the service;
    // afterwards the supervisor no longer reports what it used to be.
    let previous_registration = service_was_active
        .then(|| supervisor.registration())
        .transpose()
        .map_err(|error| error.to_string())?
        .flatten();
    install_daemon_service_with_takeover(
        socket,
        service_was_active,
        || {
            supervisor
                .stop()
                .map(|_| ())
                .map_err(|error| error.to_string())
        },
        || {
            supervisor
                .install_or_update(&config)
                .map(DaemonServiceOutcome::from)
                .map_err(|error| error.to_string())
        },
        || {
            let Some(definition) = previous_definition.as_deref() else {
                return Err("the previously active service definition is unavailable".to_string());
            };
            supervisor
                .restore(definition, previous_registration)
                .map(|_| ())
                .map_err(|error| error.to_string())
        },
    )
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
    match status.state {
        ServiceSupervisorState::Active => Ok(true),
        // Nothing is running under the supervisor, so there is no owner to
        // preserve. `uninstalled` is as final as `inactive` here.
        ServiceSupervisorState::Inactive | ServiceSupervisorState::Uninstalled => Ok(false),
        // Installed/Restarted are mutation outcomes, never a queried state; an
        // unavailable or unmodelled label means the owner is uncertain.
        state @ (ServiceSupervisorState::Installed
        | ServiceSupervisorState::Restarted
        | ServiceSupervisorState::Unavailable
        | ServiceSupervisorState::Unsupported
        | ServiceSupervisorState::Unrecognized(_)) => {
            Err(status.unavailable_because.unwrap_or_else(|| {
                format!("daemon service state is {state}; refusing to replace an uncertain owner")
            }))
        }
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
        // A refused connection is one flavour of "no daemon is running", so this
        // arm must precede the broader absence check: it is the only one that
        // reclaims the socket file the dead daemon left behind.
        Err(error) if error.io_kind() == Some(io::ErrorKind::ConnectionRefused) => {
            remove_stale_daemon_socket_after_connect_error(socket, &error).map_err(
                |remove_error| {
                    format!("could not remove the refused stale daemon socket: {remove_error}")
                },
            )?;
            require_socket_path_absent(socket)?;
        }
        Err(error) if error.daemon_is_absent() => {
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
    wait_for_stable_socket_absence_with_probe(socket, stable_for, probe_interval, || {})
}

fn wait_for_stable_socket_absence_with_probe<F>(
    socket: &Path,
    stable_for: Duration,
    probe_interval: Duration,
    mut after_absence_probe: F,
) -> Result<(), String>
where
    F: FnMut(),
{
    let absent_since = Instant::now();
    loop {
        require_socket_path_absent(socket)?;
        after_absence_probe();
        if absent_since.elapsed() >= stable_for {
            return Ok(());
        }
        thread::sleep(probe_interval);
    }
}

#[cfg(test)]
pub(crate) fn wait_for_stable_socket_absence_with_test_probe<F>(
    socket: &Path,
    stable_for: Duration,
    probe_interval: Duration,
    after_absence_probe: F,
) -> Result<(), String>
where
    F: FnMut(),
{
    wait_for_stable_socket_absence_with_probe(
        socket,
        stable_for,
        probe_interval,
        after_absence_probe,
    )
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
