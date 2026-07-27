//! Live device trust for one workspace's hosted client.
//!
//! A hosted client verifies every signed workspace head against the device
//! authorization proof verifier of whichever device signed it. Those verifiers
//! are learned from the control plane, and a daemon outlives the moment it
//! learned them: trusting a second device while the first daemon runs publishes
//! heads signed by a device the running client has never heard of. A snapshot
//! taken at construction can only answer "unknown" forever, which is what
//! stalled sync until the daemon was restarted.
//!
//! So the verifier map lives here, behind a lock, shared by the resolver
//! installed into the client and by the refresh that learns new devices. The
//! refresh is deliberately *not* reachable from the resolver: the resolver runs
//! inside the client's own response parsing (on the subscription's Tokio worker
//! for pushed refs), so calling the control plane from it would be a reentrant
//! call into the client mid-parse — a `block_on` inside its own runtime. The
//! observer calls [`WorkspaceDeviceTrust::refresh_unknown_signer`] between
//! stream attempts instead, where no client call is in flight and no lock is
//! held across the network.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use bowline_control_plane::{
    AuthorizedDeviceRecord, ControlPlaneError, ControlPlaneResult, DeviceControlPlaneClient,
    HostedControlPlaneClient,
};
use bowline_core::ids::{DeviceId, WorkspaceId};
use bowline_local::device_keys::{DeviceKeyError, DeviceProofVerifier, DeviceProofVerifierCache};

/// Shortest interval between two control-plane trust reads triggered by an
/// unverifiable head. A device that genuinely just enrolled is learned by the
/// first read, so this only paces the pathological case: a stream that keeps
/// naming signers this host will never be able to verify must not turn every
/// pushed value into a control-plane call.
pub const UNKNOWN_SIGNER_REFRESH_COOLDOWN: Duration = Duration::from_secs(30);

/// How long a refused signer is taken at its word. Long enough that a head this
/// host will never verify costs one read per device per interval, short enough
/// that a device approved after it was refused — a re-approval, or an approval
/// racing this read — is picked up without anyone restarting the daemon.
pub const REFUSED_SIGNER_RETRY_INTERVAL: Duration = Duration::from_secs(600);

/// How many refused signers are remembered. The memory exists to stop asking
/// about a device the control plane has already disowned; it is capped because
/// the device ids in a pushed ref are chosen by whoever wrote it. Past the cap
/// the cooldown alone bounds the call rate.
const MAX_REFUSED_SIGNERS: usize = 64;

/// The one control-plane read a trust refresh performs. Narrow on purpose: the
/// refresher must not be able to reach anything else on the client whose parse
/// produced the failure.
pub trait AuthorizedDeviceSource {
    fn authorized_devices(
        &self,
        workspace_id: &WorkspaceId,
    ) -> ControlPlaneResult<Vec<AuthorizedDeviceRecord>>;
}

impl AuthorizedDeviceSource for HostedControlPlaneClient {
    fn authorized_devices(
        &self,
        workspace_id: &WorkspaceId,
    ) -> ControlPlaneResult<Vec<AuthorizedDeviceRecord>> {
        Ok(self.list_device_trust(workspace_id)?.authorized_devices)
    }
}

/// Where a refreshed verifier set is written so it survives this process, and so
/// short-lived CLI runs verify refs against the same trust the daemon learned.
/// A callback rather than a key store handle because the key store trait is not
/// `Send + Sync`, and this is shared across the daemon's threads.
pub type VerifierStore =
    Arc<dyn Fn(&WorkspaceId, &[DeviceProofVerifier]) -> Result<(), DeviceKeyError> + Send + Sync>;

/// Why a trust refresh could not complete.
#[derive(Debug)]
pub enum TrustRefreshError {
    ControlPlane(ControlPlaneError),
    Persist(DeviceKeyError),
    CachePoisoned { lock: &'static str },
}

impl fmt::Display for TrustRefreshError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ControlPlane(error) => {
                write!(formatter, "device trust could not be read: {error}")
            }
            Self::Persist(error) => {
                write!(
                    formatter,
                    "refreshed device trust could not be stored: {error}"
                )
            }
            Self::CachePoisoned { lock } => {
                write!(formatter, "device trust `{lock}` lock is poisoned")
            }
        }
    }
}

impl std::error::Error for TrustRefreshError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ControlPlane(error) => Some(error),
            Self::Persist(error) => Some(error),
            Self::CachePoisoned { .. } => None,
        }
    }
}

/// What a refresh triggered by an unverifiable head concluded about the signer.
#[derive(Debug)]
pub enum TrustRefreshOutcome {
    /// The control plane authorizes the signer and its verifier is now
    /// installed. The read that failed can be retried immediately.
    Learned,
    /// The control plane does not authorize the signer in this workspace. The
    /// signer is not asked about again until the refusal expires.
    NotAuthorized,
    /// A recent refresh already refused this signer; no control-plane call was
    /// made. Reconsidered after [`REFUSED_SIGNER_RETRY_INTERVAL`].
    AlreadyRefused,
    /// A refresh ran within the cooldown window; no control-plane call was made.
    RateLimited,
    /// The refresh itself failed; the signer stays unknown and may be tried
    /// again after the cooldown.
    Unavailable(TrustRefreshError),
}

impl TrustRefreshOutcome {
    /// Whether the failed read is worth retrying at once rather than after the
    /// observer's backoff.
    pub fn learned(&self) -> bool {
        matches!(self, Self::Learned)
    }

    /// Whether the control plane has answered that this signer is not
    /// authorized here. Separate from "not learned yet": the question has been
    /// asked, and only a change in the workspace's own device trust can change
    /// the answer — a different fact for logs and for status.
    pub fn refused(&self) -> bool {
        matches!(self, Self::NotAuthorized | Self::AlreadyRefused)
    }

    pub fn as_log(&self) -> &'static str {
        match self {
            Self::Learned => "trust learned",
            Self::NotAuthorized => "device is not authorized in this workspace",
            Self::AlreadyRefused => "device was already refused by an earlier trust read",
            Self::RateLimited => "trust was read too recently to read again",
            Self::Unavailable(_) => "device trust could not be read",
        }
    }
}

impl fmt::Display for TrustRefreshOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(error) => write!(formatter, "{}: {error}", self.as_log()),
            outcome => formatter.write_str(outcome.as_log()),
        }
    }
}

/// Bounds on how often an unverifiable head may cost a control-plane call.
#[derive(Default)]
struct RefreshBudget {
    last_refresh_at: Option<Instant>,
    /// When each refused signer was last asked about.
    refused: BTreeMap<DeviceId, Instant>,
}

/// One workspace's device proof verifiers, shared between the client's verifier
/// resolver and the observer that refreshes them.
pub struct WorkspaceDeviceTrust {
    workspace_id: WorkspaceId,
    verifiers: RwLock<DeviceProofVerifierCache>,
    budget: RwLock<RefreshBudget>,
    store: VerifierStore,
}

impl WorkspaceDeviceTrust {
    pub fn new(
        workspace_id: WorkspaceId,
        verifiers: DeviceProofVerifierCache,
        store: VerifierStore,
    ) -> Arc<Self> {
        Arc::new(Self {
            workspace_id,
            verifiers: RwLock::new(verifiers),
            budget: RwLock::new(RefreshBudget::default()),
            store,
        })
    }

    /// The verifier resolver to install into the hosted client. It only reads
    /// the shared map — never the control plane — because it runs inside the
    /// client's own response parsing.
    pub fn resolver(
        self: &Arc<Self>,
    ) -> impl Fn(&WorkspaceId, &DeviceId) -> ControlPlaneResult<Option<String>> + Send + Sync + 'static
    {
        let trust = Arc::clone(self);
        move |workspace_id, device_id| {
            let verifiers = trust
                .verifiers
                .read()
                .map_err(|_| ControlPlaneError::Internal {
                    reason: "device trust verifier cache lock is poisoned",
                })?;
            Ok(verifiers
                .get(&(Some(workspace_id.clone()), device_id.clone()))
                .cloned())
        }
    }

    /// The verifiers currently installed for THIS workspace.
    ///
    /// Scoped to `self.workspace_id` rather than returning the whole cache: the
    /// seed comes from a key store that may hold several workspaces, and every
    /// caller compares this against a set that is authoritative for one
    /// workspace only. An unscoped snapshot would differ from that set whenever
    /// another workspace happened to be cached, which reads as "trust changed"
    /// on every refresh and would clear the refusal backoff that bounds
    /// control-plane reads.
    pub fn installed_verifiers(&self) -> Result<Vec<DeviceProofVerifier>, TrustRefreshError> {
        let verifiers = self.verifiers.read().map_err(|_| poisoned("verifiers"))?;
        Ok(workspace_verifiers(&verifiers, &self.workspace_id))
    }

    /// Read device trust from the control plane, make it this workspace's
    /// authoritative verifier set, and store it.
    pub fn refresh(
        &self,
        devices: &dyn AuthorizedDeviceSource,
    ) -> Result<Vec<DeviceProofVerifier>, TrustRefreshError> {
        let authoritative = self.read_and_install(devices)?;
        (self.store)(&self.workspace_id, &authoritative).map_err(TrustRefreshError::Persist)?;
        Ok(authoritative)
    }

    fn read_and_install(
        &self,
        devices: &dyn AuthorizedDeviceSource,
    ) -> Result<Vec<DeviceProofVerifier>, TrustRefreshError> {
        let authorized = devices
            .authorized_devices(&self.workspace_id)
            .map_err(TrustRefreshError::ControlPlane)?;
        let authoritative = authorized_device_proof_verifiers(&self.workspace_id, authorized);
        self.install(&authoritative)?;
        Ok(authoritative)
    }

    /// Learn trust for a head this host could not verify.
    ///
    /// Bounded twice over: a signer the control plane has already disowned is
    /// not asked about again for [`REFUSED_SIGNER_RETRY_INTERVAL`], and no two
    /// reads for this workspace happen inside
    /// [`UNKNOWN_SIGNER_REFRESH_COOLDOWN`] however many distinct signers ask. A
    /// head that stays unverifiable therefore settles at one read per interval
    /// however often it is pushed.
    pub fn refresh_unknown_signer(
        &self,
        devices: &dyn AuthorizedDeviceSource,
        device_id: &DeviceId,
        now: Instant,
    ) -> TrustRefreshOutcome {
        match self.knows(device_id) {
            Ok(true) => return TrustRefreshOutcome::Learned,
            Ok(false) => {}
            Err(error) => return TrustRefreshOutcome::Unavailable(error),
        }
        if let Err(outcome) = self.claim_refresh(device_id, now) {
            return outcome;
        }
        let before = match self.installed_verifiers() {
            Ok(before) => before,
            Err(error) => return TrustRefreshOutcome::Unavailable(error),
        };
        let refreshed = match self.read_and_install(devices) {
            Ok(refreshed) => refreshed,
            Err(error) => return TrustRefreshOutcome::Unavailable(error),
        };
        // A store that refuses the write does not undo what was learned: the
        // live cache already holds it and sync converges. Only the warm start
        // for the next process is lost, so it is reported rather than failing
        // the refresh — and reported here, the single owner of the store.
        if let Err(error) = (self.store)(&self.workspace_id, &refreshed) {
            eprintln!(
                "bowline-daemon could not store refreshed device trust for workspace {}: {error}",
                self.workspace_id.as_str()
            );
        }
        self.settle(device_id, &before, &refreshed, now)
    }

    /// Reserve this workspace's refresh slot, or explain why no call is made.
    fn claim_refresh(&self, device_id: &DeviceId, now: Instant) -> Result<(), TrustRefreshOutcome> {
        let mut budget = self
            .budget
            .write()
            .map_err(|_| TrustRefreshOutcome::Unavailable(poisoned("budget")))?;
        if budget.refused.get(device_id).is_some_and(|refused_at| {
            now.saturating_duration_since(*refused_at) < REFUSED_SIGNER_RETRY_INTERVAL
        }) {
            return Err(TrustRefreshOutcome::AlreadyRefused);
        }
        if budget.last_refresh_at.is_some_and(|last| {
            now.saturating_duration_since(last) < UNKNOWN_SIGNER_REFRESH_COOLDOWN
        }) {
            return Err(TrustRefreshOutcome::RateLimited);
        }
        // Stamped before the call so a concurrent unknown signer is paced by the
        // read already in flight rather than starting a second one.
        budget.last_refresh_at = Some(now);
        Ok(())
    }

    /// Record what the refresh concluded about `device_id`.
    fn settle(
        &self,
        device_id: &DeviceId,
        before: &[DeviceProofVerifier],
        refreshed: &[DeviceProofVerifier],
        now: Instant,
    ) -> TrustRefreshOutcome {
        let learned = refreshed
            .iter()
            .any(|verifier| &verifier.device_id == device_id);
        let mut budget = match self.budget.write() {
            Ok(budget) => budget,
            Err(_) => return TrustRefreshOutcome::Unavailable(poisoned("budget")),
        };
        // Any change in the workspace's trust can also change the answer for a
        // device refused earlier, so those refusals stop being evidence.
        if refreshed != before {
            budget.refused.clear();
        }
        if learned {
            return TrustRefreshOutcome::Learned;
        }
        if budget.refused.len() < MAX_REFUSED_SIGNERS {
            budget.refused.insert(device_id.clone(), now);
        }
        TrustRefreshOutcome::NotAuthorized
    }

    fn knows(&self, device_id: &DeviceId) -> Result<bool, TrustRefreshError> {
        let verifiers = self.verifiers.read().map_err(|_| poisoned("verifiers"))?;
        Ok(verifiers.contains_key(&(Some(self.workspace_id.clone()), device_id.clone())))
    }

    fn install(&self, authoritative: &[DeviceProofVerifier]) -> Result<(), TrustRefreshError> {
        let mut verifiers = self.verifiers.write().map_err(|_| poisoned("verifiers"))?;
        verifiers.retain(|(workspace_id, _), _| workspace_id.as_ref() != Some(&self.workspace_id));
        for verifier in authoritative {
            verifiers.insert(
                (
                    Some(verifier.workspace_id.clone()),
                    verifier.device_id.clone(),
                ),
                verifier.proof_verifier.clone(),
            );
        }
        Ok(())
    }
}

const fn poisoned(lock: &'static str) -> TrustRefreshError {
    TrustRefreshError::CachePoisoned { lock }
}

fn workspace_verifiers(
    verifiers: &DeviceProofVerifierCache,
    workspace_id: &WorkspaceId,
) -> Vec<DeviceProofVerifier> {
    verifiers
        .iter()
        .filter_map(|((cached_workspace_id, device_id), proof_verifier)| {
            let cached_workspace_id = cached_workspace_id.clone()?;
            (&cached_workspace_id == workspace_id).then(|| DeviceProofVerifier {
                workspace_id: cached_workspace_id,
                device_id: device_id.clone(),
                proof_verifier: proof_verifier.clone(),
            })
        })
        .collect()
}

/// The verifiers a workspace's authorized devices publish, in device order so
/// two refreshes of the same trust produce the same set.
fn authorized_device_proof_verifiers(
    workspace_id: &WorkspaceId,
    devices: Vec<AuthorizedDeviceRecord>,
) -> Vec<DeviceProofVerifier> {
    let mut verifiers = devices
        .into_iter()
        .filter_map(|device| {
            device
                .device_authorization_proof_verifier
                .map(|proof_verifier| DeviceProofVerifier {
                    workspace_id: workspace_id.clone(),
                    device_id: device.device_id,
                    proof_verifier,
                })
        })
        .collect::<Vec<_>>();
    verifiers.sort_by(|left, right| left.device_id.cmp(&right.device_id));
    verifiers
}

#[cfg(test)]
#[path = "device_trust/tests.rs"]
mod tests;
