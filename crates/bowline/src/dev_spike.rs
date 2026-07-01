use std::{
    env, fs, io,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use bowline_core::{
    ids::{ContentId, DeviceApprovalRequestId, DeviceId, ManifestId, SnapshotId, WorkspaceId},
    policy::{MaterializationMode, PathClassification},
    workspace_graph::{
        ContentLocator, HydrationState, NamespaceEntry, NamespaceEntryKind, RefKind, SnapshotKind,
        SnapshotManifest, WorkspaceRef as SnapshotWorkspaceRef, workspace_content_id,
    },
};
use bowline_storage::{
    ByteStore, LocalByteStore, LocalContentCache, ObjectKind as StorageObjectKind, PackRecordInput,
    RangeHydrationRequest, StorageKey, seal_snapshot_manifest, write_source_packs,
};

use bowline_control_plane::{
    CompareAndSwapError, ControlPlaneClient, ControlPlaneError, ControlPlaneTimestamp,
    DeviceApprovalInput, DeviceRequestInput, DeviceRequestInputDraft, DownloadIntentRequest,
    FakeControlPlaneClient, FirstAuthorizedDeviceInput, GrantAcceptanceInput,
    HostedControlPlaneClient, ObjectKind, ObjectManifestCommit, ObjectPointer, SignedUrlByteStore,
    UploadIntentRequest,
};
use bowline_local::{
    device_keys::{DeviceIdentity, WorkspaceKeyMaterial},
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
        Ok(grants::device_authorization_proof(
            &identity,
            &WorkspaceId::new(workspace_id.to_string()),
            &device_id,
            action,
            subject,
        ))
    }
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
    ));
    let initial_ref = control_plane.create_workspace_ref(&workspace_id)?;

    control_plane.create_first_authorized_device(FirstAuthorizedDeviceInput {
        workspace_id: workspace_id.clone(),
        device_id: writer_device.as_str().to_string(),
        device_name: "hosted-writer-process".to_string(),
        platform: "macos".to_string(),
        device_fingerprint: writer_identity.fingerprint.as_str().to_string(),
        device_authorization_proof_verifier: grants::device_authorization_proof_verifier(
            &writer_identity,
        ),
    })?;

    let mut request_input = DeviceRequestInput::new(DeviceRequestInputDraft {
        workspace_id: workspace_id.clone(),
        device_id: reader_device.as_str().to_string(),
        device_name: "hosted-reader-process".to_string(),
        device_public_key: reader_identity.public_key.as_str().to_string(),
        device_fingerprint: reader_identity.fingerprint.as_str().to_string(),
        matching_code: "phase5-smoke".to_string(),
    });
    request_input.device_authorization_proof_verifier =
        grants::device_authorization_proof_verifier(&reader_identity);
    let request = control_plane.create_device_request(request_input)?;
    let workspace = WorkspaceId::new(workspace_id.clone());
    let workspace_key = WorkspaceKeyMaterial::generate(workspace.clone(), 1)
        .map_err(|error| ControlPlaneError::Storage(error.to_string()))?;
    let grant_acceptance_proof = grants::grant_acceptance_proof(
        &workspace_key,
        &DeviceApprovalRequestId::new(request.request_id.clone()),
        &reader_device,
    );
    let grant_acceptance_proof_verifier =
        grants::grant_acceptance_proof_verifier(&grant_acceptance_proof);
    let encrypted_grant_ciphertext =
        grants::encrypt_workspace_key_for_request(&workspace_key, &request)
            .map_err(|error| ControlPlaneError::Storage(error.to_string()))?;
    control_plane.approve_device_request(DeviceApprovalInput {
        request_id: request.request_id.clone(),
        approved_by_device_id: writer_device.as_str().to_string(),
        approved_by_device_proof: grants::device_authorization_proof(
            &writer_identity,
            &workspace,
            &writer_device,
            "approve-device-request",
            &request.request_id,
        ),
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
        reader_identity,
        reader_device.clone(),
    ));
    reader_control_plane.confirm_device_grant_accepted(GrantAcceptanceInput {
        request_id: request.request_id,
        device_id: reader_device.as_str().to_string(),
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
    let pack = packs.first().expect("one pack for one source file");

    let temp_root = TempDirGuard::new("hosted-cloud-spike")?;
    let cache = LocalContentCache::open(temp_root.path().join("cache")).map_err(map_storage)?;
    let store = SignedUrlByteStore::new(&control_plane, &workspace_id);

    let pack_metadata = store
        .put_object_with_content_id(
            pack.object_key.clone(),
            StorageObjectKind::SourcePack,
            content_id.as_str(),
            &pack.bytes,
            None,
        )
        .map_err(map_storage)?;
    let retry_pack_metadata = store
        .put_object_with_content_id(
            pack.object_key.clone(),
            StorageObjectKind::SourcePack,
            content_id.as_str(),
            &pack.bytes,
            None,
        )
        .map_err(map_storage)?;
    if retry_pack_metadata.hash != pack_metadata.hash {
        return Err(ControlPlaneError::Storage(
            "hosted upload retry did not verify already-uploaded pack bytes".to_string(),
        ));
    }

    let manifest = snapshot_manifest(&workspace, &content_id, &pack.locators[0]);
    let sealed_manifest = seal_snapshot_manifest(
        ManifestId::new(format!("mf_{run_id}")),
        &manifest,
        StorageKey::deterministic(5),
        1,
    )
    .map_err(map_storage)?;
    let manifest_metadata = store
        .put_object_with_content_id(
            sealed_manifest.pointer.object_key.clone(),
            StorageObjectKind::SnapshotManifest,
            manifest.snapshot_id.as_str(),
            &sealed_manifest.bytes,
            None,
        )
        .map_err(map_storage)?;

    let manifest_commit = ObjectManifestCommit {
        workspace_id: workspace_id.clone(),
        snapshot_id: manifest.snapshot_id.as_str().to_string(),
        manifest_id: format!("manifest-cloud-spike-{run_id}"),
        manifest_object: ObjectPointer {
            object_key: manifest_metadata.key.as_str().to_string(),
            content_id: manifest.snapshot_id.as_str().to_string(),
            byte_len: manifest_metadata.byte_len,
            hash: manifest_metadata.hash.clone(),
            key_epoch: 1,
            kind: ObjectKind::SnapshotManifest,
            created_at: ControlPlaneTimestamp { tick: 200 },
        },
        pack_objects: vec![ObjectPointer {
            object_key: pack_metadata.key.as_str().to_string(),
            content_id: content_id.as_str().to_string(),
            byte_len: pack_metadata.byte_len,
            hash: pack_metadata.hash.clone(),
            key_epoch: 1,
            kind: ObjectKind::SourcePack,
            created_at: ControlPlaneTimestamp { tick: 201 },
        }],
        committed_by_device_id: writer_device.as_str().to_string(),
    };
    control_plane.commit_object_manifest(manifest_commit.clone())?;
    assert_manifest_retry_is_idempotent(&control_plane, &workspace_id, manifest_commit)?;
    let reader_store = SignedUrlByteStore::new(&reader_control_plane, &workspace_id);
    let remote_head = reader_store
        .head_object(&pack.object_key)
        .map_err(map_storage)?;
    if remote_head.hash != pack_metadata.hash {
        return Err(ControlPlaneError::Storage(
            "hosted head metadata did not match committed pack metadata".to_string(),
        ));
    }

    let advanced_ref = control_plane.compare_and_swap_workspace_ref(
        &workspace_id,
        initial_ref.version,
        "snapshot-cloud-spike",
        writer_device.as_str(),
    )?;

    let download = control_plane.create_download_intent(DownloadIntentRequest {
        workspace_id: workspace_id.clone(),
        object_key: pack.object_key.as_str().to_string(),
        range: Some(bowline_control_plane::ByteRange::new(
            pack.locators[0].offset.expect("pack offset exists"),
            pack.locators[0].length.expect("pack length exists"),
        )),
    })?;
    let hydrated = cache
        .hydrate_record_from_range(
            &reader_store,
            RangeHydrationRequest {
                object_key: &bowline_storage::ObjectKey::new(download.object_key)
                    .map_err(map_storage)?,
                workspace_id: &workspace,
                locator: &pack.locators[0],
                content_key,
                key: StorageKey::deterministic(4),
                key_epoch: 1,
            },
        )
        .map_err(map_storage)?;

    let stale = control_plane.compare_and_swap_workspace_ref(
        &workspace_id,
        initial_ref.version,
        "snapshot-loser",
        reader_device.as_str(),
    );
    let events = control_plane.list_events(&workspace_id)?;
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
    let writer_device_id = "device-writer";
    let reader_device_id = "device-reader";
    let control_plane = FakeControlPlaneClient::default();
    let initial_ref = control_plane.create_workspace(workspace_id);

    let mut request_input = DeviceRequestInput::new(DeviceRequestInputDraft {
        workspace_id: workspace_id.to_string(),
        device_id: reader_device_id.to_string(),
        device_name: "reader-process".to_string(),
        device_public_key: "age1reader".to_string(),
        device_fingerprint: "fp_reader".to_string(),
        matching_code: "phase5-smoke".to_string(),
    });
    request_input.device_authorization_proof_verifier = "dapv_phase5_reader".to_string();
    let request = control_plane.create_device_request(request_input)?;
    let approval = control_plane.approve_device_request_for_harness(DeviceApprovalInput {
        request_id: request.request_id,
        approved_by_device_id: writer_device_id.to_string(),
        approved_by_device_proof: String::new(),
        encrypted_grant_ciphertext: "bowline-harness-grant".to_string(),
        grant_acceptance_proof_verifier: String::new(),
        key_epoch: 1,
        expires_in_ticks: 600,
    })?;

    let content_key = [42_u8; 32];
    let file_bytes = b"hello from cold cloud spike file".to_vec();
    let content_id = workspace_content_id(content_key, &file_bytes);
    let workspace = WorkspaceId::new(workspace_id);
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
    let pack = packs.first().expect("one pack for one source file");

    let temp_root = TempDirGuard::new("cloud-spike")?;
    let store = LocalByteStore::open_deterministic(temp_root.path().join("objects"), 100)
        .map_err(map_storage)?;
    let cache = LocalContentCache::open(temp_root.path().join("cache")).map_err(map_storage)?;

    control_plane.create_upload_intent(
        UploadIntentRequest::new(
            workspace_id,
            ObjectKind::SourcePack,
            pack.bytes.len() as u64,
        )
        .with_object_key(pack.object_key.as_str())
        .with_content_id(content_id.as_str()),
    )?;
    let pack_metadata = store
        .put_object(
            pack.object_key.clone(),
            StorageObjectKind::SourcePack,
            &pack.bytes,
            None,
        )
        .map_err(map_storage)?;

    let manifest = snapshot_manifest(&workspace, &content_id, &pack.locators[0]);
    let sealed_manifest = seal_snapshot_manifest(
        ManifestId::new("mf_0011223344556677"),
        &manifest,
        StorageKey::deterministic(5),
        1,
    )
    .map_err(map_storage)?;
    control_plane.create_upload_intent(
        UploadIntentRequest::new(
            workspace_id,
            ObjectKind::SnapshotManifest,
            sealed_manifest.bytes.len() as u64,
        )
        .with_content_id(manifest.snapshot_id.as_str())
        .with_object_key(sealed_manifest.pointer.object_key.as_str()),
    )?;
    let manifest_metadata = store
        .put_object(
            sealed_manifest.pointer.object_key.clone(),
            StorageObjectKind::SnapshotManifest,
            &sealed_manifest.bytes,
            None,
        )
        .map_err(map_storage)?;

    let manifest_commit = ObjectManifestCommit {
        workspace_id: workspace_id.to_string(),
        snapshot_id: manifest.snapshot_id.as_str().to_string(),
        manifest_id: "manifest-cloud-spike".to_string(),
        manifest_object: ObjectPointer {
            object_key: manifest_metadata.key.as_str().to_string(),
            content_id: manifest.snapshot_id.as_str().to_string(),
            byte_len: manifest_metadata.byte_len,
            hash: manifest_metadata.hash.clone(),
            key_epoch: 1,
            kind: ObjectKind::SnapshotManifest,
            created_at: ControlPlaneTimestamp { tick: 200 },
        },
        pack_objects: vec![ObjectPointer {
            object_key: pack_metadata.key.as_str().to_string(),
            content_id: content_id.as_str().to_string(),
            byte_len: pack_metadata.byte_len,
            hash: pack_metadata.hash.clone(),
            key_epoch: 1,
            kind: ObjectKind::SourcePack,
            created_at: ControlPlaneTimestamp { tick: 201 },
        }],
        committed_by_device_id: writer_device_id.to_string(),
    };
    control_plane.commit_object_manifest(manifest_commit.clone())?;
    assert_manifest_retry_is_idempotent(&control_plane, workspace_id, manifest_commit)?;

    let advanced_ref = control_plane.compare_and_swap_workspace_ref(
        workspace_id,
        initial_ref.version,
        "snapshot-cloud-spike",
        writer_device_id,
    )?;

    let download = control_plane.create_download_intent(DownloadIntentRequest {
        workspace_id: workspace_id.to_string(),
        object_key: pack.object_key.as_str().to_string(),
        range: Some(bowline_control_plane::ByteRange::new(
            pack.locators[0].offset.expect("pack offset exists"),
            pack.locators[0].length.expect("pack length exists"),
        )),
    })?;
    let hydrated = cache
        .hydrate_record_from_range(
            &store,
            RangeHydrationRequest {
                object_key: &bowline_storage::ObjectKey::new(download.object_key)
                    .map_err(map_storage)?,
                workspace_id: &workspace,
                locator: &pack.locators[0],
                content_key,
                key: StorageKey::deterministic(4),
                key_epoch: 1,
            },
        )
        .map_err(map_storage)?;

    let stale = control_plane.compare_and_swap_workspace_ref(
        workspace_id,
        initial_ref.version,
        "snapshot-loser",
        reader_device_id,
    );
    let events = control_plane.list_events(workspace_id)?;
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

fn assert_manifest_retry_is_idempotent(
    control_plane: &impl ControlPlaneClient,
    workspace_id: &str,
    manifest_commit: ObjectManifestCommit,
) -> Result<(), ControlPlaneError> {
    let event_count_before_retry = control_plane.list_events(workspace_id)?.len();
    control_plane.commit_object_manifest(manifest_commit)?;
    let event_count_after_retry = control_plane.list_events(workspace_id)?.len();
    if event_count_after_retry != event_count_before_retry {
        return Err(ControlPlaneError::Storage(
            "manifest commit retry created duplicate control-plane events".to_string(),
        ));
    }
    Ok(())
}

fn snapshot_manifest(
    workspace: &WorkspaceId,
    content_id: &ContentId,
    locator: &ContentLocator,
) -> SnapshotManifest {
    SnapshotManifest {
        schema_version: 1,
        snapshot_id: SnapshotId::new("snapshot-cloud-spike"),
        workspace_id: workspace.clone(),
        project_id: None,
        kind: SnapshotKind::WorkspaceHead,
        base_snapshot_id: Some(SnapshotId::new("empty")),
        entries: vec![NamespaceEntry {
            path: "apps/web/src/index.ts".to_string(),
            kind: NamespaceEntryKind::File,
            classification: PathClassification::WorkspaceSync,
            mode: MaterializationMode::WorkspaceSync,
            access: Vec::new(),
            content_id: Some(content_id.clone()),
            locator: Some(locator.clone()),
            symlink_target: None,
            byte_len: Some(locator.raw_size),
            hydration_state: HydrationState::Cold,
        }],
        refs: vec![SnapshotWorkspaceRef {
            name: "workspace".to_string(),
            target_snapshot_id: SnapshotId::new("snapshot-cloud-spike"),
            kind: RefKind::Workspace,
        }],
    }
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

#[cfg(test)]
mod tests {
    use super::{run_fake_cloud_spike, skip_hosted_cloud_spike_with};

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
        let skip = skip_hosted_cloud_spike_with(|_key| None)
            .expect("hosted spike skips without Convex URL");

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
}
