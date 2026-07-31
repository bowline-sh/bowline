use std::fs::{self, File, Metadata};
use std::io::Read;
use std::path::{Path, PathBuf};

use super::WorkViewError;

const SNAPSHOT_READ_BUFFER_BYTES: usize = 64 * 1024;

pub fn materialization_snapshot(root: &Path) -> Result<String, WorkViewError> {
    let mut hasher = blake3::Hasher::new();
    snapshot_path(root, root, &mut hasher)?;
    Ok(hasher.finalize().to_hex().to_string())
}

fn snapshot_path(
    root: &Path,
    path: &Path,
    hasher: &mut blake3::Hasher,
) -> Result<(), WorkViewError> {
    let relative = path.strip_prefix(root).unwrap_or(path);
    hash_os_str(hasher, relative.as_os_str());
    hasher.update(&[0]);
    let metadata = fs::symlink_metadata(path)?;
    hash_metadata(hasher, &metadata)?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        let target = fs::read_link(path)?;
        hash_os_str(hasher, target.as_os_str());
        return Ok(());
    }
    if file_type.is_file() {
        return snapshot_file(path, &metadata, hasher);
    }
    if !file_type.is_dir() {
        return Ok(());
    }
    let entries = fs::read_dir(path)?;
    let mut children = entries
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<PathBuf>, _>>()?;
    children.sort();
    for child in children {
        snapshot_path(root, &child, hasher)?;
    }
    let after = fs::symlink_metadata(path)?;
    if !same_file_observation(&metadata, &after)? {
        return Err(WorkViewError::ContentChangedDuringCapture {
            path: path.display().to_string(),
        });
    }
    Ok(())
}

fn snapshot_file(
    path: &Path,
    before: &Metadata,
    hasher: &mut blake3::Hasher,
) -> Result<(), WorkViewError> {
    let mut file = File::open(path)?;
    let mut buffer = [0_u8; SNAPSHOT_READ_BUFFER_BYTES];
    loop {
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                hasher.update(&buffer[..read]);
            }
            Err(error) => return Err(error.into()),
        }
    }
    let after = file.metadata()?;
    if !same_file_observation(before, &after)? {
        return Err(WorkViewError::ContentChangedDuringCapture {
            path: path.display().to_string(),
        });
    }
    Ok(())
}

fn hash_metadata(hasher: &mut blake3::Hasher, metadata: &Metadata) -> Result<(), WorkViewError> {
    hasher.update(&metadata.len().to_le_bytes());
    let kind = if metadata.is_dir() {
        b'd'
    } else if metadata.is_file() {
        b'f'
    } else if metadata.file_type().is_symlink() {
        b'l'
    } else {
        b'o'
    };
    hasher.update(&[kind]);
    match metadata.modified()?.duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => hasher.update(&duration.as_nanos().to_le_bytes()),
        Err(_) => hasher.update(b"pre-epoch"),
    };
    Ok(())
}

fn same_file_observation(before: &Metadata, after: &Metadata) -> Result<bool, WorkViewError> {
    Ok(before.len() == after.len() && before.modified()? == after.modified()?)
}

#[cfg(unix)]
fn hash_os_str(hasher: &mut blake3::Hasher, value: &std::ffi::OsStr) {
    use std::os::unix::ffi::OsStrExt;
    hasher.update(value.as_bytes());
}

#[cfg(windows)]
fn hash_os_str(hasher: &mut blake3::Hasher, value: &std::ffi::OsStr) {
    use std::os::windows::ffi::OsStrExt;
    for unit in value.encode_wide() {
        hasher.update(&unit.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::TempWorkspace;

    #[test]
    fn snapshot_changes_with_content_and_special_entry_inventory() {
        let workspace = TempWorkspace::new("work-materialization-snapshot").expect("workspace");
        fs::write(workspace.root().join("file"), b"one").expect("file");
        let first = materialization_snapshot(workspace.root()).expect("first");
        fs::write(workspace.root().join("file"), b"two").expect("rewrite");
        let second = materialization_snapshot(workspace.root()).expect("second");
        assert_ne!(first, second);
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_distinguishes_byte_distinct_non_utf8_names() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let first_name = OsString::from_vec(vec![b'a', 0x80]);
        let second_name = OsString::from_vec(vec![b'a', 0x81]);
        let mut first = blake3::Hasher::new();
        hash_os_str(&mut first, &first_name);
        let mut second = blake3::Hasher::new();
        hash_os_str(&mut second, &second_name);
        assert_ne!(first.finalize(), second.finalize());
    }
}
