//! Engine-level tests for the work-view RPC operations against an in-memory
//! remote: create materializes the head, review captures + diffs, accept
//! captures + merges + publishes through the CAS. These cover the real effects
//! the cli_contract scripted-daemon tests stub out.

use std::path::{Path, PathBuf};

use bowline_local::sync::manifest_engine::work_view::{
    ChangeKind, diff_manifests, fetch_manifest, fetch_project_manifest,
};
use bowline_local::sync::manifest_engine::{
    CasOutcome, EngineCounters, ManifestKey, PublishTreeRequest, RemoteObjects, RemoteRef,
    UnledgeredNodes, WorkspaceCrypto, publish_tree,
};

use crate::daemon::rpc_service::work_views::VIEW_ENGINE_DB_FILE;
use crate::daemon::rpc_service::work_views::WorkViewEngineEnv;
use crate::daemon::rpc_service::work_views::WorkViewRpcError;
use crate::daemon::rpc_service::work_views::accept_intent_path;
use crate::daemon::rpc_service::work_views::accept_project_view_dir;
use crate::daemon::rpc_service::work_views::accept_view_dir;
use crate::daemon::rpc_service::work_views::checked_project_path;
use crate::daemon::rpc_service::work_views::checked_view_dir;
use crate::daemon::rpc_service::work_views::create_view;
use crate::daemon::rpc_service::work_views::materialize_existing_view;
use crate::daemon::rpc_service::work_views::review_view_dir;
use crate::daemon::rpc_service::work_views::view_engine;
use crate::daemon::rpc_service::work_views::view_engine_dir;
use std::collections::BTreeMap;
use std::sync::Mutex;

use bowline_core::ids::DeviceId;
use bowline_local::sync::manifest_engine::{
    BlobKey, BlobReaderUpload, BlobUpload, FileMode, KeyEpoch, Manifest, ManifestEntry,
    ManifestUpload, RefObservation, TransportError, WorkspacePath, physical_blob_key, seal_file,
};

// ---- in-memory remote -------------------------------------------------------

#[derive(Default)]
struct FakeRemote {
    blobs: Mutex<BTreeMap<String, Vec<u8>>>,
    manifests: Mutex<BTreeMap<String, Vec<u8>>>,
    head: Mutex<Option<RefObservation>>,
    history: Mutex<BTreeMap<u64, ManifestKey>>,
    lose_next_cas: Mutex<bool>,
    write_during_next_cas: Mutex<Option<(PathBuf, Vec<u8>)>>,
    break_view_dir_during_next_cas: Mutex<Option<PathBuf>>,
    lock_directory_during_next_cas: Mutex<Option<PathBuf>>,
}

impl FakeRemote {
    fn crypto() -> WorkspaceCrypto {
        WorkspaceCrypto::new("ws_work_rpc", [9_u8; 32], KeyEpoch::new(1))
    }

    /// Seal + store a file blob, returning its manifest entry.
    fn publish_blob(&self, crypto: &WorkspaceCrypto, plaintext: &[u8]) -> ManifestEntry {
        let content_id = crypto.content_id(plaintext);
        let sealed = seal_file(crypto, &content_id, plaintext).expect("blob seals");
        let blob_key = physical_blob_key(sealed.as_bytes());
        self.blobs
            .lock()
            .expect("blobs lock")
            .insert(blob_key.as_str().to_string(), sealed.into_bytes());
        ManifestEntry::File {
            size: plaintext.len() as u64,
            mode: FileMode::new(0o100_644),
            content_id,
            blob_key,
            key_epoch: crypto.key_epoch(),
        }
    }

    /// Seal + store a manifest and advance the ref to it.
    fn publish_head(
        &self,
        crypto: &WorkspaceCrypto,
        entries: &[(&str, ManifestEntry)],
    ) -> ManifestKey {
        let manifest = Manifest::new(
            crypto.key_epoch(),
            entries
                .iter()
                .map(|(path, entry)| (WorkspacePath::new(*path), entry.clone()))
                .collect(),
        );
        let key = publish_tree(PublishTreeRequest {
            objects: self,
            crypto,
            counters: &EngineCounters::default(),
            manifest: &manifest,
            ledger: &mut UnledgeredNodes,
        })
        .expect("manifest tree publishes");
        let mut head = self.head.lock().expect("head lock");
        let version = head.as_ref().map(|observed| observed.version).unwrap_or(0) + 1;
        *head = Some(RefObservation {
            version,
            manifest_key: key.clone(),
        });
        self.history
            .lock()
            .expect("history lock")
            .insert(version, key.clone());
        key
    }

    fn head_key(&self) -> ManifestKey {
        self.head
            .lock()
            .expect("head lock")
            .as_ref()
            .expect("head is published")
            .manifest_key
            .clone()
    }

    fn lose_next_cas(&self) {
        *self.lose_next_cas.lock().expect("CAS race lock") = true;
    }

    fn write_during_next_cas(&self, path: PathBuf, bytes: &[u8]) {
        *self.write_during_next_cas.lock().expect("CAS write lock") = Some((path, bytes.to_vec()));
    }

    fn break_view_dir_during_next_cas(&self, view_dir: PathBuf) {
        *self
            .break_view_dir_during_next_cas
            .lock()
            .expect("CAS view-dir failure lock") = Some(view_dir);
    }

    fn lock_directory_during_next_cas(&self, directory: PathBuf) {
        *self
            .lock_directory_during_next_cas
            .lock()
            .expect("CAS directory lock") = Some(directory);
    }
}

impl RemoteObjects for FakeRemote {
    fn put_blob(&self, upload: BlobUpload<'_>) -> Result<(), TransportError> {
        self.blobs
            .lock()
            .expect("blobs lock")
            .insert(upload.key.as_str().to_string(), upload.sealed.to_vec());
        Ok(())
    }

    fn put_blob_reader(&self, upload: BlobReaderUpload<'_>) -> Result<(), TransportError> {
        let sealed = std::fs::read(upload.spool_path)
            .map_err(|error| TransportError::new("put-blob-reader", error.to_string()))?;
        self.blobs
            .lock()
            .expect("blobs lock")
            .insert(upload.key.as_str().to_string(), sealed);
        Ok(())
    }

    fn put_manifest(&self, upload: ManifestUpload<'_>) -> Result<(), TransportError> {
        self.manifests
            .lock()
            .expect("manifests lock")
            .insert(upload.key.as_str().to_string(), upload.sealed.to_vec());
        Ok(())
    }

    fn get_blob(&self, key: &BlobKey) -> Result<Vec<u8>, TransportError> {
        self.blobs
            .lock()
            .expect("blobs lock")
            .get(key.as_str())
            .cloned()
            .ok_or_else(|| TransportError::new("get-blob", format!("missing {}", key.as_str())))
    }

    fn get_manifest(&self, key: &ManifestKey) -> Result<Vec<u8>, TransportError> {
        self.manifests
            .lock()
            .expect("manifests lock")
            .get(key.as_str())
            .cloned()
            .ok_or_else(|| TransportError::new("get-manifest", format!("missing {}", key.as_str())))
    }
}

impl RemoteRef for FakeRemote {
    fn read_ref(&self) -> Result<Option<RefObservation>, TransportError> {
        Ok(self.head.lock().expect("head lock").clone())
    }

    fn lookup_ref_version(
        &self,
        version: u64,
    ) -> Result<bowline_local::sync::manifest_engine::RefVersionLookup, TransportError> {
        Ok(self
            .history
            .lock()
            .expect("history lock")
            .get(&version)
            .cloned()
            .map(bowline_local::sync::manifest_engine::RefVersionLookup::Found)
            .unwrap_or(bowline_local::sync::manifest_engine::RefVersionLookup::NotAdvanced))
    }

    fn compare_and_swap(
        &self,
        expected_version: Option<u64>,
        new_manifest_key: &ManifestKey,
    ) -> Result<CasOutcome, TransportError> {
        let mut head = self.head.lock().expect("head lock");
        let current_version = head.as_ref().map(|observed| observed.version);
        if current_version != expected_version {
            return Ok(CasOutcome::Lost(
                head.clone().expect("lost implies a current head"),
            ));
        }
        if std::mem::take(&mut *self.lose_next_cas.lock().expect("CAS race lock")) {
            let observed = RefObservation {
                version: current_version.unwrap_or(0) + 1,
                manifest_key: head
                    .as_ref()
                    .expect("CAS race requires a current head")
                    .manifest_key
                    .clone(),
            };
            *head = Some(observed.clone());
            return Ok(CasOutcome::Lost(observed));
        }
        if let Some((path, bytes)) = self
            .write_during_next_cas
            .lock()
            .expect("CAS write lock")
            .take()
        {
            std::fs::write(path, bytes).expect("injected concurrent view edit");
        }
        if let Some(view_dir) = self
            .break_view_dir_during_next_cas
            .lock()
            .expect("CAS view-dir failure lock")
            .take()
        {
            std::fs::rename(&view_dir, view_dir.with_extension("recover"))
                .expect("move view aside for injected failure");
            std::fs::write(&view_dir, b"blocked").expect("block view directory");
        }
        if let Some(directory) = self
            .lock_directory_during_next_cas
            .lock()
            .expect("CAS directory lock")
            .take()
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o500))
                .expect("lock accept-intent directory");
        }
        let observed = RefObservation {
            version: current_version.unwrap_or(0) + 1,
            manifest_key: new_manifest_key.clone(),
        };
        *head = Some(observed.clone());
        self.history
            .lock()
            .expect("history lock")
            .insert(observed.version, observed.manifest_key.clone());
        Ok(CasOutcome::Advanced(observed))
    }
}

// ---- fixture ----------------------------------------------------------------

struct Fixture {
    _workspace: bowline_local::workspace::TempWorkspace,
    root: PathBuf,
    state_root: PathBuf,
    crypto: WorkspaceCrypto,
    remote: FakeRemote,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let workspace = bowline_local::workspace::TempWorkspace::new(name).expect("temp workspace");
        let root = workspace.root().to_path_buf();
        let crypto = FakeRemote::crypto();
        let remote = FakeRemote::default();
        let file = remote.publish_blob(&crypto, b"console.log('base')\n");
        let recursive = remote.publish_blob(&crypto, b"must not enter a view\n");
        let project_dotfile = remote.publish_blob(&crypto, b"{\"project\":true}\n");
        remote.publish_head(
            &crypto,
            &[
                (
                    "apps",
                    ManifestEntry::Directory {
                        mode: FileMode::new(0o040_755),
                    },
                ),
                (
                    "apps/web",
                    ManifestEntry::Directory {
                        mode: FileMode::new(0o040_755),
                    },
                ),
                (
                    "apps/web/src",
                    ManifestEntry::Directory {
                        mode: FileMode::new(0o040_755),
                    },
                ),
                ("apps/web/src/index.ts", file),
                (
                    "apps/web/.bowline",
                    ManifestEntry::Directory {
                        mode: FileMode::new(0o040_755),
                    },
                ),
                ("apps/web/.bowline/project.json", project_dotfile),
                (
                    ".work",
                    ManifestEntry::Directory {
                        mode: FileMode::new(0o040_755),
                    },
                ),
                (".work/old-view.txt", recursive),
            ],
        );
        Self {
            _workspace: workspace,
            state_root: root.join(".daemon-state"),
            root,
            crypto,
            remote,
        }
    }

    fn env(&self) -> WorkViewEngineEnv<'_, FakeRemote, FakeRemote> {
        WorkViewEngineEnv {
            crypto: &self.crypto,
            workspace_id: bowline_core::ids::WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device_work_rpc"),
            objects: &self.remote,
            refs: &self.remote,
            workspace_root: self.root.clone(),
            state_root: self.state_root.clone(),
        }
    }

    fn view_dir(&self) -> PathBuf {
        self.root.join(".work/apps/web/auth-fix")
    }
}

fn fetch(fixture: &Fixture, key: &ManifestKey) -> Manifest {
    fetch_manifest(&fixture.remote, &fixture.crypto, key).expect("manifest fetches")
}

fn fetch_project(fixture: &Fixture, key: &ManifestKey) -> Manifest {
    fetch_project_manifest(&fixture.remote, &fixture.crypto, key).expect("project manifest fetches")
}

#[test]
fn view_engine_state_is_namespaced_by_workspace() {
    let state_root = PathBuf::from("/daemon-state");
    let first_root = PathBuf::from("/workspaces/first");
    let second_root = PathBuf::from("/workspaces/second");
    let relative = ".work/apps/web/auth-fix";

    let first = view_engine_dir(&state_root, &first_root, &first_root.join(relative));
    let second = view_engine_dir(&state_root, &second_root, &second_root.join(relative));

    assert_ne!(first, second);
    assert_eq!(
        first.parent(),
        Some(state_root.join("work-views").as_path())
    );
    assert_eq!(
        second.parent(),
        Some(state_root.join("work-views").as_path())
    );
}

#[test]
fn view_engine_probes_the_materialized_view_volume_not_daemon_state() {
    let fixture = Fixture::new("work-rpc-endpoint-volume");
    let view_dir = fixture.view_dir();
    let (_store, context) = view_engine(
        &fixture.root,
        &fixture.state_root,
        &bowline_core::ids::WorkspaceId::new("ws_code"),
        &fixture.crypto,
        &DeviceId::new("device_work_rpc"),
        &view_dir,
    )
    .expect("view engine");

    assert_eq!(context.endpoint_probe_root(), view_dir);
    assert!(
        context.engine_dir().starts_with(&fixture.state_root),
        "SQLite state remains under daemon state"
    );
    assert!(
        !context.engine_dir().starts_with(&view_dir),
        "capability probing is not inferred from SQLite placement"
    );
}

#[test]
fn project_path_must_match_the_normalized_view_scope() {
    let root = PathBuf::from("/workspace");
    let nested = root.join(".work/apps/web/auth-fix");
    assert_eq!(
        checked_project_path(&root, &nested, "apps/web").expect("nested project"),
        "apps/web"
    );
    let root_view = root.join(".work/root-fix");
    assert_eq!(
        checked_project_path(&root, &root_view, "").expect("root project"),
        ""
    );
    for invalid in [
        "../outside",
        "/absolute",
        ".",
        "/",
        "apps//web",
        "apps/other",
    ] {
        assert!(
            checked_project_path(&root, &nested, invalid).is_err(),
            "{invalid} must be rejected",
        );
    }
}

// ---- tests ------------------------------------------------------------------

#[test]
fn create_materializes_the_current_head_into_the_view_dir() {
    let fixture = Fixture::new("work-rpc-create");
    let env = fixture.env();
    let view_dir = fixture.view_dir();

    let base = create_view(&env, &view_dir).expect("create succeeds");
    assert_ne!(base, fixture.remote.head_key());
    assert_eq!(
        fetch_project(&fixture, &base)
            .entries
            .keys()
            .map(|path| path.as_str())
            .collect::<Vec<_>>(),
        vec![".bowline", ".bowline/project.json", "src", "src/index.ts",],
    );
    let materialized = view_dir.join("src/index.ts");
    assert_eq!(
        std::fs::read(&materialized).expect("view file exists"),
        b"console.log('base')\n"
    );
    assert!(!view_dir.join(".work").exists());
    assert!(!view_dir.join("apps").exists());
    assert_eq!(
        std::fs::read(view_dir.join(".bowline/project.json"))
            .expect("project-local .bowline content materializes"),
        b"{\"project\":true}\n"
    );
}

#[test]
fn rematerializing_a_deleted_view_resets_stale_engine_state() {
    let fixture = Fixture::new("work-rpc-rematerialize");
    let env = fixture.env();
    let view_dir = fixture.view_dir();
    let overlay = create_view(&env, &view_dir).expect("create succeeds");
    std::fs::remove_dir_all(&view_dir).expect("simulate missing host materialization");

    materialize_existing_view(&env, &view_dir, &overlay, true)
        .expect("existing overlay rematerializes");

    assert_eq!(
        std::fs::read(view_dir.join("src/index.ts")).expect("view file recreated"),
        b"console.log('base')\n"
    );
}

#[test]
fn create_without_a_synced_head_reports_no_head() {
    let fixture = Fixture::new("work-rpc-create-no-head");
    *fixture.remote.head.lock().expect("head lock") = None;
    let env = fixture.env();
    assert!(matches!(
        create_view(&env, &fixture.view_dir()),
        Err(WorkViewRpcError::NoSyncedHead)
    ));
}

#[test]
fn review_captures_view_edits_and_diffs_against_base() {
    let fixture = Fixture::new("work-rpc-review");
    let env = fixture.env();
    let view_dir = fixture.view_dir();
    let base = create_view(&env, &view_dir).expect("create succeeds");

    // A clean view reviews as no changes and keeps its overlay.
    let clean = review_view_dir(&env, &view_dir, &base, &base).expect("clean review");
    assert_eq!(clean.overlay, base);
    assert!(clean.changes.is_empty());

    // An agent adds a file in the view; review captures it and reports Added.
    std::fs::write(view_dir.join("feature.txt"), b"new feature\n").expect("agent edit");
    std::fs::write(
        view_dir.join(".bowline/project.json"),
        b"{\"project\":\"edited\"}\n",
    )
    .expect("project-local dot directory edit");
    let reviewed = review_view_dir(&env, &view_dir, &base, &base).expect("review succeeds");
    assert_ne!(reviewed.overlay, base);
    let added: Vec<_> = reviewed
        .changes
        .iter()
        .map(|change| (change.path.as_str().to_string(), change.kind))
        .collect();
    assert_eq!(
        added,
        vec![
            (".bowline/project.json".to_string(), ChangeKind::Modified,),
            ("feature.txt".to_string(), ChangeKind::Added),
        ]
    );
}

#[cfg(unix)]
#[test]
fn review_reports_unsyncable_paths_and_accept_requires_exact_acknowledgement() {
    use std::os::unix::net::UnixListener;

    let fixture = Fixture::new("work-rpc-unresolved");
    let env = fixture.env();
    let view_dir = fixture.view_dir();
    let base = create_view(&env, &view_dir).expect("create succeeds");
    let socket_path = view_dir.join("agent.sock");
    let short_socket = std::env::temp_dir().join(format!(
        "bw-wv-{}-{}.sock",
        std::process::id(),
        fixture.remote.blobs.lock().expect("blobs lock").len()
    ));
    let _listener = UnixListener::bind(&short_socket).expect("short socket path");
    std::fs::rename(short_socket, &socket_path).expect("move socket into view");

    let reviewed = review_view_dir(&env, &view_dir, &base, &base).expect("review succeeds");
    assert_eq!(
        reviewed.unresolved_paths,
        vec![super::WorkUnresolvedPath {
            path: "agent.sock".to_string(),
            reason: "unsupported-kind".to_string(),
        }]
    );
    let blocked = accept_project_view_dir(&env, &view_dir, &base, &base, "apps/web", &[], &[])
        .expect_err("unacknowledged path blocks accept");
    assert!(
        matches!(
            blocked,
            WorkViewRpcError::Engine(ref detail)
                if detail.contains("agent.sock=unsupported-kind")
        ),
        "{blocked:?}"
    );

    let accepted = accept_project_view_dir(
        &env,
        &view_dir,
        &base,
        &base,
        "apps/web",
        &[],
        &["agent.sock=unsupported-kind".to_string()],
    )
    .expect("exact acknowledgement permits accept");
    assert_eq!(accepted.unresolved_paths, reviewed.unresolved_paths);
}

#[test]
fn accept_publishes_the_merged_head_through_the_cas() {
    let fixture = Fixture::new("work-rpc-accept");
    let env = fixture.env();
    let view_dir = fixture.view_dir();
    let base = create_view(&env, &view_dir).expect("create succeeds");
    std::fs::write(view_dir.join("feature.txt"), b"new feature\n").expect("agent edit");

    let outcome = accept_view_dir(&env, &view_dir, &base, &base, &[]).expect("accept succeeds");
    assert!(outcome.conflict_asides.is_empty());
    assert_eq!(outcome.published, fixture.remote.head_key());

    let merged = fetch(&fixture, &outcome.published);
    assert!(
        merged
            .entries
            .contains_key(&WorkspacePath::new("apps/web/feature.txt"))
    );
    assert!(
        merged
            .entries
            .contains_key(&WorkspacePath::new("apps/web/src/index.ts"))
    );
}

#[test]
fn accept_retries_a_concurrent_workspace_head_advance() {
    let fixture = Fixture::new("work-rpc-accept-cas-retry");
    let env = fixture.env();
    let view_dir = fixture.view_dir();
    let base = create_view(&env, &view_dir).expect("create succeeds");
    std::fs::write(view_dir.join("feature.txt"), b"new feature\n").expect("agent edit");
    fixture.remote.lose_next_cas();

    let outcome = accept_view_dir(&env, &view_dir, &base, &base, &[])
        .expect("accept absorbs a normal CAS race");

    assert_eq!(outcome.published, fixture.remote.head_key());
    assert!(
        fetch(&fixture, &outcome.published)
            .entries
            .contains_key(&WorkspacePath::new("apps/web/feature.txt"))
    );
}

#[test]
fn accept_preserves_divergent_workspace_bytes_as_conflict_asides() {
    let fixture = Fixture::new("work-rpc-accept-conflict");
    let env = fixture.env();
    let view_dir = fixture.view_dir();
    let base = create_view(&env, &view_dir).expect("create succeeds");

    // The view edits index.ts...
    std::fs::write(view_dir.join("src/index.ts"), b"console.log('view')\n").expect("view edit");
    // ...while the workspace head advances the same file differently.
    let workspace_file = fixture
        .remote
        .publish_blob(&fixture.crypto, b"console.log('workspace')\n");
    fixture.remote.publish_head(
        &fixture.crypto,
        &[
            (
                "apps/web",
                ManifestEntry::Directory {
                    mode: FileMode::new(0o040_755),
                },
            ),
            (
                "apps/web/src",
                ManifestEntry::Directory {
                    mode: FileMode::new(0o040_755),
                },
            ),
            ("apps/web/src/index.ts", workspace_file.clone()),
        ],
    );

    let outcome = accept_view_dir(&env, &view_dir, &base, &base, &[]).expect("accept succeeds");
    assert_eq!(outcome.conflict_asides.len(), 1);
    assert_eq!(
        bowline_local::sync::manifest_engine::conflict_aside_origin(&outcome.conflict_asides[0]),
        Some("src/index.ts"),
    );

    let merged = fetch(&fixture, &outcome.published);
    // Workspace bytes stay canonical; the overlay's version is the aside entry.
    assert_eq!(
        merged
            .entries
            .get(&WorkspacePath::new("apps/web/src/index.ts")),
        Some(&workspace_file)
    );
    assert!(merged.entries.contains_key(&WorkspacePath::new(format!(
        "apps/web/{}",
        outcome.conflict_asides[0]
    ))));
}

#[test]
fn accept_leaves_newly_excluded_ancestor_paths_untouched() {
    let fixture = Fixture::new("work-rpc-accept-excluded");
    let env = fixture.env();
    let view_dir = fixture.view_dir();
    let base = create_view(&env, &view_dir).expect("create succeeds");

    std::fs::write(view_dir.join(".bowlineignore"), b"src/index.ts\n").expect("exclude path");
    std::fs::write(view_dir.join("src/index.ts"), b"must stay local\n").expect("excluded edit");
    let workspace_file = fixture
        .remote
        .publish_blob(&fixture.crypto, b"console.log('workspace')\n");
    fixture.remote.publish_head(
        &fixture.crypto,
        &[
            (
                "apps/web",
                ManifestEntry::Directory {
                    mode: FileMode::new(0o040_755),
                },
            ),
            (
                "apps/web/src",
                ManifestEntry::Directory {
                    mode: FileMode::new(0o040_755),
                },
            ),
            ("apps/web/src/index.ts", workspace_file.clone()),
        ],
    );

    let outcome = accept_view_dir(&env, &view_dir, &base, &base, &[]).expect("accept succeeds");
    assert!(outcome.conflict_asides.is_empty());
    let merged = fetch(&fixture, &outcome.published);
    assert_eq!(
        merged
            .entries
            .get(&WorkspacePath::new("apps/web/src/index.ts")),
        Some(&workspace_file),
    );
}

#[test]
fn accept_reports_a_deletion_the_workspace_overrode_as_discarded() {
    let fixture = Fixture::new("work-rpc-accept-delete-conflict");
    let env = fixture.env();
    let view_dir = fixture.view_dir();
    let base = create_view(&env, &view_dir).expect("create succeeds");

    // The view deletes index.ts...
    std::fs::remove_file(view_dir.join("src/index.ts")).expect("view delete");
    // ...while the workspace head modifies the same file after the fork.
    let workspace_file = fixture
        .remote
        .publish_blob(&fixture.crypto, b"console.log('workspace')\n");
    fixture.remote.publish_head(
        &fixture.crypto,
        &[
            (
                "apps/web",
                ManifestEntry::Directory {
                    mode: FileMode::new(0o040_755),
                },
            ),
            (
                "apps/web/src",
                ManifestEntry::Directory {
                    mode: FileMode::new(0o040_755),
                },
            ),
            ("apps/web/src/index.ts", workspace_file.clone()),
        ],
    );
    let head_before = fixture.remote.head_key();

    let outcome = accept_view_dir(&env, &view_dir, &base, &base, &[]).expect("accept succeeds");

    // The deletion did not land: it is reported as discarded, never accepted, and
    // there is no aside (a deletion has no content to preserve).
    assert_eq!(
        outcome.discarded_deletions,
        vec!["src/index.ts".to_string()]
    );
    assert!(outcome.conflict_asides.is_empty());
    assert!(
        outcome.accepted_paths.is_empty(),
        "the discarded deletion is not reported as an accepted path",
    );
    // The workspace's own edit survives; nothing new was published.
    assert_eq!(outcome.published, head_before);
    let merged = fetch(&fixture, &outcome.published);
    assert_eq!(
        merged
            .entries
            .get(&WorkspacePath::new("apps/web/src/index.ts")),
        Some(&workspace_file),
        "the live workspace file stays canonical",
    );
}

#[test]
fn checked_view_dir_rejects_paths_outside_the_work_tree() {
    let root = Path::new("/tmp/bowline-test-root/Code");
    assert!(checked_view_dir(root, "/tmp/bowline-test-root/Code/.work/apps/web/x").is_ok());
    for bad in [
        "/tmp/bowline-test-root/Code/apps/web",
        "/tmp/elsewhere/.work/apps/web/x",
        ".work/apps/web/x",
        "/tmp/bowline-test-root/Code",
        // The `.work` root itself is not a view directory.
        "/tmp/bowline-test-root/Code/.work",
        "/tmp/bowline-test-root/Code/.work/",
        // Lexically inside `.work` but escapes via parent-dir traversal.
        "/tmp/bowline-test-root/Code/.work/../../x",
    ] {
        assert!(checked_view_dir(root, bad).is_err(), "{bad}");
    }
}

#[test]
fn create_rejects_a_view_dir_through_a_symlinked_intermediate_component() {
    let fixture = Fixture::new("work-rpc-symlink-intermediate");
    let env = fixture.env();
    // A directory OUTSIDE the workspace that the symlink escapes to. A separate
    // temp workspace gives us a genuinely external, auto-cleaned target.
    let external = bowline_local::workspace::TempWorkspace::new("work-rpc-escape-intermediate")
        .expect("external temp dir");
    let external_root = external.root().to_path_buf();

    // `.work` is a real directory, but `.work/apps` is a symlink pointing outside
    // the workspace. The requested view lives lexically under `.work`, so the
    // lexical prefix check passes.
    let work = fixture.root.join(".work");
    std::fs::create_dir_all(&work).expect(".work root");
    std::os::unix::fs::symlink(&external_root, work.join("apps")).expect("symlinked component");
    let view_dir = fixture.root.join(".work/apps/web/auth-fix");

    assert!(matches!(
        create_view(&env, &view_dir),
        Err(WorkViewRpcError::ViewDirEscape)
    ));
    // The escape target is untouched: no view scaffolding was materialized through
    // the symlink.
    assert!(!external_root.join("web").exists());
    assert!(
        std::fs::read_dir(&external_root)
            .expect("external readable")
            .next()
            .is_none()
    );
}

#[test]
fn create_rejects_a_view_dir_whose_work_root_is_a_symlink() {
    let fixture = Fixture::new("work-rpc-symlink-work-root");
    let env = fixture.env();
    let external = bowline_local::workspace::TempWorkspace::new("work-rpc-escape-work-root")
        .expect("external temp dir");
    let external_root = external.root().to_path_buf();

    // The `.work` root itself is a symlink to an external directory.
    std::os::unix::fs::symlink(&external_root, fixture.root.join(".work"))
        .expect("symlinked .work root");
    let view_dir = fixture.root.join(".work/apps/web/auth-fix");

    assert!(matches!(
        create_view(&env, &view_dir),
        Err(WorkViewRpcError::ViewDirEscape)
    ));
    assert!(
        std::fs::read_dir(&external_root)
            .expect("external readable")
            .next()
            .is_none()
    );
}

#[test]
fn create_builds_the_view_chain_as_real_directories() {
    let fixture = Fixture::new("work-rpc-real-chain");
    let env = fixture.env();
    let view_dir = fixture.view_dir();

    create_view(&env, &view_dir).expect("create succeeds");
    // Every component of the view chain is a real directory, never a symlink.
    for component in [
        fixture.root.join(".work"),
        fixture.root.join(".work/apps/web/auth-fix"),
    ] {
        let metadata = std::fs::symlink_metadata(&component).expect("component exists");
        assert!(
            metadata.is_dir(),
            "{} is a real directory",
            component.display()
        );
    }
    assert!(
        view_engine_dir(&fixture.state_root, &fixture.root, &view_dir)
            .join(VIEW_ENGINE_DB_FILE)
            .is_file()
    );
}

#[test]
fn partial_accept_merges_only_selected_paths() {
    let fixture = Fixture::new("work-rpc-accept-partial");
    let env = fixture.env();
    let view_dir = fixture.view_dir();
    let base = create_view(&env, &view_dir).expect("create succeeds");
    std::fs::write(view_dir.join("feature-a.txt"), b"a\n").expect("edit a");
    std::fs::write(view_dir.join("feature-b.txt"), b"b\n").expect("edit b");
    std::fs::create_dir(view_dir.join("node_modules")).expect("ignored directory");
    std::fs::write(
        view_dir.join("node_modules/local-only.txt"),
        b"host scratch\n",
    )
    .expect("ignored local file");
    let concurrent = fixture
        .remote
        .publish_blob(&fixture.crypto, b"console.log('concurrent')\n");
    fixture.remote.publish_head(
        &fixture.crypto,
        &[
            (
                "apps",
                ManifestEntry::Directory {
                    mode: FileMode::new(0o040_755),
                },
            ),
            (
                "apps/web",
                ManifestEntry::Directory {
                    mode: FileMode::new(0o040_755),
                },
            ),
            (
                "apps/web/src",
                ManifestEntry::Directory {
                    mode: FileMode::new(0o040_755),
                },
            ),
            ("apps/web/src/index.ts", concurrent.clone()),
        ],
    );

    let outcome = accept_view_dir(
        &env,
        &view_dir,
        &base,
        &base,
        &["feature-a.txt".to_string()],
    )
    .expect("partial accept succeeds");
    assert_eq!(outcome.accepted_paths, vec!["feature-a.txt".to_string()]);
    assert!(outcome.conflict_asides.is_empty());

    let merged = fetch(&fixture, &outcome.published);
    assert!(
        merged
            .entries
            .contains_key(&WorkspacePath::new("apps/web/feature-a.txt"))
    );
    assert!(
        !merged
            .entries
            .contains_key(&WorkspacePath::new("apps/web/feature-b.txt"))
    );
    assert_eq!(
        std::fs::read(view_dir.join("node_modules/local-only.txt"))
            .expect("ignored file survives rebase"),
        b"host scratch\n"
    );

    let rebased = fetch_project(&fixture, &outcome.base);
    assert!(
        rebased
            .entries
            .contains_key(&WorkspacePath::new("feature-a.txt"))
    );
    assert!(
        !rebased
            .entries
            .contains_key(&WorkspacePath::new("apps/web/feature-a.txt"))
    );
    assert_eq!(
        rebased.entries.get(&WorkspacePath::new("src/index.ts")),
        Some(&concurrent)
    );
    assert_eq!(
        diff_manifests(&rebased, &fetch_project(&fixture, &outcome.overlay))
            .into_iter()
            .map(|change| change.path.as_str().to_string())
            .collect::<Vec<_>>(),
        vec!["feature-b.txt"]
    );

    // A new edit after the rebase must capture against the rebased ancestor,
    // retaining the concurrent workspace change and only adding this edit to
    // the still-unaccepted overlay.
    std::fs::write(view_dir.join("feature-c.txt"), b"c\n").expect("edit c after rebase");
    let second = accept_view_dir(
        &env,
        &view_dir,
        &outcome.base,
        &outcome.overlay,
        &["feature-b.txt".to_string()],
    )
    .expect("second partial accept uses the project-scoped base");
    assert_eq!(second.accepted_paths, vec!["feature-b.txt".to_string()]);
    let twice_merged = fetch(&fixture, &second.published);
    assert!(
        twice_merged
            .entries
            .contains_key(&WorkspacePath::new("apps/web/feature-a.txt"))
    );
    assert!(
        twice_merged
            .entries
            .contains_key(&WorkspacePath::new("apps/web/feature-b.txt"))
    );
    assert!(
        !twice_merged
            .entries
            .contains_key(&WorkspacePath::new("apps/web/apps/web/feature-a.txt"))
    );
    assert_eq!(
        twice_merged
            .entries
            .get(&WorkspacePath::new("apps/web/src/index.ts")),
        Some(&concurrent)
    );
    assert_eq!(
        diff_manifests(
            &fetch_project(&fixture, &second.base),
            &fetch_project(&fixture, &second.overlay),
        )
        .into_iter()
        .map(|change| change.path.as_str().to_string())
        .collect::<Vec<_>>(),
        vec!["feature-c.txt"],
    );
}

#[test]
fn partial_accept_preserves_an_edit_written_after_capture() {
    let fixture = Fixture::new("work-rpc-accept-concurrent-edit");
    let env = fixture.env();
    let view_dir = fixture.view_dir();
    let base = create_view(&env, &view_dir).expect("create succeeds");
    let feature = view_dir.join("feature.txt");
    std::fs::write(&feature, b"captured\n").expect("initial edit");
    fixture
        .remote
        .write_during_next_cas(feature.clone(), b"newer local edit\n");

    let outcome = accept_view_dir(&env, &view_dir, &base, &base, &["feature.txt".to_string()])
        .expect("remote accept succeeds");

    assert!(!outcome.local_rebase_pending);
    assert_eq!(
        std::fs::read(&feature).expect("newer edit survives"),
        b"newer local edit\n"
    );
    let reviewed = review_view_dir(&env, &view_dir, &outcome.base, &outcome.overlay)
        .expect("surviving edit remains reviewable");
    assert!(
        reviewed
            .changes
            .iter()
            .any(|change| change.path.as_str() == "feature.txt")
    );
}

#[test]
fn post_cas_rebase_failure_is_recoverable_and_not_reported_as_failed_accept() {
    let fixture = Fixture::new("work-rpc-accept-rebase-recovery");
    let env = fixture.env();
    let view_dir = fixture.view_dir();
    let base = create_view(&env, &view_dir).expect("create succeeds");
    std::fs::write(view_dir.join("feature.txt"), b"feature\n").expect("edit");
    fixture
        .remote
        .break_view_dir_during_next_cas(view_dir.clone());

    let pending = accept_view_dir(&env, &view_dir, &base, &base, &["feature.txt".to_string()])
        .expect("the remote accept remains a successful outcome");

    assert!(pending.local_rebase_pending);
    assert_eq!(pending.published, fixture.remote.head_key());
    assert!(
        accept_intent_path(&env, &view_dir).is_file(),
        "the accepted head receipt remains durable"
    );
    let intent_path = accept_intent_path(&env, &view_dir);
    let mut legacy_intent: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&intent_path).expect("read durable accept receipt"))
            .expect("decode durable accept receipt");
    legacy_intent
        .as_object_mut()
        .expect("receipt is an object")
        .remove("publishedRefVersion");
    std::fs::write(
        &intent_path,
        serde_json::to_vec(&legacy_intent).expect("encode prior receipt shape"),
    )
    .expect("write prior receipt shape");

    std::fs::remove_file(&view_dir).expect("remove injected blocker");
    std::fs::rename(view_dir.with_extension("recover"), &view_dir)
        .expect("restore view for recovery");
    let recovered = accept_view_dir(&env, &view_dir, &base, &base, &["feature.txt".to_string()])
        .expect("retry resumes the accepted intent");
    assert!(!recovered.local_rebase_pending);
    assert_eq!(recovered.published, pending.published);
    assert!(!accept_intent_path(&env, &view_dir).exists());
}

#[test]
fn post_cas_recovery_survives_a_later_workspace_head_advance() {
    let fixture = Fixture::new("work-rpc-accept-rebase-superseded");
    let env = fixture.env();
    let view_dir = fixture.view_dir();
    let base = create_view(&env, &view_dir).expect("create succeeds");
    let feature = view_dir.join("feature.txt");
    std::fs::write(&feature, b"feature\n").expect("edit");
    fixture
        .remote
        .break_view_dir_during_next_cas(view_dir.clone());

    let pending = accept_view_dir(&env, &view_dir, &base, &base, &["feature.txt".to_string()])
        .expect("the remote accept remains a successful outcome");
    assert!(pending.local_rebase_pending);

    let later_file = fixture
        .remote
        .publish_blob(&fixture.crypto, b"later workspace state\n");
    let later_head = fixture
        .remote
        .publish_head(&fixture.crypto, &[("README.md", later_file)]);
    assert_ne!(later_head, pending.published);

    std::fs::remove_file(&view_dir).expect("remove injected blocker");
    std::fs::rename(view_dir.with_extension("recover"), &view_dir)
        .expect("restore view for recovery");
    let recovered = accept_view_dir(&env, &view_dir, &base, &base, &["feature.txt".to_string()])
        .expect("history proves the earlier accept before finishing its local rebase");

    assert!(!recovered.local_rebase_pending);
    assert_eq!(recovered.published, pending.published);
    assert_eq!(fixture.remote.head_key(), later_head);
    let reviewed = review_view_dir(&env, &view_dir, &recovered.base, &recovered.overlay)
        .expect("the recovered view can be reviewed");
    assert!(
        reviewed.changes.is_empty(),
        "the accepted partial edit is no longer pending"
    );
    assert!(!accept_intent_path(&env, &view_dir).exists());
}

#[test]
fn post_cas_receipt_cleanup_failure_remains_a_successful_recoverable_accept() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new("work-rpc-accept-receipt-cleanup");
    let env = fixture.env();
    let view_dir = fixture.view_dir();
    let base = create_view(&env, &view_dir).expect("create succeeds");
    std::fs::write(view_dir.join("feature.txt"), b"feature\n").expect("edit");
    let intent_parent = accept_intent_path(&env, &view_dir)
        .parent()
        .expect("intent parent")
        .to_path_buf();
    fixture
        .remote
        .lock_directory_during_next_cas(intent_parent.clone());

    let pending = accept_view_dir(&env, &view_dir, &base, &base, &["feature.txt".to_string()])
        .expect("receipt cleanup cannot turn remote success into failure");
    assert!(pending.local_rebase_pending);
    assert_eq!(pending.published, fixture.remote.head_key());

    std::fs::set_permissions(&intent_parent, std::fs::Permissions::from_mode(0o700))
        .expect("restore intent directory");
    let recovered = accept_view_dir(&env, &view_dir, &base, &base, &["feature.txt".to_string()])
        .expect("retry clears the receipt");
    assert!(!recovered.local_rebase_pending);
}

#[test]
fn partial_accept_with_no_matching_paths_publishes_nothing() {
    let fixture = Fixture::new("work-rpc-accept-empty");
    let env = fixture.env();
    let view_dir = fixture.view_dir();
    let base = create_view(&env, &view_dir).expect("create succeeds");
    std::fs::write(view_dir.join("feature.txt"), b"new\n").expect("edit");

    let head_before = fixture.remote.head_key();
    let outcome = accept_view_dir(
        &env,
        &view_dir,
        &base,
        &base,
        &["missing/*.txt".to_string()],
    )
    .expect("no-match accept returns cleanly");
    assert!(outcome.accepted_paths.is_empty());
    assert_eq!(outcome.published, head_before);
    assert_eq!(fixture.remote.head_key(), head_before);
}
