use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AtomicWriteOptions {
    pub unix_mode: Option<u32>,
    pub reject_symlink: bool,
    pub replace_existing: bool,
}

impl Default for AtomicWriteOptions {
    fn default() -> Self {
        Self {
            unix_mode: None,
            reject_symlink: false,
            replace_existing: true,
        }
    }
}

pub fn write_atomic(
    final_path: &Path,
    bytes: &[u8],
    options: AtomicWriteOptions,
) -> io::Result<()> {
    write_atomic_with(final_path, options, |file| file.write_all(bytes))
}

pub fn write_atomic_with<T>(
    final_path: &Path,
    options: AtomicWriteOptions,
    write: impl FnOnce(&mut fs::File) -> io::Result<T>,
) -> io::Result<T> {
    if options.reject_symlink
        && fs::symlink_metadata(final_path)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "atomic write destination is a symlink",
        ));
    }

    let (staged, value) = stage_atomic_with(final_path, options, write)?;
    if options.replace_existing {
        staged.commit_replace()?;
    } else {
        staged.commit_no_replace()?;
    }
    Ok(value)
}

/// A fully written and fsynced file waiting for an atomic namespace commit.
///
/// Dropping an uncommitted value removes its private sibling. Callers that
/// need to preserve a displaced preimage can therefore stage once, perform
/// their authority checks, and select the appropriate commit primitive.
#[derive(Debug)]
pub struct StagedAtomicFile {
    temp_path: PathBuf,
    final_path: PathBuf,
    committed: bool,
}

impl StagedAtomicFile {
    pub fn staged_path(&self) -> &Path {
        &self.temp_path
    }

    pub fn final_path(&self) -> &Path {
        &self.final_path
    }

    pub fn commit_replace(mut self) -> io::Result<()> {
        fs::rename(&self.temp_path, &self.final_path)?;
        self.committed = true;
        sync_parent_for_path(&self.final_path)
    }

    /// Atomically installs the staged file only while the destination is
    /// absent. A concurrently created path is never replaced.
    pub fn commit_no_replace(mut self) -> io::Result<()> {
        let final_path = self.final_path.clone();
        self.commit_no_replace_inner(&final_path)
    }

    /// Installs at a separately selected destination without replacing it.
    /// Cross-device staging is copied into a fsynced destination sibling only
    /// after the destination parent is confirmed to remain an existing,
    /// non-symlink directory.
    pub fn commit_no_replace_at(mut self, final_path: &Path) -> io::Result<()> {
        self.commit_no_replace_inner(final_path)
    }

    fn commit_no_replace_inner(&mut self, final_path: &Path) -> io::Result<()> {
        self.commit_no_replace_inner_with(final_path, |source, destination| {
            fs::hard_link(source, destination)
        })
    }

    fn commit_no_replace_inner_with(
        &mut self,
        final_path: &Path,
        link: impl FnOnce(&Path, &Path) -> io::Result<()>,
    ) -> io::Result<()> {
        match link(&self.temp_path, final_path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::CrossesDevices => {
                self.commit_cross_device_no_replace(final_path)?;
                return Ok(());
            }
            Err(error) => return Err(error),
        }
        fs::remove_file(&self.temp_path)?;
        sync_parent_for_path(&self.temp_path)?;
        self.committed = true;
        sync_parent_for_path(final_path)
    }

    fn commit_cross_device_no_replace(&mut self, final_path: &Path) -> io::Result<()> {
        match fs::symlink_metadata(final_path) {
            Ok(_) => return Err(io::Error::from(io::ErrorKind::AlreadyExists)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let metadata = fs::symlink_metadata(&self.temp_path)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cross-device atomic source is not a regular file",
            ));
        }
        #[cfg(unix)]
        let unix_mode = {
            use std::os::unix::fs::PermissionsExt;
            Some(metadata.permissions().mode() & 0o777)
        };
        #[cfg(not(unix))]
        let unix_mode = None;
        let mut source = fs::File::open(&self.temp_path)?;
        let (destination_staged, ()) = stage_atomic_with_existing_parent(
            final_path,
            AtomicWriteOptions {
                unix_mode,
                reject_symlink: true,
                replace_existing: false,
            },
            |destination| {
                io::copy(&mut source, destination)?;
                Ok(())
            },
        )?;
        destination_staged.commit_no_replace()?;
        fs::remove_file(&self.temp_path)?;
        sync_parent_for_path(&self.temp_path)?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for StagedAtomicFile {
    fn drop(&mut self) {
        if !self.committed {
            let _cleanup_attempt = fs::remove_file(&self.temp_path);
        }
    }
}

pub fn stage_atomic(
    final_path: &Path,
    bytes: &[u8],
    options: AtomicWriteOptions,
) -> io::Result<StagedAtomicFile> {
    stage_atomic_with(final_path, options, |file| file.write_all(bytes)).map(|(staged, ())| staged)
}

pub fn stage_atomic_with<T>(
    final_path: &Path,
    options: AtomicWriteOptions,
    write: impl FnOnce(&mut fs::File) -> io::Result<T>,
) -> io::Result<(StagedAtomicFile, T)> {
    stage_atomic_with_parent_policy(final_path, options, true, write)
}

pub fn stage_atomic_existing_parent(
    final_path: &Path,
    bytes: &[u8],
    options: AtomicWriteOptions,
) -> io::Result<StagedAtomicFile> {
    stage_atomic_with_existing_parent(final_path, options, |file| file.write_all(bytes))
        .map(|(staged, ())| staged)
}

fn stage_atomic_with_existing_parent<T>(
    final_path: &Path,
    options: AtomicWriteOptions,
    write: impl FnOnce(&mut fs::File) -> io::Result<T>,
) -> io::Result<(StagedAtomicFile, T)> {
    stage_atomic_with_parent_policy(final_path, options, false, write)
}

fn stage_atomic_with_parent_policy<T>(
    final_path: &Path,
    options: AtomicWriteOptions,
    create_parent: bool,
    write: impl FnOnce(&mut fs::File) -> io::Result<T>,
) -> io::Result<(StagedAtomicFile, T)> {
    if options.reject_symlink
        && fs::symlink_metadata(final_path)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "atomic write destination is a symlink",
        ));
    }
    let parent = final_path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "atomic write destination has no parent directory",
        )
    })?;
    if create_parent {
        fs::create_dir_all(parent)?;
    } else {
        let metadata = fs::symlink_metadata(parent)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "atomic destination parent is not a directory",
            ));
        }
    }
    let temp_path = temp_sibling(final_path)?;
    remove_file_if_present(&temp_path)?;
    let result = (|| {
        let mut file = create_temp_file(&temp_path, options)?;
        let value = write(&mut file)?;
        file.sync_all()?;
        drop(file);
        Ok((
            StagedAtomicFile {
                temp_path: temp_path.clone(),
                final_path: final_path.to_path_buf(),
                committed: false,
            },
            value,
        ))
    })();
    if result.is_err() {
        let _cleanup_attempt = fs::remove_file(temp_path);
    }
    result
}

pub fn sync_parent_for_path(path: &Path) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "atomic path has no parent directory",
        )
    })?;
    sync_parent_dir(parent)
}

fn temp_sibling(final_path: &Path) -> io::Result<PathBuf> {
    let parent = final_path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "atomic write destination has no parent directory",
        )
    })?;
    let file_name = final_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let sequence = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    Ok(parent.join(format!(
        ".{file_name}.{}.{}.bowline-tmp",
        std::process::id(),
        sequence
    )))
}

fn remove_file_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn create_temp_file(path: &Path, options: AtomicWriteOptions) -> io::Result<fs::File> {
    let mut open_options = fs::OpenOptions::new();
    open_options.write(true).create_new(true);
    #[cfg(unix)]
    if let Some(mode) = options.unix_mode {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        open_options.mode(mode);
        let file = open_options.open(path)?;
        file.set_permissions(fs::Permissions::from_mode(mode))?;
        return Ok(file);
    }
    open_options.open(path)
}

fn sync_parent_dir(parent: &Path) -> io::Result<()> {
    match fs::File::open(parent).and_then(|directory| directory.sync_all()) {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::Unsupported | io::ErrorKind::InvalidInput
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::{self, Write},
        path::{Path, PathBuf},
    };

    use super::{AtomicWriteOptions, stage_atomic, write_atomic, write_atomic_with};

    #[test]
    fn write_atomic_commits_complete_file_and_removes_temp() {
        let temp = TempDir::new("fs-atomic-success");
        let path = temp.path().join("state.json");

        write_atomic(&path, b"complete", AtomicWriteOptions::default()).expect("write succeeds");

        assert_eq!(fs::read(&path).expect("read final"), b"complete");
        assert_no_temp_siblings(temp.path());
    }

    #[test]
    fn write_atomic_failure_removes_temp_and_keeps_original() {
        let temp = TempDir::new("fs-atomic-failure");
        let path = temp.path().join("state.json");
        fs::write(&path, b"original").expect("seed original");

        let error = write_atomic_with(&path, AtomicWriteOptions::default(), |file| {
            file.write_all(b"partial")?;
            Err::<(), io::Error>(io::Error::other("injected failure"))
        })
        .expect_err("write fails");

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(fs::read(&path).expect("read original"), b"original");
        assert_no_temp_siblings(temp.path());
    }

    #[test]
    fn staged_no_replace_preserves_concurrently_created_destination() {
        let temp = TempDir::new("fs-atomic-no-replace-race");
        let path = temp.path().join("state.json");
        let staged = stage_atomic(&path, b"daemon target", AtomicWriteOptions::default())
            .expect("stage target");

        fs::write(&path, b"newer user bytes").expect("concurrent destination");
        let error = staged
            .commit_no_replace()
            .expect_err("newer destination wins");

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read(&path).expect("read concurrent destination"),
            b"newer user bytes"
        );
        assert_no_temp_siblings(temp.path());
    }

    #[test]
    fn cross_device_no_replace_fallback_preserves_bytes_and_race_authority() {
        let staging = TempDir::new("fs-atomic-cross-device-staging");
        let destination = TempDir::new("fs-atomic-cross-device-destination");
        let staged_path = staging.path().join("state.json");
        let final_path = destination.path().join("state.json");
        let mut staged = stage_atomic(
            &staged_path,
            b"daemon target",
            AtomicWriteOptions {
                unix_mode: Some(0o600),
                reject_symlink: true,
                replace_existing: false,
            },
        )
        .expect("stage target");

        staged
            .commit_no_replace_inner_with(&final_path, |_source, _destination| {
                Err(io::Error::from(io::ErrorKind::CrossesDevices))
            })
            .expect("fallback commit");

        assert_eq!(
            fs::read(&final_path).expect("read target"),
            b"daemon target"
        );
        assert!(!staged_path.exists());

        let mut second = stage_atomic(
            &staged_path,
            b"must not overwrite",
            AtomicWriteOptions::default(),
        )
        .expect("stage second target");
        let error = second
            .commit_no_replace_inner_with(&final_path, |_source, _destination| {
                Err(io::Error::from(io::ErrorKind::CrossesDevices))
            })
            .expect_err("existing destination wins");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read(&final_path).expect("read winner"),
            b"daemon target"
        );
    }

    #[test]
    fn dropping_staged_file_removes_uncommitted_bytes() {
        let temp = TempDir::new("fs-atomic-staged-drop");
        let path = temp.path().join("state.json");
        let staged = stage_atomic(&path, b"uncommitted", AtomicWriteOptions::default())
            .expect("stage target");
        let staged_path = staged.staged_path().to_path_buf();

        drop(staged);

        assert!(!staged_path.exists());
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_applies_unix_mode_before_secret_bytes_are_written() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new("fs-atomic-mode");
        let path = temp.path().join(".env");

        let error = write_atomic_with(
            &path,
            AtomicWriteOptions {
                unix_mode: Some(0o600),
                reject_symlink: false,
                replace_existing: true,
            },
            |file| {
                let mode = file.metadata()?.permissions().mode() & 0o777;
                assert_eq!(mode, 0o600);
                file.write_all(b"SECRET=value")?;
                Err::<(), io::Error>(io::Error::other("injected failure"))
            },
        )
        .expect_err("write fails");

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(!path.exists());
        assert_no_temp_siblings(temp.path());
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_rejects_symlink_destination() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new("fs-atomic-symlink");
        let target = temp.path().join("target");
        let link = temp.path().join("link");
        fs::write(&target, b"target").expect("target");
        symlink(&target, &link).expect("link");

        let error = write_atomic(
            &link,
            b"replacement",
            AtomicWriteOptions {
                unix_mode: None,
                reject_symlink: true,
                replace_existing: true,
            },
        )
        .expect_err("symlink rejected");

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(fs::read(&target).expect("target unchanged"), b"target");
    }

    fn assert_no_temp_siblings(root: &Path) {
        let leftovers = fs::read_dir(root)
            .expect("read dir")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("bowline-tmp"))
            .collect::<Vec<_>>();
        assert!(leftovers.is_empty(), "leftover temp files: {leftovers:?}");
    }

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("temp dir");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}
