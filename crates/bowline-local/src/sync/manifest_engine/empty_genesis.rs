//! Canonical fixtures for a workspace with no remote history: a transport whose
//! ref is absent (the engine pulls nothing, pushes land nowhere durable, and an
//! empty workspace settles into `Idle`), plus the engine construction that goes
//! with it. Lives behind the `test-support` feature rather than `#[cfg(test)]`
//! because two consumers need one copy (second copy = extract): this crate's
//! dependents drive a real engine loop in their tests — the manifest driver's
//! and the daemon watcher-recovery bridge's — and a `#[cfg(test)]` item cannot
//! cross a crate boundary.

use std::path::PathBuf;

use bowline_core::ids::{DeviceId, WorkspaceId};

use super::{
    BlobKey, BlobReaderUpload, BlobUpload, CasOutcome, EngineConfig, EngineContext, EngineCounters,
    EngineProcessIdentity, KeyEpoch, ManifestEngine, ManifestKey, ManifestStore, ManifestUpload,
    RefObservation, RemoteObjects, RemoteRef, TransportError, WorkspaceCrypto, probe_name_folding,
    probe_timestamp_granularity,
};

/// A transport for an empty genesis workspace: the ref is absent, so the engine
/// pulls nothing, has no remote blobs to fetch, and settles into `Idle`.
pub struct EmptyGenesisTransport;

impl RemoteObjects for EmptyGenesisTransport {
    fn put_blob(&self, _upload: BlobUpload<'_>) -> Result<(), TransportError> {
        Ok(())
    }
    fn put_blob_reader(&self, _upload: BlobReaderUpload<'_>) -> Result<(), TransportError> {
        Ok(())
    }
    fn put_manifest(&self, _upload: ManifestUpload<'_>) -> Result<(), TransportError> {
        Ok(())
    }
    fn get_blob(&self, _key: &BlobKey) -> Result<Vec<u8>, TransportError> {
        Err(TransportError::new(
            "get-blob",
            "empty transport".to_string(),
        ))
    }
    fn get_manifest(&self, _key: &ManifestKey) -> Result<Vec<u8>, TransportError> {
        Err(TransportError::new(
            "get-manifest",
            "empty transport".to_string(),
        ))
    }
}

impl RemoteRef for EmptyGenesisTransport {
    fn read_ref(&self) -> Result<Option<RefObservation>, TransportError> {
        Ok(None)
    }
    fn compare_and_swap(
        &self,
        _expected_version: Option<u64>,
        _new_manifest_key: &ManifestKey,
    ) -> Result<CasOutcome, TransportError> {
        Ok(CasOutcome::Ambiguous)
    }
}

/// A deterministic engine context for genesis-workspace tests: fixed crypto and
/// device identity, engine state under the workspace root's private state dir.
pub fn empty_genesis_engine_context(root: PathBuf, workspace_id: &str) -> EngineContext {
    let engine_state_dir = root.join(super::ENGINE_STATE_DIR);
    EngineContext {
        process_identity: EngineProcessIdentity::current(),
        workspace_identity: WorkspaceId::new(workspace_id),
        crypto: WorkspaceCrypto::new(workspace_id, [7_u8; 32], KeyEpoch::new(1)),
        device_id: DeviceId::new("device-a"),
        names: probe_name_folding(&engine_state_dir),
        timestamps: probe_timestamp_granularity(&engine_state_dir),
        engine_state_dir: engine_state_dir.clone(),
        endpoint_probe_root: engine_state_dir,
        workspace_root: root,
        config: EngineConfig::default(),
        project_view: false,
        counters: EngineCounters::shared(),
    }
}

/// A real engine over `root` with a fresh store at `store_path`, ready to run
/// against [`EmptyGenesisTransport`].
pub fn empty_genesis_engine(
    root: PathBuf,
    store_path: PathBuf,
    workspace_id: &str,
) -> ManifestEngine {
    let store = ManifestStore::open(store_path).expect("empty genesis engine store opens");
    ManifestEngine::new(store, empty_genesis_engine_context(root, workspace_id))
}
