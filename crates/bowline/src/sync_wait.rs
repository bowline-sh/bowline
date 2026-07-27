use super::*;

use std::time::{Duration, Instant};

use bowline_core::introspection::WorkspaceReadiness;
use bowline_daemon_rpc::RetryDisposition;
use serde::Serialize;

use crate::wire::{DaemonRpcError, await_daemon_sync_barrier};

/// Default wait budget when `--timeout` is omitted.
pub(super) const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// Device approval is account state, not filesystem convergence. Refresh it
/// only when the requested rung is below Ready, or once for timeout diagnostics.
/// A successful exact daemon barrier proves that the daemon established its
/// authenticated, trusted hosted context and verified the encrypted head.
const AUTH_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

/// A daemon can be between supervisor start and engine attach while a device is
/// already authenticated. Retry that startup boundary without reintroducing
/// sync-state polling; convergence itself remains one reactive barrier call.
const DAEMON_STARTUP_RETRY_INTERVAL: Duration = Duration::from_millis(250);

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

/// Why the readiness barrier did not answer. Reporting every failure as
/// `timeout` told a harness on a daemon-less host that its workspace was
/// `approval-pending` after burning the whole budget — false and unactionable.
///
/// Each variant belongs to exactly one `RetryDisposition`, so `is_transient`
/// restates the classification rather than re-deciding it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BarrierFailure {
    /// Nothing answered the control socket.
    DaemonUnreachable,
    /// A daemon answered; the subsystem that serves barriers has not attached
    /// yet. This is the ordinary condition for the seconds after a restart.
    EngineStarting,
    /// A running daemon speaks a wire generation this build cannot use.
    DaemonVersionSkew,
    /// The daemon answered and refused the call on its merits.
    DaemonRejected,
    /// A live engine consumed the barrier's deadline without converging.
    BarrierTimedOut,
}

impl BarrierFailure {
    fn classify(error: &DaemonRpcError) -> Self {
        match error.retry_disposition() {
            RetryDisposition::DeadlineElapsed => Self::BarrierTimedOut,
            RetryDisposition::RetryWhileStarting => {
                if error.daemon_answered() {
                    Self::EngineStarting
                } else {
                    Self::DaemonUnreachable
                }
            }
            RetryDisposition::Terminal => match error.version_skew() {
                Some(_) => Self::DaemonVersionSkew,
                None => Self::DaemonRejected,
            },
        }
    }

    fn code(self) -> &'static str {
        match self {
            Self::DaemonUnreachable => "daemon_unreachable",
            Self::EngineStarting => "sync_engine_unavailable",
            Self::DaemonVersionSkew => "daemon_version_skew",
            Self::DaemonRejected => "daemon_rejected_barrier",
            Self::BarrierTimedOut => "timeout",
        }
    }

    fn next_action(self) -> Option<&'static str> {
        match self {
            Self::DaemonUnreachable => Some("bowline daemon start"),
            Self::EngineStarting => Some("bowline daemon status --json"),
            Self::DaemonVersionSkew => Some("bowline daemon restart"),
            Self::DaemonRejected => Some("bowline doctor --json"),
            Self::BarrierTimedOut => None,
        }
    }

    /// A daemon still starting — no socket yet, or a socket whose sync engine has
    /// not attached — is exactly what a wait budget exists for. A daemon that
    /// answered and refused the call will refuse it again.
    fn is_transient(self) -> bool {
        match self {
            Self::DaemonUnreachable | Self::EngineStarting | Self::BarrierTimedOut => true,
            Self::DaemonVersionSkew | Self::DaemonRejected => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BarrierError {
    failure: BarrierFailure,
    message: String,
}

impl BarrierError {
    fn new(error: &DaemonRpcError) -> Self {
        Self {
            failure: BarrierFailure::classify(error),
            message: error.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WaitOutcome {
    Reached,
    /// The budget was spent without reaching the requested rung. The retained
    /// cause names what the wait was still waiting on; `None` means a live
    /// engine simply never converged.
    TimedOut(Option<BarrierError>),
    /// The daemon answered definitively. Waiting longer cannot change it, so the
    /// wait returns before its deadline.
    Failed(BarrierError),
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SyncWaitError {
    code: &'static str,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_action: Option<&'static str>,
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
    let (observed, outcome) = await_ready(
        deadline,
        |remaining| await_daemon_sync_barrier(socket, &workspace_id, remaining),
        || observe_authentication_readiness(&args),
        thread::sleep,
    );
    emit(&args, observed, started.elapsed(), outcome, json)
}

fn await_ready(
    deadline: Instant,
    mut barrier: impl FnMut(Duration) -> Result<u64, DaemonRpcError>,
    mut timeout_observation: impl FnMut() -> ReadinessObservation,
    mut sleep: impl FnMut(Duration),
) -> (ReadinessObservation, WaitOutcome) {
    let mut last_error: Option<BarrierError> = None;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return (timeout_observation(), timed_out_outcome(last_error));
        }
        match barrier(remaining) {
            Ok(revision) => {
                return (
                    ReadinessObservation {
                        state: WorkspaceReadiness::Ready,
                        convergence_revision: Some(revision),
                    },
                    WaitOutcome::Reached,
                );
            }
            Err(error) => {
                let error = BarrierError::new(&error);
                if !error.failure.is_transient() {
                    return (timeout_observation(), WaitOutcome::Failed(error));
                }
                if Instant::now() >= deadline {
                    return (timeout_observation(), timed_out_outcome(Some(error)));
                }
                last_error = Some(error);
                sleep(
                    DAEMON_STARTUP_RETRY_INTERVAL
                        .min(deadline.saturating_duration_since(Instant::now())),
                );
            }
        }
    }
}

/// A budget spent entirely on a daemon that never came up is a daemon problem,
/// not a convergence problem, so the retained cause is reported alongside the
/// timeout. A retained barrier deadline adds nothing the timeout does not say.
fn timed_out_outcome(last_error: Option<BarrierError>) -> WaitOutcome {
    WaitOutcome::TimedOut(
        last_error.filter(|error| error.failure != BarrierFailure::BarrierTimedOut),
    )
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
            return emit(
                args,
                observed,
                started.elapsed(),
                WaitOutcome::Reached,
                json,
            );
        }
        if Instant::now() >= deadline {
            return emit(
                args,
                observed,
                started.elapsed(),
                WaitOutcome::TimedOut(None),
                json,
            );
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

/// Render a spent budget. The last observed rung is always named; when the wait
/// was still waiting on the daemon rather than on convergence, the cause carries
/// its own code and remediation so the report stays actionable.
fn timeout_error(
    args: &SyncWaitArgs,
    observed: ReadinessObservation,
    cause: Option<&BarrierError>,
) -> SyncWaitError {
    let budget = format!(
        "workspace {} reached {} within {}s, not the requested {}",
        args.workspace_id,
        observed.state.token(),
        args.timeout.as_secs(),
        args.target_state.token()
    );
    match cause {
        None => SyncWaitError {
            code: "timeout",
            message: budget,
            next_action: None,
        },
        Some(cause) => SyncWaitError {
            code: cause.failure.code(),
            message: format!("{budget}: {}", cause.message),
            next_action: cause.failure.next_action(),
        },
    }
}

fn sync_wait_output(
    args: &SyncWaitArgs,
    observed: ReadinessObservation,
    waited: Duration,
    outcome: WaitOutcome,
) -> SyncWaitOutput {
    let error = match &outcome {
        WaitOutcome::Reached => None,
        WaitOutcome::TimedOut(cause) => Some(timeout_error(args, observed, cause.as_ref())),
        WaitOutcome::Failed(failed) => Some(SyncWaitError {
            code: failed.failure.code(),
            message: format!(
                "workspace {} could not be observed: {}",
                args.workspace_id, failed.message
            ),
            next_action: failed.failure.next_action(),
        }),
    };
    SyncWaitOutput {
        contract_version: CONTRACT_VERSION,
        generated_at: generated_at(),
        workspace_id: args.workspace_id.clone(),
        requested_state: args.target_state,
        observed_state: observed.state,
        reached: outcome == WaitOutcome::Reached,
        // `timedOut` means exactly one thing: the budget ran out before the
        // requested rung was reached. A wait that returns early because the
        // daemon refused the call is not a timeout and must never claim to be.
        timed_out: matches!(outcome, WaitOutcome::TimedOut(_)),
        waited_ms: waited.as_millis().min(u128::from(u64::MAX)) as u64,
        convergence_revision: observed.convergence_revision,
        error,
    }
}

fn emit(
    args: &SyncWaitArgs,
    observed: ReadinessObservation,
    waited: Duration,
    outcome: WaitOutcome,
    json: bool,
) -> ExitCode {
    let output = sync_wait_output(args, observed, waited, outcome);
    let reached = output.reached;
    if json {
        print_json(&output);
    } else if let Some(error) = &output.error {
        eprintln!("bowline sync wait: {}", error.message);
        if let Some(next_action) = error.next_action {
            eprintln!("  Next  {next_action}");
        }
    } else {
        println!(
            "bowline sync wait: workspace {} is {}",
            args.workspace_id,
            observed.state.token()
        );
    }
    if reached {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(EXIT_RUNTIME)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use bowline_core::wire::generated::DaemonRpcErrorCode;

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

    fn pending_observation() -> ReadinessObservation {
        ReadinessObservation {
            state: WorkspaceReadiness::ApprovalPending,
            convergence_revision: None,
        }
    }

    fn transport_error(kind: io::ErrorKind) -> DaemonRpcError {
        DaemonRpcError::from(bowline_daemon_rpc::ClientError::Io {
            operation: "connect",
            source: io::Error::from(kind),
        })
    }

    fn daemon_error(code: DaemonRpcErrorCode, message: &str) -> DaemonRpcError {
        DaemonRpcError::from(bowline_daemon_rpc::ClientError::Remote(Box::new(
            bowline_core::wire::generated::DaemonRpcError {
                code,
                message: message.to_string(),
                retryable: true,
                retry_after_ms: None,
                operation_id: None,
                required_client_version: None,
                details: None,
            },
        )))
    }

    fn wait_args(timeout: Duration) -> SyncWaitArgs {
        SyncWaitArgs {
            workspace_id: "ws_code_test".to_string(),
            target_state: WorkspaceReadiness::Ready,
            timeout,
        }
    }

    #[test]
    fn successful_ready_barrier_does_not_repeat_authentication_observation() {
        let mut observations = 0;
        let (observed, outcome) = await_ready(
            Instant::now() + Duration::from_secs(1),
            |_| Ok(42),
            || {
                observations += 1;
                pending_observation()
            },
            |_| {},
        );

        assert_eq!(outcome, WaitOutcome::Reached);
        assert_eq!(observations, 0);
        assert_eq!(observed.state, WorkspaceReadiness::Ready);
        assert_eq!(observed.convergence_revision, Some(42));
    }

    #[test]
    fn ready_wait_retries_daemon_boundary_without_polling_trust() {
        let mut attempts = 0;
        let mut observations = 0;
        let (observed, outcome) = await_ready(
            Instant::now() + Duration::from_secs(1),
            |_| {
                attempts += 1;
                if attempts == 1 {
                    Err(transport_error(io::ErrorKind::NotFound))
                } else {
                    Ok(7)
                }
            },
            || {
                observations += 1;
                pending_observation()
            },
            |_| {},
        );

        assert_eq!(outcome, WaitOutcome::Reached);
        assert_eq!(attempts, 2);
        assert_eq!(observations, 0);
        assert_eq!(observed.convergence_revision, Some(7));
    }

    /// The release blocker: `daemon install` binds the control socket before the
    /// manifest engine attaches, so the first barrier after a restart answers
    /// `unavailable`. That is the ordinary early condition a `--timeout` exists
    /// to cover, not a reason to abandon the wait after one second.
    #[test]
    fn a_starting_sync_engine_is_waited_through_rather_than_abandoned() {
        let mut attempts = 0;
        let mut slept = Duration::ZERO;
        let (observed, outcome) = await_ready(
            Instant::now() + Duration::from_secs(120),
            |_| {
                attempts += 1;
                if attempts < 4 {
                    Err(daemon_error(
                        DaemonRpcErrorCode::Unavailable,
                        "manifest sync engine is unavailable",
                    ))
                } else {
                    Ok(19)
                }
            },
            || panic!("a wait that can still converge must not fall back to trust polling"),
            |interval| slept += interval,
        );

        assert_eq!(outcome, WaitOutcome::Reached);
        assert_eq!(attempts, 4);
        assert_eq!(observed.state, WorkspaceReadiness::Ready);
        assert_eq!(observed.convergence_revision, Some(19));
        // Three retries at the startup interval: polled, never spun.
        assert_eq!(slept, DAEMON_STARTUP_RETRY_INTERVAL * 3);
    }

    /// An engine that never attaches inside the budget is a timeout, and the
    /// report must say both that the budget was spent and what it was spent on.
    #[test]
    fn an_engine_that_never_attaches_times_out_truthfully() {
        let (observed, outcome) = await_ready(
            Instant::now() + Duration::from_millis(1),
            |_| {
                Err(daemon_error(
                    DaemonRpcErrorCode::Unavailable,
                    "manifest sync engine is unavailable",
                ))
            },
            pending_observation,
            |_| {},
        );

        let output = sync_wait_output(
            &wait_args(Duration::from_secs(120)),
            observed,
            Duration::from_secs(120),
            outcome,
        );
        assert!(output.timed_out);
        assert!(!output.reached);
        let error = output.error.expect("a spent budget reports why");
        assert_eq!(error.code, "sync_engine_unavailable");
        assert!(error.message.contains("approval-pending"));
        assert!(
            error
                .message
                .contains("manifest sync engine is unavailable")
        );
        assert_eq!(error.next_action, Some("bowline daemon status --json"));
    }

    /// A host where the daemon was never installed must be told that, not told
    /// its workspace is approval-pending after burning the whole budget.
    #[test]
    fn missing_daemon_socket_reports_daemon_unreachable_not_bare_timeout() {
        let (observed, outcome) = await_ready(
            Instant::now() + Duration::from_millis(1),
            |_| Err(transport_error(io::ErrorKind::ConnectionRefused)),
            pending_observation,
            |_| {},
        );

        let WaitOutcome::TimedOut(Some(error)) = &outcome else {
            panic!("a budget spent on an absent daemon retains its cause");
        };
        assert_eq!(error.failure, BarrierFailure::DaemonUnreachable);

        let output = sync_wait_output(
            &wait_args(Duration::from_secs(1)),
            observed,
            Duration::from_secs(1),
            outcome,
        );
        assert!(output.timed_out);
        let error = output.error.expect("a spent budget reports why");
        assert_eq!(error.code, "daemon_unreachable");
        assert_eq!(error.next_action, Some("bowline daemon start"));
    }

    /// A daemon that answers and refuses the call will refuse it again, so the
    /// wait must not spend the rest of the budget on it — and must not claim to
    /// have timed out when it did not.
    #[test]
    fn a_refused_barrier_fails_fast_without_consuming_the_budget() {
        let mut attempts = 0;
        let (observed, outcome) = await_ready(
            Instant::now() + Duration::from_secs(60),
            |_| {
                attempts += 1;
                Err(daemon_error(
                    DaemonRpcErrorCode::InvalidRequest,
                    "daemon is serving a different workspace",
                ))
            },
            pending_observation,
            |_| panic!("a terminal barrier failure must not sleep"),
        );

        assert_eq!(attempts, 1);
        let WaitOutcome::Failed(error) = &outcome else {
            panic!("a refused barrier must not be reported as a timeout");
        };
        assert_eq!(error.failure, BarrierFailure::DaemonRejected);

        let output = sync_wait_output(
            &wait_args(Duration::from_secs(120)),
            observed,
            Duration::from_millis(20),
            outcome,
        );
        assert!(!output.timed_out);
        assert!(!output.reached);
        let error = output.error.expect("a refusal reports why");
        assert_eq!(error.code, "daemon_rejected_barrier");
        assert_eq!(error.next_action, Some("bowline doctor --json"));
    }

    /// A daemon whose wire generation this build cannot use is terminal too, and
    /// says so with the one remediation that helps.
    #[test]
    fn a_skewed_daemon_fails_fast_with_its_own_remediation() {
        let (_, outcome) = await_ready(
            Instant::now() + Duration::from_secs(60),
            |_| {
                Err(DaemonRpcError::from(
                    bowline_daemon_rpc::ClientError::IncompatibleVersion {
                        dimension: bowline_daemon_rpc::VersionDimension::MachineContract,
                        peer_version: 6,
                        window: bowline_daemon_rpc::VersionWindow {
                            minimum: 8,
                            supported: 8,
                        },
                    },
                ))
            },
            pending_observation,
            |_| panic!("a terminal barrier failure must not sleep"),
        );

        let WaitOutcome::Failed(error) = outcome else {
            panic!("version skew must not be reported as a timeout");
        };
        assert_eq!(error.failure, BarrierFailure::DaemonVersionSkew);
        assert_eq!(error.failure.next_action(), Some("bowline daemon restart"));
    }

    #[test]
    fn a_live_daemon_that_never_converges_still_reports_timeout() {
        let (observed, outcome) = await_ready(
            Instant::now() + Duration::from_millis(1),
            |_| Err(transport_error(io::ErrorKind::TimedOut)),
            pending_observation,
            |_| {},
        );

        assert_eq!(outcome, WaitOutcome::TimedOut(None));

        let output = sync_wait_output(
            &wait_args(Duration::from_secs(120)),
            observed,
            Duration::from_secs(120),
            outcome,
        );
        assert!(output.timed_out);
        let error = output.error.expect("a spent budget reports why");
        assert_eq!(error.code, "timeout");
        assert_eq!(output.observed_state, WorkspaceReadiness::ApprovalPending);
    }

    /// The contract a harness reads: only a spent budget carries the timeout
    /// code, and only a spent budget sets the flag. Every failure this command
    /// can produce is checked, so a new variant cannot reintroduce the split.
    #[test]
    fn the_timeout_flag_and_the_timeout_code_never_disagree() {
        let failures = [
            BarrierFailure::DaemonUnreachable,
            BarrierFailure::EngineStarting,
            BarrierFailure::DaemonVersionSkew,
            BarrierFailure::DaemonRejected,
            BarrierFailure::BarrierTimedOut,
        ];
        for failure in failures {
            let error = BarrierError {
                failure,
                message: "cause".to_string(),
            };
            let outcome = if failure.is_transient() {
                WaitOutcome::TimedOut(Some(error))
            } else {
                WaitOutcome::Failed(error)
            };
            let output = sync_wait_output(
                &wait_args(Duration::from_secs(120)),
                pending_observation(),
                Duration::from_secs(1),
                outcome,
            );
            let code = output.error.map(|error| error.code);
            assert_eq!(
                output.timed_out,
                failure.is_transient(),
                "{failure:?} must set timedOut only when the budget was spent"
            );
            assert_eq!(
                code == Some("timeout"),
                failure == BarrierFailure::BarrierTimedOut,
                "{failure:?} must not borrow the timeout code"
            );
        }
    }
}
