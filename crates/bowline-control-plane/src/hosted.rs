use std::{
    collections::BTreeMap,
    fmt,
    future::Future,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as BASE64_URL};
use bowline_storage::{
    ObjectKey as StorageObjectKey, ObjectKind as StorageObjectKind, ObjectMetadata, RetentionState,
};
use convex::{ConvexClient, FunctionResult, Value};
use futures::StreamExt;
use sha2::{Digest, Sha256};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::{
    AuthorizedDeviceRecord, BootstrapSession, BootstrapSessionInput, CompactEvent,
    CompactEventKind, CompareAndSwapError, ConflictMetadataPublish, ConflictMetadataRecord,
    ConflictResolutionMark, ControlPlaneClient, ControlPlaneError, ControlPlaneResult,
    ControlPlaneTimestamp, DeleteIntent, DeleteIntentRequest, DeviceApproval, DeviceApprovalInput,
    DeviceApprovalRequestList, DeviceDenial, DeviceDenialInput, DeviceRequest, DeviceRequestInput,
    DeviceRequestState, DeviceRevocationInput, DownloadIntent, DownloadIntentRequest,
    FirstAuthorizedDeviceInput, GrantAcceptanceInput, Lease, LeaseCreate, LeaseExecutionState,
    LeaseOutputState, LeaseUpdate, LeaseWriteTargetMode, ObjectKind, ObjectManifestCommit,
    ObjectManifestRecord, ObjectMetadataCommit, ObjectPointer, ObjectRetentionStateUpdate,
    RecoveryDeviceAuthorizationInput, RecoveryEnvelopeInput, RecoveryEnvelopeRecord,
    RecoveryEnvelopeState, RevokedDeviceRecord, SignedUrlIntent, StaleWorkViewOverlayHead,
    StaleWorkspaceRef, StatusEventWatermarks, StatusIndexSnapshot, StatusItemSnapshot,
    StatusLimitSnapshot, StatusSyncQueueSnapshot, StatusWorkspaceSummarySnapshot, UploadIntent,
    UploadIntentRequest, UploadVerificationIntentRequest, WorkViewCreate, WorkViewLifecycleState,
    WorkViewLifecycleUpdate, WorkViewOverlayCommit, WorkViewRecord, WorkViewUpdateError,
    WorkspaceRef, WorkspaceStatusSnapshot,
};

const HOSTED_CAPABILITY: &str = "hosted-convex-control-plane";
const DEFAULT_DEVICE_ID: &str = "bowline-hosted-client";
const ENV_CONTROL_PLANE_TOKEN: &str = "BOWLINE_CONTROL_PLANE_TOKEN";
const CONVEX_RPC_TIMEOUT: Duration = Duration::from_secs(20);
const ACCOUNT_SESSION_FALLBACK_TTL_SECONDS: i64 = 300;
const ACCOUNT_SESSION_EXPIRY_SAFETY_SECONDS: i64 = 60;
static NEXT_OBJECT_KEY_SEED: AtomicU64 = AtomicU64::new(1);
static HOSTED_FUNCTION_CALL_COUNTS: OnceLock<Mutex<BTreeMap<String, u64>>> = OnceLock::new();
type DeviceProofSigner =
    Arc<dyn Fn(&str, &str, &str, &str) -> ControlPlaneResult<String> + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostedFunctionCallCount {
    pub function_name: String,
    pub call_count: u64,
}

#[derive(Debug, Clone)]
struct CachedAccountSession {
    session_id: String,
    expires_at_unix: i64,
}

pub struct HostedControlPlaneClient {
    control_plane_token: String,
    deployment_url: String,
    device_id: String,
    device_proof_signer: Option<DeviceProofSigner>,
    runtime: tokio::runtime::Runtime,
    bootstrap_token: Option<String>,
    workos_access_token: Option<String>,
    account_session_id: Option<String>,
    account_session_cache: Mutex<BTreeMap<String, CachedAccountSession>>,
}

impl fmt::Debug for HostedControlPlaneClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HostedControlPlaneClient")
            .field("deployment_url", &self.deployment_url)
            .field("device_id", &self.device_id)
            .finish_non_exhaustive()
    }
}

impl HostedControlPlaneClient {
    pub fn new(deployment_url: impl Into<String>) -> Self {
        Self::try_new(deployment_url).expect("hosted Convex runtime starts")
    }

    pub fn try_new(deployment_url: impl Into<String>) -> ControlPlaneResult<Self> {
        let control_plane_token = std::env::var(ENV_CONTROL_PLANE_TOKEN).map_err(|_| {
            ControlPlaneError::Storage(format!("{ENV_CONTROL_PLANE_TOKEN} is required"))
        })?;
        Self::try_new_with_token(deployment_url, control_plane_token)
    }

    pub fn try_new_with_token(
        deployment_url: impl Into<String>,
        control_plane_token: impl Into<String>,
    ) -> ControlPlaneResult<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("bowline-convex")
            .build()
            .map_err(|error| ControlPlaneError::Storage(error.to_string()))?;

        Ok(Self {
            control_plane_token: control_plane_token.into(),
            deployment_url: deployment_url.into(),
            device_id: DEFAULT_DEVICE_ID.to_string(),
            device_proof_signer: None,
            runtime,
            bootstrap_token: None,
            workos_access_token: None,
            account_session_id: None,
            account_session_cache: Mutex::new(BTreeMap::new()),
        })
    }

    pub fn try_new_with_bootstrap_token(
        deployment_url: impl Into<String>,
        bootstrap_token: impl Into<String>,
    ) -> ControlPlaneResult<Self> {
        let mut client = Self::try_new_with_token(deployment_url, String::new())?;
        client.bootstrap_token = Some(bootstrap_token.into());
        Ok(client)
    }

    pub fn with_device_id(mut self, device_id: impl Into<String>) -> Self {
        self.device_id = device_id.into();
        self
    }

    pub fn with_device_proof_signer<F>(mut self, signer: F) -> Self
    where
        F: Fn(&str, &str, &str, &str) -> ControlPlaneResult<String> + Send + Sync + 'static,
    {
        self.device_proof_signer = Some(Arc::new(signer));
        self
    }

    pub fn with_workos_access_token(mut self, access_token: impl Into<String>) -> Self {
        self.workos_access_token = Some(access_token.into());
        if let Ok(mut cache) = self.account_session_cache.lock() {
            cache.clear();
        }
        self
    }

    pub fn with_account_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.account_session_id = Some(session_id.into());
        self
    }

    pub fn register_account_session_id(
        &self,
        access_token: impl Into<String>,
        workspace_id: Option<&str>,
    ) -> ControlPlaneResult<String> {
        self.register_account_session_id_for_token(access_token.into(), workspace_id)
    }

    pub fn deployment_url(&self) -> &str {
        &self.deployment_url
    }

    pub fn stream_workspace_ref_updates(
        &self,
        workspace_id: &str,
        sender: std::sync::mpsc::Sender<ControlPlaneResult<Option<WorkspaceRef>>>,
    ) -> ControlPlaneResult<()> {
        let deployment_url = self.deployment_url.clone();
        let mut request_args = args([("workspaceId", Value::from(workspace_id.to_string()))]);
        if self.account_session_auth_available() {
            request_args.insert(
                "accountSessionId".to_string(),
                Value::from(self.verified_account_session_id(Some(workspace_id))?),
            );
        } else {
            request_args = self.authenticated_args(request_args);
        }
        self.runtime.block_on(async move {
            let mut client = ConvexClient::new(&deployment_url)
                .await
                .map_err(map_convex_error)?;
            let mut subscription = client
                .subscribe("refs:getWorkspaceRef", request_args)
                .await
                .map_err(map_convex_error)?;
            while let Some(result) = subscription.next().await {
                record_hosted_function_call("refs:getWorkspaceRef");
                let parsed = unwrap_function_result(result).and_then(|value| {
                    if matches!(value, Value::Null) {
                        Ok(None)
                    } else {
                        parse_workspace_ref(&value).map(Some)
                    }
                });
                if sender.send(parsed).is_err() {
                    break;
                }
            }
            Ok(())
        })
    }

    fn query(&self, name: &str, args: ConvexArgs) -> ControlPlaneResult<Value> {
        record_hosted_function_call(name);
        let deployment_url = self.deployment_url.clone();
        let name = name.to_string();
        let args = self.authenticated_args(args);
        self.runtime.block_on(convex_rpc_timeout(async move {
            let mut client = ConvexClient::new(&deployment_url)
                .await
                .map_err(map_convex_error)?;
            let result = client.query(&name, args).await.map_err(map_convex_error)?;
            unwrap_function_result(result)
        }))
    }

    fn mutation(&self, name: &str, args: ConvexArgs) -> ControlPlaneResult<Value> {
        record_hosted_function_call(name);
        let deployment_url = self.deployment_url.clone();
        let name = name.to_string();
        let args = self.authenticated_args(args);
        self.runtime.block_on(convex_rpc_timeout(async move {
            let mut client = ConvexClient::new(&deployment_url)
                .await
                .map_err(map_convex_error)?;
            let result = client
                .mutation(&name, args)
                .await
                .map_err(map_convex_error)?;
            unwrap_function_result(result)
        }))
    }

    fn public_action(&self, name: &str, args: ConvexArgs) -> ControlPlaneResult<Value> {
        record_hosted_function_call(name);
        let deployment_url = self.deployment_url.clone();
        let name = name.to_string();
        self.runtime.block_on(convex_rpc_timeout(async move {
            let mut client = ConvexClient::new(&deployment_url)
                .await
                .map_err(map_convex_error)?;
            let result = client.action(&name, args).await.map_err(map_convex_error)?;
            unwrap_function_result(result)
        }))
    }

    fn public_query(&self, name: &str, args: ConvexArgs) -> ControlPlaneResult<Value> {
        record_hosted_function_call(name);
        let deployment_url = self.deployment_url.clone();
        let name = name.to_string();
        self.runtime.block_on(convex_rpc_timeout(async move {
            let mut client = ConvexClient::new(&deployment_url)
                .await
                .map_err(map_convex_error)?;
            let result = client.query(&name, args).await.map_err(map_convex_error)?;
            unwrap_function_result(result)
        }))
    }

    fn public_mutation(&self, name: &str, args: ConvexArgs) -> ControlPlaneResult<Value> {
        record_hosted_function_call(name);
        let deployment_url = self.deployment_url.clone();
        let name = name.to_string();
        self.runtime.block_on(convex_rpc_timeout(async move {
            let mut client = ConvexClient::new(&deployment_url)
                .await
                .map_err(map_convex_error)?;
            let result = client
                .mutation(&name, args)
                .await
                .map_err(map_convex_error)?;
            unwrap_function_result(result)
        }))
    }

    fn authenticated_args(&self, mut args: ConvexArgs) -> ConvexArgs {
        args.insert(
            "authToken".to_string(),
            Value::from(self.control_plane_token.clone()),
        );
        args
    }

    fn verified_account_session_id(
        &self,
        workspace_id: Option<&str>,
    ) -> ControlPlaneResult<String> {
        if let Some(session_id) = self.account_session_id.as_ref() {
            return Ok(session_id.clone());
        }
        let access_token = self
            .workos_access_token
            .as_ref()
            .ok_or(ControlPlaneError::Limited {
                capability: "workos-account",
                reason: "hosted account operations require bowline login",
            })?;
        self.register_account_session_id_for_token(access_token.clone(), workspace_id)
    }

    fn register_account_session_id_for_token(
        &self,
        access_token: String,
        workspace_id: Option<&str>,
    ) -> ControlPlaneResult<String> {
        let cache_key = account_session_cache_key(workspace_id);
        if let Some(session_id) = self.cached_account_session_id(&cache_key) {
            return Ok(session_id);
        }
        let mut account_args = args([("accessToken", Value::from(access_token.clone()))]);
        if let Some(workspace_id) = workspace_id {
            account_args.insert(
                "workspaceId".to_string(),
                Value::from(workspace_id.to_string()),
            );
        }
        let value = self.public_action("auth:registerAccountSession", account_args)?;
        let object = value_object(&value)?;
        let session_id = string_field(object, "sessionId")?;
        let expires_at_unix = optional_string_field(object, "expiresAt")?
            .and_then(|expires_at| parse_unix_timestamp(&expires_at).ok())
            .unwrap_or_else(|| {
                OffsetDateTime::now_utc().unix_timestamp() + ACCOUNT_SESSION_FALLBACK_TTL_SECONDS
            });
        if let Ok(mut cache) = self.account_session_cache.lock() {
            cache.insert(
                cache_key,
                CachedAccountSession {
                    session_id: session_id.clone(),
                    expires_at_unix,
                },
            );
        }
        Ok(session_id)
    }

    fn account_session_auth_available(&self) -> bool {
        self.control_plane_token.is_empty()
            && (self.account_session_id.is_some() || self.workos_access_token.is_some())
    }

    fn cached_account_session_id(&self, cache_key: &str) -> Option<String> {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        self.account_session_cache
            .lock()
            .ok()
            .and_then(|cache| cache.get(cache_key).cloned())
            .filter(|cached| cached.expires_at_unix - ACCOUNT_SESSION_EXPIRY_SAFETY_SECONDS > now)
            .map(|cached| cached.session_id)
    }

    fn generated_object_key(&self, kind: ObjectKind, workspace_id: &str) -> String {
        let counter = NEXT_OBJECT_KEY_SEED.fetch_add(1, Ordering::Relaxed);
        let timestamp_nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let seed = format!(
            "{}:{}:{}:{}:{}:{}:{}",
            self.deployment_url,
            self.device_id,
            workspace_id,
            kind.as_str(),
            std::process::id(),
            timestamp_nanos,
            counter
        );
        generated_object_key(kind, &seed)
    }

    fn device_proof(
        &self,
        workspace_id: &str,
        action: &str,
        subject: &str,
    ) -> ControlPlaneResult<String> {
        let signer = self
            .device_proof_signer
            .as_ref()
            .ok_or(ControlPlaneError::Limited {
                capability: "hosted-device-proof",
                reason: "hosted byte-plane and ref operations require a local device identity",
            })?;
        signer(workspace_id, &self.device_id, action, subject)
    }

    fn require_local_device(&self, device_id: &str) -> ControlPlaneResult<()> {
        if device_id == self.device_id {
            Ok(())
        } else {
            Err(ControlPlaneError::Limited {
                capability: "hosted-device-proof",
                reason: "hosted operations must be signed by this client's local device identity",
            })
        }
    }
}

impl ControlPlaneClient for HostedControlPlaneClient {
    fn create_workspace_ref(&self, workspace_id: &str) -> ControlPlaneResult<WorkspaceRef> {
        let mut request_args = args([
            ("snapshotId", Value::from("empty")),
            ("workspaceId", Value::from(workspace_id)),
        ]);
        if self.account_session_auth_available() {
            request_args.insert(
                "accountSessionId".to_string(),
                Value::from(self.verified_account_session_id(Some(workspace_id))?),
            );
        }
        let value = if self.account_session_auth_available() {
            self.public_mutation("refs:createWorkspaceRef", request_args)?
        } else {
            self.mutation("refs:createWorkspaceRef", request_args)?
        };
        parse_workspace_ref(&value)
    }

    fn get_workspace_ref(&self, workspace_id: &str) -> ControlPlaneResult<Option<WorkspaceRef>> {
        let mut request_args = args([("workspaceId", Value::from(workspace_id))]);
        let value = if self.account_session_auth_available() {
            request_args.insert(
                "accountSessionId".to_string(),
                Value::from(self.verified_account_session_id(Some(workspace_id))?),
            );
            self.public_query("refs:getWorkspaceRef", request_args)?
        } else {
            self.query("refs:getWorkspaceRef", request_args)?
        };
        if matches!(value, Value::Null) {
            Ok(None)
        } else {
            parse_workspace_ref(&value).map(Some)
        }
    }

    fn compare_and_swap_workspace_ref(
        &self,
        workspace_id: &str,
        expected_version: u64,
        new_snapshot_id: &str,
        writer_device_id: &str,
    ) -> Result<WorkspaceRef, CompareAndSwapError> {
        self.require_local_device(writer_device_id)
            .map_err(|error| CompareAndSwapError::Storage(error.to_string()))?;
        let proof_subject = workspace_ref_proof_subject(expected_version, new_snapshot_id);
        let writer_device_proof = self
            .device_proof(
                workspace_id,
                "compare-and-swap-workspace-ref",
                &proof_subject,
            )
            .map_err(|error| CompareAndSwapError::Storage(error.to_string()))?;
        let value = self
            .public_mutation(
                "refs:compareAndSwapWorkspaceRef",
                args([
                    ("expectedVersion", number_value(expected_version)),
                    ("nextSnapshotId", Value::from(new_snapshot_id)),
                    ("workspaceId", Value::from(workspace_id)),
                    ("writerDeviceId", Value::from(writer_device_id)),
                    ("writerDeviceProof", Value::from(writer_device_proof)),
                ]),
            )
            .map_err(|error| CompareAndSwapError::Storage(error.to_string()))?;

        let object = value_object(&value)
            .map_err(|error| CompareAndSwapError::Storage(error.to_string()))?;
        if bool_field(object, "ok").unwrap_or(false) {
            return parse_workspace_ref(required_field(object, "ref")?)
                .map_err(|error| CompareAndSwapError::Storage(error.to_string()));
        }

        match string_field(object, "error").as_deref() {
            Ok("workspace-missing") => Err(CompareAndSwapError::WorkspaceMissing {
                workspace_id: workspace_id.to_string(),
            }),
            Ok("stale-ref") => {
                let current = parse_workspace_ref(required_field(object, "currentRef")?)
                    .map_err(|error| CompareAndSwapError::Storage(error.to_string()))?;
                Err(CompareAndSwapError::StaleRef(StaleWorkspaceRef {
                    expected_version,
                    current,
                }))
            }
            Ok(_) | Err(_) => Err(CompareAndSwapError::Unsupported {
                capability: HOSTED_CAPABILITY,
                reason: "Convex CAS returned an unknown result shape",
            }),
        }
    }

    fn list_events(&self, workspace_id: &str) -> ControlPlaneResult<Vec<CompactEvent>> {
        let mut request_args = args([("workspaceId", Value::from(workspace_id))]);
        let value = if self.account_session_auth_available() {
            request_args.insert(
                "accountSessionId".to_string(),
                Value::from(self.verified_account_session_id(Some(workspace_id))?),
            );
            self.public_query("events:listCompactEvents", request_args)?
        } else {
            self.query("events:listCompactEvents", request_args)?
        };
        let Value::Array(events) = value else {
            return Err(shape_error(
                "events:listCompactEvents did not return an array",
            ));
        };

        events
            .iter()
            .map(parse_compact_event)
            .collect::<ControlPlaneResult<Vec<_>>>()
    }

    fn publish_conflict_metadata(
        &self,
        input: ConflictMetadataPublish,
    ) -> ControlPlaneResult<ConflictMetadataRecord> {
        self.require_local_device(&input.detected_by_device_id)?;
        let proof_subject = conflict_publish_proof_subject(&input);
        let detected_by_device_proof = self.device_proof(
            &input.workspace_id,
            "publish-conflict-metadata",
            &proof_subject,
        )?;
        let mut request_args = args([
            (
                "baseSnapshotId",
                Value::from(input.base_snapshot_id.clone()),
            ),
            ("conflictId", Value::from(input.conflict_id.clone())),
            ("conflictKind", Value::from(input.conflict_kind.clone())),
            ("containsSecrets", Value::Boolean(input.contains_secrets)),
            (
                "detectedByDeviceId",
                Value::from(input.detected_by_device_id.clone()),
            ),
            (
                "detectedByDeviceProof",
                Value::from(detected_by_device_proof),
            ),
            (
                "paths",
                Value::Array(input.paths.iter().cloned().map(Value::from).collect()),
            ),
            (
                "remoteSnapshotId",
                Value::from(input.remote_snapshot_id.clone()),
            ),
            ("workspaceId", Value::from(input.workspace_id.clone())),
        ]);
        if let Some(pointer) = input.bundle_object.as_ref() {
            request_args.insert("bundleObject".to_string(), object_pointer_value(pointer));
        }
        let value = self.public_mutation("conflicts:publishConflictMetadata", request_args)?;
        parse_conflict_metadata_record(&value)
    }

    fn list_workspace_conflicts(
        &self,
        workspace_id: &str,
        requested_by_device_id: &str,
    ) -> ControlPlaneResult<Vec<ConflictMetadataRecord>> {
        self.require_local_device(requested_by_device_id)?;
        let proof_subject = format!("workspaceId={workspace_id}");
        let requested_by_device_proof =
            self.device_proof(workspace_id, "list-workspace-conflicts", &proof_subject)?;
        let value = self.public_query(
            "conflicts:listWorkspaceConflicts",
            args([
                ("requestedByDeviceId", Value::from(requested_by_device_id)),
                (
                    "requestedByDeviceProof",
                    Value::from(requested_by_device_proof),
                ),
                ("workspaceId", Value::from(workspace_id)),
            ]),
        )?;
        let Value::Array(records) = value else {
            return Err(shape_error(
                "conflicts:listWorkspaceConflicts must return an array",
            ));
        };
        records
            .iter()
            .map(parse_conflict_metadata_record)
            .collect::<ControlPlaneResult<Vec<_>>>()
    }

    fn mark_conflict_resolved(
        &self,
        input: ConflictResolutionMark,
    ) -> ControlPlaneResult<ConflictMetadataRecord> {
        self.require_local_device(&input.resolved_by_device_id)?;
        let proof_subject = conflict_resolution_proof_subject(&input);
        let resolved_by_device_proof = self.device_proof(
            &input.workspace_id,
            "mark-conflict-resolved",
            &proof_subject,
        )?;
        let value = self.public_mutation(
            "conflicts:markConflictResolved",
            args([
                ("conflictId", Value::from(input.conflict_id.clone())),
                ("resolution", Value::from(input.resolution.as_str())),
                (
                    "resolvedByDeviceId",
                    Value::from(input.resolved_by_device_id.clone()),
                ),
                (
                    "resolvedByDeviceProof",
                    Value::from(resolved_by_device_proof),
                ),
                ("workspaceId", Value::from(input.workspace_id.clone())),
            ]),
        )?;
        parse_conflict_metadata_record(&value)
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
        let mut request_args = args([
            (
                "attentionItems",
                Value::Array(
                    snapshot
                        .attention_items
                        .iter()
                        .cloned()
                        .map(Value::from)
                        .collect(),
                ),
            ),
            (
                "eventWatermarks",
                status_event_watermarks_value(&snapshot.event_watermarks),
            ),
            ("generatedAt", Value::from(snapshot.generated_at.clone())),
            (
                "publishedByDeviceId",
                Value::from(snapshot.published_by_device_id.clone()),
            ),
            (
                "publishedByDeviceProof",
                Value::from(published_by_device_proof),
            ),
            ("snapshotId", Value::from(snapshot.snapshot_id.clone())),
            ("statusLevel", Value::from(snapshot.status_level.clone())),
            ("workspaceId", Value::from(snapshot.workspace_id.clone())),
        ]);
        if let Some(sync_queue) = snapshot.sync_queue.as_ref() {
            request_args.insert("syncQueue".to_string(), status_sync_queue_value(sync_queue));
        }
        if let Some(index) = snapshot.index.as_ref() {
            request_args.insert("index".to_string(), status_index_value(index));
        }
        if let Some(summary) = snapshot.workspace_summary.as_ref() {
            request_args.insert(
                "workspaceSummary".to_string(),
                status_workspace_summary_value(summary),
            );
        }
        if !snapshot.items.is_empty() {
            request_args.insert(
                "items".to_string(),
                Value::Array(snapshot.items.iter().map(status_item_value).collect()),
            );
        }
        if !snapshot.limits.is_empty() {
            request_args.insert(
                "limits".to_string(),
                Value::Array(snapshot.limits.iter().map(status_limit_value).collect()),
            );
        }
        self.public_mutation("status:publishWorkspaceStatus", request_args)?;
        Ok(())
    }

    fn create_upload_intent(
        &self,
        request: UploadIntentRequest,
    ) -> ControlPlaneResult<UploadIntent> {
        let object_key = request.object_key.clone().unwrap_or_else(|| {
            self.generated_object_key(request.object_kind, &request.workspace_id)
        });
        let proof_subject = upload_intent_proof_subject(
            &object_key,
            request.object_kind,
            request.byte_len,
            request.content_id.as_deref(),
        );
        let created_by_device_proof = self.device_proof(
            &request.workspace_id,
            "create-upload-intent",
            &proof_subject,
        )?;
        let mut request_args = args([
            ("byteLength", number_value(request.byte_len)),
            ("createdByDeviceId", Value::from(self.device_id.clone())),
            ("createdByDeviceProof", Value::from(created_by_device_proof)),
            ("kind", Value::from(request.object_kind.as_str())),
            ("objectKey", Value::from(object_key)),
            ("workspaceId", Value::from(request.workspace_id.clone())),
        ]);
        if let Some(content_id) = request.content_id {
            request_args.insert("contentId".to_string(), Value::from(content_id));
        }

        let value = self.public_action("objects:createUploadIntent", request_args)?;
        let object = value_object(&value)?;
        Ok(UploadIntent {
            workspace_id: string_field(object, "workspaceId")?,
            object_key: string_field(object, "objectKey")?,
            object_kind: parse_object_kind(&string_field(object, "kind")?)?,
            byte_len: u64_field(object, "byteLength")?,
            signed_url: SignedUrlIntent {
                url: string_field(object, "signedUrl")?,
                expires_at: current_timestamp(),
            },
        })
    }

    fn create_download_intent(
        &self,
        request: DownloadIntentRequest,
    ) -> ControlPlaneResult<DownloadIntent> {
        let proof_subject = download_intent_proof_subject(&request.object_key, request.range);
        let requested_by_device_proof = self.device_proof(
            &request.workspace_id,
            "create-download-intent",
            &proof_subject,
        )?;
        let mut request_args = args([
            ("objectKey", Value::from(request.object_key.clone())),
            ("requestedByDeviceId", Value::from(self.device_id.clone())),
            (
                "requestedByDeviceProof",
                Value::from(requested_by_device_proof),
            ),
            ("workspaceId", Value::from(request.workspace_id.clone())),
        ]);
        if let Some(range) = request.range {
            request_args.insert("offset".to_string(), number_value(range.offset));
            request_args.insert("length".to_string(), number_value(range.length));
        }

        let value = self.public_action("objects:createDownloadIntent", request_args)?;
        let object = value_object(&value)?;
        Ok(DownloadIntent {
            workspace_id: string_field(object, "workspaceId")?,
            object_key: string_field(object, "objectKey")?,
            range: request.range,
            signed_url: SignedUrlIntent {
                url: string_field(object, "signedUrl")?,
                expires_at: current_timestamp(),
            },
        })
    }

    fn create_upload_verification_intent(
        &self,
        request: UploadVerificationIntentRequest,
    ) -> ControlPlaneResult<DownloadIntent> {
        let proof_subject = upload_verification_proof_subject(
            &request.object_key,
            request.byte_len,
            request.content_id.as_deref(),
        );
        let requested_by_device_proof = self.device_proof(
            &request.workspace_id,
            "verify-upload-intent",
            &proof_subject,
        )?;
        let mut request_args = args([
            ("byteLength", number_value(request.byte_len)),
            ("objectKey", Value::from(request.object_key.clone())),
            ("requestedByDeviceId", Value::from(self.device_id.clone())),
            (
                "requestedByDeviceProof",
                Value::from(requested_by_device_proof),
            ),
            ("workspaceId", Value::from(request.workspace_id.clone())),
        ]);
        if let Some(content_id) = request.content_id {
            request_args.insert("contentId".to_string(), Value::from(content_id));
        }

        let value = self.public_action("objects:createUploadVerificationIntent", request_args)?;
        let object = value_object(&value)?;
        Ok(DownloadIntent {
            workspace_id: string_field(object, "workspaceId")?,
            object_key: string_field(object, "objectKey")?,
            range: None,
            signed_url: SignedUrlIntent {
                url: string_field(object, "signedUrl")?,
                expires_at: current_timestamp(),
            },
        })
    }

    fn mark_object_retention_state(
        &self,
        update: ObjectRetentionStateUpdate,
    ) -> ControlPlaneResult<ObjectMetadata> {
        if update.retention_state == RetentionState::DeleteEligible {
            return Err(ControlPlaneError::Limited {
                capability: "hosted-object-retention",
                reason: "delete-eligible retention requires hosted GC authority.",
            });
        }
        let proof_subject =
            object_retention_proof_subject(&update.object_key, update.retention_state);
        let requested_by_device_proof = self.device_proof(
            &update.workspace_id,
            "mark-object-retention-state",
            &proof_subject,
        )?;
        let value = self.public_action(
            "objects:markObjectRetentionState",
            args([
                ("objectKey", Value::from(update.object_key.clone())),
                ("requestedByDeviceId", Value::from(self.device_id.clone())),
                (
                    "requestedByDeviceProof",
                    Value::from(requested_by_device_proof),
                ),
                (
                    "retentionState",
                    Value::from(retention_state_value(update.retention_state)),
                ),
                ("workspaceId", Value::from(update.workspace_id.clone())),
            ]),
        )?;
        parse_storage_metadata(&value)
    }

    fn create_delete_intent(
        &self,
        request: DeleteIntentRequest,
    ) -> ControlPlaneResult<DeleteIntent> {
        let proof_subject = delete_intent_proof_subject(&request);
        let requested_by_device_proof = self.device_proof(
            &request.workspace_id,
            "create-delete-intent",
            &proof_subject,
        )?;
        let mut request_args = args([
            ("objectKey", Value::from(request.object_key.clone())),
            ("requestedByDeviceId", Value::from(self.device_id.clone())),
            (
                "requestedByDeviceProof",
                Value::from(requested_by_device_proof),
            ),
            ("workspaceId", Value::from(request.workspace_id.clone())),
        ]);
        if let Some(object_kind) = request.object_kind {
            request_args.insert("kind".to_string(), Value::from(object_kind.as_str()));
        }
        if let Some(key_epoch) = request.key_epoch {
            request_args.insert("keyEpoch".to_string(), number_value(u64::from(key_epoch)));
        }

        let value = self.public_action("objects:createDeleteIntent", request_args)?;
        let object = value_object(&value)?;
        Ok(DeleteIntent {
            workspace_id: string_field(object, "workspaceId")?,
            object_key: string_field(object, "objectKey")?,
            object_kind: parse_object_kind(&string_field(object, "kind")?)?,
            key_epoch: u64_field(object, "keyEpoch")? as u32,
            signed_url: SignedUrlIntent {
                url: string_field(object, "signedUrl")?,
                expires_at: current_timestamp(),
            },
        })
    }

    fn head_object_metadata(
        &self,
        workspace_id: &str,
        object_key: &str,
    ) -> ControlPlaneResult<ObjectMetadata> {
        let requested_by_device_proof =
            self.device_proof(workspace_id, "head-object-metadata", object_key)?;
        let value = self.public_query(
            "objectQueries:getObjectMetadata",
            args([
                ("objectKey", Value::from(object_key.to_string())),
                ("requestedByDeviceId", Value::from(self.device_id.clone())),
                (
                    "requestedByDeviceProof",
                    Value::from(requested_by_device_proof),
                ),
                ("workspaceId", Value::from(workspace_id.to_string())),
            ]),
        )?;
        if matches!(value, Value::Null) {
            return Err(ControlPlaneError::ObjectMissing {
                object_key: object_key.to_string(),
            });
        }
        parse_storage_metadata(&value)
    }

    fn commit_uploaded_object_metadata(
        &self,
        commit: ObjectMetadataCommit,
    ) -> ControlPlaneResult<ObjectMetadata> {
        self.require_local_device(&commit.committed_by_device_id)?;
        let proof_subject = object_metadata_proof_subject(&commit.object);
        let committed_by_device_proof = self.device_proof(
            &commit.workspace_id,
            "commit-uploaded-object-metadata",
            &proof_subject,
        )?;
        let value = self.public_action(
            "objects:commitUploadedObjectMetadata",
            args([
                (
                    "committedByDeviceId",
                    Value::from(commit.committed_by_device_id),
                ),
                (
                    "committedByDeviceProof",
                    Value::from(committed_by_device_proof),
                ),
                ("object", object_pointer_value(&commit.object)),
                ("workspaceId", Value::from(commit.workspace_id)),
            ]),
        )?;
        parse_storage_metadata(&value)
    }

    fn commit_object_manifest(
        &self,
        commit: ObjectManifestCommit,
    ) -> ControlPlaneResult<ObjectManifestRecord> {
        self.require_local_device(&commit.committed_by_device_id)?;
        let proof_subject = object_manifest_proof_subject(&commit);
        let committed_by_device_proof = self.device_proof(
            &commit.workspace_id,
            "commit-object-manifest",
            &proof_subject,
        )?;
        let value = self.public_action(
            "objects:commitObjectManifest",
            args([
                (
                    "committedByDeviceId",
                    Value::from(commit.committed_by_device_id.clone()),
                ),
                (
                    "committedByDeviceProof",
                    Value::from(committed_by_device_proof),
                ),
                ("manifestId", Value::from(commit.manifest_id.clone())),
                ("snapshotId", Value::from(commit.snapshot_id.clone())),
                (
                    "manifestObject",
                    object_pointer_value(&commit.manifest_object),
                ),
                ("packObjects", object_pointer_array(&commit.pack_objects)),
                ("workspaceId", Value::from(commit.workspace_id.clone())),
            ]),
        )?;
        let object = value_object(&value)?;
        Ok(ObjectManifestRecord {
            workspace_id: string_field(object, "workspaceId")?,
            snapshot_id: string_field(object, "snapshotId")?,
            manifest_id: string_field(object, "manifestId")?,
            manifest_object: commit.manifest_object,
            pack_objects: commit.pack_objects,
            committed_by_device_id: string_field(object, "committedByDeviceId")?,
            committed_at: current_timestamp(),
        })
    }

    fn get_snapshot_manifest_pointer(
        &self,
        workspace_id: &str,
        snapshot_id: &str,
    ) -> ControlPlaneResult<Option<ObjectManifestRecord>> {
        let requested_by_device_proof = self.device_proof(
            workspace_id,
            "get-snapshot-manifest-pointer",
            &snapshot_manifest_pointer_proof_subject(snapshot_id),
        )?;
        let value = self.public_query(
            "objectQueries:getSnapshotManifestPointer",
            args([
                ("requestedByDeviceId", Value::from(self.device_id.clone())),
                (
                    "requestedByDeviceProof",
                    Value::from(requested_by_device_proof),
                ),
                ("snapshotId", Value::from(snapshot_id.to_string())),
                ("workspaceId", Value::from(workspace_id.to_string())),
            ]),
        )?;
        if matches!(value, Value::Null) {
            return Ok(None);
        }
        parse_object_manifest_record(&value).map(Some)
    }

    fn create_work_view(&self, input: WorkViewCreate) -> ControlPlaneResult<WorkViewRecord> {
        self.require_local_device(&input.created_by_device_id)?;
        let proof_subject = work_view_create_proof_subject(&input);
        let created_by_device_proof =
            self.device_proof(&input.workspace_id, "create-work-view", &proof_subject)?;
        let value = self.public_mutation(
            "workViews:createWorkView",
            args([
                (
                    "baseSnapshotId",
                    Value::from(input.base_snapshot_id.clone()),
                ),
                (
                    "baseWorkspaceVersion",
                    number_value(input.base_workspace_version),
                ),
                (
                    "createdByDeviceId",
                    Value::from(input.created_by_device_id.clone()),
                ),
                ("createdByDeviceProof", Value::from(created_by_device_proof)),
                ("name", Value::from(input.name.clone())),
                ("projectId", Value::from(input.project_id.clone())),
                ("visiblePath", Value::from(input.visible_path.clone())),
                ("workViewId", Value::from(input.work_view_id.clone())),
                ("workspaceId", Value::from(input.workspace_id.clone())),
            ]),
        )?;
        parse_work_view_record(&value)
    }

    fn list_work_views(
        &self,
        workspace_id: &str,
        include_all: bool,
    ) -> ControlPlaneResult<Vec<WorkViewRecord>> {
        let proof_subject = format!("includeAll={include_all}");
        let requested_by_device_proof =
            self.device_proof(workspace_id, "list-work-views", &proof_subject)?;
        let value = self.public_query(
            "workViews:listWorkViews",
            args([
                ("includeAll", Value::Boolean(include_all)),
                ("requestedByDeviceId", Value::from(self.device_id.clone())),
                (
                    "requestedByDeviceProof",
                    Value::from(requested_by_device_proof),
                ),
                ("workspaceId", Value::from(workspace_id.to_string())),
            ]),
        )?;
        let Value::Array(values) = value else {
            return Err(shape_error("work view list must be an array"));
        };
        values.iter().map(parse_work_view_record).collect()
    }

    fn update_work_view_lifecycle(
        &self,
        input: WorkViewLifecycleUpdate,
    ) -> ControlPlaneResult<WorkViewRecord> {
        self.require_local_device(&input.updated_by_device_id)?;
        let proof_subject = work_view_lifecycle_proof_subject(&input);
        let updated_by_device_proof = self.device_proof(
            &input.workspace_id,
            "update-work-view-lifecycle",
            &proof_subject,
        )?;
        let value = self.public_mutation(
            "workViews:updateWorkViewLifecycle",
            args([
                ("lifecycle", Value::from(input.lifecycle.as_str())),
                (
                    "updatedByDeviceId",
                    Value::from(input.updated_by_device_id.clone()),
                ),
                ("updatedByDeviceProof", Value::from(updated_by_device_proof)),
                ("workViewId", Value::from(input.work_view_id.clone())),
                ("workspaceId", Value::from(input.workspace_id.clone())),
            ]),
        )?;
        parse_work_view_record(&value)
    }

    fn restore_work_view(
        &self,
        workspace_id: &str,
        work_view_id: &str,
        restored_by_device_id: &str,
    ) -> ControlPlaneResult<WorkViewRecord> {
        self.require_local_device(restored_by_device_id)?;
        let proof_subject = format!("workViewId={work_view_id}");
        let restored_by_device_proof =
            self.device_proof(workspace_id, "restore-work-view", &proof_subject)?;
        let value = self.public_mutation(
            "workViews:restoreWorkView",
            args([
                (
                    "restoredByDeviceId",
                    Value::from(restored_by_device_id.to_string()),
                ),
                (
                    "restoredByDeviceProof",
                    Value::from(restored_by_device_proof),
                ),
                ("workViewId", Value::from(work_view_id.to_string())),
                ("workspaceId", Value::from(workspace_id.to_string())),
            ]),
        )?;
        parse_work_view_record(&value)
    }

    fn commit_work_view_overlay(
        &self,
        input: WorkViewOverlayCommit,
    ) -> Result<WorkViewRecord, WorkViewUpdateError> {
        self.require_local_device(&input.committed_by_device_id)
            .map_err(WorkViewUpdateError::from)?;
        let proof_subject = work_view_overlay_proof_subject(&input);
        let committed_by_device_proof = self
            .device_proof(
                &input.workspace_id,
                "commit-work-view-overlay",
                &proof_subject,
            )
            .map_err(WorkViewUpdateError::from)?;
        let value = self
            .public_mutation(
                "workViews:commitOverlayPointer",
                args([
                    (
                        "committedByDeviceId",
                        Value::from(input.committed_by_device_id.clone()),
                    ),
                    (
                        "committedByDeviceProof",
                        Value::from(committed_by_device_proof),
                    ),
                    (
                        "expectedOverlayVersion",
                        number_value(input.expected_overlay_version),
                    ),
                    ("overlayObject", object_pointer_value(&input.overlay_object)),
                    ("workViewId", Value::from(input.work_view_id.clone())),
                    ("workspaceId", Value::from(input.workspace_id.clone())),
                ]),
            )
            .map_err(WorkViewUpdateError::from)?;
        let object = value_object(&value).map_err(WorkViewUpdateError::from)?;
        if object.contains_key("workspaceId") {
            return parse_work_view_record(&value).map_err(WorkViewUpdateError::from);
        }
        if bool_field(object, "ok").unwrap_or(false) {
            return parse_work_view_record(
                required_control_field(object, "workView").map_err(WorkViewUpdateError::from)?,
            )
            .map_err(WorkViewUpdateError::from);
        }

        match string_field(object, "error").as_deref() {
            Ok("work-view-missing") => Err(WorkViewUpdateError::WorkViewMissing {
                work_view_id: input.work_view_id,
            }),
            Ok("stale-overlay-head") => {
                let current = parse_work_view_record(
                    required_control_field(object, "currentWorkView")
                        .map_err(WorkViewUpdateError::from)?,
                )
                .map_err(WorkViewUpdateError::from)?;
                Err(WorkViewUpdateError::StaleOverlayHead(Box::new(
                    StaleWorkViewOverlayHead {
                        expected_overlay_version: input.expected_overlay_version,
                        current,
                    },
                )))
            }
            Ok(_) | Err(_) => Err(WorkViewUpdateError::Unsupported {
                capability: HOSTED_CAPABILITY,
                reason: "Convex work view overlay commit returned an unknown result shape",
            }),
        }
    }

    fn create_lease(&self, input: LeaseCreate) -> ControlPlaneResult<Lease> {
        self.require_local_device(&input.device_id)?;
        let proof_subject = lease_create_proof_subject(&input);
        let device_proof =
            self.device_proof(&input.workspace_id, "create-lease", &proof_subject)?;
        let mut request_args = args([
            (
                "baseSnapshotId",
                Value::from(input.base_snapshot_id.clone()),
            ),
            ("deviceId", Value::from(input.device_id.clone())),
            ("deviceProof", Value::from(device_proof)),
            (
                "executionState",
                Value::from(input.execution_state.as_str()),
            ),
            ("expiresAt", Value::from(input.expires_at.to_string())),
            ("leaseId", Value::from(input.lease_id.clone())),
            ("outputState", Value::from(input.output_state.as_str())),
            ("projectId", Value::from(input.project_id.clone())),
            ("statusCode", Value::from(input.status_code.clone())),
            (
                "writeTargetMode",
                Value::from(input.write_target_mode.as_str()),
            ),
            ("workspaceId", Value::from(input.workspace_id.clone())),
        ]);
        if let Some(work_view_id) = input.work_view_id.as_ref() {
            request_args.insert("workViewId".to_string(), Value::from(work_view_id.clone()));
        }
        if let Some(output_object) = input.output_object.as_ref() {
            request_args.insert(
                "outputObject".to_string(),
                object_pointer_value(output_object),
            );
        }
        if let Some(audit_object) = input.audit_object.as_ref() {
            request_args.insert(
                "auditObject".to_string(),
                object_pointer_value(audit_object),
            );
        }
        let value = self.public_mutation("events:createLease", request_args)?;
        let object = value_object(&value)?;
        parse_lease(required_control_field(object, "lease")?)
    }

    fn update_lease(&self, input: LeaseUpdate) -> ControlPlaneResult<Lease> {
        self.require_local_device(&input.updated_by_device_id)?;
        let proof_subject = lease_update_proof_subject(&input);
        let updated_by_device_proof =
            self.device_proof(&input.workspace_id, "update-lease", &proof_subject)?;
        let mut request_args = args([
            ("expectedVersion", number_value(input.expected_version)),
            ("leaseId", Value::from(input.lease_id.clone())),
            (
                "updatedByDeviceId",
                Value::from(input.updated_by_device_id.clone()),
            ),
            ("updatedByDeviceProof", Value::from(updated_by_device_proof)),
            ("workspaceId", Value::from(input.workspace_id.clone())),
        ]);
        if let Some(execution_state) = input.execution_state {
            request_args.insert(
                "executionState".to_string(),
                Value::from(execution_state.as_str()),
            );
        }
        if let Some(output_state) = input.output_state {
            request_args.insert(
                "outputState".to_string(),
                Value::from(output_state.as_str()),
            );
        }
        if let Some(status_code) = input.status_code.as_ref() {
            request_args.insert("statusCode".to_string(), Value::from(status_code.clone()));
        }
        if let Some(output_object) = input.output_object.as_ref() {
            request_args.insert(
                "outputObject".to_string(),
                object_pointer_value(output_object),
            );
        }
        if let Some(audit_object) = input.audit_object.as_ref() {
            request_args.insert(
                "auditObject".to_string(),
                object_pointer_value(audit_object),
            );
        }
        if let Some(event_kind) = input.event_kind {
            request_args.insert("eventKind".to_string(), Value::from(event_kind.as_str()));
        }

        let value = self.public_mutation("events:updateLease", request_args)?;
        let object = value_object(&value)?;
        if bool_field(object, "ok").unwrap_or(false) {
            return parse_lease(required_control_field(object, "lease")?);
        }
        match string_field(object, "error").as_deref() {
            Ok("lease-missing") => Err(ControlPlaneError::LeaseMissing {
                lease_id: input.lease_id,
            }),
            Ok("stale-lease") => Err(ControlPlaneError::Conflict {
                resource: "agent lease",
                reason: "lease version is stale",
            }),
            Ok(_) | Err(_) => Err(shape_error("events:updateLease returned an unknown shape")),
        }
    }

    fn list_leases(&self, workspace_id: &str) -> ControlPlaneResult<Vec<Lease>> {
        let requested_by_device_proof =
            self.device_proof(workspace_id, "list-leases", "compact=true")?;
        let value = self.public_query(
            "events:listLeases",
            args([
                ("requestedByDeviceId", Value::from(self.device_id.clone())),
                (
                    "requestedByDeviceProof",
                    Value::from(requested_by_device_proof),
                ),
                ("workspaceId", Value::from(workspace_id.to_string())),
            ]),
        )?;
        let Value::Array(values) = value else {
            return Err(shape_error("lease list must be an array"));
        };
        values.iter().map(parse_lease).collect()
    }

    fn create_device_request(
        &self,
        input: DeviceRequestInput,
    ) -> ControlPlaneResult<DeviceRequest> {
        if let Some(bootstrap_token) = &self.bootstrap_token {
            let mut request_args = args([
                ("bootstrapToken", Value::from(bootstrap_token.clone())),
                ("deviceId", Value::from(input.device_id.clone())),
                ("deviceName", Value::from(input.device_name.clone())),
                (
                    "deviceFingerprint",
                    Value::from(input.device_fingerprint.clone()),
                ),
                (
                    "deviceAuthorizationProofVerifier",
                    Value::from(input.device_authorization_proof_verifier.clone()),
                ),
                (
                    "devicePublicKey",
                    Value::from(input.device_public_key.clone()),
                ),
                ("expiresInTicks", number_value(input.expires_in_ticks)),
                ("matchingCode", Value::from(input.matching_code.clone())),
                ("platform", Value::from(input.platform.clone())),
                ("workspaceId", Value::from(input.workspace_id.clone())),
            ]);
            if let Some(host) = input.host {
                request_args.insert("host".to_string(), Value::from(host));
            }
            if let Some(root) = input.root {
                request_args.insert("root".to_string(), Value::from(root));
            }

            let value =
                self.public_mutation("devices:createPendingDeviceWithBootstrap", request_args)?;
            return parse_device_request(&value);
        }

        let mut request_args = args([
            (
                "accountSessionId",
                Value::from(self.verified_account_session_id(Some(&input.workspace_id))?),
            ),
            ("deviceId", Value::from(input.device_id.clone())),
            ("deviceName", Value::from(input.device_name.clone())),
            (
                "deviceFingerprint",
                Value::from(input.device_fingerprint.clone()),
            ),
            (
                "deviceAuthorizationProofVerifier",
                Value::from(input.device_authorization_proof_verifier.clone()),
            ),
            (
                "devicePublicKey",
                Value::from(input.device_public_key.clone()),
            ),
            ("expiresInTicks", number_value(input.expires_in_ticks)),
            ("matchingCode", Value::from(input.matching_code.clone())),
            ("platform", Value::from(input.platform.clone())),
            ("workspaceId", Value::from(input.workspace_id.clone())),
        ]);
        if let Some(host) = input.host {
            request_args.insert("host".to_string(), Value::from(host));
        }
        if let Some(root) = input.root {
            request_args.insert("root".to_string(), Value::from(root));
        }

        let value = self.public_mutation("devices:createPendingDevice", request_args)?;
        parse_device_request(&value)
    }

    fn create_bootstrap_session(
        &self,
        input: BootstrapSessionInput,
    ) -> ControlPlaneResult<BootstrapSession> {
        let token = generate_bootstrap_token()?;
        let token_hash = sha256_token_hash(token.as_bytes());
        let proof_subject = bootstrap_session_proof_subject(&input, &token_hash);
        let mut request_args = args([
            ("bootstrapToken", Value::from(token.clone())),
            ("expiresInTicks", number_value(input.expires_in_ticks)),
            ("workspaceId", Value::from(input.workspace_id.clone())),
        ]);
        if self.account_session_auth_available() {
            request_args.insert(
                "accountSessionId".to_string(),
                Value::from(self.verified_account_session_id(Some(&input.workspace_id))?),
            );
        } else {
            let created_by_device_proof = self.device_proof(
                &input.workspace_id,
                "create-bootstrap-session",
                &proof_subject,
            )?;
            request_args.insert(
                "createdByDeviceId".to_string(),
                Value::from(self.device_id.clone()),
            );
            request_args.insert(
                "createdByDeviceProof".to_string(),
                Value::from(created_by_device_proof),
            );
        }
        if let Some(host) = input.host {
            request_args.insert("host".to_string(), Value::from(host));
        }
        if let Some(root) = input.root {
            request_args.insert("root".to_string(), Value::from(root));
        }

        let value = self.public_mutation("devices:createBootstrapSession", request_args)?;
        parse_bootstrap_session(&value, token)
    }

    fn create_first_authorized_device(
        &self,
        input: FirstAuthorizedDeviceInput,
    ) -> ControlPlaneResult<AuthorizedDeviceRecord> {
        let value = self.public_mutation(
            "devices:createFirstAuthorizedDevice",
            args([
                (
                    "accountSessionId",
                    Value::from(self.verified_account_session_id(Some(&input.workspace_id))?),
                ),
                ("deviceFingerprint", Value::from(input.device_fingerprint)),
                (
                    "deviceAuthorizationProofVerifier",
                    Value::from(input.device_authorization_proof_verifier),
                ),
                ("deviceId", Value::from(input.device_id)),
                ("deviceName", Value::from(input.device_name)),
                ("platform", Value::from(input.platform)),
                ("workspaceId", Value::from(input.workspace_id)),
            ]),
        )?;
        parse_authorized_device(&value)
    }

    fn list_device_trust(
        &self,
        workspace_id: &str,
    ) -> ControlPlaneResult<DeviceApprovalRequestList> {
        let value = self.public_query(
            "devices:listDeviceTrust",
            args([
                (
                    "accountSessionId",
                    Value::from(self.verified_account_session_id(Some(workspace_id))?),
                ),
                ("workspaceId", Value::from(workspace_id.to_string())),
            ]),
        )?;
        let object = value_object(&value)?;
        Ok(DeviceApprovalRequestList {
            pending_requests: array_field(object, "pendingRequests")?
                .iter()
                .map(parse_device_request)
                .collect::<ControlPlaneResult<Vec<_>>>()?,
            authorized_devices: array_field(object, "authorizedDevices")?
                .iter()
                .map(parse_authorized_device)
                .collect::<ControlPlaneResult<Vec<_>>>()?,
            revoked_devices: array_field(object, "revokedDevices")?
                .iter()
                .map(parse_revoked_device)
                .collect::<ControlPlaneResult<Vec<_>>>()?,
        })
    }

    fn approve_device_request(
        &self,
        input: DeviceApprovalInput,
    ) -> ControlPlaneResult<DeviceApproval> {
        let value = self.public_mutation(
            "devices:approveDeviceRequest",
            args([
                ("approverDeviceId", Value::from(input.approved_by_device_id)),
                (
                    "approverDeviceProof",
                    Value::from(input.approved_by_device_proof),
                ),
                ("ciphertext", Value::from(input.encrypted_grant_ciphertext)),
                ("expiresInTicks", number_value(input.expires_in_ticks)),
                (
                    "grantAcceptanceProofVerifier",
                    Value::from(input.grant_acceptance_proof_verifier),
                ),
                ("keyEpoch", number_value(input.key_epoch.into())),
                ("requestId", Value::from(input.request_id)),
            ]),
        )?;
        parse_device_approval(&value)
    }

    fn deny_device_request(&self, input: DeviceDenialInput) -> ControlPlaneResult<DeviceDenial> {
        let value = self.public_mutation(
            "devices:denyDeviceRequest",
            args([
                ("deniedByDeviceId", Value::from(input.denied_by_device_id)),
                (
                    "deniedByDeviceProof",
                    Value::from(input.denied_by_device_proof),
                ),
                ("reason", Value::from(input.reason)),
                ("requestId", Value::from(input.request_id)),
            ]),
        )?;
        parse_device_denial(&value)
    }

    fn revoke_device(
        &self,
        input: DeviceRevocationInput,
    ) -> ControlPlaneResult<RevokedDeviceRecord> {
        let value = self.public_mutation(
            "devices:revokeDevice",
            args([
                ("deviceId", Value::from(input.device_id)),
                ("reason", Value::from(input.reason)),
                ("revokedByDeviceId", Value::from(input.revoked_by_device_id)),
                (
                    "revokedByDeviceProof",
                    Value::from(input.revoked_by_device_proof),
                ),
                ("workspaceId", Value::from(input.workspace_id)),
            ]),
        )?;
        parse_revoked_device(&value)
    }

    fn get_encrypted_device_grant(
        &self,
        request_id: &str,
        device_id: &str,
    ) -> ControlPlaneResult<Option<DeviceApproval>> {
        if let Some(bootstrap_token) = &self.bootstrap_token {
            let value = self.public_query(
                "devices:getEncryptedGrantWithBootstrap",
                args([
                    ("bootstrapToken", Value::from(bootstrap_token.clone())),
                    ("deviceId", Value::from(device_id.to_string())),
                    ("requestId", Value::from(request_id.to_string())),
                ]),
            )?;
            return if matches!(value, Value::Null) {
                Ok(None)
            } else {
                parse_device_approval(&value).map(Some)
            };
        }

        let value = self.public_query(
            "devices:getEncryptedGrant",
            args([
                ("deviceId", Value::from(device_id.to_string())),
                ("requestId", Value::from(request_id.to_string())),
            ]),
        )?;
        if matches!(value, Value::Null) {
            Ok(None)
        } else {
            parse_device_approval(&value).map(Some)
        }
    }

    fn confirm_device_grant_accepted(
        &self,
        input: GrantAcceptanceInput,
    ) -> ControlPlaneResult<DeviceApproval> {
        if let Some(bootstrap_token) = &self.bootstrap_token {
            let value = self.public_mutation(
                "devices:confirmGrantAcceptedWithBootstrap",
                args([
                    ("bootstrapToken", Value::from(bootstrap_token.clone())),
                    ("deviceId", Value::from(input.device_id)),
                    (
                        "grantAcceptanceProof",
                        Value::from(input.grant_acceptance_proof),
                    ),
                    ("requestId", Value::from(input.request_id)),
                ]),
            )?;
            return parse_device_approval(&value);
        }

        let value = self.public_mutation(
            "devices:confirmGrantAccepted",
            args([
                ("deviceId", Value::from(input.device_id)),
                (
                    "grantAcceptanceProof",
                    Value::from(input.grant_acceptance_proof),
                ),
                ("requestId", Value::from(input.request_id)),
            ]),
        )?;
        parse_device_approval(&value)
    }

    fn create_recovery_envelope(
        &self,
        input: RecoveryEnvelopeInput,
    ) -> ControlPlaneResult<RecoveryEnvelopeRecord> {
        let value = self.public_mutation(
            "recovery:createRecoveryEnvelope",
            args([
                ("ciphertext", Value::from(input.ciphertext)),
                ("createdByDeviceId", Value::from(input.created_by_device_id)),
                (
                    "createdByDeviceProof",
                    Value::from(input.created_by_device_proof),
                ),
                ("envelopeId", Value::from(input.envelope_id)),
                ("fingerprint", Value::from(input.fingerprint)),
                (
                    "recoveryProofVerifier",
                    Value::from(input.recovery_proof_verifier),
                ),
                ("workspaceId", Value::from(input.workspace_id)),
            ]),
        )?;
        parse_recovery_envelope(&value)
    }

    fn verify_recovery_envelope(
        &self,
        workspace_id: &str,
        envelope_id: &str,
        verified_by_device_id: &str,
        verified_by_device_proof: &str,
        recovery_proof: &str,
    ) -> ControlPlaneResult<RecoveryEnvelopeRecord> {
        let value = self.public_mutation(
            "recovery:verifyRecoveryEnvelope",
            args([
                ("envelopeId", Value::from(envelope_id.to_string())),
                ("recoveryProof", Value::from(recovery_proof.to_string())),
                (
                    "verifiedByDeviceId",
                    Value::from(verified_by_device_id.to_string()),
                ),
                (
                    "verifiedByDeviceProof",
                    Value::from(verified_by_device_proof.to_string()),
                ),
                ("workspaceId", Value::from(workspace_id.to_string())),
            ]),
        )?;
        parse_recovery_envelope(&value)
    }

    fn rotate_recovery_envelope(
        &self,
        input: RecoveryEnvelopeInput,
    ) -> ControlPlaneResult<RecoveryEnvelopeRecord> {
        let value = self.public_mutation(
            "recovery:rotateRecoveryEnvelope",
            args([
                ("ciphertext", Value::from(input.ciphertext)),
                ("createdByDeviceId", Value::from(input.created_by_device_id)),
                (
                    "createdByDeviceProof",
                    Value::from(input.created_by_device_proof),
                ),
                ("envelopeId", Value::from(input.envelope_id)),
                ("fingerprint", Value::from(input.fingerprint)),
                (
                    "recoveryProofVerifier",
                    Value::from(input.recovery_proof_verifier),
                ),
                ("workspaceId", Value::from(input.workspace_id)),
            ]),
        )?;
        parse_recovery_envelope(&value)
    }

    fn revoke_recovery_envelope(
        &self,
        workspace_id: &str,
        envelope_id: &str,
        revoked_by_device_id: &str,
        revoked_by_device_proof: &str,
    ) -> ControlPlaneResult<RecoveryEnvelopeRecord> {
        let value = self.public_mutation(
            "recovery:revokeRecoveryEnvelope",
            args([
                ("envelopeId", Value::from(envelope_id.to_string())),
                (
                    "revokedByDeviceId",
                    Value::from(revoked_by_device_id.to_string()),
                ),
                (
                    "revokedByDeviceProof",
                    Value::from(revoked_by_device_proof.to_string()),
                ),
                ("workspaceId", Value::from(workspace_id.to_string())),
            ]),
        )?;
        parse_recovery_envelope(&value)
    }

    fn list_recovery_envelopes(
        &self,
        workspace_id: &str,
    ) -> ControlPlaneResult<Vec<RecoveryEnvelopeRecord>> {
        let value = self.public_query(
            "recovery:getRecoveryEnvelopes",
            args([
                (
                    "accountSessionId",
                    Value::from(self.verified_account_session_id(Some(workspace_id))?),
                ),
                ("workspaceId", Value::from(workspace_id.to_string())),
            ]),
        )?;
        let Value::Array(values) = value else {
            return Err(shape_error("recovery envelope list must be an array"));
        };
        values.iter().map(parse_recovery_envelope).collect()
    }

    fn authorize_device_with_recovery(
        &self,
        input: RecoveryDeviceAuthorizationInput,
    ) -> ControlPlaneResult<DeviceApproval> {
        let value = self.public_mutation(
            "recovery:authorizeDeviceWithRecovery",
            args([
                (
                    "accountSessionId",
                    Value::from(self.verified_account_session_id(Some(&input.workspace_id))?),
                ),
                ("ciphertext", Value::from(input.encrypted_grant_ciphertext)),
                ("envelopeId", Value::from(input.envelope_id)),
                ("expiresInTicks", number_value(input.expires_in_ticks)),
                (
                    "grantAcceptanceProofVerifier",
                    Value::from(input.grant_acceptance_proof_verifier),
                ),
                ("keyEpoch", number_value(u64::from(input.key_epoch))),
                ("recoveryProof", Value::from(input.recovery_proof)),
                ("requestId", Value::from(input.request_id)),
                ("workspaceId", Value::from(input.workspace_id)),
            ]),
        )?;
        parse_device_approval(&value)
    }
}

pub fn hosted_function_call_counts() -> Vec<HostedFunctionCallCount> {
    hosted_function_call_count_store()
        .lock()
        .map(|counts| {
            counts
                .iter()
                .map(|(function_name, call_count)| HostedFunctionCallCount {
                    function_name: function_name.clone(),
                    call_count: *call_count,
                })
                .collect()
        })
        .unwrap_or_default()
}

fn hosted_function_call_count_store() -> &'static Mutex<BTreeMap<String, u64>> {
    HOSTED_FUNCTION_CALL_COUNTS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn record_hosted_function_call(name: &str) {
    if let Ok(mut counts) = hosted_function_call_count_store().lock() {
        *counts.entry(name.to_string()).or_default() += 1;
    }
}

#[cfg(test)]
fn reset_hosted_function_call_counts() {
    if let Ok(mut counts) = hosted_function_call_count_store().lock() {
        counts.clear();
    }
}

type ConvexArgs = BTreeMap<String, Value>;

async fn convex_rpc_timeout<F>(future: F) -> ControlPlaneResult<Value>
where
    F: Future<Output = ControlPlaneResult<Value>>,
{
    tokio::time::timeout(CONVEX_RPC_TIMEOUT, future)
        .await
        .map_err(|_| ControlPlaneError::Limited {
            capability: HOSTED_CAPABILITY,
            reason: "hosted Convex request timed out",
        })?
}

fn args<const N: usize>(entries: [(&'static str, Value); N]) -> ConvexArgs {
    entries
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect()
}

fn unwrap_function_result(result: FunctionResult) -> ControlPlaneResult<Value> {
    match result {
        FunctionResult::Value(value) => Ok(value),
        FunctionResult::ErrorMessage(message) => Err(ControlPlaneError::Storage(format!(
            "Convex function failed: {message}"
        ))),
        FunctionResult::ConvexError(error) => Err(ControlPlaneError::Storage(format!(
            "Convex function failed: {error:?}"
        ))),
    }
}

fn map_convex_error(error: impl fmt::Display) -> ControlPlaneError {
    ControlPlaneError::Storage(format!("Convex client failed: {error}"))
}

fn value_object(value: &Value) -> ControlPlaneResult<&BTreeMap<String, Value>> {
    match value {
        Value::Object(object) => Ok(object),
        _ => Err(shape_error("expected Convex object")),
    }
}

fn required_field<'a>(
    object: &'a BTreeMap<String, Value>,
    field: &'static str,
) -> Result<&'a Value, CompareAndSwapError> {
    object.get(field).ok_or(CompareAndSwapError::Unsupported {
        capability: HOSTED_CAPABILITY,
        reason: "Convex result was missing a required field",
    })
}

fn string_field(
    object: &BTreeMap<String, Value>,
    field: &'static str,
) -> ControlPlaneResult<String> {
    match object.get(field) {
        Some(Value::String(value)) => Ok(value.clone()),
        _ => Err(shape_error("expected Convex string field")),
    }
}

fn value_string(value: &Value) -> ControlPlaneResult<String> {
    match value {
        Value::String(value) => Ok(value.clone()),
        _ => Err(shape_error("expected Convex string value")),
    }
}

fn optional_string_field(
    object: &BTreeMap<String, Value>,
    field: &'static str,
) -> ControlPlaneResult<Option<String>> {
    match object.get(field) {
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(Value::Null) | None => Ok(None),
        _ => Err(shape_error("expected optional Convex string field")),
    }
}

fn u64_field(object: &BTreeMap<String, Value>, field: &'static str) -> ControlPlaneResult<u64> {
    match object.get(field) {
        Some(value) => value_u64(value),
        None => Err(shape_error("expected Convex numeric field")),
    }
}

fn value_u64(value: &Value) -> ControlPlaneResult<u64> {
    match value {
        Value::Int64(value) if *value >= 0 => Ok(*value as u64),
        Value::Float64(value) if value.is_finite() && *value >= 0.0 && value.fract() == 0.0 => {
            Ok(*value as u64)
        }
        _ => Err(shape_error("expected non-negative integer-valued number")),
    }
}

fn bool_field(object: &BTreeMap<String, Value>, field: &'static str) -> ControlPlaneResult<bool> {
    match object.get(field) {
        Some(Value::Boolean(value)) => Ok(*value),
        _ => Err(shape_error("expected Convex boolean field")),
    }
}

fn array_field<'a>(
    object: &'a BTreeMap<String, Value>,
    field: &'static str,
) -> ControlPlaneResult<&'a Vec<Value>> {
    match object.get(field) {
        Some(Value::Array(value)) => Ok(value),
        _ => Err(shape_error("expected Convex array field")),
    }
}

fn parse_workspace_ref(value: &Value) -> ControlPlaneResult<WorkspaceRef> {
    let object = value_object(value)?;
    Ok(WorkspaceRef {
        workspace_id: string_field(object, "workspaceId")?,
        version: u64_field(object, "version")?,
        snapshot_id: string_field(object, "snapshotId")?,
        updated_at: current_timestamp(),
        updated_by_device_id: optional_string_field(object, "updatedByDeviceId")?,
    })
}

fn parse_compact_event(value: &Value) -> ControlPlaneResult<CompactEvent> {
    let object = value_object(value)?;
    Ok(CompactEvent {
        event_id: string_field(object, "eventId")?,
        workspace_id: string_field(object, "workspaceId")?,
        at: current_timestamp(),
        kind: parse_event_kind(&string_field(object, "kind")?)?,
        subject: string_field(object, "subject")?,
    })
}

fn parse_event_kind(kind: &str) -> ControlPlaneResult<CompactEventKind> {
    match kind {
        "device.harness_approved" => Ok(CompactEventKind::DeviceHarnessApproved),
        "device.approval_requested" => Ok(CompactEventKind::DeviceApprovalRequested),
        "device.approved" => Ok(CompactEventKind::DeviceApproved),
        "device.denied" => Ok(CompactEventKind::DeviceDenied),
        "device.revoked" => Ok(CompactEventKind::DeviceRevoked),
        "device.requested" => Ok(CompactEventKind::DeviceRequested),
        "recovery_key.created" => Ok(CompactEventKind::RecoveryKeyCreated),
        "recovery_key.verified" => Ok(CompactEventKind::RecoveryKeyVerified),
        "recovery_key.rotated" => Ok(CompactEventKind::RecoveryKeyRotated),
        "recovery_key.revoked" => Ok(CompactEventKind::RecoveryKeyRevoked),
        "auth.login_started" => Ok(CompactEventKind::AuthLoginStarted),
        "auth.login_completed" => Ok(CompactEventKind::AuthLoginCompleted),
        "conflict.detected" => Ok(CompactEventKind::ConflictDetected),
        "conflict.resolved" => Ok(CompactEventKind::ConflictResolved),
        "lease.blocked" => Ok(CompactEventKind::LeaseBlocked),
        "lease.cleanup_completed" => Ok(CompactEventKind::LeaseCleanupCompleted),
        "lease.completed" => Ok(CompactEventKind::LeaseCompleted),
        "lease.created" => Ok(CompactEventKind::LeaseCreated),
        "lease.expired" => Ok(CompactEventKind::LeaseExpired),
        "lease.hydration_requested" => Ok(CompactEventKind::LeaseHydrationRequested),
        "lease.revoked" => Ok(CompactEventKind::LeaseRevoked),
        "lease.review_ready" => Ok(CompactEventKind::LeaseReviewReady),
        "lease.tool_denied" => Ok(CompactEventKind::LeaseToolDenied),
        "lease.tool_invoked" => Ok(CompactEventKind::LeaseToolInvoked),
        "lease.updated" => Ok(CompactEventKind::LeaseUpdated),
        "object_manifest.committed" => Ok(CompactEventKind::ObjectManifestCommitted),
        "object_pointer.added" => Ok(CompactEventKind::ObjectPointerAdded),
        "overlay.changed" => Ok(CompactEventKind::OverlayChanged),
        "publish.requested" => Ok(CompactEventKind::PublishRequested),
        "work.accepted" => Ok(CompactEventKind::WorkAccepted),
        "work.archived" => Ok(CompactEventKind::WorkArchived),
        "work.cleanup_completed" => Ok(CompactEventKind::WorkCleanupCompleted),
        "work.cleanup_previewed" => Ok(CompactEventKind::WorkCleanupPreviewed),
        "work.created" => Ok(CompactEventKind::WorkCreated),
        "work.discarded" => Ok(CompactEventKind::WorkDiscarded),
        "work.expired" => Ok(CompactEventKind::WorkExpired),
        "work.restored" => Ok(CompactEventKind::WorkRestored),
        "work.review_ready" => Ok(CompactEventKind::WorkReviewReady),
        "work.updated" => Ok(CompactEventKind::WorkUpdated),
        "workspace.created" => Ok(CompactEventKind::WorkspaceCreated),
        "workspace_ref.advanced" => Ok(CompactEventKind::WorkspaceRefAdvanced),
        _ => Err(shape_error("unknown compact event kind")),
    }
}

fn parse_device_request(value: &Value) -> ControlPlaneResult<DeviceRequest> {
    let object = value_object(value)?;
    Ok(DeviceRequest {
        request_id: string_field(object, "requestId")?,
        workspace_id: string_field(object, "workspaceId")?,
        device_id: string_field(object, "deviceId")?,
        device_name: string_field(object, "deviceName")?,
        platform: string_field(object, "platform")?,
        device_public_key: string_field(object, "devicePublicKey")?,
        device_fingerprint: string_field(object, "deviceFingerprint")?,
        matching_code: string_field(object, "matchingCode")?,
        account_id: optional_string_field(object, "accountId")?,
        host: optional_string_field(object, "host")?,
        root: optional_string_field(object, "root")?,
        requested_at: current_timestamp(),
        expires_at: current_timestamp(),
        state: parse_device_request_state(
            &string_field(object, "state").unwrap_or_else(|_| "pending".to_string()),
        )?,
    })
}

fn parse_device_request_state(state: &str) -> ControlPlaneResult<DeviceRequestState> {
    match state {
        "pending" => Ok(DeviceRequestState::Pending),
        "approved" => Ok(DeviceRequestState::Approved),
        "denied" => Ok(DeviceRequestState::Denied),
        "expired" => Ok(DeviceRequestState::Expired),
        _ => Err(shape_error("unknown device request state")),
    }
}

fn parse_bootstrap_session(value: &Value, token: String) -> ControlPlaneResult<BootstrapSession> {
    let object = value_object(value)?;
    Ok(BootstrapSession {
        session_id: string_field(object, "sessionId")?,
        workspace_id: string_field(object, "workspaceId")?,
        token,
        expires_at: current_timestamp(),
    })
}

fn parse_authorized_device(value: &Value) -> ControlPlaneResult<AuthorizedDeviceRecord> {
    let object = value_object(value)?;
    Ok(AuthorizedDeviceRecord {
        workspace_id: string_field(object, "workspaceId")?,
        device_id: string_field(object, "deviceId")?,
        device_name: string_field(object, "deviceName")?,
        platform: string_field(object, "platform")?,
        device_fingerprint: string_field(object, "deviceFingerprint")?,
        authorized_at: current_timestamp(),
        authorized_by_device_id: optional_string_field(object, "authorizedByDeviceId")?,
        revoked_at: None,
    })
}

fn parse_revoked_device(value: &Value) -> ControlPlaneResult<RevokedDeviceRecord> {
    let object = value_object(value)?;
    Ok(RevokedDeviceRecord {
        workspace_id: string_field(object, "workspaceId")?,
        device_id: string_field(object, "deviceId")?,
        device_name: string_field(object, "deviceName")?,
        platform: string_field(object, "platform")?,
        device_fingerprint: string_field(object, "deviceFingerprint")?,
        revoked_at: current_timestamp(),
        revoked_by_device_id: string_field(object, "revokedByDeviceId")?,
        reason: string_field(object, "reason")?,
    })
}

fn parse_device_approval(value: &Value) -> ControlPlaneResult<DeviceApproval> {
    let object = value_object(value)?;
    Ok(DeviceApproval {
        grant_id: string_field(object, "grantId")?,
        request_id: string_field(object, "requestId")?,
        workspace_id: string_field(object, "workspaceId")?,
        device_id: string_field(object, "deviceId")
            .or_else(|_| string_field(object, "requesterDeviceId"))?,
        device_name: string_field(object, "deviceName")?,
        platform: string_field(object, "platform")?,
        device_fingerprint: string_field(object, "deviceFingerprint")
            .or_else(|_| string_field(object, "requesterDeviceFingerprint"))?,
        approved_by_device_id: string_field(object, "approverDeviceId")?,
        encrypted_grant_ciphertext: string_field(object, "ciphertext")?,
        key_epoch: u64_field(object, "keyEpoch")? as u32,
        granted_at: current_timestamp(),
        expires_at: current_timestamp(),
        accepted_at: optional_string_field(object, "acceptedAt")?.map(|_| current_timestamp()),
        harness_only: false,
    })
}

fn parse_device_denial(value: &Value) -> ControlPlaneResult<DeviceDenial> {
    let object = value_object(value)?;
    Ok(DeviceDenial {
        request_id: string_field(object, "requestId")?,
        workspace_id: string_field(object, "workspaceId")?,
        device_id: string_field(object, "deviceId")?,
        denied_by_device_id: string_field(object, "deniedByDeviceId")?,
        denied_at: current_timestamp(),
        reason: string_field(object, "reason")?,
    })
}

fn parse_recovery_envelope(value: &Value) -> ControlPlaneResult<RecoveryEnvelopeRecord> {
    let object = value_object(value)?;
    Ok(RecoveryEnvelopeRecord {
        workspace_id: string_field(object, "workspaceId")?,
        envelope_id: string_field(object, "envelopeId")?,
        created_by_device_id: string_field(object, "createdByDeviceId")?,
        ciphertext: string_field(object, "ciphertext")?,
        fingerprint: string_field(object, "fingerprint")?,
        state: parse_recovery_envelope_state(&string_field(object, "state")?)?,
        created_at: current_timestamp(),
        verified_at: optional_string_field(object, "verifiedAt")?.map(|_| current_timestamp()),
        rotated_at: optional_string_field(object, "rotatedAt")?.map(|_| current_timestamp()),
        revoked_at: optional_string_field(object, "revokedAt")?.map(|_| current_timestamp()),
    })
}

fn parse_recovery_envelope_state(state: &str) -> ControlPlaneResult<RecoveryEnvelopeState> {
    match state {
        "generated-unverified" => Ok(RecoveryEnvelopeState::GeneratedUnverified),
        "active" => Ok(RecoveryEnvelopeState::Active),
        "rotated" => Ok(RecoveryEnvelopeState::Rotated),
        "revoked" => Ok(RecoveryEnvelopeState::Revoked),
        _ => Err(shape_error("unknown recovery envelope state")),
    }
}

fn parse_object_kind(kind: &str) -> ControlPlaneResult<ObjectKind> {
    match kind {
        "source-pack" => Ok(ObjectKind::SourcePack),
        "index-pack" => Ok(ObjectKind::IndexPack),
        "locator-index" => Ok(ObjectKind::LocatorIndex),
        "snapshot-manifest" => Ok(ObjectKind::SnapshotManifest),
        "overlay-pack" | "agent-overlay" => Ok(ObjectKind::AgentOverlay),
        _ => Err(shape_error("unknown object kind")),
    }
}

fn parse_storage_metadata(value: &Value) -> ControlPlaneResult<ObjectMetadata> {
    let object = value_object(value)?;
    let object_key = string_field(object, "objectKey")?;
    Ok(ObjectMetadata {
        key: StorageObjectKey::new(object_key).map_err(|_| {
            ControlPlaneError::InvalidObjectKey {
                reason: "object keys must be generated opaque pack, manifest, or overlay keys",
            }
        })?,
        kind: parse_storage_object_kind(&string_field(object, "kind")?)?,
        byte_len: u64_field(object, "byteLength")?,
        hash: string_field(object, "hash")?,
        key_epoch: u64_field(object, "keyEpoch")? as u32,
        created_by_device_id: None,
        created_at_unix_ms: current_timestamp().tick,
        retention_state: parse_retention_state(
            &string_field(object, "retentionState").unwrap_or_else(|_| "current".to_string()),
        )?,
        retain_until_unix_ms: None,
    })
}

fn parse_object_manifest_record(value: &Value) -> ControlPlaneResult<ObjectManifestRecord> {
    let object = value_object(value)?;
    Ok(ObjectManifestRecord {
        workspace_id: string_field(object, "workspaceId")?,
        snapshot_id: string_field(object, "snapshotId")?,
        manifest_id: string_field(object, "manifestId")?,
        manifest_object: parse_object_pointer(required_control_field(object, "manifestObject")?)?,
        pack_objects: array_field(object, "packObjects")?
            .iter()
            .map(parse_object_pointer)
            .collect::<ControlPlaneResult<Vec<_>>>()?,
        committed_by_device_id: string_field(object, "committedByDeviceId")?,
        committed_at: current_timestamp(),
    })
}

fn parse_object_pointer(value: &Value) -> ControlPlaneResult<ObjectPointer> {
    let object = value_object(value)?;
    Ok(ObjectPointer {
        object_key: string_field(object, "objectKey")?,
        content_id: string_field(object, "contentId")?,
        byte_len: u64_field(object, "byteLength")?,
        hash: string_field(object, "hash")?,
        key_epoch: u64_field(object, "keyEpoch")? as u32,
        kind: parse_object_kind(&string_field(object, "kind")?)?,
        created_at: current_timestamp(),
    })
}

fn parse_conflict_metadata_record(value: &Value) -> ControlPlaneResult<ConflictMetadataRecord> {
    let object = value_object(value)?;
    Ok(ConflictMetadataRecord {
        workspace_id: string_field(object, "workspaceId")?,
        conflict_id: string_field(object, "conflictId")?,
        conflict_kind: string_field(object, "conflictKind")?,
        paths: array_field(object, "paths")?
            .iter()
            .map(value_string)
            .collect::<ControlPlaneResult<Vec<_>>>()?,
        contains_secrets: bool_field(object, "containsSecrets")?,
        state: string_field(object, "state")?,
        base_snapshot_id: string_field(object, "baseSnapshotId")?,
        remote_snapshot_id: string_field(object, "remoteSnapshotId")?,
        detected_by_device_id: string_field(object, "detectedByDeviceId")?,
        bundle_object: match object.get("bundleObject") {
            Some(Value::Null) | None => None,
            Some(value) => Some(parse_object_pointer(value)?),
        },
        detected_at: current_timestamp(),
        resolved_by_device_id: optional_string_field(object, "resolvedByDeviceId")?,
        resolved_at: optional_string_field(object, "resolvedAt")?.map(|_| current_timestamp()),
    })
}

fn parse_work_view_record(value: &Value) -> ControlPlaneResult<WorkViewRecord> {
    let object = value_object(value)?;
    if let Some(work_view) = object.get("workView") {
        return parse_work_view_record(work_view);
    }
    Ok(WorkViewRecord {
        workspace_id: string_field(object, "workspaceId")?,
        work_view_id: string_field(object, "workViewId")?,
        project_id: string_field(object, "projectId")?,
        name: string_field(object, "name")?,
        visible_path: string_field(object, "visiblePath")?,
        base_snapshot_id: string_field(object, "baseSnapshotId")?,
        base_workspace_version: u64_field(object, "baseWorkspaceVersion")?,
        overlay_head: optional_object_pointer_field(object, "overlayHead")?,
        overlay_version: u64_field(object, "overlayVersion")?,
        lifecycle: parse_work_view_lifecycle(&string_field(object, "lifecycle")?)?,
        created_by_device_id: string_field(object, "createdByDeviceId")?,
        updated_by_device_id: string_field(object, "updatedByDeviceId")?,
        created_at: current_timestamp(),
        updated_at: current_timestamp(),
    })
}

fn parse_lease(value: &Value) -> ControlPlaneResult<Lease> {
    let object = value_object(value)?;
    if let Some(lease) = object.get("lease") {
        return parse_lease(lease);
    }
    Ok(Lease {
        lease_id: string_field(object, "leaseId")?,
        workspace_id: string_field(object, "workspaceId")?,
        project_id: string_field(object, "projectId")?,
        device_id: string_field(object, "deviceId")?,
        write_target_mode: parse_lease_write_target_mode(&string_field(
            object,
            "writeTargetMode",
        )?)?,
        work_view_id: optional_string_field(object, "workViewId")?,
        base_snapshot_id: string_field(object, "baseSnapshotId")?,
        version: u64_field(object, "version")?,
        execution_state: parse_lease_execution_state(&string_field(object, "executionState")?)?,
        output_state: parse_lease_output_state(&string_field(object, "outputState")?)?,
        status_code: string_field(object, "statusCode")?,
        output_object: optional_object_pointer_field(object, "outputObject")?,
        audit_object: optional_object_pointer_field(object, "auditObject")?,
        created_at: parse_control_timestamp_field(object, "createdAt")?,
        updated_at: parse_control_timestamp_field(object, "updatedAt")?,
        expires_at: parse_control_timestamp_field(object, "expiresAt")?,
    })
}

fn parse_control_timestamp_field(
    object: &BTreeMap<String, Value>,
    field: &'static str,
) -> ControlPlaneResult<ControlPlaneTimestamp> {
    parse_control_timestamp(&string_field(object, field)?)
}

fn parse_control_timestamp(value: &str) -> ControlPlaneResult<ControlPlaneTimestamp> {
    if let Some(tick) = value.strip_prefix('t') {
        return Ok(ControlPlaneTimestamp {
            tick: tick
                .parse::<u64>()
                .map_err(|_| shape_error("timestamp tick is invalid"))?,
        });
    }
    let parsed = OffsetDateTime::parse(value, &Rfc3339)
        .map_err(|_| shape_error("timestamp must be RFC3339 or compact tick format"))?;
    let millis = parsed.unix_timestamp_nanos() / 1_000_000;
    if millis < 0 {
        return Err(shape_error("timestamp is before Unix epoch"));
    }
    Ok(ControlPlaneTimestamp {
        tick: u64::try_from(millis).map_err(|_| shape_error("timestamp is out of range"))?,
    })
}

fn parse_unix_timestamp(value: &str) -> ControlPlaneResult<i64> {
    let timestamp = parse_control_timestamp(value)?;
    i64::try_from(timestamp.tick / 1000).map_err(|_| shape_error("timestamp is out of range"))
}

fn account_session_cache_key(workspace_id: Option<&str>) -> String {
    workspace_id.unwrap_or("").to_string()
}

fn parse_lease_execution_state(state: &str) -> ControlPlaneResult<LeaseExecutionState> {
    match state {
        "active" => Ok(LeaseExecutionState::Active),
        "blocked" => Ok(LeaseExecutionState::Blocked),
        "completed" => Ok(LeaseExecutionState::Completed),
        "expired" => Ok(LeaseExecutionState::Expired),
        "revoked" => Ok(LeaseExecutionState::Revoked),
        _ => Err(shape_error("unknown lease execution state")),
    }
}

fn parse_lease_output_state(state: &str) -> ControlPlaneResult<LeaseOutputState> {
    match state {
        "empty" => Ok(LeaseOutputState::Empty),
        "dirty" => Ok(LeaseOutputState::Dirty),
        "review-ready" => Ok(LeaseOutputState::ReviewReady),
        "accepted" => Ok(LeaseOutputState::Accepted),
        "discarded" => Ok(LeaseOutputState::Discarded),
        "conflicted" => Ok(LeaseOutputState::Conflicted),
        "retained" => Ok(LeaseOutputState::Retained),
        _ => Err(shape_error("unknown lease output state")),
    }
}

fn parse_lease_write_target_mode(state: &str) -> ControlPlaneResult<LeaseWriteTargetMode> {
    match state {
        "direct" => Ok(LeaseWriteTargetMode::Direct),
        "work-view" => Ok(LeaseWriteTargetMode::WorkView),
        _ => Err(shape_error("unknown lease write target mode")),
    }
}

fn optional_object_pointer_field(
    object: &BTreeMap<String, Value>,
    field: &'static str,
) -> ControlPlaneResult<Option<ObjectPointer>> {
    match object.get(field) {
        Some(Value::Null) | None => Ok(None),
        Some(value) => parse_object_pointer(value).map(Some),
    }
}

fn parse_work_view_lifecycle(state: &str) -> ControlPlaneResult<WorkViewLifecycleState> {
    match state {
        "active" => Ok(WorkViewLifecycleState::Active),
        "review-ready" => Ok(WorkViewLifecycleState::ReviewReady),
        "accepted" => Ok(WorkViewLifecycleState::Accepted),
        "discarded" => Ok(WorkViewLifecycleState::Discarded),
        "expired" => Ok(WorkViewLifecycleState::Expired),
        "archived" => Ok(WorkViewLifecycleState::Archived),
        _ => Err(shape_error("unknown work view lifecycle state")),
    }
}

fn required_control_field<'a>(
    object: &'a BTreeMap<String, Value>,
    field: &'static str,
) -> ControlPlaneResult<&'a Value> {
    object
        .get(field)
        .ok_or_else(|| shape_error("expected Convex object field"))
}

fn parse_storage_object_kind(kind: &str) -> ControlPlaneResult<StorageObjectKind> {
    match kind {
        "source-pack" => Ok(StorageObjectKind::SourcePack),
        "index-pack" => Ok(StorageObjectKind::IndexPack),
        "locator-index" => Ok(StorageObjectKind::LocatorIndex),
        "snapshot-manifest" => Ok(StorageObjectKind::SnapshotManifest),
        "overlay-pack" | "agent-overlay" => Ok(StorageObjectKind::AgentOverlay),
        _ => Err(shape_error("unknown storage object kind")),
    }
}

fn parse_retention_state(state: &str) -> ControlPlaneResult<RetentionState> {
    match state {
        "pending" => Ok(RetentionState::Pending),
        "current" => Ok(RetentionState::Current),
        "orphan-candidate" => Ok(RetentionState::OrphanCandidate),
        "retained" => Ok(RetentionState::Retained),
        "delete-eligible" => Ok(RetentionState::DeleteEligible),
        _ => Err(shape_error("unknown retention state")),
    }
}

fn object_pointer_value(pointer: &ObjectPointer) -> Value {
    Value::Object(args([
        ("byteLength", number_value(pointer.byte_len)),
        ("contentId", Value::from(pointer.content_id.clone())),
        ("hash", Value::from(pointer.hash.clone())),
        ("keyEpoch", number_value(u64::from(pointer.key_epoch))),
        ("kind", Value::from(pointer.kind.as_str())),
        ("objectKey", Value::from(pointer.object_key.clone())),
    ]))
}

fn object_pointer_array(pointers: &[ObjectPointer]) -> Value {
    Value::Array(pointers.iter().map(object_pointer_value).collect())
}

fn status_event_watermarks_value(watermarks: &StatusEventWatermarks) -> Value {
    let mut object = ConvexArgs::new();
    if let Some(last_event_id) = watermarks.last_event_id.as_ref() {
        object.insert(
            "lastEventId".to_string(),
            Value::from(last_event_id.clone()),
        );
    }
    if let Some(last_scan_at) = watermarks.last_scan_at.as_ref() {
        object.insert("lastScanAt".to_string(), Value::from(last_scan_at.clone()));
    }
    if let Some(sync_state) = watermarks.sync_state.as_ref() {
        object.insert("syncState".to_string(), Value::from(sync_state.clone()));
    }
    if let Some(watcher_state) = watermarks.watcher_state.as_ref() {
        object.insert(
            "watcherState".to_string(),
            Value::from(watcher_state.clone()),
        );
    }
    if let Some(network_state) = watermarks.network_state.as_ref() {
        object.insert(
            "networkState".to_string(),
            Value::from(network_state.clone()),
        );
    }
    Value::Object(object)
}

fn status_sync_queue_value(queue: &StatusSyncQueueSnapshot) -> Value {
    Value::Object(args([
        ("attention", number_value(queue.attention)),
        ("blockedOffline", number_value(queue.blocked_offline)),
        ("claimed", number_value(queue.claimed)),
        ("completed", number_value(queue.completed)),
        ("queued", number_value(queue.queued)),
        ("waitingRetry", number_value(queue.waiting_retry)),
    ]))
}

fn status_index_value(index: &StatusIndexSnapshot) -> Value {
    Value::Object(args([
        ("fileCount", number_value(index.file_count)),
        ("pathCount", number_value(index.path_count)),
        ("state", Value::from(index.state.clone())),
        ("summary", Value::from(index.summary.clone())),
    ]))
}

fn status_workspace_summary_value(summary: &StatusWorkspaceSummarySnapshot) -> Value {
    let mut object = ConvexArgs::new();
    if let Some(env_file_count) = summary.env_file_count {
        object.insert("envFileCount".to_string(), number_value(env_file_count));
    }
    if let Some(repo_count) = summary.repo_count {
        object.insert("repoCount".to_string(), number_value(repo_count));
    }
    if let Some(total_projects) = summary.total_projects {
        object.insert("totalProjects".to_string(), number_value(total_projects));
    }
    Value::Object(object)
}

fn status_item_value(item: &StatusItemSnapshot) -> Value {
    let mut object = ConvexArgs::new();
    object.insert("kind".to_string(), Value::from(item.kind.clone()));
    object.insert("summary".to_string(), Value::from(item.summary.clone()));
    if let Some(path) = item.path.as_ref() {
        object.insert("path".to_string(), Value::from(path.clone()));
    }
    if let Some(event_name) = item.event_name.as_ref() {
        object.insert("eventName".to_string(), Value::from(event_name.clone()));
    }
    Value::Object(object)
}

fn status_limit_value(limit: &StatusLimitSnapshot) -> Value {
    let mut object = ConvexArgs::new();
    object.insert(
        "capability".to_string(),
        Value::from(limit.capability.clone()),
    );
    object.insert(
        "unavailableBecause".to_string(),
        Value::from(limit.unavailable_because.clone()),
    );
    object.insert(
        "stillWorks".to_string(),
        Value::Array(limit.still_works.iter().cloned().map(Value::from).collect()),
    );
    if let Some(path) = limit.path.as_ref() {
        object.insert("path".to_string(), Value::from(path.clone()));
    }
    Value::Object(object)
}

fn conflict_publish_proof_subject(input: &ConflictMetadataPublish) -> String {
    format!(
        "conflictId={}\nconflictKind={}\npaths={}\nbaseSnapshotId={}\nremoteSnapshotId={}\ncontainsSecrets={}",
        input.conflict_id,
        input.conflict_kind,
        input.paths.join(","),
        input.base_snapshot_id,
        input.remote_snapshot_id,
        input.contains_secrets
    )
}

fn conflict_resolution_proof_subject(input: &ConflictResolutionMark) -> String {
    format!(
        "conflictId={}\nresolution={}",
        input.conflict_id,
        input.resolution.as_str()
    )
}

fn workspace_ref_proof_subject(expected_version: u64, next_snapshot_id: &str) -> String {
    format!("expectedVersion={expected_version}\nnextSnapshotId={next_snapshot_id}")
}

fn upload_intent_proof_subject(
    object_key: &str,
    kind: ObjectKind,
    byte_len: u64,
    content_id: Option<&str>,
) -> String {
    format!(
        "objectKey={object_key}\nkind={}\nbyteLength={byte_len}\ncontentId={}",
        kind.as_str(),
        content_id.unwrap_or_default()
    )
}

fn download_intent_proof_subject(
    object_key: &str,
    range: Option<bowline_storage::ByteRange>,
) -> String {
    match range {
        Some(range) => format!(
            "objectKey={object_key}\nrange=bounded\noffset={}\nlength={}",
            range.offset, range.length
        ),
        None => format!("objectKey={object_key}\nrange=full\noffset=\nlength="),
    }
}

fn upload_verification_proof_subject(
    object_key: &str,
    byte_len: u64,
    content_id: Option<&str>,
) -> String {
    format!(
        "objectKey={object_key}\nbyteLength={byte_len}\ncontentId={}",
        content_id.unwrap_or_default()
    )
}

fn object_retention_proof_subject(object_key: &str, retention_state: RetentionState) -> String {
    format!(
        "objectKey={object_key}\nretentionState={}",
        retention_state_value(retention_state)
    )
}

fn delete_intent_proof_subject(request: &DeleteIntentRequest) -> String {
    format!(
        "objectKey={}\nkind={}\nkeyEpoch={}\nretentionState=delete-eligible",
        request.object_key,
        request
            .object_kind
            .map(ObjectKind::as_str)
            .unwrap_or_default(),
        request
            .key_epoch
            .map(|key_epoch| key_epoch.to_string())
            .unwrap_or_default()
    )
}

fn retention_state_value(state: RetentionState) -> &'static str {
    match state {
        RetentionState::Pending => "pending",
        RetentionState::Current => "current",
        RetentionState::OrphanCandidate => "orphan-candidate",
        RetentionState::Retained => "retained",
        RetentionState::DeleteEligible => "delete-eligible",
    }
}

fn object_manifest_proof_subject(commit: &ObjectManifestCommit) -> String {
    let pack_objects = commit
        .pack_objects
        .iter()
        .map(object_pointer_proof_subject)
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "snapshotId={}\nmanifestId={}\nmanifestObject={}\npackObjects={pack_objects}",
        commit.snapshot_id,
        commit.manifest_id,
        object_pointer_proof_subject(&commit.manifest_object),
    )
}

fn object_metadata_proof_subject(pointer: &ObjectPointer) -> String {
    format!("object={}", object_pointer_proof_subject(pointer))
}

fn snapshot_manifest_pointer_proof_subject(snapshot_id: &str) -> String {
    format!("snapshotId={snapshot_id}")
}

fn work_view_create_proof_subject(input: &WorkViewCreate) -> String {
    format!(
        "workViewId={}\nprojectId={}\nname={}\nvisiblePath={}\nbaseSnapshotId={}\nbaseWorkspaceVersion={}",
        input.work_view_id,
        input.project_id,
        input.name,
        input.visible_path,
        input.base_snapshot_id,
        input.base_workspace_version
    )
}

fn work_view_lifecycle_proof_subject(input: &WorkViewLifecycleUpdate) -> String {
    format!(
        "workViewId={}\nlifecycle={}",
        input.work_view_id,
        input.lifecycle.as_str()
    )
}

fn work_view_overlay_proof_subject(input: &WorkViewOverlayCommit) -> String {
    format!(
        "workViewId={}\nexpectedOverlayVersion={}\noverlayObject={}",
        input.work_view_id,
        input.expected_overlay_version,
        object_pointer_proof_subject(&input.overlay_object)
    )
}

fn lease_create_proof_subject(input: &LeaseCreate) -> String {
    [
        format!("leaseId={}", input.lease_id),
        format!("projectId={}", input.project_id),
        format!("writeTargetMode={}", input.write_target_mode.as_str()),
        format!("workViewId={}", input.work_view_id.as_deref().unwrap_or("")),
        format!("baseSnapshotId={}", input.base_snapshot_id),
        format!("executionState={}", input.execution_state.as_str()),
        format!("outputState={}", input.output_state.as_str()),
        format!("statusCode={}", input.status_code),
        format!("expiresAt={}", input.expires_at),
        lease_pointer_proof_subject("outputObject", input.output_object.as_ref()),
        lease_pointer_proof_subject("auditObject", input.audit_object.as_ref()),
    ]
    .join("\n")
}

fn lease_update_proof_subject(input: &LeaseUpdate) -> String {
    [
        format!("leaseId={}", input.lease_id),
        format!("expectedVersion={}", input.expected_version),
        format!(
            "eventKind={}",
            input.event_kind.map(CompactEventKind::as_str).unwrap_or("")
        ),
        format!(
            "executionState={}",
            input
                .execution_state
                .map(LeaseExecutionState::as_str)
                .unwrap_or("")
        ),
        format!(
            "outputState={}",
            input
                .output_state
                .map(LeaseOutputState::as_str)
                .unwrap_or("")
        ),
        format!("statusCode={}", input.status_code.as_deref().unwrap_or("")),
        lease_pointer_proof_subject("outputObject", input.output_object.as_ref()),
        lease_pointer_proof_subject("auditObject", input.audit_object.as_ref()),
    ]
    .join("\n")
}

fn lease_pointer_proof_subject(label: &str, pointer: Option<&ObjectPointer>) -> String {
    let Some(pointer) = pointer else {
        return format!("{label}=");
    };
    [
        format!("{label}.kind={}", pointer.kind.as_str()),
        format!("{label}.objectKey={}", pointer.object_key),
        format!("{label}.contentId={}", pointer.content_id),
        format!("{label}.hash={}", pointer.hash),
        format!("{label}.byteLength={}", pointer.byte_len),
        format!("{label}.keyEpoch={}", pointer.key_epoch),
    ]
    .join("\n")
}

fn bootstrap_session_proof_subject(
    input: &BootstrapSessionInput,
    bootstrap_token_hash: &str,
) -> String {
    [
        format!("workspaceId={}", input.workspace_id),
        format!("host={}", input.host.as_deref().unwrap_or_default()),
        format!("root={}", input.root.as_deref().unwrap_or_default()),
        format!("expiresInTicks={}", input.expires_in_ticks),
        format!("bootstrapTokenHash={bootstrap_token_hash}"),
    ]
    .join("\n")
}

fn sha256_token_hash(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("sha256:{digest:x}")
}

fn object_pointer_proof_subject(pointer: &ObjectPointer) -> String {
    format!(
        "{}:{}:{}:{}:{}",
        pointer.object_key,
        pointer.kind.as_str(),
        pointer.byte_len,
        pointer.hash,
        pointer.key_epoch
    )
}

fn number_value(value: u64) -> Value {
    Value::Float64(value as f64)
}

fn generated_object_key(kind: ObjectKind, seed: &str) -> String {
    let suffix = blake3::hash(seed.as_bytes()).to_hex()[..16].to_string();
    match kind {
        ObjectKind::SourcePack => format!("packs_pk_{suffix}"),
        ObjectKind::IndexPack | ObjectKind::LocatorIndex => format!("indexes_ix_{suffix}"),
        ObjectKind::SnapshotManifest => format!("manifests_mf_{suffix}"),
        ObjectKind::AgentOverlay => format!("packs_pk_{suffix}"),
    }
}

fn generate_bootstrap_token() -> ControlPlaneResult<String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| ControlPlaneError::Storage(error.to_string()))?;
    Ok(format!("bowline_bootstrap_{}", BASE64_URL.encode(bytes)))
}

fn current_timestamp() -> ControlPlaneTimestamp {
    let tick = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default();
    ControlPlaneTimestamp { tick }
}

fn shape_error(message: impl Into<String>) -> ControlPlaneError {
    ControlPlaneError::Storage(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_object_keys_preserve_shape_and_change_with_seed() {
        let first = generated_object_key(ObjectKind::SourcePack, "workspace:device:1");
        let second = generated_object_key(ObjectKind::SourcePack, "workspace:device:2");
        let manifest = generated_object_key(ObjectKind::SnapshotManifest, "workspace:device:1");
        let overlay = generated_object_key(ObjectKind::AgentOverlay, "workspace:device:1");

        assert_ne!(first, second);
        assert!(first.starts_with("packs_pk_"));
        assert!(manifest.starts_with("manifests_mf_"));
        assert!(overlay.starts_with("packs_pk_"));
        assert!(StorageObjectKey::new(first).is_ok());
        assert!(StorageObjectKey::new(second).is_ok());
        assert!(StorageObjectKey::new(manifest).is_ok());
        assert!(StorageObjectKey::new(overlay).is_ok());
    }

    #[test]
    fn status_snapshot_value_builders_use_convex_arg_names() {
        let watermarks = StatusEventWatermarks {
            last_event_id: Some("evt_42".to_string()),
            last_scan_at: Some("2026-06-29T12:00:00Z".to_string()),
            sync_state: Some("ready".to_string()),
            watcher_state: Some("degraded".to_string()),
            network_state: None,
        };
        let Value::Object(watermark_object) = status_event_watermarks_value(&watermarks) else {
            panic!("watermarks must serialize to a Convex object");
        };
        assert_eq!(
            watermark_object.get("lastEventId"),
            Some(&Value::from("evt_42"))
        );
        assert_eq!(
            watermark_object.get("syncState"),
            Some(&Value::from("ready"))
        );
        assert_eq!(
            watermark_object.get("watcherState"),
            Some(&Value::from("degraded"))
        );
        // Absent optional fields must be omitted entirely (Convex v.optional).
        assert!(!watermark_object.contains_key("networkState"));

        let queue = StatusSyncQueueSnapshot {
            queued: 1,
            claimed: 2,
            waiting_retry: 3,
            blocked_offline: 4,
            attention: 5,
            completed: 6,
        };
        let Value::Object(queue_object) = status_sync_queue_value(&queue) else {
            panic!("sync queue must serialize to a Convex object");
        };
        assert_eq!(queue_object.get("queued"), Some(&number_value(1)));
        assert_eq!(queue_object.get("waitingRetry"), Some(&number_value(3)));
        assert_eq!(queue_object.get("blockedOffline"), Some(&number_value(4)));

        let limit = StatusLimitSnapshot {
            capability: "search".to_string(),
            unavailable_because: "index degraded".to_string(),
            path: None,
            still_works: vec!["status".to_string()],
        };
        let Value::Object(limit_object) = status_limit_value(&limit) else {
            panic!("limit must serialize to a Convex object");
        };
        assert_eq!(
            limit_object.get("unavailableBecause"),
            Some(&Value::from("index degraded"))
        );
        assert!(!limit_object.contains_key("path"));
    }

    #[test]
    fn hosted_parser_accepts_phase_10_lease_event_kinds() {
        for kind in [
            CompactEventKind::LeaseBlocked,
            CompactEventKind::LeaseCleanupCompleted,
            CompactEventKind::LeaseCompleted,
            CompactEventKind::LeaseCreated,
            CompactEventKind::LeaseExpired,
            CompactEventKind::LeaseHydrationRequested,
            CompactEventKind::LeaseRevoked,
            CompactEventKind::LeaseReviewReady,
            CompactEventKind::LeaseToolDenied,
            CompactEventKind::LeaseToolInvoked,
            CompactEventKind::LeaseUpdated,
            CompactEventKind::OverlayChanged,
            CompactEventKind::PublishRequested,
        ] {
            assert_eq!(parse_event_kind(kind.as_str()).expect("event kind"), kind);
        }
    }

    #[test]
    fn bootstrap_session_proof_subject_binds_bootstrap_token_hash() {
        let input = BootstrapSessionInput {
            workspace_id: "workspace_1".to_string(),
            host: Some("mac-mini".to_string()),
            root: Some("/workspace/Code".to_string()),
            expires_in_ticks: 900,
        };

        assert_eq!(
            bootstrap_session_proof_subject(&input, "sha256:token_hash_1"),
            [
                "workspaceId=workspace_1",
                "host=mac-mini",
                "root=/workspace/Code",
                "expiresInTicks=900",
                "bootstrapTokenHash=sha256:token_hash_1",
            ]
            .join("\n")
        );
    }

    #[test]
    fn hosted_lease_parser_preserves_returned_timestamps() {
        let lease = parse_lease(&Value::Object(args([
            ("baseSnapshotId", Value::from("snap_1")),
            ("createdAt", Value::from("2026-06-25T12:00:00Z")),
            ("deviceId", Value::from("device_1")),
            ("executionState", Value::from("active")),
            ("expiresAt", Value::from("t000000003600")),
            ("leaseId", Value::from("lease_1")),
            ("outputState", Value::from("empty")),
            ("projectId", Value::from("project_1")),
            ("statusCode", Value::from("active")),
            ("updatedAt", Value::from("2026-06-25T12:00:01Z")),
            ("version", number_value(2)),
            ("writeTargetMode", Value::from("work-view")),
            ("workViewId", Value::from("work_1")),
            ("workspaceId", Value::from("workspace_1")),
        ])))
        .expect("lease parses");

        assert_eq!(lease.created_at.tick, 1_782_388_800_000);
        assert_eq!(lease.updated_at.tick, 1_782_388_801_000);
        assert_eq!(lease.expires_at.tick, 3_600);
    }

    #[test]
    fn account_session_cache_reuses_unexpired_session() {
        let client = HostedControlPlaneClient::try_new_with_token(
            "https://example.convex.cloud",
            "test-control-plane-token",
        )
        .expect("client");
        client.account_session_cache.lock().expect("cache").insert(
            account_session_cache_key(Some("workspace_1")),
            CachedAccountSession {
                session_id: "session_cached".to_string(),
                expires_at_unix: OffsetDateTime::now_utc().unix_timestamp() + 600,
            },
        );

        assert_eq!(
            client.cached_account_session_id(&account_session_cache_key(Some("workspace_1"))),
            Some("session_cached".to_string())
        );
    }

    #[test]
    fn account_session_cache_ignores_expired_session() {
        let client = HostedControlPlaneClient::try_new_with_token(
            "https://example.convex.cloud",
            "test-control-plane-token",
        )
        .expect("client");
        client.account_session_cache.lock().expect("cache").insert(
            account_session_cache_key(Some("workspace_1")),
            CachedAccountSession {
                session_id: "session_expired".to_string(),
                expires_at_unix: OffsetDateTime::now_utc().unix_timestamp() + 10,
            },
        );

        assert_eq!(
            client.cached_account_session_id(&account_session_cache_key(Some("workspace_1"))),
            None
        );
    }

    #[test]
    fn hosted_function_call_counts_are_process_local_and_low_cardinality() {
        reset_hosted_function_call_counts();

        record_hosted_function_call("refs:getWorkspaceRef");
        record_hosted_function_call("refs:getWorkspaceRef");
        record_hosted_function_call("objects:createDownloadIntent");

        assert_eq!(
            hosted_function_call_counts(),
            vec![
                HostedFunctionCallCount {
                    function_name: "objects:createDownloadIntent".to_string(),
                    call_count: 1,
                },
                HostedFunctionCallCount {
                    function_name: "refs:getWorkspaceRef".to_string(),
                    call_count: 2,
                },
            ]
        );
    }
}
