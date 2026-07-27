//! Keeps the supervisor-facing daemon logs inside their size cap.
//!
//! The cap has to be enforced from inside the daemon, and not only because no
//! other Bowline process is guaranteed to be running on an agent host: rotation
//! re-points this process's own stdout and stderr at the file it creates, which
//! is what makes the cutoff atomic. See `bowline_local::daemon_logs`.

use std::{io, path::PathBuf, thread, time::Duration};

use bowline_local::daemon_logs::{LogRotationPolicy, enforce_daemon_log_caps};

/// Between checks the live file can exceed the cap by whatever the daemon wrote
/// in one interval. A minute keeps that overshoot small without waking a
/// mostly-idle agent host for nothing.
const LOG_CAP_CHECK_INTERVAL: Duration = Duration::from_secs(60);

/// The enforcement loop, boxed so the spawn itself is a seam: the failure worth
/// testing here is the one where no thread starts at all.
type SupervisorLoop = Box<dyn FnOnce() + Send + 'static>;

/// Starts the background enforcement loop for one state root. The thread is
/// detached: it owns no daemon state, and a daemon that is shutting down has no
/// reason to wait a minute for a log check to come around.
///
/// A thread that never starts is reported rather than dropped. Nothing else
/// enforces the cap, so the only other symptom is a log file that grows for the
/// life of the process — on a long-lived agent host, exactly what this module
/// exists to prevent — with no explanation anywhere.
pub(super) fn spawn_log_cap_supervisor(state_root: PathBuf) {
    if let Err(error) = start_log_cap_supervisor(state_root, spawn_detached) {
        eprintln!(
            "bowline-daemon could not start its log size cap supervisor, so log caps will not be enforced: {error}"
        );
    }
}

fn start_log_cap_supervisor(
    state_root: PathBuf,
    spawn: impl FnOnce(SupervisorLoop) -> io::Result<()>,
) -> io::Result<()> {
    spawn(Box::new(move || {
        loop {
            enforce_once(&state_root);
            thread::sleep(LOG_CAP_CHECK_INTERVAL);
        }
    }))
}

fn spawn_detached(supervise: SupervisorLoop) -> io::Result<()> {
    thread::Builder::new()
        .name("bowline-log-cap".to_string())
        .spawn(supervise)
        .map(|_detached| ())
}

/// One enforcement pass. A failure is reported and the loop continues: an
/// unrotatable log is a disk-space problem, not a reason to stop syncing, and
/// swallowing it silently would leave the growth unexplained.
fn enforce_once(state_root: &std::path::Path) {
    if let Err(error) = enforce_daemon_log_caps(state_root, LogRotationPolicy::DEFAULT) {
        eprintln!("bowline-daemon could not enforce its log size cap: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// A supervisor that never starts is the one failure this module used to
    /// swallow, and the one nothing else can compensate for.
    #[test]
    fn a_supervisor_that_cannot_start_reports_the_failure() {
        let error = start_log_cap_supervisor(PathBuf::from("/nonexistent"), |_supervise| {
            Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                "thread limit reached",
            ))
        })
        .expect_err("a spawn failure is not a silent success");

        assert_eq!(error.kind(), io::ErrorKind::OutOfMemory);
    }

    #[test]
    fn the_detached_spawn_runs_the_supervisor_loop() {
        let (started, was_started) = mpsc::channel();

        spawn_detached(Box::new(move || {
            started.send(()).expect("supervisor test channel is open");
        }))
        .expect("a supervisor thread starts");

        was_started
            .recv_timeout(Duration::from_secs(5))
            .expect("the spawned loop runs");
    }
}
