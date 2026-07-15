use std::{
    env, fs, io,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use bowline_core::{
    ids::{ContentId, DeviceId, SnapshotId, WorkspaceId},
    workspace_graph::{ContentLocator, ContentStorage, workspace_content_id},
};
use bowline_storage::{
    ByteStore, LocalByteStore, LocalContentCache, ObjectKey, PackRecordInput,
    RangeHydrationRequest, StorageKey, write_source_packs,
};

use bowline_control_plane::{
    CompareAndSwapError, ControlPlaneClient, ControlPlaneError, DeviceApprovalInput,
    DeviceControlPlaneClient, DeviceRequestInput, DeviceRequestInputDraft, DownloadIntentRequest,
    FakeControlPlaneClient, FirstAuthorizedDeviceInput, GrantAcceptanceInput,
    HostedControlPlaneClient, ObjectControlPlaneClient, SignedUrlByteStore, SnapshotRootCommit,
    SnapshotRootRecord, WorkspaceControlPlaneClient,
};
use bowline_local::{
    device_keys::{DeviceIdentity, WorkspaceKeyMaterial},
    sync::{SnapshotContent, UploadOutcome, coalesce_workspace_scan, upload_snapshot_candidate},
    trust::grants,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudSpikeReport {
    pub provider: String,
    pub workspace_id: String,
    pub starting_version: u64,
    pub advanced_version: u64,
    pub pack_object_count: usize,
    pub source_file_count: usize,
    pub hydrated_cold_file_bytes: Vec<u8>,
    pub stale_ref_detected: bool,
    pub device_approval_harness_only: bool,
    pub event_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudSpikeSkip {
    pub provider: String,
    pub skipped: bool,
    pub missing_env: Vec<String>,
}

pub fn skip_hosted_cloud_spike_from_env() -> Option<CloudSpikeSkip> {
    skip_hosted_cloud_spike_with(hosted_env_value)
}

pub fn skip_hosted_cloud_spike_with(
    get_env: impl Fn(&str) -> Option<String>,
) -> Option<CloudSpikeSkip> {
    let missing_env = [
        "CONVEX_URL",
        "BOWLINE_CONTROL_PLANE_TOKEN",
        "BOWLINE_WORKOS_ACCESS_TOKEN",
    ]
    .into_iter()
    .filter(|key| get_env(key).filter(|value| !value.is_empty()).is_none())
    .map(ToOwned::to_owned)
    .collect::<Vec<_>>();

    (!missing_env.is_empty()).then_some(CloudSpikeSkip {
        provider: "hosted".to_string(),
        skipped: true,
        missing_env,
    })
}

fn hosted_device_proof_signer(
    identity: DeviceIdentity,
    device_id: DeviceId,
) -> impl Fn(&str, &str, &str, &str) -> Result<String, ControlPlaneError> + Send + Sync + 'static {
    move |workspace_id, proof_device_id, action, subject| {
        if proof_device_id != device_id.as_str() {
            return Err(ControlPlaneError::Storage(
                "hosted spike refused to sign for a different device id".to_string(),
            ));
        }
        grants::device_authorization_proof(
            &identity,
            &WorkspaceId::new(workspace_id.to_string()),
            &device_id,
            action,
            subject,
        )
        .map_err(|error| ControlPlaneError::Storage(error.to_string()))
    }
}

fn hosted_device_proof_verifier(
    identity: DeviceIdentity,
    device_id: DeviceId,
) -> impl Fn(&str, &str) -> Result<Option<String>, ControlPlaneError> + Send + Sync + 'static {
    move |_workspace_id, proof_device_id| {
        if proof_device_id != device_id.as_str() {
            return Ok(None);
        }
        grants::device_authorization_proof_verifier(&identity)
            .map(Some)
            .map_err(|error| ControlPlaneError::Storage(error.to_string()))
    }
}

fn hosted_known_device_proof_verifier(
    writer_identity: DeviceIdentity,
    writer_device: DeviceId,
    reader_identity: DeviceIdentity,
    reader_device: DeviceId,
) -> impl Fn(&str, &str) -> Result<Option<String>, ControlPlaneError> + Send + Sync + 'static {
    move |_workspace_id, proof_device_id| {
        let identity = if proof_device_id == writer_device.as_str() {
            &writer_identity
        } else if proof_device_id == reader_device.as_str() {
            &reader_identity
        } else {
            return Ok(None);
        };
        grants::device_authorization_proof_verifier(identity)
            .map(Some)
            .map_err(|error| ControlPlaneError::Storage(error.to_string()))
    }
}

fn encrypted_workspace_key_for_device_request(
    workspace_key: &WorkspaceKeyMaterial,
    request: &bowline_control_plane::DeviceRequest,
    authorizing_identity: &DeviceIdentity,
    authorizing_device: DeviceId,
) -> Result<String, ControlPlaneError> {
    grants::encrypt_workspace_key_for_request(
        workspace_key,
        request,
        Some(grants::DeviceGrantAuthorizer {
            device_id: authorizing_device,
            device_authorization_proof_verifier: grants::device_authorization_proof_verifier(
                authorizing_identity,
            )
            .map_err(|error| ControlPlaneError::Storage(error.to_string()))?,
        }),
    )
    .map_err(|error| ControlPlaneError::Storage(error.to_string()))
}

fn hosted_reader_device_request_input(
    workspace_id: WorkspaceId,
    reader_device: &DeviceId,
    reader_identity: &DeviceIdentity,
) -> Result<DeviceRequestInput, ControlPlaneError> {
    Ok(DeviceRequestInput::new(DeviceRequestInputDraft {
        workspace_id,
        device_id: reader_device.clone(),
        device_name: "hosted-reader-process".to_string(),
        device_public_key: reader_identity.public_key.as_str().to_string(),
        device_fingerprint: reader_identity.fingerprint.as_str().to_string(),
        device_authorization_proof_verifier: grants::device_authorization_proof_verifier(
            reader_identity,
        )
        .map_err(|error| ControlPlaneError::Storage(error.to_string()))?,
        matching_code: "phase5-smoke".to_string(),
    }))
}

pub fn run_hosted_cloud_spike_from_env() -> Result<CloudSpikeReport, ControlPlaneError> {
    let convex_url = hosted_env_value("CONVEX_URL").ok_or_else(|| {
        ControlPlaneError::Storage("CONVEX_URL is required for the hosted cloud spike".to_string())
    })?;
    let control_plane_token = hosted_env_value("BOWLINE_CONTROL_PLANE_TOKEN").ok_or_else(|| {
        ControlPlaneError::Storage(
            "BOWLINE_CONTROL_PLANE_TOKEN is required for the hosted cloud spike".to_string(),
        )
    })?;
    let workos_access_token = hosted_env_value("BOWLINE_WORKOS_ACCESS_TOKEN").ok_or_else(|| {
        ControlPlaneError::Storage(
            "BOWLINE_WORKOS_ACCESS_TOKEN is required for the hosted cloud spike".to_string(),
        )
    })?;
    let run_id = unique_hex_id();
    let workspace_id = format!("workspace-cloud-spike-{run_id}");
    let workspace = WorkspaceId::new(workspace_id.clone());
    let writer_device_id = "device-hosted-writer";
    let reader_device_id = "device-hosted-reader";
    let writer_device = DeviceId::new(writer_device_id);
    let reader_device = DeviceId::new(reader_device_id);
    let writer_identity = DeviceIdentity::generate();
    let reader_identity = DeviceIdentity::generate();
    let control_plane = HostedControlPlaneClient::try_new_with_token(
        convex_url.clone(),
        control_plane_token.clone(),
    )?
    .with_workos_access_token(workos_access_token.clone())
    .with_device_id(writer_device.as_str())
    .with_device_proof_signer(hosted_device_proof_signer(
        writer_identity.clone(),
        writer_device.clone(),
    ))
    .with_device_proof_verifier_resolver(hosted_device_proof_verifier(
        writer_identity.clone(),
        writer_device.clone(),
    ));
    let initial_ref = control_plane.create_workspace_ref(&workspace)?;

    control_plane.create_first_authorized_device(FirstAuthorizedDeviceInput {
        workspace_id: workspace.clone(),
        device_id: writer_device.clone(),
        device_name: "hosted-writer-process".to_string(),
        platform: "macos".to_string(),
        device_fingerprint: writer_identity.fingerprint.as_str().to_string(),
        device_authorization_proof_verifier: grants::device_authorization_proof_verifier(
            &writer_identity,
        )
        .map_err(|error| ControlPlaneError::Storage(error.to_string()))?,
    })?;

    let request_input =
        hosted_reader_device_request_input(workspace.clone(), &reader_device, &reader_identity)?;
    let request = control_plane.create_device_request(request_input)?;
    let workspace_key = WorkspaceKeyMaterial::generate(workspace.clone(), 1)
        .map_err(|error| ControlPlaneError::Storage(error.to_string()))?;
    let grant_acceptance_proof =
        grants::grant_acceptance_proof(&workspace_key, &request.request_id, &reader_device);
    let grant_acceptance_proof_verifier =
        grants::grant_acceptance_proof_verifier(&grant_acceptance_proof);
    let encrypted_grant_ciphertext = encrypted_workspace_key_for_device_request(
        &workspace_key,
        &request,
        &writer_identity,
        writer_device.clone(),
    )?;
    control_plane.approve_device_request(DeviceApprovalInput {
        request_id: request.request_id.clone(),
        approved_by_device_id: writer_device.clone(),
        approved_by_device_proof: grants::device_authorization_proof(
            &writer_identity,
            &workspace,
            &writer_device,
            "approve-device-request",
            &grants::device_request_proof_subject(&request.request_id),
        )
        .map_err(|error| ControlPlaneError::Storage(error.to_string()))?,
        encrypted_grant_ciphertext,
        grant_acceptance_proof_verifier,
        key_epoch: workspace_key.key_epoch,
        expires_in_ticks: 600,
    })?;
    let reader_control_plane = HostedControlPlaneClient::try_new_with_token(
        convex_url.clone(),
        control_plane_token.clone(),
    )?
    .with_workos_access_token(workos_access_token.clone())
    .with_device_id(reader_device.as_str())
    .with_device_proof_signer(hosted_device_proof_signer(
        reader_identity.clone(),
        reader_device.clone(),
    ))
    .with_device_proof_verifier_resolver(hosted_known_device_proof_verifier(
        writer_identity,
        writer_device.clone(),
        reader_identity,
        reader_device.clone(),
    ));
    reader_control_plane.confirm_device_grant_accepted(GrantAcceptanceInput {
        request_id: request.request_id,
        device_id: reader_device.clone(),
        grant_acceptance_proof,
    })?;

    let content_key = [42_u8; 32];
    let file_bytes = b"hello from cold cloud spike file".to_vec();
    let content_id = workspace_content_id(content_key, &file_bytes);
    let packs = write_source_packs(
        workspace.clone(),
        &[PackRecordInput {
            content_id: content_id.clone(),
            bytes: file_bytes.clone(),
        }],
        16 * 1024 * 1024,
        StorageKey::deterministic(4),
        1,
    )
    .map_err(map_storage)?;
    let temp_root = TempDirGuard::new("hosted-cloud-spike")?;
    let cache = LocalContentCache::open(temp_root.path().join("cache")).map_err(map_storage)?;
    let store = SignedUrlByteStore::new(&control_plane, &workspace_id);

    let workspace_root = temp_root.path().join("workspace");
    let source_path = workspace_root.join("apps/web/src/index.ts");
    fs::create_dir_all(source_path.parent().expect("source parent")).map_err(map_io)?;
    fs::write(&source_path, &file_bytes).map_err(map_io)?;
    let candidate = coalesce_workspace_scan(
        &workspace_root,
        workspace.clone(),
        &initial_ref,
        writer_device.clone(),
        content_key,
        "2026-07-14T12:00:00Z",
    )
    .map_err(|error| ControlPlaneError::Storage(error.to_string()))?;
    let (advanced_ref, snapshot_root, bound_snapshot) = match upload_snapshot_candidate(
        &candidate,
        &control_plane,
        &store,
        StorageKey::deterministic(4),
        1,
    )
    .map_err(|error| ControlPlaneError::Storage(error.to_string()))?
    {
        UploadOutcome::Advanced {
            workspace_ref,
            snapshot_root,
            bound_snapshot: Some(bound_snapshot),
        } => (workspace_ref, snapshot_root, bound_snapshot),
        UploadOutcome::Advanced { .. } => {
            return Err(ControlPlaneError::Storage(
                "hosted spike upload did not return its bound page snapshot".to_string(),
            ));
        }
        UploadOutcome::Stale { .. } => {
            return Err(ControlPlaneError::Storage(
                "hosted spike upload unexpectedly lost its initial ref CAS".to_string(),
            ));
        }
    };
    assert_root_retry_is_idempotent(&control_plane, &workspace, &snapshot_root)?;
    let (uploaded_locator, uploaded_object_key) = uploaded_content_locator(&bound_snapshot)?;
    let reader_store = SignedUrlByteStore::new(&reader_control_plane, &workspace_id);
    let remote_head = reader_store
        .head_object(&uploaded_object_key)
        .map_err(map_storage)?;
    if remote_head.hash.is_empty() {
        return Err(ControlPlaneError::Storage(
            "hosted head metadata omitted the committed pack hash".to_string(),
        ));
    }

    let download = control_plane.create_download_intent(DownloadIntentRequest {
        workspace_id: workspace.clone(),
        object_key: uploaded_object_key.as_str().to_string(),
        range: Some(bowline_control_plane::ByteRange::new(
            uploaded_locator.offset.expect("pack offset exists"),
            uploaded_locator.length.expect("pack length exists"),
        )),
    })?;
    let hydrated = cache
        .hydrate_record_from_range(
            &reader_store,
            RangeHydrationRequest {
                object_key: &bowline_storage::ObjectKey::new(download.object_key)
                    .map_err(map_storage)?,
                workspace_id: &workspace,
                locator: &uploaded_locator,
                content_key,
                content_verification: bowline_storage::ContentVerification::AuthenticatedSegment,
                key: StorageKey::deterministic(4),
                key_epoch: 1,
            },
        )
        .map_err(map_storage)?;

    let stale = reader_control_plane.compare_and_swap_workspace_ref(
        &workspace,
        initial_ref.version,
        &SnapshotId::new("snapshot-loser"),
        &reader_device,
    );
    let events = control_plane.list_events(&workspace)?;
    temp_root.close()?;

    Ok(CloudSpikeReport {
        provider: "hosted".to_string(),
        workspace_id,
        starting_version: initial_ref.version,
        advanced_version: advanced_ref.version,
        pack_object_count: packs.len(),
        source_file_count: 1,
        hydrated_cold_file_bytes: hydrated,
        stale_ref_detected: matches!(stale, Err(CompareAndSwapError::StaleRef(_))),
        device_approval_harness_only: false,
        event_count: events.len(),
    })
}

pub fn run_fake_cloud_spike() -> Result<CloudSpikeReport, ControlPlaneError> {
    let workspace_id = "workspace-cloud-spike";
    let workspace = WorkspaceId::new(workspace_id);
    let writer_device_id = "device-writer";
    let reader_device_id = "device-reader";
    let writer_device = DeviceId::new(writer_device_id);
    let reader_device = DeviceId::new(reader_device_id);
    let control_plane = FakeControlPlaneClient::default();
    let initial_ref = control_plane.create_workspace(workspace_id);

    let request_input = DeviceRequestInput::new(DeviceRequestInputDraft {
        workspace_id: workspace.clone(),
        device_id: reader_device.clone(),
        device_name: "reader-process".to_string(),
        device_public_key: "age1reader".to_string(),
        device_fingerprint: "fp_reader".to_string(),
        device_authorization_proof_verifier: "dapv_phase5_reader".to_string(),
        matching_code: "phase5-smoke".to_string(),
    });
    let request = control_plane.create_device_request(request_input)?;
    let approval = control_plane.approve_device_request_for_harness(DeviceApprovalInput {
        request_id: request.request_id,
        approved_by_device_id: writer_device.clone(),
        approved_by_device_proof: String::new(),
        encrypted_grant_ciphertext: "bowline-harness-grant".to_string(),
        grant_acceptance_proof_verifier: String::new(),
        key_epoch: 1,
        expires_in_ticks: 600,
    })?;

    let content_key = [42_u8; 32];
    let file_bytes = b"hello from cold cloud spike file".to_vec();
    let content_id = workspace_content_id(content_key, &file_bytes);
    let packs = write_source_packs(
        workspace.clone(),
        &[PackRecordInput {
            content_id: content_id.clone(),
            bytes: file_bytes.clone(),
        }],
        16 * 1024 * 1024,
        StorageKey::deterministic(4),
        1,
    )
    .map_err(map_storage)?;
    let temp_root = TempDirGuard::new("cloud-spike")?;
    let store = LocalByteStore::open_deterministic(temp_root.path().join("objects"), 100)
        .map_err(map_storage)?;
    let cache = LocalContentCache::open(temp_root.path().join("cache")).map_err(map_storage)?;

    let workspace_root = temp_root.path().join("workspace");
    let source_path = workspace_root.join("apps/web/src/index.ts");
    fs::create_dir_all(source_path.parent().expect("source parent")).map_err(map_io)?;
    fs::write(&source_path, &file_bytes).map_err(map_io)?;
    let candidate = coalesce_workspace_scan(
        &workspace_root,
        workspace.clone(),
        &initial_ref,
        writer_device.clone(),
        content_key,
        "2026-07-14T12:00:00Z",
    )
    .map_err(|error| ControlPlaneError::Storage(error.to_string()))?;
    let (advanced_ref, snapshot_root, bound_snapshot) = match upload_snapshot_candidate(
        &candidate,
        &control_plane,
        &store,
        StorageKey::deterministic(4),
        1,
    )
    .map_err(|error| ControlPlaneError::Storage(error.to_string()))?
    {
        UploadOutcome::Advanced {
            workspace_ref,
            snapshot_root,
            bound_snapshot: Some(bound_snapshot),
        } => (workspace_ref, snapshot_root, bound_snapshot),
        UploadOutcome::Advanced { .. } => {
            return Err(ControlPlaneError::Storage(
                "fake spike upload did not return its bound page snapshot".to_string(),
            ));
        }
        UploadOutcome::Stale { .. } => {
            return Err(ControlPlaneError::Storage(
                "fake spike upload unexpectedly lost its initial ref CAS".to_string(),
            ));
        }
    };
    assert_root_retry_is_idempotent(&control_plane, &workspace, &snapshot_root)?;
    let (uploaded_locator, uploaded_object_key) = uploaded_content_locator(&bound_snapshot)?;

    let download = control_plane.create_download_intent(DownloadIntentRequest {
        workspace_id: workspace.clone(),
        object_key: uploaded_object_key.as_str().to_string(),
        range: Some(bowline_control_plane::ByteRange::new(
            uploaded_locator.offset.expect("pack offset exists"),
            uploaded_locator.length.expect("pack length exists"),
        )),
    })?;
    let hydrated = cache
        .hydrate_record_from_range(
            &store,
            RangeHydrationRequest {
                object_key: &bowline_storage::ObjectKey::new(download.object_key)
                    .map_err(map_storage)?,
                workspace_id: &workspace,
                locator: &uploaded_locator,
                content_key,
                content_verification: bowline_storage::ContentVerification::AuthenticatedSegment,
                key: StorageKey::deterministic(4),
                key_epoch: 1,
            },
        )
        .map_err(map_storage)?;

    let stale = control_plane.compare_and_swap_workspace_ref(
        &workspace,
        initial_ref.version,
        &SnapshotId::new("snapshot-loser"),
        &reader_device,
    );
    let events = control_plane.list_events(&workspace)?;
    temp_root.close()?;

    Ok(CloudSpikeReport {
        provider: "fake".to_string(),
        workspace_id: workspace_id.to_string(),
        starting_version: initial_ref.version,
        advanced_version: advanced_ref.version,
        pack_object_count: packs.len(),
        source_file_count: 1,
        hydrated_cold_file_bytes: hydrated,
        stale_ref_detected: matches!(stale, Err(CompareAndSwapError::StaleRef(_))),
        device_approval_harness_only: approval.harness_only,
        event_count: events.len(),
    })
}

fn assert_root_retry_is_idempotent(
    control_plane: &impl ControlPlaneClient,
    workspace_id: &WorkspaceId,
    root: &SnapshotRootRecord,
) -> Result<(), ControlPlaneError> {
    let event_count_before_retry = control_plane.list_events(workspace_id)?.len();
    control_plane.commit_snapshot_root(SnapshotRootCommit {
        workspace_id: root.workspace_id.clone(),
        snapshot_id: root.snapshot_id.clone(),
        manifest_id: root.manifest_id.clone(),
        manifest_object: root.manifest_object.clone(),
        namespace_root_id: root.namespace_root_id.clone(),
        extra_root_logical_ids: root.extra_root_logical_ids.clone(),
        committed_by_device_id: root.committed_by_device_id.clone(),
    })?;
    let event_count_after_retry = control_plane.list_events(workspace_id)?.len();
    if event_count_after_retry != event_count_before_retry {
        return Err(ControlPlaneError::Storage(
            "snapshot-root commit retry created duplicate control-plane events".to_string(),
        ));
    }
    Ok(())
}

fn uploaded_content_locator(
    snapshot: &SnapshotContent,
) -> Result<(ContentLocator, ObjectKey), ControlPlaneError> {
    let entry = snapshot
        .entry_for_path("apps/web/src/index.ts")
        .map_err(|error| ControlPlaneError::Storage(error.to_string()))?
        .ok_or_else(|| {
            ControlPlaneError::Storage(
                "bound spike snapshot omitted the uploaded source entry".to_string(),
            )
        })?;
    let segment = entry
        .content_layout
        .as_ref()
        .and_then(|layout| layout.segments().first())
        .ok_or_else(|| {
            ControlPlaneError::Storage(
                "bound spike snapshot omitted the uploaded content locator".to_string(),
            )
        })?;
    let locator = ContentLocator {
        content_id: ContentId::new(segment.segment_id.as_str()),
        storage: ContentStorage::Packed,
        raw_size: segment.plaintext_length,
        pack_id: Some(segment.pack_id.clone()),
        offset: Some(segment.offset),
        length: Some(segment.length),
    };
    let object_key = ObjectKey::from_pack_id(&segment.pack_id).map_err(map_storage)?;
    Ok((locator, object_key))
}

struct TempDirGuard {
    path: PathBuf,
}

impl TempDirGuard {
    fn new(label: &str) -> Result<Self, ControlPlaneError> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(map_storage)?
            .as_nanos();
        let path = env::temp_dir().join(format!("bowline-{label}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&path).map_err(map_io)?;
        Ok(Self { path })
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }

    fn close(mut self) -> Result<(), ControlPlaneError> {
        let path = std::mem::take(&mut self.path);
        fs::remove_dir_all(path).map_err(map_io)
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        if !self.path.as_os_str().is_empty() {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

fn map_storage(error: impl ToString) -> ControlPlaneError {
    ControlPlaneError::Storage(error.to_string())
}

fn map_io(error: io::Error) -> ControlPlaneError {
    ControlPlaneError::Storage(error.to_string())
}

fn hosted_env_value(key: &str) -> Option<String> {
    env::var(key)
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| dotenv_file_value(".env.local", key))
}

fn dotenv_file_value(path: &str, key: &str) -> Option<String> {
    let contents = fs::read_to_string(path).ok()?;
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((name, value)) = trimmed.split_once('=') else {
            continue;
        };
        if name.trim() != key {
            continue;
        }
        return Some(unquote_env_value(value.trim()).to_string());
    }
    None
}

fn unquote_env_value(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .or_else(|| {
            value
                .strip_prefix('\'')
                .and_then(|value| value.strip_suffix('\''))
        })
        .unwrap_or(value)
}

fn unique_hex_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let mut hasher = blake3::Hasher::new();
    hasher.update(&nanos.to_le_bytes());
    hasher.update(&std::process::id().to_le_bytes());
    let hex = hasher.finalize().to_hex().to_string();
    hex[..32].to_string()
}

#[test]
fn fake_cloud_spike_proves_pack_range_hydration_and_stale_ref() {
    let report = run_fake_cloud_spike().expect("fake spike runs");

    assert_eq!(report.provider, "fake");
    assert_eq!(report.starting_version, 0);
    assert_eq!(report.advanced_version, 1);
    assert_eq!(report.pack_object_count, 1);
    assert_eq!(report.source_file_count, 1);
    assert_eq!(
        report.hydrated_cold_file_bytes,
        b"hello from cold cloud spike file"
    );
    assert!(report.stale_ref_detected);
    assert!(report.device_approval_harness_only);
    assert!(report.event_count >= 5);
}

#[test]
fn hosted_cloud_spike_reports_missing_env_as_skip() {
    let skip =
        skip_hosted_cloud_spike_with(|_key| None).expect("hosted spike skips without Convex URL");

    assert!(skip.skipped);
    assert_eq!(skip.provider, "hosted");
    assert_eq!(
        skip.missing_env,
        vec![
            "CONVEX_URL".to_string(),
            "BOWLINE_CONTROL_PLANE_TOKEN".to_string(),
            "BOWLINE_WORKOS_ACCESS_TOKEN".to_string()
        ]
    );
    assert!(
        skip_hosted_cloud_spike_with(|key| {
            match key {
                "CONVEX_URL" => Some("https://example.convex.cloud".to_string()),
                "BOWLINE_CONTROL_PLANE_TOKEN" => Some("test-token".to_string()),
                "BOWLINE_WORKOS_ACCESS_TOKEN" => Some("workos-token".to_string()),
                _ => None,
            }
        })
        .is_none()
    );
}

#[test]
#[ignore]
fn hosted_cloud_spike_end_to_end() {
    if skip_hosted_cloud_spike_from_env().is_some() {
        return;
    }
    let report = run_hosted_cloud_spike_from_env().expect("hosted spike runs");

    assert_eq!(report.provider, "hosted");
    assert_eq!(report.advanced_version, report.starting_version + 1);
    assert!(report.stale_ref_detected);
    assert!(!report.device_approval_harness_only);
}
