use std::{
    fs,
    io::{self, Read, Write as _},
    path::Path,
};

use bowline_core::{ids::ContentId, workspace_graph::FileExecutability};
use bowline_storage::{CacheError, LocalContentCache};

use crate::work_views::safe_materialization::SafeMaterializationRoot;

use crate::work_views::pending_materialization::{cleanup_pending, create_pending_destination};

pub(super) fn materialize_verified_content(
    cache: &LocalContentCache,
    content_id: &ContentId,
    destination: &Path,
    owner_only: bool,
) -> Result<String, CacheError> {
    let mut source = cache.open_previously_verified_content(content_id)?;
    publish_verified_reader(&mut source, destination, owner_only).map_err(Into::into)
}

pub(crate) fn materialize_workspace_keyed_content(
    source: &mut dyn Read,
    workspace_content_key: [u8; 32],
    expected_content_id: &ContentId,
    destination: &Path,
    owner_only: bool,
) -> Result<(), CacheError> {
    let (pending, mut target) = create_pending_destination(destination, owner_only)?;
    let result = (|| -> io::Result<()> {
        let mut hasher = blake3::Hasher::new_keyed(&workspace_content_key);
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = source.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            target.write_all(&buffer[..read])?;
            hasher.update(&buffer[..read]);
        }
        let actual = ContentId::new(format!("cid_{}", hasher.finalize().to_hex()));
        if &actual != expected_content_id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "live content does not match canonical identity",
            ));
        }
        target.sync_all()?;
        drop(target);
        fs::rename(&pending, destination)?;
        sync_materialization_parent(destination)
    })();
    if result.is_err() {
        cleanup_pending(&pending);
    }
    result.map_err(Into::into)
}

fn publish_verified_reader(
    source: &mut dyn Read,
    destination: &Path,
    owner_only: bool,
) -> io::Result<String> {
    let (pending, mut target) = create_pending_destination(destination, owner_only)?;
    let result = (|| -> io::Result<String> {
        let mut hasher = blake3::Hasher::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = source.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            target.write_all(&buffer[..read])?;
            hasher.update(&buffer[..read]);
        }
        target.sync_all()?;
        drop(target);
        fs::rename(&pending, destination)?;
        sync_materialization_parent(destination)?;
        Ok(format!("b3_{}", hasher.finalize().to_hex()))
    })();
    if result.is_err() {
        cleanup_pending(&pending);
    }
    result
}

#[cfg(unix)]
pub(super) fn apply_file_permissions(
    staging: &SafeMaterializationRoot<'_>,
    relative: &Path,
    executability: FileExecutability,
    owner_only: bool,
) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let mode = if owner_only {
        0o600
    } else if matches!(executability, FileExecutability::Executable) {
        0o755
    } else {
        0o644
    };
    staging.set_permissions(relative, fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
pub(super) fn apply_file_permissions(
    _staging: &SafeMaterializationRoot<'_>,
    _relative: &Path,
    _executability: FileExecutability,
    _owner_only: bool,
) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn sync_materialization_parent(path: &Path) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "materialized path has no parent",
        )
    })?;
    fs::File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_materialization_parent(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use bowline_core::ids::ContentId;

    use super::*;
    use crate::workspace::TempWorkspace;

    #[test]
    fn unverified_content_never_publishes_destination_or_pending_file() {
        let temp = TempWorkspace::new("unverified-work-view-content").expect("temp");
        let cache = LocalContentCache::open(temp.root().join("cache")).expect("cache");
        let content_id = ContentId::new("cid_unverified");
        cache
            .put_content(&content_id, b"not authenticated")
            .expect("cache bytes");
        let destination = temp.root().join("visible/secret.env");
        fs::create_dir_all(destination.parent().expect("parent")).expect("destination parent");

        let result = materialize_verified_content(&cache, &content_id, &destination, true);

        assert!(matches!(
            result,
            Err(CacheError::MissingCachedBytes("verified content marker"))
        ));
        assert!(!destination.exists());
        assert!(
            fs::read_dir(destination.parent().expect("parent"))
                .expect("pending directory")
                .all(|entry| !entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .contains(".bowline-pending-"))
        );
    }

    #[test]
    fn streaming_failure_removes_pending_file_without_publishing_destination() {
        let temp = TempWorkspace::new("failed-work-view-content-stream").expect("temp");
        let destination = temp.root().join("visible/large.bin");
        fs::create_dir_all(destination.parent().expect("parent")).expect("destination parent");
        let mut source = FailingReader { emitted: false };

        let result = publish_verified_reader(&mut source, &destination, false);

        assert_eq!(
            result.expect_err("stream fails").kind(),
            io::ErrorKind::Other
        );
        assert!(!destination.exists());
        assert_no_pending_files(destination.parent().expect("parent"));
    }

    struct FailingReader {
        emitted: bool,
    }

    impl Read for FailingReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if self.emitted {
                return Err(io::Error::other("injected stream failure"));
            }
            self.emitted = true;
            let bytes = b"partial";
            buffer[..bytes.len()].copy_from_slice(bytes);
            Ok(bytes.len())
        }
    }

    fn assert_no_pending_files(parent: &Path) {
        assert!(
            fs::read_dir(parent)
                .expect("pending directory")
                .all(|entry| !entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .contains(".bowline-pending-"))
        );
    }
}
