use std::{collections::BTreeSet, fs, path::PathBuf};

use serde::Deserialize;
use serde_json::Value as JsonValue;

use super::*;

const RUST_BUILDERS: &[&str] = &[
    "statusPublish",
    "bootstrapSession",
    "deviceRequestApproval",
    "deviceRequestDenial",
    "deviceRevocation",
    "conflictReconcile",
    "conflictList",
    "workspaceRef",
    "workspaceHead",
    "uploadIntent",
    "downloadIntent",
    "uploadVerification",
    "objectRetention",
    "metadataBindings",
    "resolveMetadataBindings",
    "snapshotRoot",
    "objectMetadata",
    "objectPointer",
    "snapshotRootQuery",
    "headObjectMetadata",
    "workViewCreate",
    "workViewLifecycle",
    "workViewOverlay",
    "workViewList",
    "workViewRestore",
    "leaseCreate",
    "leaseUpdate",
    "leaseList",
    "recoveryEnvelopeCreate",
    "recoveryEnvelopeVerify",
    "recoveryEnvelopeRotate",
    "recoveryEnvelopeRevoke",
];

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FixtureFile {
    cases: Vec<FixtureCase>,
    projection_invariants: Vec<ProjectionInvariant>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FixtureCase {
    action: String,
    builder: String,
    case_name: String,
    implementations: Vec<String>,
    input: JsonValue,
    expected_subject: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectionInvariant {
    builder: String,
    case_name: String,
    implementations: Vec<String>,
    base_input: JsonValue,
    unsigned_transport_input: JsonValue,
    signed_transport_input: JsonValue,
    expected_subject: String,
    expected_signed_subject: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StatusInput {
    workspace_id: String,
    snapshot_id: String,
    availability: String,
    attention: String,
    schema_hash: String,
    snapshot_version: u64,
    observed_at: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BootstrapInput {
    workspace_id: String,
    host: Option<String>,
    lease_handoff_digest: Option<String>,
    lease_id: Option<String>,
    root: Option<String>,
    runtime: Option<String>,
    setup_receipts_digest: Option<String>,
    expires_in_ticks: Option<u64>,
    bootstrap_token_hash: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeviceRequestProofInput {
    request_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeviceRevocationProofInput {
    device_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecoveryEnvelopeProofInput {
    envelope_id: String,
    ciphertext: Option<String>,
    fingerprint: Option<String>,
    recovery_proof_verifier: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConflictReconcileInput {
    conflict_id: String,
    conflict_kind: String,
    paths: Vec<String>,
    base_snapshot_id: String,
    remote_snapshot_id: String,
    contains_secrets: bool,
    desired_state: String,
    occurrence_version: u64,
    reason: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceIdInput {
    workspace_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceRefInput {
    expected_version: u64,
    next_snapshot_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceHeadInput {
    workspace_id: String,
    version: u64,
    snapshot_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadIntentInput {
    object_key: String,
    kind: String,
    byte_length: u64,
    content_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DownloadIntentInput {
    object_key: String,
    range: Option<RangeInput>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RangeInput {
    offset: u64,
    length: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadVerificationInput {
    object_key: String,
    byte_length: u64,
    content_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ObjectRetentionInput {
    object_key: String,
    retention_state: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ObjectPointerInput {
    object_key: String,
    content_id: String,
    byte_length: u64,
    hash: String,
    key_epoch: u32,
    kind: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MetadataBindingsInput {
    bindings: Vec<MetadataBindingFixtureInput>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MetadataBindingFixtureInput {
    logical_id: String,
    record_kind: String,
    object: ObjectPointerInput,
    sidecar: MetadataSidecarInput,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MetadataSidecarInput {
    child_logical_ids: Vec<String>,
    direct_object_keys: Vec<String>,
    digest: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LogicalIdsInput {
    logical_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotRootInput {
    snapshot_id: String,
    manifest_id: String,
    manifest_object: ObjectPointerInput,
    namespace_root_id: String,
    extra_root_logical_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotInput {
    snapshot_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ObjectKeyInput {
    object_key: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkViewCreateInput {
    work_view_id: String,
    project_id: String,
    name: String,
    visible_path: String,
    base_snapshot_id: String,
    base_workspace_version: u64,
    expires_at: Option<String>,
    retain_until: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkViewLifecycleInput {
    work_view_id: String,
    lifecycle: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkViewOverlayInput {
    work_view_id: String,
    expected_overlay_version: u64,
    overlay_object: ObjectPointerInput,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkViewListInput {
    include_all: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkViewRestoreInput {
    work_view_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LeaseCreateInput {
    lease_id: String,
    project_id: String,
    write_target_mode: String,
    work_view_id: Option<String>,
    base_snapshot_id: String,
    task_label: Option<String>,
    session_state: Option<String>,
    status_code: String,
    expires_at: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LeaseUpdateInput {
    lease_id: String,
    expected_version: u64,
    event_kind: Option<String>,
    session_state: Option<String>,
    status_code: Option<String>,
}

#[test]
fn device_proof_subjects_match_shared_contract_fixture() {
    let fixture = load_fixture();
    let mut checked = 0;
    for case in fixture
        .cases
        .iter()
        .filter(|case| has_implementation(case, "rust"))
    {
        checked += 1;
        assert_eq!(
            build_subject(case),
            case.expected_subject,
            "{} / {}",
            case.builder,
            case.case_name
        );
        assert!(
            builder_allows_action(&case.builder, &case.action),
            "{} / {} uses unexpected action {}",
            case.builder,
            case.case_name,
            case.action
        );
    }
    assert!(
        checked >= 40,
        "expected at least 40 rust-backed case checks"
    );
}

fn builder_allows_action(builder: &str, action: &str) -> bool {
    let allowed: &[&str] = match builder {
        "bootstrapSession" => &["create-bootstrap-session"],
        "deviceRequestApproval" => &["approve-device-request"],
        "deviceRequestDenial" => &["deny-device-request"],
        "deviceRevocation" => &["revoke-device"],
        "conflictReconcile" => &["reconcile-conflict-occurrence"],
        "conflictList" => &["list-workspace-conflicts"],
        "workspaceRef" => &["compare-and-swap-workspace-ref"],
        "workspaceHead" => &["sign-workspace-head"],
        "uploadIntent" => &["create-upload-intent"],
        "downloadIntent" => &["create-download-intent"],
        "uploadVerification" => &["verify-upload-intent"],
        "objectRetention" => &["mark-object-retention-state"],
        "metadataBindings" => &["commit-metadata-bindings"],
        "resolveMetadataBindings" => &["resolve-metadata-bindings"],
        "snapshotRoot" => &["commit-snapshot-root"],
        "objectMetadata" => &["commit-uploaded-object-metadata"],
        "objectPointer" => &["fragment", "fixture-only"],
        "snapshotRootQuery" => &["get-snapshot-root"],
        "headObjectMetadata" => &["head-object-metadata"],
        "statusPublish" => &["publish-workspace-status"],
        "workViewCreate" => &["create-work-view"],
        "workViewLifecycle" => &["update-work-view-lifecycle"],
        "workViewOverlay" => &["commit-work-view-overlay"],
        "workViewList" => &["list-work-views"],
        "workViewRestore" => &["restore-work-view"],
        "leaseCreate" => &["create-lease"],
        "leaseUpdate" => &["update-lease"],
        "leaseList" => &["list-leases"],
        "recoveryEnvelopeCreate" => &["create-recovery-envelope"],
        "recoveryEnvelopeVerify" => &["verify-recovery-envelope"],
        "recoveryEnvelopeRotate" => &["rotate-recovery-envelope"],
        "recoveryEnvelopeRevoke" => &["revoke-recovery-envelope"],
        _ => return false,
    };
    allowed.contains(&action)
}

#[test]
fn rust_builder_registry_matches_fixture_inventory() {
    let fixture = load_fixture();
    let fixture_builders = fixture
        .cases
        .iter()
        .filter(|case| has_implementation(case, "rust"))
        .map(|case| case.builder.as_str())
        .collect::<BTreeSet<_>>();
    let registry_builders = RUST_BUILDERS.iter().copied().collect::<BTreeSet<_>>();
    assert_eq!(fixture_builders, registry_builders);
}

#[test]
fn proof_projection_fixture_locks_signed_and_unsigned_transport_behavior() {
    let fixture = load_fixture();
    let invariants = fixture
        .projection_invariants
        .iter()
        .filter(|invariant| {
            invariant
                .implementations
                .iter()
                .any(|implementation| implementation == "rust")
        })
        .collect::<Vec<_>>();
    assert!(
        !invariants.is_empty(),
        "expected rust projection invariants"
    );

    for invariant in invariants {
        let base = invariant_subject(invariant, invariant.base_input.clone());
        let unsigned = invariant_subject(invariant, invariant.unsigned_transport_input.clone());
        let signed = invariant_subject(invariant, invariant.signed_transport_input.clone());

        assert_eq!(
            base, invariant.expected_subject,
            "{} base",
            invariant.case_name
        );
        assert_eq!(
            unsigned, base,
            "{} unsigned transport field",
            invariant.case_name
        );
        assert_eq!(
            signed, invariant.expected_signed_subject,
            "{} signed transport field",
            invariant.case_name
        );
        assert_ne!(
            signed, base,
            "{} signed transport field",
            invariant.case_name
        );
    }
}

fn invariant_subject(invariant: &ProjectionInvariant, input: JsonValue) -> String {
    match invariant.builder.as_str() {
        "workspaceHead" => {
            let input = serde_json::from_value::<WorkspaceHeadInput>(input)
                .expect("workspace head projection fixture parses");
            workspace_head_proof_subject(&input.workspace_id, input.version, &input.snapshot_id)
        }
        _ => panic!("unknown projection invariant builder {}", invariant.builder),
    }
}

fn build_subject(case: &FixtureCase) -> String {
    match case.builder.as_str() {
        "statusPublish" => status_publish_subject(case),
        "bootstrapSession" => bootstrap_session_subject(case),
        "deviceRequestApproval" | "deviceRequestDenial" => crate::device_request_proof_subject(
            &deserialize::<DeviceRequestProofInput>(case).request_id,
        ),
        "deviceRevocation" => crate::device_revocation_proof_subject(
            &deserialize::<DeviceRevocationProofInput>(case).device_id,
        ),
        "conflictReconcile" => {
            let input = deserialize::<ConflictReconcileInput>(case);
            conflict_reconcile_proof_subject(&ConflictOccurrenceReconcile {
                workspace_id: WorkspaceId::new("ws_fixture"),
                conflict_id: ConflictId::new(input.conflict_id),
                conflict_kind: input.conflict_kind,
                paths: input.paths,
                contains_secrets: input.contains_secrets,
                base_snapshot_id: SnapshotId::new(input.base_snapshot_id),
                remote_snapshot_id: SnapshotId::new(input.remote_snapshot_id),
                occurrence_version: input.occurrence_version,
                desired_state: parse_conflict_occurrence_state(&input.desired_state),
                device_id: DeviceId::new("device_fixture"),
                reason: input.reason,
                bundle_object: None,
            })
        }
        "conflictList" => {
            conflict_list_proof_subject(&deserialize::<WorkspaceIdInput>(case).workspace_id)
        }
        "workspaceRef" => {
            let input = deserialize::<WorkspaceRefInput>(case);
            workspace_ref_proof_subject(input.expected_version, &input.next_snapshot_id)
        }
        "workspaceHead" => {
            let input = deserialize::<WorkspaceHeadInput>(case);
            workspace_head_proof_subject(&input.workspace_id, input.version, &input.snapshot_id)
        }
        "uploadIntent" => {
            let input = deserialize::<UploadIntentInput>(case);
            upload_intent_proof_subject(
                &input.object_key,
                parse_object_kind_for_fixture(&input.kind),
                input.byte_length,
                input.content_id.as_deref(),
            )
        }
        "downloadIntent" => {
            let input = deserialize::<DownloadIntentInput>(case);
            download_intent_proof_subject(
                &input.object_key,
                input.range.map(|range| bowline_storage::ByteRange {
                    offset: range.offset,
                    length: range.length,
                }),
            )
        }
        "uploadVerification" => {
            let input = deserialize::<UploadVerificationInput>(case);
            upload_verification_proof_subject(
                &input.object_key,
                input.byte_length,
                input.content_id.as_deref(),
            )
        }
        "objectRetention" => {
            let input = deserialize::<ObjectRetentionInput>(case);
            object_retention_proof_subject(
                &input.object_key,
                parse_retention_state(&input.retention_state).expect("retention state fixture"),
            )
        }
        "metadataBindings" => {
            let input = deserialize::<MetadataBindingsInput>(case);
            let bindings = input
                .bindings
                .into_iter()
                .map(|binding| MetadataBindingInput {
                    logical_id: binding.logical_id,
                    record_kind: match binding.record_kind.as_str() {
                        "namespace-page" => MetadataRecordKind::NamespacePage,
                        "content-layout" => MetadataRecordKind::ContentLayout,
                        "segment-page" => MetadataRecordKind::SegmentPage,
                        value => panic!("invalid metadata record kind fixture: {value}"),
                    },
                    object: object_pointer(binding.object),
                    sidecar: MetadataSidecar {
                        child_logical_ids: binding.sidecar.child_logical_ids,
                        direct_object_keys: binding.sidecar.direct_object_keys,
                        digest: binding.sidecar.digest,
                    },
                })
                .collect::<Vec<_>>();
            metadata_bindings_proof_subject(&bindings)
        }
        "resolveMetadataBindings" => resolve_metadata_bindings_proof_subject(
            &deserialize::<LogicalIdsInput>(case).logical_ids,
        ),
        "snapshotRoot" => {
            let input = deserialize::<SnapshotRootInput>(case);
            snapshot_root_proof_subject(&SnapshotRootCommit {
                workspace_id: WorkspaceId::new("ws_fixture"),
                snapshot_id: SnapshotId::new(input.snapshot_id),
                manifest_id: ManifestId::new(input.manifest_id),
                manifest_object: object_pointer(input.manifest_object),
                namespace_root_id: input.namespace_root_id,
                extra_root_logical_ids: input.extra_root_logical_ids,
                committed_by_device_id: DeviceId::new("device_fixture"),
            })
        }
        "objectMetadata" => object_metadata_proof_subject(&object_pointer(deserialize(case))),
        "objectPointer" => object_pointer_proof_subject(&object_pointer(deserialize(case))),
        "snapshotRootQuery" => {
            snapshot_root_query_proof_subject(&deserialize::<SnapshotInput>(case).snapshot_id)
        }
        "headObjectMetadata" => deserialize::<ObjectKeyInput>(case).object_key,
        "workViewCreate" => {
            let input = deserialize::<WorkViewCreateInput>(case);
            work_view_create_proof_subject(&WorkViewCreate {
                workspace_id: WorkspaceId::new("ws_fixture"),
                work_view_id: WorkViewId::new(input.work_view_id),
                project_id: ProjectId::new(input.project_id),
                name: input.name,
                visible_path: input.visible_path,
                base_snapshot_id: SnapshotId::new(input.base_snapshot_id),
                base_workspace_version: input.base_workspace_version,
                expires_at: input.expires_at,
                retain_until: input.retain_until,
                created_by_device_id: DeviceId::new("device_fixture"),
            })
        }
        "workViewLifecycle" => {
            let input = deserialize::<WorkViewLifecycleInput>(case);
            work_view_lifecycle_proof_subject(&WorkViewLifecycleUpdate {
                workspace_id: WorkspaceId::new("ws_fixture"),
                work_view_id: WorkViewId::new(input.work_view_id),
                lifecycle: parse_work_view_lifecycle(&input.lifecycle)
                    .expect("work view lifecycle fixture"),
                updated_by_device_id: DeviceId::new("device_fixture"),
            })
        }
        "workViewOverlay" => {
            let input = deserialize::<WorkViewOverlayInput>(case);
            work_view_overlay_proof_subject(&WorkViewOverlayCommit {
                workspace_id: WorkspaceId::new("ws_fixture"),
                work_view_id: WorkViewId::new(input.work_view_id),
                expected_overlay_version: input.expected_overlay_version,
                overlay_object: object_pointer(input.overlay_object),
                committed_by_device_id: DeviceId::new("device_fixture"),
            })
        }
        "workViewList" => {
            work_view_list_proof_subject(deserialize::<WorkViewListInput>(case).include_all)
        }
        "workViewRestore" => {
            work_view_restore_proof_subject(&deserialize::<WorkViewRestoreInput>(case).work_view_id)
        }
        "leaseCreate" => {
            let input = deserialize::<LeaseCreateInput>(case);
            lease_create_proof_subject(&LeaseCreate {
                workspace_id: WorkspaceId::new("ws_fixture"),
                lease_id: LeaseId::new(input.lease_id),
                project_id: ProjectId::new(input.project_id),
                device_id: DeviceId::new("device_fixture"),
                target_device_ref: None,
                origin_device_ref: None,
                write_target_mode: parse_lease_write_target_mode(&input.write_target_mode)
                    .expect("lease write target fixture"),
                work_view_id: input.work_view_id.map(WorkViewId::new),
                base_snapshot_id: SnapshotId::new(input.base_snapshot_id),
                task_label: input.task_label,
                session_state: input
                    .session_state
                    .as_deref()
                    .map(parse_lease_session_state_for_fixture)
                    .unwrap_or(LeaseSessionState::Open),
                status_code: input.status_code,
                expires_at: timestamp_string(input.expires_at),
            })
        }
        "leaseUpdate" => {
            let input = deserialize::<LeaseUpdateInput>(case);
            lease_update_proof_subject(&LeaseUpdate {
                workspace_id: WorkspaceId::new("ws_fixture"),
                lease_id: LeaseId::new(input.lease_id),
                expected_version: input.expected_version,
                updated_by_device_id: DeviceId::new("device_fixture"),
                session_state: input
                    .session_state
                    .as_deref()
                    .map(parse_lease_session_state_for_fixture),
                status_code: input.status_code,
                event_kind: input
                    .event_kind
                    .as_deref()
                    .map(parse_event_kind_for_fixture),
            })
        }
        "leaseList" => LEASE_LIST_PROOF_SUBJECT.to_string(),
        "recoveryEnvelopeCreate" | "recoveryEnvelopeRotate" => {
            let input = deserialize::<RecoveryEnvelopeProofInput>(case);
            crate::recovery_envelope_payload_proof_subject(&RecoveryEnvelopeInput {
                workspace_id: WorkspaceId::new("ws_fixture"),
                envelope_id: RecoveryEnvelopeId::new(input.envelope_id),
                created_by_device_id: DeviceId::new("device_fixture"),
                created_by_device_proof: String::new(),
                ciphertext: input.ciphertext.expect("recovery fixture ciphertext"),
                fingerprint: input.fingerprint.expect("recovery fixture fingerprint"),
                recovery_proof_verifier: input
                    .recovery_proof_verifier
                    .expect("recovery fixture proof verifier"),
            })
        }
        "recoveryEnvelopeVerify" | "recoveryEnvelopeRevoke" => {
            crate::recovery_envelope_proof_subject(
                &deserialize::<RecoveryEnvelopeProofInput>(case).envelope_id,
            )
        }
        _ => panic!(
            "unknown rust proof builder {} / {}",
            case.builder, case.case_name
        ),
    }
}

fn status_publish_subject(case: &FixtureCase) -> String {
    let input = deserialize::<StatusInput>(case);
    WorkspaceStatusSnapshot {
        workspace_id: WorkspaceId::new(input.workspace_id),
        snapshot_id: SnapshotId::new(input.snapshot_id),
        availability: input.availability,
        attention: input.attention,
        primary_fact_id: None,
        facts: Vec::new(),
        freshness: "fresh".to_string(),
        schema_hash: input.schema_hash,
        snapshot_version: input.snapshot_version,
        producer_version: "fixture".to_string(),
        observed_at: input.observed_at.clone(),
        attention_items: Vec::new(),
        event_watermarks: StatusEventWatermarks::default(),
        sync_queue: None,
        workspace_summary: None,
        items: Vec::new(),
        limits: Vec::new(),
        published_by_device_id: DeviceId::new("device-fixture"),
    }
    .proof_subject()
}

fn bootstrap_session_subject(case: &FixtureCase) -> String {
    let input = deserialize::<BootstrapInput>(case);
    bootstrap_session_proof_subject(
        &BootstrapSessionInput {
            workspace_id: WorkspaceId::new(input.workspace_id),
            host: input.host,
            lease_handoff_digest: input.lease_handoff_digest,
            lease_id: input.lease_id.map(LeaseId::new),
            root: input.root,
            runtime: input.runtime,
            setup_receipts_digest: input.setup_receipts_digest,
            expires_in_ticks: input.expires_in_ticks.unwrap_or(600),
        },
        &input.bootstrap_token_hash,
    )
}

fn has_implementation(case: &FixtureCase, implementation: &str) -> bool {
    case.implementations
        .iter()
        .any(|candidate| candidate == implementation)
}

fn object_pointer(input: ObjectPointerInput) -> ObjectPointer {
    ObjectPointer {
        object_key: input.object_key,
        content_id: ContentId::new(input.content_id),
        byte_len: input.byte_length,
        hash: input.hash,
        key_epoch: input.key_epoch,
        kind: parse_object_kind_for_fixture(&input.kind),
        created_at: ControlPlaneTimestamp { tick: 0 },
    }
}

fn parse_conflict_occurrence_state(value: &str) -> ConflictOccurrenceState {
    match value {
        "unresolved" => ConflictOccurrenceState::Unresolved,
        "accepted" => ConflictOccurrenceState::Accepted,
        "rejected" => ConflictOccurrenceState::Rejected,
        _ => panic!("unknown conflict occurrence fixture {value}"),
    }
}

fn parse_object_kind_for_fixture(value: &str) -> ObjectKind {
    parse_object_kind(value).expect("object kind fixture")
}

fn parse_lease_session_state_for_fixture(value: &str) -> LeaseSessionState {
    parse_lease_session_state(value).expect("lease execution state fixture")
}

fn parse_event_kind_for_fixture(value: &str) -> CompactEventKind {
    parse_event_kind(value).expect("compact event kind fixture")
}

fn timestamp_string(value: String) -> ControlPlaneTimestamp {
    let tick = value
        .strip_prefix('t')
        .unwrap_or(&value)
        .parse::<u64>()
        .expect("timestamp fixture tick parses");
    ControlPlaneTimestamp { tick }
}

fn deserialize<T>(case: &FixtureCase) -> T
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_value(case.input.clone()).unwrap_or_else(|error| {
        panic!(
            "{} / {} input parses: {error}",
            case.builder, case.case_name
        )
    })
}

fn load_fixture() -> FixtureFile {
    let text = fs::read_to_string(fixture_path()).expect("proof fixture is readable");
    serde_json::from_str(&text).expect("proof fixture parses")
}

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/contracts/proofs/device-proof-subjects.json")
}
