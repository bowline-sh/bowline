use std::{fs, io, path::PathBuf};

use bowline_core::{
    commands::{HydrationBudgetScope, HydrationBudgetState, HydrationBudgetStatus},
    events::{
        EventActor, EventActorKind, EventName, EventSeverity, EventSubject, EventSubjectKind,
        WorkspaceEvent,
    },
    ids::EventId,
    ids::{LeaseId, ProjectId, WorkspaceId},
    status::SafeAction,
};
use rusqlite::{OptionalExtension, params};
use serde_json::json;

use crate::metadata::{AgentLeaseRecord, MetadataError, MetadataStore};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HydrationBudgetReservation {
    pub reservation_id: String,
    pub accepted: bool,
    pub status: HydrationBudgetStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HydrationBudgetReservationRequest<'a> {
    pub workspace_id: &'a WorkspaceId,
    pub project_id: &'a ProjectId,
    pub lease_id: &'a LeaseId,
    pub path: &'a str,
    pub content_id: Option<&'a str>,
    pub cause: &'a str,
    pub requested_bytes: u64,
    pub limit_bytes: u64,
    pub now: &'a str,
}

pub fn lease_budget_status(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
    lease_id: &LeaseId,
    limit_bytes: u64,
) -> Result<HydrationBudgetStatus, MetadataError> {
    budget_status(
        store,
        workspace_id,
        Some(project_id),
        Some(lease_id),
        limit_bytes,
    )
}

pub fn reserve_lease_bytes(
    store: &mut MetadataStore,
    request: HydrationBudgetReservationRequest<'_>,
) -> Result<HydrationBudgetReservation, MetadataError> {
    let reservation_id = format!(
        "budget_{}_{}",
        request.lease_id.as_str(),
        stable_token(&format!(
            "{}:{}:{}",
            request.path, request.cause, request.now
        ))
    );
    let accepted = store.with_transaction(|transaction| {
        let used: u64 = transaction
            .query_row(
                "SELECT COALESCE(SUM(committed_bytes), 0)
                 FROM hydration_budget_ledger
                 WHERE workspace_id = ?1 AND lease_id = ?2 AND outcome = 'committed'",
                params![request.workspace_id.as_str(), request.lease_id.as_str()],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .unwrap_or(0)
            .try_into()
            .unwrap_or(0);
        let reserved: u64 = transaction
            .query_row(
                "SELECT COALESCE(SUM(reserved_bytes), 0)
                 FROM hydration_budget_ledger
                 WHERE workspace_id = ?1 AND lease_id = ?2 AND outcome = 'reserved'",
                params![request.workspace_id.as_str(), request.lease_id.as_str()],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .unwrap_or(0)
            .try_into()
            .unwrap_or(0);
        let available = used
            .saturating_add(reserved)
            .saturating_add(request.requested_bytes)
            <= request.limit_bytes;
        transaction.execute(
            "INSERT INTO hydration_budget_ledger
             (id, workspace_id, project_id, lease_id, path, content_id, cause, requested_bytes,
              reserved_bytes, committed_bytes, outcome, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0, ?10, ?11, ?11)",
            params![
                reservation_id,
                request.workspace_id.as_str(),
                request.project_id.as_str(),
                request.lease_id.as_str(),
                request.path,
                request.content_id,
                request.cause,
                request.requested_bytes,
                if available {
                    request.requested_bytes
                } else {
                    0
                },
                if available { "reserved" } else { "denied" },
                request.now,
            ],
        )?;
        Ok(available)
    })?;

    let mut status = lease_budget_status(
        store,
        request.workspace_id,
        request.project_id,
        request.lease_id,
        request.limit_bytes,
    )?;
    if !accepted {
        status.state = HydrationBudgetState::Exhausted;
        status.next_action = Some(increase_budget_action(Some(request.lease_id)));
    }
    append_budget_event(
        store,
        BudgetEvent {
            id: budget_event_id(
                if accepted { "reserved" } else { "denied" },
                &reservation_id,
                request.now,
            ),
            name: if accepted {
                EventName::HydrationBudgetReserved
            } else {
                EventName::HydrationBudgetDenied
            },
            severity: if accepted {
                EventSeverity::Info
            } else {
                EventSeverity::Limited
            },
            summary: if accepted {
                "Agent hydration budget reserved."
            } else {
                "Agent hydration budget was exhausted."
            },
            workspace_id: request.workspace_id,
            project_id: Some(request.project_id),
            lease_id: Some(request.lease_id),
            path: Some(request.path),
            requested_bytes: request.requested_bytes,
            affected_bytes: if accepted { request.requested_bytes } else { 0 },
            occurred_at: request.now,
        },
    );
    Ok(HydrationBudgetReservation {
        reservation_id,
        accepted,
        status,
    })
}

pub fn commit_reservation(
    store: &MetadataStore,
    reservation_id: &str,
    committed_bytes: u64,
    now: &str,
) -> Result<(), MetadataError> {
    let changed = store.connection().execute(
        "UPDATE hydration_budget_ledger
         SET outcome = 'committed',
             committed_bytes = ?2,
             reserved_bytes = 0,
             updated_at = ?3
         WHERE id = ?1 AND outcome = 'reserved'",
        params![reservation_id, committed_bytes, now],
    )?;
    if changed > 0
        && let Some(row) = budget_ledger_event_row(store, reservation_id)?
    {
        append_budget_event(
            store,
            BudgetEvent {
                id: budget_event_id("committed", reservation_id, now),
                name: EventName::HydrationBudgetCommitted,
                severity: EventSeverity::Info,
                summary: "Agent hydration budget committed.",
                workspace_id: &row.workspace_id,
                project_id: row.project_id.as_ref(),
                lease_id: row.lease_id.as_ref(),
                path: Some(&row.path),
                requested_bytes: row.requested_bytes,
                affected_bytes: committed_bytes,
                occurred_at: now,
            },
        );
    }
    Ok(())
}

pub fn release_reservation(
    store: &MetadataStore,
    reservation_id: &str,
    now: &str,
) -> Result<(), MetadataError> {
    let changed = store.connection().execute(
        "UPDATE hydration_budget_ledger
         SET outcome = 'released',
             reserved_bytes = 0,
             updated_at = ?2
         WHERE id = ?1 AND outcome = 'reserved'",
        params![reservation_id, now],
    )?;
    if changed > 0
        && let Some(row) = budget_ledger_event_row(store, reservation_id)?
    {
        append_budget_event(
            store,
            BudgetEvent {
                id: budget_event_id("released", reservation_id, now),
                name: EventName::HydrationBudgetReleased,
                severity: EventSeverity::Info,
                summary: "Agent hydration budget reservation released.",
                workspace_id: &row.workspace_id,
                project_id: row.project_id.as_ref(),
                lease_id: row.lease_id.as_ref(),
                path: Some(&row.path),
                requested_bytes: row.requested_bytes,
                affected_bytes: row.requested_bytes,
                occurred_at: now,
            },
        );
    }
    Ok(())
}

pub fn grant_lease_budget_override(
    store: &mut MetadataStore,
    lease: &AgentLeaseRecord,
    added_bytes: u64,
    now: &str,
) -> Result<String, MetadataError> {
    let override_id = format!(
        "budget_override_{}_{}",
        lease.id.as_str(),
        stable_token(&format!("{added_bytes}:{now}"))
    );
    store.grant_agent_lease_budget_override(lease, &override_id, added_bytes, now)?;
    append_budget_event(
        store,
        BudgetEvent {
            id: budget_event_id("override", &override_id, now),
            name: EventName::HydrationBudgetOverrideGranted,
            severity: EventSeverity::Info,
            summary: "Agent hydration budget override granted.",
            workspace_id: &lease.workspace_id,
            project_id: Some(&lease.project_id),
            lease_id: Some(&lease.id),
            path: Some("."),
            requested_bytes: added_bytes,
            affected_bytes: added_bytes,
            occurred_at: now,
        },
    );
    Ok(override_id)
}

#[derive(Debug, Clone)]
struct BudgetLedgerEventRow {
    workspace_id: WorkspaceId,
    project_id: Option<ProjectId>,
    lease_id: Option<LeaseId>,
    path: String,
    requested_bytes: u64,
}

fn budget_ledger_event_row(
    store: &MetadataStore,
    reservation_id: &str,
) -> Result<Option<BudgetLedgerEventRow>, MetadataError> {
    store
        .connection()
        .query_row(
            "SELECT workspace_id, project_id, lease_id, path, requested_bytes
             FROM hydration_budget_ledger
             WHERE id = ?1",
            params![reservation_id],
            |row| {
                let requested_bytes: i64 = row.get(4)?;
                Ok(BudgetLedgerEventRow {
                    workspace_id: WorkspaceId::new(row.get::<_, String>(0)?),
                    project_id: row.get::<_, Option<String>>(1)?.map(ProjectId::new),
                    lease_id: row.get::<_, Option<String>>(2)?.map(LeaseId::new),
                    path: row.get(3)?,
                    requested_bytes: requested_bytes.try_into().unwrap_or(0),
                })
            },
        )
        .optional()
        .map_err(Into::into)
}

struct BudgetEvent<'a> {
    id: EventId,
    name: EventName,
    severity: EventSeverity,
    summary: &'a str,
    workspace_id: &'a WorkspaceId,
    project_id: Option<&'a ProjectId>,
    lease_id: Option<&'a LeaseId>,
    path: Option<&'a str>,
    requested_bytes: u64,
    affected_bytes: u64,
    occurred_at: &'a str,
}

fn append_budget_event(store: &MetadataStore, event: BudgetEvent<'_>) {
    let mut workspace_event = WorkspaceEvent::new(
        event.id,
        event.name,
        event.occurred_at,
        event.severity,
        event.summary,
        event.workspace_id.clone(),
    );
    workspace_event.project_id = event.project_id.cloned();
    workspace_event.lease_id = event.lease_id.cloned();
    workspace_event.path = event.path.map(str::to_string);
    if let Some(lease_id) = event.lease_id {
        workspace_event.subject = Some(EventSubject {
            kind: EventSubjectKind::Lease,
            id: lease_id.as_str().to_string(),
            path: event.path.map(str::to_string),
        });
    }
    workspace_event.actor = Some(EventActor {
        kind: EventActorKind::System,
        id: None,
        display_name: None,
    });
    workspace_event
        .payload
        .insert("requestedBytes".to_string(), json!(event.requested_bytes));
    workspace_event
        .payload
        .insert("affectedBytes".to_string(), json!(event.affected_bytes));
    let _ = store.append_event(workspace_event);
}

fn budget_event_id(outcome: &str, reservation_id: &str, now: &str) -> EventId {
    EventId::new(format!(
        "evt_budget_{}_{}",
        outcome,
        stable_token(&format!("{reservation_id}:{now}"))
    ))
}

pub fn commit_queued_hydration(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    path: &str,
    cause: &str,
    committed_bytes: u64,
    now: &str,
) -> Result<bool, MetadataError> {
    let Some((queue_id, reservation_id)) = queued_hydration(store, workspace_id, path, cause)?
    else {
        return Ok(false);
    };
    if let Some(reservation_id) = reservation_id {
        commit_reservation(store, &reservation_id, committed_bytes, now)?;
    }
    store.connection().execute(
        "UPDATE hydration_queue
         SET state = 'completed', updated_at = ?3
         WHERE workspace_id = ?1 AND id = ?2 AND state = 'queued'",
        params![workspace_id.as_str(), queue_id, now],
    )?;
    Ok(true)
}

pub fn release_queued_hydration(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    path: &str,
    cause: &str,
    now: &str,
) -> Result<bool, MetadataError> {
    let Some((queue_id, reservation_id)) = queued_hydration(store, workspace_id, path, cause)?
    else {
        return Ok(false);
    };
    if let Some(reservation_id) = reservation_id {
        release_reservation(store, &reservation_id, now)?;
    }
    store.connection().execute(
        "UPDATE hydration_queue
         SET state = 'failed', updated_at = ?3
         WHERE workspace_id = ?1 AND id = ?2 AND state = 'queued'",
        params![workspace_id.as_str(), queue_id, now],
    )?;
    Ok(true)
}

pub fn reconcile_materialized_hydration_queue(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    now: &str,
) -> Result<usize, MetadataError> {
    let workspace_root = store.current_workspace_root()?.map(PathBuf::from);
    let mut settled = 0;
    for record in store.hydration_queue(workspace_id)? {
        if record.state != "queued"
            || !matches!(
                record.cause.as_str(),
                "agent-lease" | "hot-project-prefetch"
            )
        {
            continue;
        }
        let Some(path) = materialized_queue_path(workspace_root.as_ref(), &record.path) else {
            continue;
        };
        let metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => {
                if release_queued_hydration(store, workspace_id, &record.path, &record.cause, now)?
                {
                    settled += 1;
                }
                continue;
            }
        };
        if !metadata.is_file() {
            continue;
        }
        if commit_queued_hydration(
            store,
            workspace_id,
            &record.path,
            &record.cause,
            metadata.len(),
            now,
        )? {
            settled += 1;
        }
    }
    Ok(settled)
}

fn materialized_queue_path(workspace_root: Option<&PathBuf>, path: &str) -> Option<PathBuf> {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        return Some(path);
    }
    workspace_root.map(|root| root.join(path))
}

fn queued_hydration(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    path: &str,
    cause: &str,
) -> Result<Option<(String, Option<String>)>, MetadataError> {
    let path = store.workspace_relative_path(workspace_id, path)?;
    let queue_id = store
        .connection()
        .query_row(
            "SELECT id
             FROM hydration_queue
             WHERE workspace_id = ?1 AND path = ?2 AND cause = ?3 AND state = 'queued'
             LIMIT 1",
            params![workspace_id.as_str(), path, cause],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    Ok(queue_id.map(|queue_id| {
        let reservation_id = queue_id
            .strip_prefix("hydrate_")
            .map(std::string::ToString::to_string);
        (queue_id, reservation_id)
    }))
}

fn budget_status(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    project_id: Option<&ProjectId>,
    lease_id: Option<&LeaseId>,
    limit_bytes: u64,
) -> Result<HydrationBudgetStatus, MetadataError> {
    let used = sum_budget(
        store,
        workspace_id,
        lease_id,
        "committed",
        "committed_bytes",
    )?;
    let reserved = sum_budget(store, workspace_id, lease_id, "reserved", "reserved_bytes")?;
    let remaining = limit_bytes.saturating_sub(used.saturating_add(reserved));
    let state = if limit_bytes == 0 {
        HydrationBudgetState::Unavailable
    } else if remaining == 0 {
        HydrationBudgetState::Exhausted
    } else {
        HydrationBudgetState::Available
    };
    Ok(HydrationBudgetStatus {
        state,
        limit_bytes,
        used_bytes: used,
        reserved_bytes: reserved,
        remaining_bytes: remaining,
        scope: HydrationBudgetScope::Lease,
        lease_id: lease_id.cloned(),
        project_id: project_id.cloned(),
        reset_at: None,
        next_action: (state == HydrationBudgetState::Exhausted)
            .then(|| increase_budget_action(lease_id)),
    })
}

fn increase_budget_action(lease_id: Option<&LeaseId>) -> SafeAction {
    let command = lease_id
        .map(|id| format!("bowline agent budget --lease {} --add 64MiB", id.as_str()))
        .unwrap_or_else(|| {
            "bowline agent start <project> --task <task> --hydrate-budget <bytes>".to_string()
        });
    SafeAction {
        label: "Increase agent hydration budget".to_string(),
        command: Some(command),
    }
}

fn sum_budget(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    lease_id: Option<&LeaseId>,
    outcome: &str,
    column: &str,
) -> Result<u64, MetadataError> {
    let query = format!(
        "SELECT COALESCE(SUM({column}), 0)
         FROM hydration_budget_ledger
         WHERE workspace_id = ?1 AND (?2 IS NULL OR lease_id = ?2) AND outcome = ?3"
    );
    let value: i64 = store.connection().query_row(
        &query,
        params![
            workspace_id.as_str(),
            lease_id.map(|id| id.as_str()),
            outcome
        ],
        |row| row.get(0),
    )?;
    Ok(value.try_into().unwrap_or(0))
}

fn stable_token(input: &str) -> String {
    let hash = blake3::hash(input.as_bytes());
    hash.to_hex().chars().take(16).collect()
}

#[cfg(test)]
mod tests {
    use bowline_core::ids::{LeaseId, ProjectId, WorkspaceId};

    use crate::{
        hydration_budget::{
            HydrationBudgetReservationRequest, commit_queued_hydration, release_queued_hydration,
            reserve_lease_bytes,
        },
        metadata::{HydrationQueueRecord, MetadataStore},
        workspace::TempWorkspace,
    };

    #[test]
    fn reservations_are_atomic_against_remaining_budget() {
        let temp = TempWorkspace::new("budget-ledger").expect("temp");
        let db_path = temp.root().join(".state").join("local.sqlite3");
        let mut store = MetadataStore::open(db_path).expect("store");
        let workspace_id = WorkspaceId::new("ws_code");
        let project_id = ProjectId::new("proj_web");
        let lease_id = LeaseId::new("lease_auth");
        store
            .insert_workspace(&workspace_id, "Theo Code", "2026-06-25T00:00:00Z")
            .expect("workspace");
        store
            .insert_root(
                "root_code",
                &workspace_id,
                temp.root().to_str().expect("root path"),
                "2026-06-25T00:00:00Z",
            )
            .expect("root");
        store
            .insert_project(
                &project_id,
                &workspace_id,
                "root_code",
                temp.root().to_str().expect("project path"),
                "2026-06-25T00:00:00Z",
            )
            .expect("project");

        let first = reserve_lease_bytes(
            &mut store,
            HydrationBudgetReservationRequest {
                workspace_id: &workspace_id,
                project_id: &project_id,
                lease_id: &lease_id,
                path: "src/a.ts",
                content_id: Some("cid_a"),
                cause: "agent-lease",
                requested_bytes: 8,
                limit_bytes: 10,
                now: "2026-06-25T00:00:01Z",
            },
        )
        .expect("first reservation");
        assert!(first.accepted);
        assert_eq!(first.status.remaining_bytes, 2);

        let second = reserve_lease_bytes(
            &mut store,
            HydrationBudgetReservationRequest {
                workspace_id: &workspace_id,
                project_id: &project_id,
                lease_id: &lease_id,
                path: "src/b.ts",
                content_id: Some("cid_b"),
                cause: "agent-lease",
                requested_bytes: 3,
                limit_bytes: 10,
                now: "2026-06-25T00:00:02Z",
            },
        )
        .expect("second reservation");
        assert!(!second.accepted);
        assert_eq!(second.status.remaining_bytes, 2);
    }

    #[test]
    fn queued_hydration_completion_commits_or_releases_reserved_budget() {
        let temp = TempWorkspace::new("budget-queue-transition").expect("temp");
        let db_path = temp.root().join(".state").join("local.sqlite3");
        let mut store = MetadataStore::open(db_path).expect("store");
        let workspace_id = WorkspaceId::new("ws_code");
        let project_id = ProjectId::new("proj_web");
        let lease_id = LeaseId::new("lease_auth");
        store
            .insert_workspace(&workspace_id, "Theo Code", "2026-06-25T00:00:00Z")
            .expect("workspace");
        store
            .insert_root(
                "root_code",
                &workspace_id,
                temp.root().to_str().expect("root path"),
                "2026-06-25T00:00:00Z",
            )
            .expect("root");
        store
            .insert_project(
                &project_id,
                &workspace_id,
                "root_code",
                temp.root().to_str().expect("project path"),
                "2026-06-25T00:00:00Z",
            )
            .expect("project");

        let committed = reserve_lease_bytes(
            &mut store,
            HydrationBudgetReservationRequest {
                workspace_id: &workspace_id,
                project_id: &project_id,
                lease_id: &lease_id,
                path: "src/a.ts",
                content_id: Some("cid_a"),
                cause: "agent-lease",
                requested_bytes: 8,
                limit_bytes: 20,
                now: "2026-06-25T00:00:01Z",
            },
        )
        .expect("reservation");
        store
            .enqueue_hydration(&HydrationQueueRecord {
                id: format!("hydrate_{}", committed.reservation_id),
                workspace_id: workspace_id.clone(),
                project_id: Some(project_id.clone()),
                path: "src/a.ts".to_string(),
                content_id: Some(bowline_core::ids::ContentId::new("cid_a")),
                priority: "agent-lease".to_string(),
                state: "queued".to_string(),
                cause: "agent-lease".to_string(),
                updated_at: "2026-06-25T00:00:01Z".to_string(),
            })
            .expect("queue");

        let released = reserve_lease_bytes(
            &mut store,
            HydrationBudgetReservationRequest {
                workspace_id: &workspace_id,
                project_id: &project_id,
                lease_id: &lease_id,
                path: "src/b.ts",
                content_id: Some("cid_b"),
                cause: "agent-lease",
                requested_bytes: 4,
                limit_bytes: 20,
                now: "2026-06-25T00:00:02Z",
            },
        )
        .expect("reservation");
        store
            .enqueue_hydration(&HydrationQueueRecord {
                id: format!("hydrate_{}", released.reservation_id),
                workspace_id: workspace_id.clone(),
                project_id: Some(project_id.clone()),
                path: "src/b.ts".to_string(),
                content_id: Some(bowline_core::ids::ContentId::new("cid_b")),
                priority: "agent-lease".to_string(),
                state: "queued".to_string(),
                cause: "agent-lease".to_string(),
                updated_at: "2026-06-25T00:00:02Z".to_string(),
            })
            .expect("queue");

        assert!(
            commit_queued_hydration(
                &store,
                &workspace_id,
                "src/a.ts",
                "agent-lease",
                8,
                "2026-06-25T00:00:03Z",
            )
            .expect("commit")
        );
        assert!(
            release_queued_hydration(
                &store,
                &workspace_id,
                "src/b.ts",
                "agent-lease",
                "2026-06-25T00:00:04Z",
            )
            .expect("release")
        );
        let status = super::lease_budget_status(&store, &workspace_id, &project_id, &lease_id, 20)
            .expect("status");
        assert_eq!(status.used_bytes, 8);
        assert_eq!(status.reserved_bytes, 0);
        let queue = store.hydration_queue(&workspace_id).expect("queue");
        assert!(queue.iter().any(|record| record.state == "completed"));
        assert!(queue.iter().any(|record| record.state == "failed"));
    }

    #[test]
    fn hot_project_prefetch_completion_does_not_require_lease_budget_reservation() {
        let temp = TempWorkspace::new("budget-hot-prefetch-queue").expect("temp");
        let db_path = temp.root().join(".state").join("local.sqlite3");
        let store = MetadataStore::open(db_path).expect("store");
        let workspace_id = WorkspaceId::new("ws_code");
        let project_id = ProjectId::new("proj_web");
        store
            .insert_workspace(&workspace_id, "Theo Code", "2026-06-26T13:00:00Z")
            .expect("workspace");
        store
            .insert_root(
                "root_code",
                &workspace_id,
                temp.root().to_str().expect("root path"),
                "2026-06-26T13:00:00Z",
            )
            .expect("root");
        store
            .insert_project(
                &project_id,
                &workspace_id,
                "root_code",
                "app",
                "2026-06-26T13:00:00Z",
            )
            .expect("project");
        store
            .enqueue_hydration(&HydrationQueueRecord {
                id: "prefetch_src".to_string(),
                workspace_id: workspace_id.clone(),
                project_id: Some(project_id),
                path: "app/src/main.ts".to_string(),
                content_id: Some(bowline_core::ids::ContentId::new("cid_source")),
                priority: "hot-project-prefetch".to_string(),
                state: "queued".to_string(),
                cause: "hot-project-prefetch".to_string(),
                updated_at: "2026-06-26T13:00:01Z".to_string(),
            })
            .expect("queue");

        assert!(
            commit_queued_hydration(
                &store,
                &workspace_id,
                "app/src/main.ts",
                "hot-project-prefetch",
                24,
                "2026-06-26T13:00:02Z",
            )
            .expect("complete hot prefetch")
        );
        let queue = store.hydration_queue(&workspace_id).expect("queue");
        assert_eq!(queue[0].state, "completed");
    }
}
