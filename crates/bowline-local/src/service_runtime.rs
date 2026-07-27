//! Process plumbing every service supervisor shares: one error type, one
//! command runner, one failure classification.

use std::{fmt, io};

use crate::bootstrap::process::{ProcessError, ProcessOutput, ProcessRunner};

/// Every failure any supervisor can produce. One enum, so a new failure mode is
/// modelled once instead of once per platform.
#[derive(Debug)]
pub enum ServiceError {
    /// `HOME` is unset, so neither the systemd user unit directory nor the
    /// launchd agents directory can be resolved.
    MissingHome,
    /// The current user id could not be read, so launchd's GUI domain is unknown.
    MissingUserId,
    Io(io::Error),
    Process(ProcessError),
    /// The supervisor exists but refuses to serve this session.
    Unavailable(String),
    CommandFailed {
        program: String,
        status_code: i32,
        stderr: String,
    },
}

impl ServiceError {
    /// The stderr of a failed supervisor command, for callers that classify a
    /// failure by what the supervisor said.
    pub fn command_stderr(&self) -> Option<&str> {
        match self {
            Self::CommandFailed { stderr, .. } => Some(stderr),
            _ => None,
        }
    }
}

impl fmt::Display for ServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingHome => formatter.write_str("HOME is unavailable"),
            Self::MissingUserId => formatter.write_str("the current user id is unavailable"),
            Self::Io(error) => write!(formatter, "service file operation failed: {error}"),
            Self::Process(error) => fmt::Display::fmt(error, formatter),
            Self::Unavailable(message) => formatter.write_str(message),
            Self::CommandFailed {
                program,
                status_code,
                stderr,
            } => write!(
                formatter,
                "`{program}` failed with status {status_code}: {stderr}"
            ),
        }
    }
}

impl std::error::Error for ServiceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Process(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for ServiceError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<ProcessError> for ServiceError {
    fn from(error: ProcessError) -> Self {
        Self::Process(error)
    }
}

/// How a supervisor command decides that a non-zero exit is not a failure and
/// how it explains the failures that are.
pub(crate) struct ServiceCommand<'a> {
    pub(crate) program: &'a str,
    /// Stderr patterns that mean "there was nothing to act on", which every
    /// supervisor reports as an error and every caller treats as success.
    pub(crate) tolerate_failure: fn(&str) -> bool,
    /// Stderr patterns that mean the supervisor itself is not serving this
    /// session, which is a different kind of failure from a rejected command.
    pub(crate) unavailable: fn(&str) -> bool,
    pub(crate) unavailable_message: &'static str,
}

impl ServiceCommand<'_> {
    pub(crate) fn run<R, I, S>(&self, runner: &R, args: I) -> Result<ProcessOutput, ServiceError>
    where
        R: ProcessRunner + ?Sized,
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let args = args
            .into_iter()
            .map(|arg| arg.as_ref().to_string())
            .collect::<Vec<_>>();
        let output = runner.run(self.program, &args)?;
        if output.status_code == 0 || (self.tolerate_failure)(&output.stderr) {
            return Ok(output);
        }
        if (self.unavailable)(&output.stderr) {
            return Err(ServiceError::Unavailable(
                self.unavailable_message.to_string(),
            ));
        }
        Err(ServiceError::CommandFailed {
            program: self.program.to_string(),
            status_code: output.status_code,
            stderr: output.stderr,
        })
    }
}

pub(crate) fn tolerate_nothing(_stderr: &str) -> bool {
    false
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::{cell::RefCell, collections::VecDeque, rc::Rc};

    use crate::bootstrap::process::{ProcessError, ProcessOutput, ProcessRunner};

    #[derive(Clone)]
    pub(crate) struct RecordingRunner {
        pub(crate) calls: Rc<RefCell<Vec<Vec<String>>>>,
        output: ProcessOutput,
    }

    impl RecordingRunner {
        pub(crate) fn ok() -> Self {
            Self {
                calls: Rc::new(RefCell::new(Vec::new())),
                output: ProcessOutput {
                    status_code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                },
            }
        }

        pub(crate) fn with_output(output: ProcessOutput) -> Self {
            Self {
                calls: Rc::new(RefCell::new(Vec::new())),
                output,
            }
        }
    }

    impl ProcessRunner for RecordingRunner {
        fn run(&self, program: &str, args: &[String]) -> Result<ProcessOutput, ProcessError> {
            let mut call = vec![program.to_string()];
            call.extend(args.iter().cloned());
            self.calls.borrow_mut().push(call);
            Ok(self.output.clone())
        }
    }

    #[derive(Clone)]
    pub(crate) struct SequenceRunner {
        pub(crate) calls: Rc<RefCell<Vec<Vec<String>>>>,
        outputs: Rc<RefCell<VecDeque<ProcessOutput>>>,
    }

    impl SequenceRunner {
        pub(crate) fn new(outputs: Vec<ProcessOutput>) -> Self {
            Self {
                calls: Rc::new(RefCell::new(Vec::new())),
                outputs: Rc::new(RefCell::new(outputs.into())),
            }
        }
    }

    impl ProcessRunner for SequenceRunner {
        fn run(&self, program: &str, args: &[String]) -> Result<ProcessOutput, ProcessError> {
            let mut call = vec![program.to_string()];
            call.extend(args.iter().cloned());
            self.calls.borrow_mut().push(call);
            Ok(self
                .outputs
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| ProcessOutput {
                    status_code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                }))
        }
    }
}
