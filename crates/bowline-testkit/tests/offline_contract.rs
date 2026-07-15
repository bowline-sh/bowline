use std::{fs, path::Path};

use bowline_control_plane::{
    ConflictOccurrenceState, ConflictReconcileOutcome, ControlPlaneClient,
    WorkspaceControlPlaneClient,
};
use bowline_core::ids::DeviceId;
use bowline_core::status::StatusLevel;
use bowline_local::{
    metadata::{
        DEFAULT_DATABASE_FILE, MetadataStore, SyncClaimCheck, SyncCommittedCancelledLateResult,
        SyncOperationKind, SyncOperationState,
    },
    status::{StatusOptions, compose_status},
    sync::{
        ConflictState, WorkViewOverlaySyncResult, conflict_occurrence_is_current,
        conflict_occurrence_queue_result, decode_conflict_occurrence_operation,
        decode_work_view_overlay_sync_operation, mark_conflict_occurrence_reconciled,
        work_view_overlay_sync_result,
    },
};
use bowline_testkit::{ScenarioError, SyncScenario, TwoDeviceSyncScenario};

#[test]
fn r1_hydrated_paths_remain_locally_usable_while_remote_is_offline() {
    let scenario = SyncScenario::new("offline-r1-hydrated").expect("scenario");
    scenario
        .workspace()
        .write_file("project/src/lib.rs", b"pub fn before() {}\n")
        .expect("fixture file");
    scenario.tick().expect("initial online sync");

    scenario.control_plane().set_offline(true);
    let path = scenario.workspace().root().join("project/src/lib.rs");
    let read_ok = fs::read_to_string(&path)
        .expect("local read")
        .contains("before");
    fs::write(&path, "pub fn before() {}\npub fn offline_edit() {}\n").expect("local edit");
    let edit_ok = fs::read_to_string(&path)
        .expect("edited local read")
        .contains("offline_edit");
    let sync_error = scenario
        .tick_with_reconcile_queue()
        .expect_err("offline sync must surface transport");
    let status = status_output(&scenario);

    assert!(read_ok, "hydrated file should remain readable offline");
    assert!(edit_ok, "hydrated file should remain editable offline");
    assert_ne!(status.status.level, StatusLevel::Healthy);
    assert!(status_names_offline_network(&status));
    assert!(
        offline_error(&sync_error),
        "unexpected offline error: {sync_error}"
    );
}

#[test]
fn r2_unhydrated_content_fetch_reports_observed_limited_behavior() {
    let scenario = TwoDeviceSyncScenario::new("offline-r2-unhydrated").expect("scenario");
    scenario
        .first()
        .workspace()
        .write_file("project/src/main.rs", b"fn main() {}\n")
        .expect("fixture file");
    scenario.first().tick().expect("first online upload");

    scenario.first().control_plane().set_offline(true);
    let materialized = scenario
        .second()
        .workspace()
        .root()
        .join("project/src/main.rs");
    let sync_error = scenario
        .second()
        .tick_with_reconcile_queue()
        .expect_err("cold device cannot fetch while offline");
    seed_accepted_root(scenario.second());
    let status = status_output_for_requested_path(scenario.second(), "project/src/main.rs", false);
    let limit = status
        .limits
        .iter()
        .find(|limit| limit.unavailable_because == "cannot fetch content while offline")
        .expect("offline content fetch limit");

    assert!(
        !materialized.exists(),
        "cold content must not materialize offline"
    );
    assert_eq!(status.status.level, StatusLevel::Limited);
    assert_eq!(limit.capability, "content-fetch");
    assert_eq!(limit.path.as_deref(), Some("project/src/main.rs"));
    assert!(
        limit
            .still_works
            .iter()
            .any(|item| item == "project structure")
    );
    assert!(
        offline_error(&sync_error),
        "unexpected offline error: {sync_error}"
    );
}

#[test]
fn r3_offline_file_creates_are_observed_for_durable_queue_state() {
    let scenario = SyncScenario::new("offline-r3-queue").expect("scenario");
    scenario
        .workspace()
        .write_file("project/README.md", b"online base\n")
        .expect("fixture file");
    scenario.tick().expect("initial online sync");

    scenario.control_plane().set_offline(true);
    scenario
        .workspace()
        .write_file("project/offline.txt", b"offline payload\n")
        .expect("offline file");
    let sync_error = scenario
        .tick_with_reconcile_queue()
        .expect_err("offline upload fails");
    let queued_before_reopen = blocked_offline_operation_count(&scenario);
    let queued_after_reopen = blocked_offline_operation_count(&scenario);

    assert_eq!(queued_before_reopen, 1);
    assert_eq!(queued_after_reopen, queued_before_reopen);
    assert!(
        offline_error(&sync_error),
        "unexpected offline error: {sync_error}"
    );
    scenario.control_plane().set_offline(false);
    scenario
        .tick_with_reconcile_queue()
        .expect("queued offline create drains on reconnect");
    assert_eq!(blocked_offline_operation_count(&scenario), 0);
}

#[test]
fn r4_new_project_offline_capture_and_reconnect_visibility_are_observed() {
    let scenario = TwoDeviceSyncScenario::new("offline-r4-new-project").expect("scenario");
    scenario
        .first()
        .workspace()
        .write_file("existing/README.md", b"online base\n")
        .expect("fixture file");
    scenario.first().tick().expect("initial online sync");
    scenario.second().tick().expect("second online materialize");

    scenario.first().control_plane().set_offline(true);
    scenario
        .first()
        .workspace()
        .create_git_repo("offline-new")
        .expect("offline project git marker");
    scenario
        .first()
        .workspace()
        .write_project_file("offline-new", "src/lib.rs", b"pub fn offline() {}\n")
        .expect("offline project file");
    let sync_error = scenario
        .first()
        .tick_with_reconcile_queue()
        .expect_err("offline project upload fails");
    let queued_offline = blocked_offline_operation_count(scenario.first());

    scenario.first().control_plane().set_offline(false);
    scenario
        .first()
        .tick_with_reconcile_queue()
        .expect("reconnect upload");
    scenario
        .second()
        .tick()
        .expect("second device materialize after reconnect");
    let appeared_on_second = scenario
        .second()
        .workspace()
        .root()
        .join("offline-new/src/lib.rs")
        .exists();

    assert_eq!(queued_offline, 1);
    assert!(appeared_on_second);
    assert_eq!(blocked_offline_operation_count(scenario.first()), 0);
    assert!(
        offline_error(&sync_error),
        "unexpected offline error: {sync_error}"
    );
}

#[test]
fn r5_materialized_env_remains_readable_while_offline() {
    let scenario = SyncScenario::new("offline-r5-env").expect("scenario");
    scenario
        .workspace()
        .create_env_file("project", ".env", b"API_TOKEN=materialized\n")
        .expect("fixture env");
    scenario.tick().expect("initial online sync");

    scenario.control_plane().set_offline(true);
    let env_path = scenario.workspace().root().join("project/.env");
    let file_readable = fs::read_to_string(&env_path)
        .expect("local env read")
        .contains("materialized");
    let metadata_env_count = metadata_store(&scenario)
        .env_records(scenario.workspace_id())
        .expect("env records")
        .len();
    assert!(file_readable, "materialized env file should remain on disk");
    assert!(
        metadata_env_count > 0,
        "materialized env metadata should remain local"
    );
}

#[test]
fn r6_offline_status_truthfulness_is_observed() {
    let scenario = SyncScenario::new("offline-r6-status").expect("scenario");
    scenario
        .workspace()
        .write_file("project/README.md", b"online base\n")
        .expect("fixture file");
    scenario.tick().expect("initial online sync");

    scenario.control_plane().set_offline(true);
    let sync_error = scenario
        .tick_with_reconcile_queue()
        .expect_err("offline sync fails");
    let status = status_output(&scenario);

    assert_ne!(status.status.level, StatusLevel::Healthy);
    assert!(status_names_offline_network(&status));
    assert!(
        offline_error(&sync_error),
        "unexpected offline error: {sync_error}"
    );
}

#[test]
fn r7_divergent_offline_edits_preserve_bytes_and_conflict_metadata_observed() {
    let scenario = TwoDeviceSyncScenario::new("offline-r7-divergent").expect("scenario");
    let note = Path::new("project/notes/conflict.txt");
    scenario
        .first()
        .workspace()
        .write_file(note, b"line one\nshared line\nsafe line\n")
        .expect("base file");
    scenario.first().tick().expect("first upload");
    scenario.second().tick().expect("second materialize");

    scenario.first().control_plane().set_offline(true);
    scenario
        .first()
        .workspace()
        .write_file(note, b"line one\nfirst offline edit\nsafe line\n")
        .expect("first offline edit");
    scenario
        .second()
        .workspace()
        .write_file(
            note,
            b"line one\nsecond offline edit\nsafe line changed safely\n",
        )
        .expect("second offline edit");
    let first_offline = scenario
        .first()
        .tick_with_reconcile_queue()
        .expect_err("first offline upload fails");
    let second_offline = scenario
        .second()
        .tick_with_reconcile_queue()
        .expect_err("second offline upload fails");

    scenario.first().control_plane().set_offline(false);
    scenario
        .first()
        .tick_with_reconcile_queue()
        .expect("first reconnect");
    scenario
        .second()
        .tick_with_reconcile_queue()
        .expect("second reconnect");
    assert_eq!(
        drive_pending_conflict_occurrences(
            scenario.second().state_root(),
            scenario.second().workspace_id(),
            scenario.second().control_plane(),
            "2026-07-07T12:00:01Z",
        )
        .expect("durable conflict occurrence worker"),
        1
    );
    let conflicts = scenario
        .first()
        .control_plane()
        .list_workspace_conflicts(
            scenario.first().workspace_id(),
            &DeviceId::new("device_first"),
        )
        .unwrap_or_default();
    let first_bytes = read_string(scenario.first().workspace().root().join(note));
    let second_bytes = read_string(scenario.second().workspace().root().join(note));
    let first_preserved = first_bytes.contains("first offline edit");
    let second_preserved = second_bytes.contains("second offline edit");
    let conflict_recorded = !conflicts.is_empty();

    assert!(
        first_preserved,
        "first offline edit must remain recoverable"
    );
    assert!(
        second_preserved,
        "second offline edit must remain recoverable"
    );
    assert!(
        conflict_recorded,
        "divergent offline edits must publish conflict metadata"
    );
    assert!(
        offline_error(&first_offline),
        "unexpected offline error: {first_offline}"
    );
    assert!(
        offline_error(&second_offline),
        "unexpected offline error: {second_offline}"
    );
}

fn status_output(scenario: &SyncScenario) -> bowline_core::commands::StatusCommandOutput {
    status_output_for_path(scenario, scenario.workspace().root())
}

fn status_output_for_path(
    scenario: &SyncScenario,
    path: impl AsRef<Path>,
) -> bowline_core::commands::StatusCommandOutput {
    status_output_for_requested_path(
        scenario,
        &path.as_ref().display().to_string(),
        path.as_ref() == scenario.workspace().root(),
    )
}

fn status_output_for_requested_path(
    scenario: &SyncScenario,
    requested_path: &str,
    workspace_scope: bool,
) -> bowline_core::commands::StatusCommandOutput {
    compose_status(StatusOptions {
        db_path: Some(scenario.state_root().join(DEFAULT_DATABASE_FILE)),
        requested_path: Some(requested_path.to_string()),
        workspace_scope,
        generated_at: "2026-07-07T12:00:00Z".to_string(),
    })
    .expect("status composes")
}

fn metadata_store(scenario: &SyncScenario) -> MetadataStore {
    MetadataStore::open(scenario.state_root().join(DEFAULT_DATABASE_FILE)).expect("metadata opens")
}

fn seed_accepted_root(scenario: &SyncScenario) {
    let store = metadata_store(scenario);
    store
        .insert_workspace(scenario.workspace_id(), "Code", "2026-07-07T12:00:00Z")
        .expect("workspace row");
    store
        .insert_root(
            "root_offline_contract",
            scenario.workspace_id(),
            &scenario.workspace().root().display().to_string(),
            "2026-07-07T12:00:00Z",
        )
        .expect("accepted root");
}

fn blocked_offline_operation_count(scenario: &SyncScenario) -> usize {
    metadata_store(scenario)
        .sync_operations(scenario.workspace_id())
        .expect("sync operations")
        .into_iter()
        .filter(|operation| operation.state == SyncOperationState::BlockedOffline)
        .count()
}

fn offline_error(error: &ScenarioError) -> bool {
    error.to_string().contains("offline") || error.to_string().contains("transport")
}

fn status_names_offline_network(output: &bowline_core::commands::StatusCommandOutput) -> bool {
    output
        .items
        .iter()
        .any(|item| item.summary == "Network is offline; local cached state remains available.")
        && output.limits.iter().any(|limit| {
            limit.capability == "content-fetch"
                && limit.unavailable_because == "cannot fetch content while offline"
        })
}

fn read_string(path: impl AsRef<Path>) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

fn drive_pending_conflict_occurrences(
    state_root: &Path,
    workspace_id: &bowline_core::ids::WorkspaceId,
    control_plane: &dyn ControlPlaneClient,
    now: &str,
) -> Result<usize, String> {
    let store = MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE))
        .map_err(|error| error.to_string())?;
    let mut processed = 0;
    loop {
        let Some(claimed) = store
            .claim_next_sync_operation(
                workspace_id,
                "offline-contract-conflict-worker",
                now,
                "2999-01-01T00:00:00Z",
            )
            .map_err(|error| error.to_string())?
        else {
            return Ok(processed);
        };
        if claimed.operation.kind == SyncOperationKind::WorkViewOverlaySync {
            complete_empty_work_view_overlay(&store, &claimed, workspace_id, now)?;
            continue;
        }
        if claimed.operation.kind != SyncOperationKind::ConflictOccurrenceReconcile {
            return Err(format!(
                "conflict worker claimed unexpected operation kind {:?}",
                claimed.operation.kind
            ));
        }
        match store
            .authorize_sync_operation_boundary(&claimed.claim)
            .map_err(|error| error.to_string())?
        {
            SyncClaimCheck::Owned => {}
            SyncClaimCheck::CancellationRequested => {
                store
                    .cancel_claimed_sync_operation(
                        &claimed.claim,
                        r#"{"outcome":"cancelled"}"#,
                        now,
                    )
                    .map_err(|error| error.to_string())?;
                return Err("conflict operation was cancelled before hosted reconcile".to_string());
            }
            SyncClaimCheck::OwnershipLost => {
                return Err("conflict operation claim ownership was lost".to_string());
            }
        }

        let input = decode_conflict_occurrence_operation(&claimed.operation)
            .map_err(|error| error.to_string())?;
        let local_state = match input.desired_state {
            ConflictOccurrenceState::Unresolved => ConflictState::Unresolved,
            ConflictOccurrenceState::Accepted => ConflictState::Accepted,
            ConflictOccurrenceState::Rejected => ConflictState::Rejected,
        };
        let current = conflict_occurrence_is_current(
            state_root,
            input.conflict_id.as_str(),
            input.occurrence_version,
            local_state,
        )
        .map_err(|error| error.to_string())?;
        let remote_outcome = if current {
            control_plane
                .reconcile_conflict_occurrence(input.clone())
                .map_err(|error| error.to_string())?
                .outcome
        } else {
            ConflictReconcileOutcome::Superseded
        };
        let terminal_claim_state = store
            .renew_sync_operation_reconciliation_boundary(&claimed.claim)
            .map_err(|error| error.to_string())?;
        if terminal_claim_state == SyncClaimCheck::OwnershipLost {
            return Err(
                "conflict operation claim ownership was lost after hosted reconcile".to_string(),
            );
        }
        let outcome = if matches!(
            remote_outcome,
            ConflictReconcileOutcome::Applied | ConflictReconcileOutcome::Idempotent
        ) && mark_conflict_occurrence_reconciled(
            state_root,
            input.conflict_id.as_str(),
            input.occurrence_version,
            local_state,
            now,
        )
        .map_err(|error| error.to_string())?
        {
            remote_outcome
        } else {
            ConflictReconcileOutcome::Superseded
        };
        let result_json =
            conflict_occurrence_queue_result(outcome).map_err(|error| error.to_string())?;
        match terminal_claim_state {
            SyncClaimCheck::Owned => {
                store
                    .complete_claimed_sync_operation(&claimed.claim, &result_json, now)
                    .map_err(|error| error.to_string())?;
            }
            SyncClaimCheck::CancellationRequested => {
                let committed_result =
                    serde_json::from_str(&result_json).map_err(|error| error.to_string())?;
                store
                    .complete_committed_cancelled_late_sync_operation(
                        &claimed.claim,
                        &SyncCommittedCancelledLateResult::new(
                            SyncOperationKind::ConflictOccurrenceReconcile,
                            committed_result,
                        ),
                        now,
                    )
                    .map_err(|error| error.to_string())?;
            }
            SyncClaimCheck::OwnershipLost => unreachable!("ownership loss returned above"),
        }
        processed += 1;
    }
}

fn complete_empty_work_view_overlay(
    store: &MetadataStore,
    claimed: &bowline_local::metadata::ClaimedSyncOperation,
    workspace_id: &bowline_core::ids::WorkspaceId,
    now: &str,
) -> Result<(), String> {
    let input = decode_work_view_overlay_sync_operation(&claimed.operation)
        .map_err(|error| error.to_string())?;
    if input.workspace_id != *workspace_id {
        return Err("work-view overlay operation targeted another workspace".to_string());
    }
    if !store
        .work_views(workspace_id, true, None)
        .map_err(|error| error.to_string())?
        .is_empty()
    {
        return Err("offline conflict fixture unexpectedly contains work views".to_string());
    }
    if store
        .authorize_sync_operation_boundary(&claimed.claim)
        .map_err(|error| error.to_string())?
        != SyncClaimCheck::Owned
    {
        return Err("work-view overlay predecessor claim was not owned".to_string());
    }
    let result_json = work_view_overlay_sync_result(WorkViewOverlaySyncResult {
        uploaded: 0,
        attention: 0,
        ..WorkViewOverlaySyncResult::default()
    })
    .map_err(|error| error.to_string())?;
    if store
        .authorize_sync_operation_boundary(&claimed.claim)
        .map_err(|error| error.to_string())?
        != SyncClaimCheck::Owned
    {
        return Err("work-view overlay predecessor lost its claim".to_string());
    }
    store
        .complete_claimed_sync_operation(&claimed.claim, &result_json, now)
        .map_err(|error| error.to_string())?;
    Ok(())
}
