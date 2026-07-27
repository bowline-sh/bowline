use super::generated::{
    EventsListCompactEvents, HostedCompactEvent, HostedCompactEventKind, HostedEventWatermarks,
    HostedEventsListCompactEventsRequest, HostedLimitedCapability,
    HostedRefsCompareAndSwapWorkspaceRefRequest, HostedRefsCreateWorkspaceRefRequest,
    HostedRefsGetWorkspaceRefRequest, HostedRefsListWorkspaceRefHistoryRequest,
    HostedStatusActionReference, HostedStatusAttention, HostedStatusAvailability, HostedStatusFact,
    HostedStatusFactAttentionImpact, HostedStatusFactAvailabilityImpact, HostedStatusFactScope,
    HostedStatusFreshness, HostedStatusItem, HostedStatusPublishWorkspaceStatusRequest,
    HostedSyncQueue, HostedWorkspaceRefHistoryRecord, HostedWorkspaceSummary,
    RefsCompareAndSwapWorkspaceRef, RefsCreateWorkspaceRef, RefsGetWorkspaceRef,
    RefsListWorkspaceRefHistory, StatusPublishWorkspaceStatus,
};
use super::*;
use crate::WorkspaceControlPlaneClient;
use bowline_core::status::{
    StatusAttention, StatusAvailability, StatusFactAvailabilityImpact, StatusFactScope,
    StatusSnapshotFreshness,
};

impl WorkspaceControlPlaneClient for HostedControlPlaneClient {
    fn create_workspace_ref(&self, workspace_id: &WorkspaceId) -> ControlPlaneResult<WorkspaceRef> {
        // Pure workspace establishment: seeds a headless version-0 genesis ref.
        let dto = self.call_reauthenticating::<RefsCreateWorkspaceRef, _>(
            Some(workspace_id.as_str()),
            || {
                Ok(HostedRefsCreateWorkspaceRefRequest {
                    account_session_id: self
                        .verified_account_session_id(Some(workspace_id.as_str()))?,
                    workspace_id: workspace_id.as_str().to_string(),
                })
            },
        )?;
        // The DTO -> domain boundary re-runs the workspace-head signature
        // verification; a bare serde decode is not verification.
        workspace_ref_from_dto(dto, |workspace_id, device_id| {
            self.device_proof_verifier(workspace_id, device_id)
        })
    }

    fn get_workspace_ref(
        &self,
        workspace_id: &WorkspaceId,
    ) -> ControlPlaneResult<Option<WorkspaceRef>> {
        let response = self.call_reauthenticating::<RefsGetWorkspaceRef, _>(
            Some(workspace_id.as_str()),
            || {
                Ok(HostedRefsGetWorkspaceRefRequest {
                    workspace_id: workspace_id.as_str().to_string(),
                    account_session_id: self
                        .verified_account_session_id(Some(workspace_id.as_str()))?,
                })
            },
        )?;
        match response {
            None => Ok(None),
            // The DTO -> domain boundary re-runs the workspace-head signature
            // verification before the ref is trusted.
            Some(dto) => workspace_ref_from_dto(dto, |workspace_id, device_id| {
                self.device_proof_verifier(workspace_id, device_id)
            })
            .map(Some),
        }
    }

    fn compare_and_swap_workspace_ref(
        &self,
        workspace_id: &WorkspaceId,
        expected_version: u64,
        new_snapshot_id: &SnapshotId,
        writer_device_id: &DeviceId,
    ) -> Result<WorkspaceRef, CompareAndSwapError> {
        self.compare_and_swap_workspace_ref_for_project(
            workspace_id,
            expected_version,
            new_snapshot_id,
            writer_device_id,
            None,
        )
    }

    fn compare_and_swap_workspace_ref_for_project(
        &self,
        workspace_id: &WorkspaceId,
        expected_version: u64,
        new_snapshot_id: &SnapshotId,
        writer_device_id: &DeviceId,
        project_id: Option<&ProjectId>,
    ) -> Result<WorkspaceRef, CompareAndSwapError> {
        self.require_local_device(writer_device_id)
            .map_err(CompareAndSwapError::rejected)?;
        let proof_subject = workspace_ref_proof_subject(expected_version, new_snapshot_id);
        let writer_device_proof = self
            .device_proof(
                workspace_id,
                "compare-and-swap-workspace-ref",
                &proof_subject,
            )
            .map_err(CompareAndSwapError::rejected)?;
        let head_signature_subject =
            workspace_head_proof_subject(workspace_id, expected_version + 1, new_snapshot_id);
        let head_signature = self
            .device_proof(workspace_id, "sign-workspace-head", &head_signature_subject)
            .map_err(CompareAndSwapError::rejected)?;
        let request = HostedRefsCompareAndSwapWorkspaceRefRequest {
            expected_version,
            head_signature,
            next_snapshot_id: new_snapshot_id.as_str().to_string(),
            project_id: project_id.map(|project_id| project_id.as_str().to_string()),
            workspace_id: workspace_id.as_str().to_string(),
            writer_device_id: writer_device_id.as_str().to_string(),
            writer_device_proof,
        };
        let response = self
            .call::<RefsCompareAndSwapWorkspaceRef>(&request)
            .map_err(compare_and_swap_call_failure)?;

        if response.ok {
            let dto = response.r#ref.ok_or(CompareAndSwapError::Unsupported {
                capability: HOSTED_CAPABILITY,
                reason: "Convex CAS reported success without a ref",
            })?;
            // The DTO -> domain boundary re-runs the workspace-head signature
            // verification before the advanced ref is trusted.
            // The swap applied, but the advanced ref failed head-signature
            // verification, so the caller must re-read rather than trust it.
            return workspace_ref_from_dto(dto, |workspace_id, device_id| {
                self.device_proof_verifier(workspace_id, device_id)
            })
            .map_err(CompareAndSwapError::ambiguous);
        }

        match response.error.as_deref() {
            Some("workspace-missing") => Err(CompareAndSwapError::WorkspaceMissing {
                workspace_id: workspace_id.clone(),
            }),
            Some("stale-ref") => {
                let dto = response
                    .current_ref
                    .ok_or(CompareAndSwapError::Unsupported {
                        capability: HOSTED_CAPABILITY,
                        reason: "Convex CAS reported stale-ref without a current ref",
                    })?;
                let current = workspace_ref_from_dto(dto, |workspace_id, device_id| {
                    self.device_proof_verifier(workspace_id, device_id)
                })
                .map_err(CompareAndSwapError::rejected)?;
                Err(CompareAndSwapError::StaleRef(StaleWorkspaceRef {
                    expected_version,
                    current,
                }))
            }
            Some(_) | None => Err(CompareAndSwapError::Unsupported {
                capability: HOSTED_CAPABILITY,
                reason: "Convex CAS returned an unknown result shape",
            }),
        }
    }

    fn list_events(&self, workspace_id: &WorkspaceId) -> ControlPlaneResult<Vec<CompactEvent>> {
        self.call_reauthenticating::<EventsListCompactEvents, _>(
            Some(workspace_id.as_str()),
            || {
                Ok(HostedEventsListCompactEventsRequest {
                    account_session_id: self
                        .verified_account_session_id(Some(workspace_id.as_str()))?,
                    limit: None,
                    workspace_id: workspace_id.as_str().to_string(),
                })
            },
        )?
        .into_iter()
        .map(CompactEvent::try_from)
        .collect()
    }

    fn list_workspace_ref_history(
        &self,
        workspace_id: &WorkspaceId,
        limit: u32,
    ) -> ControlPlaneResult<Vec<WorkspaceRefHistoryRecord>> {
        // The server clamps `limit` to 1..500 (default 100); the request carries
        // it verbatim.
        self.call_reauthenticating::<RefsListWorkspaceRefHistory, _>(
            Some(workspace_id.as_str()),
            || {
                Ok(HostedRefsListWorkspaceRefHistoryRequest {
                    workspace_id: workspace_id.as_str().to_string(),
                    limit: Some(limit),
                    account_session_id: self
                        .verified_account_session_id(Some(workspace_id.as_str()))?,
                })
            },
        )?
        .into_iter()
        .map(WorkspaceRefHistoryRecord::try_from)
        .collect()
    }

    fn publish_workspace_status(
        &self,
        snapshot: &WorkspaceStatusSnapshot,
    ) -> ControlPlaneResult<()> {
        self.require_local_device(&snapshot.published_by_device_id)?;
        let proof_subject = snapshot.proof_subject();
        let published_by_device_proof = self.device_proof(
            &snapshot.workspace_id,
            "publish-workspace-status",
            &proof_subject,
        )?;
        let request = status_publish_request_from_snapshot(snapshot, published_by_device_proof)?;
        // The typed request carries every signed field (workspaceId, snapshotId,
        // availability, attention, schemaHash, snapshotVersion, observedAt) with
        // the identical wire value the proof subject was built over.
        self.call::<StatusPublishWorkspaceStatus>(&request)?;
        Ok(())
    }
}

impl TryFrom<HostedWorkspaceRefHistoryRecord> for WorkspaceRefHistoryRecord {
    type Error = ControlPlaneError;

    fn try_from(record: HostedWorkspaceRefHistoryRecord) -> Result<Self, Self::Error> {
        // baseSnapshotId is absent for the genesis advance (version 1), which has
        // no prior head to restore to; the domain record mirrors that with an
        // Option rather than fabricating a base.
        Ok(Self {
            workspace_id: WorkspaceId::new(record.workspace_id),
            version: record.version,
            base_snapshot_id: record.base_snapshot_id.map(SnapshotId::new),
            target_snapshot_id: SnapshotId::new(record.target_snapshot_id),
            occurred_at: parse_control_timestamp(record.occurred_at.as_str())
                .map_err(|error| add_field_context(error, "occurredAt"))?,
            advanced_by_device_id: record.advanced_by_device_id.map(DeviceId::new),
            caused_by_event_id: record.caused_by_event_id.map(EventId::new),
            project_id: record.project_id.map(ProjectId::new),
        })
    }
}

impl TryFrom<HostedCompactEvent> for CompactEvent {
    type Error = ControlPlaneError;

    fn try_from(dto: HostedCompactEvent) -> Result<Self, Self::Error> {
        Ok(CompactEvent {
            event_id: EventId::new(dto.event_id),
            workspace_id: WorkspaceId::new(dto.workspace_id),
            at: parse_control_timestamp(dto.occurred_at.as_str())
                .map_err(|error| add_field_context(error, "occurredAt"))?,
            kind: event_kind_from_dto(dto.kind),
            subject: dto.subject,
        })
    }
}

// Total map from the generated wire vocabulary onto the domain enum. It must
// stay total: a partial map made one unmodeled event permanently poison every
// list_events call for that workspace.
fn event_kind_from_dto(kind: HostedCompactEventKind) -> CompactEventKind {
    match kind {
        HostedCompactEventKind::WorkspaceCreated => CompactEventKind::WorkspaceCreated,
        HostedCompactEventKind::WorkspaceRefAdvanced => CompactEventKind::WorkspaceRefAdvanced,
        HostedCompactEventKind::ObjectPointerAdded => CompactEventKind::ObjectPointerAdded,
        HostedCompactEventKind::DeviceRequested => CompactEventKind::DeviceRequested,
        HostedCompactEventKind::DeviceHarnessApproved => CompactEventKind::DeviceHarnessApproved,
        HostedCompactEventKind::DeviceApprovalRequested => {
            CompactEventKind::DeviceApprovalRequested
        }
        HostedCompactEventKind::DeviceApproved => CompactEventKind::DeviceApproved,
        HostedCompactEventKind::DeviceDenied => CompactEventKind::DeviceDenied,
        HostedCompactEventKind::DeviceRevoked => CompactEventKind::DeviceRevoked,
        HostedCompactEventKind::RecoveryKeyCreated => CompactEventKind::RecoveryKeyCreated,
        HostedCompactEventKind::RecoveryKeyVerified => CompactEventKind::RecoveryKeyVerified,
        HostedCompactEventKind::RecoveryKeyRotated => CompactEventKind::RecoveryKeyRotated,
        HostedCompactEventKind::RecoveryKeyRevoked => CompactEventKind::RecoveryKeyRevoked,
        HostedCompactEventKind::AuthLoginStarted => CompactEventKind::AuthLoginStarted,
        HostedCompactEventKind::AuthLoginCompleted => CompactEventKind::AuthLoginCompleted,
        HostedCompactEventKind::NamespaceArchived => CompactEventKind::NamespaceArchived,
        HostedCompactEventKind::NamespaceArchiveRestored => {
            CompactEventKind::NamespaceArchiveRestored
        }
        HostedCompactEventKind::NamespacePurgePending => CompactEventKind::NamespacePurgePending,
        HostedCompactEventKind::NamespacePurgeCancelled => {
            CompactEventKind::NamespacePurgeCancelled
        }
        HostedCompactEventKind::WorkspaceStatusPublished => {
            CompactEventKind::WorkspaceStatusPublished
        }
        HostedCompactEventKind::MemberInvited => CompactEventKind::MemberInvited,
        HostedCompactEventKind::MemberJoined => CompactEventKind::MemberJoined,
        HostedCompactEventKind::MemberRemoved => CompactEventKind::MemberRemoved,
        HostedCompactEventKind::WorkspaceKeyRotated => CompactEventKind::WorkspaceKeyRotated,
        HostedCompactEventKind::WorkspaceKeySeeded => CompactEventKind::WorkspaceKeySeeded,
        HostedCompactEventKind::WorkspaceKeyRegrantOffered => {
            CompactEventKind::WorkspaceKeyRegrantOffered
        }
        HostedCompactEventKind::WorkspaceKeyRegrantSettled => {
            CompactEventKind::WorkspaceKeyRegrantSettled
        }
    }
}

/// Build the typed publish-workspace-status request from the domain snapshot.
/// Availability/attention/freshness are already domain enums, so the wire
/// encoding is total and the signed proof subject cannot disagree with it.
/// Observation instants are the one part the domain still carries as strings,
/// so they are re-validated here rather than published unchecked.
fn status_publish_request_from_snapshot(
    snapshot: &WorkspaceStatusSnapshot,
    published_by_device_proof: String,
) -> ControlPlaneResult<HostedStatusPublishWorkspaceStatusRequest> {
    let observed_at = wire_timestamp_from_domain(&snapshot.observed_at, "observedAt")?;
    Ok(HostedStatusPublishWorkspaceStatusRequest {
        attention: status_attention_to_dto(snapshot.attention),
        attention_items: snapshot.attention_items.clone(),
        availability: status_availability_to_dto(snapshot.availability),
        event_watermarks: event_watermarks_to_dto(&snapshot.event_watermarks),
        facts: snapshot
            .facts
            .iter()
            .map(status_fact_to_dto)
            .collect::<ControlPlaneResult<Vec<_>>>()?,
        freshness: status_freshness_to_dto(snapshot.freshness),
        // `generatedAt` mirrors `observedAt`, matching the prior hand-assembled
        // request.
        generated_at: observed_at.clone(),
        items: (!snapshot.items.is_empty())
            .then(|| snapshot.items.iter().map(status_item_to_dto).collect()),
        limits: (!snapshot.limits.is_empty())
            .then(|| snapshot.limits.iter().map(status_limit_to_dto).collect()),
        observed_at,
        primary_fact_id: snapshot.primary_fact_id.clone(),
        producer_version: snapshot.producer_version.clone(),
        published_by_device_id: snapshot.published_by_device_id.as_str().to_string(),
        published_by_device_proof,
        schema_hash: snapshot.schema_hash.clone(),
        snapshot_id: snapshot.snapshot_id.as_str().to_string(),
        snapshot_version: snapshot.snapshot_version,
        sync_queue: snapshot.sync_queue.as_ref().map(sync_queue_to_dto),
        workspace_id: snapshot.workspace_id.as_str().to_string(),
        workspace_summary: snapshot
            .workspace_summary
            .as_ref()
            .map(workspace_summary_to_dto),
    })
}

/// Re-validate a domain timestamp string on its way onto the wire. The domain
/// snapshot still carries observation instants as strings, so this is the last
/// point that can reject a value the hosted contract would refuse.
fn wire_timestamp_from_domain(
    value: &str,
    field: &'static str,
) -> ControlPlaneResult<generated::Timestamp> {
    generated::Timestamp::new(value)
        .map_err(|_| add_field_context(shape_error("timestamp must be RFC3339"), field))
}

fn status_attention_to_dto(attention: StatusAttention) -> HostedStatusAttention {
    match attention {
        StatusAttention::None => HostedStatusAttention::None,
        StatusAttention::Recommended => HostedStatusAttention::Recommended,
        StatusAttention::Required => HostedStatusAttention::Required,
    }
}

fn status_availability_to_dto(availability: StatusAvailability) -> HostedStatusAvailability {
    match availability {
        StatusAvailability::Ready => HostedStatusAvailability::Ready,
        StatusAvailability::Degraded => HostedStatusAvailability::Degraded,
        StatusAvailability::Unavailable => HostedStatusAvailability::Unavailable,
    }
}

fn status_freshness_to_dto(freshness: StatusSnapshotFreshness) -> HostedStatusFreshness {
    match freshness {
        StatusSnapshotFreshness::Fresh => HostedStatusFreshness::Fresh,
        StatusSnapshotFreshness::Stale => HostedStatusFreshness::Stale,
        StatusSnapshotFreshness::Unknown => HostedStatusFreshness::Unknown,
    }
}

/// Classify a compare-and-swap mutation failure. Only failures that provably
/// never reached the server, or that the server decided, are `Rejected`; a lost
/// response may have applied, so everything else is `Ambiguous`.
fn compare_and_swap_call_failure(error: ControlPlaneError) -> CompareAndSwapError {
    match &error {
        ControlPlaneError::Rejected { .. }
        | ControlPlaneError::Internal { .. }
        | ControlPlaneError::ContractViolation {
            failure: WireContractFailure::Request,
            ..
        } => CompareAndSwapError::rejected(error),
        _ => CompareAndSwapError::ambiguous(error),
    }
}

fn status_fact_to_dto(fact: &StatusFact) -> ControlPlaneResult<HostedStatusFact> {
    Ok(HostedStatusFact {
        id: fact.id.as_str().to_string(),
        kind: fact.kind.as_str().to_string(),
        source: fact.source.as_str().to_string(),
        scope: status_fact_scope_to_dto(fact.scope),
        scope_id: fact.scope_id.clone(),
        availability_impact: status_fact_availability_to_dto(fact.availability_impact),
        attention_impact: status_fact_attention_to_dto(fact.attention_impact),
        summary_key: fact.summary_key.clone(),
        // The control-plane client publishes an empty parameters map, matching
        // the prior hand-assembled request.
        parameters: BTreeMap::new(),
        action: fact
            .action
            .as_ref()
            .map(|action| HostedStatusActionReference {
                kind: action.kind.clone(),
                target_id: action.target_id.clone(),
            }),
        observed_at: wire_timestamp_from_domain(&fact.observed_at, "observedAt")?,
        stale_after: fact
            .stale_after
            .as_deref()
            .map(|value| wire_timestamp_from_domain(value, "staleAfter"))
            .transpose()?,
        dedupe_key: fact.dedupe_key.as_str().to_string(),
    })
}

fn status_fact_scope_to_dto(scope: StatusFactScope) -> HostedStatusFactScope {
    match scope {
        StatusFactScope::Account => HostedStatusFactScope::Account,
        StatusFactScope::Workspace => HostedStatusFactScope::Workspace,
        StatusFactScope::Project => HostedStatusFactScope::Project,
        StatusFactScope::Device => HostedStatusFactScope::Device,
        StatusFactScope::Session => HostedStatusFactScope::Session,
        StatusFactScope::WorkView => HostedStatusFactScope::WorkView,
        StatusFactScope::Lease => HostedStatusFactScope::Lease,
        StatusFactScope::Path => HostedStatusFactScope::Path,
    }
}

fn status_fact_availability_to_dto(
    impact: StatusFactAvailabilityImpact,
) -> HostedStatusFactAvailabilityImpact {
    match impact {
        StatusFactAvailabilityImpact::None => HostedStatusFactAvailabilityImpact::None,
        StatusFactAvailabilityImpact::Degraded => HostedStatusFactAvailabilityImpact::Degraded,
        StatusFactAvailabilityImpact::Unavailable => {
            HostedStatusFactAvailabilityImpact::Unavailable
        }
    }
}

fn status_fact_attention_to_dto(impact: StatusAttention) -> HostedStatusFactAttentionImpact {
    match impact {
        StatusAttention::None => HostedStatusFactAttentionImpact::None,
        StatusAttention::Recommended => HostedStatusFactAttentionImpact::Recommended,
        StatusAttention::Required => HostedStatusFactAttentionImpact::Required,
    }
}

fn event_watermarks_to_dto(watermarks: &StatusEventWatermarks) -> HostedEventWatermarks {
    HostedEventWatermarks {
        last_event_id: watermarks
            .last_event_id
            .as_ref()
            .map(|event_id| event_id.as_str().to_string()),
        last_scan_at: watermarks.last_scan_at.clone(),
    }
}

fn sync_queue_to_dto(queue: &StatusSyncQueueSnapshot) -> HostedSyncQueue {
    HostedSyncQueue {
        attention: queue.attention,
        blocked_offline: queue.blocked_offline,
        claimed: queue.claimed,
        completed: queue.completed,
        queued: queue.queued,
        reconciliation_required: queue.reconciliation_required,
        waiting_retry: queue.waiting_retry,
    }
}

fn workspace_summary_to_dto(summary: &StatusWorkspaceSummarySnapshot) -> HostedWorkspaceSummary {
    HostedWorkspaceSummary {
        total_projects: summary.total_projects,
        repo_count: summary.repo_count,
        env_file_count: summary.env_file_count,
    }
}

fn status_item_to_dto(item: &StatusItemSnapshot) -> HostedStatusItem {
    HostedStatusItem {
        kind: item.kind.clone(),
        summary: item.summary.clone(),
        path: item.path.clone(),
        event_name: item.event_name.clone(),
    }
}

fn status_limit_to_dto(limit: &StatusLimitSnapshot) -> HostedLimitedCapability {
    HostedLimitedCapability {
        capability: limit.capability.clone(),
        support_capability: limit.support_capability.clone(),
        unavailable_because: limit.unavailable_because.clone(),
        path: limit.path.clone(),
        still_works: limit.still_works.clone(),
    }
}

#[cfg(test)]
mod boundary_tests {
    use super::*;

    #[test]
    fn event_kind_dto_maps_every_wire_kind_without_failing() {
        assert_eq!(
            event_kind_from_dto(HostedCompactEventKind::WorkspaceRefAdvanced),
            CompactEventKind::WorkspaceRefAdvanced
        );
        // Kinds the server routinely emits (workspace key rotation, membership,
        // namespace lifecycle) used to poison the whole listing.
        assert_eq!(
            event_kind_from_dto(HostedCompactEventKind::WorkspaceKeyRotated),
            CompactEventKind::WorkspaceKeyRotated
        );
        assert_eq!(
            event_kind_from_dto(HostedCompactEventKind::NamespaceArchived),
            CompactEventKind::NamespaceArchived
        );
        assert_eq!(
            event_kind_from_dto(HostedCompactEventKind::WorkspaceStatusPublished),
            CompactEventKind::WorkspaceStatusPublished
        );
    }

    fn status_snapshot() -> WorkspaceStatusSnapshot {
        WorkspaceStatusSnapshot {
            workspace_id: WorkspaceId::new("workspace_1"),
            snapshot_id: SnapshotId::new("snap_1"),
            availability: StatusAvailability::Ready,
            attention: StatusAttention::None,
            primary_fact_id: None,
            facts: Vec::new(),
            freshness: StatusSnapshotFreshness::Fresh,
            schema_hash: "hash".to_string(),
            snapshot_version: 1,
            producer_version: "1.0.0".to_string(),
            observed_at: "2026-07-02T12:00:00Z".to_string(),
            attention_items: Vec::new(),
            event_watermarks: StatusEventWatermarks::default(),
            sync_queue: None,
            workspace_summary: None,
            items: Vec::new(),
            limits: Vec::new(),
            published_by_device_id: DeviceId::new("device_1"),
        }
    }

    #[test]
    fn status_request_mirrors_observed_at_and_maps_enums() {
        let request =
            status_publish_request_from_snapshot(&status_snapshot(), "proof_publish".to_string())
                .expect("snapshot fixture publishes");
        assert_eq!(request.attention, HostedStatusAttention::None);
        assert_eq!(request.availability, HostedStatusAvailability::Ready);
        assert_eq!(request.freshness, HostedStatusFreshness::Fresh);
        // generatedAt mirrors observedAt, matching the prior hand-assembled request.
        assert_eq!(request.generated_at, request.observed_at);
        assert_eq!(request.generated_at.as_str(), "2026-07-02T12:00:00Z");
        // Empty collections are omitted, matching the prior request.
        assert!(request.items.is_none());
        assert!(request.limits.is_none());
        assert_eq!(request.published_by_device_proof, "proof_publish");
    }

    #[test]
    fn signed_proof_subject_matches_the_wire_availability_and_attention() {
        let mut snapshot = status_snapshot();
        snapshot.availability = StatusAvailability::Degraded;
        snapshot.attention = StatusAttention::Required;
        let request = status_publish_request_from_snapshot(&snapshot, "proof_publish".to_string())
            .expect("snapshot fixture publishes");
        assert_eq!(request.availability, HostedStatusAvailability::Degraded);
        assert_eq!(request.attention, HostedStatusAttention::Required);
        let subject = snapshot.proof_subject();
        assert!(subject.contains("availability=degraded"), "got: {subject}");
        assert!(subject.contains("attention=required"), "got: {subject}");
    }
}
