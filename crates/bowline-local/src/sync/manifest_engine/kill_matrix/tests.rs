//! The kill-9 durability matrix (Plan 109 Step 6, review ADD 1 — binding).
//!
//! A parent test seeds a real store + a disk-backed remote, then re-invokes this
//! test binary as a child ([`kill_child`], selected by env vars). The child
//! drives the real apply stages up to a chosen barrier and `process::exit`s —
//! the deterministic stand-in for `kill -9`, since `exit` runs no destructors and
//! leaves exactly the on-disk state a crash would. The parent then reopens the
//! REAL store + filesystem, runs recovery + convergence through the production
//! `pull`, and asserts the durability invariants: every pre-existing user byte is
//! canonical / aside / preserved preimage; the target is old or complete-new,
//! never partial; recovery is idempotent; deletions do not resurrect; and no
//! remote plaintext is left behind in an orphan temp.
//!
//! The matrix runs across create, update, delete, mode-change, symlink, and
//! conflict-aside operations. `stage_write_temp` fuses temp *create* and *fsync*
//! (`write_private_file` fsyncs), so the two are one barrier (`AfterTempWrite`);
//! a crash strictly between them is dominated — an unfsynced temp is orphan
//! scratch, swept identically.

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::engine_test_support::{SharedRemote, open_store, test_context, test_crypto};
use super::fs_guard::{ParentChainMode, observe, prepare_parent_chain};
use super::manifest::{FileMode, ManifestEntry, ManifestKey, WorkspacePath};
use super::pull_apply::apply::build_intent;
use super::pull_apply::materialize::{
    TempFile, fsync_parent, materialize_aside, preserve_preimage, set_mode, stage_write_temp,
};
use super::pull_apply::{
    FsOp, FsOpKind, PullDeps, decide_head, entry_mode, pull, record_for_entry,
};
use super::push::{PushDeps, RemoteObjects, RemoteRef, push};
use super::store::AncestorCommit;
use crate::workspace::TempWorkspace;

const ENV_WORKSPACE: &str = "BOWLINE_KILL_WORKSPACE";
const ENV_REMOTE: &str = "BOWLINE_KILL_REMOTE";
const ENV_OP: &str = "BOWLINE_KILL_OP";
const ENV_BARRIER: &str = "BOWLINE_KILL_BARRIER";
const ENV_PATH: &str = "BOWLINE_KILL_PATH";

/// One device id for both the crashing child and the recovering parent, so a
/// conflict-aside name is identical across the process boundary.
const DEVICE: &str = "device-kill";
const CHILD_TEST: &str = "sync::manifest_engine::kill_matrix::kill_child";

// ---- operations + barriers --------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Create,
    Update,
    Delete,
    ModeChange,
    Symlink,
    ConflictAside,
}

impl Op {
    fn name(self) -> &'static str {
        match self {
            Op::Create => "create",
            Op::Update => "update",
            Op::Delete => "delete",
            Op::ModeChange => "mode-change",
            Op::Symlink => "symlink",
            Op::ConflictAside => "conflict-aside",
        }
    }

    fn target(self) -> &'static str {
        match self {
            Op::Symlink => "work/link",
            _ => "work/file.dat",
        }
    }

    /// The crash barriers that are reachable for this operation's apply sequence.
    fn barriers(self) -> &'static [Barrier] {
        use Barrier::*;
        match self {
            Op::Create => &[
                AfterTempWrite,
                AfterIntentCommit,
                AfterMutation,
                AfterParentFsync,
                BeforeOutcome,
                AfterOutcome,
            ],
            Op::Update => &[
                AfterTempWrite,
                AfterIntentCommit,
                AfterPreimagePreserved,
                AfterMutation,
                AfterParentFsync,
                BeforeOutcome,
                AfterOutcome,
            ],
            Op::Delete => &[
                AfterIntentCommit,
                AfterPreimagePreserved,
                AfterMutation,
                AfterParentFsync,
                BeforeOutcome,
                AfterOutcome,
            ],
            Op::ModeChange | Op::Symlink => &[
                AfterIntentCommit,
                AfterMutation,
                AfterParentFsync,
                BeforeOutcome,
                AfterOutcome,
            ],
            Op::ConflictAside => &[
                AfterTempWrite,
                AfterIntentCommit,
                AfterMutation,
                AfterParentFsync,
                BeforeOutcome,
                AfterOutcome,
            ],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Barrier {
    AfterTempWrite,
    AfterIntentCommit,
    AfterPreimagePreserved,
    AfterMutation,
    AfterParentFsync,
    BeforeOutcome,
    AfterOutcome,
}

impl Barrier {
    fn name(self) -> &'static str {
        match self {
            Barrier::AfterTempWrite => "after-temp-write",
            Barrier::AfterIntentCommit => "after-intent-commit",
            Barrier::AfterPreimagePreserved => "after-preimage-preserved",
            Barrier::AfterMutation => "after-mutation",
            Barrier::AfterParentFsync => "after-parent-fsync",
            Barrier::BeforeOutcome => "before-outcome",
            Barrier::AfterOutcome => "after-outcome",
        }
    }

    fn from_name(value: &str) -> Barrier {
        match value {
            "after-temp-write" => Barrier::AfterTempWrite,
            "after-intent-commit" => Barrier::AfterIntentCommit,
            "after-preimage-preserved" => Barrier::AfterPreimagePreserved,
            "after-mutation" => Barrier::AfterMutation,
            "after-parent-fsync" => Barrier::AfterParentFsync,
            "before-outcome" => Barrier::BeforeOutcome,
            "after-outcome" => Barrier::AfterOutcome,
            other => panic!("unknown barrier {other}"),
        }
    }
}

// ---- content fixtures -------------------------------------------------------

fn old_bytes(op: Op) -> Vec<u8> {
    format!("USER-ORIGINAL-{}-0123456789abcdef", op.name()).into_bytes()
}

/// The remote/incoming plaintext. Carries a `SECRET-REMOTE` marker so the
/// "no plaintext in orphan temps" invariant can scan for it.
fn new_bytes(op: Op) -> Vec<u8> {
    format!("SECRET-REMOTE-{}-fedcba9876543210", op.name()).into_bytes()
}

fn local_bytes(op: Op) -> Vec<u8> {
    format!("USER-LOCAL-EDIT-{}-zyxwvutsrqpo", op.name()).into_bytes()
}

// ---- parent tests (one per operation) ---------------------------------------

#[test]
fn kill_matrix_create() {
    run_op_matrix(Op::Create);
}

#[test]
fn kill_matrix_update() {
    run_op_matrix(Op::Update);
}

#[test]
fn kill_matrix_delete() {
    run_op_matrix(Op::Delete);
}

#[test]
fn kill_matrix_mode_change() {
    run_op_matrix(Op::ModeChange);
}

#[test]
fn kill_matrix_symlink() {
    run_op_matrix(Op::Symlink);
}

#[test]
fn kill_matrix_conflict_aside() {
    run_op_matrix(Op::ConflictAside);
}

fn run_op_matrix(op: Op) {
    for barrier in op.barriers() {
        run_cell(op, *barrier);
    }
}

fn run_cell(op: Op, barrier: Barrier) {
    let workspace = TempWorkspace::new(&format!("kill-{}-{}", op.name(), barrier.name()))
        .expect("temp workspace");
    let remote_dir = env::temp_dir().join(format!(
        "bowline-kill-remote-{}-{}-{}",
        std::process::id(),
        op.name(),
        barrier.name()
    ));
    let _ = fs::remove_dir_all(&remote_dir);

    let path = seed(op, workspace.root(), &remote_dir);
    spawn_child(workspace.root(), &remote_dir, op, barrier, &path);
    assert_recovers(op, barrier, workspace.root(), &remote_dir, &path);

    let _ = fs::remove_dir_all(&remote_dir);
}

// ---- seeding (parent) -------------------------------------------------------

fn seed(op: Op, root: &Path, remote_dir: &Path) -> String {
    let crypto = test_crypto();
    let ctx = test_context(root.to_path_buf(), DEVICE);
    let remote = SharedRemote::open(remote_dir.to_path_buf());
    let path = op.target().to_string();

    // Scope the store so its SQLite connection is dropped (WAL flushed) before the
    // child process opens the same database.
    {
        let mut store = open_store(root);
        match op {
            Op::Create => {
                let entry = remote.publish_blob(&crypto, &new_bytes(op));
                remote.publish(&crypto, &[(&path, entry)]);
            }
            Op::Update => {
                seed_pushed(&mut store, &remote, &ctx, &path, &old_bytes(op));
                let entry = remote.publish_blob(&crypto, &new_bytes(op));
                remote.publish(&crypto, &[(&path, entry)]);
            }
            Op::Delete => {
                seed_pushed(&mut store, &remote, &ctx, &path, &old_bytes(op));
                remote.publish(&crypto, &[]);
            }
            Op::ModeChange => {
                seed_pushed(&mut store, &remote, &ctx, &path, &old_bytes(op));
                let entry = mode_variant(&store, &path, 0o755);
                remote.publish(&crypto, &[(&path, entry)]);
            }
            Op::Symlink => {
                let entry = ManifestEntry::Symlink {
                    mode: FileMode::new(0o777),
                    target: "does/not/exist".to_string(),
                };
                remote.publish(&crypto, &[(&path, entry)]);
            }
            Op::ConflictAside => {
                seed_pushed(&mut store, &remote, &ctx, &path, &old_bytes(op));
                // A local edit diverges from the pushed ancestor before the remote
                // publishes its own different bytes: a genuine conflict.
                write_file(root, &path, &local_bytes(op));
                let entry = remote.publish_blob(&crypto, &new_bytes(op));
                remote.publish(&crypto, &[(&path, entry)]);
            }
        }
    }
    path
}

fn seed_pushed(
    store: &mut super::store::ManifestStore,
    remote: &SharedRemote,
    ctx: &super::push::EngineContext,
    path: &str,
    bytes: &[u8],
) {
    write_file(&ctx.workspace_root, path, bytes);
    let deps = PushDeps {
        ctx,
        objects: remote,
        refs: remote,
    };
    let dirty: BTreeSet<WorkspacePath> = [WorkspacePath::new(path)].into_iter().collect();
    push(store, &deps, &dirty).expect("seed push");
}

fn mode_variant(store: &super::store::ManifestStore, path: &str, mode: u32) -> ManifestEntry {
    let record = store.all_files().expect("files")[&WorkspacePath::new(path)].clone();
    ManifestEntry::File {
        size: record.size,
        mode: FileMode::new(mode),
        content_id: record.content_id.expect("content id"),
        blob_key: record.blob_key.expect("blob key"),
        key_epoch: record.key_epoch.expect("key epoch"),
    }
}

// ---- child (crash simulation) -----------------------------------------------

#[test]
#[ignore = "spawned by the kill_matrix_* parent tests with env-selected barriers"]
fn kill_child() {
    let Ok(root) = env::var(ENV_WORKSPACE) else {
        return; // Not spawned by a parent: nothing to do.
    };
    let root = PathBuf::from(root);
    let remote_dir = PathBuf::from(env::var(ENV_REMOTE).expect("remote env"));
    // The op is implicit in the head + target: the child re-derives the fs op from
    // the real classification, so only the barrier and target select behavior.
    let barrier = Barrier::from_name(&env::var(ENV_BARRIER).expect("barrier env"));
    let target = env::var(ENV_PATH).expect("path env");

    let ctx = test_context(root.clone(), DEVICE);
    let mut store = open_store(&root);
    let remote = SharedRemote::open(remote_dir);
    let deps = PullDeps {
        ctx: &ctx,
        objects: &remote,
        refs: &remote,
    };
    let head = remote
        .current_ref()
        .expect("seeded head must exist for the child");
    let plan = decide_head(&mut store, &deps, &head).expect("decide head");
    let op_fs = plan
        .fs_ops
        .into_iter()
        .find(|fs_op| fs_op.path.as_str() == target)
        .expect("an fs op for the target path");

    staged_apply(
        &mut store,
        &deps,
        &op_fs,
        &head.manifest_key,
        head.version,
        barrier,
    );
    // Fully completed (AfterOutcome) or the barrier was never hit: exit cleanly.
    std::process::exit(0);
}

/// Replays the real apply sequence stage by stage, calling `process::exit` at the
/// selected barrier. Every stage is a production `pub(crate)` apply primitive, so
/// this crashes at genuine on-disk boundaries rather than a test reimplementation.
fn staged_apply<O: RemoteObjects, R: RemoteRef>(
    store: &mut super::store::ManifestStore,
    deps: &PullDeps<'_, O, R>,
    op: &FsOp,
    manifest_key: &ManifestKey,
    ref_version: u64,
    barrier: Barrier,
) {
    let ctx = deps.ctx;
    let hit = |current: Barrier| {
        if current == barrier {
            std::process::exit(0);
        }
    };

    let temp = stage_write_temp(ctx, deps.objects, op).expect("stage temp");
    hit(Barrier::AfterTempWrite);
    store
        .open_intent(&build_intent(op, temp.as_ref(), manifest_key))
        .expect("open intent");
    hit(Barrier::AfterIntentCommit);

    let absolute = ctx.workspace_root.join(op.path.as_str());
    let observed = observe(&ctx.workspace_root, &op.path).expect("observe");
    let mut commit = AncestorCommit::default();

    match &op.kind {
        FsOpKind::Install(entry) => {
            prepare_parent_chain(
                &ctx.workspace_root,
                &op.path,
                ParentChainMode::CreateMissing,
            )
            .expect("prepare parent chain");
            let replacing = observed.is_some();
            if replacing {
                preserve_preimage(ctx, &op.path, &absolute).expect("preserve preimage");
            }
            hit(Barrier::AfterPreimagePreserved);
            install_bytes(ctx, &op.path, &absolute, entry, temp.as_ref(), replacing);
            hit(Barrier::AfterMutation);
            fsync_parent(&absolute).expect("fsync parent");
            hit(Barrier::AfterParentFsync);
            let fingerprint = observe(&ctx.workspace_root, &op.path)
                .expect("re-observe")
                .expect("installed target present")
                .fingerprint;
            commit
                .upserts
                .insert(op.path.clone(), record_for_entry(entry, fingerprint));
        }
        FsOpKind::Delete => {
            preserve_preimage(ctx, &op.path, &absolute).expect("preserve preimage");
            hit(Barrier::AfterPreimagePreserved);
            remove_path(&absolute);
            hit(Barrier::AfterMutation);
            fsync_parent(&absolute).expect("fsync parent");
            hit(Barrier::AfterParentFsync);
            commit.removals.insert(op.path.clone());
        }
        FsOpKind::ModeChange(entry) => {
            set_mode(&ctx.workspace_root, &op.path, entry_mode(entry)).expect("set mode");
            hit(Barrier::AfterMutation);
            fsync_parent(&absolute).expect("fsync parent");
            hit(Barrier::AfterParentFsync);
            let observed = observe(&ctx.workspace_root, &op.path)
                .expect("re-observe")
                .expect("mode target present");
            commit.upserts.insert(
                op.path.clone(),
                record_for_entry(entry, observed.fingerprint),
            );
        }
        FsOpKind::ConflictAside(entry) => {
            materialize_aside(ctx, deps.objects, &op.path, entry, temp).expect("materialize aside");
            hit(Barrier::AfterMutation);
            hit(Barrier::AfterParentFsync);
            // The original path is kept-local: its ancestor row is unchanged.
        }
    }

    hit(Barrier::BeforeOutcome);
    store
        .commit_pull_outcome(
            &commit,
            Some((manifest_key, ref_version)),
            Some((manifest_key, ref_version)),
            std::slice::from_ref(&op.path),
        )
        .expect("commit outcome");
    hit(Barrier::AfterOutcome);
}

fn install_bytes(
    ctx: &super::push::EngineContext,
    path: &WorkspacePath,
    absolute: &Path,
    entry: &ManifestEntry,
    temp: Option<&TempFile>,
    replacing: bool,
) {
    match entry {
        ManifestEntry::File { .. } => {
            let temp = temp.expect("file install has a temp");
            fs::rename(&temp.path, absolute).expect("atomic install rename");
        }
        ManifestEntry::Directory { mode } => {
            fs::create_dir_all(absolute).expect("mkdir");
            set_mode(&ctx.workspace_root, path, *mode).expect("dir mode");
        }
        ManifestEntry::Symlink { target, .. } => {
            if replacing {
                let _ = fs::remove_file(absolute);
            }
            std::os::unix::fs::symlink(target, absolute).expect("symlink");
        }
    }
}

fn remove_path(absolute: &Path) {
    match fs::symlink_metadata(absolute) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(absolute).expect("rmdir"),
        Ok(_) => fs::remove_file(absolute).expect("rm"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("remove failed: {error}"),
    }
}

// ---- spawn + recovery assertions (parent) -----------------------------------

fn spawn_child(root: &Path, remote_dir: &Path, op: Op, barrier: Barrier, path: &str) {
    let exe = env::current_exe().expect("current test binary");
    let status = Command::new(exe)
        .arg(CHILD_TEST)
        .arg("--exact")
        .arg("--ignored")
        .arg("--nocapture")
        .env(ENV_WORKSPACE, root)
        .env(ENV_REMOTE, remote_dir)
        .env(ENV_OP, op.name())
        .env(ENV_BARRIER, barrier.name())
        .env(ENV_PATH, path)
        .status()
        .expect("spawn kill child");
    assert!(
        status.success(),
        "kill child crashed non-zero for {}/{}",
        op.name(),
        barrier.name()
    );
}

fn assert_recovers(op: Op, barrier: Barrier, root: &Path, remote_dir: &Path, path: &str) {
    let ctx = test_context(root.to_path_buf(), DEVICE);
    let mut store = open_store(root);
    let remote = SharedRemote::open(remote_dir.to_path_buf());
    let deps = PullDeps {
        ctx: &ctx,
        objects: &remote,
        refs: &remote,
    };
    let label = format!("{}/{}", op.name(), barrier.name());

    // Recovery runs inside pull; a second pull proves idempotence and no
    // resurrection.
    pull(&mut store, &deps).unwrap_or_else(|error| panic!("recover+pull {label}: {error}"));
    pull(&mut store, &deps).unwrap_or_else(|error| panic!("second pull {label}: {error}"));

    assert!(
        store.pending_intents().expect("intents").is_empty(),
        "journal must be clear after recovery for {label}"
    );
    assert!(
        !tmp_contains(root, &new_bytes(op)),
        "no remote plaintext may be left in an orphan temp for {label}"
    );

    match op {
        Op::Create => {
            assert_eq!(read(root, path), new_bytes(op), "create target for {label}");
        }
        Op::Update => {
            assert_eq!(read(root, path), new_bytes(op), "update target for {label}");
            assert!(
                quarantine_contains(root, &old_bytes(op)),
                "preimage preserved for {label}"
            );
        }
        Op::Delete => {
            assert!(!exists(root, path), "deleted target absent for {label}");
            assert!(
                quarantine_contains(root, &old_bytes(op)),
                "preimage preserved for {label}"
            );
            // Deletions must not resurrect on the follow-on pull.
            assert!(
                !exists(root, path),
                "deletion does not resurrect for {label}"
            );
        }
        Op::ModeChange => {
            assert_eq!(
                read(root, path),
                old_bytes(op),
                "mode target content {label}"
            );
            assert_eq!(mode_of(root, path), 0o755, "mode applied for {label}");
            // The recovered ancestor row must be COMPLETE. A mode change moves no
            // content, so content_id/blob_key/key_epoch have to survive recovery;
            // a content-less row would kill the next push (AncestorRowMissing).
            let record = store.all_files().expect("files")[&WorkspacePath::new(path)].clone();
            assert!(
                record.content_id.is_some()
                    && record.blob_key.is_some()
                    && record.key_epoch.is_some(),
                "mode-change recovery left a complete ancestor row for {label}"
            );
            // Prove it end to end: a push of an unrelated file projects every
            // ancestor row through build_manifest and must succeed.
            write_file(root, "work/after.dat", format!("after-{label}").as_bytes());
            let push_deps = PushDeps {
                ctx: &ctx,
                objects: &remote,
                refs: &remote,
            };
            let dirty: BTreeSet<WorkspacePath> =
                [WorkspacePath::new("work/after.dat")].into_iter().collect();
            push(&mut store, &push_deps, &dirty)
                .unwrap_or_else(|error| panic!("post-recovery push {label}: {error}"));
        }
        Op::Symlink => {
            let meta = fs::symlink_metadata(root.join(path)).expect("symlink present");
            assert!(meta.file_type().is_symlink(), "symlink kind for {label}");
            assert_eq!(
                fs::read_link(root.join(path)).expect("readlink"),
                Path::new("does/not/exist"),
                "symlink target for {label}"
            );
        }
        Op::ConflictAside => {
            assert_eq!(read(root, path), local_bytes(op), "local kept for {label}");
            assert!(
                workspace_has_file_content(root, &new_bytes(op)),
                "remote materialized as an aside for {label}"
            );
        }
    }
}

// ---- filesystem probes ------------------------------------------------------

fn write_file(root: &Path, rel: &str, bytes: &[u8]) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("mkdir");
    }
    fs::write(&path, bytes).expect("write");
}

fn read(root: &Path, rel: &str) -> Vec<u8> {
    fs::read(root.join(rel)).expect("read target")
}

fn exists(root: &Path, rel: &str) -> bool {
    root.join(rel).exists()
}

fn mode_of(root: &Path, rel: &str) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::symlink_metadata(root.join(rel))
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777
}

fn tmp_contains(root: &Path, needle: &[u8]) -> bool {
    dir_has_file_content(&root.join(".bowline").join("tmp"), needle)
}

fn quarantine_contains(root: &Path, needle: &[u8]) -> bool {
    dir_has_file_content(&root.join(".bowline").join("quarantine"), needle)
}

fn dir_has_file_content(dir: &Path, needle: &[u8]) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file()
            && fs::read(&path)
                .map(|bytes| bytes == needle)
                .unwrap_or(false)
        {
            return true;
        }
    }
    false
}

/// Whether any regular file under the workspace (excluding private engine state)
/// has exactly `needle` bytes — used to find a conflict-aside by content.
fn workspace_has_file_content(root: &Path, needle: &[u8]) -> bool {
    fn walk(dir: &Path, needle: &[u8]) -> bool {
        let Ok(entries) = fs::read_dir(dir) else {
            return false;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.file_name().is_some_and(|name| name == ".bowline") {
                continue;
            }
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            if metadata.is_dir() {
                if walk(&path, needle) {
                    return true;
                }
            } else if metadata.is_file()
                && fs::read(&path)
                    .map(|bytes| bytes == needle)
                    .unwrap_or(false)
            {
                return true;
            }
        }
        false
    }
    walk(root, needle)
}
