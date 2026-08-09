//! What an applied filesystem op did to the pull outcome and the ancestor commit.
//!
//! Split from [`super::apply`] at the seam between *executing* a plan against the
//! filesystem and *recording* what each op settled: the executor decides, this
//! decides what the decision means for the rows that commit.

use super::super::PullOutcome;
use crate::sync::manifest_engine::endpoint::prove_rows;
use crate::sync::manifest_engine::manifest::WorkspacePath;
use crate::sync::manifest_engine::push::EngineContext;
use crate::sync::manifest_engine::store::{AncestorCommit, FileRecord};
use crate::sync::manifest_engine::unsyncable::UnsyncableRecord;

/// Stamp every row this apply adopted with the endpoint instant that proves it,
/// immediately before the transaction that commits them.
///
/// Here rather than where the rows are built because an installed file's own
/// mtime is stamped by the volume AFTER the row was decided, so only a reading
/// taken once the writes are done can prove anything about it — and a row that
/// cannot be proved is read on the next push instead of trusted
/// (see [`crate::sync::manifest_engine::endpoint`]).
pub(super) fn prove_commit(ctx: &EngineContext, commit: &mut AncestorCommit) {
    prove_rows(
        &ctx.workspace_root,
        ctx.endpoint_probe_root(),
        ctx.timestamps,
        &ctx.crypto,
        ctx.config.max_seal_bytes,
        commit.upserts.iter_mut(),
    );
}

pub(crate) enum Applied {
    Upsert(WorkspacePath, FileRecord),
    Remove(WorkspacePath),
    Aside(WorkspacePath),
    KeptLocal(WorkspacePath),
    /// The op could not be carried out for a condition about this path alone.
    /// The path is FROZEN, exactly as the merge matrix freezes `L::Unreadable`:
    /// no filesystem op, no ancestor change, and no re-push — the engine knows
    /// nothing new about local content it could not observe or write.
    Unsyncable(WorkspacePath, UnsyncableRecord),
}

pub(crate) fn record_applied(
    commit: &mut AncestorCommit,
    outcome: &mut PullOutcome,
    applied: Applied,
) {
    match applied {
        Applied::Upsert(path, record) => {
            outcome.installed.insert(path.clone());
            commit.upserts.insert(path, record);
        }
        Applied::Remove(path) => {
            outcome.deleted.insert(path.clone());
            commit.removals.insert(path);
        }
        Applied::Aside(path) => {
            outcome.conflict_asides.insert(path);
        }
        Applied::KeptLocal(path) => {
            outcome.push_again.insert(path);
        }
        Applied::Unsyncable(path, record) => {
            outcome.unsyncable.insert(path, record);
        }
    }
}
