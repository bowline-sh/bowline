use bowline_core::work_views::{
    WorkView, WorkViewLifecycle, WorkViewRetention, WorkViewRetentionState, WorkViewSyncState,
    WorkViewVisibility,
};

use crate::metadata::{MetadataStore, WorkViewAcceptClaimHandle, WorkViewAcceptClaimTransition};

use super::super::{WorkViewError, paths::append_work_event};

pub(crate) fn finalize_snapshot_accept_under_claim(
    store: &MetadataStore,
    work_view: &WorkView,
    claim: &WorkViewAcceptClaimHandle,
    generated_at: &str,
    claim_checked_at: &str,
) -> Result<bool, WorkViewError> {
    let accepted = accepted_work_view(work_view, generated_at);
    if store.upsert_work_view_under_accept_claim(&accepted, claim, claim_checked_at)?
        != WorkViewAcceptClaimTransition::Applied
    {
        return Ok(false);
    }
    if accepted != *work_view {
        append_work_event(
            store,
            bowline_core::events::EventName::WorkAccepted,
            &accepted,
            generated_at,
        );
    }
    Ok(true)
}

fn accepted_work_view(work_view: &WorkView, generated_at: &str) -> WorkView {
    let mut accepted = work_view.clone();
    accepted.lifecycle = WorkViewLifecycle::Accepted;
    accepted.visibility = WorkViewVisibility::Hidden;
    accepted.sync_state = WorkViewSyncState::Synced;
    accepted.attention.clear();
    accepted.retention = WorkViewRetention {
        state: WorkViewRetentionState::Retained,
        retain_until: None,
        restorable: true,
    };
    accepted.updated_at = generated_at.to_string();
    accepted
}
