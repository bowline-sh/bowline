use super::*;

pub fn command_error_output(
    command: CommandName,
    generated_at: String,
    code: impl Into<String>,
    message: impl Into<String>,
    recoverability: CommandRecoverability,
) -> CommandErrorOutput {
    CommandErrorOutput {
        contract_version: CONTRACT_VERSION,
        command,
        generated_at,
        status: CommandErrorStatus::Failed,
        error: CommandError {
            code: code.into(),
            message: message.into(),
            recoverability,
            remediation: None,
            details: None,
            retry_after_seconds: None,
            correlation_id: None,
        },
        next_actions: Vec::new(),
    }
}

/// Map a fully composed [`StatusCommandOutput`] into the redacted snapshot the
/// daemon publishes to the control plane. Counts, state enums, and timestamps
/// are preserved; every `path` is reduced to a workspace-relative form, and
/// anything that still looks like an absolute filesystem path or an env file is
/// dropped so secrets and local layout never leave the device.
///
/// `syncState`/`watcherState`/`networkState` reflect whatever component states
/// `compose_status` observed; the daemon may overwrite them with its live
/// in-memory states before publishing.
pub fn redacted_status_snapshot(
    output: &StatusCommandOutput,
    device_id: &str,
) -> WorkspaceStatusSnapshot {
    let workspace_root = output.resolved_workspace_root.as_deref();

    let event_watermarks = StatusEventWatermarks {
        last_event_id: output
            .event_watermarks
            .last_event_id
            .as_ref()
            .map(|id| id.as_str().to_string()),
        last_scan_at: output.event_watermarks.last_scan_at.clone(),
        sync_state: output
            .event_watermarks
            .sync_state
            .map(|state| component_state_label(state).to_string()),
        watcher_state: output
            .event_watermarks
            .watcher_state
            .map(|state| component_state_label(state).to_string()),
        network_state: output
            .event_watermarks
            .network_state
            .map(|state| network_state_label(state).to_string()),
    };

    let sync_queue = output
        .sync_queue
        .as_ref()
        .map(|queue| StatusSyncQueueSnapshot {
            queued: queue.queued,
            claimed: queue.claimed,
            waiting_retry: queue.waiting_retry,
            blocked_offline: queue.blocked_offline,
            attention: queue.attention,
            completed: queue.completed,
        });

    let index = output.index.as_ref().map(|index| StatusIndexSnapshot {
        state: index_state_label(index.state).to_string(),
        file_count: index.file_count,
        path_count: index.path_count,
        summary: redact_status_text(&index.summary, workspace_root),
    });

    let workspace_summary = output.workspace_summary.as_ref().map(|summary| {
        let observed = summary.observed.as_ref();
        StatusWorkspaceSummarySnapshot {
            total_projects: summary.total_projects,
            repo_count: observed.map(|observed| observed.repo_count),
            env_file_count: observed.map(|observed| observed.env_file_count),
        }
    });

    let items = output
        .items
        .iter()
        .map(|item| StatusItemSnapshot {
            kind: status_item_kind_label(item.kind),
            summary: redact_status_text(&item.summary, workspace_root),
            path: item
                .path
                .as_deref()
                .and_then(|path| redact_workspace_path(path, workspace_root)),
            event_name: item.event_name.map(event_name_label),
        })
        .collect();

    let limits = output
        .limits
        .iter()
        .map(|limit| StatusLimitSnapshot {
            capability: limit.capability.clone(),
            unavailable_because: redact_status_text(&limit.unavailable_because, workspace_root),
            path: limit
                .path
                .as_deref()
                .and_then(|path| redact_workspace_path(path, workspace_root)),
            still_works: limit
                .still_works
                .iter()
                .map(|text| redact_status_text(text, workspace_root))
                .collect(),
        })
        .collect();

    WorkspaceStatusSnapshot {
        snapshot_id: status_snapshot_id(output.workspace_id.as_str(), &output.generated_at),
        workspace_id: output.workspace_id.as_str().to_string(),
        status_level: status_level_label(output.status.level).to_string(),
        attention_items: output
            .status
            .attention_items
            .iter()
            .map(|text| redact_status_text(text, workspace_root))
            .collect(),
        generated_at: output.generated_at.clone(),
        event_watermarks,
        sync_queue,
        index,
        workspace_summary,
        items,
        limits,
        published_by_device_id: device_id.to_string(),
    }
}

fn status_snapshot_id(workspace_id: &str, generated_at: &str) -> String {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    workspace_id.hash(&mut hasher);
    generated_at.hash(&mut hasher);
    format!("wss_{:016x}", hasher.finish())
}

fn component_state_label(state: ComponentState) -> &'static str {
    match state {
        ComponentState::Ready => "ready",
        ComponentState::Degraded => "degraded",
        ComponentState::Unavailable => "unavailable",
    }
}

fn network_state_label(state: NetworkState) -> &'static str {
    match state {
        NetworkState::Online => "online",
        NetworkState::Degraded => "degraded",
        NetworkState::Offline => "offline",
    }
}

fn index_state_label(state: IndexState) -> &'static str {
    match state {
        IndexState::Ready => "ready",
        IndexState::Stale => "stale",
        IndexState::Rebuilding => "rebuilding",
        IndexState::Degraded => "degraded",
    }
}

fn status_item_kind_label(kind: StatusItemKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| format!("{kind:?}").to_ascii_lowercase())
}

/// Reduce a status path to a safe, workspace-relative form, or drop it entirely
/// when it still looks like an absolute filesystem path or an env file.
pub(crate) fn redact_workspace_path(path: &str, workspace_root: Option<&str>) -> Option<String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return None;
    }
    let relative = strip_workspace_root(trimmed, workspace_root);
    if path_is_absolute_like(&relative) || path_basename_is_env(&relative) {
        return None;
    }
    Some(relative)
}

fn strip_workspace_root(path: &str, workspace_root: Option<&str>) -> String {
    let Some(root) = workspace_root else {
        return path.to_string();
    };
    let root = root.trim_end_matches('/');
    if root.is_empty() {
        return path.to_string();
    }
    match Path::new(path).strip_prefix(Path::new(root)) {
        Ok(rest) => {
            let rest = rest.to_string_lossy();
            if rest.is_empty() {
                ".".to_string()
            } else {
                rest.to_string()
            }
        }
        Err(_) => path.to_string(),
    }
}

fn redact_status_text(text: &str, workspace_root: Option<&str>) -> String {
    if text
        .split_whitespace()
        .map(status_text_token)
        .any(|token| status_token_needs_redaction(token, workspace_root))
    {
        "Sensitive local path redacted.".to_string()
    } else {
        text.to_string()
    }
}

fn status_text_token(token: &str) -> &str {
    token.trim_matches(|ch: char| {
        matches!(
            ch,
            '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';' | '.' | '!'
        )
    })
}

fn status_token_needs_redaction(token: &str, workspace_root: Option<&str>) -> bool {
    if token.is_empty() {
        return false;
    }
    if !(token.contains('/') || token.contains('\\') || token.contains(".env")) {
        return false;
    }
    redact_workspace_path(token, workspace_root).is_none()
}

fn path_is_absolute_like(path: &str) -> bool {
    path.starts_with('/')
        || path.starts_with('~')
        || path.starts_with('\\')
        || has_windows_drive_prefix(path)
}

fn has_windows_drive_prefix(path: &str) -> bool {
    let mut chars = path.chars();
    matches!(
        (chars.next(), chars.next(), chars.next()),
        (Some(drive), Some(':'), Some('/' | '\\')) if drive.is_ascii_alphabetic()
    )
}

fn path_basename_is_env(path: &str) -> bool {
    let basename = path.rsplit(['/', '\\']).next().unwrap_or(path);
    basename == ".env" || basename.starts_with(".env.")
}
