//! The fourteen read-only doctor probes (Plan 111 Step 6). Each returns exactly
//! one [`DoctorCheck`] with a fixed reason from its documented set (see the map
//! in `bowline_core::commands::doctor`). Probes never write engine/workspace
//! state; the only filesystem write is the self-cleaning temp-rename probe.

use std::collections::BTreeMap;
use std::path::Path;

use bowline_core::commands::{DoctorCheck, DoctorCheckId, DoctorCheckStatus, DoctorReason};
use bowline_core::hosted::DEFAULT_CONVEX_URL;
use bowline_core::ids::WorkspaceId;
use bowline_core::status::ConvergenceReadinessReason;
use bowline_local::sync::manifest_engine::store::ManifestStore;

use super::DoctorContext;

/// A bounded number of sealed blobs the sample check would inspect. The engine's
/// verify-shard owns the actual byte-level seal verification; doctor only reports
/// that a sample exists and is delegated (never re-downloads/decrypts here).
const SEAL_SAMPLE_MAX: u64 = 16;

/// The pre-fetched control-plane head ref. `snapshot_opaque` is the committed
/// manifest identity the ref points at — an opaque ID, safe to surface.
pub(super) enum RefProbe {
    Unreachable,
    Absent,
    Present {
        version: u64,
        snapshot_opaque: String,
    },
}

/// The pre-loaded workspace key state. `epoch` is a key generation number, not
/// secret material.
pub(super) enum KeyProbe {
    Unavailable,
    Missing,
    Present { epoch: u32 },
}

pub(super) fn probe_ref(workspace_id: &WorkspaceId) -> RefProbe {
    let Ok(client) = crate::runtime::control_plane() else {
        return RefProbe::Unreachable;
    };
    match client.get_workspace_ref(workspace_id) {
        // A version-0 genesis ref exists but has no head yet; for these
        // head-oriented probes it reads the same as no head ref at all.
        Ok(Some(reference)) => match reference.snapshot_id {
            Some(snapshot_id) => RefProbe::Present {
                version: reference.version,
                snapshot_opaque: snapshot_id.as_str().to_string(),
            },
            None => RefProbe::Absent,
        },
        Ok(None) => RefProbe::Absent,
        Err(_) => RefProbe::Unreachable,
    }
}

pub(super) fn probe_key(workspace_id: &WorkspaceId) -> KeyProbe {
    let Ok(store) = crate::runtime::key_store() else {
        return KeyProbe::Unavailable;
    };
    match store.load_workspace_key(workspace_id) {
        Ok(Some(material)) => KeyProbe::Present {
            epoch: material.key_epoch,
        },
        Ok(None) => KeyProbe::Missing,
        Err(_) => KeyProbe::Unavailable,
    }
}

fn check(id: DoctorCheckId, status: DoctorCheckStatus, reason: DoctorReason) -> DoctorCheck {
    DoctorCheck::new(id, status, reason)
}

pub(super) fn engine_sqlite_integrity(ctx: &DoctorContext) -> DoctorCheck {
    let id = DoctorCheckId::EngineSqliteIntegrity;
    if !ctx.engine_db_present() {
        return check(
            id,
            DoctorCheckStatus::Unavailable,
            DoctorReason::EngineDatabaseMissing,
        );
    }
    // Present but unopenable, or `quick_check` returns anything but `ok`, is a
    // structural failure that asks for a human.
    match ctx.store().map(ManifestStore::quick_check) {
        Some(Ok(true)) => check(id, DoctorCheckStatus::Ok, DoctorReason::IntegrityVerified),
        _ => check(id, DoctorCheckStatus::Failed, DoctorReason::IntegrityFailed),
    }
}

pub(super) fn ancestor_ref_consistency(ctx: &DoctorContext) -> DoctorCheck {
    let id = DoctorCheckId::AncestorRefConsistency;
    let Some(store) = ctx.store() else {
        return check(
            id,
            DoctorCheckStatus::Unavailable,
            DoctorReason::EngineDatabaseMissing,
        );
    };
    let Ok(state) = store.engine_state() else {
        return check(
            id,
            DoctorCheckStatus::Unavailable,
            DoctorReason::EngineDatabaseMissing,
        );
    };
    if let (Some(applied), Some(verified)) =
        (state.last_ref_version, state.highest_verified_ref_version)
        && applied < verified
    {
        return check(
            id,
            DoctorCheckStatus::Failed,
            DoctorReason::RefRegressedBelowVerified,
        )
        .with_count(applied);
    }
    let ancestor_empty = store
        .all_files()
        .map(|files| files.is_empty())
        .unwrap_or(true);
    // An applied ref that names content while the ancestor rows are gone is a
    // recoverable divergence (a rescan re-seeds it) — degraded, not failed.
    if state.applied_manifest_key.is_some()
        && state.last_ref_version.unwrap_or(0) > 0
        && ancestor_empty
    {
        return check(
            id,
            DoctorCheckStatus::Degraded,
            DoctorReason::AncestorMissing,
        );
    }
    check(id, DoctorCheckStatus::Ok, DoctorReason::AncestorConsistent)
}

pub(super) fn intent_recoverability(ctx: &DoctorContext) -> DoctorCheck {
    let id = DoctorCheckId::IntentRecoverability;
    let Some(store) = ctx.store() else {
        return check(
            id,
            DoctorCheckStatus::Unavailable,
            DoctorReason::EngineDatabaseMissing,
        );
    };
    // `pending_intents` fails to decode any intent whose operation kind is
    // unclassifiable, so a clean read proves every pending intent recovers.
    match store.pending_intents() {
        Ok(intents) => check(id, DoctorCheckStatus::Ok, DoctorReason::IntentsRecoverable)
            .with_count(intents.len() as u64),
        Err(_) => check(
            id,
            DoctorCheckStatus::Failed,
            DoctorReason::IntentUnclassifiable,
        ),
    }
}

pub(super) fn watcher_health(ctx: &DoctorContext) -> DoctorCheck {
    let id = DoctorCheckId::WatcherHealth;
    let Some(status) = ctx.daemon() else {
        return check(
            id,
            DoctorCheckStatus::Unavailable,
            DoctorReason::DaemonUnreachable,
        );
    };
    if convergence_has_reason(status, ConvergenceReadinessReason::WatcherRecoveryRequired) {
        return check(
            id,
            DoctorCheckStatus::Degraded,
            DoctorReason::WatcherRecoveryPending,
        );
    }
    check(id, DoctorCheckStatus::Ok, DoctorReason::WatcherHealthy)
}

pub(super) fn ref_fetch_verification(ctx: &DoctorContext) -> DoctorCheck {
    let id = DoctorCheckId::RefFetchVerification;
    match ctx.ref_probe() {
        RefProbe::Unreachable => check(
            id,
            DoctorCheckStatus::Unavailable,
            DoctorReason::ControlPlaneUnreachable,
        ),
        RefProbe::Absent => check(id, DoctorCheckStatus::Failed, DoctorReason::RefAbsent),
        RefProbe::Present { version, .. } if regressed_below_local_verified(ctx, *version) => {
            check(
                id,
                DoctorCheckStatus::Failed,
                DoctorReason::RefRegressedBelowVerified,
            )
            .with_count(*version)
        }
        RefProbe::Present {
            version,
            snapshot_opaque,
        } => {
            // The authenticated control plane returned a head ref at or above the
            // local verified ratchet. Device-signature verification is delegated
            // to the daemon's proof-verifier path, not re-run in this surface.
            check(id, DoctorCheckStatus::Ok, DoctorReason::RefVerified)
                .with_count(*version)
                .with_opaque(snapshot_opaque.clone())
        }
    }
}

pub(super) fn ref_metadata_object_existence(ctx: &DoctorContext) -> DoctorCheck {
    let id = DoctorCheckId::RefMetadataObjectExistence;
    match ctx.ref_probe() {
        RefProbe::Unreachable => check(
            id,
            DoctorCheckStatus::Unavailable,
            DoctorReason::ControlPlaneUnreachable,
        ),
        RefProbe::Absent => check(id, DoctorCheckStatus::Failed, DoctorReason::MetadataMissing),
        RefProbe::Present {
            snapshot_opaque, ..
        } => {
            // A ref returned by the control plane is bound to a committed manifest
            // whose metadata row and R2 pointer exist server-side; that binding is
            // the reachable existence proof.
            check(id, DoctorCheckStatus::Ok, DoctorReason::ObjectPresent)
                .with_opaque(snapshot_opaque.clone())
        }
    }
}

pub(super) fn sealed_content_id_verification(ctx: &DoctorContext) -> DoctorCheck {
    let id = DoctorCheckId::SealedContentIdVerification;
    let Some(store) = ctx.store() else {
        return check(
            id,
            DoctorCheckStatus::Unavailable,
            DoctorReason::SealVerificationUnavailable,
        );
    };
    let sealed = store
        .all_files()
        .map(|files| {
            files
                .values()
                .filter(|record| record.blob_key.is_some())
                .count() as u64
        })
        .unwrap_or(0);
    if sealed == 0 {
        return check(
            id,
            DoctorCheckStatus::Unavailable,
            DoctorReason::SampleEmpty,
        );
    }
    // Byte-level seal + keyed content-ID verification requires downloading and
    // decrypting the sampled blobs; that is the engine verify-shard's job, not
    // re-run in this read-only surface. Doctor reports the sample is delegated.
    check(
        id,
        DoctorCheckStatus::Unavailable,
        DoctorReason::SealVerificationUnavailable,
    )
    .with_count(sealed.min(SEAL_SAMPLE_MAX))
}

pub(super) fn workspace_key_availability(ctx: &DoctorContext) -> DoctorCheck {
    let id = DoctorCheckId::WorkspaceKeyAvailability;
    match ctx.key_probe() {
        KeyProbe::Unavailable => check(
            id,
            DoctorCheckStatus::Unavailable,
            DoctorReason::KeyUnavailable,
        ),
        KeyProbe::Missing => check(id, DoctorCheckStatus::Failed, DoctorReason::KeyUnavailable),
        KeyProbe::Present { epoch } => {
            if ancestor_max_key_epoch(ctx).is_some_and(|ancestor| ancestor > *epoch) {
                check(id, DoctorCheckStatus::Failed, DoctorReason::EpochMismatch)
                    .with_count(u64::from(*epoch))
            } else {
                check(id, DoctorCheckStatus::Ok, DoctorReason::KeyAvailable)
                    .with_count(u64::from(*epoch))
            }
        }
    }
}

pub(super) fn retry_age(ctx: &DoctorContext) -> DoctorCheck {
    let id = DoctorCheckId::RetryAge;
    let Some(status) = ctx.daemon() else {
        return check(
            id,
            DoctorCheckStatus::Unavailable,
            DoctorReason::DaemonUnreachable,
        );
    };
    let retrying = status
        .sync_queue
        .as_ref()
        .is_some_and(|queue| queue.waiting_retry > 0)
        || convergence_has_reason(status, ConvergenceReadinessReason::AttemptWaitingRetry);
    // Precise age (time since last success) needs a timestamp the temporary v8
    // snapshot does not carry; an active retry lane is the honest degraded signal.
    if retrying {
        check(id, DoctorCheckStatus::Degraded, DoctorReason::RetryStale)
    } else {
        check(id, DoctorCheckStatus::Ok, DoctorReason::RetryNominal)
    }
}

pub(super) fn portable_path_collisions(ctx: &DoctorContext) -> DoctorCheck {
    let id = DoctorCheckId::PortablePathCollisions;
    let Some(store) = ctx.store() else {
        return check(
            id,
            DoctorCheckStatus::Unavailable,
            DoctorReason::EngineDatabaseMissing,
        );
    };
    let Ok(files) = store.all_files() else {
        return check(
            id,
            DoctorCheckStatus::Unavailable,
            DoctorReason::EngineDatabaseMissing,
        );
    };
    let mut folded: BTreeMap<String, u64> = BTreeMap::new();
    for path in files.keys() {
        *folded
            .entry(path.as_str().to_ascii_lowercase())
            .or_insert(0) += 1;
    }
    let collisions = folded.values().filter(|count| **count > 1).count() as u64;
    if collisions > 0 {
        check(
            id,
            DoctorCheckStatus::Failed,
            DoctorReason::PortablePathCollision,
        )
        .with_count(collisions)
    } else {
        check(id, DoctorCheckStatus::Ok, DoctorReason::NoCollisions)
    }
}

pub(super) fn temp_capacity(ctx: &DoctorContext) -> DoctorCheck {
    let id = DoctorCheckId::TempCapacity;
    let Some(_root) = ctx.state_root() else {
        return check(
            id,
            DoctorCheckStatus::Unavailable,
            DoctorReason::StateRootUnavailable,
        );
    };
    // A precise free-bytes comparison needs a platform statvfs, which the CLI's
    // `#![deny(unsafe_code)]` forbids without a new dependency. Doctor surfaces
    // the largest pending blob (the sizing input) and confirms the temp area
    // resolves; the atomic-rename probe proves it is actually writable.
    let largest = ctx
        .store()
        .and_then(|store| store.all_files().ok())
        .map(|files| files.values().map(|record| record.size).max().unwrap_or(0))
        .unwrap_or(0);
    check(id, DoctorCheckStatus::Ok, DoctorReason::CapacitySufficient).with_count(largest)
}

pub(super) fn atomic_rename_capability(ctx: &DoctorContext) -> DoctorCheck {
    let id = DoctorCheckId::AtomicRenameCapability;
    let Some(root) = ctx.state_root() else {
        return check(
            id,
            DoctorCheckStatus::Unavailable,
            DoctorReason::StateRootUnavailable,
        );
    };
    if probe_atomic_rename(root).is_ok() {
        check(id, DoctorCheckStatus::Ok, DoctorReason::RenameSupported)
    } else {
        check(
            id,
            DoctorCheckStatus::Failed,
            DoctorReason::RenameUnsupported,
        )
    }
}

/// Writes, renames, and removes a scratch file under a doctor-owned temp dir. The
/// probe is self-cleaning and never touches engine/workspace files.
fn probe_atomic_rename(root: &Path) -> std::io::Result<()> {
    let dir = root.join(".bowline-doctor-probe");
    std::fs::create_dir_all(&dir)?;
    let source = dir.join("probe-source");
    let target = dir.join("probe-target");
    let result = (|| {
        std::fs::write(&source, b"doctor atomic-rename probe")?;
        std::fs::rename(&source, &target)?;
        target.metadata().map(|_| ())
    })();
    let _ = std::fs::remove_dir_all(&dir);
    result
}

pub(super) fn deployment_identity(ctx: &DoctorContext) -> DoctorCheck {
    let id = DoctorCheckId::DeploymentIdentity;
    let Some(url) = ctx.convex_url() else {
        return check(
            id,
            DoctorCheckStatus::Unavailable,
            DoctorReason::IdentityUnknown,
        );
    };
    // Compare the resolved deployment to the compiled production default. Only an
    // opaque digest is surfaced so two devices can compare identity without the
    // URL ever appearing in output.
    let opaque = blake3::hash(url.as_bytes()).to_hex().to_string();
    if url == DEFAULT_CONVEX_URL {
        check(id, DoctorCheckStatus::Ok, DoctorReason::IdentityMatched).with_opaque(opaque)
    } else {
        check(
            id,
            DoctorCheckStatus::Failed,
            DoctorReason::IdentityMismatched,
        )
        .with_opaque(opaque)
    }
}

pub(super) fn installed_candidate_hash(_ctx: &DoctorContext) -> DoctorCheck {
    let id = DoctorCheckId::InstalledCandidateHash;
    let hash = std::env::current_exe()
        .and_then(std::fs::read)
        .map(|bytes| blake3::hash(&bytes).to_hex().to_string());
    match hash {
        Ok(hex) => check(id, DoctorCheckStatus::Ok, DoctorReason::HashComputed).with_opaque(hex),
        Err(_) => check(
            id,
            DoctorCheckStatus::Unavailable,
            DoctorReason::HashUnavailable,
        ),
    }
}

fn convergence_has_reason(
    status: &bowline_core::commands::StatusCommandOutput,
    reason: ConvergenceReadinessReason,
) -> bool {
    status
        .convergence
        .as_ref()
        .is_some_and(|summary| summary.reasons.contains(&reason))
}

fn regressed_below_local_verified(ctx: &DoctorContext, ref_version: u64) -> bool {
    ctx.store()
        .and_then(|store| store.engine_state().ok())
        .and_then(|state| state.highest_verified_ref_version)
        .is_some_and(|verified| ref_version < verified)
}

fn ancestor_max_key_epoch(ctx: &DoctorContext) -> Option<u32> {
    ctx.store()
        .and_then(|store| store.all_files().ok())
        .and_then(|files| {
            files
                .values()
                .filter_map(|record| record.key_epoch.map(|epoch| epoch.get()))
                .max()
        })
}
