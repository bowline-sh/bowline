//! Timeline entries for the conflict-aside lifecycle.
//!
//! A conflict-aside is a file, so `bowline status` and `bowline conflicts` read
//! the workspace rather than the event log. These events exist for the other
//! question — *when did this appear, and who decided it* — which only the
//! timeline can answer, and they are what clears a legacy conflict signal
//! through [`crate::status`]'s status-clear keys.

use bowline_core::{
    events::{EventName, EventSeverity, EventSubject, EventSubjectKind, WorkspaceEvent},
    ids::{EventId, ProjectId, WorkspaceId},
};

use crate::{conflicts::ConflictResolution, metadata::MetadataStore};

use super::LocalEventError;

/// Identity of one conflict for the timeline: the file that stayed canonical and
/// the aside beside it.
pub struct ConflictEventSubject<'a> {
    pub workspace_id: &'a WorkspaceId,
    pub project_id: Option<&'a ProjectId>,
    pub origin_path: &'a str,
    pub aside_path: &'a str,
    pub occurred_at: &'a str,
}

impl MetadataStore {
    /// Record that an aside now exists. Idempotent by construction: the event id
    /// is derived from the aside path, so replaying the same conflict appends
    /// once and later replays report a duplicate the caller can ignore.
    pub fn append_conflict_created(
        &self,
        subject: &ConflictEventSubject<'_>,
    ) -> Result<(), LocalEventError> {
        self.append_conflict_event(
            subject,
            EventName::ConflictCreated,
            EventSeverity::Attention,
            format!(
                "{} kept your version; the incoming one is preserved at {}.",
                subject.origin_path, subject.aside_path
            ),
        )
    }

    /// Record which side won. The two resolutions are distinct names, not a
    /// payload flag, because status clears a conflict signal by event name.
    pub fn append_conflict_resolved(
        &self,
        subject: &ConflictEventSubject<'_>,
        resolution: ConflictResolution,
    ) -> Result<(), LocalEventError> {
        let (name, summary) = match resolution {
            ConflictResolution::TakeRemote => (
                EventName::ConflictResolutionAccepted,
                format!("{} now holds the incoming version.", subject.origin_path),
            ),
            ConflictResolution::KeepLocal => (
                EventName::ConflictResolutionRejected,
                format!(
                    "{} kept your version; the incoming one was discarded.",
                    subject.origin_path
                ),
            ),
        };
        self.append_conflict_event(subject, name, EventSeverity::Info, summary)
    }

    fn append_conflict_event(
        &self,
        subject: &ConflictEventSubject<'_>,
        name: EventName,
        severity: EventSeverity,
        summary: String,
    ) -> Result<(), LocalEventError> {
        let mut event = WorkspaceEvent::new(
            conflict_event_id(&name, subject.aside_path),
            name,
            subject.occurred_at,
            severity,
            summary,
            subject.workspace_id.clone(),
        );
        event.project_id = subject.project_id.cloned();
        // The aside path is the key everything else agrees on: it is what status
        // stats on disk and what `bowline resolve` is given.
        event.path = Some(subject.aside_path.to_string());
        event.subject = Some(EventSubject {
            kind: EventSubjectKind::Conflict,
            id: subject.aside_path.to_string(),
            path: Some(subject.origin_path.to_string()),
        });
        match self.append_event(event) {
            Ok(_) | Err(LocalEventError::DuplicateEventId(_)) => Ok(()),
            Err(error) => Err(error),
        }
    }
}

/// Deterministic per-(name, aside) id so re-observing one conflict never grows
/// the timeline.
fn conflict_event_id(name: &EventName, aside_path: &str) -> EventId {
    let digest = blake3::hash(aside_path.as_bytes()).to_hex();
    let verb = match name {
        EventName::ConflictResolutionAccepted => "accepted",
        EventName::ConflictResolutionRejected => "rejected",
        _ => "created",
    };
    EventId::new(format!("evt_conflict_{verb}_{}", &digest[..24]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::TempWorkspace;

    fn subject<'a>(workspace_id: &'a WorkspaceId) -> ConflictEventSubject<'a> {
        ConflictEventSubject {
            workspace_id,
            project_id: None,
            origin_path: "acme/web/src/auth.ts",
            aside_path: "acme/web/src/auth.ts.bowline-conflict.deadbeef",
            occurred_at: "2026-06-23T12:00:00Z",
        }
    }

    fn store(name: &str, workspace_id: &WorkspaceId) -> (TempWorkspace, MetadataStore) {
        let temp = TempWorkspace::new(name).expect("temp workspace");
        let store = MetadataStore::open(temp.root().join("local.sqlite3")).expect("store");
        store
            .insert_workspace(workspace_id, "User Code", "2026-06-23T12:00:00Z")
            .expect("workspace insert");
        (temp, store)
    }

    #[test]
    fn recording_one_conflict_twice_appends_once() {
        let workspace_id = WorkspaceId::new("ws_conflict_events");
        let (_temp, store) = store("conflict-events-idempotent", &workspace_id);
        let subject = subject(&workspace_id);

        store.append_conflict_created(&subject).expect("first");
        store.append_conflict_created(&subject).expect("replay");

        let events = store.list_events(10).expect("events");
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].name, EventName::ConflictCreated);
        assert_eq!(events[0].severity, EventSeverity::Attention);
    }

    #[test]
    fn the_two_resolutions_are_distinct_event_names() {
        let workspace_id = WorkspaceId::new("ws_conflict_resolutions");
        let (_temp, store) = store("conflict-events-resolutions", &workspace_id);
        let subject = subject(&workspace_id);

        store
            .append_conflict_resolved(&subject, ConflictResolution::TakeRemote)
            .expect("take remote");
        store
            .append_conflict_resolved(&subject, ConflictResolution::KeepLocal)
            .expect("keep local");

        let names = store
            .list_events(10)
            .expect("events")
            .into_iter()
            .map(|event| event.name)
            .collect::<Vec<_>>();
        assert!(
            names.contains(&EventName::ConflictResolutionAccepted),
            "{names:?}"
        );
        assert!(
            names.contains(&EventName::ConflictResolutionRejected),
            "{names:?}"
        );
    }
}
