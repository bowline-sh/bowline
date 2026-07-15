use super::permissions::MaterializedFilePermissions;
use super::*;
use bowline_core::git_worktree_link::denormalize_worktree_link_entry_bytes;
use fs2::FileExt;

const MATERIALIZATION_LOCK_DIRECTORY: &str = "materialization";
const MATERIALIZATION_LOCK_FILE: &str = "workspace.lock";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MaterializationBoundary {
    GuardAcquired,
    BeforeMutation,
    AfterMutation,
}

pub(super) struct MaterializationRequest<'a> {
    state_root: &'a Path,
    root: &'a Path,
    base: Option<&'a SnapshotContent>,
    target: &'a SnapshotContent,
    preserved_paths: &'a BTreeSet<String>,
    intentionally_absent_paths: &'a BTreeSet<String>,
    selected_path: Option<&'a str>,
}

impl<'a> MaterializationRequest<'a> {
    pub(super) fn all(
        state_root: &'a Path,
        root: &'a Path,
        base: Option<&'a SnapshotContent>,
        target: &'a SnapshotContent,
    ) -> Self {
        Self {
            state_root,
            root,
            base,
            target,
            preserved_paths: empty_paths(),
            intentionally_absent_paths: empty_paths(),
            selected_path: None,
        }
    }

    pub(super) fn excluding(
        state_root: &'a Path,
        root: &'a Path,
        base: Option<&'a SnapshotContent>,
        target: &'a SnapshotContent,
        preserved_paths: &'a BTreeSet<String>,
    ) -> Self {
        Self {
            state_root,
            root,
            base,
            target,
            preserved_paths,
            intentionally_absent_paths: empty_paths(),
            selected_path: None,
        }
    }

    #[cfg(test)]
    pub(super) fn omitting(
        state_root: &'a Path,
        root: &'a Path,
        base: Option<&'a SnapshotContent>,
        target: &'a SnapshotContent,
        intentionally_absent_paths: &'a BTreeSet<String>,
    ) -> Self {
        Self {
            state_root,
            root,
            base,
            target,
            preserved_paths: empty_paths(),
            intentionally_absent_paths,
            selected_path: None,
        }
    }

    pub(super) fn task(
        state_root: &'a Path,
        root: &'a Path,
        base: Option<&'a SnapshotContent>,
        target: &'a SnapshotContent,
        intentionally_absent_paths: &'a BTreeSet<String>,
        selected_path: &'a str,
    ) -> Self {
        Self {
            state_root,
            root,
            base,
            target,
            preserved_paths: empty_paths(),
            intentionally_absent_paths,
            selected_path: Some(selected_path),
        }
    }
}

struct WorkspaceMaterializationGuard {
    file: fs::File,
}

impl WorkspaceMaterializationGuard {
    fn acquire(state_root: &Path) -> Result<Self, SyncRunnerError> {
        let lock_root = state_root.join(MATERIALIZATION_LOCK_DIRECTORY);
        fs::create_dir_all(&lock_root).map_err(SyncRunnerError::StateIo)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&lock_root, fs::Permissions::from_mode(0o700))
                .map_err(SyncRunnerError::StateIo)?;
        }
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(lock_root.join(MATERIALIZATION_LOCK_FILE))
            .map_err(SyncRunnerError::StateIo)?;
        file.lock_exclusive().map_err(SyncRunnerError::StateIo)?;
        Ok(Self { file })
    }
}

impl Drop for WorkspaceMaterializationGuard {
    fn drop(&mut self) {
        if let Err(error) = FileExt::unlock(&self.file) {
            eprintln!("bowline-sync materialization lock release failed: {error}");
        }
    }
}

pub(super) fn materialize_snapshot_guarded(
    request: MaterializationRequest<'_>,
    mut authorize: impl FnMut(MaterializationBoundary) -> Result<(), SyncRunnerError>,
) -> Result<(), SyncRunnerError> {
    preflight_materialization_payloads(&request)?;
    let deletions = materialization_deletions(
        request.base,
        request.target,
        request.preserved_paths,
        request.intentionally_absent_paths,
        request.selected_path,
    )?;
    let _guard = WorkspaceMaterializationGuard::acquire(request.state_root)?;
    authorize(MaterializationBoundary::GuardAcquired)?;

    for entry in deletions.first {
        authorize(MaterializationBoundary::BeforeMutation)?;
        remove_materialized_entry(request.root, &entry)?;
        authorize(MaterializationBoundary::AfterMutation)?;
    }

    for phase in [
        MaterializationTargetPhase::Directory,
        MaterializationTargetPhase::ImmutableObject,
        MaterializationTargetPhase::OrdinaryWrite,
        MaterializationTargetPhase::PointerState,
    ] {
        visit_materialization_targets(
            request.target,
            request.preserved_paths,
            request.intentionally_absent_paths,
            request.selected_path,
            phase,
            |entry| {
                authorize(MaterializationBoundary::BeforeMutation)?;
                if entry.kind == NamespaceEntryKind::Directory {
                    ensure_directory_without_symlink(request.root, Path::new(&entry.path))?;
                } else {
                    write_materialized_entry(request.root, request.target, &entry)?;
                }
                authorize(MaterializationBoundary::AfterMutation)
            },
        )?;
    }

    for entry in deletions.last {
        authorize(MaterializationBoundary::BeforeMutation)?;
        remove_materialized_entry(request.root, &entry)?;
        authorize(MaterializationBoundary::AfterMutation)?;
    }
    Ok(())
}

fn preflight_materialization_payloads(
    request: &MaterializationRequest<'_>,
) -> Result<(), SyncRunnerError> {
    for phase in [
        MaterializationTargetPhase::ImmutableObject,
        MaterializationTargetPhase::OrdinaryWrite,
        MaterializationTargetPhase::PointerState,
    ] {
        visit_materialization_targets(
            request.target,
            request.preserved_paths,
            request.intentionally_absent_paths,
            request.selected_path,
            phase,
            |entry| {
                if entry.kind == NamespaceEntryKind::File
                    && entry.content_id.as_ref().is_none_or(|content_id| {
                        !request.target.prepared_content().contains_key(content_id)
                    })
                {
                    return Err(SyncRunnerError::MissingMaterializationContent(entry.path));
                }
                Ok(())
            },
        )?;
    }
    Ok(())
}

fn write_materialized_entry(
    root: &Path,
    target: &SnapshotContent,
    entry: &NamespaceEntry,
) -> Result<(), SyncRunnerError> {
    match entry.kind {
        NamespaceEntryKind::File => write_file_entry(root, target, entry),
        NamespaceEntryKind::Symlink => write_symlink_entry(root, entry),
        NamespaceEntryKind::Directory
        | NamespaceEntryKind::Placeholder
        | NamespaceEntryKind::Tombstone => Ok(()),
    }
}

fn write_file_entry(
    root: &Path,
    target: &SnapshotContent,
    entry: &NamespaceEntry,
) -> Result<(), SyncRunnerError> {
    let permissions = MaterializedFilePermissions::for_entry(
        &entry.path,
        entry.classification,
        entry.mode,
        entry.executability,
    );
    let relative_path = Path::new(&entry.path);
    let bytes = target
        .read_file_for_path(&entry.path)
        .map_err(SyncRunnerError::StateIo)?
        .ok_or_else(|| SyncRunnerError::MissingMaterializationContent(entry.path.clone()))?;
    let localized;
    let bytes = if portable_git_worktree_link_entry(entry).is_some() {
        localized = denormalize_worktree_link_entry_bytes(&entry.path, entry.kind, &bytes, root);
        localized.as_slice()
    } else {
        bytes.as_slice()
    };
    prepare_parent_dirs(root, relative_path)?;
    write_materialized_file(&root.join(relative_path), bytes, permissions)
}

fn write_symlink_entry(root: &Path, entry: &NamespaceEntry) -> Result<(), SyncRunnerError> {
    let Some(target_path) = &entry.symlink_target else {
        return Ok(());
    };
    validate_materialized_symlink_target(target_path)?;
    let relative_path = Path::new(&entry.path);
    prepare_parent_dirs(root, relative_path)?;
    write_materialized_symlink(&root.join(relative_path), target_path)
}

fn empty_paths() -> &'static BTreeSet<String> {
    static EMPTY: std::sync::OnceLock<BTreeSet<String>> = std::sync::OnceLock::new();
    EMPTY.get_or_init(BTreeSet::new)
}

#[cfg(test)]
pub(super) fn materialize_snapshot(
    root: &Path,
    base: Option<&SnapshotContent>,
    target: &SnapshotContent,
) -> Result<(), SyncRunnerError> {
    materialize_snapshot_for_test(MaterializationRequest::all(
        &root.join(".bowline-test-state"),
        root,
        base,
        target,
    ))
}

#[cfg(test)]
pub(super) fn materialize_snapshot_omitting(
    root: &Path,
    base: Option<&SnapshotContent>,
    target: &SnapshotContent,
    intentionally_absent_paths: &BTreeSet<String>,
) -> Result<(), SyncRunnerError> {
    materialize_snapshot_for_test(MaterializationRequest::omitting(
        &root.join(".bowline-test-state"),
        root,
        base,
        target,
        intentionally_absent_paths,
    ))
}

#[cfg(test)]
fn materialize_snapshot_for_test(
    request: MaterializationRequest<'_>,
) -> Result<(), SyncRunnerError> {
    materialize_snapshot_guarded(request, |_| Ok(()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::TempWorkspace;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    };
    use std::time::Duration;

    #[test]
    fn reclaimed_authority_cannot_overtake_or_resume_over_newer_materialization() {
        let workspace = TempWorkspace::new("materialization-claim-fence").expect("workspace");
        let root = workspace.root().to_path_buf();
        let state_root = root.join(".state");
        let workspace_id = WorkspaceId::new("ws_code");
        let stale = super::super::tests::snapshot_with_files(
            workspace_id.clone(),
            &[("src/value.txt", b"stale".as_slice())],
        );
        let current = super::super::tests::snapshot_with_files(
            workspace_id,
            &[("src/value.txt", b"current".as_slice())],
        );
        let authority_revoked = Arc::new(AtomicBool::new(false));
        let (stale_guarded_tx, stale_guarded_rx) = mpsc::channel();
        let (resume_stale_tx, resume_stale_rx) = mpsc::channel();
        let (current_started_tx, current_started_rx) = mpsc::channel();
        let (current_guarded_tx, current_guarded_rx) = mpsc::channel();

        std::thread::scope(|scope| {
            let revoked = Arc::clone(&authority_revoked);
            let stale_state_root = &state_root;
            let stale_root = &root;
            let stale_snapshot = &stale;
            let stale_worker = scope.spawn(move || {
                materialize_snapshot_guarded(
                    MaterializationRequest::all(stale_state_root, stale_root, None, stale_snapshot),
                    |boundary| {
                        if boundary == MaterializationBoundary::GuardAcquired {
                            stale_guarded_tx.send(()).expect("report stale guard");
                            resume_stale_rx.recv().expect("resume stale worker");
                        }
                        if revoked.load(Ordering::Acquire) {
                            Err(SyncRunnerError::SyncClaimOwnershipLost)
                        } else {
                            Ok(())
                        }
                    },
                )
            });
            stale_guarded_rx.recv().expect("stale guard acquired");

            let current_state_root = &state_root;
            let current_root = &root;
            let current_snapshot = &current;
            let current_worker = scope.spawn(move || {
                current_started_tx.send(()).expect("report current start");
                materialize_snapshot_guarded(
                    MaterializationRequest::all(
                        current_state_root,
                        current_root,
                        None,
                        current_snapshot,
                    ),
                    |boundary| {
                        if boundary == MaterializationBoundary::GuardAcquired {
                            current_guarded_tx.send(()).expect("report current guard");
                        }
                        Ok(())
                    },
                )
            });
            current_started_rx.recv().expect("current worker started");
            assert!(
                current_guarded_rx
                    .recv_timeout(Duration::from_millis(50))
                    .is_err(),
                "a replacement worker must not enter while the stale worker owns the guard"
            );

            authority_revoked.store(true, Ordering::Release);
            resume_stale_tx.send(()).expect("resume stale worker");
            assert!(matches!(
                stale_worker.join().expect("stale worker joined"),
                Err(SyncRunnerError::SyncClaimOwnershipLost)
            ));
            current_guarded_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("current worker acquires released guard");
            current_worker
                .join()
                .expect("current worker joined")
                .expect("current materialization");
        });

        assert_eq!(
            fs::read(root.join("src/value.txt")).expect("materialized bytes"),
            b"current"
        );
    }

    #[test]
    fn revoked_authority_before_guard_causes_zero_workspace_mutations() {
        let workspace = TempWorkspace::new("materialization-pre-guard-fence").expect("workspace");
        let root = workspace.root();
        let state_root = root.join(".state");
        let target = super::super::tests::snapshot_with_files(
            WorkspaceId::new("ws_code"),
            &[("src/value.txt", b"must-not-appear".as_slice())],
        );

        let result = materialize_snapshot_guarded(
            MaterializationRequest::all(&state_root, root, None, &target),
            |_| Err(SyncRunnerError::SyncClaimOwnershipLost),
        );

        assert!(matches!(
            result,
            Err(SyncRunnerError::SyncClaimOwnershipLost)
        ));
        assert!(!root.join("src/value.txt").exists());
    }

    #[test]
    fn after_mutation_boundary_precedes_authorization_for_the_next_path() {
        let workspace = TempWorkspace::new("materialization-mid-plan-cancel").expect("workspace");
        let root = workspace.root();
        let state_root = root.join(".state");
        let target = super::super::tests::snapshot_with_files(
            WorkspaceId::new("ws_code"),
            &[
                ("src/first.txt", b"first".as_slice()),
                ("src/second.txt", b"second".as_slice()),
            ],
        );
        let committed_mutations = Cell::new(0_u32);

        let result = materialize_snapshot_guarded(
            MaterializationRequest::all(&state_root, root, None, &target),
            |boundary| match boundary {
                MaterializationBoundary::AfterMutation => {
                    committed_mutations.set(committed_mutations.get() + 1);
                    Ok(())
                }
                MaterializationBoundary::BeforeMutation if committed_mutations.get() >= 1 => {
                    Err(SyncRunnerError::SyncOperationCancellationRequested)
                }
                MaterializationBoundary::GuardAcquired
                | MaterializationBoundary::BeforeMutation => Ok(()),
            },
        );

        assert!(matches!(
            result,
            Err(SyncRunnerError::SyncOperationCancellationRequested)
        ));
        assert_eq!(
            fs::read(root.join("src/first.txt")).expect("first"),
            b"first"
        );
        assert!(!root.join("src/second.txt").exists());
    }

    #[test]
    fn task_materialization_does_not_preflight_or_write_unselected_missing_content() {
        let workspace = TempWorkspace::new("materialization-task-incremental").expect("workspace");
        let root = workspace.root();
        let state_root = root.join(".state");
        let mut target = super::super::tests::snapshot_with_files(
            WorkspaceId::new("ws_code"),
            &[
                ("src/ready.txt", b"ready".as_slice()),
                ("src/missing.txt", b"missing".as_slice()),
            ],
        );
        let missing_content_id = target
            .entry_for_path("src/missing.txt")
            .expect("page read")
            .and_then(|entry| entry.content_id)
            .expect("missing content id");
        target.prepared_content_mut().remove(&missing_content_id);

        materialize_snapshot_guarded(
            MaterializationRequest::task(
                &state_root,
                root,
                None,
                &target,
                &BTreeSet::new(),
                "src/ready.txt",
            ),
            |_| Ok(()),
        )
        .expect("ready task materializes independently");
        assert_eq!(
            fs::read(root.join("src/ready.txt")).expect("ready bytes"),
            b"ready"
        );
        assert!(!root.join("src/missing.txt").exists());

        assert!(matches!(
            materialize_snapshot_guarded(
                MaterializationRequest::task(
                    &state_root,
                    root,
                    None,
                    &target,
                    &BTreeSet::new(),
                    "src/missing.txt",
                ),
                |_| Ok(()),
            ),
            Err(SyncRunnerError::MissingMaterializationContent(path))
                if path == "src/missing.txt"
        ));
    }
}
