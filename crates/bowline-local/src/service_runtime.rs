use std::{fmt, io, path::PathBuf};

use crate::bootstrap::process::{ProcessError, ProcessOutput, ProcessRunner};

// The service files keep platform parsing local; shared runtime plumbing lives here so platform-specific behavior cannot drift through copy-paste.
#[derive(Debug)]
pub(crate) enum ServiceRuntimeError {
    Io(io::Error),
    Process(ProcessError),
    Unavailable(String),
    CommandFailed(CommandFailure),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandFailure {
    program: String,
    status_code: i32,
    stderr: String,
}

impl CommandFailure {
    fn new(program: &str, status_code: i32, stderr: String) -> Self {
        Self {
            program: program.to_string(),
            status_code,
            stderr,
        }
    }

    pub(crate) fn stderr(&self) -> &str {
        &self.stderr
    }

    pub(crate) fn into_parts(self) -> (String, i32, String) {
        (self.program, self.status_code, self.stderr)
    }
}

pub(crate) trait ServiceOutcomeParts<State> {
    fn from_service_parts(service_name: String, unit_path: PathBuf, state: State) -> Self;
}

pub(crate) fn service_outcome<Outcome, State>(
    service_name: &str,
    unit_path: PathBuf,
    state: State,
) -> Outcome
where
    Outcome: ServiceOutcomeParts<State>,
{
    Outcome::from_service_parts(service_name.to_string(), unit_path, state)
}

pub(crate) fn run_service_command<R, I, S, IgnoreFailure, ClassifyFailure>(
    runner: &R,
    program: &str,
    args: I,
    ignore_failure: IgnoreFailure,
    classify_failure: ClassifyFailure,
) -> Result<ProcessOutput, ServiceRuntimeError>
where
    R: ProcessRunner,
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
    IgnoreFailure: FnOnce(&str) -> bool,
    ClassifyFailure: FnOnce(CommandFailure) -> ServiceRuntimeError,
{
    let args = args
        .into_iter()
        .map(|arg| arg.as_ref().to_string())
        .collect::<Vec<_>>();
    let output = runner.run(program, &args)?;
    if output.status_code == 0 || ignore_failure(&output.stderr) {
        return Ok(output);
    }
    Err(classify_failure(CommandFailure::new(
        program,
        output.status_code,
        output.stderr,
    )))
}

pub(crate) fn classify_command_failure(
    failure: CommandFailure,
    unavailable: impl FnOnce(&str) -> bool,
    unavailable_message: &'static str,
) -> ServiceRuntimeError {
    if unavailable(failure.stderr()) {
        return ServiceRuntimeError::Unavailable(unavailable_message.to_string());
    }
    ServiceRuntimeError::CommandFailed(failure)
}

pub(crate) fn fmt_io_error(
    formatter: &mut fmt::Formatter<'_>,
    context: &str,
    error: &io::Error,
) -> fmt::Result {
    write!(formatter, "{context}: {error}")
}

pub(crate) fn fmt_command_failed(
    formatter: &mut fmt::Formatter<'_>,
    program: &str,
    status_code: i32,
    stderr: &str,
) -> fmt::Result {
    write!(
        formatter,
        "`{program}` failed with status {status_code}: {stderr}"
    )
}

impl From<io::Error> for ServiceRuntimeError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<ProcessError> for ServiceRuntimeError {
    fn from(error: ProcessError) -> Self {
        Self::Process(error)
    }
}

macro_rules! service_error {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $($platform_variant:ident => $platform_message:expr,)*
        }
        io_context: $io_context:expr $(,)?
    ) => {
        $(#[$meta])*
        #[derive(Debug)]
        $vis enum $name {
            $($platform_variant,)*
            Io(std::io::Error),
            Process($crate::bootstrap::process::ProcessError),
            Unavailable(String),
            CommandFailed {
                program: String,
                status_code: i32,
                stderr: String,
            },
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                match self {
                    $(Self::$platform_variant => formatter.write_str($platform_message),)*
                    Self::Io(error) => $crate::service_runtime::fmt_io_error(
                        formatter,
                        $io_context,
                        error,
                    ),
                    Self::Process(error) => std::fmt::Display::fmt(error, formatter),
                    Self::Unavailable(message) => formatter.write_str(message),
                    Self::CommandFailed {
                        program,
                        status_code,
                        stderr,
                    } => $crate::service_runtime::fmt_command_failed(
                        formatter,
                        program,
                        *status_code,
                        stderr,
                    ),
                }
            }
        }

        impl std::error::Error for $name {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                match self {
                    Self::Io(error) => Some(error),
                    Self::Process(error) => Some(error),
                    _ => None,
                }
            }
        }

        impl From<std::io::Error> for $name {
            fn from(error: std::io::Error) -> Self {
                Self::Io(error)
            }
        }

        impl From<$crate::bootstrap::process::ProcessError> for $name {
            fn from(error: $crate::bootstrap::process::ProcessError) -> Self {
                Self::Process(error)
            }
        }

        impl From<$crate::service_runtime::ServiceRuntimeError> for $name {
            fn from(error: $crate::service_runtime::ServiceRuntimeError) -> Self {
                match error {
                    $crate::service_runtime::ServiceRuntimeError::Io(error) => Self::Io(error),
                    $crate::service_runtime::ServiceRuntimeError::Process(error) => {
                        Self::Process(error)
                    }
                    $crate::service_runtime::ServiceRuntimeError::Unavailable(message) => {
                        Self::Unavailable(message)
                    }
                    $crate::service_runtime::ServiceRuntimeError::CommandFailed(failure) => {
                        let (program, status_code, stderr) = failure.into_parts();
                        Self::CommandFailed {
                            program,
                            status_code,
                            stderr,
                        }
                    }
                }
            }
        }
    };
}

pub(crate) use service_error;

macro_rules! service_outcome_parts {
    ($outcome:ty, $state:ty) => {
        impl $crate::service_runtime::ServiceOutcomeParts<$state> for $outcome {
            fn from_service_parts(
                service_name: String,
                unit_path: std::path::PathBuf,
                state: $state,
            ) -> Self {
                Self {
                    service_name,
                    unit_path,
                    state,
                }
            }
        }
    };
}

pub(crate) use service_outcome_parts;

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
