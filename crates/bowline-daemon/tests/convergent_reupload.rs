//! The reinstall path: a device pushes content the workspace already holds.
//!
//! Sealing is convergent — the envelope's nonce is derived from the workspace
//! key and the content id — so an object key is a pure function of the
//! plaintext. Any device whose local blob ledger is empty therefore re-presents
//! objects the control plane already stores: a reinstall, a restored machine, a
//! second device pushing what the first already uploaded, or a first push whose
//! objects landed before its ref CAS did.
//!
//! Before the already-committed outcome existed, the control plane answered that
//! with an error and the engine treated it as a failed upload. Production sync
//! never converged: the ref stayed at version 0 while the client retried
//! forever. This drives the real engine over the real transport to prove the
//! whole cycle — blobs, the manifest tree, and the ref CAS — now completes.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use bowline_control_plane::{FakeControlPlaneClient, WorkspaceControlPlaneClient};
use bowline_core::ids::{DeviceId, WorkspaceId};
use bowline_daemon::manifest_transport::ManifestTransport;
use bowline_local::sync::manifest_engine::{
    CasOutcome, ENGINE_STATE_DIR, EngineConfig, EngineContext, EngineCounters, KeyEpoch,
    ManifestKey, ManifestStore, PushDeps, PushOutcome, RefObservation, RemoteRef, TransportError,
    WorkspaceCrypto, WorkspacePath, probe_name_folding, probe_timestamp_granularity, push,
};

const WORKSPACE: &str = "ws_convergent_reupload";
const DEVICE: &str = "device_convergent_reupload";
const FILE_PATH: &str = "notes.txt";
const FILE_BYTES: &[u8] = b"content the workspace already holds";

/// The engine's ref seam for the first attempt, which must publish every object
/// and then fail to move the ref — a crash, a lost ack, or a lost race. It never
/// touches the control plane, so the hosted ref stays at genesis: an ambiguous
/// swap sends push back to `read_ref`, which reports no head.
struct RefNeverMoves;

impl RemoteRef for RefNeverMoves {
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

#[test]
fn push_converges_when_every_object_is_already_committed() {
    let fixture = Fixture::new();
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace(WORKSPACE);
    let puts = PutCounter::default();
    control_plane.set_signed_url_override("upload", puts.serve());

    let transport = ManifestTransport::new(
        &control_plane,
        WorkspaceId::new(WORKSPACE),
        DeviceId::new(DEVICE),
    );
    let ctx = fixture.engine_context();
    let dirty = BTreeSet::from([WorkspacePath::new(FILE_PATH)]);

    // Attempt 1 publishes every object, then loses the ref. The objects are
    // durable regardless: a lost CAS does not un-commit them.
    let mut abandoned = fixture.open_store("abandoned");
    let lost = push(
        &mut abandoned,
        &PushDeps {
            ctx: &ctx,
            objects: &transport,
            refs: &RefNeverMoves,
        },
        &dirty,
    )
    .expect("the first attempt publishes its objects");
    assert!(
        matches!(lost, PushOutcome::RefLost { .. }),
        "the fixture needs the first attempt to leave the ref untouched, got {lost:?}"
    );
    let published = puts.count();
    assert!(published > 0, "the first attempt uploaded nothing");
    drop(abandoned);

    // Attempt 2 is the reinstalled device: an empty ledger against a workspace
    // that already holds every object this push is about to name.
    let mut fresh = fixture.open_store("fresh");
    let content_id = ctx.crypto.content_id(FILE_BYTES);
    assert!(
        fresh
            .sealed_blob(&content_id, ctx.key_epoch())
            .expect("blob ledger reads")
            .is_none(),
        "the fixture needs an empty blob ledger"
    );

    let advanced = push(
        &mut fresh,
        &PushDeps {
            ctx: &ctx,
            objects: &transport,
            refs: &transport,
        },
        &dirty,
    )
    .expect("a push whose objects are already committed still converges");

    let PushOutcome::Advanced {
        manifest_key,
        ref_version,
        ..
    } = advanced
    else {
        panic!("expected the ref to advance, got {advanced:?}");
    };
    assert_eq!(ref_version, 1);

    // The manifest object travels the same already-committed path as the blobs,
    // so this is the assertion that the thing production was stuck on converged.
    let hosted = control_plane
        .get_workspace_ref(&WorkspaceId::new(WORKSPACE))
        .expect("hosted ref reads")
        .expect("hosted ref exists");
    assert_eq!(hosted.version, 1);
    assert_eq!(
        hosted.snapshot_id.as_ref().map(|id| id.as_str()),
        Some(manifest_key.as_str()),
    );

    // Nothing was transferred: every key was already stored.
    assert_eq!(puts.count(), published);

    // The ledger now remembers the blob, so the next cycle does not ask again.
    assert!(
        fresh
            .sealed_blob(&content_id, ctx.key_epoch())
            .expect("blob ledger reads")
            .is_some(),
        "an already-present blob must still be recorded locally"
    );
}

// ---- fixture ----------------------------------------------------------------

struct Fixture {
    temp: PathBuf,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = std::env::temp_dir().join(format!(
            "bowline-convergent-reupload-{}-{:?}",
            std::process::id(),
            thread::current().id(),
        ));
        let root = temp.join("Code");
        std::fs::create_dir_all(&root).expect("workspace root");
        std::fs::write(root.join(FILE_PATH), FILE_BYTES).expect("workspace file");
        Self { temp, root }
    }

    /// Stores live outside the workspace root so two engine attempts see byte
    /// for byte the same tree.
    fn open_store(&self, name: &str) -> ManifestStore {
        ManifestStore::open(self.temp.join(format!("{name}.sqlite3"))).expect("engine store opens")
    }

    fn engine_context(&self) -> EngineContext {
        let engine_state_dir = self.root.join(ENGINE_STATE_DIR);
        EngineContext {
            crypto: WorkspaceCrypto::new(WORKSPACE, [11_u8; 32], KeyEpoch::new(1)),
            device_id: DeviceId::new(DEVICE),
            names: probe_name_folding(&engine_state_dir),
            timestamps: probe_timestamp_granularity(&engine_state_dir),
            endpoint_probe_root: engine_state_dir.clone(),
            engine_state_dir,
            workspace_root: self.root.clone(),
            config: EngineConfig::default(),
            project_view: false,
            counters: EngineCounters::shared(),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.temp);
    }
}

// ---- signed-URL PUT server --------------------------------------------------

/// An always-200 stand-in for R2 that counts the transfers it served, so the
/// test can assert that the converging push moved no bytes at all.
#[derive(Default)]
struct PutCounter {
    served: Arc<AtomicUsize>,
}

impl PutCounter {
    fn serve(&self) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test listener");
        let address = listener.local_addr().expect("listener address");
        let served = Arc::clone(&self.served);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                served.fetch_add(1, Ordering::Relaxed);
                thread::spawn(move || {
                    let mut request = [0; 4096];
                    let _ = stream.read(&mut request);
                    let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
                });
            }
        });
        format!("http://{address}/object")
    }

    fn count(&self) -> usize {
        self.served.load(Ordering::Relaxed)
    }
}
