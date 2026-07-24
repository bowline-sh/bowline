use super::*;

use std::time::{Duration, Instant};

use bowline_core::introspection::WorkspaceReadiness;
use serde::Serialize;

use crate::wire::await_daemon_sync_barrier;

/// Default wait budget when `--timeout` is omitted.
pub(super) const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// Device approval is account state, not filesystem convergence. Refresh it
/// only when the requested rung is below Ready, or once for timeout diagnostics.
/// A successful exact daemon barrier proves that the daemon established its
/// authenticated, trusted hosted context and verified the encrypted head.
const AUTH_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

/// A daemon can be between supervisor start and socket bind while a device is
/// already authenticated. Retry that transport boundary without reintroducing
/// sync-state polling; convergence itself remains one reactive barrier call.
const DAEMON_RECONNECT_INTERVAL: Duration = Duration::from_millis(250);

/// Upper bound on `--timeout` so a typo cannot wedge a harness for hours.
const MAX_TIMEOUT: Duration = Duration::from_secs(3600);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TimeoutParseError {
    Empty,
    NotANumber,
    Zero,
    TooLarge,
    UnknownUnit,
}

impl TimeoutParseError {
    fn message(self) -> String {
        match self {
            Self::Empty => "--timeout requires a value, e.g. 120s or 2m".to_string(),
            Self::NotANumber => {
                "--timeout must be a number optionally suffixed with s, m, or h".to_string()
            }
            Self::Zero => "--timeout must be greater than zero".to_string(),
            Self::TooLarge => format!(
                "--timeout must be at most {} seconds",
                MAX_TIMEOUT.as_secs()
            ),
            Self::UnknownUnit => {
                "--timeout unit must be s (seconds), m (minutes), or h (hours)".to_string()
            }
        }
    }
}

/// Parse a human duration like `120s`, `2m`, `1h`, or a bare seconds count.
pub(super) fn parse_timeout(raw: &str) -> Result<Duration, String> {
    parse_timeout_inner(raw).map_err(TimeoutParseError::message)
}

fn parse_timeout_inner(raw: &str) -> Result<Duration, TimeoutParseError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(TimeoutParseError::Empty);
    }
    let (digits, unit_seconds) = match trimmed.chars().last() {
        Some('s') => (&trimmed[..trimmed.len() - 1], 1),
        Some('m') => (&trimmed[..trimmed.len() - 1], 60),
        Some('h') => (&trimmed[..trimmed.len() - 1], 3600),
        Some(ch) if ch.is_ascii_digit() => (trimmed, 1),
        Some(_) => return Err(TimeoutParseError::UnknownUnit),
        None => return Err(TimeoutParseError::Empty),
    };
    let value: u64 = digits.parse().map_err(|_| TimeoutParseError::NotANumber)?;
    if value == 0 {
        return Err(TimeoutParseError::Zero);
    }
    let seconds = value
        .checked_mul(unit_seconds)
        .ok_or(TimeoutParseError::TooLarge)?;
    let duration = Duration::from_secs(seconds);
    if duration > MAX_TIMEOUT {
        return Err(TimeoutParseError::TooLarge);
    }
    Ok(duration)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SyncWaitError {
    code: &'static str,
    message: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SyncWaitOutput {
    contract_version: u16,
    generated_at: String,
    workspace_id: String,
    requested_state: WorkspaceReadiness,
    observed_state: WorkspaceReadiness,
    reached: bool,
    timed_out: bool,
    waited_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    convergence_revision: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<SyncWaitError>,
}

pub(super) fn print_sync_wait(args: SyncWaitArgs, json: bool, socket: &Path) -> ExitCode {
    let started = Instant::now();
    let deadline = started + args.timeout;
    if args.target_state != WorkspaceReadiness::Ready {
        return wait_for_authentication_state(&args, json, started, deadline);
    }
    let workspace_id = WorkspaceId::new(args.workspace_id.clone());
    let (observed, timed_out) = await_ready(
        deadline,
        |remaining| await_daemon_sync_barrier(socket, &workspace_id, remaining),
        || observe_authentication_readiness(&args),
        thread::sleep,
    );
    emit(&args, observed, started.elapsed(), timed_out, json)
}

fn await_ready(
    deadline: Instant,
    mut barrier: impl FnMut(Duration) -> io::Result<u64>,
    mut timeout_observation: impl FnMut() -> ReadinessObservation,
    mut sleep: impl FnMut(Duration),
) -> (ReadinessObservation, bool) {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return (timeout_observation(), true);
        }
        match barrier(remaining) {
            Ok(revision) => {
                return (
                    ReadinessObservation {
                        state: WorkspaceReadiness::Ready,
                        convergence_revision: Some(revision),
                    },
                    false,
                );
            }
            Err(_) if Instant::now() >= deadline => {
                return (timeout_observation(), true);
            }
            Err(_) => sleep(
                DAEMON_RECONNECT_INTERVAL.min(deadline.saturating_duration_since(Instant::now())),
            ),
        }
    }
}

fn wait_for_authentication_state(
    args: &SyncWaitArgs,
    json: bool,
    started: Instant,
    deadline: Instant,
) -> ExitCode {
    loop {
        let observed = observe_authentication_readiness(args);
        if observed.state.satisfies(args.target_state) {
            return emit(args, observed, started.elapsed(), false, json);
        }
        if Instant::now() >= deadline {
            return emit(args, observed, started.elapsed(), true, json);
        }
        thread::sleep(
            AUTH_REFRESH_INTERVAL.min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

fn observe_authentication_readiness(args: &SyncWaitArgs) -> ReadinessObservation {
    let workspace_id = WorkspaceId::new(args.workspace_id.clone());
    let authenticated = crate::status_commands::account_authenticated();
    let trust = crate::status_commands::fetch_device_trust(workspace_id.as_str());
    let auth = crate::status_commands::authentication_state(&workspace_id, &trust, authenticated);
    ReadinessObservation {
        state: WorkspaceReadiness::derive(auth, false),
        convergence_revision: None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReadinessObservation {
    state: WorkspaceReadiness,
    convergence_revision: Option<u64>,
}

fn emit(
    args: &SyncWaitArgs,
    observed: ReadinessObservation,
    waited: Duration,
    timed_out: bool,
    json: bool,
) -> ExitCode {
    let reached = !timed_out;
    let error = timed_out.then(|| SyncWaitError {
        code: "timeout",
        message: format!(
            "workspace {} reached {} within {}s, not the requested {}",
            args.workspace_id,
            observed.state.token(),
            args.timeout.as_secs(),
            args.target_state.token()
        ),
    });
    let output = SyncWaitOutput {
        contract_version: CONTRACT_VERSION,
        generated_at: generated_at(),
        workspace_id: args.workspace_id.clone(),
        requested_state: args.target_state,
        observed_state: observed.state,
        reached,
        timed_out,
        waited_ms: waited.as_millis().min(u128::from(u64::MAX)) as u64,
        convergence_revision: observed.convergence_revision,
        error,
    };
    if json {
        print_json(&output);
    } else if timed_out {
        eprintln!(
            "bowline sync wait: timed out; workspace {} is {} (wanted {})",
            args.workspace_id,
            observed.state.token(),
            args.target_state.token()
        );
    } else {
        println!(
            "bowline sync wait: workspace {} is {}",
            args.workspace_id,
            observed.state.token()
        );
    }
    if timed_out {
        ExitCode::from(EXIT_RUNTIME)
    } else {
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_seconds() {
        assert_eq!(parse_timeout("120").unwrap(), Duration::from_secs(120));
    }

    #[test]
    fn parses_seconds_suffix() {
        assert_eq!(parse_timeout("90s").unwrap(), Duration::from_secs(90));
    }

    #[test]
    fn parses_minutes_and_hours() {
        assert_eq!(parse_timeout("2m").unwrap(), Duration::from_secs(120));
        assert_eq!(parse_timeout("1h").unwrap(), Duration::from_secs(3600));
    }

    #[test]
    fn rejects_zero() {
        assert_eq!(parse_timeout_inner("0s"), Err(TimeoutParseError::Zero));
    }

    #[test]
    fn rejects_non_numeric() {
        assert_eq!(
            parse_timeout_inner("10.5s"),
            Err(TimeoutParseError::NotANumber)
        );
    }

    #[test]
    fn rejects_unknown_unit() {
        assert_eq!(
            parse_timeout_inner("5d"),
            Err(TimeoutParseError::UnknownUnit)
        );
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(parse_timeout_inner("   "), Err(TimeoutParseError::Empty));
    }

    #[test]
    fn accepts_the_maximum_and_rejects_beyond_it() {
        assert_eq!(parse_timeout_inner("1h"), Ok(MAX_TIMEOUT));
        assert_eq!(parse_timeout_inner("2h"), Err(TimeoutParseError::TooLarge));
        assert_eq!(
            parse_timeout_inner("9999h"),
            Err(TimeoutParseError::TooLarge)
        );
    }

    #[test]
    fn successful_ready_barrier_does_not_repeat_authentication_observation() {
        let mut observations = 0;
        let (observed, timed_out) = await_ready(
            Instant::now() + Duration::from_secs(1),
            |_| Ok(42),
            || {
                observations += 1;
                ReadinessObservation {
                    state: WorkspaceReadiness::ApprovalPending,
                    convergence_revision: None,
                }
            },
            |_| {},
        );

        assert!(!timed_out);
        assert_eq!(observations, 0);
        assert_eq!(observed.state, WorkspaceReadiness::Ready);
        assert_eq!(observed.convergence_revision, Some(42));
    }

    #[test]
    fn ready_wait_retries_daemon_boundary_without_polling_trust() {
        let mut attempts = 0;
        let mut observations = 0;
        let (observed, timed_out) = await_ready(
            Instant::now() + Duration::from_secs(1),
            |_| {
                attempts += 1;
                if attempts == 1 {
                    Err(io::Error::other("socket not bound yet"))
                } else {
                    Ok(7)
                }
            },
            || {
                observations += 1;
                ReadinessObservation {
                    state: WorkspaceReadiness::ApprovalPending,
                    convergence_revision: None,
                }
            },
            |_| {},
        );

        assert!(!timed_out);
        assert_eq!(attempts, 2);
        assert_eq!(observations, 0);
        assert_eq!(observed.convergence_revision, Some(7));
    }
}
