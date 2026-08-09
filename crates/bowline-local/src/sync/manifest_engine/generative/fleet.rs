//! A fleet of engine devices sharing one remote — the substrate every
//! generative property runs on.
//!
//! Each [`Device`] owns a real temp workspace, a real `ManifestEngine` over a
//! real SQLite ancestor store, and a virtual clock; the remote is passed in by
//! reference so several devices genuinely share one object store and one CAS
//! ref rather than copies of it. Nothing here reaches into engine internals:
//! the only levers are the public event/cycle API, which keeps the harness
//! stable while pull classification, sealing, and path normalization change
//! underneath it.

use std::error::Error;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use super::super::engine_test_support::{TestClock, engine_io, open_engine_store, test_context};
use super::super::push::{RemoteObjects, RemoteRef};
use super::super::{Degradation, EngineError, EngineEvent, FullScanReason, ManifestEngine};
use super::tree::TreeSpec;
use crate::workspace::TempWorkspace;

/// Cycles one [`Device::settle`] may run before the harness declares the device
/// unable to reach a quiet state. Generous, because a settle legitimately loops
/// over pull → aside → re-push → pull; a real livelock still terminates the test
/// instead of hanging CI.
const MAX_SETTLE_CYCLES: u32 = 64;

#[derive(Debug)]
pub(crate) enum FleetError {
    Start {
        device: String,
        error: EngineError,
    },
    Cycle {
        device: String,
        error: EngineError,
    },
    Workspace {
        operation: &'static str,
        error: io::Error,
    },
    /// The device still had work due after [`MAX_SETTLE_CYCLES`] cycles.
    Unsettled {
        device: String,
        cycles: u32,
    },
}

impl fmt::Display for FleetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Start { device, error } => write!(formatter, "{device} failed to start: {error}"),
            Self::Cycle { device, error } => write!(formatter, "{device} cycle failed: {error}"),
            Self::Workspace { operation, error } => {
                write!(formatter, "workspace {operation} failed: {error}")
            }
            Self::Unsettled { device, cycles } => {
                write!(
                    formatter,
                    "{device} still had work due after {cycles} cycles"
                )
            }
        }
    }
}

impl Error for FleetError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Start { error, .. } | Self::Cycle { error, .. } => Some(error),
            Self::Workspace { error, .. } => Some(error),
            Self::Unsettled { .. } => None,
        }
    }
}

/// How a settle finished, so a property can assert on the work it took rather
/// than only on the end state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SettleOutcome {
    pub(crate) cycles: u32,
}

pub(crate) struct Device {
    // Held so the temp workspace outlives the device (Drop cleans it up).
    _workspace: TempWorkspace,
    root: PathBuf,
    device_id: String,
    engine: ManifestEngine,
    clock: TestClock,
    crashes: u64,
}

impl Device {
    pub(crate) fn new(workspace_name: &str, device_id: &str) -> Result<Self, FleetError> {
        let workspace =
            TempWorkspace::new(workspace_name).map_err(|error| FleetError::Workspace {
                operation: "create",
                error,
            })?;
        let root = workspace.root().to_path_buf();
        let engine = ManifestEngine::new(
            open_engine_store(&root),
            test_context(root.clone(), device_id),
        );
        Ok(Self {
            _workspace: workspace,
            root,
            device_id: device_id.to_string(),
            engine,
            clock: TestClock::new(),
            crashes: 0,
        })
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn device_id(&self) -> &str {
        &self.device_id
    }

    pub(crate) fn start<T>(&mut self, remote: &T) -> Result<(), FleetError>
    where
        T: RemoteObjects + RemoteRef,
    {
        let io = engine_io(remote, &self.clock);
        self.engine.start(&io).map_err(|error| FleetError::Start {
            device: self.device_id.clone(),
            error,
        })
    }

    /// Drop the engine and rebuild it over the SAME database file, then re-run
    /// startup — the crash/restart path, without touching workspace bytes.
    pub(crate) fn restart<T>(&mut self, remote: &T) -> Result<(), FleetError>
    where
        T: RemoteObjects + RemoteRef,
    {
        self.engine = ManifestEngine::new(
            open_engine_store(&self.root),
            test_context(self.root.clone(), &self.device_id),
        );
        self.start(remote)
    }

    /// Re-observe both authorities and run cycles until this device has nothing
    /// left due.
    ///
    /// This is a work-drain operation, not exact workspace authorization. The
    /// generative corpus intentionally plants unsyncable filesystem objects: an
    /// engine can finish every actionable scan/pull/push while that represented
    /// blocker correctly prevents an exact convergence receipt. Re-observe both
    /// authorities directly, then stop only once the actionable frontier is
    /// drained, the authoritative and applied refs agree, and no transient
    /// degradation remains.
    pub(crate) fn settle<T>(&mut self, remote: &T) -> Result<SettleOutcome, FleetError>
    where
        T: RemoteObjects + RemoteRef,
    {
        self.engine.on_event(
            EngineEvent::FullScanRequired(FullScanReason::PeriodicAudit),
            &self.clock,
        );
        self.engine.on_event(EngineEvent::RefChanged, &self.clock);

        for cycle in 0..MAX_SETTLE_CYCLES {
            let outcome = {
                let io = engine_io(remote, &self.clock);
                self.engine.run_due_work(&io)
            };
            outcome.map_err(|error| FleetError::Cycle {
                device: self.device_id.clone(),
                error,
            })?;
            let now = self.clock.millis();
            let due = self.engine.next_timeout(now);
            let work_due_now = due.is_some_and(|timeout| timeout.is_zero());
            let snapshot = self.engine.snapshot();
            if snapshot.is_work_drained()
                && snapshot.degradation == Degradation::Nominal
                && snapshot.exact_observed_ref() == snapshot.applied_ref
                && !work_due_now
            {
                return Ok(SettleOutcome { cycles: cycle + 1 });
            }
            // Nothing due at this instant: jump the virtual clock to the next
            // armed deadline (debounce, backoff, or audit) so retries and
            // deferred rescans actually run instead of the loop spinning.
            match due {
                Some(timeout) if !timeout.is_zero() => {
                    let millis = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
                    self.clock.advance(millis.saturating_add(1));
                }
                Some(_) => {}
                None => {
                    return Err(FleetError::Unsettled {
                        device: self.device_id.clone(),
                        cycles: cycle + 1,
                    });
                }
            }
        }
        Err(FleetError::Unsettled {
            device: self.device_id.clone(),
            cycles: MAX_SETTLE_CYCLES,
        })
    }

    /// Run one due cycle without settling, so a chaos schedule can interleave
    /// faults and crashes between individual cycles.
    pub(crate) fn step<T>(&mut self, remote: &T) -> Result<(), EngineError>
    where
        T: RemoteObjects + RemoteRef,
    {
        let io = engine_io(remote, &self.clock);
        self.engine.run_due_work(&io)
    }

    pub(crate) fn wake(&mut self, event: EngineEvent) {
        self.engine.on_event(event, &self.clock);
    }

    pub(crate) fn advance(&mut self, millis: u64) {
        self.clock.advance(millis);
    }

    pub(crate) fn files(&self) -> Result<TreeSpec, FleetError> {
        TreeSpec::read_from_disk(&self.root).map_err(|error| FleetError::Workspace {
            operation: "read",
            error,
        })
    }

    /// Copy the entire device root — workspace bytes AND the engine's private
    /// state directory — so a later rollback restores a coherent pre-cycle
    /// world, not just the user-visible files.
    pub(crate) fn snapshot_to(&self, destination: &Path) -> Result<(), FleetError> {
        reset_dir(destination)?;
        copy_tree(&self.root, destination)
    }

    /// Power loss: close the live engine, roll the whole root back to a
    /// snapshot, and start a fresh engine over the restored state.
    ///
    /// The engine is first detached onto a scratch root so its SQLite handle is
    /// released before the database file underneath it is replaced — rolling a
    /// file back beneath an open connection would be testing the harness, not
    /// the engine.
    pub(crate) fn crash_to_snapshot<T>(
        &mut self,
        snapshot: &Path,
        remote: &T,
    ) -> Result<(), FleetError>
    where
        T: RemoteObjects + RemoteRef,
    {
        let scratch = std::env::temp_dir().join(format!(
            "bowline-detached-{}-{}-{}",
            std::process::id(),
            self.device_id,
            self.crashes
        ));
        self.crashes = self.crashes.saturating_add(1);
        reset_dir(&scratch)?;
        let detached = std::mem::replace(
            &mut self.engine,
            ManifestEngine::new(
                open_engine_store(&scratch),
                test_context(scratch.clone(), &self.device_id),
            ),
        );
        drop(detached);

        reset_dir(&self.root)?;
        copy_tree(snapshot, &self.root)?;

        self.engine = ManifestEngine::new(
            open_engine_store(&self.root),
            test_context(self.root.clone(), &self.device_id),
        );
        // The scratch engine is dropped by the assignment above, so its store
        // file is closed and the directory can go.
        let _ = std::fs::remove_dir_all(&scratch);
        self.start(remote)
    }
}

fn reset_dir(path: &Path) -> Result<(), FleetError> {
    if path.exists() {
        std::fs::remove_dir_all(path).map_err(|error| FleetError::Workspace {
            operation: "clear",
            error,
        })?;
    }
    std::fs::create_dir_all(path).map_err(|error| FleetError::Workspace {
        operation: "create",
        error,
    })
}

fn copy_tree(source: &Path, destination: &Path) -> Result<(), FleetError> {
    copy_tree_io(source, destination).map_err(|error| FleetError::Workspace {
        operation: "copy",
        error,
    })
}

fn copy_tree_io(source: &Path, destination: &Path) -> io::Result<()> {
    std::fs::create_dir_all(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        let metadata = std::fs::symlink_metadata(entry.path())?;
        if metadata.is_dir() {
            copy_tree_io(&entry.path(), &target)?;
        } else if metadata.is_file() {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}
