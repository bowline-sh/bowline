use std::{
    fs,
    io::{self, Write as _},
    path::{Path, PathBuf},
};

const TRANSACTION_PREFIX: &str = ".accept-transaction-";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AcceptCheckpoint {
    Staged,
    Committing,
    Published,
}

impl AcceptCheckpoint {
    fn as_str(self) -> &'static str {
        match self {
            Self::Staged => "staged",
            Self::Committing => "committing",
            Self::Published => "published",
        }
    }

    fn parse(value: &str) -> io::Result<Self> {
        match value.trim() {
            "staged" => Ok(Self::Staged),
            "committing" => Ok(Self::Committing),
            "published" => Ok(Self::Published),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "work-view accept checkpoint is invalid",
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AcceptRecovery {
    Rebuild,
    Published,
}

/// A same-filesystem directory transaction for one work-view accept.
///
/// The staged tree is complete before `publish` starts. The checkpoint is
/// persisted before the first rename, so a later invocation can deterministically
/// finish either rename without replaying path-level mutations.
#[derive(Debug)]
pub(crate) struct AcceptTransaction {
    root: PathBuf,
    main_root: PathBuf,
    staged_root: PathBuf,
    backup_root: PathBuf,
    checkpoint_path: PathBuf,
}

impl AcceptTransaction {
    pub(crate) fn open(
        namespace_root: &Path,
        main_root: &Path,
        work_view_id: &str,
    ) -> io::Result<Self> {
        fs::create_dir_all(namespace_root)?;
        let root = transaction_root(namespace_root, main_root, work_view_id)?;
        Ok(Self {
            staged_root: root.join("staged"),
            backup_root: root.join("previous-main"),
            checkpoint_path: root.join("checkpoint"),
            root,
            main_root: main_root.to_path_buf(),
        })
    }

    pub(crate) fn recover(&self) -> io::Result<AcceptRecovery> {
        let Some(checkpoint) = self.checkpoint()? else {
            return Ok(AcceptRecovery::Rebuild);
        };
        match checkpoint {
            AcceptCheckpoint::Staged => Ok(AcceptRecovery::Rebuild),
            AcceptCheckpoint::Committing => self.recover_commit(),
            AcceptCheckpoint::Published => {
                if self.main_root.is_dir() {
                    Ok(AcceptRecovery::Published)
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "published work-view accept is missing the main tree",
                    ))
                }
            }
        }
    }

    pub(crate) fn stage(&self, build: impl FnOnce(&Path) -> io::Result<()>) -> io::Result<()> {
        self.reset_uncommitted()?;
        fs::create_dir_all(&self.staged_root)?;
        let result = build(&self.staged_root).and_then(|()| sync_tree(&self.staged_root));
        if let Err(error) = result {
            let cleanup = fs::remove_dir_all(&self.staged_root);
            return match cleanup {
                Ok(()) => Err(error),
                Err(cleanup_error) => Err(io::Error::other(format!(
                    "staging failed: {error}; cleanup failed: {cleanup_error}"
                ))),
            };
        }
        self.write_checkpoint(AcceptCheckpoint::Staged)
    }

    pub(crate) fn publish(&self, authorize: impl FnOnce() -> io::Result<()>) -> io::Result<()> {
        authorize()?;
        if self.checkpoint()? != Some(AcceptCheckpoint::Staged) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "work-view accept is not staged",
            ));
        }
        self.write_checkpoint(AcceptCheckpoint::Committing)?;
        if self.backup_root.exists() {
            fs::remove_dir_all(&self.backup_root)?;
        }
        fs::rename(&self.main_root, &self.backup_root)?;
        sync_parent(&self.main_root)?;
        fs::rename(&self.staged_root, &self.main_root)?;
        sync_parent(&self.main_root)?;
        self.write_checkpoint(AcceptCheckpoint::Published)
    }

    pub(crate) fn complete(self) -> io::Result<()> {
        if self.checkpoint()? != Some(AcceptCheckpoint::Published) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "cannot complete an unpublished work-view accept",
            ));
        }
        if self.backup_root.exists() {
            fs::remove_dir_all(&self.backup_root)?;
        }
        fs::remove_dir_all(self.root)
    }

    fn recover_commit(&self) -> io::Result<AcceptRecovery> {
        match (
            self.main_root.exists(),
            self.staged_root.exists(),
            self.backup_root.exists(),
        ) {
            (true, true, false) => Ok(AcceptRecovery::Rebuild),
            (false, true, true) => {
                fs::rename(&self.staged_root, &self.main_root)?;
                sync_parent(&self.main_root)?;
                self.write_checkpoint(AcceptCheckpoint::Published)?;
                Ok(AcceptRecovery::Published)
            }
            (true, false, true) => {
                self.write_checkpoint(AcceptCheckpoint::Published)?;
                Ok(AcceptRecovery::Published)
            }
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "work-view accept transaction has an ambiguous directory state",
            )),
        }
    }

    fn reset_uncommitted(&self) -> io::Result<()> {
        if self.checkpoint()? == Some(AcceptCheckpoint::Published) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "work-view accept is already published",
            ));
        }
        if self.backup_root.exists() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "work-view accept backup requires recovery",
            ));
        }
        if self.root.exists() {
            fs::remove_dir_all(&self.root)?;
        }
        fs::create_dir_all(&self.root)
    }

    fn checkpoint(&self) -> io::Result<Option<AcceptCheckpoint>> {
        match fs::read_to_string(&self.checkpoint_path) {
            Ok(value) => AcceptCheckpoint::parse(&value).map(Some),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn write_checkpoint(&self, checkpoint: AcceptCheckpoint) -> io::Result<()> {
        fs::create_dir_all(&self.root)?;
        let pending = self.root.join("checkpoint.pending");
        let mut file = fs::File::create(&pending)?;
        file.write_all(checkpoint.as_str().as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);
        fs::rename(pending, &self.checkpoint_path)?;
        sync_parent(&self.checkpoint_path)
    }
}

fn transaction_root(
    namespace_root: &Path,
    main_root: &Path,
    work_view_id: &str,
) -> io::Result<PathBuf> {
    let identity = format!("{}\0{work_view_id}", main_root.display());
    let name = format!("{TRANSACTION_PREFIX}{}", stable_path_component(&identity));
    if namespace_root.starts_with(main_root) {
        return main_root
            .parent()
            .map(|parent| parent.join(name))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "root-project accept has no same-filesystem transaction parent",
                )
            });
    }
    Ok(namespace_root.join(name))
}

fn stable_path_component(value: &str) -> String {
    let hash = blake3::hash(value.as_bytes());
    hash.to_hex()[..20].to_string()
}

pub(super) fn sync_tree(root: &Path) -> io::Result<()> {
    let mut children = fs::read_dir(root)?.collect::<Result<Vec<_>, _>>()?;
    children.sort_by_key(fs::DirEntry::file_name);
    for child in children {
        let metadata = fs::symlink_metadata(child.path())?;
        if metadata.is_dir() {
            sync_tree(&child.path())?;
        } else if metadata.is_file() {
            fs::File::open(child.path())?.sync_all()?;
        }
    }
    fs::File::open(root)?.sync_all()
}

pub(super) fn sync_parent(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "accept path has no parent"))?;
    fs::File::open(parent)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work_views::safe_materialization::SafeMaterializationRoot;
    use crate::workspace::TempWorkspace;

    #[cfg(unix)]
    #[test]
    fn failed_symlink_stage_is_cleaned_and_retryable() {
        use std::os::unix::fs::symlink;

        let temp = TempWorkspace::new("accept-transaction-symlink-failure").expect("temp");
        let main = temp.root().join("Code/apps/web");
        let namespace = temp.root().join("Code/.work/apps/web");
        let outside = temp.root().join("outside");
        fs::create_dir_all(&main).expect("main");
        fs::create_dir_all(&outside).expect("outside");
        fs::write(outside.join("sentinel"), "untouched").expect("sentinel");
        let transaction =
            AcceptTransaction::open(&namespace, &main, "view-stage-cleanup").expect("transaction");

        let error = transaction
            .stage(|staged| {
                symlink(&outside, staged.join("link"))?;
                SafeMaterializationRoot::new(staged)?.remove(Path::new("link/sentinel"), false)
            })
            .expect_err("stage must fail");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(!transaction.staged_root.exists());
        assert_eq!(transaction.checkpoint().expect("checkpoint"), None);
        assert_eq!(fs::read(outside.join("sentinel")).unwrap(), b"untouched");
        assert!(main.is_dir());
        transaction
            .stage(|staged| fs::write(staged.join("safe"), "retry"))
            .expect("retry");
    }

    #[cfg(unix)]
    #[test]
    fn sync_tree_treats_symlinked_directory_as_a_leaf() {
        use std::os::unix::fs::symlink;

        let temp = TempWorkspace::new("accept-sync-symlink-leaf").expect("temp");
        let staged = temp.root().join("staged");
        let outside = temp.root().join("outside");
        fs::create_dir_all(&staged).expect("staged");
        fs::create_dir_all(&outside).expect("outside");
        symlink(&outside, staged.join("legitimate-link")).expect("symlink");

        sync_tree(&staged).expect("sync does not traverse symlink");

        assert_eq!(
            fs::read_link(staged.join("legitimate-link")).unwrap(),
            outside
        );
    }

    #[test]
    fn root_project_transaction_uses_a_sibling_and_publishes() {
        let temp = TempWorkspace::new("accept-transaction-root-project").expect("temp");
        let main = temp.root().join("Code");
        let namespace = main.join(".work/root-view");
        fs::create_dir_all(&namespace).expect("namespace");
        fs::write(main.join("before"), "before\n").expect("main file");
        let transaction =
            AcceptTransaction::open(&namespace, &main, "root-view").expect("transaction");

        assert!(!transaction.root.starts_with(&main));
        transaction
            .stage(|staged| fs::write(staged.join("after"), "after\n"))
            .expect("stage");
        transaction.publish(|| Ok(())).expect("publish");

        assert!(!main.join("before").exists());
        assert_eq!(fs::read(main.join("after")).unwrap(), b"after\n");
        assert_eq!(transaction.recover().unwrap(), AcceptRecovery::Published);
        transaction.complete().expect("complete");
    }

    #[test]
    fn authorization_failure_leaves_main_byte_identical() {
        let temp = TempWorkspace::new("accept-transaction-fence").expect("temp");
        let main = temp.root().join("Code/apps/web");
        let namespace = temp.root().join("Code/.work/apps/web");
        fs::create_dir_all(&main).expect("main");
        fs::write(main.join("value.txt"), "before\n").expect("main file");
        let transaction =
            AcceptTransaction::open(&namespace, &main, "view-fence").expect("transaction");
        transaction
            .stage(|staged| fs::write(staged.join("value.txt"), "after\n"))
            .expect("stage");

        let error = transaction
            .publish(|| Err(io::Error::other("main fence changed")))
            .expect_err("fence must reject");

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(fs::read(main.join("value.txt")).unwrap(), b"before\n");
        assert_eq!(transaction.recover().unwrap(), AcceptRecovery::Rebuild);
        transaction
            .stage(|staged| fs::write(staged.join("value.txt"), "after\n"))
            .expect("restage after rejected fence");
        transaction.publish(|| Ok(())).expect("retry publishes");
        assert_eq!(fs::read(main.join("value.txt")).unwrap(), b"after\n");
        transaction.complete().expect("complete retry");
    }

    #[test]
    fn committing_checkpoint_recovers_without_replaying_mutations() {
        let temp = TempWorkspace::new("accept-transaction-recover").expect("temp");
        let main = temp.root().join("Code/apps/web");
        let namespace = temp.root().join("Code/.work/apps/web");
        fs::create_dir_all(&main).expect("main");
        fs::write(main.join("value.txt"), "before\n").expect("main file");
        let transaction =
            AcceptTransaction::open(&namespace, &main, "view-recover").expect("transaction");
        transaction
            .stage(|staged| fs::write(staged.join("value.txt"), "after\n"))
            .expect("stage");
        transaction
            .write_checkpoint(AcceptCheckpoint::Committing)
            .expect("checkpoint");
        fs::rename(&transaction.main_root, &transaction.backup_root).expect("backup main");

        assert_eq!(transaction.recover().unwrap(), AcceptRecovery::Published);
        assert_eq!(fs::read(main.join("value.txt")).unwrap(), b"after\n");
        assert_eq!(transaction.recover().unwrap(), AcceptRecovery::Published);
        transaction.complete().expect("complete");
        assert!(fs::read_dir(namespace).expect("namespace").all(|entry| {
            !entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .starts_with(TRANSACTION_PREFIX)
        }));
    }
}
