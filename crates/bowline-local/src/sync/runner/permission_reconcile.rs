use std::{
    io,
    path::{Component, Path},
};

use super::{SyncRunnerError, permissions::MaterializedFilePermissions};

pub(super) fn apply_materialized_permissions(
    root: &Path,
    relative_path: &Path,
    permissions: MaterializedFilePermissions,
) -> Result<(), SyncRunnerError> {
    #[cfg(unix)]
    {
        match open_workspace_parent_dir_no_follow(root, relative_path)? {
            Some((parent, name)) => {
                let Some(stat) = statat_no_follow(&parent, &name)? else {
                    return Ok(());
                };
                if rustix::fs::FileType::from_raw_mode(stat.st_mode)
                    != rustix::fs::FileType::RegularFile
                    || rustix::fs::Mode::from_raw_mode(stat.st_mode & 0o7777)
                        == rustix::fs::Mode::from_raw_mode(permissions.unix_mode() as _)
                {
                    return Ok(());
                }
                chmodat_no_follow_or_fallback(&parent, &name, permissions)
            }
            None => Ok(()),
        }
    }

    #[cfg(not(unix))]
    {
        let _ = (root, relative_path, permissions);
        Ok(())
    }
}

#[cfg(unix)]
fn open_workspace_parent_dir_no_follow(
    root: &Path,
    relative_path: &Path,
) -> Result<Option<(rustix::fd::OwnedFd, std::ffi::CString)>, SyncRunnerError> {
    let mut current = open_root_dir(root).map_err(SyncRunnerError::StateIo)?;
    let mut components = relative_path.components().peekable();
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            return Err(SyncRunnerError::UnsafeMaterializationPath(
                relative_path.display().to_string(),
            ));
        };
        let name = cstring_path_component(name)?;
        let is_final = components.peek().is_none();
        if is_final {
            return Ok(Some((current, name)));
        }
        let Some(next) = openat_no_follow_dir(&current, &name)? else {
            return Ok(None);
        };
        current = next;
    }
    Err(SyncRunnerError::UnsafeMaterializationPath(
        relative_path.display().to_string(),
    ))
}

#[cfg(unix)]
fn open_root_dir(root: &Path) -> io::Result<rustix::fd::OwnedFd> {
    use std::os::unix::ffi::OsStrExt;

    let root = std::ffi::CString::new(root.as_os_str().as_bytes())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    rustix::fs::open(
        root.as_c_str(),
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::DIRECTORY,
        rustix::fs::Mode::empty(),
    )
    .map_err(rustix_to_io)
}

#[cfg(unix)]
fn cstring_path_component(name: &std::ffi::OsStr) -> Result<std::ffi::CString, SyncRunnerError> {
    use std::os::unix::ffi::OsStrExt;

    std::ffi::CString::new(name.as_bytes()).map_err(|error| {
        SyncRunnerError::StateIo(io::Error::new(io::ErrorKind::InvalidInput, error))
    })
}

#[cfg(unix)]
fn openat_no_follow_dir(
    dir_fd: &rustix::fd::OwnedFd,
    name: &std::ffi::CString,
) -> Result<Option<rustix::fd::OwnedFd>, SyncRunnerError> {
    match rustix::fs::openat(
        dir_fd,
        name.as_c_str(),
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::DIRECTORY,
        rustix::fs::Mode::empty(),
    ) {
        Ok(fd) => Ok(Some(fd)),
        Err(
            rustix::io::Errno::NOENT
            | rustix::io::Errno::NOTDIR
            | rustix::io::Errno::LOOP
            | rustix::io::Errno::ACCESS
            | rustix::io::Errno::PERM,
        ) => Ok(None),
        Err(error) => Err(SyncRunnerError::StateIo(rustix_to_io(error))),
    }
}

#[cfg(unix)]
fn rustix_to_io(error: rustix::io::Errno) -> io::Error {
    io::Error::from(error)
}

#[cfg(unix)]
fn statat_no_follow(
    parent: &rustix::fd::OwnedFd,
    name: &std::ffi::CString,
) -> Result<Option<rustix::fs::Stat>, SyncRunnerError> {
    match rustix::fs::statat(
        parent,
        name.as_c_str(),
        rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
    ) {
        Ok(stat) => Ok(Some(stat)),
        Err(rustix::io::Errno::NOENT | rustix::io::Errno::NOTDIR | rustix::io::Errno::LOOP) => {
            Ok(None)
        }
        Err(error) => Err(SyncRunnerError::StateIo(rustix_to_io(error))),
    }
}

#[cfg(unix)]
fn chmodat_no_follow_or_fallback(
    parent: &rustix::fd::OwnedFd,
    name: &std::ffi::CString,
    permissions: MaterializedFilePermissions,
) -> Result<(), SyncRunnerError> {
    let desired = rustix::fs::Mode::from_raw_mode(permissions.unix_mode() as _);
    match rustix::fs::chmodat(
        parent,
        name.as_c_str(),
        desired,
        rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
    ) {
        Ok(()) => Ok(()),
        Err(rustix::io::Errno::NOENT | rustix::io::Errno::NOTDIR | rustix::io::Errno::LOOP) => {
            Ok(())
        }
        Err(rustix::io::Errno::NOSYS | rustix::io::Errno::OPNOTSUPP) => {
            fchmod_opened_no_follow(parent, name, desired)
        }
        Err(error) => Err(SyncRunnerError::StateIo(rustix_to_io(error))),
    }
}

#[cfg(unix)]
fn fchmod_opened_no_follow(
    parent: &rustix::fd::OwnedFd,
    name: &std::ffi::CString,
    mode: rustix::fs::Mode,
) -> Result<(), SyncRunnerError> {
    match rustix::fs::openat(
        parent,
        name.as_c_str(),
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    ) {
        Ok(fd) => rustix::fs::fchmod(&fd, mode)
            .map_err(rustix_to_io)
            .map_err(SyncRunnerError::StateIo),
        Err(
            rustix::io::Errno::NOENT
            | rustix::io::Errno::NOTDIR
            | rustix::io::Errno::LOOP
            | rustix::io::Errno::ACCESS
            | rustix::io::Errno::PERM,
        ) => Ok(()),
        Err(error) => Err(SyncRunnerError::StateIo(rustix_to_io(error))),
    }
}
