use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as BASE64_URL};
use bowline_core::ids::{
    AccountId, BootstrapSessionId, DeviceApprovalRequestId, DeviceId, EncryptedDeviceGrantId,
    EventId, ProjectId, RecoveryEnvelopeId, SnapshotId, WorkspaceId,
};
use bowline_core::status::StatusFact;
use bowline_storage::{
    ObjectKey as StorageObjectKey, ObjectKind as StorageObjectKind, ObjectMetadata, RetentionState,
};
use convex::{
    ConvexClient, ConvexClientBuilder, ConvexError, FunctionResult, Value, WebSocketState,
};
use futures::{StreamExt, future::Either};
use sha2::{Digest, Sha256};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::sync::Mutex as TokioMutex;

use crate::{
    AuthorizedDeviceRecord, BootstrapSession, BootstrapSessionInput,
    CURRENT_SNAPSHOT_AUTHORITY_FORMAT_VERSION, Capability, CapabilityReporting, CompactEvent,
    CompactEventKind, CompareAndSwapError, ControlPlaneError, ControlPlaneResult,
    ControlPlaneTimestamp, DeleteIntent, DeviceApproval, DeviceApprovalInput,
    DeviceApprovalRequestList, DeviceDenial, DeviceDenialInput, DeviceRequest, DeviceRequestInput,
    DeviceRequestState, DeviceRevocationInput, DownloadIntent, DownloadIntentRequest,
    FirstAuthorizedDeviceInput, GrantAcceptanceInput, ObjectKind, ObjectMetadataCommit,
    ObjectPointer, ObjectRetentionStateUpdate, RecoveryDeviceAuthorizationInput,
    RecoveryEnvelopeInput, RecoveryEnvelopeRecord, RecoveryEnvelopeState, RejectionCode,
    RevokedDeviceRecord, SignedUrlIntent, StaleWorkspaceRef, StatusEventWatermarks,
    StatusItemSnapshot, StatusLimitSnapshot, StatusSyncQueueSnapshot,
    StatusWorkspaceSummarySnapshot, UploadIntent, UploadIntentRequest,
    UploadVerificationIntentRequest, WorkspaceRef, WorkspaceRefHistoryRecord,
    WorkspaceStatusSnapshot,
};

pub(crate) mod contracts;
mod dashboard;
mod devices;
mod generated;
use generated::{AuthRegisterAccountSession, HostedAuthRegisterAccountSessionRequest};
mod objects;
mod parse;
mod proof;
mod recovery;
mod rpc;
mod sync;
mod wire_validation;

pub use dashboard::*;

use contracts::{HostedContractFailure, HostedEndpoint};
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
type DeviceProofVerifierResolver =
    Arc<dyn Fn(&str, &str) -> ControlPlaneResult<Option<String>> + Send + Sync>;
#[cfg(test)]
type RpcOverride =
    Arc<dyn Fn(ConvexRpcKind, &str, ConvexArgs) -> ControlPlaneResult<Value> + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConvexRpcKind {
    Query,
    Mutation,
    Action,
}

pub struct WorkspaceRefStreamShutdown(Option<tokio::sync::oneshot::Sender<()>>);

pub struct WorkspaceRefStreamCancellation(tokio::sync::oneshot::Receiver<()>);

/// Connection lifecycle emitted by the Convex websocket that owns a workspace
/// ref subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceRefStreamConnectionState {
    Connecting,
    Connected,
}

/// Ordered output from one workspace-ref subscription. Connection lifecycle and
/// values share one channel so consumers cannot attribute a value to the wrong
/// websocket generation.
#[derive(Debug)]
pub enum WorkspaceRefStreamEvent {
    ConnectionState(WorkspaceRefStreamConnectionState),
    Ref(ControlPlaneResult<Option<WorkspaceRef>>),
}

enum WorkspaceRefStreamOutput {
    Values(std::sync::mpsc::Sender<ControlPlaneResult<Option<WorkspaceRef>>>),
    Events(std::sync::mpsc::Sender<WorkspaceRefStreamEvent>),
}

impl WorkspaceRefStreamOutput {
    fn send_state(&self, state: WorkspaceRefStreamConnectionState) -> bool {
        match self {
            Self::Values(_) => true,
            Self::Events(sender) => sender
                .send(WorkspaceRefStreamEvent::ConnectionState(state))
                .is_ok(),
        }
    }

    fn send_ref(&self, value: ControlPlaneResult<Option<WorkspaceRef>>) -> bool {
        match self {
            Self::Values(sender) => sender.send(value).is_ok(),
            Self::Events(sender) => sender.send(WorkspaceRefStreamEvent::Ref(value)).is_ok(),
        }
    }
}

pub fn workspace_ref_stream_shutdown_pair()
-> (WorkspaceRefStreamShutdown, WorkspaceRefStreamCancellation) {
    let (shutdown, cancellation) = tokio::sync::oneshot::channel();
    (
        WorkspaceRefStreamShutdown(Some(shutdown)),
        WorkspaceRefStreamCancellation(cancellation),
    )
}

impl Drop for WorkspaceRefStreamShutdown {
    fn drop(&mut self) {
        if let Some(shutdown) = self.0.take() {
            let _already_stopped = shutdown.send(());
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostedFunctionCallCount {
    pub function_name: String,
    pub call_count: u64,
}

#[derive(Debug, Clone)]
struct CachedAccountSession {
    session_id: String,
    revocation_token: String,
    expires_at_unix: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredAccountSession {
    pub session_id: String,
    pub revocation_token: String,
}

pub struct HostedControlPlaneClient {
    control_plane_token: String,
    deployment_url: String,
    device_id: String,
    device_proof_signer: Option<DeviceProofSigner>,
    device_proof_verifier_resolver: Option<DeviceProofVerifierResolver>,
    runtime: tokio::runtime::Runtime,
    bootstrap_token: Option<String>,
    workos_access_token: Option<String>,
    account_session_id: Option<String>,
    account_session_cache: Mutex<BTreeMap<String, CachedAccountSession>>,
    rpc_client: TokioMutex<Option<ConvexClient>>,
    #[cfg(test)]
    rpc_override: Option<RpcOverride>,
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

impl CapabilityReporting for HostedControlPlaneClient {
    fn capabilities(&self) -> BTreeSet<Capability> {
        hosted_supported_capabilities().iter().copied().collect()
    }
}

fn hosted_supported_capabilities() -> &'static [Capability] {
    &[
        Capability::WorkspaceRefHistory,
        Capability::StorageGc,
        Capability::ObjectMetadata,
        Capability::DeviceBootstrap,
        Capability::DeviceTrust,
        Capability::RecoveryKey,
    ]
}

impl HostedControlPlaneClient {
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
            device_proof_verifier_resolver: None,
            runtime,
            bootstrap_token: None,
            workos_access_token: None,
            account_session_id: None,
            account_session_cache: Mutex::new(BTreeMap::new()),
            rpc_client: TokioMutex::new(None),
            #[cfg(test)]
            rpc_override: None,
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

    pub fn with_device_proof_verifier_resolver<F>(mut self, resolver: F) -> Self
    where
        F: Fn(&str, &str) -> ControlPlaneResult<Option<String>> + Send + Sync + 'static,
    {
        self.device_proof_verifier_resolver = Some(Arc::new(resolver));
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

    #[cfg(test)]
    fn with_public_action_override<F>(mut self, action: F) -> Self
    where
        F: Fn(&str, ConvexArgs) -> ControlPlaneResult<Value> + Send + Sync + 'static,
    {
        self.rpc_override = Some(Arc::new(move |kind, name, args| {
            debug_assert_eq!(kind, ConvexRpcKind::Action);
            action(name, args)
        }));
        self
    }

    #[cfg(test)]
    fn with_rpc_override<F>(mut self, rpc: F) -> Self
    where
        F: Fn(ConvexRpcKind, &str, ConvexArgs) -> ControlPlaneResult<Value> + Send + Sync + 'static,
    {
        self.rpc_override = Some(Arc::new(rpc));
        self
    }

    pub fn register_account_session(
        &self,
        access_token: impl Into<String>,
        workspace_id: Option<&str>,
    ) -> ControlPlaneResult<RegisteredAccountSession> {
        self.register_account_session_for_token(access_token.into(), workspace_id)
    }

    pub fn revoke_account_session(
        &self,
        session_id: &str,
        revocation_token: &str,
    ) -> ControlPlaneResult<()> {
        self.rpc(
            ConvexRpcKind::Action,
            "auth:revokeAccountSession",
            BTreeMap::from([
                (
                    "revocationToken".to_string(),
                    Value::from(revocation_token.to_string()),
                ),
                ("sessionId".to_string(), Value::from(session_id.to_string())),
            ]),
        )?;
        Ok(())
    }

    pub fn deployment_url(&self) -> &str {
        &self.deployment_url
    }

    pub fn stream_workspace_ref_updates(
        &self,
        workspace_id: &str,
        sender: std::sync::mpsc::Sender<ControlPlaneResult<Option<WorkspaceRef>>>,
    ) -> ControlPlaneResult<()> {
        let (_keepalive, shutdown) = workspace_ref_stream_shutdown_pair();
        self.stream_workspace_ref_updates_until(workspace_id, sender, shutdown)
    }

    pub fn stream_workspace_ref_updates_until(
        &self,
        workspace_id: &str,
        sender: std::sync::mpsc::Sender<ControlPlaneResult<Option<WorkspaceRef>>>,
        shutdown: WorkspaceRefStreamCancellation,
    ) -> ControlPlaneResult<()> {
        self.stream_workspace_ref_output_until(
            workspace_id,
            WorkspaceRefStreamOutput::Values(sender),
            shutdown,
        )
    }

    pub fn stream_workspace_ref_events_until(
        &self,
        workspace_id: &str,
        sender: std::sync::mpsc::Sender<WorkspaceRefStreamEvent>,
        shutdown: WorkspaceRefStreamCancellation,
    ) -> ControlPlaneResult<()> {
        self.stream_workspace_ref_output_until(
            workspace_id,
            WorkspaceRefStreamOutput::Events(sender),
            shutdown,
        )
    }

    fn stream_workspace_ref_output_until(
        &self,
        workspace_id: &str,
        output: WorkspaceRefStreamOutput,
        shutdown: WorkspaceRefStreamCancellation,
    ) -> ControlPlaneResult<()> {
        let deployment_url = self.deployment_url.clone();
        let device_proof_verifier_resolver = self.device_proof_verifier_resolver.clone();
        // The live subscription shares the typed refs:getWorkspaceRef contract:
        // the request is encoded through the endpoint marker and each pushed
        // value is decoded and head-signature verified by the same DTO boundary
        // as the one-shot query path.
        let mut request = generated::HostedRefsGetWorkspaceRefRequest {
            workspace_id: workspace_id.to_string(),
            auth_token: None,
            account_session_id: None,
        };
        if self.account_session_auth_available() {
            request.account_session_id =
                Some(self.verified_account_session_id(Some(workspace_id))?);
        } else {
            request.auth_token = Some(self.control_plane_token.clone());
        }
        let request_args = encode_hosted_request::<generated::RefsGetWorkspaceRef>(&request)?;
        let function_name = generated::RefsGetWorkspaceRef::CONVEX_FUNCTION;
        self.runtime.block_on(async move {
            let mut shutdown = Box::pin(shutdown.0);
            let (websocket_state_tx, mut websocket_state_rx) = tokio::sync::mpsc::channel(8);
            let Some(client) = until_workspace_ref_stream_shutdown(
                &mut shutdown,
                ConvexClientBuilder::new(&deployment_url)
                    .with_on_state_change(websocket_state_tx)
                    .build(),
            )
            .await
            else {
                return Ok(());
            };
            let mut client = client.map_err(map_convex_error)?;
            let Some(subscription) = until_workspace_ref_stream_shutdown(
                &mut shutdown,
                client.subscribe(function_name, request_args),
            )
            .await
            else {
                return Ok(());
            };
            let mut subscription = subscription.map_err(map_convex_error)?;
            let mut websocket_state_open = true;
            loop {
                tokio::select! {
                    biased;
                    _ = &mut shutdown => break,
                    state = websocket_state_rx.recv(), if websocket_state_open => {
                        let Some(state) = state else {
                            websocket_state_open = false;
                            continue;
                        };
                        let state = match state {
                            WebSocketState::Connected => WorkspaceRefStreamConnectionState::Connected,
                            WebSocketState::Connecting => WorkspaceRefStreamConnectionState::Connecting,
                        };
                        if !output.send_state(state) {
                            break;
                        }
                    }
                    result = subscription.next() => {
                        let Some(result) = result else { break };
                        record_hosted_function_call(function_name);
                        let parsed = unwrap_function_result(result).and_then(|value| {
                            decode_hosted_response::<generated::RefsGetWorkspaceRef>(value).and_then(
                                |maybe_dto| {
                                    maybe_dto
                                        .map(|dto| {
                                            workspace_ref_from_dto(dto, |workspace_id, device_id| {
                                                let Some(resolver) =
                                                    device_proof_verifier_resolver.as_ref()
                                                else {
                                                    return Ok(None);
                                                };
                                                resolver(workspace_id, device_id)
                                            })
                                        })
                                        .transpose()
                                },
                            )
                        });
                        if !output.send_ref(parsed) {
                            break;
                        }
                    }
                }
            }
            Ok(())
        })
    }

    fn rpc(&self, kind: ConvexRpcKind, name: &str, args: ConvexArgs) -> ControlPlaneResult<Value> {
        record_hosted_function_call(name);
        #[cfg(test)]
        if let Some(rpc) = self.rpc_override.as_ref() {
            return rpc(kind, name, args);
        }
        let deployment_url = self.deployment_url.clone();
        let name = name.to_string();
        self.runtime.block_on(rpc_with_cached_client(
            &self.rpc_client,
            matches!(kind, ConvexRpcKind::Query),
            || async {
                ConvexClient::new(&deployment_url)
                    .await
                    .map_err(map_convex_error)
            },
            |mut client| {
                let name = name.clone();
                let args = args.clone();
                Box::pin(async move { call_convex_rpc(&mut client, kind, &name, args).await })
            },
        ))
    }

    fn call<E: HostedEndpoint>(&self, request: &E::Request) -> ControlPlaneResult<E::Response> {
        let args = encode_hosted_request::<E>(request)?;
        let response = self.rpc(E::KIND, E::CONVEX_FUNCTION, args)?;
        decode_hosted_response::<E>(response)
    }

    fn verified_account_session_id(
        &self,
        workspace_id: Option<&str>,
    ) -> ControlPlaneResult<String> {
        if let Some(session_id) = self.account_session_id.as_ref() {
            return Ok(session_id.clone());
        }
        let access_token =
            self.workos_access_token
                .as_ref()
                .ok_or_else(|| ControlPlaneError::Rejected {
                    code: RejectionCode::InvalidRequest,
                    message: "hosted account operations require bowline login".to_string(),
                })?;
        self.register_account_session_for_token(access_token.clone(), workspace_id)
            .map(|registration| registration.session_id)
    }

    fn register_account_session_for_token(
        &self,
        access_token: String,
        workspace_id: Option<&str>,
    ) -> ControlPlaneResult<RegisteredAccountSession> {
        let cache_key = account_session_cache_key(workspace_id);
        let mut cache = self.account_session_cache.lock().map_err(|_| {
            ControlPlaneError::Storage("account session cache lock poisoned".to_string())
        })?;
        if let Some(registration) = cached_account_session_from_cache(&cache, &cache_key) {
            return Ok(registration);
        }
        // Keep check -> action -> insert under one lock so concurrent callers
        // share a single account-session registration for this client.
        let request = HostedAuthRegisterAccountSessionRequest {
            access_token: access_token.clone(),
            workspace_id: workspace_id.map(|id| id.to_string()),
        };
        let response = self.call::<AuthRegisterAccountSession>(&request)?;
        let session_id = response.session_id;
        let revocation_token = response.revocation_token;
        let expires_at_unix = response
            .expires_at
            .as_deref()
            .and_then(|expires_at| parse_unix_timestamp(expires_at).ok())
            .unwrap_or_else(|| {
                OffsetDateTime::now_utc().unix_timestamp() + ACCOUNT_SESSION_FALLBACK_TTL_SECONDS
            });
        cache.insert(
            cache_key,
            CachedAccountSession {
                session_id: session_id.clone(),
                revocation_token: revocation_token.clone(),
                expires_at_unix,
            },
        );
        Ok(RegisteredAccountSession {
            session_id,
            revocation_token,
        })
    }

    fn account_session_auth_available(&self) -> bool {
        self.control_plane_token.is_empty()
            && (self.account_session_id.is_some() || self.workos_access_token.is_some())
    }

    #[cfg(test)]
    fn cached_account_session_id(&self, cache_key: &str) -> Option<String> {
        self.account_session_cache
            .lock()
            .ok()
            .and_then(|cache| cached_account_session_id_from_cache(&cache, cache_key))
    }

    fn generated_object_key(&self, kind: ObjectKind, workspace_id: impl AsRef<str>) -> String {
        let workspace_id = workspace_id.as_ref();
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
        workspace_id: impl AsRef<str>,
        action: &str,
        subject: &str,
    ) -> ControlPlaneResult<String> {
        let signer =
            self.device_proof_signer
                .as_ref()
                .ok_or_else(|| ControlPlaneError::Rejected {
                    code: RejectionCode::DeviceNotTrusted,
                    message: "hosted byte-plane and ref operations require a local device identity"
                        .to_string(),
                })?;
        signer(workspace_id.as_ref(), &self.device_id, action, subject)
    }

    fn require_local_device(&self, device_id: impl AsRef<str>) -> ControlPlaneResult<()> {
        if device_id.as_ref() == self.device_id {
            Ok(())
        } else {
            Err(ControlPlaneError::Rejected {
                code: RejectionCode::DeviceNotTrusted,
                message: "hosted operations must be signed by this client's local device identity"
                    .to_string(),
            })
        }
    }

    fn device_proof_verifier(
        &self,
        workspace_id: impl AsRef<str>,
        device_id: impl AsRef<str>,
    ) -> ControlPlaneResult<Option<String>> {
        let Some(resolver) = self.device_proof_verifier_resolver.as_ref() else {
            return Ok(None);
        };
        resolver(workspace_id.as_ref(), device_id.as_ref())
    }
}

async fn until_workspace_ref_stream_shutdown<T>(
    shutdown: &mut std::pin::Pin<Box<tokio::sync::oneshot::Receiver<()>>>,
    future: impl std::future::Future<Output = T>,
) -> Option<T> {
    match futures::future::select(shutdown.as_mut(), Box::pin(future)).await {
        Either::Left((_shutdown, _pending)) => None,
        Either::Right((output, _shutdown)) => Some(output),
    }
}

#[cfg(test)]
fn cached_account_session_id_from_cache(
    cache: &BTreeMap<String, CachedAccountSession>,
    cache_key: &str,
) -> Option<String> {
    let now = OffsetDateTime::now_utc().unix_timestamp();
    cache
        .get(cache_key)
        .cloned()
        .filter(|cached| cached.expires_at_unix - ACCOUNT_SESSION_EXPIRY_SAFETY_SECONDS > now)
        .map(|cached| cached.session_id)
}

fn cached_account_session_from_cache(
    cache: &BTreeMap<String, CachedAccountSession>,
    cache_key: &str,
) -> Option<RegisteredAccountSession> {
    let now = OffsetDateTime::now_utc().unix_timestamp();
    cache
        .get(cache_key)
        .filter(|cached| cached.expires_at_unix - ACCOUNT_SESSION_EXPIRY_SAFETY_SECONDS > now)
        .map(|cached| RegisteredAccountSession {
            session_id: cached.session_id.clone(),
            revocation_token: cached.revocation_token.clone(),
        })
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
mod proof_contract;
#[cfg(test)]
mod tests;
