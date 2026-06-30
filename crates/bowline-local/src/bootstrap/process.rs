use std::{
    error::Error,
    fmt,
    io::Write,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const DEFAULT_PROCESS_TIMEOUT: Duration = Duration::from_secs(300);

pub trait ProcessRunner {
    fn run(&self, program: &str, args: &[String]) -> Result<ProcessOutput, ProcessError>;

    fn run_with_stdin(
        &self,
        program: &str,
        args: &[String],
        stdin: &str,
    ) -> Result<ProcessOutput, ProcessError> {
        let _ = stdin;
        self.run(program, args)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessOutput {
    pub status_code: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug)]
pub enum ProcessError {
    Io(std::io::Error),
    TimedOut { program: String, seconds: u64 },
}

#[derive(Debug, Clone, Copy)]
pub struct SystemProcessRunner;

impl ProcessRunner for SystemProcessRunner {
    fn run(&self, program: &str, args: &[String]) -> Result<ProcessOutput, ProcessError> {
        self.run_with_stdin(program, args, "")
    }

    fn run_with_stdin(
        &self,
        program: &str,
        args: &[String],
        stdin: &str,
    ) -> Result<ProcessOutput, ProcessError> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        if let Some(mut child_stdin) = child.stdin.take()
            && !stdin.is_empty()
        {
            child_stdin.write_all(stdin.as_bytes())?;
        }
        let deadline = Instant::now() + DEFAULT_PROCESS_TIMEOUT;
        loop {
            if let Some(status) = child.try_wait()? {
                let output = child.wait_with_output()?;
                return Ok(ProcessOutput {
                    status_code: status.code().unwrap_or(1),
                    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                });
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ProcessError::TimedOut {
                    program: program.to_string(),
                    seconds: DEFAULT_PROCESS_TIMEOUT.as_secs(),
                });
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}

impl fmt::Display for ProcessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "process failed: {error}"),
            Self::TimedOut { program, seconds } => {
                write!(formatter, "`{program}` timed out after {seconds}s")
            }
        }
    }
}

impl Error for ProcessError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::TimedOut { .. } => None,
        }
    }
}

impl From<std::io::Error> for ProcessError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}
