use super::*;

use crate::daemon::control_plane::{
    HostedSetupError, ResolvedHostedContext, build_hosted_control_plane, resolve_hosted_context,
};

pub(super) const HOSTED_CONTEXT_TRUST_REFRESH_INTERVAL: Duration = Duration::from_secs(300);

#[derive(Clone, PartialEq, Eq)]
struct HostedContextFingerprint(String);

pub(super) struct HostedContext {
    pub(super) client: Arc<HostedControlPlaneClient>,
    pub(super) http: SignedUrlHttpClient,
}

pub(super) type HostedContextResolver =
    Arc<dyn Fn(&SyncArgs) -> Result<Arc<HostedContext>, Box<dyn std::error::Error>> + Send + Sync>;

pub(super) fn hosted_context_resolver(cache: Arc<HostedContextCache>) -> HostedContextResolver {
    Arc::new(move |args| Ok(cache.get_or_build(args)?))
}

pub(in crate::daemon) trait HostedContextFactory: Send + Sync {
    fn resolve(&self, args: &SyncArgs) -> Result<ResolvedHostedContext, HostedSetupError>;
    fn build(
        &self,
        args: &SyncArgs,
        resolved: ResolvedHostedContext,
    ) -> Result<
        (
            Arc<HostedContext>,
            ResolvedHostedContext,
            Vec<DeviceProofVerifier>,
        ),
        HostedSetupError,
    >;
}

struct ProductionHostedContextFactory;

impl HostedContextFactory for ProductionHostedContextFactory {
    fn resolve(&self, args: &SyncArgs) -> Result<ResolvedHostedContext, HostedSetupError> {
        let key_store = key_store()?;
        resolve_hosted_context(&*key_store, &WorkspaceId::new(args.workspace_id.clone()))
    }

    fn build(
        &self,
        args: &SyncArgs,
        resolved: ResolvedHostedContext,
    ) -> Result<
        (
            Arc<HostedContext>,
            ResolvedHostedContext,
            Vec<DeviceProofVerifier>,
        ),
        HostedSetupError,
    > {
        let key_store = key_store()?;
        let built = build_hosted_control_plane(
            &*key_store,
            WorkspaceId::new(args.workspace_id.clone()),
            DeviceId::new(args.device_id.clone()),
            resolved,
        )?;
        let context = Arc::new(HostedContext {
            client: Arc::new(built.client),
            http: SignedUrlByteStore::<HostedControlPlaneClient>::build_http_client(),
        });
        let final_resolved =
            resolve_hosted_context(&*key_store, &WorkspaceId::new(args.workspace_id.clone()))?;
        Ok((context, final_resolved, built.installed_verifiers))
    }
}

struct CachedHostedContext {
    fingerprint: HostedContextFingerprint,
    context: Arc<HostedContext>,
    refresh_at: Instant,
}

pub(super) struct HostedContextCache {
    inner: Mutex<Option<CachedHostedContext>>,
    factory: Arc<dyn HostedContextFactory>,
}

impl HostedContextCache {
    pub(super) fn new() -> Self {
        Self {
            inner: Mutex::new(None),
            factory: Arc::new(ProductionHostedContextFactory),
        }
    }

    pub(super) fn get_or_build(
        &self,
        args: &SyncArgs,
    ) -> Result<Arc<HostedContext>, HostedSetupError> {
        let workspace_id = WorkspaceId::new(args.workspace_id.clone());
        let device_id = DeviceId::new(args.device_id.clone());
        for _ in 0..3 {
            let resolved = self.factory.resolve(args)?;
            let result =
                self.get_or_build_resolved_with(resolved, &workspace_id, &device_id, |resolved| {
                    self.factory.build(args, resolved)
                });
            if !matches!(result, Err(HostedSetupError::ContextChangedDuringBuild)) {
                return result;
            }
        }
        Err(HostedSetupError::ContextChangedDuringBuild)
    }

    fn get_or_build_resolved_with(
        &self,
        resolved: ResolvedHostedContext,
        workspace_id: &WorkspaceId,
        device_id: &DeviceId,
        build: impl FnOnce(
            ResolvedHostedContext,
        ) -> Result<
            (
                Arc<HostedContext>,
                ResolvedHostedContext,
                Vec<DeviceProofVerifier>,
            ),
            HostedSetupError,
        >,
    ) -> Result<Arc<HostedContext>, HostedSetupError> {
        let fingerprint =
            HostedContextFingerprint::from_resolved(&resolved, workspace_id, device_id);
        if let Some(context) = self.cached(fingerprint)? {
            return Ok(context);
        }

        // Client/runtime construction and remote trust refresh happen outside
        // the mutex. A racing build is discarded by the second check below.
        let expected_credentials = resolved.credentials.clone();
        let expected_identity = resolved.identity.clone();
        let (context, final_resolved, installed_verifiers) = build(resolved)?;
        if final_resolved.credentials != expected_credentials
            || final_resolved.identity != expected_identity
            || !same_verifier_set(&final_resolved.verifiers, &installed_verifiers)
        {
            return Err(HostedSetupError::ContextChangedDuringBuild);
        }
        let final_fingerprint =
            HostedContextFingerprint::from_resolved(&final_resolved, workspace_id, device_id);
        let mut cached = self
            .inner
            .lock()
            .map_err(|_| HostedSetupError::CachePoisoned)?;
        if let Some(existing) = cached.as_ref()
            && existing.fingerprint == final_fingerprint
            && Instant::now() < existing.refresh_at
        {
            return Ok(existing.context.clone());
        }
        *cached = Some(CachedHostedContext {
            fingerprint: final_fingerprint,
            context: context.clone(),
            refresh_at: Instant::now() + HOSTED_CONTEXT_TRUST_REFRESH_INTERVAL,
        });
        Ok(context)
    }

    fn cached(
        &self,
        fingerprint: HostedContextFingerprint,
    ) -> Result<Option<Arc<HostedContext>>, HostedSetupError> {
        let cached = self
            .inner
            .lock()
            .map_err(|_| HostedSetupError::CachePoisoned)?;
        Ok(cached
            .as_ref()
            .filter(|cached| {
                cached.fingerprint == fingerprint && Instant::now() < cached.refresh_at
            })
            .map(|cached| cached.context.clone()))
    }
}

fn same_verifier_set(left: &[DeviceProofVerifier], right: &[DeviceProofVerifier]) -> bool {
    let mut left = left.to_vec();
    let mut right = right.to_vec();
    let order = |left: &DeviceProofVerifier, right: &DeviceProofVerifier| {
        left.workspace_id
            .cmp(&right.workspace_id)
            .then_with(|| left.device_id.cmp(&right.device_id))
            .then_with(|| left.proof_verifier.cmp(&right.proof_verifier))
    };
    left.sort_by(order);
    right.sort_by(order);
    left == right
}

impl HostedContextFingerprint {
    fn from_resolved(
        resolved: &ResolvedHostedContext,
        workspace_id: &WorkspaceId,
        device_id: &DeviceId,
    ) -> Self {
        let mut framed = Vec::new();
        hash_field(&mut framed, resolved.credentials.deployment_url.as_bytes());
        hash_optional(
            &mut framed,
            resolved.credentials.control_plane_token.as_deref(),
        );
        hash_optional(
            &mut framed,
            resolved.credentials.account_session_id.as_deref(),
        );
        hash_optional(
            &mut framed,
            resolved.credentials.workos_access_token.as_deref(),
        );
        hash_field(&mut framed, workspace_id.as_str().as_bytes());
        hash_field(&mut framed, device_id.as_str().as_bytes());
        hash_field(
            &mut framed,
            resolved.identity.public_key.as_str().as_bytes(),
        );
        hash_field(
            &mut framed,
            resolved.identity.fingerprint.as_str().as_bytes(),
        );
        let mut verifiers = resolved.verifiers.iter().collect::<Vec<_>>();
        verifiers.sort_by(|left, right| {
            left.workspace_id
                .cmp(&right.workspace_id)
                .then_with(|| left.device_id.cmp(&right.device_id))
                .then_with(|| left.proof_verifier.cmp(&right.proof_verifier))
        });
        for verifier in verifiers {
            hash_field(&mut framed, verifier.workspace_id.as_str().as_bytes());
            hash_field(&mut framed, verifier.device_id.as_str().as_bytes());
            hash_field(&mut framed, verifier.proof_verifier.as_bytes());
        }
        Self(bowline_storage::stable_object_hash(&framed))
    }
}

fn hash_optional(framed: &mut Vec<u8>, value: Option<&str>) {
    match value {
        Some(value) => {
            framed.push(1);
            hash_field(framed, value.as_bytes());
        }
        None => {
            framed.push(0);
        }
    }
}

fn hash_field(framed: &mut Vec<u8>, value: &[u8]) {
    framed.extend_from_slice(&(value.len() as u64).to_le_bytes());
    framed.extend_from_slice(value);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::control_plane::DaemonCredentials;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn resolved(token: &str) -> ResolvedHostedContext {
        ResolvedHostedContext {
            credentials: DaemonCredentials {
                deployment_url: "https://example.convex.cloud".to_string(),
                control_plane_token: Some(token.to_string()),
                account_session_id: None,
                workos_access_token: None,
            },
            identity: bowline_local::device_keys::DeviceIdentity::generate(),
            verifiers: Vec::new(),
        }
    }

    fn test_context() -> Arc<HostedContext> {
        Arc::new(HostedContext {
            client: Arc::new(
                HostedControlPlaneClient::try_new_with_token(
                    "https://example.convex.cloud",
                    "test-token",
                )
                .expect("test hosted client"),
            ),
            http: SignedUrlByteStore::<HostedControlPlaneClient>::build_http_client(),
        })
    }

    #[derive(Default)]
    struct ConstructionCounters {
        hosted_clients: AtomicUsize,
        runtimes: AtomicUsize,
        http_clients: AtomicUsize,
    }

    fn counted_context(counters: &ConstructionCounters) -> Arc<HostedContext> {
        counters.hosted_clients.fetch_add(1, Ordering::SeqCst);
        counters.runtimes.fetch_add(1, Ordering::SeqCst);
        let client = HostedControlPlaneClient::try_new_with_token(
            "https://example.convex.cloud",
            "test-token",
        )
        .expect("test hosted client");
        counters.http_clients.fetch_add(1, Ordering::SeqCst);
        Arc::new(HostedContext {
            client: Arc::new(client),
            http: SignedUrlByteStore::<HostedControlPlaneClient>::build_http_client(),
        })
    }

    fn get_with_factories(
        cache: &HostedContextCache,
        resolved: ResolvedHostedContext,
        workspace_id: &WorkspaceId,
        device_id: &DeviceId,
        counters: &ConstructionCounters,
    ) -> Arc<HostedContext> {
        let final_resolved = resolved.clone();
        let installed_verifiers = resolved.verifiers.clone();
        cache
            .get_or_build_resolved_with(resolved, workspace_id, device_id, |_| {
                Ok((
                    counted_context(counters),
                    final_resolved,
                    installed_verifiers,
                ))
            })
            .expect("cached hosted context")
    }

    fn get_with_counter(
        cache: &HostedContextCache,
        resolved: ResolvedHostedContext,
        workspace_id: &WorkspaceId,
        device_id: &DeviceId,
        constructions: &AtomicUsize,
    ) -> Arc<HostedContext> {
        let final_resolved = resolved.clone();
        let installed_verifiers = resolved.verifiers.clone();
        cache
            .get_or_build_resolved_with(resolved, workspace_id, device_id, |_| {
                constructions.fetch_add(1, Ordering::SeqCst);
                Ok((test_context(), final_resolved, installed_verifiers))
            })
            .expect("cached hosted context")
    }

    #[test]
    fn acceptance_workload_constructs_once_and_rebuilds_once_after_rotation() {
        let cache = HostedContextCache::new();
        let workspace_id = WorkspaceId::new("workspace_a");
        let device_id = DeviceId::new("device_a");
        let stable = resolved("token-a");
        let counters = ConstructionCounters::default();
        let first =
            get_with_factories(&cache, stable.clone(), &workspace_id, &device_id, &counters);
        // 10 syncs + 10 dispatch polls + 2 status publishes + 1 observer.
        for _ in 1..23 {
            let reused =
                get_with_factories(&cache, stable.clone(), &workspace_id, &device_id, &counters);
            assert!(Arc::ptr_eq(&first, &reused));
        }
        assert_eq!(counters.hosted_clients.load(Ordering::SeqCst), 1);
        assert_eq!(counters.runtimes.load(Ordering::SeqCst), 1);
        assert_eq!(counters.http_clients.load(Ordering::SeqCst), 1);

        let mut rotated = stable;
        rotated.credentials.control_plane_token = Some("token-b".to_string());
        let rebuilt = get_with_factories(
            &cache,
            rotated.clone(),
            &workspace_id,
            &device_id,
            &counters,
        );
        assert!(!Arc::ptr_eq(&first, &rebuilt));
        let stabilized = get_with_factories(&cache, rotated, &workspace_id, &device_id, &counters);
        assert!(Arc::ptr_eq(&rebuilt, &stabilized));
        assert_eq!(counters.hosted_clients.load(Ordering::SeqCst), 2);
        assert_eq!(counters.runtimes.load(Ordering::SeqCst), 2);
        assert_eq!(counters.http_clients.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn verifier_revision_and_signing_scope_each_invalidate() {
        let cache = HostedContextCache::new();
        let workspace_id = WorkspaceId::new("workspace_a");
        let device_id = DeviceId::new("device_a");
        let base = resolved("token-a");
        let constructions = AtomicUsize::new(0);
        let first = get_with_counter(
            &cache,
            base.clone(),
            &workspace_id,
            &device_id,
            &constructions,
        );
        let mut revised = base.clone();
        revised.verifiers.push(DeviceProofVerifier {
            workspace_id: workspace_id.clone(),
            device_id: DeviceId::new("device_b"),
            proof_verifier: "dapv_p256_v1_revised".to_string(),
        });
        let verifier_rebuild = get_with_counter(
            &cache,
            revised.clone(),
            &workspace_id,
            &device_id,
            &constructions,
        );
        assert!(!Arc::ptr_eq(&first, &verifier_rebuild));
        let verifier_stabilized =
            get_with_counter(&cache, revised, &workspace_id, &device_id, &constructions);
        assert!(Arc::ptr_eq(&verifier_rebuild, &verifier_stabilized));
        let scope_rebuild = get_with_counter(
            &cache,
            base,
            &WorkspaceId::new("workspace_b"),
            &device_id,
            &constructions,
        );
        assert!(!Arc::ptr_eq(&verifier_rebuild, &scope_rebuild));
        assert_eq!(constructions.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn bounded_trust_refresh_rebuilds_once_at_deadline() {
        let cache = HostedContextCache::new();
        let workspace_id = WorkspaceId::new("workspace_a");
        let device_id = DeviceId::new("device_a");
        let stable = resolved("token-a");
        let constructions = AtomicUsize::new(0);
        get_with_counter(
            &cache,
            stable.clone(),
            &workspace_id,
            &device_id,
            &constructions,
        );
        cache
            .inner
            .lock()
            .expect("test cache lock")
            .as_mut()
            .expect("cached context")
            .refresh_at = Instant::now();
        get_with_counter(
            &cache,
            stable.clone(),
            &workspace_id,
            &device_id,
            &constructions,
        );
        get_with_counter(&cache, stable, &workspace_id, &device_id, &constructions);
        assert_eq!(constructions.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn add_replace_and_revoke_during_build_are_never_cached_under_new_fingerprint() {
        let workspace_id = WorkspaceId::new("workspace_a");
        let device_id = DeviceId::new("device_a");
        let existing = DeviceProofVerifier {
            workspace_id: workspace_id.clone(),
            device_id: DeviceId::new("device_b"),
            proof_verifier: "dapv_old".to_string(),
        };
        let changes = [
            (Vec::new(), vec![existing.clone()]),
            (
                vec![existing.clone()],
                vec![DeviceProofVerifier {
                    proof_verifier: "dapv_replaced".to_string(),
                    ..existing.clone()
                }],
            ),
            (vec![existing.clone()], Vec::new()),
        ];
        for (initial_verifiers, final_verifiers) in changes {
            let cache = HostedContextCache::new();
            let mut initial = resolved("token-a");
            initial.verifiers = initial_verifiers;
            let mut final_resolved = initial.clone();
            final_resolved.verifiers = final_verifiers;
            let installed = initial.verifiers.clone();
            let result =
                cache.get_or_build_resolved_with(initial, &workspace_id, &device_id, |_| {
                    Ok((test_context(), final_resolved, installed))
                });
            assert!(matches!(
                result,
                Err(HostedSetupError::ContextChangedDuringBuild)
            ));
            assert!(cache.inner.lock().expect("test cache lock").is_none());
        }
    }
}
