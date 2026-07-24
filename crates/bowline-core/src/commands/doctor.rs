//! Read-only engine diagnostics contract (Plan 111 Step 6).
//!
//! REDACTION INVARIANT (binding): every field defined here is a typed enum, a
//! count, a timestamp, a safe workspace ID, an opaque object key, or an opaque
//! hex digest. There is deliberately NO field that can carry a workspace path, a
//! filename, a plaintext hash, or a serialized crypto error. `bowline doctor` is
//! diagnostics, never a required user step; it classifies every probe into one
//! fixed reason code so operators and agents read a stable, safe surface.
//!
//! Check → reason-code map (each check emits exactly one reason from its set):
//!
//! | check id                       | reason codes                                                        |
//! |--------------------------------|---------------------------------------------------------------------|
//! | engine-sqlite-integrity        | integrity-verified · integrity-failed · engine-database-missing     |
//! | ancestor-ref-consistency       | ancestor-consistent · ancestor-missing · ref-regressed-below-verified · engine-database-missing |
//! | intent-recoverability          | intents-recoverable · intent-unclassifiable · engine-database-missing |
//! | watcher-health                 | watcher-healthy · watcher-recovery-pending · daemon-unreachable      |
//! | ref-fetch-verification         | ref-verified · ref-signature-unverifiable · ref-absent · control-plane-unreachable |
//! | ref-metadata-object-existence  | object-present · metadata-missing · object-missing · control-plane-unreachable |
//! | sealed-content-id-verification | sample-verified · content-id-mismatch · seal-verification-unavailable · sample-empty |
//! | workspace-key-availability     | key-available · key-unavailable · epoch-mismatch                     |
//! | retry-age                      | retry-nominal · retry-stale · daemon-unreachable                     |
//! | portable-path-collisions       | no-collisions · portable-path-collision · engine-database-missing    |
//! | temp-capacity                  | capacity-sufficient · capacity-insufficient · state-root-unavailable |
//! | atomic-rename-capability       | rename-supported · rename-unsupported · state-root-unavailable       |
//! | deployment-identity            | identity-matched · identity-mismatched · identity-unknown            |
//! | installed-candidate-hash       | hash-computed · hash-unavailable                                     |

use serde::{Deserialize, Serialize};

use crate::commands::CommandName;
use crate::ids::WorkspaceId;

/// The engine a doctor run targets. Only the manifest engine exists post-cutover;
/// the enum keeps the wire honest if a second engine is ever introduced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DoctorEngine {
    Manifest,
}

/// The stable identity of one diagnostic probe. Ordering is the run/report order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DoctorCheckId {
    EngineSqliteIntegrity,
    AncestorRefConsistency,
    IntentRecoverability,
    WatcherHealth,
    RefFetchVerification,
    RefMetadataObjectExistence,
    SealedContentIdVerification,
    WorkspaceKeyAvailability,
    RetryAge,
    PortablePathCollisions,
    TempCapacity,
    AtomicRenameCapability,
    DeploymentIdentity,
    InstalledCandidateHash,
}

impl DoctorCheckId {
    /// Every check, in deterministic run/report order. The handler iterates this
    /// so the golden JSON contract never drifts from a hand-maintained list.
    pub const ALL: [Self; 14] = [
        Self::EngineSqliteIntegrity,
        Self::AncestorRefConsistency,
        Self::IntentRecoverability,
        Self::WatcherHealth,
        Self::RefFetchVerification,
        Self::RefMetadataObjectExistence,
        Self::SealedContentIdVerification,
        Self::WorkspaceKeyAvailability,
        Self::RetryAge,
        Self::PortablePathCollisions,
        Self::TempCapacity,
        Self::AtomicRenameCapability,
        Self::DeploymentIdentity,
        Self::InstalledCandidateHash,
    ];
}

/// The severity rung of a probe outcome. `Degraded` is a reachable dependency the
/// engine recovers from automatically (never a user step); `Unavailable` is a
/// probe that could not run; `Failed` is the only rung that asks for a human.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DoctorCheckStatus {
    Ok,
    Degraded,
    Unavailable,
    Failed,
}

/// The fixed, safe reason code a probe resolves to. One flat enum keeps the set
/// closed and greppable; the module doc maps each check to its legal subset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DoctorReason {
    IntegrityVerified,
    IntegrityFailed,
    EngineDatabaseMissing,
    AncestorConsistent,
    AncestorMissing,
    RefRegressedBelowVerified,
    IntentsRecoverable,
    IntentUnclassifiable,
    WatcherHealthy,
    WatcherRecoveryPending,
    DaemonUnreachable,
    RefVerified,
    RefSignatureUnverifiable,
    RefAbsent,
    ControlPlaneUnreachable,
    ObjectPresent,
    MetadataMissing,
    ObjectMissing,
    SampleVerified,
    ContentIdMismatch,
    SealVerificationUnavailable,
    SampleEmpty,
    KeyAvailable,
    KeyUnavailable,
    EpochMismatch,
    RetryNominal,
    RetryStale,
    NoCollisions,
    PortablePathCollision,
    CapacitySufficient,
    CapacityInsufficient,
    StateRootUnavailable,
    RenameSupported,
    RenameUnsupported,
    IdentityMatched,
    IdentityMismatched,
    IdentityUnknown,
    HashComputed,
    HashUnavailable,
}

/// One probe's outcome. `count` and `opaque` are optional safe detail: `count` is
/// a plain scalar (pending intents, sampled blobs, free MiB); `opaque` is an
/// opaque hex digest or object key ONLY — the type system cannot enforce that, so
/// every producer is reviewed and the redaction test seeds distinctive paths and
/// asserts none survive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DoctorCheck {
    pub id: DoctorCheckId,
    pub status: DoctorCheckStatus,
    pub reason: DoctorReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opaque: Option<String>,
}

impl DoctorCheck {
    /// A bare outcome with no detail scalar.
    pub fn new(id: DoctorCheckId, status: DoctorCheckStatus, reason: DoctorReason) -> Self {
        Self {
            id,
            status,
            reason,
            count: None,
            opaque: None,
        }
    }

    #[must_use]
    pub fn with_count(mut self, count: u64) -> Self {
        self.count = Some(count);
        self
    }

    /// Attaches an opaque detail string. Callers pass ONLY an opaque hex digest or
    /// object key — never a path, filename, or plaintext hash.
    #[must_use]
    pub fn with_opaque(mut self, opaque: String) -> Self {
        self.opaque = Some(opaque);
        self
    }
}

/// The tallied verdict. `attentionRequired` is true iff any check `Failed`, the
/// only rung that asks for a human; degraded/unavailable are self-healing or
/// informational and never gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DoctorSummary {
    pub ok: u32,
    pub degraded: u32,
    pub unavailable: u32,
    pub failed: u32,
    pub attention_required: bool,
}

impl DoctorSummary {
    /// Tallies a check set into the verdict. Deterministic: a pure fold over the
    /// outcomes, no ordering dependence.
    pub fn tally(checks: &[DoctorCheck]) -> Self {
        let mut summary = Self {
            ok: 0,
            degraded: 0,
            unavailable: 0,
            failed: 0,
            attention_required: false,
        };
        for check in checks {
            match check.status {
                DoctorCheckStatus::Ok => summary.ok += 1,
                DoctorCheckStatus::Degraded => summary.degraded += 1,
                DoctorCheckStatus::Unavailable => summary.unavailable += 1,
                DoctorCheckStatus::Failed => summary.failed += 1,
            }
        }
        summary.attention_required = summary.failed > 0;
        summary
    }
}

/// The `bowline doctor --engine manifest --json` output. Every field is safe by
/// construction (see the module redaction invariant).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DoctorCommandOutput {
    pub contract_version: u16,
    pub command: CommandName,
    pub generated_at: String,
    pub engine: DoctorEngine,
    pub workspace_id: WorkspaceId,
    pub summary: DoctorSummary,
    pub checks: Vec<DoctorCheck>,
}
