use std::{error::Error, fmt};

use bowline_core::{
    events::{
        EventActor, EventRedaction, EventRedactionStatus, EventSeverity, EventSubject,
        WorkspaceEvent,
    },
    ids::{EventId, ProjectId, WorkspaceId},
    status::EventWatermarks,
    workspace_graph::normalize_workspace_path,
};
use rusqlite::{OptionalExtension, ToSql, params, params_from_iter};
use serde_json::{Map, Value};

use crate::metadata::{MetadataError, MetadataStore};

#[derive(Debug)]
pub enum LocalEventError {
    Metadata(MetadataError),
    Sqlite(rusqlite::Error),
    Json(serde_json::Error),
    DuplicateEventId(EventId),
}

#[derive(Debug, Clone)]
pub struct EventQuery {
    pub workspace_id: Option<WorkspaceId>,
    pub project_id: Option<ProjectId>,
    pub path_prefix: Option<String>,
    pub limit: u32,
}

const STATUS_CLEAR_EVENT_NAMES: &[&str] = &[
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
    "index.updated",
    "sync.completed",
    "sync.recovered",
    "watcher.recovered",
    "network.recovered",
    "work.accepted",
    "work.archived",
    "work.cleanup_completed",
    "work.discarded",
    "work.restored",
];

impl MetadataStore {
    pub fn append_event(
        &self,
        event: impl Into<WorkspaceEvent>,
    ) -> Result<WorkspaceEvent, LocalEventError> {
        let event = self.sanitize_event(event.into())?;
        let name = json_variant_string(&event.name)?;
        let severity = json_variant_string(&event.severity)?;
        let subject_json = optional_json(&event.subject)?;
        let actor_json = optional_json(&event.actor)?;
        let payload_json = serde_json::to_string(&event.payload)?;
        let redaction_json = serde_json::to_string(&event.redaction)?;

        let result = self.connection().execute(
            "INSERT INTO events
             (id, schema_version, name, occurred_at, severity, summary, workspace_id,
              project_id, path, lease_id, device_id, subject_json, actor_json,
              payload_json, causation_id, correlation_id, redaction_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
            params![
                event.id.as_str(),
                event.schema_version,
                name,
                event.occurred_at,
                severity,
                event.summary,
                event.workspace_id.as_str(),
                event.project_id.as_ref().map(|id| id.as_str()),
                event.path.as_deref(),
                event.lease_id.as_ref().map(|id| id.as_str()),
                event.device_id.as_ref().map(|id| id.as_str()),
                subject_json,
                actor_json,
                payload_json,
                event.causation_id.as_ref().map(|id| id.as_str()),
                event.correlation_id.as_ref().map(|id| id.as_str()),
                redaction_json,
            ],
        );
        match result {
            Ok(_) => {}
            Err(error) if is_constraint_error(&error) => {
                return Err(LocalEventError::DuplicateEventId(event.id.clone()));
            }
            Err(error) => return Err(error.into()),
        }

        Ok(event)
    }

    pub fn list_events(&self, limit: u32) -> Result<Vec<WorkspaceEvent>, LocalEventError> {
        self.list_events_scoped(EventQuery {
            workspace_id: None,
            project_id: None,
            path_prefix: None,
            limit,
        })
    }

    pub fn list_events_scoped(
        &self,
        query: EventQuery,
    ) -> Result<Vec<WorkspaceEvent>, LocalEventError> {
        let (sql, params) = scoped_events_sql(&query, false);
        let mut statement = self.connection().prepare(&sql)?;
        let rows = statement.query_map(
            params_from_iter(params.iter().map(|value| &**value)),
            row_to_event,
        )?;

        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn list_status_signal_events_scoped(
        &self,
        query: EventQuery,
    ) -> Result<Vec<WorkspaceEvent>, LocalEventError> {
        let (sql, params) = scoped_status_signal_events_sql(&query);
        let mut statement = self.connection().prepare(&sql)?;
        let rows = statement.query_map(
            params_from_iter(params.iter().map(|value| &**value)),
            row_to_event,
        )?;

        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn scoped_event_watermarks(
        &self,
        query: EventQuery,
    ) -> Result<EventWatermarks, LocalEventError> {
        let mut watermarks = self.event_watermarks()?;
        watermarks.last_event_id = self.latest_event_id_scoped(query)?;
        Ok(watermarks)
    }

    fn latest_event_id_scoped(
        &self,
        query: EventQuery,
    ) -> Result<Option<EventId>, LocalEventError> {
        let (sql, params) = scoped_events_sql(&EventQuery { limit: 1, ..query }, true);
        let mut statement = self.connection().prepare(&sql)?;
        let id = statement
            .query_row(
                params_from_iter(params.iter().map(|value| &**value)),
                |row| row.get::<_, String>(0),
            )
            .optional()?;

        Ok(id.map(EventId::new))
    }

    fn sanitize_event(&self, mut event: WorkspaceEvent) -> Result<WorkspaceEvent, LocalEventError> {
        let mut rules = match event.redaction.status {
            EventRedactionStatus::Applied => event.redaction.rules.clone(),
            EventRedactionStatus::NotNeeded => Vec::new(),
        };
        let workspace_id = event.workspace_id.clone();
        event.summary = redact_text(&event.summary, &mut rules);
        event.path = event
            .path
            .as_deref()
            .map(|path| sanitize_path(self, &workspace_id, path, &mut rules))
            .transpose()?;
        event.subject = event
            .subject
            .map(|subject| sanitize_subject(self, &workspace_id, subject, &mut rules))
            .transpose()?;
        event.actor = event.actor.map(|actor| sanitize_actor(actor, &mut rules));
        redact_payload(&mut event.payload, &mut rules);

        if rules.is_empty() {
            event.redaction = EventRedaction::not_needed();
        } else {
            rules.sort();
            rules.dedup();
            event.redaction = EventRedaction::applied(rules);
        }

        Ok(event)
    }
}

impl fmt::Display for LocalEventError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Metadata(error) => error.fmt(formatter),
            Self::Sqlite(error) => write!(formatter, "local event SQLite failed: {error}"),
            Self::Json(error) => write!(formatter, "local event JSON failed: {error}"),
            Self::DuplicateEventId(id) => {
                write!(formatter, "event id {} already exists", id.as_str())
            }
        }
    }
}

impl Error for LocalEventError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Metadata(error) => Some(error),
            Self::Sqlite(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::DuplicateEventId(_) => None,
        }
    }
}

impl From<MetadataError> for LocalEventError {
    fn from(error: MetadataError) -> Self {
        Self::Metadata(error)
    }
}

impl From<rusqlite::Error> for LocalEventError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

impl From<serde_json::Error> for LocalEventError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

fn row_to_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkspaceEvent> {
    let name: String = row.get(2)?;
    let severity: String = row.get(4)?;
    let subject_json: Option<String> = row.get(11)?;
    let actor_json: Option<String> = row.get(12)?;
    let payload_json: String = row.get(13)?;
    let redaction_json: String = row.get(16)?;

    Ok(WorkspaceEvent {
        id: bowline_core::ids::EventId::new(row.get::<_, String>(0)?),
        schema_version: row.get::<_, u16>(1)?,
        name: parse_json_variant(&name)?,
        occurred_at: row.get(3)?,
        severity: parse_json_variant::<EventSeverity>(&severity)?,
        summary: row.get(5)?,
        workspace_id: bowline_core::ids::WorkspaceId::new(row.get::<_, String>(6)?),
        project_id: row
            .get::<_, Option<String>>(7)?
            .map(bowline_core::ids::ProjectId::new),
        path: row.get(8)?,
        lease_id: row
            .get::<_, Option<String>>(9)?
            .map(bowline_core::ids::LeaseId::new),
        device_id: row
            .get::<_, Option<String>>(10)?
            .map(bowline_core::ids::DeviceId::new),
        subject: parse_optional_json(&subject_json)?,
        actor: parse_optional_json(&actor_json)?,
        payload: serde_json::from_str::<Map<String, Value>>(&payload_json)
            .map_err(json_to_sql_error)?,
        causation_id: row
            .get::<_, Option<String>>(14)?
            .map(bowline_core::ids::EventId::new),
        correlation_id: row
            .get::<_, Option<String>>(15)?
            .map(bowline_core::ids::EventId::new),
        redaction: serde_json::from_str(&redaction_json).map_err(json_to_sql_error)?,
    })
}

fn redact_payload(payload: &mut Map<String, Value>, rules: &mut Vec<String>) {
    for (key, value) in payload {
        if is_sensitive_key(key) {
            *value = Value::String("[redacted]".to_string());
            rules.push("secret-values".to_string());
        } else {
            redact_value(value, rules);
        }
    }
}

fn redact_value(value: &mut Value, rules: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if is_sensitive_key(key) {
                    *value = Value::String("[redacted]".to_string());
                    rules.push("secret-values".to_string());
                } else {
                    redact_value(value, rules);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                redact_value(value, rules);
            }
        }
        Value::String(text) => {
            let redacted = redact_text(text, rules);
            *text = redacted;
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn redact_text(text: &str, rules: &mut Vec<String>) -> String {
    let mut redacted = text.to_string();
    if looks_like_secret(text) {
        redacted = redact_secret_fragments(&redacted);
        rules.push("secret-values".to_string());
    }
    let home_redacted = redact_home_paths(&redacted);
    if home_redacted != redacted {
        redacted = home_redacted;
        rules.push("absolute-home-paths".to_string());
    }
    redacted
}

fn redact_home_paths(text: &str) -> String {
    let redacted = replace_absolute_home_marker(text, "/Users/");
    let redacted = replace_absolute_home_marker(&redacted, "/home/");
    let redacted = replace_normalized_home_prefix(&redacted, "Users/");
    replace_normalized_home_prefix(&redacted, "home/")
}

fn replace_absolute_home_marker(text: &str, marker: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut remaining = text;

    while let Some(index) = remaining.find(marker) {
        output.push_str(&remaining[..index]);
        output.push('~');

        let after_marker = &remaining[index + marker.len()..];
        remaining = after_user_segment(after_marker);
    }

    output.push_str(remaining);
    output
}

fn replace_normalized_home_prefix(text: &str, marker: &str) -> String {
    let Some(after_marker) = text.strip_prefix(marker) else {
        return text.to_string();
    };

    format!("~{}", after_user_segment(after_marker))
}

fn after_user_segment(path: &str) -> &str {
    path.find('/').map_or("", |index| &path[index..])
}

fn is_sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    let compact_key = key
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .collect::<String>();
    key.contains("secret")
        || key.contains("token")
        || key.contains("password")
        || key.contains("credential")
        || (key.contains("private") && key.contains("key"))
        || (key.contains("ssh") && key.contains("key"))
        || key.contains("api_key")
        || key.contains("apikey")
        || compact_key.contains("apikey")
        || compact_key.contains("privatekey")
        || compact_key.contains("sshkey")
        || compact_key.contains("authtoken")
        || compact_key.contains("authorization")
        || compact_key == "auth"
        || compact_key == "jwt"
        || key == "key"
}

fn looks_like_secret(text: &str) -> bool {
    let upper = text.to_ascii_uppercase();
    let lower = text.to_ascii_lowercase();
    ((upper.contains("SK-")
        || upper.contains("TOKEN_")
        || upper.contains("SECRET_")
        || upper.contains("BEARER ")
        || (upper.contains("-----BEGIN ") && upper.contains("PRIVATE KEY"))
        || upper.contains("OPENSSH PRIVATE KEY")
        || upper.contains("AKIA")
        || lower.contains("ghp_")
        || lower.contains("github_pat_")
        || lower.contains("xoxb-")
        || lower.contains("xoxp-"))
        || looks_like_jwt(text))
        && text.len() >= 12
}

fn looks_like_jwt(text: &str) -> bool {
    text.split(|character: char| character.is_ascii_whitespace() || character == ':')
        .any(is_jwt_like_token)
}

fn is_jwt_like_token(token: &str) -> bool {
    let token = token.trim_matches(|character: char| {
        matches!(character, '"' | '\'' | ',' | ';' | ')' | ']' | '}')
    });
    if token.len() < 20 || !token.starts_with("eyJ") {
        return false;
    }

    let segments = token.split('.').collect::<Vec<_>>();
    if segments.len() > 1 && segments.len() != 3 {
        return false;
    }

    segments.iter().all(|segment| {
        !segment.is_empty()
            && segment.chars().all(|character| {
                character.is_ascii_alphanumeric() || character == '-' || character == '_'
            })
    })
}

fn redact_secret_fragments(text: &str) -> String {
    if text.to_ascii_lowercase().contains("bearer ") {
        return "[redacted]".to_string();
    }

    if text.split_whitespace().count() <= 1 {
        return "[redacted]".to_string();
    }

    text.split_whitespace()
        .map(|part| {
            if looks_like_secret(part) {
                "[redacted]"
            } else {
                part
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn json_variant_string<T: serde::Serialize>(value: &T) -> Result<String, LocalEventError> {
    serde_json::to_value(value)?
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| serde_json::Error::io(std::io::Error::other("variant was not a string")))
        .map_err(Into::into)
}

fn parse_json_variant<T: serde::de::DeserializeOwned>(value: &str) -> rusqlite::Result<T> {
    serde_json::from_value(Value::String(value.to_string())).map_err(json_to_sql_error)
}

fn optional_json<T: serde::Serialize>(
    value: &Option<T>,
) -> Result<Option<String>, LocalEventError> {
    value
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(Into::into)
}

fn parse_optional_json<T: serde::de::DeserializeOwned>(
    value: &Option<String>,
) -> rusqlite::Result<Option<T>> {
    value
        .as_ref()
        .map(|json| serde_json::from_str(json).map_err(json_to_sql_error))
        .transpose()
}

fn json_to_sql_error(error: serde_json::Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
}

fn sanitize_subject(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    mut subject: EventSubject,
    rules: &mut Vec<String>,
) -> Result<EventSubject, LocalEventError> {
    subject.id = redact_text(&subject.id, rules);
    subject.path = subject
        .path
        .as_deref()
        .map(|path| sanitize_path(store, workspace_id, path, rules))
        .transpose()?;
    Ok(subject)
}

fn sanitize_actor(mut actor: EventActor, rules: &mut Vec<String>) -> EventActor {
    actor.id = actor.id.as_deref().map(|id| redact_text(id, rules));
    actor.display_name = actor
        .display_name
        .as_deref()
        .map(|display_name| redact_text(display_name, rules));
    actor
}

fn sanitize_path(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    path: &str,
    rules: &mut Vec<String>,
) -> Result<String, LocalEventError> {
    let relative = store.workspace_relative_path(workspace_id, path)?;
    if relative != normalize_workspace_path(path) {
        rules.push("workspace-relative-paths".to_string());
        return Ok(redact_text(&relative, rules));
    }

    let redacted_path = redact_text(path, rules);
    let redacted_relative = store.workspace_relative_path(workspace_id, &redacted_path)?;
    if redacted_relative != normalize_workspace_path(&redacted_path) {
        rules.push("workspace-relative-paths".to_string());
    }
    Ok(redact_text(&redacted_relative, rules))
}

fn scoped_events_sql(query: &EventQuery, select_id_only: bool) -> (String, Vec<Box<dyn ToSql>>) {
    let mut sql = format!("{} FROM events", event_select(select_id_only));
    let mut clauses = Vec::new();
    let mut params: Vec<Box<dyn ToSql>> = Vec::new();

    add_scope_clauses(query, &mut clauses, &mut params);
    append_where_clauses(&mut sql, &clauses);
    sql.push_str(" ORDER BY occurred_at DESC, id DESC LIMIT ?");
    params.push(Box::new(i64::from(query.limit)));

    (sql, params)
}

fn scoped_status_signal_events_sql(query: &EventQuery) -> (String, Vec<Box<dyn ToSql>>) {
    let mut sql = format!("{} FROM events", event_select(false));
    let mut clauses = Vec::new();
    let mut params: Vec<Box<dyn ToSql>> = Vec::new();

    add_scope_clauses(query, &mut clauses, &mut params);
    let placeholders = vec!["?"; STATUS_CLEAR_EVENT_NAMES.len()].join(", ");
    clauses.push(format!("(severity != 'info' OR name IN ({placeholders}))"));
    for name in STATUS_CLEAR_EVENT_NAMES {
        params.push(Box::new((*name).to_string()));
    }

    append_where_clauses(&mut sql, &clauses);
    sql.push_str(" ORDER BY occurred_at DESC, id DESC");

    (sql, params)
}

fn event_select(select_id_only: bool) -> &'static str {
    if select_id_only {
        "SELECT id"
    } else {
        "SELECT id, schema_version, name, occurred_at, severity, summary,
                workspace_id, project_id, path, lease_id, device_id, subject_json,
                actor_json, payload_json, causation_id, correlation_id, redaction_json"
    }
}

fn add_scope_clauses(
    query: &EventQuery,
    clauses: &mut Vec<String>,
    params: &mut Vec<Box<dyn ToSql>>,
) {
    if let Some(workspace_id) = &query.workspace_id {
        clauses.push("workspace_id = ?".to_string());
        params.push(Box::new(workspace_id.as_str().to_string()));
    }
    match (&query.project_id, &query.path_prefix) {
        (Some(project_id), Some(path_prefix)) => {
            clauses.push("(project_id = ? OR path = ? OR instr(path, ?) = 1)".to_string());
            params.push(Box::new(project_id.as_str().to_string()));
            push_path_prefix_params(path_prefix, params);
        }
        (Some(project_id), None) => {
            clauses.push("project_id = ?".to_string());
            params.push(Box::new(project_id.as_str().to_string()));
        }
        (None, Some(path_prefix)) => {
            clauses.push("(path = ? OR instr(path, ?) = 1)".to_string());
            push_path_prefix_params(path_prefix, params);
        }
        (None, None) => {}
    }
}

fn push_path_prefix_params(path_prefix: &str, params: &mut Vec<Box<dyn ToSql>>) {
    params.push(Box::new(path_prefix.to_string()));
    params.push(Box::new(format!("{path_prefix}/")));
}

fn append_where_clauses(sql: &mut String, clauses: &[String]) {
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }
}

fn is_constraint_error(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(sqlite_error, _)
            if sqlite_error.code == rusqlite::ErrorCode::ConstraintViolation
    )
}

#[cfg(test)]
mod tests {
    use bowline_core::{
        events::{
            EventActor, EventActorKind, EventName, EventRedaction, EventRedactionStatus,
            EventSeverity, EventSubject, EventSubjectKind, WorkspaceEvent,
        },
        ids::{EventId, WorkspaceId},
    };
    use serde_json::json;

    use crate::{metadata::MetadataStore, workspace::TempWorkspace};

    #[test]
    fn events_survive_store_reopen() {
        let temp = TempWorkspace::new("events-reopen").expect("temp workspace");
        let db_path = temp.root().join("state").join("local.sqlite3");
        let workspace_id = WorkspaceId::new("ws_code");

        let store = MetadataStore::open(&db_path).expect("metadata opens");
        store
            .insert_workspace(&workspace_id, "User Code", "2026-06-23T12:00:00Z")
            .expect("workspace insert");
        store
            .append_event(test_event(&workspace_id))
            .expect("event append");
        drop(store);

        let reopened = MetadataStore::open(&db_path).expect("metadata reopens");
        let events = reopened.list_events(10).expect("events list");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id.as_str(), "evt_test_001");
    }

    #[test]
    fn event_append_redacts_secrets_and_absolute_home_paths() {
        let temp = TempWorkspace::new("events-redaction").expect("temp workspace");
        let db_path = temp.root().join("state").join("local.sqlite3");
        let workspace_id = WorkspaceId::new("ws_code");
        let project_id = bowline_core::ids::ProjectId::new("proj_acme");

        let store = MetadataStore::open(&db_path).expect("metadata opens");
        store
            .insert_workspace(&workspace_id, "User Code", "2026-06-23T12:00:00Z")
            .expect("workspace insert");
        store
            .insert_root(
                "root_code",
                &workspace_id,
                &["", "home", "user", "Code"].join("/"),
                "2026-06-23T12:00:00Z",
            )
            .expect("root insert");
        store
            .insert_project(
                &project_id,
                &workspace_id,
                "root_code",
                "acme",
                "2026-06-23T12:00:00Z",
            )
            .expect("project insert");

        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJ0aGVvIn0.signatureValue";
        let root_path = ["", "home", "user", "Code"].join("/");
        let other_home = ["", "home", "linux_box"].join("/");
        let event_path = format!("{root_path}/acme/.env.local");
        let mut event = test_event(&workspace_id);
        event.path = Some(event_path.clone());
        event.summary = format!("Read {root_path} with Bearer sk-test-secret-token");
        event.subject = Some(EventSubject {
            kind: EventSubjectKind::Path,
            id: format!("path-{event_path}"),
            path: Some(event_path),
        });
        event.actor = Some(EventActor {
            kind: EventActorKind::Agent,
            id: Some("agent TOKEN_super_secret".to_string()),
            display_name: Some(format!("User {other_home} should stay private")),
        });
        event.payload = json!({
            "OPENAI_API_KEY": "sk-test-secret-token",
            "privateKey": "-----BEGIN OPENSSH PRIVATE KEY-----\nabc123",
            "sshKey": "raw-ssh-private-key-material",
            "safe": format!("{other_home}/Code/acme"),
            "message": "Authorization Bearer sk-embedded-secret-token",
            "jwt": jwt,
            "authorization": jwt,
            "nested": {
                "auth": jwt
            }
        })
        .as_object()
        .expect("payload object")
        .clone();

        let stored = store.append_event(event).expect("event append");
        let raw_row: String = store
            .connection()
            .query_row(
                "SELECT path || ' ' || subject_json || ' ' || actor_json || ' ' || payload_json || ' ' || summary
                 FROM events WHERE id = 'evt_test_001'",
                [],
                |row| row.get(0),
            )
            .expect("raw row");

        assert_eq!(stored.path.as_deref(), Some("acme/.env.local"));
        assert!(!raw_row.contains("sk-test-secret-token"));
        assert!(!raw_row.contains("TOKEN_super_secret"));
        assert!(!raw_row.contains("eyJhbGciOiJIUzI1NiJ9"));
        assert!(!raw_row.contains("OPENSSH PRIVATE KEY"));
        assert!(!raw_row.contains("raw-ssh-private-key-material"));
        assert!(!raw_row.contains(&root_path));
        assert!(!raw_row.contains("Users/user"));
        assert!(!raw_row.contains("~/user"));
        assert!(!raw_row.contains(&other_home));
        assert!(!raw_row.contains("home/user"));
        assert!(matches!(
            stored.redaction.status,
            bowline_core::events::EventRedactionStatus::Applied
        ));
    }

    #[test]
    fn event_append_preserves_intentional_redaction_when_no_scrubbed_values_are_found() {
        let temp = TempWorkspace::new("events-declared-redaction").expect("temp workspace");
        let db_path = temp.root().join("state").join("local.sqlite3");
        let workspace_id = WorkspaceId::new("ws_code");

        let store = MetadataStore::open(&db_path).expect("metadata opens");
        store
            .insert_workspace(&workspace_id, "User Code", "2026-06-23T12:00:00Z")
            .expect("workspace insert");

        let mut event = test_event(&workspace_id);
        event.redaction = EventRedaction::applied(["secret-values-not-included"]);

        let stored = store.append_event(event).expect("event append");

        assert_eq!(stored.redaction.status, EventRedactionStatus::Applied);
        assert_eq!(
            stored.redaction.rules,
            vec!["secret-values-not-included".to_string()]
        );
    }

    #[test]
    fn event_path_redaction_matches_tilde_roots_before_fallback_normalization() {
        let temp = TempWorkspace::new("events-tilde-redaction").expect("temp workspace");
        let db_path = temp.root().join("state").join("local.sqlite3");
        let workspace_id = WorkspaceId::new("ws_code");
        let project_id = bowline_core::ids::ProjectId::new("proj_acme");

        let store = MetadataStore::open(&db_path).expect("metadata opens");
        store
            .insert_workspace(&workspace_id, "User Code", "2026-06-23T12:00:00Z")
            .expect("workspace insert");
        store
            .insert_root("root_code", &workspace_id, "~/Code", "2026-06-23T12:00:00Z")
            .expect("root insert");
        store
            .insert_project(
                &project_id,
                &workspace_id,
                "root_code",
                "acme",
                "2026-06-23T12:00:00Z",
            )
            .expect("project insert");

        let root_path = ["", "home", "user", "Code"].join("/");
        let event_path = format!("{root_path}/acme/.env.local");
        let mut event = test_event(&workspace_id);
        event.path = Some(event_path.clone());
        event.subject = Some(EventSubject {
            kind: EventSubjectKind::Path,
            id: format!("path-{event_path}"),
            path: Some(event_path.clone()),
        });
        event.summary = format!("Observed {event_path}");

        let stored = store.append_event(event).expect("event append");
        let raw_row: String = store
            .connection()
            .query_row(
                "SELECT path || ' ' || subject_json || ' ' || summary
                 FROM events WHERE id = 'evt_test_001'",
                [],
                |row| row.get(0),
            )
            .expect("raw row");

        assert_eq!(stored.path.as_deref(), Some("acme/.env.local"));
        assert!(!raw_row.contains(&root_path));
        assert!(!raw_row.contains("Users/user"));
        assert!(!raw_row.contains("~/user"));
    }

    #[test]
    fn duplicate_event_id_does_not_overwrite_existing_event() {
        let temp = TempWorkspace::new("events-duplicate").expect("temp workspace");
        let db_path = temp.root().join("state").join("local.sqlite3");
        let workspace_id = WorkspaceId::new("ws_code");

        let store = MetadataStore::open(&db_path).expect("metadata opens");
        store
            .insert_workspace(&workspace_id, "User Code", "2026-06-23T12:00:00Z")
            .expect("workspace insert");
        store
            .append_event(test_event(&workspace_id))
            .expect("event append");

        let mut duplicate = test_event(&workspace_id);
        duplicate.summary = "Different summary must not replace history.".to_string();

        let error = store.append_event(duplicate).expect_err("duplicate fails");
        assert!(matches!(error, super::LocalEventError::DuplicateEventId(_)));
        let events = store.list_events(10).expect("events list");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].summary, "Index updated.");
    }

    fn test_event(workspace_id: &WorkspaceId) -> WorkspaceEvent {
        let mut event = WorkspaceEvent::new(
            EventId::new("evt_test_001"),
            EventName::IndexUpdated,
            "2026-06-23T12:00:00Z",
            EventSeverity::Info,
            "Index updated.",
            workspace_id.clone(),
        );
        event.subject = Some(EventSubject {
            kind: EventSubjectKind::Metadata,
            id: "metadata-local".to_string(),
            path: None,
        });
        event.actor = Some(EventActor {
            kind: EventActorKind::Daemon,
            id: Some("daemon-local".to_string()),
            display_name: None,
        });
        event
    }
}
