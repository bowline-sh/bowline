use std::collections::BTreeMap;

use bowline_core::{
    commands::StatusCommandOutput,
    status::{
        LimitedCapability, PROJECT_CONVERGENCE_FACT_ID, StatusAttention, StatusFact,
        StatusFactAvailabilityImpact, StatusFactScope, StatusItem, StatusItemKind,
        StatusSnapshotFreshness, StatusSubject, StatusSubjectKind, SyncQueueStatus,
        WORKSPACE_CONVERGENCE_FACT_ID, reduce_status_facts, remove_convergence_surfaces,
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

    let convergence_current = sources
        .get(&StatusSource::Convergence)
        .is_some_and(|revision| revision.freshness == SourceFreshness::Current);
    let convergence_source_present = sources.contains_key(&StatusSource::Convergence);
    let convergence = if convergence_current {
        source_facts
            .get(&StatusSource::Convergence)
            .and_then(convergence_facts)
    } else {
        None
    };
    if let Some(status) = convergence {
        apply_convergence_status(
            &mut output,
            status,
            ConvergenceStatusOutputs {
                facts: &mut facts,
                items: &mut supplemental_items,
                limits: &mut supplemental_limits,
                attention: &mut supplemental_attention,
                generated_at,
                presentation: ConvergencePresentation::Workspace,
            },
        );
    } else {
        // Convergence and its queue are owned by the manifest engine now
        // (Plan 111 Step 1c). Never inherit stale journal-derived values from the
        // metadata base status, and fail closed while a present source is
        // unreadable — every readiness consumer sees no convergence authority.
        output.convergence = None;
        output.sync_queue = None;
    }

    // Metadata is the temporary parity collector, while aggregate facts let the
    // canonical reducer apply newer daemon source truth before anything is published.
    for (source, revision) in sources {
        let state_facts = source_facts
            .get(source)
            .and_then(StatusSourceFacts::state_facts);
        let condition = ProjectedSourceCondition::new(*source, revision.freshness, state_facts);
        condition.apply_sync_queue(&mut output.sync_queue, convergence_source_present);
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

/// Replace only the convergence-owned portion of an already reduced status
/// projection. Project-scoped daemon responses retain metadata, trust, service,
/// update, and notification truth while removing the workspace-wide engine
/// result and reducing the scoped engine facts through the same canonical path.
pub fn replace_convergence_status(
    output: &mut StatusCommandOutput,
    status: &super::engine_status::EngineConvergenceFacts,
    project_scope_id: &str,
) {
    let prior_freshness = output.status_summary.freshness;
    let snapshot_version = output.status_summary.snapshot_version;
    let generated_at = StatusTimestamp::new(output.generated_at.clone());
    remove_convergence_surfaces(output);

    let mut facts = output.status_summary.facts.clone();
    let mut items = Vec::new();
    let mut limits = Vec::new();
    let mut attention = Vec::new();
    apply_convergence_status(
        output,
        status,
        ConvergenceStatusOutputs {
            facts: &mut facts,
            items: &mut items,
            limits: &mut limits,
            attention: &mut attention,
            generated_at: &generated_at,
            presentation: ConvergencePresentation::Project {
                scope_id: project_scope_id,
            },
        },
    );
    output.items.extend(items);
    output.limits.extend(limits);
    output.status.attention_items.append(&mut attention);
    output.status.attention_items.sort();
    output.status.attention_items.dedup();
    output.status_summary = reduce_status_facts(facts, snapshot_version, generated_at.as_str());
    output.status_summary.freshness = prior_freshness;
    output.status.level = output.status_summary.presentation_level();
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

    fn apply_sync_queue(&self, queue: &mut Option<SyncQueueStatus>, canonical: bool) {
        if canonical
            || self.source != StatusSource::SyncRuntime
            || self.freshness == SourceFreshness::Failed
        {
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
        // The live runtime owns the current queued lane; retry, block,
        // reconciliation, attention, and completion lanes remain metadata-derived.
        queue.queued = self.pending_count;
    }
}

fn convergence_facts(
    facts: &StatusSourceFacts,
) -> Option<&super::engine_status::EngineConvergenceFacts> {
    match facts {
        StatusSourceFacts::Convergence(status) => Some(status),
        _ => None,
    }
}

#[derive(Clone, Copy)]
enum ConvergencePresentation<'a> {
    Workspace,
    Project { scope_id: &'a str },
}

impl<'a> ConvergencePresentation<'a> {
    fn id(self) -> &'static str {
        match self {
            Self::Workspace => WORKSPACE_CONVERGENCE_FACT_ID,
            Self::Project { .. } => PROJECT_CONVERGENCE_FACT_ID,
        }
    }

    fn kind(self) -> &'static str {
        match self {
            Self::Workspace => "workspace.convergence",
            Self::Project { .. } => "project.convergence",
        }
    }

    fn scope(self) -> StatusFactScope {
        match self {
            Self::Workspace => StatusFactScope::Workspace,
            Self::Project { .. } => StatusFactScope::Project,
        }
    }

    fn subject_kind(self) -> StatusSubjectKind {
        match self {
            Self::Workspace => StatusSubjectKind::Component,
            Self::Project { .. } => StatusSubjectKind::Project,
        }
    }

    fn scope_id(self) -> Option<&'a str> {
        match self {
            Self::Workspace => None,
            Self::Project { scope_id } => Some(scope_id),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Workspace => "Workspace",
            Self::Project { .. } => "Project",
        }
    }
}

struct ConvergenceStatusOutputs<'a> {
    facts: &'a mut Vec<StatusFact>,
    items: &'a mut Vec<StatusItem>,
    limits: &'a mut Vec<LimitedCapability>,
    attention: &'a mut Vec<String>,
    generated_at: &'a StatusTimestamp,
    presentation: ConvergencePresentation<'a>,
}

fn apply_convergence_status(
    output: &mut StatusCommandOutput,
    status: &super::engine_status::EngineConvergenceFacts,
    outputs: ConvergenceStatusOutputs<'_>,
) {
    let ConvergenceStatusOutputs {
        facts,
        items,
        limits,
        attention,
        generated_at,
        presentation,
    } = outputs;
    output.convergence = Some(status.summary.clone());
    output.sync_queue = Some(status.queue.clone());
    if status.ready {
        return;
    }
    let state = match status.summary.state {
        bowline_core::status::ConvergenceReadinessState::Ready => "ready",
        bowline_core::status::ConvergenceReadinessState::Converging => "syncing",
        bowline_core::status::ConvergenceReadinessState::Recovering => "recovering",
        bowline_core::status::ConvergenceReadinessState::Limited => "needs attention",
    };
    let summary = format!(
        "{} sync is {state} at revision {}.",
        presentation.label(),
        status.revision
    );
    let id = presentation.id();
    let mut fact = StatusFact::new(
        id,
        presentation.kind(),
        id,
        presentation.scope(),
        generated_at.as_str(),
        format!("{id}:{}", status.revision),
    )
    .with_impacts(status.availability, status.attention);
    fact.scope_id = presentation.scope_id().map(str::to_string);
    fact.parameters
        .insert("revision".to_string(), status.revision.to_string());
    fact.parameters.insert(
        "reasons".to_string(),
        status
            .summary
            .reasons
            .iter()
            .map(|reason| format!("{reason:?}"))
            .collect::<Vec<_>>()
            .join(","),
    );
    facts.push(fact);
    items.push(StatusItem {
        kind: StatusItemKind::Continuity,
        summary: summary.clone(),
        subject: Some(StatusSubject {
            kind: presentation.subject_kind(),
            id: id.to_string(),
            path: presentation.scope_id().map(str::to_string),
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
    });
    attention.push(summary.clone());
    if status.limited {
        limits.push(LimitedCapability {
            capability: id.to_string(),
            support_capability: None,
            unavailable_because: summary,
            still_works: vec![
                "Sync retries automatically and clears when it recovers.".to_string(),
            ],
            path: None,
        });
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
        | StatusSource::Convergence
        | StatusSource::SyncRuntime
        | StatusSource::ServiceRuntime => StatusAttention::None,
    }
}

fn item_kind(source: StatusSource) -> StatusItemKind {
    match source {
        StatusSource::Metadata => StatusItemKind::Metadata,
        StatusSource::Convergence => StatusItemKind::Continuity,
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
        StatusSource::Convergence => "Workspace convergence",
        StatusSource::SyncRuntime => "Sync runtime",
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
