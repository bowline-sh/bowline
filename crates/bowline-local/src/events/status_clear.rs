use bowline_core::events::EventName;
use rusqlite::ToSql;

use super::{EventQuery, add_scope_clauses, append_where_clauses, event_name_label, event_select};

const STATUS_CLEAR_EVENTS: &[EventName] = &[
    EventName::ConflictResolutionAccepted,
    EventName::ConflictResolutionRejected,
    EventName::DeviceApproved,
    EventName::DeviceRevoked,
    EventName::SetupCompleted,
    EventName::HydrationCompleted,
    EventName::PolicyChanged,
    EventName::LeaseCreated,
    EventName::LeaseUpdated,
    EventName::DaemonRecovered,
    EventName::SyncCompleted,
    EventName::SyncRecovered,
    EventName::WatcherRecovered,
    EventName::NetworkRecovered,
    EventName::WorkAccepted,
    EventName::WorkCleanupCompleted,
    EventName::WorkDiscarded,
    EventName::WorkRestored,
];

pub(super) fn scoped_status_signal_events_sql(query: &EventQuery) -> (String, Vec<Box<dyn ToSql>>) {
    let mut sql = format!("{} FROM events", event_select(false));
    let mut clauses = Vec::new();
    let mut params: Vec<Box<dyn ToSql>> = Vec::new();

    add_scope_clauses(query, &mut clauses, &mut params);
    let placeholders = vec!["?"; STATUS_CLEAR_EVENTS.len()].join(", ");
    clauses.push(format!("(severity != 'info' OR name IN ({placeholders}))"));
    for name in STATUS_CLEAR_EVENTS {
        params.push(Box::new(event_name_label(name)));
    }

    append_where_clauses(&mut sql, &clauses);
    sql.push_str(" ORDER BY occurred_at DESC, id DESC");

    (sql, params)
}

#[cfg(test)]
mod tests {
    use super::{
        EventQuery, STATUS_CLEAR_EVENTS, event_name_label, scoped_status_signal_events_sql,
    };

    #[test]
    fn status_clear_events_derive_canonical_wire_names() {
        let names = STATUS_CLEAR_EVENTS
            .iter()
            .map(event_name_label)
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                "conflict.resolution_accepted",
                "conflict.resolution_rejected",
                "device.approved",
                "device.revoked",
                "setup.completed",
                "hydration.completed",
                "policy.changed",
                "lease.created",
                "lease.updated",
                "daemon.recovered",
                "sync.completed",
                "sync.recovered",
                "watcher.recovered",
                "network.recovered",
                "work.accepted",
                "work.cleanup_completed",
                "work.discarded",
                "work.restored",
            ]
        );
        let (_, params) = scoped_status_signal_events_sql(&EventQuery {
            workspace_id: None,
            project_id: None,
            path_prefix: None,
            limit: 10,
        });
        assert_eq!(params.len(), STATUS_CLEAR_EVENTS.len());
    }
}
