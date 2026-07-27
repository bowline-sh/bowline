use std::{
    error::Error,
    fmt,
    io::{Read, Write},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const DEFAULT_PROCESS_TIMEOUT: Duration = Duration::from_secs(300);

type PipeReader = thread::JoinHandle<std::io::Result<Vec<u8>>>;

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
    TimedOut {
        program: String,
        seconds: u64,
    },
    /// A pipe drain thread ended without handing its bytes back, so the output
    /// this call reports would be a truncation nobody could tell from real
    /// output. Reported rather than substituted with what was collected.
    OutputLost {
        program: String,
        stream: &'static str,
    },
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
        // Drain both pipes from the moment the child starts. A child that fills
        // its ~64 KiB stdout buffer blocks in `write` and can never exit, so
        // waiting for exit before reading turns any large remote answer into a
        // full-timeout stall instead of the output the caller asked for.
        let stdout = child.stdout.take().map(drain_pipe);
        let stderr = child.stderr.take().map(drain_pipe);
        if let Some(mut child_stdin) = child.stdin.take()
            && !stdin.is_empty()
        {
            child_stdin.write_all(stdin.as_bytes())?;
        }
        let deadline = Instant::now() + DEFAULT_PROCESS_TIMEOUT;
        loop {
            if let Some(status) = child.try_wait()? {
                return Ok(ProcessOutput {
                    status_code: status.code().unwrap_or(1),
                    stdout: collect_pipe(program, "stdout", stdout)?,
                    stderr: collect_pipe(program, "stderr", stderr)?,
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

fn drain_pipe(mut pipe: impl Read + Send + 'static) -> PipeReader {
    thread::spawn(move || {
        let mut buffer = Vec::new();
        pipe.read_to_end(&mut buffer)?;
        Ok(buffer)
    })
}

fn collect_pipe(
    program: &str,
    stream: &'static str,
    reader: Option<PipeReader>,
) -> Result<String, ProcessError> {
    let Some(reader) = reader else {
        return Ok(String::new());
    };
    let bytes = reader.join().map_err(|_| ProcessError::OutputLost {
        program: program.to_string(),
        stream,
    })??;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

impl fmt::Display for ProcessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "process failed: {error}"),
            Self::TimedOut { program, seconds } => {
                write!(formatter, "`{program}` timed out after {seconds}s")
            }
            Self::OutputLost { program, stream } => {
                write!(formatter, "`{program}` {stream} could not be collected")
            }
        }
    }
}

impl Error for ProcessError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::TimedOut { .. } | Self::OutputLost { .. } => None,
        }
    }
}

impl From<std::io::Error> for ProcessError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// A child whose output outgrows the pipe buffer blocks in `write` and can
    /// never exit, so a runner that waits for exit before reading deadlocks
    /// until its own timeout. Bounded here rather than at the caller: the caller
    /// only learns about it as a five-minute stall it cannot explain.
    #[test]
    fn output_larger_than_the_pipe_buffer_returns_without_stalling() {
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let output = SystemProcessRunner.run_with_stdin(
                "sh",
                &[
                    "-c".to_string(),
                    "i=0; while [ $i -lt 4000 ]; do printf '%050d\\n' $i; i=$((i+1)); done"
                        .to_string(),
                ],
                "ignored stdin\n",
            );
            let _ = sender.send(output);
        });

        let output = receiver
            .recv_timeout(Duration::from_secs(30))
            .expect("a child that fills the pipe buffer still completes")
            .expect("the child runs");

        assert_eq!(output.status_code, 0);
        assert_eq!(output.stdout.len(), 4_000 * 51);
    }

    #[test]
    fn stdin_reaches_the_child_and_is_closed_so_it_can_exit() {
        let output = SystemProcessRunner
            .run_with_stdin(
                "sh",
                &["-c".to_string(), "cat".to_string()],
                "delivered secret\n",
            )
            .expect("the child runs");

        assert_eq!(output.status_code, 0);
        assert_eq!(output.stdout, "delivered secret\n");
    }
}
