use std::collections::BTreeMap;

use bowline_core::{
    commands::StatusCommandOutput,
    status::{
        LimitedCapability, StatusAttention, StatusFact, StatusFactAvailabilityImpact,
        StatusFactScope, StatusItem, StatusItemKind, StatusSnapshotFreshness, StatusSubject,
        StatusSubjectKind, SyncQueueStatus, reduce_status_facts,
    },
};

use super::{
    collector::{StatusSourceFacts, StatusSourceState, StatusSourceStateFacts},
    types::{SourceFreshness, SourceRevision, StatusSource, StatusTimestamp},
};

pub(crate) fn reduce_projection_status(
    metadata_status: &StatusCommandOutput,
    sources: &BTreeMap<StatusSource, SourceRevision>,
    source_facts: &BTreeMap<StatusSource, StatusSourceFacts>,
    generated_at: &StatusTimestamp,
) -> StatusCommandOutput {
    let mut output = metadata_status.clone();
    output.generated_at = generated_at.as_str().to_string();
    let mut facts = output.status_summary.facts.clone();
    let mut supplemental_items = Vec::new();
    let mut supplemental_limits = Vec::new();
    let mut supplemental_attention = Vec::new();

    // Metadata is the temporary parity collector, while aggregate facts let the
    // canonical reducer apply newer daemon source truth before anything is published.
    for (source, revision) in sources {
        let state_facts = source_facts
            .get(source)
            .and_then(StatusSourceFacts::state_facts);
        let condition = ProjectedSourceCondition::new(*source, revision.freshness, state_facts);
        condition.apply_sync_queue(&mut output.sync_queue);
        if !condition.is_visible() {
            continue;
        }
        facts.push(condition.status_fact(revision, generated_at));
        let summary = condition.summary();
        supplemental_items.push(condition.status_item(summary.clone()));
        if condition.attention != StatusAttention::None
            || condition.availability != StatusFactAvailabilityImpact::None
        {
            supplemental_attention.push(summary.clone());
        }
        if condition.availability != StatusFactAvailabilityImpact::None {
            supplemental_limits.push(condition.limited_capability(summary));
        }
    }

    if let Some(StatusSourceFacts::DeviceTrustDetails(device_trust)) =
        source_facts.get(&StatusSource::DeviceTrust)
    {
        facts.extend(device_trust.facts.clone());
        output.items.extend(device_trust.items.clone());
        output
            .device_approvals
            .extend(device_trust.approvals.clone());
    }

    output.status_summary = reduce_status_facts(
        facts,
        output.status_summary.snapshot_version,
        generated_at.as_str(),
    );
    output.status_summary.freshness =
        projection_freshness(sources, output.status_summary.freshness);
    output.status.level = output.status_summary.presentation_level();
    output.status.attention_items.extend(supplemental_attention);
    output.status.attention_items.sort();
    output.status.attention_items.dedup();
    output.items.extend(supplemental_items);
    output.limits.extend(supplemental_limits);
    output
}

fn projection_freshness(
    sources: &BTreeMap<StatusSource, SourceRevision>,
    reduced_freshness: StatusSnapshotFreshness,
) -> StatusSnapshotFreshness {
    if sources
        .values()
        .any(|source| source.freshness == SourceFreshness::Failed)
    {
        StatusSnapshotFreshness::Unknown
    } else if sources
        .values()
        .any(|source| source.freshness == SourceFreshness::Stale)
    {
        StatusSnapshotFreshness::Stale
    } else {
        reduced_freshness
    }
}

struct ProjectedSourceCondition {
    source: StatusSource,
    freshness: SourceFreshness,
    state: StatusSourceState,
    pending_count: u64,
    availability: StatusFactAvailabilityImpact,
    attention: StatusAttention,
}

impl ProjectedSourceCondition {
    fn new(
        source: StatusSource,
        freshness: SourceFreshness,
        facts: Option<&StatusSourceStateFacts>,
    ) -> Self {
        let state = facts.map_or_else(
            || {
                if freshness == SourceFreshness::Failed {
                    StatusSourceState::Unavailable
                } else {
                    StatusSourceState::Ready
                }
            },
            |facts| facts.state,
        );
        let pending_count = facts.map_or(0, |facts| facts.pending_count);
        let (mut availability, mut attention) = state_impacts(state);
        match freshness {
            SourceFreshness::Current => {}
            SourceFreshness::Stale => {
                availability = availability.max(StatusFactAvailabilityImpact::Degraded);
                attention = attention.max(StatusAttention::Recommended);
            }
            SourceFreshness::Failed => {
                availability = StatusFactAvailabilityImpact::Unavailable;
                attention = StatusAttention::Required;
            }
        }
        if pending_count > 0 {
            attention = attention.max(pending_attention(source));
        }
        Self {
            source,
            freshness,
            state,
            pending_count,
            availability,
            attention,
        }
    }

    fn is_visible(&self) -> bool {
        self.availability != StatusFactAvailabilityImpact::None
            || self.attention != StatusAttention::None
            || self.pending_count > 0
    }

    fn status_fact(&self, revision: &SourceRevision, generated_at: &StatusTimestamp) -> StatusFact {
        let token = self.source.as_str();
        let observed_at = if self.freshness == SourceFreshness::Failed {
            generated_at.as_str()
        } else {
            revision.observed_at.as_str()
        };
        let mut fact = StatusFact::new(
            format!("projection-{token}"),
            "status.aggregate_input",
            "status-reducer",
            StatusFactScope::Workspace,
            observed_at,
            format!("projection-{token}"),
        )
        .with_impacts(self.availability, self.attention);
        fact.parameters
            .insert("source".to_string(), token.to_string());
        fact.parameters
            .insert("state".to_string(), state_token(self.state).to_string());
        fact.parameters.insert(
            "freshness".to_string(),
            freshness_token(self.freshness).to_string(),
        );
        fact.parameters
            .insert("pendingCount".to_string(), self.pending_count.to_string());
        fact
    }

    fn summary(&self) -> String {
        let label = source_label(self.source);
        if self.freshness == SourceFreshness::Failed {
            return format!("{label} status collection failed.");
        }
        if self.state == StatusSourceState::Unavailable {
            return format!("{label} is unavailable.");
        }
        if self.freshness == SourceFreshness::Stale {
            return format!("{label} status is stale.");
        }
        if self.state == StatusSourceState::Degraded {
            return format!("{label} is degraded.");
        }
        format!("{label} has {} pending.", self.pending_count)
    }

    fn status_item(&self, summary: String) -> StatusItem {
        StatusItem {
            kind: item_kind(self.source),
            summary,
            subject: Some(StatusSubject {
                kind: StatusSubjectKind::Component,
                id: format!("status-source-{}", self.source.as_str()),
                path: None,
            }),
            path: None,
            classification: None,
            mode: None,
            access: Vec::new(),
            event_id: None,
            event_name: None,
            device_id: None,
            lease_id: None,
            project_id: None,
            snapshot_id: None,
            policy_version: None,
            env_record_id: None,
        }
    }

    fn limited_capability(&self, summary: String) -> LimitedCapability {
        LimitedCapability {
            capability: self.source.as_str().to_string(),
            support_capability: None,
            unavailable_because: summary,
            still_works: vec!["Last known local metadata status remains available.".to_string()],
            path: None,
        }
    }

    fn apply_sync_queue(&self, queue: &mut Option<SyncQueueStatus>) {
        if self.source != StatusSource::SyncRuntime || self.freshness == SourceFreshness::Failed {
            return;
        }
        let queue = queue.get_or_insert(SyncQueueStatus {
            queued: 0,
            claimed: 0,
            waiting_retry: 0,
            blocked_offline: 0,
            reconciliation_required: 0,
            attention: 0,
            completed: 0,
        });
        // The live runtime owns the current queued lane; durable retry, block,
        // reconciliation, attention, and completion lanes remain metadata-derived.
        queue.queued = self.pending_count;
    }
}

fn state_impacts(state: StatusSourceState) -> (StatusFactAvailabilityImpact, StatusAttention) {
    match state {
        StatusSourceState::Ready => (StatusFactAvailabilityImpact::None, StatusAttention::None),
        StatusSourceState::Degraded => (
            StatusFactAvailabilityImpact::Degraded,
            StatusAttention::Recommended,
        ),
        StatusSourceState::Unavailable => (
            StatusFactAvailabilityImpact::Unavailable,
            StatusAttention::Required,
        ),
    }
}

fn pending_attention(source: StatusSource) -> StatusAttention {
    match source {
        StatusSource::DeviceTrust => StatusAttention::Required,
        StatusSource::UpdateAvailability | StatusSource::NotificationState => {
            StatusAttention::Recommended
        }
        StatusSource::Metadata
        | StatusSource::SyncRuntime
        | StatusSource::StoreHealth
        | StatusSource::ServiceRuntime => StatusAttention::None,
    }
}

fn item_kind(source: StatusSource) -> StatusItemKind {
    match source {
        StatusSource::Metadata | StatusSource::StoreHealth => StatusItemKind::Metadata,
        StatusSource::SyncRuntime => StatusItemKind::Network,
        StatusSource::DeviceTrust => StatusItemKind::Device,
        StatusSource::UpdateAvailability => StatusItemKind::Update,
        StatusSource::NotificationState => StatusItemKind::Continuity,
        StatusSource::ServiceRuntime => StatusItemKind::Watcher,
    }
}

fn source_label(source: StatusSource) -> &'static str {
    match source {
        StatusSource::Metadata => "Metadata",
        StatusSource::SyncRuntime => "Sync runtime",
        StatusSource::StoreHealth => "Store health",
        StatusSource::DeviceTrust => "Device trust",
        StatusSource::UpdateAvailability => "Update availability",
        StatusSource::NotificationState => "Notification state",
        StatusSource::ServiceRuntime => "Service runtime",
    }
}

fn state_token(state: StatusSourceState) -> &'static str {
    match state {
        StatusSourceState::Ready => "ready",
        StatusSourceState::Degraded => "degraded",
        StatusSourceState::Unavailable => "unavailable",
    }
}

fn freshness_token(freshness: SourceFreshness) -> &'static str {
    match freshness {
        SourceFreshness::Current => "current",
        SourceFreshness::Stale => "stale",
        SourceFreshness::Failed => "failed",
    }
}
