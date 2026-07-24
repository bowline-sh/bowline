use bowline_core::ids::{DeviceId, EventId, SnapshotId, WorkspaceId};

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
/// are preserved; filesystem paths and secrets never leave the device.
///
/// Convergence readiness and queue settlement retain their canonical typed
/// projections; event watermarks carry event chronology only.
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
            .map(|id| EventId::new(id.as_str())),
        last_scan_at: output.event_watermarks.last_scan_at.clone(),
    };

    let sync_queue = output
        .sync_queue
        .as_ref()
        .map(|queue| StatusSyncQueueSnapshot {
            queued: queue.queued,
            claimed: queue.claimed,
            waiting_retry: queue.waiting_retry,
            blocked_offline: queue.blocked_offline,
            reconciliation_required: queue.reconciliation_required,
            attention: queue.attention,
            completed: queue.completed,
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
            path: None,
            event_name: item.event_name.as_ref().map(event_name_label),
        })
        .collect();

    let limits = output
        .limits
        .iter()
        .map(|limit| StatusLimitSnapshot {
            capability: limit.capability.clone(),
            support_capability: limit
                .support_capability
                .map(support_capability_label)
                .map(str::to_string),
            unavailable_because: redact_status_text(&limit.unavailable_because, workspace_root),
            path: None,
            still_works: limit
                .still_works
                .iter()
                .map(|text| redact_status_text(text, workspace_root))
                .collect(),
        })
        .collect();

    let summary = output.status_summary.clone();
    let hosted_facts = summary
        .facts
        .iter()
        .filter(|fact| {
            status_fact_policy(fact.kind.as_str()).hosted_allowed
                && fact.scope != StatusFactScope::Path
        })
        .cloned()
        .map(|mut fact| {
            fact.parameters.clear();
            if fact
                .action
                .as_ref()
                .is_some_and(|action| action.kind == "approve-device-local")
            {
                fact.action = None;
            }
            fact
        })
        .collect::<Vec<_>>();
    let hosted_summary = reduce_status_facts(
        hosted_facts
            .iter()
            .filter(|fact| {
                fact.scope == StatusFactScope::Workspace
                    || status_fact_policy(fact.kind.as_str()).workspace_affecting
            })
            .cloned(),
        summary.snapshot_version,
        summary.observed_at.clone(),
    );

    WorkspaceStatusSnapshot {
        snapshot_id: SnapshotId::new(status_snapshot_id(
            output.workspace_id.as_str(),
            &output.generated_at,
        )),
        workspace_id: WorkspaceId::new(output.workspace_id.as_str()),
        availability: status_availability_label(hosted_summary.availability).to_string(),
        attention: status_attention_label(hosted_summary.attention).to_string(),
        primary_fact_id: hosted_summary
            .primary_fact_id
            .as_ref()
            .map(|id| id.as_str().to_string()),
        facts: hosted_facts,
        freshness: status_snapshot_freshness_label(hosted_summary.freshness).to_string(),
        schema_hash: bowline_core::wire::WIRE_SCHEMA_HASH.to_string(),
        snapshot_version: hosted_summary.snapshot_version,
        producer_version: env!("CARGO_PKG_VERSION").to_string(),
        observed_at: hosted_summary.observed_at,
        attention_items: output
            .status
            .attention_items
            .iter()
            .map(|text| redact_status_text(text, workspace_root))
            .collect(),
        event_watermarks,
        sync_queue,
        workspace_summary,
        items,
        limits,
        published_by_device_id: DeviceId::new(device_id),
    }
}

fn status_availability_label(value: StatusAvailability) -> &'static str {
    match value {
        StatusAvailability::Ready => "ready",
        StatusAvailability::Degraded => "degraded",
        StatusAvailability::Unavailable => "unavailable",
    }
}

fn status_attention_label(value: StatusAttention) -> &'static str {
    match value {
        StatusAttention::None => "none",
        StatusAttention::Recommended => "recommended",
        StatusAttention::Required => "required",
    }
}

fn status_snapshot_freshness_label(value: StatusSnapshotFreshness) -> &'static str {
    match value {
        StatusSnapshotFreshness::Fresh => "fresh",
        StatusSnapshotFreshness::Stale => "stale",
        StatusSnapshotFreshness::Unknown => "unknown",
    }
}

fn support_capability_label(
    support_capability: bowline_core::status::ControlPlaneSupportCapability,
) -> &'static str {
    match support_capability {
        bowline_core::status::ControlPlaneSupportCapability::DeviceApproval => "device-approval",
        bowline_core::status::ControlPlaneSupportCapability::ProjectScopedWorkspaceRefCas => {
            "project-scoped-workspace-ref-cas"
        }
        bowline_core::status::ControlPlaneSupportCapability::WorkView => "work-view",
        bowline_core::status::ControlPlaneSupportCapability::AgentLease => "agent-lease",
        bowline_core::status::ControlPlaneSupportCapability::EncryptedObjectStore => {
            "encrypted-object-store"
        }
        bowline_core::status::ControlPlaneSupportCapability::Recovery => "recovery",
    }
}

fn status_snapshot_id(workspace_id: &str, generated_at: &str) -> String {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    workspace_id.hash(&mut hasher);
    generated_at.hash(&mut hasher);
    format!("wss_{:016x}", hasher.finish())
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
    if !(token.contains('/') || token.contains('\\') || token.to_ascii_lowercase().contains(".env"))
    {
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
    crate::policy::is_project_env_name(basename)
}
