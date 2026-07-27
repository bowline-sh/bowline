//! The operator surface for the push-side deletion breaker.
//!
//! The engine refuses to publish a removal batch that no plausible edit
//! produces, and then publishes nothing at all until a human agrees the
//! deletions are real. These two methods are the only way that agreement
//! reaches the engine: `sync.getBlockedDeletions` reads what is refused, and
//! `sync.confirmDeletions` authorises exactly one push of it.
//!
//! Neither method takes a batch as a parameter. What can be authorised is
//! whatever the engine is refusing when it folds the event — a caller that
//! could name its own batch would be re-deciding the guard from outside it.

use bowline_core::commands::{
    BlockedDeletionBatch, BlockedDeletionsReport, DeletionsConfirmation,
    DeletionsConfirmationReport, DeletionsState,
};
use bowline_core::wire::generated::{DaemonRpcError, DaemonRpcErrorCode};
use bowline_local::sync::manifest_engine::{
    Degradation, EngineSnapshot, WorkspacePath, mass_deletion_threshold,
};

use crate::daemon::DaemonServerState;
use crate::daemon::rpc_service::{
    CancellationPoint, RequestContext, RpcResult, checkpoint, internal_serialization_error,
    request_context_error, rpc_error,
};

/// How many refused paths one response carries. A refusal can name every entry
/// in the workspace; a preview is meant to be read, and the count beside it
/// already carries the magnitude.
const MAX_LISTED_PATHS: usize = 200;

/// Read the refused batch off a snapshot, deriving the ceiling from the same
/// function the guard applied rather than carrying a second copy of it.
fn blocked_batch(snapshot: &EngineSnapshot) -> Option<BlockedDeletionBatch> {
    let Degradation::MassDeletionBlocked { removals, entries } = snapshot.degradation else {
        return None;
    };
    let paths = snapshot
        .refused_removals
        .iter()
        .take(MAX_LISTED_PATHS)
        .map(|path| WorkspacePath::as_str(path).to_string())
        .collect::<Vec<_>>();
    Some(BlockedDeletionBatch {
        removals: saturated_u64(removals),
        entries: saturated_u64(entries),
        threshold: saturated_u64(mass_deletion_threshold(entries)),
        listed: saturated_u64(paths.len()),
        paths,
    })
}

fn saturated_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

/// Report the refused removal batch, changing nothing.
pub(super) fn get_blocked_deletions(
    context: &RequestContext,
    state: &DaemonServerState,
) -> RpcResult<serde_json::Value> {
    checkpoint(context, CancellationPoint::BeforeProjectionRead)?;
    let snapshot = state.engine_snapshot().ok_or_else(engine_unavailable)?;
    let blocked = blocked_batch(&snapshot);
    serde_json::to_value(BlockedDeletionsReport {
        state: if blocked.is_some() {
            DeletionsState::Blocked
        } else {
            DeletionsState::Clear
        },
        blocked,
    })
    .map_err(internal_serialization_error)
}

/// Authorise one push of the refused removal batch.
pub(super) fn confirm_deletions(
    context: &RequestContext,
    state: &DaemonServerState,
    peer_credential_checked: bool,
) -> RpcResult<serde_json::Value> {
    if !peer_credential_checked {
        return Err(rpc_error(
            DaemonRpcErrorCode::PermissionDenied,
            "confirming deletions requires a verified same-user local socket peer",
            false,
        ));
    }
    // Read the block before arming it: the batch the caller is told it released
    // must be the one the engine was refusing, and that state is gone the moment
    // the confirmation is folded.
    let snapshot = state.engine_snapshot().ok_or_else(engine_unavailable)?;
    let Some(blocked) = blocked_batch(&snapshot) else {
        return serde_json::to_value(DeletionsConfirmationReport {
            state: DeletionsConfirmation::NotBlocked,
            blocked: None,
        })
        .map_err(internal_serialization_error);
    };
    context
        .begin_commit_fence()
        .map_err(|error| request_context_error(context, error))?;
    state
        .confirm_mass_deletion()
        .map_err(|error| rpc_error(DaemonRpcErrorCode::Unavailable, error.as_str(), true))?;
    serde_json::to_value(DeletionsConfirmationReport {
        state: DeletionsConfirmation::Authorized,
        blocked: Some(blocked),
    })
    .map_err(internal_serialization_error)
}

fn engine_unavailable() -> Box<DaemonRpcError> {
    rpc_error(
        DaemonRpcErrorCode::Unavailable,
        "daemon is not serving a sync workspace",
        true,
    )
}
