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
use crate::status::RepairCommand;

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

impl DoctorReason {
    /// A plain-language sentence explaining what this reason means.
    ///
    /// Fixed sentences keyed off a closed enum, so the redaction invariant holds
    /// by construction: no producer can splice a path or filename in here.
    pub const fn explanation(self) -> &'static str {
        match self {
            Self::IntegrityVerified => "The local engine database passed its integrity check.",
            Self::IntegrityFailed => {
                "The local engine database failed its integrity check. Its contents are all \
                 re-derivable from the workspace, so rebuilding it is safe."
            }
            Self::EngineDatabaseMissing => {
                "This device has no local engine database yet, so the check could not run."
            }
            Self::AncestorConsistent => "Every local ref descends from its verified ancestor.",
            Self::AncestorMissing => {
                "A ref names an ancestor this device has not fetched yet. Sync will fetch it."
            }
            Self::RefRegressedBelowVerified => {
                "A ref moved backwards past state this device already verified. Sync is paused \
                 rather than accepting the regression."
            }
            Self::IntentsRecoverable => "Every queued sync intent can be replayed.",
            Self::IntentUnclassifiable => {
                "A queued sync intent cannot be classified, so it will never drain on its own."
            }
            Self::WatcherHealthy => "The filesystem watcher is running and current.",
            Self::WatcherRecoveryPending => {
                "The filesystem watcher is rebuilding its view. It recovers on its own."
            }
            Self::DaemonUnreachable => {
                "The Bowline daemon is not answering, so nothing is syncing on this device."
            }
            Self::RefVerified => "The workspace ref carries a signature this device could verify.",
            Self::RefSignatureUnverifiable => {
                "The workspace ref carries a signature this device cannot verify, so sync will \
                 not act on it."
            }
            Self::RefAbsent => "The workspace has no ref yet. It appears after the first sync.",
            Self::ControlPlaneUnreachable => {
                "Bowline's hosted service is unreachable, so remote checks could not run. Local \
                 work is unaffected."
            }
            Self::ObjectPresent => "Every sampled object referenced by the ref exists.",
            Self::MetadataMissing => {
                "An object referenced by the ref has no metadata record, so it cannot be \
                 hydrated."
            }
            Self::ObjectMissing => {
                "An object referenced by the ref is absent from storage, so files that need it \
                 cannot be hydrated."
            }
            Self::SampleVerified => "Sampled sealed content matched its content id.",
            Self::ContentIdMismatch => {
                "Sampled sealed content did not match its content id. Bowline refuses to \
                 materialize it."
            }
            Self::SealVerificationUnavailable => {
                "Seal verification could not run on this device, so sampled content is \
                 unchecked."
            }
            Self::SampleEmpty => "There was no sealed content to sample yet.",
            Self::KeyAvailable => "This device holds the workspace key.",
            Self::KeyUnavailable => {
                "This device has not received its workspace key, so nothing here can be \
                 decrypted."
            }
            Self::EpochMismatch => {
                "This device holds an older workspace key than the workspace is sealed with."
            }
            Self::RetryNominal => "Retrying work is draining normally.",
            Self::RetryStale => {
                "Work has been retrying for longer than expected, usually because the hosted \
                 service or network is unreachable."
            }
            Self::NoCollisions => "No path in this workspace collides across platforms.",
            Self::PortablePathCollision => {
                "Two paths in this workspace differ only in ways some filesystems ignore, so \
                 they cannot both exist on every device."
            }
            Self::CapacitySufficient => "There is enough free space for sync to stage writes.",
            Self::CapacityInsufficient => {
                "There is not enough free space for sync to stage writes safely."
            }
            Self::StateRootUnavailable => {
                "Bowline's state directory is unreadable, so filesystem capability checks could \
                 not run."
            }
            Self::RenameSupported => "The filesystem supports the atomic renames sync relies on.",
            Self::RenameUnsupported => {
                "The filesystem does not support atomic renames, so Bowline cannot write \
                 safely here."
            }
            Self::IdentityMatched => "The running binary matches the deployment it reports.",
            Self::IdentityMismatched => {
                "The running binary does not match the deployment it reports, usually a partly \
                 applied update."
            }
            Self::IdentityUnknown => "The running binary's deployment identity could not be read.",
            Self::HashComputed => "The installed candidate binary hashed successfully.",
            Self::HashUnavailable => "The installed candidate binary could not be hashed.",
        }
    }

    /// The command that repairs this reason, if one exists.
    ///
    /// A compiler-checked match rather than a lookup table: the reason set is
    /// closed, so a new reason cannot ship without answering "and what do I tell
    /// the user to run?". `None` means no single command fixes it — the caller
    /// falls back to collecting diagnostics.
    pub const fn repair_command(self) -> Option<&'static str> {
        match self {
            Self::IntegrityFailed => Some("bowline daemon restart"),
            Self::DaemonUnreachable | Self::WatcherRecoveryPending => Some("bowline daemon start"),
            Self::KeyUnavailable => Some("bowline device approve"),
            Self::EpochMismatch => Some("bowline device rotate"),
            Self::ControlPlaneUnreachable | Self::RetryStale => Some("bowline sync retry"),
            Self::RefRegressedBelowVerified
            | Self::IntentUnclassifiable
            | Self::MetadataMissing
            | Self::ObjectMissing
            | Self::ContentIdMismatch
            | Self::RefSignatureUnverifiable
            | Self::PortablePathCollision
            | Self::CapacityInsufficient
            | Self::RenameUnsupported
            | Self::StateRootUnavailable
            | Self::SealVerificationUnavailable
            | Self::IdentityMismatched
            | Self::IdentityUnknown
            | Self::HashUnavailable
            | Self::AncestorMissing
            | Self::EngineDatabaseMissing => None,
            Self::IntegrityVerified
            | Self::AncestorConsistent
            | Self::IntentsRecoverable
            | Self::WatcherHealthy
            | Self::RefVerified
            | Self::RefAbsent
            | Self::ObjectPresent
            | Self::SampleVerified
            | Self::SampleEmpty
            | Self::KeyAvailable
            | Self::RetryNominal
            | Self::NoCollisions
            | Self::CapacitySufficient
            | Self::RenameSupported
            | Self::IdentityMatched
            | Self::HashComputed => None,
        }
    }
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
    /// What to do about the checks that failed.
    ///
    /// `bowline doctor` exits `UserActionRequired` whenever anything failed, so
    /// it must name the action. This is derived from the failed checks, never
    /// hand-assembled by a caller.
    pub next_actions: Vec<RepairCommand>,
}

impl DoctorCommandOutput {
    /// The repair affordances for a check set, in check order and deduplicated.
    ///
    /// Only `Failed` checks produce an action: degraded and unavailable rungs are
    /// self-healing or informational and never ask for a human. A failed check
    /// with no known repair still produces one — pointing at diagnostics — so the
    /// exit code never asks for an action the output does not name.
    pub fn repair_actions(checks: &[DoctorCheck]) -> Vec<RepairCommand> {
        let mut actions: Vec<RepairCommand> = Vec::new();
        for check in checks {
            if check.status != DoctorCheckStatus::Failed {
                continue;
            }
            let action = match check.reason.repair_command() {
                Some(command) => {
                    RepairCommand::mutating(check.reason.explanation(), Some(command.to_string()))
                }
                None => RepairCommand::inspect(
                    check.reason.explanation(),
                    Some(DIAGNOSTICS_FALLBACK_COMMAND.to_string()),
                ),
            };
            if !actions.contains(&action) {
                actions.push(action);
            }
        }
        actions
    }
}

/// Where a failed check with no single repair sends the user.
const DIAGNOSTICS_FALLBACK_COMMAND: &str = "bowline diagnostics collect";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_failed_check_names_an_action_the_user_can_run() {
        // `bowline doctor` exits UserActionRequired whenever anything failed, so
        // there must be no reason that fails without naming a command.
        for reason in ALL_DOCTOR_REASONS {
            let checks = [DoctorCheck::new(
                DoctorCheckId::EngineSqliteIntegrity,
                DoctorCheckStatus::Failed,
                reason,
            )];
            let actions = DoctorCommandOutput::repair_actions(&checks);

            assert_eq!(actions.len(), 1, "{reason:?}");
            assert!(
                actions[0].command.is_some(),
                "{reason:?} failed with no command"
            );
            assert!(
                actions[0].label.ends_with('.'),
                "{reason:?} explanation must read as a sentence"
            );
        }
    }

    #[test]
    fn healthy_and_self_healing_checks_ask_for_nothing() {
        let checks = [
            DoctorCheck::new(
                DoctorCheckId::EngineSqliteIntegrity,
                DoctorCheckStatus::Ok,
                DoctorReason::IntegrityVerified,
            ),
            DoctorCheck::new(
                DoctorCheckId::WatcherHealth,
                DoctorCheckStatus::Degraded,
                DoctorReason::WatcherRecoveryPending,
            ),
            DoctorCheck::new(
                DoctorCheckId::RefFetchVerification,
                DoctorCheckStatus::Unavailable,
                DoctorReason::ControlPlaneUnreachable,
            ),
        ];

        assert!(DoctorCommandOutput::repair_actions(&checks).is_empty());
    }

    #[test]
    fn one_repair_is_offered_once_however_many_checks_report_it() {
        let checks = [
            DoctorCheck::new(
                DoctorCheckId::WatcherHealth,
                DoctorCheckStatus::Failed,
                DoctorReason::DaemonUnreachable,
            ),
            DoctorCheck::new(
                DoctorCheckId::RetryAge,
                DoctorCheckStatus::Failed,
                DoctorReason::DaemonUnreachable,
            ),
        ];

        assert_eq!(DoctorCommandOutput::repair_actions(&checks).len(), 1);
    }

    #[test]
    fn explanations_carry_no_path_shaped_detail() {
        for reason in ALL_DOCTOR_REASONS {
            let explanation = reason.explanation();
            assert!(!explanation.contains('/'), "{reason:?}");
            assert!(!explanation.contains('\\'), "{reason:?}");
        }
    }

    const ALL_DOCTOR_REASONS: [DoctorReason; 39] = [
        DoctorReason::IntegrityVerified,
        DoctorReason::IntegrityFailed,
        DoctorReason::EngineDatabaseMissing,
        DoctorReason::AncestorConsistent,
        DoctorReason::AncestorMissing,
        DoctorReason::RefRegressedBelowVerified,
        DoctorReason::IntentsRecoverable,
        DoctorReason::IntentUnclassifiable,
        DoctorReason::WatcherHealthy,
        DoctorReason::WatcherRecoveryPending,
        DoctorReason::DaemonUnreachable,
        DoctorReason::RefVerified,
        DoctorReason::RefSignatureUnverifiable,
        DoctorReason::RefAbsent,
        DoctorReason::ControlPlaneUnreachable,
        DoctorReason::ObjectPresent,
        DoctorReason::MetadataMissing,
        DoctorReason::ObjectMissing,
        DoctorReason::SampleVerified,
        DoctorReason::ContentIdMismatch,
        DoctorReason::SealVerificationUnavailable,
        DoctorReason::SampleEmpty,
        DoctorReason::KeyAvailable,
        DoctorReason::KeyUnavailable,
        DoctorReason::EpochMismatch,
        DoctorReason::RetryNominal,
        DoctorReason::RetryStale,
        DoctorReason::NoCollisions,
        DoctorReason::PortablePathCollision,
        DoctorReason::CapacitySufficient,
        DoctorReason::CapacityInsufficient,
        DoctorReason::StateRootUnavailable,
        DoctorReason::RenameSupported,
        DoctorReason::RenameUnsupported,
        DoctorReason::IdentityMatched,
        DoctorReason::IdentityMismatched,
        DoctorReason::IdentityUnknown,
        DoctorReason::HashComputed,
        DoctorReason::HashUnavailable,
    ];
}
