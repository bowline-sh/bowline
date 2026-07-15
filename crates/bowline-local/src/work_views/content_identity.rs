use std::{
    fs,
    io::{Read, Seek as _, SeekFrom},
    path::Path,
};

use bowline_core::ids::ContentId;
use bowline_storage::LocalContentCache;

use super::{WorkViewError, paths::cancellation_checkpoint};

const COMPARE_BUFFER_BYTES: usize = 64 * 1024;

pub(super) fn verified_content_matches_path(
    cache: &LocalContentCache,
    content_id: &ContentId,
    path: &Path,
) -> Result<bool, WorkViewError> {
    verified_content_matches_path_with_checkpoint(cache, content_id, path, &mut || true)
}

pub(super) fn verified_content_matches_path_with_checkpoint(
    cache: &LocalContentCache,
    content_id: &ContentId,
    path: &Path,
    checkpoint: &mut dyn FnMut() -> bool,
) -> Result<bool, WorkViewError> {
    verified_content_matches_path_inner(cache, content_id, path, || {}, checkpoint)
}

pub(super) fn workspace_content_matches_path(
    workspace_content_key: [u8; 32],
    content_id: &ContentId,
    path: &Path,
) -> Result<bool, WorkViewError> {
    let (identity, mut file) = open_stable_regular_file(path, None)?;
    let actual = bowline_core::workspace_graph::workspace_content_id_reader(
        workspace_content_key,
        &mut file,
    )?;
    verify_stable_regular_file(path, &file, identity)?;
    Ok(&actual == content_id)
}

pub(super) fn materialize_verified_live_content(
    workspace_content_key: [u8; 32],
    expected_content_id: &ContentId,
    source: &Path,
    expected_source_identity: FileIdentity,
    destination: &Path,
    owner_only: bool,
) -> Result<(), WorkViewError> {
    let (identity, mut file) = open_stable_regular_file(source, Some(expected_source_identity))?;
    super::materialize::materialize_workspace_keyed_content(
        &mut file,
        workspace_content_key,
        expected_content_id,
        destination,
        owner_only,
    )?;
    verify_stable_regular_file(source, &file, identity)
}

#[cfg(test)]
fn verified_content_matches_path_with(
    cache: &LocalContentCache,
    content_id: &ContentId,
    path: &Path,
    after_open: impl FnOnce(),
) -> Result<bool, WorkViewError> {
    verified_content_matches_path_inner(cache, content_id, path, after_open, &mut || true)
}

fn verified_content_matches_path_inner(
    cache: &LocalContentCache,
    content_id: &ContentId,
    path: &Path,
    after_open: impl FnOnce(),
    checkpoint: &mut dyn FnMut() -> bool,
) -> Result<bool, WorkViewError> {
    let (before, mut actual) = open_stable_regular_file(path, None)?;
    after_open();
    let mut expected = cache
        .open_previously_verified_content(content_id)
        .map_err(|source| WorkViewError::ExposedBaseContentUnavailable {
            path: path.display().to_string(),
            content_id: content_id.clone(),
            source,
        })?;
    let matches = readers_match_with_checkpoint(&mut actual, &mut expected, checkpoint)?;
    verify_stable_regular_file(path, &actual, before)?;
    Ok(matches)
}

#[cfg(test)]
pub(super) fn capture_stable_bytes(
    path: &Path,
    expected: FileIdentity,
) -> Result<Vec<u8>, WorkViewError> {
    capture_stable_bytes_with(path, expected, || {})
}

#[cfg(test)]
fn capture_stable_bytes_with(
    path: &Path,
    expected: FileIdentity,
    after_open: impl FnOnce(),
) -> Result<Vec<u8>, WorkViewError> {
    let (identity, mut file) = open_stable_regular_file(path, Some(expected))?;
    after_open();
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    verify_stable_regular_file(path, &file, identity)?;
    Ok(bytes)
}

pub(super) fn open_stable_regular_file(
    path: &Path,
    expected: Option<FileIdentity>,
) -> Result<(FileIdentity, fs::File), WorkViewError> {
    let file = open_no_follow(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(WorkViewError::ContentChangedDuringCapture {
            path: path.display().to_string(),
        });
    }
    let identity = FileIdentity::from_metadata(&metadata)?;
    if expected.is_some_and(|expected| expected != identity) {
        return Err(WorkViewError::ContentChangedDuringCapture {
            path: path.display().to_string(),
        });
    }
    verify_stable_regular_file(path, &file, identity)?;
    Ok((identity, file))
}

pub(super) fn verify_stable_regular_file(
    path: &Path,
    file: &fs::File,
    expected: FileIdentity,
) -> Result<(), WorkViewError> {
    let after = FileIdentity::from_open_file(file)?;
    let current_metadata = fs::symlink_metadata(path)?;
    if current_metadata.file_type().is_symlink()
        || !current_metadata.is_file()
        || after != expected
        || FileIdentity::from_metadata(&current_metadata)? != expected
    {
        return Err(WorkViewError::ContentChangedDuringCapture {
            path: path.display().to_string(),
        });
    }
    Ok(())
}

pub(super) fn clone_file_at_start(file: &fs::File) -> std::io::Result<fs::File> {
    let mut clone = file.try_clone()?;
    clone.seek(SeekFrom::Start(0))?;
    Ok(clone)
}

#[cfg(unix)]
fn open_no_follow(path: &Path) -> std::io::Result<fs::File> {
    let descriptor = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    )
    .map_err(std::io::Error::from)?;
    Ok(fs::File::from(descriptor))
}

#[cfg(not(unix))]
fn open_no_follow(path: &Path) -> std::io::Result<fs::File> {
    if fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "captured file is a symlink",
        ));
    }
    fs::File::open(path)
}

#[cfg(test)]
fn readers_match(left: &mut impl Read, right: &mut impl Read) -> std::io::Result<bool> {
    readers_match_with_checkpoint(left, right, &mut || true).map_err(|error| match error {
        WorkViewError::Io(error) => error,
        other => std::io::Error::other(other),
    })
}

fn readers_match_with_checkpoint(
    left: &mut impl Read,
    right: &mut impl Read,
    checkpoint: &mut dyn FnMut() -> bool,
) -> Result<bool, WorkViewError> {
    let mut left_buffer = [0_u8; COMPARE_BUFFER_BYTES];
    let mut right_buffer = [0_u8; COMPARE_BUFFER_BYTES];
    loop {
        cancellation_checkpoint(checkpoint)?;
        let left_read = fill_buffer(left, &mut left_buffer)?;
        let right_read = fill_buffer(right, &mut right_buffer)?;
        if left_read != right_read || left_buffer[..left_read] != right_buffer[..right_read] {
            return Ok(false);
        }
        if left_read == 0 {
            return Ok(true);
        }
    }
}

fn fill_buffer(reader: &mut impl Read, buffer: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        let read = reader.read(&mut buffer[filled..])?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    Ok(filled)
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FileIdentity {
    device: u64,
    inode: u64,
    byte_len: u64,
    modified_seconds: i64,
    modified_nanos: i64,
}

#[cfg(unix)]
impl FileIdentity {
    fn from_open_file(file: &fs::File) -> std::io::Result<Self> {
        Self::from_metadata(&file.metadata()?)
    }

    pub(super) fn from_metadata(metadata: &fs::Metadata) -> std::io::Result<Self> {
        use std::os::unix::fs::MetadataExt;

        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            byte_len: metadata.len(),
            modified_seconds: metadata.mtime(),
            modified_nanos: metadata.mtime_nsec(),
        })
    }
}

#[cfg(not(unix))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FileIdentity {
    byte_len: u64,
    modified: std::time::SystemTime,
}

#[cfg(not(unix))]
impl FileIdentity {
    fn from_open_file(file: &fs::File) -> std::io::Result<Self> {
        Self::from_metadata(&file.metadata()?)
    }

    pub(super) fn from_metadata(metadata: &fs::Metadata) -> std::io::Result<Self> {
        Ok(Self {
            byte_len: metadata.len(),
            modified: metadata.modified()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::TempWorkspace;
    use bowline_core::workspace_graph::workspace_content_id;

    fn verified_cache(
        temp: &TempWorkspace,
        key: [u8; 32],
        bytes: &[u8],
    ) -> (LocalContentCache, ContentId) {
        let cache = LocalContentCache::open(temp.root().join("cache")).expect("cache");
        let content_id = workspace_content_id(key, bytes);
        cache.put_content(&content_id, bytes).expect("cache bytes");
        cache.get_content(&content_id, key).expect("verify cache");
        (cache, content_id)
    }

    #[test]
    fn same_length_change_is_not_equal_to_verified_content() {
        let temp = TempWorkspace::new("content-identity-same-length").expect("temp");
        let (cache, content_id) = verified_cache(&temp, [7_u8; 32], b"canonical");
        let path = temp.root().join("large.bin");
        fs::write(&path, b"different").expect("changed bytes");

        assert!(!verified_content_matches_path(&cache, &content_id, &path).expect("comparison"));
    }

    #[test]
    fn replacement_during_comparison_fails_as_concurrent_change() {
        let temp = TempWorkspace::new("content-identity-concurrent").expect("temp");
        let bytes = vec![b'a'; COMPARE_BUFFER_BYTES * 2];
        let (cache, content_id) = verified_cache(&temp, [8_u8; 32], &bytes);
        let path = temp.root().join("large.bin");
        fs::write(&path, &bytes).expect("source");

        let error = verified_content_matches_path_with(&cache, &content_id, &path, || {
            let replacement = temp.root().join("replacement.bin");
            fs::write(&replacement, vec![b'b'; bytes.len()]).expect("replacement");
            fs::rename(replacement, &path).expect("replace source");
        })
        .expect_err("replacement must be detected");

        assert!(matches!(
            error,
            WorkViewError::ContentChangedDuringCapture { .. }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_replacement_during_inline_capture_is_rejected() {
        use std::os::unix::fs::symlink;

        let temp = TempWorkspace::new("content-identity-inline-symlink-race").expect("temp");
        let path = temp.root().join("source.env");
        let outside = temp.root().join("outside.env");
        fs::write(&path, "expected").expect("source");
        fs::write(&outside, "outside-secret").expect("outside");
        let expected = FileIdentity::from_metadata(&fs::symlink_metadata(&path).unwrap()).unwrap();

        let error = capture_stable_bytes_with(&path, expected, || {
            fs::remove_file(&path).expect("remove source");
            symlink(&outside, &path).expect("replacement link");
        })
        .expect_err("symlink swap must fail");

        assert!(matches!(
            error,
            WorkViewError::ContentChangedDuringCapture { .. }
        ));
        assert_eq!(fs::read(&outside).unwrap(), b"outside-secret");
    }

    #[cfg(unix)]
    #[test]
    fn no_follow_open_rejects_an_initial_symlink() {
        use std::os::unix::fs::symlink;

        let temp = TempWorkspace::new("content-identity-initial-symlink").expect("temp");
        let outside = temp.root().join("outside");
        let link = temp.root().join("link");
        fs::write(&outside, "outside-secret").expect("outside");
        symlink(&outside, &link).expect("link");

        assert!(open_stable_regular_file(&link, None).is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"outside-secret");
    }

    #[test]
    fn corrupt_verified_cache_is_an_explicit_error() {
        let temp = TempWorkspace::new("content-identity-corrupt-cache").expect("temp");
        let (cache, content_id) = verified_cache(&temp, [9_u8; 32], b"canonical");
        let content_path = temp.root().join("cache/content").join(content_id.as_str());
        fs::write(content_path, b"corrupted").expect("corrupt cache");
        let path = temp.root().join("large.bin");
        fs::write(&path, b"canonical").expect("source");

        let error = verified_content_matches_path(&cache, &content_id, &path)
            .expect_err("corrupt cache must fail");
        assert!(matches!(
            error,
            WorkViewError::ExposedBaseContentUnavailable { .. }
        ));
    }

    #[test]
    fn comparison_handles_short_reads_with_bounded_requests() {
        let bytes = vec![b'z'; COMPARE_BUFFER_BYTES * 3 + 11];
        let mut left = RecordingReader::new(&bytes, 7);
        let mut right = RecordingReader::new(&bytes, 13);

        assert!(readers_match(&mut left, &mut right).expect("short-read comparison"));
        assert!(left.max_requested <= COMPARE_BUFFER_BYTES);
        assert!(right.max_requested <= COMPARE_BUFFER_BYTES);
    }

    struct RecordingReader<'a> {
        bytes: &'a [u8],
        position: usize,
        chunk_limit: usize,
        max_requested: usize,
    }

    impl<'a> RecordingReader<'a> {
        fn new(bytes: &'a [u8], chunk_limit: usize) -> Self {
            Self {
                bytes,
                position: 0,
                chunk_limit,
                max_requested: 0,
            }
        }
    }

    impl Read for RecordingReader<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            self.max_requested = self.max_requested.max(buffer.len());
            let remaining = &self.bytes[self.position..];
            let count = remaining.len().min(buffer.len()).min(self.chunk_limit);
            buffer[..count].copy_from_slice(&remaining[..count]);
            self.position += count;
            Ok(count)
        }
    }
}
