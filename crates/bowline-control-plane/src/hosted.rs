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
    ConflictResolutionMark, ControlPlaneError, ControlPlaneResult, ControlPlaneTimestamp,
    DeleteIntent, DeleteIntentRequest, DeviceApproval, DeviceApprovalInput,
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

mod client;
mod devices;
mod leases;
mod objects;
mod parse;
mod proof;
mod recovery;
mod rpc;
mod sync;
mod work_views;

use parse::*;
use proof::*;
use rpc::*;

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

#[cfg(test)]
mod tests;
