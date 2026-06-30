use std::{
    fs, io,
    path::{Component, Path},
    process::{Command, Stdio},
};

use super::WorkViewError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaterializedFileMethod {
    Clone,
    Copy,
}

pub fn materialize_base_files(root: &Path, visible_path: &Path) -> Result<(), WorkViewError> {
    materialize_base_files_inner(root, root, visible_path).map(|_| ())
}

#[cfg(test)]
pub fn materialize_base_files_with_methods(
    root: &Path,
    visible_path: &Path,
) -> Result<Vec<(String, MaterializedFileMethod)>, WorkViewError> {
    let mut methods = Vec::new();
    materialize_base_files_inner_with_methods(root, root, visible_path, &mut methods)?;
    Ok(methods)
}

fn materialize_base_files_inner(
    root: &Path,
    path: &Path,
    visible_path: &Path,
) -> Result<(), WorkViewError> {
    let mut ignored = Vec::new();
    materialize_base_files_inner_with_methods(root, path, visible_path, &mut ignored)
}

fn materialize_base_files_inner_with_methods(
    root: &Path,
    path: &Path,
    visible_path: &Path,
    methods: &mut Vec<(String, MaterializedFileMethod)>,
) -> Result<(), WorkViewError> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if is_bowline_owned_namespace(relative) || is_secret_bearing_work_path(relative) {
            continue;
        }
        if is_source_control_metadata_path(relative) {
            continue;
        }
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        let destination = visible_path.join(relative);
        if metadata.is_dir() {
            fs::create_dir_all(&destination)?;
            materialize_base_files_inner_with_methods(root, &path, visible_path, methods)?;
        } else if metadata.is_file() {
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            let method = clone_or_copy_file(&path, &destination)?;
            methods.push((relative.display().to_string(), method));
        }
    }
    Ok(())
}

fn clone_or_copy_file(
    source: &Path,
    destination: &Path,
) -> Result<MaterializedFileMethod, io::Error> {
    if clone_file(source, destination) {
        return Ok(MaterializedFileMethod::Clone);
    }
    fs::copy(source, destination)?;
    Ok(MaterializedFileMethod::Copy)
}

fn clone_file(source: &Path, destination: &Path) -> bool {
    #[cfg(target_os = "macos")]
    {
        let Ok(status) = Command::new("/bin/cp")
            .arg("-c")
            .arg(source)
            .arg(destination)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
        else {
            return false;
        };
        status.success()
    }

    #[cfg(target_os = "linux")]
    {
        let Ok(status) = Command::new("/bin/cp")
            .arg("--reflink=always")
            .arg(source)
            .arg(destination)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
        else {
            return false;
        };
        status.success()
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (source, destination);
        false
    }
}

fn is_bowline_owned_namespace(relative: &Path) -> bool {
    matches!(
        relative.components().next(),
        Some(Component::Normal(name)) if name.to_str() == Some(".work")
    )
}

fn is_secret_bearing_work_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(".env"))
}

fn is_source_control_metadata_path(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(
            component,
            Component::Normal(name)
                if matches!(name.to_str(), Some(".git" | ".jj" | ".hg" | ".svn"))
        )
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn materialization_skips_bowline_secret_and_source_control_namespaces() {
        let temp = tempfile_dir("bowline-work-view-materialize");
        let root = temp.join("project");
        let visible = temp.join("work");
        fs::create_dir_all(root.join("src")).expect("src");
        fs::create_dir_all(root.join(".work/other")).expect("work");
        fs::create_dir_all(root.join(".git")).expect("git");
        fs::write(root.join("src/index.ts"), "base").expect("source");
        fs::write(root.join(".env.local"), "SECRET=value").expect("env");
        fs::write(root.join(".git/config"), "[core]").expect("git config");
        fs::write(root.join(".work/other/file"), "internal").expect("internal");

        let methods = materialize_base_files_with_methods(&root, &visible).expect("materialize");

        assert_eq!(methods.len(), 1);
        assert!(visible.join("src/index.ts").exists());
        assert!(!visible.join(".env.local").exists());
        assert!(!visible.join(".git/config").exists());
        assert!(!visible.join(".work/other/file").exists());
    }

    fn tempfile_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "{name}-{}-{}",
            std::process::id(),
            blake3::hash(name.as_bytes()).to_hex()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("temp dir");
        path
    }
}
