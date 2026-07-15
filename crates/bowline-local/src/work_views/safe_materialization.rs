use std::{
    fs, io,
    path::{Component, Path, PathBuf},
};

use super::WorkViewError;

/// Filesystem mutations confined to a private materialization root.
///
/// Bowline owns this root while it is being built. Every path is relative and
/// every ancestor is checked with `symlink_metadata` immediately before a
/// mutation. Explicit creation may replace an ancestor that is not a directory;
/// observation-only operations reject it instead.
pub(crate) struct SafeMaterializationRoot<'a> {
    root: &'a Path,
}

impl<'a> SafeMaterializationRoot<'a> {
    pub(crate) fn new(root: &'a Path) -> io::Result<Self> {
        match fs::symlink_metadata(root) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(unsafe_component(
                    root,
                    "staging root is not a physical directory",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir_all(root)?,
            Err(error) => return Err(error),
        }
        Ok(Self { root })
    }

    pub(crate) fn path(&self, relative: &Path) -> io::Result<PathBuf> {
        validate_relative(relative)?;
        Ok(self.root.join(relative))
    }

    pub(crate) fn reject_main_symlink_ancestors(
        &self,
        relative: &Path,
    ) -> Result<(), WorkViewError> {
        self.ensure_ancestors(relative, false)
            .map_err(|_| WorkViewError::UnsafeWorkViewPath {
                path: relative.display().to_string(),
                reason: "canonical main path has a symlink ancestor",
            })
    }

    pub(crate) fn create_dir(&self, relative: &Path) -> io::Result<()> {
        self.ensure_ancestors(relative, true)?;
        let destination = self.path(relative)?;
        match fs::symlink_metadata(&destination) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(()),
            Ok(_) => {
                remove_leaf(&destination)?;
                fs::create_dir(&destination)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir(&destination),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn create_file(&self, relative: &Path) -> io::Result<fs::File> {
        let destination = self.prepare_file(relative)?;
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(destination)
    }

    pub(crate) fn prepare_file(&self, relative: &Path) -> io::Result<PathBuf> {
        self.ensure_ancestors(relative, true)?;
        let destination = self.path(relative)?;
        remove_leaf(&destination)?;
        Ok(destination)
    }

    #[cfg(test)]
    pub(crate) fn write(&self, relative: &Path, bytes: &[u8]) -> io::Result<()> {
        use std::io::Write as _;
        let mut file = self.create_file(relative)?;
        file.write_all(bytes)?;
        file.sync_all()
    }

    pub(crate) fn remove(&self, relative: &Path, empty_directory_only: bool) -> io::Result<()> {
        self.ensure_ancestors(relative, false)?;
        let destination = self.path(relative)?;
        let metadata = match fs::symlink_metadata(&destination) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            if empty_directory_only {
                match fs::remove_dir(destination) {
                    Ok(()) => Ok(()),
                    Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => Ok(()),
                    Err(error) => Err(error),
                }
            } else {
                fs::remove_dir_all(destination)
            }
        } else {
            fs::remove_file(destination)
        }
    }

    pub(crate) fn set_permissions(
        &self,
        relative: &Path,
        permissions: fs::Permissions,
    ) -> io::Result<()> {
        self.ensure_ancestors(relative, false)?;
        let destination = self.path(relative)?;
        let metadata = fs::symlink_metadata(&destination)?;
        if metadata.file_type().is_symlink() {
            return Err(unsafe_component(relative, "destination leaf is a symlink"));
        }
        fs::set_permissions(destination, permissions)
    }

    #[cfg(unix)]
    pub(crate) fn create_symlink(&self, relative: &Path, target: &Path) -> io::Result<()> {
        self.ensure_ancestors(relative, true)?;
        let destination = self.path(relative)?;
        remove_leaf(&destination)?;
        std::os::unix::fs::symlink(target, destination)
    }

    fn ensure_ancestors(&self, relative: &Path, replace_conflicts: bool) -> io::Result<()> {
        validate_relative(relative)?;
        let Some(parent) = relative.parent() else {
            return Ok(());
        };
        let mut current = self.root.to_path_buf();
        for component in parent.components() {
            current.push(component.as_os_str());
            match fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
                Ok(_) if replace_conflicts => {
                    remove_leaf(&current)?;
                    fs::create_dir(&current)?;
                }
                Ok(_) => {
                    return Err(unsafe_component(
                        relative,
                        "ancestor is not a physical directory",
                    ));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound && replace_conflicts => {
                    fs::create_dir(&current)?;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}

fn validate_relative(relative: &Path) -> io::Result<()> {
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(unsafe_component(
            relative,
            "path is not a normalized relative path",
        ));
    }
    Ok(())
}

fn remove_leaf(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            fs::remove_dir_all(path)
        }
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn unsafe_component(path: &Path, reason: &'static str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "unsafe staged materialization path `{}`: {reason}",
            path.display()
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::TempWorkspace;

    #[cfg(unix)]
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    #[cfg(unix)]
    #[test]
    fn explicit_descendant_replaces_symlink_ancestor_inside_staging() {
        let temp = TempWorkspace::new("safe-stage-replace-ancestor").expect("temp");
        let staged = temp.root().join("staged");
        let outside = temp.root().join("outside");
        fs::create_dir_all(&staged).expect("staged");
        fs::create_dir_all(&outside).expect("outside");
        fs::write(outside.join("sentinel"), "outside").expect("sentinel");
        symlink(&outside, staged.join("link")).expect("link");

        SafeMaterializationRoot::new(&staged)
            .expect("safe root")
            .write(Path::new("link/selected.txt"), b"inside")
            .expect("selected descendant replaces staged link");

        assert_eq!(fs::read(outside.join("sentinel")).unwrap(), b"outside");
        assert!(!outside.join("selected.txt").exists());
        assert_eq!(
            fs::read(staged.join("link/selected.txt")).unwrap(),
            b"inside"
        );
        assert!(fs::symlink_metadata(staged.join("link")).unwrap().is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn file_creation_replaces_symlink_leaf_without_writing_target() {
        let temp = TempWorkspace::new("safe-stage-replace-leaf").expect("temp");
        let staged = temp.root().join("staged");
        let outside = temp.root().join("outside.txt");
        fs::create_dir_all(&staged).expect("staged");
        fs::write(&outside, "outside").expect("outside");
        symlink(&outside, staged.join("selected.txt")).expect("link");

        SafeMaterializationRoot::new(&staged)
            .expect("safe root")
            .write(Path::new("selected.txt"), b"inside")
            .expect("replace leaf");

        assert_eq!(fs::read(&outside).unwrap(), b"outside");
        assert_eq!(fs::read(staged.join("selected.txt")).unwrap(), b"inside");
        assert!(
            !fs::symlink_metadata(staged.join("selected.txt"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(unix)]
    #[test]
    fn delete_and_chmod_reject_symlink_ancestors() {
        let temp = TempWorkspace::new("safe-stage-observation-ancestor").expect("temp");
        let staged = temp.root().join("staged");
        let outside = temp.root().join("outside");
        fs::create_dir_all(&staged).expect("staged");
        fs::create_dir_all(&outside).expect("outside");
        let sentinel = outside.join("sentinel");
        fs::write(&sentinel, "outside").expect("sentinel");
        fs::set_permissions(&sentinel, fs::Permissions::from_mode(0o640)).expect("mode");
        symlink(&outside, staged.join("link")).expect("link");
        let root = SafeMaterializationRoot::new(&staged).expect("safe root");

        assert_eq!(
            root.remove(Path::new("link/sentinel"), false)
                .expect_err("delete rejects ancestor")
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            root.set_permissions(
                Path::new("link/sentinel"),
                fs::Permissions::from_mode(0o777),
            )
            .expect_err("chmod rejects ancestor")
            .kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(fs::read(&sentinel).unwrap(), b"outside");
        assert_eq!(
            fs::metadata(&sentinel).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }

    #[cfg(unix)]
    #[test]
    fn chmod_rejects_symlink_leaf_and_delete_only_unlinks_leaf() {
        let temp = TempWorkspace::new("safe-stage-observation-leaf").expect("temp");
        let staged = temp.root().join("staged");
        let outside = temp.root().join("outside.txt");
        fs::create_dir_all(&staged).expect("staged");
        fs::write(&outside, "outside").expect("outside");
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o640)).expect("mode");
        symlink(&outside, staged.join("link")).expect("link");
        let root = SafeMaterializationRoot::new(&staged).expect("safe root");

        assert_eq!(
            root.set_permissions(Path::new("link"), fs::Permissions::from_mode(0o777))
                .expect_err("chmod rejects leaf")
                .kind(),
            io::ErrorKind::InvalidData
        );
        root.remove(Path::new("link"), false).expect("unlink leaf");

        assert_eq!(fs::read(&outside).unwrap(), b"outside");
        assert_eq!(
            fs::metadata(&outside).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert!(!staged.join("link").exists());
    }

    #[test]
    fn rejects_paths_outside_the_staging_root() {
        let temp = TempWorkspace::new("safe-stage-relative-paths").expect("temp");
        let staged = temp.root().join("staged");
        fs::create_dir_all(&staged).expect("staged");
        let root = SafeMaterializationRoot::new(&staged).expect("safe root");

        assert!(root.create_file(Path::new("../escape")).is_err());
        assert!(root.create_file(Path::new("/absolute")).is_err());
    }
}
