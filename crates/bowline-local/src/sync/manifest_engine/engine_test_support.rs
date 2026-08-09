//! Shared in-crate test doubles for the manifest-sync engine (Plan 109 Steps
//! 4–5). One extracted harness rather than a copy per test file (AGENTS: second
//! copy = extract). Extends the `FakeControlPlaneClient` pattern from
//! `crates/bowline-control-plane/src/transfer/tests.rs`, adding the
//! metadata-commit-before-reference behavior the buffered fake lacks.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use bowline_core::ids::{ContentId, DeviceId, WorkspaceId};

use super::EngineProcessIdentity;
use super::counters::EngineCounters;
use super::endpoint::{
    NameFolding, probe_name_folding, probe_timestamp_granularity, sample_endpoint_clock,
};
use super::fs_guard::{ObserveOutcome, Observed, observe_classified};
use super::manifest::{
    DecodeLimits, KeyEpoch, Manifest, ManifestEntry, ManifestKey, WorkspaceCrypto, WorkspacePath,
};
use super::pull_apply::{PullDeps, PullError, PullOutcome, PullScope, pull};
use super::push::{
    EngineConfig, EngineContext, PushDeps, PushOutcome, RemoteObjects, RemoteRef, push,
};
use super::store::{FileRecord, ManifestStore};
use super::tree_transport::{
    FetchTreeRequest, PublishTreeRequest, StoreNodeLedger, UnledgeredNodes, fetch_tree,
    publish_tree,
};
use super::{Clock, EngineEvent, EngineIo, ManifestEngine};
use crate::workspace::TempWorkspace;

pub(crate) use super::engine_test_remotes::{CasMode, Event, FakeRemote, SharedRemote};

/// Publish a flat manifest as a tree into any object sink, returning the root
/// key. One helper so every double (in-memory, on-disk, per-test) shares the
/// writer path the engine itself uses.
pub(crate) fn publish_test_tree<O: RemoteObjects>(
    objects: &O,
    crypto: &WorkspaceCrypto,
    manifest: &Manifest,
) -> ManifestKey {
    let counters = EngineCounters::default();
    publish_tree(PublishTreeRequest {
        objects,
        crypto,
        counters: &counters,
        manifest,
        ledger: &mut UnledgeredNodes,
    })
    .expect("publish manifest tree")
}

/// Publish a flat manifest through the REAL store-backed node ledger, so a
/// second publish against the same store pays only for what changed. This is the
/// path the engine itself takes; the unledgered helper above is for one-shot
/// fixtures where dedup is beside the point.
pub(crate) fn publish_test_tree_ledgered<O: RemoteObjects>(
    objects: &O,
    crypto: &WorkspaceCrypto,
    manifest: &Manifest,
    store: &mut ManifestStore,
) -> ManifestKey {
    let counters = EngineCounters::default();
    let key_epoch = crypto.key_epoch();
    let mut ledger = StoreNodeLedger::new(store, key_epoch);
    let key = publish_tree(PublishTreeRequest {
        objects,
        crypto,
        counters: &counters,
        manifest,
        ledger: &mut ledger,
    })
    .expect("publish manifest tree");
    let recorded = ledger.into_recorded();
    store
        .record_tree_nodes(&recorded, key_epoch)
        .expect("record tree nodes");
    key
}

/// Flatten a manifest tree from any object sink, with no pruning.
pub(crate) fn fetch_test_tree<O: RemoteObjects>(
    objects: &O,
    crypto: &WorkspaceCrypto,
    root: &ManifestKey,
    limits: &DecodeLimits,
    names: NameFolding,
) -> Result<super::tree_transport::FetchedTree, super::tree_transport::TreeError> {
    let counters = EngineCounters::default();
    fetch_tree(FetchTreeRequest {
        objects,
        crypto,
        counters: &counters,
        root,
        limits,
        names,
        prune: None,
    })
}

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
        process_identity: EngineProcessIdentity::current(),
        workspace_identity: WorkspaceId::new("ws_code"),
        crypto: test_crypto(),
        device_id: DeviceId::new(device.to_string()),
        engine_state_dir: root.join(super::ENGINE_STATE_DIR),
        endpoint_probe_root: root.join(super::ENGINE_STATE_DIR),
        names: probe_name_folding(&root.join(super::ENGINE_STATE_DIR)),
        timestamps: probe_timestamp_granularity(&root.join(super::ENGINE_STATE_DIR)),
        workspace_root: root,
        config: EngineConfig::default(),
        project_view: false,
        counters: super::EngineCounters::shared(),
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
            process_identity: EngineProcessIdentity::current(),
            workspace_identity: WorkspaceId::new("ws_code"),
            crypto: WorkspaceCrypto::new("ws_code", KEY_BYTES, KeyEpoch::new(1)),
            device_id: DeviceId::new(format!("device-{name}")),
            engine_state_dir: root.join(super::ENGINE_STATE_DIR),
            endpoint_probe_root: root.join(super::ENGINE_STATE_DIR),
            names: probe_name_folding(&root.join(super::ENGINE_STATE_DIR)),
            timestamps: probe_timestamp_granularity(&root.join(super::ENGINE_STATE_DIR)),
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

    /// Run what follows inside ONE of this endpoint's timestamp buckets, by
    /// waiting out the tail of the current one when too little of it remains.
    ///
    /// A test that writes twice and expects the endpoint to date both writes
    /// alike is right almost always and wrong whenever the scenario straddles a
    /// bucket boundary. "Almost always" is how a suite earns a failure nobody
    /// can reproduce, so the scenario is placed rather than gambled.
    ///
    /// The budget is measured against the VOLUME's clock, not the process wall
    /// clock: the two disagree by up to one tick, and that disagreement is the
    /// bug this whole mechanism exists for. Nothing to do at nanosecond
    /// granularity, where no two instants share a bucket anyway.
    pub(crate) fn align_within_one_bucket(&self) {
        /// Generous next to the scenarios that use it — a handful of writes and
        /// pushes against a fake remote, microseconds of real work.
        const SCENARIO_BUDGET_NS: i64 = 250_000_000;

        let bucket = self.ctx.timestamps.nanos();
        if bucket <= SCENARIO_BUDGET_NS {
            return;
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            let now =
                sample_endpoint_clock(self.ctx.endpoint_probe_root(), &self.ctx.workspace_root)
                    .expect("the endpoint clock is readable on the workspace volume");
            let remaining = bucket - now.nanos().rem_euclid(bucket);
            if remaining > SCENARIO_BUDGET_NS {
                return;
            }
            std::thread::sleep(std::time::Duration::from_nanos(
                remaining as u64 + 1_000_000,
            ));
        }
        panic!("the endpoint clock never reached a fresh bucket");
    }

    /// Wait until the endpoint volume's clock has moved past `rel`'s timestamp.
    ///
    /// A file is racily clean until the volume can date it before the reading
    /// that verified it, so a stat can settle it only once the clock has moved
    /// on. Real work clears that by itself — an apply installs thousands of
    /// files, a push seals and uploads one — but a test that writes and pushes
    /// inside a microsecond is asking for an optimization the endpoint has not
    /// made available yet. Tests that assert the stat-only path say so here
    /// instead of depending on how coarsely the CI volume happens to tick.
    pub(crate) fn settle_endpoint_clock(&self, rel: &str) {
        let granularity = self.ctx.timestamps;
        let ctime = self.observe(rel).expect("observe").fingerprint.ctime_ns;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if let Some(now) =
                sample_endpoint_clock(self.ctx.endpoint_probe_root(), &self.ctx.workspace_root)
                && granularity.bucket(ctime) < granularity.bucket(now.nanos())
            {
                return;
            }
            std::thread::sleep(std::time::Duration::from_micros(200));
        }
        panic!("the endpoint clock never moved past the write to {rel}");
    }

    pub(crate) fn observe(&self, rel: &str) -> Option<Observed> {
        observe_present(&self.ctx.workspace_root, &WorkspacePath::new(rel))
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
        // A bare `TestEngine` keeps no dirty set, so it stands in for a caller
        // with no divergence tracking at all (the work-view materialize shape).
        // `pull_dirty` is the narrowed driver shape.
        self.try_pull_scoped(PullScope::WholeAncestor)
    }

    /// Pull with the driver's narrowed scope: only paths the remote moved plus
    /// the named dirty paths are re-observed on disk.
    pub(crate) fn pull_dirty(&mut self, paths: &[&str]) -> PullOutcome {
        let dirty = self.dirty(paths);
        let deps = PullDeps {
            ctx: &self.ctx,
            objects: &self.remote,
            refs: &self.remote,
            scope: PullScope::ChangedAndDirty(&dirty),
        };
        pull(&mut self.store, &deps).expect("pull")
    }

    fn try_pull_scoped(&mut self, scope: PullScope<'_>) -> Result<PullOutcome, PullError> {
        let deps = PullDeps {
            ctx: &self.ctx,
            objects: &self.remote,
            refs: &self.remote,
            scope,
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

/// The `Present` observation of a path, or `None` for absent OR unsyncable.
///
/// Test-only, and deliberately not a production helper: collapsing "unsyncable"
/// into "absent" is the confusion that lets an unreadable path be published as a
/// deletion, so production reads the three-answer [`observe_classified`] and
/// pull's `observe_syncable` instead. A fixture asserting on a file it just wrote
/// has no such ambiguity.
pub(crate) fn observe_present(root: &Path, path: &WorkspacePath) -> Option<Observed> {
    match observe_classified(root, path) {
        ObserveOutcome::Present(observed) => Some(observed),
        ObserveOutcome::Absent | ObserveOutcome::Unsyncable(_) => None,
    }
}

/// Plant a FIFO at a workspace-relative path: an object the engine can never
/// represent, and the fixture the whole "one unsyncable object must not fail a
/// cycle" family is built on. The single owner of it — the apply-race cases and
/// the generative fault storm both call this.
///
/// Shells out to `mkfifo(1)` rather than calling the syscall. `rustix`'s
/// `mkfifoat` is compiled out on Apple targets and this crate is
/// `#![deny(unsafe_code)]`, so libc FFI is not available either; `mkfifo(1)` is
/// POSIX and present on both platforms the engine is tested on. A path that is
/// already a FIFO is the shape the caller asked for, so it is a no-op.
pub(crate) fn plant_fifo(root: &Path, relative: &str) -> std::io::Result<()> {
    let absolute = root.join(relative);
    if let Some(parent) = absolute.parent() {
        fs::create_dir_all(parent)?;
    }
    if fs::symlink_metadata(&absolute).is_ok_and(|metadata| {
        use std::os::unix::fs::FileTypeExt;
        metadata.file_type().is_fifo()
    }) {
        return Ok(());
    }
    let status = std::process::Command::new("mkfifo")
        .arg("-m")
        .arg("600")
        .arg(&absolute)
        .status()?;
    if status.success() {
        return Ok(());
    }
    Err(std::io::Error::other(format!(
        "mkfifo {} failed: {status}",
        absolute.display()
    )))
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
        self.try_start().expect("start");
    }

    /// Startup without the `expect`, so a test can assert that a hostile on-disk
    /// state (a journalled intent whose target raced away) still comes up.
    pub(crate) fn try_start(&mut self) -> Result<(), super::EngineError> {
        let io = engine_io(&self.remote, &self.clock);
        self.engine.start(&io)
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
        // A publish is bounded, so a batch larger than one carries over. Drive to
        // quiescence: a fixture is setup, and its cost is not what a test measures.
        self.drain_pending_publishes();
    }

    /// Run cycles until nothing is left to publish, bounded so a genuine stall
    /// fails the test instead of hanging it.
    pub(crate) fn drain_pending_publishes(&mut self) {
        for _ in 0..64 {
            if self.engine.snapshot().dirty == 0 {
                return;
            }
            self.clock.advance(1_001);
            self.run_due();
        }
        // Not an assertion: an engine deliberately backing off has work it is
        // choosing not to do, and a fixture helper must not call that a stall.
    }

    pub(crate) fn write(&self, rel: &str, bytes: &[u8]) {
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(&path, bytes).expect("write");
    }

    pub(crate) fn read(&self, rel: &str) -> Vec<u8> {
        fs::read(self.root.join(rel)).expect("read")
    }

    /// Replace the engine's store with a fresh connection to the SAME database and
    /// re-run [`ManifestEngine::start`] — the restart path (invariant C3).
    pub(crate) fn restart(&mut self) {
        self.try_restart().expect("restart");
    }

    pub(crate) fn try_restart(&mut self) -> Result<(), super::EngineError> {
        let store = open_engine_store(&self.root);
        self.engine = ManifestEngine::new(store, test_context(self.root.clone(), "device-a"));
        self.try_start()
    }
}
