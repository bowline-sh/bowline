//! The daemon's supervisor-facing log files and the size cap that keeps them
//! from growing forever.
//!
//! launchd opens `StandardOutPath` / `StandardErrorPath` itself and never
//! rotates them, so an agent host left running for months fills its disk with
//! daemon chatter. Dropping the paths would cap the growth by throwing every
//! diagnostic away, which is worse: the logs are the only record of why a
//! headless host stopped syncing.
//!
//! So the daemon caps its own files, and it can, because launchd only OPENS
//! them — it `dup2`s the two descriptors onto the child's stdout and stderr
//! before `exec`, and from that moment they are entries in the daemon's own
//! descriptor table. That is what makes an atomic cutoff reachable here:
//! rotation RENAMES the live file to the retained generation and then re-points
//! the daemon's own stream at a freshly opened live file. Every record is in one
//! file or the other, never in neither.
//!
//! A copy-then-truncate rotation cannot say that. Everything appended after the
//! copy reached EOF and before the truncation landed is absent from the retained
//! generation AND erased from the live file — so the mechanism that exists to
//! preserve diagnostics would destroy exactly the ones written while it ran,
//! which is when something interesting is usually happening.
//!
//! Two residual bounds, stated rather than hidden:
//!
//! - Between the rename and the `dup2` the daemon is still appending to the
//!   renamed inode. Those records are in the retained generation, not lost.
//! - A child process that inherited the pre-rotation stdout keeps writing into
//!   the retained generation until it exits. Same story: retained, not lost.

use std::{
    error::Error,
    fmt, fs, io,
    os::fd::{AsFd, BorrowedFd},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

const STDOUT_LOG_FILE_NAME: &str = "bowline-daemon.out.log";
const STDERR_LOG_FILE_NAME: &str = "bowline-daemon.err.log";

/// Mode for a log file this module creates. A daemon log names workspace paths
/// and project directories, so it is owner-only like every other file the engine
/// authors.
const PRIVATE_LOG_MODE: u32 = 0o600;

/// The size bound for one log stream. Total on-disk bytes per stream are
/// `max_bytes * (retained_generations + 1)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogRotationPolicy {
    pub max_bytes: u64,
    pub retained_generations: u8,
}

impl LogRotationPolicy {
    /// 8 MiB live plus one retained generation per stream: enough history to
    /// explain a failure that happened overnight, bounded at 32 MiB for the
    /// daemon's two streams together.
    pub const DEFAULT: Self = Self {
        max_bytes: 8 * 1024 * 1024,
        retained_generations: 1,
    };
}

/// Which file a descriptor is open on, as the kernel identifies it. Rotation
/// compares this against the file it is about to rotate: re-pointing a stream is
/// only correct when that stream is the one writing the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileIdentity {
    pub device: u64,
    pub inode: u64,
}

/// A descriptor the daemon's records are written through, which rotation
/// re-points at the new live file.
///
/// Both methods take `&self`: re-pointing a standard stream mutates the
/// process's descriptor table, not this value, and a writer appending through
/// the same descriptor concurrently must be able to hold it at the same time.
pub trait LogStream {
    /// Which file the stream writes into right now.
    fn current_file(&self) -> io::Result<FileIdentity>;
    /// Make every subsequent write through this stream land in `replacement`.
    fn reattach(&self, replacement: BorrowedFd<'_>) -> io::Result<()>;
}

/// The daemon's own standard streams — the descriptors launchd opened onto the
/// two log files before handing the process over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StandardStream {
    Out,
    Error,
}

impl LogStream for StandardStream {
    fn current_file(&self) -> io::Result<FileIdentity> {
        descriptor_identity(match self {
            Self::Out => rustix::stdio::stdout(),
            Self::Error => rustix::stdio::stderr(),
        })
    }

    fn reattach(&self, replacement: BorrowedFd<'_>) -> io::Result<()> {
        match self {
            Self::Out => rustix::stdio::dup2_stdout(replacement),
            Self::Error => rustix::stdio::dup2_stderr(replacement),
        }
        .map_err(io::Error::from)
    }
}

#[derive(Debug)]
pub enum LogRotationError {
    Inspect {
        path: PathBuf,
        source: io::Error,
    },
    Retain {
        path: PathBuf,
        source: io::Error,
    },
    /// The live file was rotated aside but the stream could not be pointed at a
    /// new one. `restored_live_file` says whether the rotated file was put back
    /// under its own name, which is the difference between "the next pass tries
    /// again" and "the daemon is appending to a file nothing will cap".
    Reattach {
        path: PathBuf,
        source: io::Error,
        restored_live_file: bool,
    },
}

/// Whether one enforcement pass rotated a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogRotationOutcome {
    /// The file was missing or still under the cap.
    Unchanged,
    /// The file was over the cap: it became the newest retained generation and
    /// the stream now writes a fresh live file.
    Rotated,
    /// The file is over the cap but this process's stream does not write into
    /// it — a daemon started by hand has a terminal on stdout, and a stale log
    /// from a supervised run is nobody's live file. Rotating it would hijack
    /// whatever the stream really points at and leave the real writer appending
    /// into the retained copy.
    Unattached,
}

pub fn stdout_log_path(state_root: &Path) -> PathBuf {
    state_root.join(STDOUT_LOG_FILE_NAME)
}

pub fn stderr_log_path(state_root: &Path) -> PathBuf {
    state_root.join(STDERR_LOG_FILE_NAME)
}

/// Both daemon log streams for one state root: the file launchd opens, paired
/// with the descriptor the daemon writes it through. One table owns the pairing,
/// so the plist, the cap, and the reattach can never name different files.
pub fn daemon_log_streams(state_root: &Path) -> [(PathBuf, StandardStream); 2] {
    [
        (stdout_log_path(state_root), StandardStream::Out),
        (stderr_log_path(state_root), StandardStream::Error),
    ]
}

/// Both daemon log paths, in the fixed order above, for callers that report
/// per-file outcomes and never write to the streams.
pub fn daemon_log_paths(state_root: &Path) -> [PathBuf; 2] {
    daemon_log_streams(state_root).map(|(path, _)| path)
}

/// Enforces the cap on both of this process's log streams.
///
/// Only the daemon may call this: it re-points the calling process's own stdout
/// and stderr, which is the whole reason the cutoff can be atomic.
pub fn enforce_daemon_log_caps(
    state_root: &Path,
    policy: LogRotationPolicy,
) -> Result<(), LogRotationError> {
    let [(out_path, out_stream), (error_path, error_stream)] = daemon_log_streams(state_root);
    enforce_stream_caps(
        [
            (out_path, &out_stream as &dyn LogStream),
            (error_path, &error_stream as &dyn LogStream),
        ],
        policy,
    )
}

/// Enforces the cap on every named stream and reports the first failure. Every
/// stream is attempted: one unrotatable file must not leave the other growing,
/// which is exactly the failure mode this module exists to prevent.
fn enforce_stream_caps<'a>(
    streams: impl IntoIterator<Item = (PathBuf, &'a dyn LogStream)>,
    policy: LogRotationPolicy,
) -> Result<(), LogRotationError> {
    let mut first_failure = None;
    for (path, stream) in streams {
        if let Err(error) = enforce_log_cap(&path, policy, stream) {
            first_failure = first_failure.or(Some(error));
        }
    }
    match first_failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

pub fn enforce_log_cap(
    path: &Path,
    policy: LogRotationPolicy,
    stream: &dyn LogStream,
) -> Result<LogRotationOutcome, LogRotationError> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        // A supervisor that has not spawned the daemon yet has written no file.
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(LogRotationOutcome::Unchanged);
        }
        Err(source) => {
            return Err(LogRotationError::Inspect {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    if metadata.len() <= policy.max_bytes {
        return Ok(LogRotationOutcome::Unchanged);
    }
    let written_here = stream
        .current_file()
        .map_err(|source| LogRotationError::Inspect {
            path: path.to_path_buf(),
            source,
        })?;
    if written_here != identity_of(&metadata) {
        return Ok(LogRotationOutcome::Unattached);
    }
    rotate(path, policy, stream)?;
    Ok(LogRotationOutcome::Rotated)
}

fn rotate(
    path: &Path,
    policy: LogRotationPolicy,
    stream: &dyn LogStream,
) -> Result<(), LogRotationError> {
    shift_retained_generations(path, policy.retained_generations)?;
    let retained = generation_path(path, 1);
    // The cutoff. Renaming carries the live inode — with every record written
    // into it up to this instant — into the retained generation in one atomic
    // step, so nothing can fall between a snapshot and a truncation.
    fs::rename(path, &retained).map_err(|source| LogRotationError::Retain {
        path: retained.clone(),
        source,
    })?;
    if let Err(source) = reattach_live_file(path, stream) {
        // Put the live inode back under its own name. The stream is still
        // writing into it, so leaving it renamed would hide every later record
        // from the reader and from the next cap check alike.
        let restored_live_file = fs::rename(&retained, path).is_ok();
        return Err(LogRotationError::Reattach {
            path: path.to_path_buf(),
            source,
            restored_live_file,
        });
    }
    if policy.retained_generations == 0 {
        // The policy keeps no history; the generation existed only to be the
        // atomic cutoff.
        fs::remove_file(&retained).map_err(|source| LogRotationError::Retain {
            path: retained,
            source,
        })?;
    }
    Ok(())
}

/// Shifts `<path>.1..<path>.N-1` one generation older, dropping what falls off
/// the end so the live file's rename has generation 1 free.
fn shift_retained_generations(
    path: &Path,
    retained_generations: u8,
) -> Result<(), LogRotationError> {
    for generation in (1..retained_generations).rev() {
        let from = generation_path(path, generation);
        let to = generation_path(path, generation + 1);
        match fs::rename(&from, &to) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(LogRotationError::Retain { path: from, source }),
        }
    }
    Ok(())
}

/// Create the new live file and point the stream at it. Append mode because the
/// descriptor launchd handed over was append mode and a child may still share
/// it; Rust's line-buffered stdout flushes at every newline, so the cutoff falls
/// on a record boundary rather than mid-line.
fn reattach_live_file(path: &Path, stream: &dyn LogStream) -> io::Result<()> {
    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(PRIVATE_LOG_MODE)
        .open(path)?;
    stream.reattach(file.as_fd())
}

fn descriptor_identity(descriptor: BorrowedFd<'_>) -> io::Result<FileIdentity> {
    // The clone is closed with the `File`; the descriptor passed in is untouched.
    let file = fs::File::from(descriptor.try_clone_to_owned()?);
    Ok(identity_of(&file.metadata()?))
}

fn identity_of(metadata: &fs::Metadata) -> FileIdentity {
    FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    }
}

fn generation_path(path: &Path, generation: u8) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".{generation}"));
    PathBuf::from(name)
}

impl fmt::Display for LogRotationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Inspect { path, source } => {
                write!(
                    formatter,
                    "could not read log size {}: {source}",
                    path.display()
                )
            }
            Self::Retain { path, source } => write!(
                formatter,
                "could not retain rotated log {}: {source}",
                path.display()
            ),
            Self::Reattach {
                path,
                source,
                restored_live_file,
            } => write!(
                formatter,
                "could not reopen log {} after rotating it aside ({}): {source}",
                path.display(),
                if *restored_live_file {
                    "the rotated file was restored under its own name"
                } else {
                    "the records are in the retained generation"
                },
            ),
        }
    }
}

impl Error for LogRotationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Inspect { source, .. }
            | Self::Retain { source, .. }
            | Self::Reattach { source, .. } => Some(source),
        }
    }
}

#[cfg(test)]
mod tests;
