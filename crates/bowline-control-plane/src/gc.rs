use bowline_core::ids::WorkspaceId;
use bowline_storage::{ByteStore, ObjectKey, StorageGcExecutionReport, execute_gc_plan, plan_gc};

use crate::{ControlPlaneResult, ObjectControlPlaneClient, Retryability};

/// How many list-plan-delete passes one sweep will run.
///
/// A sweep is only ever interrupted between two idempotent halves (bytes, then
/// the metadata row), so at most one extra pass is needed to finish whatever the
/// previous run left half-done. The rest of the budget absorbs rows that became
/// delete-eligible while the sweep was running. Anything still outstanding after
/// that is not a race — it is a stall, and it must be reported as one rather
/// than retried forever.
const MAX_SWEEP_PASSES: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlPlaneGcSweepReport {
    pub execution: StorageGcExecutionReport,
    /// Keys with no metadata row left in the control plane. Includes rows an
    /// earlier interrupted sweep had already removed, so a partially completed
    /// delete converges instead of failing forever.
    pub metadata_deleted: Vec<ObjectKey>,
    pub metadata_failures: Vec<ControlPlaneGcMetadataFailure>,
}

impl ControlPlaneGcSweepReport {
    /// Objects this pass planned to delete but did not finish removing, so a
    /// caller can see what a stall is made of.
    pub fn unfinished(&self) -> usize {
        self.execution.skipped.len() + self.execution.failures.len() + self.metadata_failures.len()
    }

    /// Removing the metadata row is the only step that shrinks the next pass's
    /// candidate list, so it — not the byte delete — is what "progress" means.
    /// A byte delete repeated against an already-absent object succeeds every
    /// time and would otherwise read as progress forever.
    fn made_progress(&self) -> bool {
        !self.metadata_deleted.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlPlaneGcMetadataFailure {
    pub key: ObjectKey,
    pub retryability: Retryability,
    pub detail: String,
}

/// Why a multi-pass sweep stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageGcSweepVerdict {
    /// A pass found nothing left to delete. Storage and metadata agree.
    Converged,
    /// Work remains and the last pass removed no metadata rows. Retrying on the
    /// same inputs would loop, so the caller must surface this instead.
    Stalled { unfinished: usize },
    /// Passes were still making progress when the budget ran out. The next
    /// scheduled sweep continues from here; nothing is wrong.
    BudgetExhausted { unfinished: usize },
}

/// Every pass of one sweep plus the reason it stopped. The per-pass reports are
/// retained rather than folded together: a stall is diagnosed from which pass
/// stopped removing rows, and a merged total erases exactly that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlPlaneGcSweep {
    pub passes: Vec<ControlPlaneGcSweepReport>,
    pub verdict: StorageGcSweepVerdict,
}

/// Delete every unreferenced object's bytes, then its control-plane metadata
/// row. Both halves are idempotent, so an interrupted sweep is finished by the
/// next one: bytes that are already gone delete cleanly, and a metadata row
/// that is already gone reports `deleted = false` rather than erroring.
pub fn sweep_storage_gc(
    control_plane: &impl ObjectControlPlaneClient,
    workspace_id: &WorkspaceId,
    store: &impl ByteStore,
) -> ControlPlaneResult<ControlPlaneGcSweepReport> {
    let latest_objects = control_plane.list_storage_gc_objects(workspace_id)?;
    let plan = plan_gc(&latest_objects);
    let execution = execute_gc_plan(&plan, &latest_objects, store);
    let mut metadata_deleted = Vec::new();
    let mut metadata_failures = Vec::new();

    for key in &execution.deleted {
        match control_plane.delete_object_metadata_after_gc(workspace_id, key.as_str()) {
            Ok(_deleted_now_or_already_absent) => metadata_deleted.push(key.clone()),
            Err(error) => metadata_failures.push(ControlPlaneGcMetadataFailure {
                key: key.clone(),
                retryability: error.retryability(),
                detail: error.to_string(),
            }),
        }
    }

    Ok(ControlPlaneGcSweepReport {
        execution,
        metadata_deleted,
        metadata_failures,
    })
}

/// Sweep repeatedly until storage and metadata agree, the sweep stalls, or the
/// pass budget runs out.
///
/// One pass cannot finish a workspace whose previous sweep died between deleting
/// an object's bytes and deleting its metadata row: the row is still listed, so
/// clearing it takes another list-plan-delete cycle. Converging here — rather
/// than leaving it to whenever the next scheduled sweep happens — is what turns
/// "eventually consistent if you wait" into a verdict a caller can act on.
pub fn sweep_storage_gc_until_converged(
    control_plane: &impl ObjectControlPlaneClient,
    workspace_id: &WorkspaceId,
    store: &impl ByteStore,
) -> ControlPlaneResult<ControlPlaneGcSweep> {
    let mut passes = Vec::new();

    for _pass in 0..MAX_SWEEP_PASSES {
        let report = sweep_storage_gc(control_plane, workspace_id, store)?;
        let unfinished = report.unfinished();
        let progressed = report.made_progress();
        let nothing_to_do = report.execution.deleted.is_empty() && unfinished == 0;
        passes.push(report);

        if nothing_to_do {
            return Ok(ControlPlaneGcSweep {
                passes,
                verdict: StorageGcSweepVerdict::Converged,
            });
        }
        if !progressed {
            return Ok(ControlPlaneGcSweep {
                passes,
                verdict: StorageGcSweepVerdict::Stalled { unfinished },
            });
        }
    }

    let unfinished = passes
        .last()
        .map_or(0, ControlPlaneGcSweepReport::unfinished);
    Ok(ControlPlaneGcSweep {
        passes,
        verdict: StorageGcSweepVerdict::BudgetExhausted { unfinished },
    })
}

#[cfg(test)]
#[path = "gc/tests.rs"]
mod tests;
