use std::{
    collections::BTreeMap,
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as BASE64_URL};
use bowline_core::ids::{
    AccountId, BootstrapSessionId, ContentId, DeviceApprovalRequestId, DeviceId,
    EncryptedDeviceGrantId, EventId, ProjectId, RecoveryEnvelopeId, SnapshotId, WorkspaceId,
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
    CURRENT_SNAPSHOT_AUTHORITY_FORMAT_VERSION, CompactEvent, CompactEventKind, CompareAndSwapError,
    ControlPlaneError, ControlPlaneResult, ControlPlaneTimestamp, DeleteIntent, DeviceApproval,
    DeviceApprovalInput, DeviceApprovalRequestList, DeviceDenial, DeviceDenialInput, DeviceRequest,
    DeviceRequestInput, DeviceRequestState, DeviceRevocationInput, DownloadIntent,
    DownloadIntentRequest, FirstAuthorizedDeviceInput, GrantAcceptanceInput, ObjectKind,
    ObjectMetadataCommit, ObjectPointer, ObjectRetentionStateUpdate,
    RecoveryDeviceAuthorizationInput, RecoveryEnvelopeInput, RecoveryEnvelopeRecord,
    RecoveryEnvelopeState, RejectionCode, Retryability, RevokedDeviceRecord, SignedUrlIntent,
    StaleWorkspaceRef, StatusEventWatermarks, StatusItemSnapshot, StatusLimitSnapshot,
    StatusSyncQueueSnapshot, StatusWorkspaceSummarySnapshot, UploadIntent, UploadIntentRequest,
    UploadVerificationIntentRequest, WireContractFailure, WorkspaceRef, WorkspaceRefHistoryRecord,
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
mod retry;
mod rpc;
mod subscription;
mod sync;
mod wire_validation;
mod workspace_keys;

pub use dashboard::*;
pub use subscription::*;

use contracts::HostedEndpoint;
use parse::*;
use proof::*;
use rpc::*;

const HOSTED_CAPABILITY: &str = "hosted-convex-control-plane";
const DEFAULT_DEVICE_ID: &str = "bowline-hosted-client";
const ENV_CONTROL_PLANE_TOKEN: &str = "BOWLINE_CONTROL_PLANE_TOKEN";
const CONVEX_RPC_TIMEOUT: Duration = Duration::from_secs(20);
/// Names both recoveries, because the two hosts that hit this cannot use the
/// same one: a laptop signs in, an agent host is reprovisioned from a signed-in
/// device because it has no browser to sign in with.
pub const MISSING_ACCOUNT_SESSION_MESSAGE: &str = "this device holds no bowline account session; run `bowline login` here, \
     or re-run `bowline connect <host>` from a signed-in device to reprovision this host";
const ACCOUNT_SESSION_FALLBACK_TTL_SECONDS: i64 = 300;
const ACCOUNT_SESSION_EXPIRY_SAFETY_SECONDS: i64 = 60;
static NEXT_OBJECT_KEY_SEED: AtomicU64 = AtomicU64::new(1);
type DeviceProofSigner =
    Arc<dyn Fn(&WorkspaceId, &DeviceId, &str, &str) -> ControlPlaneResult<String> + Send + Sync>;
type DeviceProofVerifierResolver =
    Arc<dyn Fn(&WorkspaceId, &DeviceId) -> ControlPlaneResult<Option<String>> + Send + Sync>;
/// Yields a currently valid WorkOS access token, refreshing it if the owner
/// knows how. Re-registration is only possible while this answers `Some`, so a
/// long-lived daemon supplies a provider rather than a token captured at build.
type AccountAccessTokenProvider = Arc<dyn Fn() -> Option<String> + Send + Sync>;
/// Receives every account session this client registers, so the owner can
/// persist it and the next process start does not begin with a refused call.
type AccountSessionSink = Arc<dyn Fn(&RegisteredAccountSession) + Send + Sync>;
#[cfg(test)]
type RpcOverride =
    Arc<dyn Fn(ConvexRpcKind, &str, ConvexArgs) -> ControlPlaneResult<Value> + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConvexRpcKind {
    Query,
    Mutation,
    Action,
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
    device_id: DeviceId,
    device_proof_signer: Option<DeviceProofSigner>,
    device_proof_verifier_resolver: Option<DeviceProofVerifierResolver>,
    runtime: tokio::runtime::Runtime,
    bootstrap_token: Option<String>,
    workos_access_token: Option<AccountAccessTokenProvider>,
    /// The session this client was handed at construction, held behind a lock
    /// because the control plane can refuse it mid-run: a persisted session that
    /// expired has to be droppable, or every later call repeats the same refusal
    /// until a human restarts the daemon.
    account_session_id: Mutex<Option<String>>,
    account_session_sink: Option<AccountSessionSink>,
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

impl HostedControlPlaneClient {
    pub fn try_new(deployment_url: impl Into<String>) -> ControlPlaneResult<Self> {
        let control_plane_token =
            std::env::var(ENV_CONTROL_PLANE_TOKEN).map_err(|_| ControlPlaneError::Internal {
                reason: "BOWLINE_CONTROL_PLANE_TOKEN is required",
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
            .map_err(|_| ControlPlaneError::Internal {
                reason: "convex client runtime could not be started",
            })?;

        Ok(Self {
            control_plane_token: control_plane_token.into(),
            deployment_url: deployment_url.into(),
            device_id: DeviceId::new(DEFAULT_DEVICE_ID),
            device_proof_signer: None,
            device_proof_verifier_resolver: None,
            runtime,
            bootstrap_token: None,
            workos_access_token: None,
            account_session_id: Mutex::new(None),
            account_session_sink: None,
            account_session_cache: Mutex::new(BTreeMap::new()),
            rpc_client: TokioMutex::new(None),
            #[cfg(test)]
            rpc_override: None,
        })
    }

    /// Adds the short-lived enrolment credential a not-yet-trusted device is
    /// handed over SSH.
    ///
    /// It is additive, never exclusive: it authenticates only the enrolment
    /// endpoints that have a `*WithBootstrap` form. Account-scoped reads —
    /// `listDeviceTrust` above all, which `accept_device_grant` performs between
    /// fetching its grant and confirming it — have no such form and still need
    /// the account session, so a client that carries one must keep the other.
    pub fn with_bootstrap_token(mut self, bootstrap_token: impl Into<String>) -> Self {
        self.bootstrap_token = Some(bootstrap_token.into());
        self
    }

    pub fn with_device_id(mut self, device_id: DeviceId) -> Self {
        self.device_id = device_id;
        self
    }

    pub fn with_device_proof_signer<F>(mut self, signer: F) -> Self
    where
        F: Fn(&WorkspaceId, &DeviceId, &str, &str) -> ControlPlaneResult<String>
            + Send
            + Sync
            + 'static,
    {
        self.device_proof_signer = Some(Arc::new(signer));
        self
    }

    pub fn with_device_proof_verifier_resolver<F>(mut self, resolver: F) -> Self
    where
        F: Fn(&WorkspaceId, &DeviceId) -> ControlPlaneResult<Option<String>>
            + Send
            + Sync
            + 'static,
    {
        self.device_proof_verifier_resolver = Some(Arc::new(resolver));
        self
    }

    /// A token captured once. Correct for a short-lived CLI invocation; a daemon
    /// that outlives the token's own expiry wants
    /// [`Self::with_workos_access_token_provider`] instead.
    pub fn with_workos_access_token(self, access_token: impl Into<String>) -> Self {
        let access_token = access_token.into();
        self.with_workos_access_token_provider(move || Some(access_token.clone()))
    }

    pub fn with_workos_access_token_provider<F>(mut self, access_token: F) -> Self
    where
        F: Fn() -> Option<String> + Send + Sync + 'static,
    {
        self.workos_access_token = Some(Arc::new(access_token));
        if let Ok(mut cache) = self.account_session_cache.lock() {
            cache.clear();
        }
        self
    }

    pub fn with_account_session_id(self, session_id: impl Into<String>) -> Self {
        if let Ok(mut pinned) = self.account_session_id.lock() {
            *pinned = Some(session_id.into());
        }
        self
    }

    /// Called with every account session this client registers, including the
    /// ones it registers to replace a session the control plane refused.
    ///
    /// The sink runs with no client lock held, so it is free to block and free to
    /// call back into this client.
    pub fn with_account_session_sink<F>(mut self, sink: F) -> Self
    where
        F: Fn(&RegisteredAccountSession) + Send + Sync + 'static,
    {
        self.account_session_sink = Some(Arc::new(sink));
        self
    }

    /// Whether this client holds a credential an account call can authenticate
    /// with — a pinned session, or a WorkOS token it can register one from.
    ///
    /// Every workspace-ref, event, ref-history, and device-trust call carries a
    /// verified account session, so `false` means all of them are refused.
    /// Builders check it so a client that cannot possibly sync is never handed
    /// out as configured.
    pub fn can_authenticate_account_calls(&self) -> bool {
        self.pinned_account_session_id().is_some() || self.workos_access_token().is_some()
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

    fn rpc(
        &self,
        kind: ConvexRpcKind,
        name: &'static str,
        args: ConvexArgs,
    ) -> ControlPlaneResult<Value> {
        #[cfg(test)]
        if let Some(rpc) = self.rpc_override.as_ref() {
            return rpc(kind, name, args);
        }
        let deployment_url = self.deployment_url.clone();
        self.runtime.block_on(rpc_with_cached_client(
            &self.rpc_client,
            name,
            matches!(kind, ConvexRpcKind::Query),
            || async {
                ConvexClient::new(&deployment_url)
                    .await
                    .map_err(map_convex_error)
            },
            |mut client| {
                let args = args.clone();
                Box::pin(async move { call_convex_rpc(&mut client, kind, name, args).await })
            },
        ))
    }

    fn call<E: HostedEndpoint>(&self, request: &E::Request) -> ControlPlaneResult<E::Response> {
        // `encode_hosted_request` already validated this payload against the
        // contract this client was generated from, so it is well-formed by its
        // own lights. Every hosted handler throws a typed `ConvexError`; an
        // unclassified refusal therefore came from the deployment's argument
        // validator, which is the version-skew signal a redacted "Server Error"
        // would otherwise swallow.
        let args = encode_hosted_request::<E>(request)?;
        let response = self
            .rpc(E::KIND, E::CONVEX_FUNCTION, args)
            .map_err(named_contract_skew::<E>)?;
        decode_hosted_response::<E>(response)
    }

    /// Runs one account-session call and, if the control plane refuses the
    /// session, registers a fresh one and runs the call exactly once more.
    ///
    /// Re-issuing a mutation is safe here and only here. `Unauthorized` comes
    /// from the session check every hosted handler runs before it reads or
    /// writes anything, so the refused attempt provably did not apply.
    fn call_reauthenticating<E, F>(
        &self,
        workspace_id: Option<&str>,
        build_request: F,
    ) -> ControlPlaneResult<E::Response>
    where
        E: HostedEndpoint,
        F: Fn() -> ControlPlaneResult<E::Request>,
    {
        let refusal = match self.call::<E>(&build_request()?) {
            Ok(response) => return Ok(response),
            Err(error) => error,
        };
        if refusal.retryability() != Retryability::AuthExpired
            || !self.reregister_account_session(workspace_id)?
        {
            return Err(refusal);
        }
        self.call::<E>(&build_request()?)
    }

    /// Drops the session the control plane refused and registers a replacement.
    /// Answers `false` when this client holds no WorkOS credential to register
    /// with, which is the caller's signal to surface the original refusal rather
    /// than a misleading configuration error.
    fn reregister_account_session(&self, workspace_id: Option<&str>) -> ControlPlaneResult<bool> {
        let Some(access_token) = self.workos_access_token() else {
            return Ok(false);
        };
        self.forget_account_session(workspace_id)?;
        self.register_account_session_for_token(access_token, workspace_id)?;
        Ok(true)
    }

    fn forget_account_session(&self, workspace_id: Option<&str>) -> ControlPlaneResult<()> {
        self.account_session_id
            .lock()
            .map_err(|_| ControlPlaneError::Internal {
                reason: "account session lock poisoned",
            })?
            .take();
        self.account_session_cache
            .lock()
            .map_err(|_| ControlPlaneError::Internal {
                reason: "account session cache lock poisoned",
            })?
            .remove(&account_session_cache_key(workspace_id));
        Ok(())
    }

    fn pinned_account_session_id(&self) -> Option<String> {
        self.account_session_id.lock().ok()?.clone()
    }

    fn workos_access_token(&self) -> Option<String> {
        self.workos_access_token
            .as_ref()
            .and_then(|access_token| access_token())
    }

    fn verified_account_session_id(
        &self,
        workspace_id: Option<&str>,
    ) -> ControlPlaneResult<String> {
        if let Some(session_id) = self.pinned_account_session_id() {
            return Ok(session_id);
        }
        let access_token =
            self.workos_access_token()
                .ok_or_else(|| ControlPlaneError::Rejected {
                    code: RejectionCode::InvalidRequest,
                    message: MISSING_ACCOUNT_SESSION_MESSAGE.to_string(),
                })?;
        self.register_account_session_for_token(access_token, workspace_id)
            .map(|registration| registration.session_id)
    }

    fn register_account_session_for_token(
        &self,
        access_token: String,
        workspace_id: Option<&str>,
    ) -> ControlPlaneResult<RegisteredAccountSession> {
        let cache_key = account_session_cache_key(workspace_id);
        let mut cache =
            self.account_session_cache
                .lock()
                .map_err(|_| ControlPlaneError::Internal {
                    reason: "account session cache lock poisoned",
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
            .as_ref()
            .and_then(|expires_at| parse_unix_timestamp(expires_at.as_str()).ok())
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
        let registration = RegisteredAccountSession {
            session_id,
            revocation_token,
        };
        // The sink runs outside the cache lock, and the registration is cloned
        // rather than borrowed from the cache so it can. It is owner code: it may
        // block on disk or a keychain prompt, and it may re-enter this client —
        // neither of which a non-reentrant lock every other registration queues
        // behind can survive.
        drop(cache);
        if let Some(sink) = self.account_session_sink.as_ref() {
            sink(&registration);
        }
        Ok(registration)
    }

    /// Whether an account credential can be obtained *right now*. The provider is
    /// called rather than merely tested for presence: a long-lived daemon signed
    /// out after construction still holds the provider, and treating that as
    /// available sends an already-trusted device down the account branch — where
    /// it fails — instead of the device-proof branch it could still satisfy.
    fn account_session_auth_available(&self) -> bool {
        self.control_plane_token.is_empty()
            && (self.pinned_account_session_id().is_some() || self.workos_access_token().is_some())
    }

    #[cfg(test)]
    fn cached_account_session_id(&self, cache_key: &str) -> Option<String> {
        self.account_session_cache
            .lock()
            .ok()
            .and_then(|cache| cached_account_session_id_from_cache(&cache, cache_key))
    }

    fn generated_object_key(&self, kind: ObjectKind, workspace_id: &WorkspaceId) -> String {
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
        workspace_id: &WorkspaceId,
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
        signer(workspace_id, &self.device_id, action, subject)
    }

    fn require_local_device(&self, device_id: &DeviceId) -> ControlPlaneResult<()> {
        if device_id == &self.device_id {
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
        workspace_id: &WorkspaceId,
        device_id: &DeviceId,
    ) -> ControlPlaneResult<Option<String>> {
        let Some(resolver) = self.device_proof_verifier_resolver.as_ref() else {
            return Ok(None);
        };
        resolver(workspace_id, device_id)
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

fn named_contract_skew<E: HostedEndpoint>(error: ControlPlaneError) -> ControlPlaneError {
    match error {
        ControlPlaneError::ServerError { message, .. } => ControlPlaneError::ContractSkew {
            endpoint: E::ID,
            function: E::CONVEX_FUNCTION,
            client_wire_schema_digest: bowline_core::wire::WIRE_SCHEMA_HASH,
            detail: message,
        },
        error => error,
    }
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

#[cfg(test)]
mod proof_contract;
#[cfg(test)]
mod tests;
