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

    let parent = final_path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "atomic write destination has no parent directory",
        )
    })?;
    fs::create_dir_all(parent)?;

    let temp_path = temp_sibling(final_path)?;
    remove_file_if_present(&temp_path)?;
    let result = (|| {
        let mut file = create_temp_file(&temp_path, options)?;
        let value = write(&mut file)?;
        file.sync_all()?;
        drop(file);
        commit_temp_file(&temp_path, final_path, options)?;
        sync_parent_dir(parent)?;
        Ok(value)
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

fn commit_temp_file(
    temp_path: &Path,
    final_path: &Path,
    options: AtomicWriteOptions,
) -> io::Result<()> {
    if options.replace_existing {
        return fs::rename(temp_path, final_path);
    }
    fs::hard_link(temp_path, final_path)?;
    fs::remove_file(temp_path)
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

    use super::{AtomicWriteOptions, write_atomic, write_atomic_with};

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
