use std::{collections::BTreeSet, fs, path::PathBuf};

use serde::Deserialize;
use serde_json::Value as JsonValue;

use super::*;
use crate::Sha256Checksum;
use bowline_core::ids::ContentId;
use bowline_core::status::{StatusAttention, StatusAvailability, StatusSnapshotFreshness};

const RUST_BUILDERS: &[&str] = &[
    "statusPublish",
    "bootstrapSession",
    "deviceRequestApproval",
    "deviceRequestDenial",
    "deviceGrantFetch",
    "deviceRevocation",
    "workspaceRef",
    "workspaceHead",
    "uploadIntent",
    "downloadIntent",
    "uploadVerification",
    "objectRetention",
    "objectMetadata",
    "objectPointer",
    "headObjectMetadata",
    "storageGcList",
    "storageGcObject",
    "recoveryEnvelopeCreate",
    "recoveryEnvelopeVerify",
    "recoveryEnvelopeRotate",
    "recoveryEnvelopeRevoke",
    "devicePublicKey",
    "keyRegrantWork",
    "keyRegrantAccept",
    "keyRegrantSeed",
    "keyRegrantOffer",
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
    // Deserialized straight into the domain enums: the fixture spelling and the
    // signed proof-subject spelling are then the same value, not two strings
    // that happen to agree.
    availability: StatusAvailability,
    attention: StatusAttention,
    schema_hash: String,
    snapshot_version: u64,
    observed_at: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BootstrapInput {
    workspace_id: String,
    host: Option<String>,
    root: Option<String>,
    runtime: Option<String>,
    setup_receipts_digest: Option<String>,
    expires_in_ticks: Option<u64>,
    bootstrap_token_hash: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeviceRequestProofInput {
    request_id: DeviceApprovalRequestId,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeviceRevocationProofInput {
    device_id: DeviceId,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DevicePublicKeyProofInput {
    device_public_key: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct KeyRegrantWorkProofInput {
    workspace_id: WorkspaceId,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct KeyRegrantEpochProofInput {
    key_epoch: u32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct KeyRegrantOfferProofInput {
    key_epoch: u32,
    offers: Vec<KeyRegrantOfferFixture>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct KeyRegrantOfferFixture {
    recipient_device_id: DeviceId,
    acceptance_proof_verifier: String,
    ciphertext: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecoveryEnvelopeProofInput {
    envelope_id: RecoveryEnvelopeId,
    ciphertext: Option<String>,
    fingerprint: Option<String>,
    recovery_proof_verifier: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceRefInput {
    expected_version: u64,
    next_snapshot_id: SnapshotId,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceHeadInput {
    workspace_id: WorkspaceId,
    version: u64,
    snapshot_id: SnapshotId,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadIntentInput {
    object_key: String,
    kind: String,
    byte_length: u64,
    checksum_sha256: String,
    content_id: Option<ContentId>,
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
    content_id: Option<ContentId>,
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
    content_id: ContentId,
    byte_length: u64,
    hash: String,
    key_epoch: u32,
    kind: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ObjectKeyInput {
    object_key: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GcCursorInput {
    cursor: Option<String>,
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
    // The Plan 111 cutover trimmed the fixture to the surviving proof
    // vocabulary; every remaining rust-backed case must still be checked.
    assert!(
        checked >= 25,
        "expected at least 25 rust-backed case checks"
    );
}

fn builder_allows_action(builder: &str, action: &str) -> bool {
    let allowed: &[&str] = match builder {
        "bootstrapSession" => &["create-bootstrap-session"],
        "deviceRequestApproval" => &["approve-device-request"],
        "deviceRequestDenial" => &["deny-device-request"],
        "deviceGrantFetch" => &[crate::FETCH_DEVICE_GRANT_ACTION],
        "deviceRevocation" => &["revoke-device"],
        "devicePublicKey" => &["attest-device-public-key"],
        "keyRegrantWork" => &["list-workspace-key-regrant-work"],
        "keyRegrantAccept" => &["accept-workspace-key-regrant"],
        "keyRegrantSeed" => &["seed-workspace-key-epoch"],
        "keyRegrantOffer" => &["offer-workspace-key-regrant"],
        "workspaceRef" => &["compare-and-swap-workspace-ref"],
        "workspaceHead" => &["sign-workspace-head"],
        "uploadIntent" => &["create-upload-intent"],
        "downloadIntent" => &["create-download-intent"],
        "uploadVerification" => &["verify-upload-intent"],
        "objectRetention" => &["mark-object-retention-state"],
        "objectMetadata" => &["commit-uploaded-object-metadata"],
        "objectPointer" => &["fragment", "fixture-only"],
        "headObjectMetadata" => &["head-object-metadata"],
        "storageGcList" => &["list-storage-gc-objects"],
        "storageGcObject" => &[
            "create-storage-gc-delete-intent",
            "delete-object-metadata-after-gc",
        ],
        "statusPublish" => &["publish-workspace-status"],
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
        "deviceRequestApproval" | "deviceRequestDenial" | "deviceGrantFetch" => {
            crate::device_request_proof_subject(
                &deserialize::<DeviceRequestProofInput>(case).request_id,
            )
        }
        "deviceRevocation" => crate::device_revocation_proof_subject(
            &deserialize::<DeviceRevocationProofInput>(case).device_id,
        ),
        "devicePublicKey" => crate::device_public_key_proof_subject(
            &deserialize::<DevicePublicKeyProofInput>(case).device_public_key,
        ),
        "keyRegrantWork" => crate::key_regrant_work_proof_subject(
            &deserialize::<KeyRegrantWorkProofInput>(case).workspace_id,
        ),
        "keyRegrantAccept" => crate::key_regrant_accept_proof_subject(
            deserialize::<KeyRegrantEpochProofInput>(case).key_epoch,
        ),
        "keyRegrantSeed" | "keyRegrantOffer" => {
            let input = deserialize::<KeyRegrantOfferProofInput>(case);
            let offers = input
                .offers
                .into_iter()
                .map(|offer| crate::WorkspaceKeyRegrantOffer {
                    acceptance_proof_verifier: offer.acceptance_proof_verifier,
                    ciphertext: offer.ciphertext,
                    recipient_device_id: offer.recipient_device_id,
                })
                .collect::<Vec<_>>();
            crate::key_regrant_offer_proof_subject(input.key_epoch, &offers)
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
            let checksum_sha256 = Sha256Checksum::for_bytes(b"");
            assert_eq!(input.checksum_sha256, checksum_sha256.as_str());
            upload_intent_proof_subject(
                &input.object_key,
                parse_object_kind_for_fixture(&input.kind),
                input.byte_length,
                &checksum_sha256,
                input.content_id.as_ref(),
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
                input.content_id.as_ref(),
            )
        }
        "objectRetention" => {
            let input = deserialize::<ObjectRetentionInput>(case);
            object_retention_proof_subject(
                &input.object_key,
                parse_retention_state(&input.retention_state).expect("retention state fixture"),
            )
        }
        "objectMetadata" => object_metadata_proof_subject(&object_pointer(deserialize(case))),
        "objectPointer" => object_pointer_proof_subject(&object_pointer(deserialize(case))),
        "headObjectMetadata" => deserialize::<ObjectKeyInput>(case).object_key,
        "storageGcList" => {
            storage_gc_list_proof_subject(deserialize::<GcCursorInput>(case).cursor.as_deref())
        }
        "storageGcObject" => {
            storage_gc_object_proof_subject(&deserialize::<ObjectKeyInput>(case).object_key)
        }
        "recoveryEnvelopeCreate" | "recoveryEnvelopeRotate" => {
            let input = deserialize::<RecoveryEnvelopeProofInput>(case);
            crate::recovery_envelope_payload_proof_subject(&RecoveryEnvelopeInput {
                workspace_id: WorkspaceId::new("ws_fixture"),
                envelope_id: input.envelope_id,
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
        freshness: StatusSnapshotFreshness::Fresh,
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
        content_id: input.content_id,
        byte_len: input.byte_length,
        hash: input.hash,
        key_epoch: input.key_epoch,
        kind: parse_object_kind_for_fixture(&input.kind),
        created_at: ControlPlaneTimestamp { tick: 0 },
    }
}

fn parse_object_kind_for_fixture(value: &str) -> ObjectKind {
    parse_object_kind(value).expect("object kind fixture")
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
