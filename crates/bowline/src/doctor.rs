//! `bowline doctor --engine manifest --json` — read-only engine diagnostics
//! (Plan 111 Step 6).
//!
//! Doctor never mutates workspace, engine, or hosted state: it opens the engine
//! SQLite read-only, probes the control plane and daemon with plain reads, and
//! runs a self-cleaning temp-rename probe in a scratch directory it also removes.
//! Every handle is resolved once into [`DoctorContext`] so each check is a pure
//! function of already-gathered facts; an absent handle is represented honestly
//! (`Unreachable`/`None`), never fabricated.
//!
//! REDACTION: the output type ([`DoctorCommandOutput`]) carries only typed enums,
//! counts, timestamps, a safe workspace ID, and opaque hex/keys. No check here
//! puts a path, filename, plaintext hash, or serialized crypto error into the
//! output — the `opaque` detail is only ever an opaque digest or object key. The
//! `doctor_output_never_contains_workspace_paths` test enforces this.
//!
//! PRODUCT SURFACE: the plain-language "sync needs attention" notice the plan
//! asks for is already produced by the status projection, not duplicated here.
//! `status_projection::engine_status` maps `Degradation::IntegrityStalled` to the
//! `Limited` rung (`StatusAttention::Required`, summary "…needs attention…") and
//! `OfflineRetrying` to `Recovering`, and it clears itself back to `Ready` when
//! the engine returns to `Nominal`. Doctor is diagnostics only, never required.

use super::*;

use bowline_local::sync::manifest_engine::store::ManifestStore;

mod checks;

const ENGINE_DB_FILE: &str = "manifest_engine.sqlite3";

/// Read-only diagnostic context. Every field is gathered once; checks read it.
pub(super) struct DoctorContext {
    workspace_id: WorkspaceId,
    state_root: Option<PathBuf>,
    store: Option<ManifestStore>,
    engine_db_present: bool,
    ref_probe: checks::RefProbe,
    daemon: Option<StatusCommandOutput>,
    convex_url: Option<String>,
    key_probe: checks::KeyProbe,
}

pub(super) fn run_doctor(args: DoctorArgs, json: bool, socket: &Path) -> ExitCode {
    let DoctorArgs { engine } = args;
    let ctx = DoctorContext::resolve(socket);
    let checks = ctx.run_all_checks();
    let summary = DoctorSummary::tally(&checks);
    // A failed check is the only rung that asks for a human; degraded/unavailable
    // are self-healing or informational, so doctor stays a diagnostic, not a gate.
    let exit = if summary.attention_required {
        CommandExitCode::UserActionRequired
    } else {
        CommandExitCode::Success
    };
    let output = DoctorCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::Doctor,
        generated_at: generated_at(),
        engine,
        workspace_id: ctx.workspace_id.clone(),
        summary,
        checks,
    };
    if json {
        print_json(&output);
    } else {
        print_human(&output);
    }
    exit.into()
}

impl DoctorContext {
    fn resolve(socket: &Path) -> Self {
        let workspace_id = crate::runtime::active_workspace_id();
        let state_root = crate::runtime::selected_metadata_database_path()
            .and_then(|path| path.parent().map(Path::to_path_buf));
        let engine_db = state_root.as_ref().map(|root| root.join(ENGINE_DB_FILE));
        let engine_db_present = engine_db.as_ref().is_some_and(|path| path.exists());
        let store = engine_db
            .as_ref()
            .filter(|path| path.exists())
            .and_then(|path| ManifestStore::open_read_only(path).ok());
        Self {
            ref_probe: checks::probe_ref(&workspace_id),
            daemon: crate::wire::daemon_status_snapshot(socket),
            convex_url: crate::runtime::hosted_convex_url(),
            key_probe: checks::probe_key(&workspace_id),
            workspace_id,
            state_root,
            store,
            engine_db_present,
        }
    }

    fn run_all_checks(&self) -> Vec<DoctorCheck> {
        DoctorCheckId::ALL
            .iter()
            .map(|id| self.run_check(*id))
            .collect()
    }

    fn run_check(&self, id: DoctorCheckId) -> DoctorCheck {
        match id {
            DoctorCheckId::EngineSqliteIntegrity => checks::engine_sqlite_integrity(self),
            DoctorCheckId::AncestorRefConsistency => checks::ancestor_ref_consistency(self),
            DoctorCheckId::IntentRecoverability => checks::intent_recoverability(self),
            DoctorCheckId::WatcherHealth => checks::watcher_health(self),
            DoctorCheckId::RefFetchVerification => checks::ref_fetch_verification(self),
            DoctorCheckId::RefMetadataObjectExistence => {
                checks::ref_metadata_object_existence(self)
            }
            DoctorCheckId::SealedContentIdVerification => {
                checks::sealed_content_id_verification(self)
            }
            DoctorCheckId::WorkspaceKeyAvailability => checks::workspace_key_availability(self),
            DoctorCheckId::RetryAge => checks::retry_age(self),
            DoctorCheckId::PortablePathCollisions => checks::portable_path_collisions(self),
            DoctorCheckId::TempCapacity => checks::temp_capacity(self),
            DoctorCheckId::AtomicRenameCapability => checks::atomic_rename_capability(self),
            DoctorCheckId::DeploymentIdentity => checks::deployment_identity(self),
            DoctorCheckId::InstalledCandidateHash => checks::installed_candidate_hash(self),
        }
    }

    /// The engine store opened read-only, if the database exists. Store-backed
    /// checks that find `None` here report `engine-database-missing`.
    pub(super) fn store(&self) -> Option<&ManifestStore> {
        self.store.as_ref()
    }

    pub(super) fn engine_db_present(&self) -> bool {
        self.engine_db_present
    }

    pub(super) fn state_root(&self) -> Option<&Path> {
        self.state_root.as_deref()
    }

    pub(in crate::doctor) fn ref_probe(&self) -> &checks::RefProbe {
        &self.ref_probe
    }

    pub(super) fn daemon(&self) -> Option<&StatusCommandOutput> {
        self.daemon.as_ref()
    }

    pub(super) fn convex_url(&self) -> Option<&str> {
        self.convex_url.as_deref()
    }

    pub(in crate::doctor) fn key_probe(&self) -> &checks::KeyProbe {
        &self.key_probe
    }
}

fn print_human(output: &DoctorCommandOutput) {
    println!(
        "bowline doctor: {} ok, {} degraded, {} unavailable, {} failed",
        output.summary.ok,
        output.summary.degraded,
        output.summary.unavailable,
        output.summary.failed,
    );
    for check in &output.checks {
        let detail = match (check.count, check.opaque.as_deref()) {
            (Some(count), Some(opaque)) => format!(" count={count} id={opaque}"),
            (Some(count), None) => format!(" count={count}"),
            (None, Some(opaque)) => format!(" id={opaque}"),
            (None, None) => String::new(),
        };
        println!(
            "  {} {} {}{detail}",
            serde_plain(&check.id),
            serde_plain(&check.status),
            serde_plain(&check.reason),
        );
    }
}

/// Renders a serde enum to its wire token for human output without hand-writing a
/// parallel label table (house rule: never maintain a twin of a derivable map).
fn serde_plain(value: &impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
