use serde::Serialize;

use super::{SyncOperationCountsJson, WatcherRuntimeStateJson};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SyncSuccessStatusJson<'a> {
    pub(super) state: &'static str,
    pub(super) tick_count: u64,
    pub(super) watcher_state: WatcherRuntimeStateJson<'a>,
    pub(super) last_outcome: &'static str,
    pub(super) workspace_id: &'a str,
    pub(super) snapshot_id: &'a str,
    pub(super) version: u64,
    pub(super) conflict_count: usize,
    pub(super) scan: super::SyncScanSummary,
    pub(super) queue_counts: SyncOperationCountsJson,
    pub(super) local_head: Option<LocalHeadJson>,
    pub(super) remote_head: Option<RemoteHeadJson>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct LimitedSyncStatusJson<'a> {
    pub(super) state: &'static str,
    pub(super) tick_count: u64,
    pub(super) watcher_state: WatcherRuntimeStateJson<'a>,
    pub(super) limited_capability: &'static str,
    // Fixed machine code (SyncExternalFailureCode::as_code); never raw error
    // text, which can embed workspace paths.
    pub(super) unavailable_because: &'static str,
    pub(super) blocked_action: &'static str,
    pub(super) still_works: &'static [&'static str],
    pub(super) queue_counts: SyncOperationCountsJson,
    pub(super) local_head: Option<LocalHeadJson>,
    pub(super) remote_head: Option<RemoteHeadJson>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct WaitingQueueStatusJson<'a> {
    pub(super) state: &'static str,
    pub(super) tick_count: u64,
    pub(super) watcher_state: WatcherRuntimeStateJson<'a>,
    pub(super) limited_capability: &'static str,
    pub(super) unavailable_because: &'static str,
    pub(super) blocked_action: &'static str,
    pub(super) still_works: Vec<String>,
    pub(super) queue_counts: SyncOperationCountsJson,
    pub(super) local_head: Option<LocalHeadJson>,
    pub(super) remote_head: Option<RemoteHeadJson>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RemoteObserverErrorStatusJson {
    pub(super) state: &'static str,
    pub(super) tick_count: u64,
    // Fixed machine code (SyncExternalFailureCode::as_code); never raw error
    // text, which can embed workspace paths.
    pub(super) unavailable_because: &'static str,
    pub(super) next_action: &'static str,
    pub(super) queue: SyncOperationCountsJson,
    pub(super) local_head: Option<LocalHeadJson>,
    pub(super) remote_head: Option<RemoteHeadJson>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct LocalHeadJson {
    pub(super) workspace_id: String,
    pub(super) snapshot_id: String,
    pub(super) version: u64,
    pub(super) updated_at_tick: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RemoteHeadJson {
    pub(super) workspace_id: String,
    pub(super) snapshot_id: String,
    pub(super) version: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct DaemonReconcilePayloadJson {
    pub(super) root: String,
    pub(super) state_root: String,
    pub(super) tick_count: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SyncCompletionPayloadJson<'a> {
    pub(super) outcome: &'static str,
    pub(super) workspace_id: &'a str,
    pub(super) snapshot_id: &'a str,
    pub(super) version: u64,
    pub(super) conflict_count: usize,
    pub(super) scan: super::SyncScanSummary,
}
