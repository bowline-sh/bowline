use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use bowline_control_plane::DependencyFailureClass;
use bowline_core::ids::{DeviceId, WorkspaceId};
use bowline_local::sync::manifest_engine::EngineProcessIdentity;
use bowline_local::sync::manifest_engine::{ManifestKey, RefObservation};

/// Lifecycle of the reactive hosted-ref observer. `Live` requires a verified
/// initial authority, not merely a running worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefObserverState {
    Connecting,
    Live,
    Retrying,
    Blocked,
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct RefObserverEndpointGeneration(u64);

impl RefObserverEndpointGeneration {
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

pub type RefObserverProcessIdentity = EngineProcessIdentity;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefObserverAuthoritySource {
    process_identity: RefObserverProcessIdentity,
    workspace_identity: WorkspaceId,
    endpoint_generation: RefObserverEndpointGeneration,
}

impl RefObserverAuthoritySource {
    pub(crate) fn issue(
        process_identity: RefObserverProcessIdentity,
        workspace_identity: WorkspaceId,
        endpoint_generation: RefObserverEndpointGeneration,
    ) -> Self {
        Self {
            process_identity,
            workspace_identity,
            endpoint_generation,
        }
    }

    pub const fn process_identity(&self) -> &RefObserverProcessIdentity {
        &self.process_identity
    }

    pub fn workspace_identity(&self) -> &WorkspaceId {
        &self.workspace_identity
    }

    pub const fn endpoint_generation(&self) -> RefObserverEndpointGeneration {
        self.endpoint_generation
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct RefObserverLifecycleRevision(u64);

impl RefObserverLifecycleRevision {
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Exact, signature-verified authority carried by the subscription. Genesis is
/// typed rather than encoded as a missing key.
#[derive(Debug, Clone, PartialEq, Eq)]
enum VerifiedWorkspaceRefValue {
    Genesis,
    Head {
        version: u64,
        manifest_key: ManifestKey,
    },
}

/// Signature-verified authority issued only by the observer. Callers can inspect
/// the exact tagged value but cannot fabricate a verified frontier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedWorkspaceRef(VerifiedWorkspaceRefValue);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifiedWorkspaceRefView<'a> {
    Genesis,
    Head {
        version: u64,
        manifest_key: &'a ManifestKey,
    },
}

impl VerifiedWorkspaceRef {
    pub(crate) fn genesis() -> Self {
        Self(VerifiedWorkspaceRefValue::Genesis)
    }

    pub(crate) fn from_observation(observation: RefObservation) -> Self {
        Self(VerifiedWorkspaceRefValue::Head {
            version: observation.version,
            manifest_key: observation.manifest_key,
        })
    }

    pub fn view(&self) -> VerifiedWorkspaceRefView<'_> {
        match &self.0 {
            VerifiedWorkspaceRefValue::Genesis => VerifiedWorkspaceRefView::Genesis,
            VerifiedWorkspaceRefValue::Head {
                version,
                manifest_key,
            } => VerifiedWorkspaceRefView::Head {
                version: *version,
                manifest_key,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefObserverFailureStage {
    Start,
    InitialValue,
    Stream,
    Authentication,
    UnknownSigner(DeviceId),
    UntrustedSigner(DeviceId),
    Authorization,
    Integrity,
    FatalContract,
}

/// Stable diagnostic code retained in the snapshot. It never contains raw
/// provider text, paths, credentials, or private workspace data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefObserverFailureCode {
    StartUnavailable,
    InitialValueTimeout,
    StreamUnavailable,
    AuthenticationRequired,
    UnknownSigner,
    AuthorizationLost,
    Integrity,
    FatalContract,
}

impl RefObserverFailureCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StartUnavailable => "start-unavailable",
            Self::InitialValueTimeout => "initial-value-timeout",
            Self::StreamUnavailable => "stream-unavailable",
            Self::AuthenticationRequired => "authentication-required",
            Self::UnknownSigner => "unknown-signer",
            Self::AuthorizationLost => "authorization-lost",
            Self::Integrity => "integrity",
            Self::FatalContract => "fatal-contract",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefObserverReadiness {
    Live,
    Retrying,
    Blocked {
        class: DependencyFailureClass,
        code: RefObserverFailureCode,
    },
}

/// The specific external fact that changed after a terminal observer failure.
/// Each terminal class requires its own evidence; successful authentication
/// cannot silently clear an integrity or contract failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefObserverRemediationKind {
    AuthenticationRestored,
    AuthorizationRestored,
    IntegrityRevalidated,
    ContractUpgraded,
}

/// Owner-issued evidence that remediates one exact blocked observer frontier.
/// Private fields prevent a stale callback from clearing a later failure that
/// merely happens to share the same dependency class.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefObserverRemediation {
    authority_source: RefObserverAuthoritySource,
    blocked_lifecycle_revision: RefObserverLifecycleRevision,
    failure_class: DependencyFailureClass,
    failure_code: RefObserverFailureCode,
    kind: RefObserverRemediationKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefObserverFailure {
    pub stage: RefObserverFailureStage,
    pub class: DependencyFailureClass,
    pub code: RefObserverFailureCode,
}

/// One immutable observer frontier. Exact callers compare both identities for
/// equality at admission and completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefObserverSnapshot {
    pub authority_source: RefObserverAuthoritySource,
    pub lifecycle_revision: RefObserverLifecycleRevision,
    pub state: RefObserverState,
    pub verified_ref: Option<VerifiedWorkspaceRef>,
    pub consecutive_failures: u32,
    pub reconnects: u64,
    pub last_failure: Option<RefObserverFailure>,
}

impl RefObserverSnapshot {
    pub fn readiness(&self) -> RefObserverReadiness {
        if self.state == RefObserverState::Live {
            return RefObserverReadiness::Live;
        }
        match self.last_failure.as_ref() {
            Some(failure) => match failure.class {
                DependencyFailureClass::Retryable => RefObserverReadiness::Retrying,
                DependencyFailureClass::AuthenticationRequired
                | DependencyFailureClass::AuthorizationLost
                | DependencyFailureClass::Integrity
                | DependencyFailureClass::FatalContract => RefObserverReadiness::Blocked {
                    class: failure.class,
                    code: failure.code,
                },
            },
            None => RefObserverReadiness::Retrying,
        }
    }

    pub fn frontier(&self) -> Option<RefObserverFrontier> {
        if self.state != RefObserverState::Live {
            return None;
        }
        Some(RefObserverFrontier {
            authority_source: self.authority_source.clone(),
            lifecycle_revision: self.lifecycle_revision,
            verified_ref: self.verified_ref.clone()?,
        })
    }

    fn starting(authority_source: RefObserverAuthoritySource) -> Self {
        Self {
            authority_source,
            lifecycle_revision: RefObserverLifecycleRevision(0),
            state: RefObserverState::Connecting,
            verified_ref: None,
            consecutive_failures: 0,
            reconnects: 0,
            last_failure: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefObserverFrontier {
    pub authority_source: RefObserverAuthoritySource,
    pub lifecycle_revision: RefObserverLifecycleRevision,
    pub verified_ref: VerifiedWorkspaceRef,
}

struct RefObserverShared {
    authority_source: RefObserverAuthoritySource,
    snapshot: Mutex<RefObserverSnapshot>,
    authority_restored: Condvar,
}

impl std::fmt::Debug for RefObserverShared {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("RefObserverShared")
    }
}

#[derive(Clone, Debug)]
pub struct RefObserverSnapshotHandle(Arc<RefObserverShared>);

impl RefObserverSnapshotHandle {
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        Self::for_endpoint(RefObserverEndpointGeneration::new(0))
    }

    pub(crate) fn for_source(authority_source: RefObserverAuthoritySource) -> Self {
        Self(Arc::new(RefObserverShared {
            authority_source: authority_source.clone(),
            snapshot: Mutex::new(RefObserverSnapshot::starting(authority_source)),
            authority_restored: Condvar::new(),
        }))
    }

    #[cfg(test)]
    pub(crate) fn for_endpoint(endpoint_generation: RefObserverEndpointGeneration) -> Self {
        Self::for_source(RefObserverAuthoritySource::issue(
            RefObserverProcessIdentity::current(),
            WorkspaceId::new("ws_ref_observer_test"),
            endpoint_generation,
        ))
    }

    pub fn current(&self) -> RefObserverSnapshot {
        self.0
            .snapshot
            .lock()
            .map(|snapshot| snapshot.clone())
            .unwrap_or_else(|_| RefObserverSnapshot::starting(self.0.authority_source.clone()))
    }

    pub fn readiness(&self) -> RefObserverReadiness {
        self.current().readiness()
    }

    pub(crate) fn connecting(&self, consecutive_failures: u32) {
        if let Ok(mut snapshot) = self.0.snapshot.lock() {
            if !advance_lifecycle(&mut snapshot) {
                return;
            }
            snapshot.state = RefObserverState::Connecting;
            snapshot.consecutive_failures = consecutive_failures;
        }
    }

    /// Temporary integration seam. The exact observer path uses the narrower
    /// transition methods below; a synthetic Live transition is typed Genesis.
    pub(crate) fn transition(
        &self,
        state: RefObserverState,
        consecutive_failures: u32,
        reconnect: bool,
        last_failure: Option<RefObserverFailure>,
    ) {
        if state == RefObserverState::Live {
            self.live_with_ref(VerifiedWorkspaceRef::genesis());
        } else {
            self.transition_without_ref(state, consecutive_failures, reconnect, last_failure);
        }
    }

    fn transition_without_ref(
        &self,
        state: RefObserverState,
        consecutive_failures: u32,
        reconnect: bool,
        last_failure: Option<RefObserverFailure>,
    ) {
        if let Ok(mut snapshot) = self.0.snapshot.lock() {
            if !advance_lifecycle(&mut snapshot) {
                return;
            }
            snapshot.state = state;
            snapshot.consecutive_failures = consecutive_failures;
            snapshot.reconnects = snapshot.reconnects.saturating_add(u64::from(reconnect));
            snapshot.last_failure = last_failure;
        }
    }

    pub(crate) fn live_with_ref(&self, verified_ref: VerifiedWorkspaceRef) {
        if let Ok(mut snapshot) = self.0.snapshot.lock() {
            if !advance_lifecycle(&mut snapshot) {
                return;
            }
            snapshot.state = RefObserverState::Live;
            snapshot.verified_ref = Some(verified_ref);
            snapshot.consecutive_failures = 0;
            snapshot.last_failure = None;
        }
    }

    pub(super) fn blocked(&self, failure: RefObserverFailure, consecutive_failures: u32) {
        self.transition_without_ref(
            RefObserverState::Blocked,
            consecutive_failures,
            false,
            Some(failure),
        );
    }

    /// Explicitly acknowledge the exact remediation required by the retained
    /// terminal failure. Merely waiting cannot leave `Blocked`.
    pub fn remediation_completed(&self, remediation: RefObserverRemediation) -> bool {
        let Ok(mut snapshot) = self.0.snapshot.lock() else {
            return false;
        };
        let remediation_matches = snapshot.last_failure.as_ref().is_some_and(|failure| {
            remediation.authority_source == snapshot.authority_source
                && remediation.blocked_lifecycle_revision == snapshot.lifecycle_revision
                && remediation.failure_class == failure.class
                && remediation.failure_code == failure.code
                && remediation.kind.matches(failure.class)
        });
        if snapshot.state != RefObserverState::Blocked
            || !remediation_matches
            || !advance_lifecycle(&mut snapshot)
        {
            return false;
        }
        snapshot.state = RefObserverState::Connecting;
        snapshot.consecutive_failures = 0;
        snapshot.last_failure = None;
        self.0.authority_restored.notify_all();
        true
    }

    #[cfg(test)]
    pub(crate) fn remediation_for_current_block(
        &self,
        kind: RefObserverRemediationKind,
    ) -> Option<RefObserverRemediation> {
        let snapshot = self.0.snapshot.lock().ok()?;
        let failure = snapshot.last_failure.as_ref()?;
        (snapshot.state == RefObserverState::Blocked && kind.matches(failure.class)).then(|| {
            RefObserverRemediation {
                authority_source: snapshot.authority_source.clone(),
                blocked_lifecycle_revision: snapshot.lifecycle_revision,
                failure_class: failure.class,
                failure_code: failure.code,
                kind,
            }
        })
    }

    pub(super) fn wait_for_authority_restore(&self, shutdown: &AtomicBool) -> bool {
        let Ok(mut snapshot) = self.0.snapshot.lock() else {
            return false;
        };
        while snapshot.state == RefObserverState::Blocked && !shutdown.load(Ordering::SeqCst) {
            let Ok(waited) = self.0.authority_restored.wait(snapshot) else {
                return false;
            };
            snapshot = waited;
        }
        !shutdown.load(Ordering::SeqCst)
    }

    /// Set the wait predicate and notify while owning its mutex. This prevents
    /// shutdown from landing between a blocked worker's predicate check and
    /// its condvar enrollment.
    pub(super) fn request_shutdown(&self, shutdown: &AtomicBool) {
        match self.0.snapshot.lock() {
            Ok(_snapshot) => {
                shutdown.store(true, Ordering::SeqCst);
                self.0.authority_restored.notify_all();
            }
            Err(poisoned) => {
                let _snapshot = poisoned.into_inner();
                shutdown.store(true, Ordering::SeqCst);
                self.0.authority_restored.notify_all();
            }
        }
    }

    pub(super) fn stopped(&self) {
        if let Ok(mut snapshot) = self.0.snapshot.lock() {
            if !advance_lifecycle(&mut snapshot) {
                return;
            }
            snapshot.state = RefObserverState::Stopped;
        }
    }
}

impl RefObserverRemediationKind {
    fn matches(self, class: DependencyFailureClass) -> bool {
        matches!(
            (self, class),
            (
                Self::AuthenticationRestored,
                DependencyFailureClass::AuthenticationRequired
            ) | (
                Self::AuthorizationRestored,
                DependencyFailureClass::AuthorizationLost
            ) | (
                Self::IntegrityRevalidated,
                DependencyFailureClass::Integrity
            ) | (
                Self::ContractUpgraded,
                DependencyFailureClass::FatalContract
            )
        )
    }
}

fn advance_lifecycle(snapshot: &mut RefObserverSnapshot) -> bool {
    let Some(next) = snapshot.lifecycle_revision.0.checked_add(1) else {
        snapshot.state = RefObserverState::Blocked;
        snapshot.last_failure = Some(RefObserverFailure {
            stage: RefObserverFailureStage::FatalContract,
            class: DependencyFailureClass::FatalContract,
            code: RefObserverFailureCode::FatalContract,
        });
        return false;
    };
    snapshot.lifecycle_revision = RefObserverLifecycleRevision(next);
    true
}

pub type RefObserverHealth = RefObserverSnapshot;
pub type RefObserverHealthHandle = RefObserverSnapshotHandle;
