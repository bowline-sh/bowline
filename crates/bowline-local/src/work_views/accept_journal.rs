use std::{
    fs, io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug)]
pub(super) struct AcceptJournal {
    main_root: PathBuf,
    backup_dir: PathBuf,
    entries: Vec<JournalEntry>,
}

#[derive(Debug)]
enum JournalEntry {
    Backup { original: PathBuf, backup: PathBuf },
    CreatedFile { path: PathBuf },
    CreatedDir { path: PathBuf },
}

impl AcceptJournal {
    pub(super) fn create(namespace_root: &Path, main_root: &Path) -> io::Result<Self> {
        fs::create_dir_all(namespace_root)?;
        let process_id = std::process::id();
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos();

        for attempt in 0..100_u32 {
            let backup_dir = namespace_root.join(format!(
                ".accept-journal-{process_id}-{timestamp}-{attempt}"
            ));
            match fs::create_dir(&backup_dir) {
                Ok(()) => {
                    return Ok(Self {
                        main_root: main_root.to_path_buf(),
                        backup_dir,
                        entries: Vec::new(),
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }

        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "accept journal path collision limit reached",
        ))
    }

    pub(super) fn backup_dir(&self) -> &Path {
        &self.backup_dir
    }

    pub(super) fn backup_existing(&mut self, path: &Path) -> io::Result<()> {
        match fs::symlink_metadata(path) {
            Ok(_) => {
                let backup = self.backup_path_for(path)?;
                if let Some(parent) = backup.parent() {
                    fs::create_dir_all(parent)?;
                }
                // Accept mutates the canonical project in place. The journal
                // relies on `.work` being co-located with the project so these
                // renames are same-filesystem, atomic moves into scratch space.
                fs::rename(path, &backup)?;
                self.entries.push(JournalEntry::Backup {
                    original: path.to_path_buf(),
                    backup,
                });
                Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    pub(super) fn record_created(&mut self, path: &Path) {
        self.entries.push(JournalEntry::CreatedFile {
            path: path.to_path_buf(),
        });
    }

    pub(super) fn record_created_dir(&mut self, path: &Path) {
        self.entries.push(JournalEntry::CreatedDir {
            path: path.to_path_buf(),
        });
    }

    pub(super) fn commit(self) -> io::Result<()> {
        fs::remove_dir_all(self.backup_dir)
    }

    pub(super) fn rollback(mut self) -> Result<(), (PathBuf, io::Error)> {
        while let Some(entry) = self.entries.pop() {
            match entry {
                JournalEntry::CreatedFile { path } => {
                    if let Err(error) = remove_created_file(&path) {
                        return Err((path, error));
                    }
                }
                JournalEntry::CreatedDir { path } => {
                    if let Err(error) = remove_created_dir_if_empty(&path) {
                        return Err((path, error));
                    }
                }
                JournalEntry::Backup { original, backup } => {
                    if let Some(parent) = original.parent()
                        && let Err(error) = fs::create_dir_all(parent)
                    {
                        return Err((parent.to_path_buf(), error));
                    }
                    if let Err(error) = fs::rename(&backup, &original) {
                        return Err((original, error));
                    }
                }
            }
        }
        if let Err(error) = fs::remove_dir_all(&self.backup_dir) {
            return Err((self.backup_dir, error));
        }
        Ok(())
    }

    fn backup_path_for(&self, path: &Path) -> io::Result<PathBuf> {
        let relative = path.strip_prefix(&self.main_root).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("accepted path is outside the project: {error}"),
            )
        })?;
        Ok(self.backup_dir.join(relative))
    }
}

fn remove_created_file(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            fs::remove_dir_all(path)
        }
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn remove_created_dir_if_empty(path: &Path) -> io::Result<()> {
    match fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(error),
    }
}
