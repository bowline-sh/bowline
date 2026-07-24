//! Shared in-crate test doubles for the manifest-sync engine (Plan 109 Steps
//! 4–5). One extracted harness rather than a copy per test file (AGENTS: second
//! copy = extract). Extends the `FakeControlPlaneClient` pattern from
//! `crates/bowline-control-plane/src/transfer/tests.rs`, adding the
//! metadata-commit-before-reference behavior the buffered fake lacks.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use bowline_core::ids::{ContentId, DeviceId};

use super::fs_guard::{Observed, observe};
use super::manifest::{
    BlobKey, DecodeLimits, FileMode, KeyEpoch, Manifest, ManifestEntry, ManifestKey,
    WorkspaceCrypto, WorkspacePath, open_manifest, physical_blob_key, physical_manifest_key,
    seal_file, seal_manifest,
};
use super::pull_apply::{PullDeps, PullError, PullOutcome, pull};
use super::push::{
    BlobReaderUpload, BlobUpload, CasOutcome, EngineConfig, EngineContext, ManifestUpload,
    PushDeps, PushOutcome, RefObservation, RemoteObjects, RemoteRef, TransportError, push,
};
use super::store::{FileRecord, ManifestStore};
use super::{Clock, EngineEvent, EngineIo, ManifestEngine};
use crate::workspace::TempWorkspace;

pub(crate) const KEY_BYTES: [u8; 32] = [9; 32];

/// A virtual clock tests advance by hand, so the debounce/backoff schedule runs
/// deterministically with no real sleeping. The system impl is [`super::SystemClock`].
pub(crate) struct TestClock {
    millis: Cell<u64>,
}

impl TestClock {
    pub(crate) fn new() -> Self {
        Self {
            millis: Cell::new(0),
        }
    }

    pub(crate) fn advance(&self, delta: u64) {
        self.millis.set(self.millis.get() + delta);
    }

    pub(crate) fn millis(&self) -> u64 {
        self.millis.get()
    }
}

impl Clock for TestClock {
    fn now_millis(&self) -> u64 {
        self.millis.get()
    }
}

/// Build the shared workspace crypto every test double uses. Two devices must
/// share the key/epoch/workspace id (only the device id differs) so each can open
/// the other's sealed blobs.
pub(crate) fn test_crypto() -> WorkspaceCrypto {
    WorkspaceCrypto::new("ws_code", KEY_BYTES, KeyEpoch::new(1))
}

pub(crate) fn test_context(root: PathBuf, device: &str) -> EngineContext {
    EngineContext {
        crypto: test_crypto(),
        device_id: DeviceId::new(device.to_string()),
        engine_state_dir: root.join(super::ENGINE_STATE_DIR),
        workspace_root: root,
        config: EngineConfig::default(),
        project_view: false,
        counters: super::EngineCounters::shared(),
    }
}

/// One recorded transport event, so tests can assert ordering (a blob's metadata
/// commit always precedes the manifest that references it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Event {
    PutBlob(String),
    PutBlobReader(String),
    GetBlob(String),
    PutManifest(String),
    GetManifest(String),
    Cas,
}

/// Injected CAS behavior for one push.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum CasMode {
    #[default]
    Normal,
    /// Mutate the ref but drop the ack (the `AckAmbiguous` path).
    AmbiguousAfterSwap,
    /// Fail the CAS transport before any swap (crash-before-CAS).
    FailBeforeSwap,
}

/// In-memory object store + CAS ref implementing both engine transport traits.
pub(crate) struct FakeRemote {
    blobs: RefCell<BTreeMap<String, Vec<u8>>>,
    manifests: RefCell<BTreeMap<String, Vec<u8>>>,
    reference: RefCell<Option<RefObservation>>,
    version: RefCell<u64>,
    events: RefCell<Vec<Event>>,
    cas_mode: RefCell<CasMode>,
    read_ref_count: Cell<u64>,
    /// When set, every transport call fails — the offline condition the driver's
    /// backoff loop is tested against.
    offline: Cell<bool>,
}

impl FakeRemote {
    pub(crate) fn new() -> Self {
        Self {
            blobs: RefCell::new(BTreeMap::new()),
            manifests: RefCell::new(BTreeMap::new()),
            reference: RefCell::new(None),
            version: RefCell::new(0),
            events: RefCell::new(Vec::new()),
            cas_mode: RefCell::new(CasMode::Normal),
            read_ref_count: Cell::new(0),
            offline: Cell::new(false),
        }
    }

    pub(crate) fn set_cas_mode(&self, mode: CasMode) {
        *self.cas_mode.borrow_mut() = mode;
    }

    /// Drive the network up (`false`) or down (`true`) for the backoff tests.
    pub(crate) fn set_offline(&self, offline: bool) {
        self.offline.set(offline);
    }

    /// Override the OBSERVED head (`read_ref`) without touching the CAS version
    /// counter, so a test can simulate a hosted rollback to a lower version or a
    /// forged high-version ref while the real CAS sequence continues from its own
    /// counter. Distinct from `publish_*`, which always advance monotonically.
    pub(crate) fn force_ref(&self, version: u64, manifest_key: ManifestKey) {
        *self.reference.borrow_mut() = Some(RefObservation {
            version,
            manifest_key,
        });
    }

    fn guard(&self, operation: &'static str) -> Result<(), TransportError> {
        if self.offline.get() {
            return Err(TransportError::new(operation, "simulated offline"));
        }
        Ok(())
    }

    pub(crate) fn events(&self) -> Vec<Event> {
        self.events.borrow().clone()
    }

    pub(crate) fn blob_put_count(&self) -> usize {
        self.events
            .borrow()
            .iter()
            .filter(|event| matches!(event, Event::PutBlob(_) | Event::PutBlobReader(_)))
            .count()
    }

    pub(crate) fn reader_put_count(&self) -> usize {
        self.events
            .borrow()
            .iter()
            .filter(|event| matches!(event, Event::PutBlobReader(_)))
            .count()
    }

    pub(crate) fn current_ref(&self) -> Option<RefObservation> {
        self.reference.borrow().clone()
    }

    pub(crate) fn read_ref_count(&self) -> u64 {
        self.read_ref_count.get()
    }

    /// Decode the manifest the head currently points at, for asserting on the
    /// exact entries a push produced (e.g. that a mode-only change preserved a
    /// file's content identity).
    pub(crate) fn decoded_manifest(&self, crypto: &WorkspaceCrypto) -> Option<Manifest> {
        let key = self.current_ref()?.manifest_key;
        let sealed = self.manifests.borrow().get(key.as_str()).cloned()?;
        let decoded = open_manifest(crypto, &sealed, &DecodeLimits::default())
            .expect("decode current manifest");
        Some(decoded.manifest)
    }

    /// A fresh remote holding a snapshot of this one's objects + head, so a peer
    /// `Harness` (which owns its own remote) can pull against the same state.
    pub(crate) fn clone_state(&self) -> FakeRemote {
        FakeRemote {
            blobs: RefCell::new(self.blobs.borrow().clone()),
            manifests: RefCell::new(self.manifests.borrow().clone()),
            reference: RefCell::new(self.reference.borrow().clone()),
            version: RefCell::new(*self.version.borrow()),
            events: RefCell::new(Vec::new()),
            cas_mode: RefCell::new(CasMode::Normal),
            read_ref_count: Cell::new(0),
            offline: Cell::new(false),
        }
    }

    /// Publish a remote manifest directly (a simulated peer), advancing the ref.
    pub(crate) fn publish_manifest(
        &self,
        crypto: &WorkspaceCrypto,
        manifest: &Manifest,
    ) -> ManifestKey {
        let plaintext = manifest.to_canonical_bytes().expect("canonical");
        let sealed = seal_manifest(crypto, &plaintext).expect("seal manifest");
        let key = physical_manifest_key(sealed.as_bytes());
        self.manifests
            .borrow_mut()
            .insert(key.as_str().to_string(), sealed.into_bytes());
        let mut version = self.version.borrow_mut();
        *version += 1;
        *self.reference.borrow_mut() = Some(RefObservation {
            version: *version,
            manifest_key: key.clone(),
        });
        key
    }

    /// Seal + store a file blob so a published manifest can reference it.
    pub(crate) fn publish_blob(&self, crypto: &WorkspaceCrypto, plaintext: &[u8]) -> ManifestEntry {
        let content_id = crypto.content_id(plaintext);
        let sealed = seal_file(crypto, &content_id, plaintext).expect("seal file");
        let key = physical_blob_key(sealed.as_bytes());
        self.blobs
            .borrow_mut()
            .insert(key.as_str().to_string(), sealed.into_bytes());
        ManifestEntry::File {
            size: plaintext.len() as u64,
            mode: FileMode::new(0o644),
            content_id,
            blob_key: key,
            key_epoch: crypto.key_epoch(),
        }
    }
}

impl RemoteObjects for FakeRemote {
    fn put_blob(&self, upload: BlobUpload<'_>) -> Result<(), TransportError> {
        self.guard("put_blob")?;
        self.events
            .borrow_mut()
            .push(Event::PutBlob(upload.key.as_str().to_string()));
        self.blobs
            .borrow_mut()
            .insert(upload.key.as_str().to_string(), upload.sealed.to_vec());
        Ok(())
    }

    fn put_blob_reader(&self, upload: BlobReaderUpload<'_>) -> Result<(), TransportError> {
        self.guard("put_blob_reader")?;
        self.events
            .borrow_mut()
            .push(Event::PutBlobReader(upload.key.as_str().to_string()));
        let bytes = fs::read(upload.spool_path)
            .map_err(|error| TransportError::new("put_blob_reader", error.to_string()))?;
        self.blobs
            .borrow_mut()
            .insert(upload.key.as_str().to_string(), bytes);
        Ok(())
    }

    fn put_manifest(&self, upload: ManifestUpload<'_>) -> Result<(), TransportError> {
        self.guard("put_manifest")?;
        self.events
            .borrow_mut()
            .push(Event::PutManifest(upload.key.as_str().to_string()));
        self.manifests
            .borrow_mut()
            .insert(upload.key.as_str().to_string(), upload.sealed.to_vec());
        Ok(())
    }

    fn get_blob(&self, key: &BlobKey) -> Result<Vec<u8>, TransportError> {
        self.guard("get_blob")?;
        self.events
            .borrow_mut()
            .push(Event::GetBlob(key.as_str().to_string()));
        self.blobs
            .borrow()
            .get(key.as_str())
            .cloned()
            .ok_or_else(|| TransportError::new("get_blob", "missing blob"))
    }

    fn get_manifest(&self, key: &ManifestKey) -> Result<Vec<u8>, TransportError> {
        self.guard("get_manifest")?;
        self.events
            .borrow_mut()
            .push(Event::GetManifest(key.as_str().to_string()));
        self.manifests
            .borrow()
            .get(key.as_str())
            .cloned()
            .ok_or_else(|| TransportError::new("get_manifest", "missing manifest"))
    }
}

impl RemoteRef for FakeRemote {
    fn read_ref(&self) -> Result<Option<RefObservation>, TransportError> {
        self.guard("read_ref")?;
        self.read_ref_count
            .set(self.read_ref_count.get().saturating_add(1));
        Ok(self.reference.borrow().clone())
    }

    fn compare_and_swap(
        &self,
        expected_version: Option<u64>,
        new_manifest_key: &ManifestKey,
    ) -> Result<CasOutcome, TransportError> {
        self.guard("compare_and_swap")?;
        self.events.borrow_mut().push(Event::Cas);
        let mode = *self.cas_mode.borrow();
        if mode == CasMode::FailBeforeSwap {
            return Err(TransportError::new("cas", "simulated crash before swap"));
        }
        let current = self.reference.borrow().clone();
        let current_version = current.as_ref().map(|observed| observed.version);
        if current_version != expected_version {
            return Ok(CasOutcome::Lost(
                current.expect("lost implies a current ref"),
            ));
        }
        let mut version = self.version.borrow_mut();
        *version += 1;
        let observed = RefObservation {
            version: *version,
            manifest_key: new_manifest_key.clone(),
        };
        *self.reference.borrow_mut() = Some(observed.clone());
        match mode {
            CasMode::AmbiguousAfterSwap => Ok(CasOutcome::Ambiguous),
            _ => Ok(CasOutcome::Advanced(observed)),
        }
    }
}

/// A self-contained engine under test: a temp workspace, its own store, crypto,
/// and a fake remote. Small-file thresholds are tuned so tests can exercise the
/// large-file spool path with tiny fixtures.
pub(crate) struct TestEngine {
    // Held so the temp workspace outlives the test (Drop cleans it up).
    _workspace: TempWorkspace,
    pub(crate) store: ManifestStore,
    pub(crate) ctx: EngineContext,
    pub(crate) remote: FakeRemote,
}

impl TestEngine {
    pub(crate) fn new(name: &str) -> Self {
        Self::with_config(name, EngineConfig::default())
    }

    pub(crate) fn with_config(name: &str, config: EngineConfig) -> Self {
        let workspace = TempWorkspace::new(name).expect("temp workspace");
        let root = workspace.root().to_path_buf();
        let store = ManifestStore::open(root.join("manifest_engine.sqlite3")).expect("open store");
        let ctx = EngineContext {
            crypto: WorkspaceCrypto::new("ws_code", KEY_BYTES, KeyEpoch::new(1)),
            device_id: DeviceId::new(format!("device-{name}")),
            engine_state_dir: root.join(super::ENGINE_STATE_DIR),
            workspace_root: root,
            config,
            project_view: false,
            counters: super::EngineCounters::shared(),
        };
        Self {
            _workspace: workspace,
            store,
            ctx,
            remote: FakeRemote::new(),
        }
    }

    pub(crate) fn root(&self) -> PathBuf {
        self.ctx.workspace_root.clone()
    }

    /// A point-in-time copy of the shared engine cost meters (Plan 111 Step 5).
    pub(crate) fn counters(&self) -> super::counters::CountersSnapshot {
        self.ctx.counters.snapshot()
    }

    pub(crate) fn write(&self, rel: &str, bytes: &[u8]) {
        let path = self.ctx.workspace_root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(&path, bytes).expect("write");
    }

    pub(crate) fn read(&self, rel: &str) -> Vec<u8> {
        fs::read(self.ctx.workspace_root.join(rel)).expect("read")
    }

    pub(crate) fn remove(&self, rel: &str) {
        fs::remove_file(self.ctx.workspace_root.join(rel)).expect("remove");
    }

    pub(crate) fn mode_bits(&self, rel: &str) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::symlink_metadata(self.ctx.workspace_root.join(rel))
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777
    }

    pub(crate) fn exists(&self, rel: &str) -> bool {
        self.ctx.workspace_root.join(rel).exists()
    }

    pub(crate) fn observe(&self, rel: &str) -> Option<Observed> {
        observe(&self.ctx.workspace_root, &WorkspacePath::new(rel)).expect("observe")
    }

    pub(crate) fn files(&self) -> BTreeMap<WorkspacePath, FileRecord> {
        self.store.all_files().expect("all files")
    }

    pub(crate) fn dirty(&self, paths: &[&str]) -> BTreeSet<WorkspacePath> {
        paths.iter().map(|path| WorkspacePath::new(*path)).collect()
    }

    pub(crate) fn push(&mut self, paths: &[&str]) -> PushOutcome {
        let dirty = self.dirty(paths);
        let deps = PushDeps {
            ctx: &self.ctx,
            objects: &self.remote,
            refs: &self.remote,
        };
        push(&mut self.store, &deps, &dirty).expect("push")
    }

    pub(crate) fn try_push(
        &mut self,
        paths: &[&str],
    ) -> Result<PushOutcome, super::push::PushError> {
        let dirty = self.dirty(paths);
        let deps = PushDeps {
            ctx: &self.ctx,
            objects: &self.remote,
            refs: &self.remote,
        };
        push(&mut self.store, &deps, &dirty)
    }

    pub(crate) fn pull(&mut self) -> PullOutcome {
        self.try_pull().expect("pull")
    }

    pub(crate) fn try_pull(&mut self) -> Result<PullOutcome, PullError> {
        let deps = PullDeps {
            ctx: &self.ctx,
            objects: &self.remote,
            refs: &self.remote,
        };
        pull(&mut self.store, &deps)
    }

    /// Reopen the engine store against the SAME database file — a restart. The
    /// durable ratchet and applied state must survive it.
    pub(crate) fn reopen_store(&mut self) {
        self.store = ManifestStore::open(self.ctx.workspace_root.join("manifest_engine.sqlite3"))
            .expect("reopen store");
    }

    /// Publish a remote head from `(path, entry)` pairs, advancing the ref.
    pub(crate) fn publish(&self, entries: &[(&str, ManifestEntry)]) -> ManifestKey {
        let map: BTreeMap<WorkspacePath, ManifestEntry> = entries
            .iter()
            .map(|(path, entry)| (WorkspacePath::new(*path), entry.clone()))
            .collect();
        let manifest = Manifest::new(self.ctx.crypto.key_epoch(), map);
        self.remote.publish_manifest(&self.ctx.crypto, &manifest)
    }

    pub(crate) fn remote_file(&self, plaintext: &[u8]) -> ManifestEntry {
        self.remote.publish_blob(&self.ctx.crypto, plaintext)
    }

    pub(crate) fn content_id(&self, plaintext: &[u8]) -> ContentId {
        self.ctx.crypto.content_id(plaintext)
    }
}

/// Open (creating) an engine store at `<root>/manifest_engine.sqlite3`.
pub(crate) fn open_store(root: &Path) -> ManifestStore {
    ManifestStore::open(root.join("manifest_engine.sqlite3")).expect("open store")
}

/// Open the engine store under `<root>/.bowline/` — private engine state the stat
/// walker skips — so a driver full scan never treats its own database as a
/// syncable workspace file (in production the state root lives here too).
pub(crate) fn open_engine_store(root: &Path) -> ManifestStore {
    let dir = root.join(".bowline");
    fs::create_dir_all(&dir).expect("engine state dir");
    ManifestStore::open(dir.join("manifest_engine.sqlite3")).expect("open engine store")
}

/// A disk-backed object store + CAS ref. Unlike [`FakeRemote`] (in-memory), this
/// persists to a directory so a parent test process and a re-invoked child
/// process (the kill-9 matrix, Step 6) share the SAME sealed bytes and CAS head:
/// the physical keys `blake3(sealed)` match across processes only because both
/// read identical persisted blobs.
pub(crate) struct SharedRemote {
    root: PathBuf,
}

impl SharedRemote {
    pub(crate) fn open(root: PathBuf) -> Self {
        fs::create_dir_all(root.join("blobs")).expect("blobs dir");
        fs::create_dir_all(root.join("manifests")).expect("manifests dir");
        Self { root }
    }

    fn blob_path(&self, key: &str) -> PathBuf {
        self.root.join("blobs").join(key)
    }

    fn manifest_path(&self, key: &str) -> PathBuf {
        self.root.join("manifests").join(key)
    }

    fn ref_path(&self) -> PathBuf {
        self.root.join("ref.json")
    }

    fn write_ref(&self, observed: &RefObservation) {
        let line = format!("{}\n{}\n", observed.version, observed.manifest_key.as_str());
        fs::write(self.ref_path(), line).expect("write ref");
    }

    pub(crate) fn current_ref(&self) -> Option<RefObservation> {
        let raw = fs::read_to_string(self.ref_path()).ok()?;
        let mut lines = raw.lines();
        let version = lines.next()?.parse().ok()?;
        let manifest_key = ManifestKey::new(lines.next()?.to_string());
        Some(RefObservation {
            version,
            manifest_key,
        })
    }

    /// Seal + persist a file blob so a published manifest can reference it.
    pub(crate) fn publish_blob(&self, crypto: &WorkspaceCrypto, plaintext: &[u8]) -> ManifestEntry {
        let content_id = crypto.content_id(plaintext);
        let sealed = seal_file(crypto, &content_id, plaintext).expect("seal file");
        let key = physical_blob_key(sealed.as_bytes());
        fs::write(self.blob_path(key.as_str()), sealed.into_bytes()).expect("write blob");
        ManifestEntry::File {
            size: plaintext.len() as u64,
            mode: FileMode::new(0o644),
            content_id,
            blob_key: key,
            key_epoch: crypto.key_epoch(),
        }
    }

    /// Publish a head from `(path, entry)` pairs, advancing the ref.
    pub(crate) fn publish(
        &self,
        crypto: &WorkspaceCrypto,
        entries: &[(&str, ManifestEntry)],
    ) -> ManifestKey {
        let map: BTreeMap<WorkspacePath, ManifestEntry> = entries
            .iter()
            .map(|(path, entry)| (WorkspacePath::new(*path), entry.clone()))
            .collect();
        let manifest = Manifest::new(crypto.key_epoch(), map);
        let plaintext = manifest.to_canonical_bytes().expect("canonical");
        let sealed = seal_manifest(crypto, &plaintext).expect("seal manifest");
        let key = physical_manifest_key(sealed.as_bytes());
        fs::write(self.manifest_path(key.as_str()), sealed.into_bytes()).expect("write manifest");
        let version = self
            .current_ref()
            .map(|observed| observed.version)
            .unwrap_or(0)
            + 1;
        self.write_ref(&RefObservation {
            version,
            manifest_key: key.clone(),
        });
        key
    }
}

impl RemoteObjects for SharedRemote {
    fn put_blob(&self, upload: BlobUpload<'_>) -> Result<(), TransportError> {
        fs::write(self.blob_path(upload.key.as_str()), upload.sealed)
            .map_err(|error| TransportError::new("put_blob", error.to_string()))
    }

    fn put_blob_reader(&self, upload: BlobReaderUpload<'_>) -> Result<(), TransportError> {
        let bytes = fs::read(upload.spool_path)
            .map_err(|error| TransportError::new("put_blob_reader", error.to_string()))?;
        fs::write(self.blob_path(upload.key.as_str()), bytes)
            .map_err(|error| TransportError::new("put_blob_reader", error.to_string()))
    }

    fn put_manifest(&self, upload: ManifestUpload<'_>) -> Result<(), TransportError> {
        fs::write(self.manifest_path(upload.key.as_str()), upload.sealed)
            .map_err(|error| TransportError::new("put_manifest", error.to_string()))
    }

    fn get_blob(&self, key: &BlobKey) -> Result<Vec<u8>, TransportError> {
        fs::read(self.blob_path(key.as_str()))
            .map_err(|error| TransportError::new("get_blob", error.to_string()))
    }

    fn get_manifest(&self, key: &ManifestKey) -> Result<Vec<u8>, TransportError> {
        fs::read(self.manifest_path(key.as_str()))
            .map_err(|error| TransportError::new("get_manifest", error.to_string()))
    }
}

impl RemoteRef for SharedRemote {
    fn read_ref(&self) -> Result<Option<RefObservation>, TransportError> {
        Ok(self.current_ref())
    }

    fn compare_and_swap(
        &self,
        expected_version: Option<u64>,
        new_manifest_key: &ManifestKey,
    ) -> Result<CasOutcome, TransportError> {
        // The kill matrix drives one child then the parent — never concurrently —
        // so a read-compare-write is sufficient (no cross-process lock needed).
        let current = self.current_ref();
        if current.as_ref().map(|observed| observed.version) != expected_version {
            return Ok(CasOutcome::Lost(
                current.expect("lost implies a current ref"),
            ));
        }
        let version = current.map(|observed| observed.version).unwrap_or(0) + 1;
        let observed = RefObservation {
            version,
            manifest_key: new_manifest_key.clone(),
        };
        self.write_ref(&observed);
        Ok(CasOutcome::Advanced(observed))
    }
}

/// Build the driver-cycle dependency bundle from a fake/shared remote and clock.
pub(crate) fn engine_io<'a, T>(remote: &'a T, clock: &'a TestClock) -> EngineIo<'a, T, T, TestClock>
where
    T: RemoteObjects + RemoteRef,
{
    EngineIo {
        objects: remote,
        refs: remote,
        clock,
    }
}

/// A single-engine driver under test: its own temp workspace, engine (store under
/// `.bowline`), fake remote, and virtual clock. Shared by the driver tests and
/// the invariant tests so neither copies the wiring.
pub(crate) struct DriverHarness {
    _workspace: TempWorkspace,
    pub(crate) root: PathBuf,
    pub(crate) engine: ManifestEngine,
    pub(crate) remote: FakeRemote,
    pub(crate) clock: TestClock,
}

impl DriverHarness {
    pub(crate) fn new(name: &str, device: &str) -> Self {
        let workspace = TempWorkspace::new(name).expect("temp workspace");
        let root = workspace.root().to_path_buf();
        let store = open_engine_store(&root);
        let ctx = test_context(root.clone(), device);
        Self {
            _workspace: workspace,
            root,
            engine: ManifestEngine::new(store, ctx),
            remote: FakeRemote::new(),
            clock: TestClock::new(),
        }
    }

    pub(crate) fn start(&mut self) {
        let io = engine_io(&self.remote, &self.clock);
        self.engine.start(&io).expect("start");
    }

    pub(crate) fn event(&mut self, event: EngineEvent) {
        self.engine.on_event(event, &self.clock);
    }

    pub(crate) fn run_due(&mut self) {
        let io = engine_io(&self.remote, &self.clock);
        self.engine.run_due_work(&io).expect("run due work");
    }

    /// A point-in-time copy of the engine's shared cost meters (Plan 111 Step 5).
    pub(crate) fn counters(&self) -> super::counters::CountersSnapshot {
        self.engine.counters().snapshot()
    }

    /// Deliver a watcher batch, let the debounce window elapse, and run the cycle.
    pub(crate) fn edit(&mut self, paths: &[&str]) {
        let set = paths.iter().map(|path| WorkspacePath::new(*path)).collect();
        self.event(EngineEvent::Paths(set));
        self.clock.advance(1_001);
        self.run_due();
    }

    pub(crate) fn write(&self, rel: &str, bytes: &[u8]) {
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(&path, bytes).expect("write");
    }

    /// Replace the engine's store with a fresh connection to the SAME database and
    /// re-run [`ManifestEngine::start`] — the restart path (invariant C3).
    pub(crate) fn restart(&mut self) {
        let store = open_engine_store(&self.root);
        self.engine = ManifestEngine::new(store, test_context(self.root.clone(), "device-a"));
        self.start();
    }
}
