use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use super::*;
use bowline_storage::{
    ContentSourceReader, PackRecordReader, PackStreamWriteOutput, PackfileError,
    ReopenableObjectSource, write_source_pack_reader_batches_with,
};

use crate::sync::PreparedContent;

pub(super) const SOURCE_SEGMENT_BYTES: u64 = 4 * 1024 * 1024;
const SEGMENT_FORMAT_VERSION: u16 = 1;

static NEXT_UPLOAD_PACK_SPOOL_SEED: AtomicU64 = AtomicU64::new(1);

pub(super) struct PreparedSourcePack(pub(super) StreamedSourcePack);

impl ReopenableObjectSource for UploadPackSpoolFile {
    fn open(&self) -> io::Result<Box<dyn io::Read + Send>> {
        Ok(Box::new(SealedFileReader {
            file: self.file.clone(),
            offset: 0,
            byte_len: self.byte_len,
        }))
    }
}

impl PreparedSourcePack {
    pub(super) fn pack_id(&self) -> &PackId {
        &self.0.output.pack_id
    }

    pub(super) fn locators(&self) -> &[ContentLocator] {
        &self.0.output.locators
    }

    pub(super) fn object_key(&self) -> &ObjectKey {
        &self.0.output.object_key
    }

    pub(super) fn byte_len(&self) -> u64 {
        self.0.output.byte_len
    }

    pub(super) fn hash(&self) -> String {
        self.0.output.hash.clone()
    }
}

pub(super) struct StreamedSourcePack {
    pub(super) output: PackStreamWriteOutput,
    pub(super) spool: UploadPackSpoolFile,
}

#[derive(Debug)]
pub(super) struct UploadPackSpoolFile {
    file: Arc<File>,
    byte_len: u64,
    #[cfg(test)]
    original_path: PathBuf,
}

impl UploadPackSpoolFile {
    #[cfg(test)]
    pub(super) fn reader(&self) -> Result<Box<dyn Read + Send>, UploadError> {
        ReopenableObjectSource::open(self)
            .map_err(ByteStoreError::Io)
            .map_err(Into::into)
    }

    #[cfg(test)]
    pub(super) fn original_path(&self) -> &std::path::Path {
        &self.original_path
    }
}

pub(super) struct UploadPackSpoolBuilder {
    path: PathBuf,
}

impl UploadPackSpoolBuilder {
    pub(super) fn create() -> Result<(Self, File), UploadError> {
        for _ in 0..32 {
            let path = upload_pack_spool_path();
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&path) {
                Ok(file) => return Ok((Self { path }, file)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(ByteStoreError::Io(error).into()),
            }
        }
        Err(ByteStoreError::Io(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not create unique upload pack spool file",
        ))
        .into())
    }

    pub(super) fn seal(
        self,
        key: &ObjectKey,
        byte_len: u64,
        expected_hash: &str,
    ) -> Result<UploadPackSpoolFile, UploadError> {
        let mut source = File::open(&self.path).map_err(ByteStoreError::Io)?;
        let mut sealed = create_anonymous_spool_file().map_err(ByteStoreError::Io)?;
        let mut hasher = blake3::Hasher::new();
        let mut copied = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = source.read(&mut buffer).map_err(ByteStoreError::Io)?;
            if read == 0 {
                break;
            }
            copied = copied.checked_add(read as u64).ok_or_else(|| {
                ByteStoreError::Io(io::Error::other("upload spool length overflowed"))
            })?;
            hasher.update(&buffer[..read]);
            sealed
                .write_all(&buffer[..read])
                .map_err(ByteStoreError::Io)?;
        }
        sealed.flush().map_err(ByteStoreError::Io)?;
        let actual_hash = format!("b3_{}", hasher.finalize().to_hex());
        if copied != byte_len || actual_hash != expected_hash {
            return Err(ByteStoreError::CorruptObject {
                key: key.clone(),
                reason: "upload spool changed before sealing",
            }
            .into());
        }
        drop(source);
        #[cfg(test)]
        let original_path = self.path.clone();
        fs::remove_file(&self.path).map_err(ByteStoreError::Io)?;
        Ok(UploadPackSpoolFile {
            file: Arc::new(sealed),
            byte_len,
            #[cfg(test)]
            original_path,
        })
    }
}

impl Drop for UploadPackSpoolBuilder {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

struct SealedFileReader {
    file: Arc<File>,
    offset: u64,
    byte_len: u64,
}

impl Read for SealedFileReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.offset == self.byte_len {
            return Ok(0);
        }
        let remaining = usize::try_from((self.byte_len - self.offset).min(buffer.len() as u64))
            .map_err(|_| io::Error::other("sealed upload spool read length overflowed"))?;
        let read = read_file_at(&self.file, &mut buffer[..remaining], self.offset)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "sealed upload spool ended early",
            ));
        }
        self.offset += read as u64;
        Ok(read)
    }
}

#[cfg(unix)]
fn read_file_at(file: &File, buffer: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buffer, offset)
}

#[cfg(windows)]
fn read_file_at(file: &File, buffer: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, buffer, offset)
}

fn create_anonymous_spool_file() -> io::Result<File> {
    for _ in 0..32 {
        let path = upload_pack_spool_path();
        let mut options = OpenOptions::new();
        options.create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.read(true).write(true).mode(0o600);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            const GENERIC_READ: u32 = 0x8000_0000;
            const GENERIC_WRITE: u32 = 0x4000_0000;
            const DELETE: u32 = 0x0001_0000;
            const FILE_SHARE_READ: u32 = 0x0000_0001;
            const FILE_SHARE_WRITE: u32 = 0x0000_0002;
            const FILE_SHARE_DELETE: u32 = 0x0000_0004;
            const FILE_FLAG_DELETE_ON_CLOSE: u32 = 0x0400_0000;
            const ACCESS_RIGHTS: u32 = GENERIC_READ | GENERIC_WRITE | DELETE;
            const SHARE_MODE: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
            const _: () = assert!(ACCESS_RIGHTS == 0xc001_0000);
            const _: () = assert!(SHARE_MODE == 0x0000_0007);

            // DELETE access is required to mark the handle delete-on-close. Explicit access_mode
            // avoids mixing the Windows mask with the generic read/write OpenOptions setters.
            options
                .access_mode(ACCESS_RIGHTS)
                .share_mode(SHARE_MODE)
                .custom_flags(FILE_FLAG_DELETE_ON_CLOSE);
        }
        match options.open(&path) {
            Ok(file) => {
                #[cfg(unix)]
                {
                    // Unix keeps the inode alive through the handle after unlinking the pathname.
                    if let Err(error) = fs::remove_file(&path) {
                        drop(file);
                        let _ = fs::remove_file(&path);
                        return Err(error);
                    }
                    return Ok(file);
                }
                #[cfg(windows)]
                {
                    // Delete-on-close already made the pathname delete-pending; calling
                    // DeleteFile again can fail even though the anonymous handle is valid.
                    return Ok(file);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not create unique anonymous upload spool",
    ))
}

pub(super) struct PreparedSegmentedSourcePacks {
    pub(super) packs: Vec<PreparedSourcePack>,
    pub(super) layouts: BTreeMap<ContentId, ContentLayout>,
}

pub(super) fn prepare_segmented_source_packs(
    workspace_id: bowline_core::ids::WorkspaceId,
    records: &[PackRecordReader<'_>],
    existing_segment_locators: &BTreeMap<ContentId, ContentLocator>,
    target_raw_pack_size: usize,
    storage_key: StorageKey,
    key_epoch: u32,
) -> Result<PreparedSegmentedSourcePacks, UploadError> {
    let segment_sources = records
        .iter()
        .flat_map(segment_sources_for_record)
        .collect::<Vec<_>>();
    let pack_records = segment_sources
        .iter()
        .filter(|segment| !existing_segment_locators.contains_key(&segment.content_id))
        .map(|segment| PackRecordReader {
            content_id: &segment.content_id,
            source: segment,
        })
        .collect::<Vec<_>>();
    let mut packs = Vec::new();
    write_source_pack_reader_batches_with(
        workspace_id,
        &pack_records,
        target_raw_pack_size as u64,
        storage_key,
        key_epoch,
        |writer, batch| {
            let (spool_builder, mut file) =
                UploadPackSpoolBuilder::create().map_err(|error| match error {
                    UploadError::Packfile(error) => error,
                    UploadError::ByteStore(ByteStoreError::Io(error)) => error.into(),
                    _ => PackfileError::Io(io::Error::other(error.to_string())),
                })?;
            let output = writer.write_reader_streaming(batch, &mut file)?;
            file.sync_all()?;
            drop(file);
            let spool = spool_builder
                .seal(&output.object_key, output.byte_len, &output.hash)
                .map_err(|error| PackfileError::Io(io::Error::other(error.to_string())))?;
            packs.push(PreparedSourcePack(StreamedSourcePack { output, spool }));
            Ok(())
        },
    )?;
    let mut locators = existing_segment_locators.clone();
    locators.extend(locators_by_prepared_content(&packs));
    let layouts = records
        .iter()
        .map(|record| {
            let layout = layout_for_record(record, &locators)?;
            Ok((record.content_id.clone(), layout))
        })
        .collect::<Result<BTreeMap<_, _>, UploadError>>()?;
    Ok(PreparedSegmentedSourcePacks { packs, layouts })
}

pub(super) fn locators_by_prepared_content(
    packs: &[PreparedSourcePack],
) -> BTreeMap<ContentId, ContentLocator> {
    let mut locator_by_content = BTreeMap::<ContentId, ContentLocator>::new();
    for pack in packs {
        for locator in pack.locators() {
            locator_by_content.insert(locator.content_id.clone(), locator.clone());
        }
    }
    locator_by_content
}

fn upload_pack_spool_path() -> PathBuf {
    let sequence = NEXT_UPLOAD_PACK_SPOOL_SEED.fetch_add(1, Ordering::Relaxed);
    let now_nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    std::env::temp_dir().join(format!(
        "bowline-upload-pack-spool-{}-{sequence}-{now_nanos}.tmp",
        std::process::id()
    ))
}

struct SegmentContentSource<'a> {
    content_id: ContentId,
    source: &'a dyn ContentSourceReader,
    offset: u64,
    length: u64,
}

impl ContentSourceReader for SegmentContentSource<'_> {
    fn logical_len(&self) -> u64 {
        self.length
    }

    fn open(&self) -> Result<Box<dyn Read + Send + '_>, PackfileError> {
        self.source.open_range(self.offset, self.length)
    }

    fn open_range(
        &self,
        offset: u64,
        length: u64,
    ) -> Result<Box<dyn Read + Send + '_>, PackfileError> {
        offset
            .checked_add(length)
            .filter(|end| *end <= self.length)
            .ok_or(PackfileError::InvalidRecordRange)?;
        let source_offset = self
            .offset
            .checked_add(offset)
            .ok_or(PackfileError::InvalidRecordRange)?;
        self.source.open_range(source_offset, length)
    }
}

fn segment_sources_for_record<'a>(
    record: &'a PackRecordReader<'a>,
) -> impl Iterator<Item = SegmentContentSource<'a>> + 'a {
    let logical_len = record.source.logical_len();
    let segment_count = logical_len.div_ceil(SOURCE_SEGMENT_BYTES);
    (0..segment_count).map(move |ordinal| {
        let offset = ordinal * SOURCE_SEGMENT_BYTES;
        let length = (logical_len - offset).min(SOURCE_SEGMENT_BYTES);
        SegmentContentSource {
            content_id: segment_content_id(record.content_id, ordinal, length),
            source: record.source,
            offset,
            length,
        }
    })
}

pub(crate) fn segment_content_id(
    logical_content_id: &ContentId,
    ordinal: u64,
    length: u64,
) -> ContentId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"bowline-segmented-v1\0");
    hasher.update(logical_content_id.as_str().as_bytes());
    hasher.update(&ordinal.to_le_bytes());
    hasher.update(&length.to_le_bytes());
    ContentId::new(format!("seg_{}", hasher.finalize().to_hex()))
}

pub(super) fn prepared_segment_bytes(
    prepared_content: &BTreeMap<ContentId, PreparedContent>,
    segment_content_id: &ContentId,
    expected_len: u64,
) -> Result<Option<Vec<u8>>, PackfileError> {
    for (logical_content_id, content) in prepared_content {
        let record = PackRecordReader {
            content_id: logical_content_id,
            source: content,
        };
        for segment in segment_sources_for_record(&record) {
            if &segment.content_id != segment_content_id {
                continue;
            }
            if segment.length != expected_len {
                return Err(PackfileError::ContentSourceLengthMismatch {
                    expected: segment.length,
                    actual: expected_len,
                });
            }
            let mut bytes = Vec::with_capacity(expected_len as usize);
            segment
                .open()?
                .take(expected_len.saturating_add(1))
                .read_to_end(&mut bytes)?;
            if bytes.len() as u64 != expected_len {
                return Err(PackfileError::ContentSourceLengthMismatch {
                    expected: expected_len,
                    actual: bytes.len() as u64,
                });
            }
            return Ok(Some(bytes));
        }
    }
    Ok(None)
}

fn layout_for_record(
    record: &PackRecordReader<'_>,
    locators: &BTreeMap<ContentId, ContentLocator>,
) -> Result<ContentLayout, UploadError> {
    let logical_length = record.source.logical_len();
    let segments = segment_sources_for_record(record)
        .enumerate()
        .map(|(ordinal, segment)| {
            let locator = locators
                .get(&segment.content_id)
                .ok_or(PackfileError::MissingRecord)?;
            Ok(SegmentLocator {
                ordinal: u32::try_from(ordinal).map_err(|_| PackfileError::PackTooLarge)?,
                plaintext_length: segment.length,
                segment_id: SegmentId::new(segment.content_id.as_str()),
                pack_id: locator
                    .pack_id
                    .clone()
                    .ok_or(PackfileError::InvalidRecordRange)?,
                offset: locator.offset.ok_or(PackfileError::InvalidRecordRange)?,
                length: locator.length.ok_or(PackfileError::InvalidRecordRange)?,
                format_version: SEGMENT_FORMAT_VERSION,
            })
        })
        .collect::<Result<Vec<_>, PackfileError>>()?;
    Ok(ContentLayout::SegmentedV1 {
        logical_content_id: record.content_id.clone(),
        logical_length,
        segment_size: SOURCE_SEGMENT_BYTES,
        segments,
    })
}

#[cfg(test)]
mod tests {
    use std::{
        io::Cursor,
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use super::*;

    struct RangeTrackingSource {
        bytes: Vec<u8>,
        full_opens: AtomicUsize,
        ranges: Mutex<Vec<(u64, u64)>>,
    }

    impl ContentSourceReader for RangeTrackingSource {
        fn logical_len(&self) -> u64 {
            self.bytes.len() as u64
        }

        fn open(&self) -> Result<Box<dyn Read + Send + '_>, PackfileError> {
            self.full_opens.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(Cursor::new(self.bytes.as_slice())))
        }

        fn open_range(
            &self,
            offset: u64,
            length: u64,
        ) -> Result<Box<dyn Read + Send + '_>, PackfileError> {
            let start = usize::try_from(offset).map_err(|_| PackfileError::InvalidRecordRange)?;
            let length = usize::try_from(length).map_err(|_| PackfileError::InvalidRecordRange)?;
            let end = start
                .checked_add(length)
                .filter(|end| *end <= self.bytes.len())
                .ok_or(PackfileError::InvalidRecordRange)?;
            self.ranges
                .lock()
                .expect("range tracker lock remains available")
                .push((offset, length as u64));
            Ok(Box::new(Cursor::new(&self.bytes[start..end])))
        }
    }

    #[test]
    fn segment_sources_read_exact_contiguous_ranges_without_prefix_scans() {
        let logical_length = SOURCE_SEGMENT_BYTES * 2 + 37;
        let bytes = (0..logical_length)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        let source = RangeTrackingSource {
            bytes,
            full_opens: AtomicUsize::new(0),
            ranges: Mutex::new(Vec::new()),
        };
        let content_id = ContentId::new("cid_segment_range_regression");
        let record = PackRecordReader {
            content_id: &content_id,
            source: &source,
        };
        let segments = segment_sources_for_record(&record).collect::<Vec<_>>();
        let mut reconstructed = Vec::new();

        for segment in &segments {
            let mut segment_bytes = Vec::new();
            segment
                .open()
                .expect("segment range opens")
                .read_to_end(&mut segment_bytes)
                .expect("segment range reads");
            let start = usize::try_from(segment.offset).expect("segment offset fits usize");
            let end = start + usize::try_from(segment.length).expect("segment length fits usize");
            assert_eq!(segment_bytes, source.bytes[start..end]);
            reconstructed.extend_from_slice(&segment_bytes);
        }

        assert_eq!(reconstructed, source.bytes);
        assert_eq!(source.full_opens.load(Ordering::SeqCst), 0);
        let ranges = source
            .ranges
            .lock()
            .expect("range tracker lock remains available");
        assert_eq!(
            ranges.as_slice(),
            &[
                (0, SOURCE_SEGMENT_BYTES),
                (SOURCE_SEGMENT_BYTES, SOURCE_SEGMENT_BYTES),
                (SOURCE_SEGMENT_BYTES * 2, 37),
            ]
        );
        assert_eq!(
            ranges.iter().map(|(_, length)| length).sum::<u64>(),
            logical_length
        );
    }
}
