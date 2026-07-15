use std::{
    fs, io,
    path::{Path, PathBuf},
};

pub(super) fn create_pending_destination(
    destination: &Path,
    owner_only: bool,
) -> io::Result<(PathBuf, fs::File)> {
    for _ in 0..8 {
        let mut nonce = [0_u8; 8];
        getrandom::fill(&mut nonce).map_err(|error| {
            io::Error::other(format!("pending path randomness failed: {error}"))
        })?;
        let pending = pending_materialization_path(destination, &nonce);
        match open_pending_destination(&pending, owner_only) {
            Ok(file) => {
                let metadata = match file.metadata() {
                    Ok(metadata) => metadata,
                    Err(error) => {
                        drop(file);
                        cleanup_pending(&pending);
                        return Err(error);
                    }
                };
                if !metadata.file_type().is_file() {
                    drop(file);
                    cleanup_pending(&pending);
                    return Err(io::Error::other(
                        "pending materialization target is not a regular file",
                    ));
                }
                return Ok((pending, file));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not reserve a pending materialization path",
    ))
}

pub(super) fn cleanup_pending(path: &Path) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => eprintln!("bowline pending materialization cleanup failed: {error}"),
    }
}

fn pending_materialization_path(destination: &Path, nonce: &[u8; 8]) -> PathBuf {
    let name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("content");
    destination.with_file_name(format!(
        ".{name}.bowline-pending-{:016x}",
        u64::from_le_bytes(*nonce)
    ))
}

#[cfg(unix)]
fn open_pending_destination(path: &Path, owner_only: bool) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(if owner_only { 0o600 } else { 0o644 })
        .open(path)
}

#[cfg(not(unix))]
fn open_pending_destination(path: &Path, _owner_only: bool) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}
