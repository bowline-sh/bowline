use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use super::*;

const STREAM_COPY_BUFFER_LEN: usize = 64 * 1024;

static NEXT_SPOOL_FILE_SEED: AtomicU64 = AtomicU64::new(1);

#[cfg(test)]
thread_local! {
    static TEST_SPOOL_CREATIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn record_test_spool_creation() {
    TEST_SPOOL_CREATIONS.set(TEST_SPOOL_CREATIONS.get() + 1);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackStreamWriteOutput {
    pub pack_id: PackId,
    pub object_key: ObjectKey,
    pub locators: Vec<ContentLocator>,
    pub byte_len: u64,
    pub hash: String,
}

impl PackWriter {
    /// Convenience wrapper for bounded in-memory records. Production sources
    /// should use `write_reader_streaming` so retries can open immutable input
    /// again without retaining every record's bytes.
    pub fn write_streaming(
        &self,
        records: &[PackRecordInput],
        writer: &mut impl Write,
    ) -> Result<PackStreamWriteOutput, PackfileError> {
        let records = records
            .iter()
            .map(|record| PackRecordRef {
                content_id: &record.content_id,
                bytes: &record.bytes,
            })
            .collect::<Vec<_>>();
        self.write_streaming_refs(&records, writer)
    }

    /// Convenience wrapper for already-bounded borrowed slices.
    pub fn write_streaming_refs(
        &self,
        records: &[PackRecordRef<'_>],
        writer: &mut impl Write,
    ) -> Result<PackStreamWriteOutput, PackfileError> {
        let sources = records
            .iter()
            .map(|record| SliceContentSource {
                bytes: record.bytes,
            })
            .collect::<Vec<_>>();
        let reader_records = records
            .iter()
            .zip(&sources)
            .map(|(record, source)| PackRecordReader {
                content_id: record.content_id,
                source,
            })
            .collect::<Vec<_>>();
        self.write_reader_streaming(&reader_records, writer)
    }

    /// Writes pack-v2 from reopenable immutable sources. The current envelope
    /// still seals one whole record in memory, but plaintext from prior records
    /// is released before the next source is opened.
    pub fn write_reader_streaming(
        &self,
        records: &[PackRecordReader<'_>],
        writer: &mut impl Write,
    ) -> Result<PackStreamWriteOutput, PackfileError> {
        if records.is_empty() {
            return Err(PackfileError::EmptyPack);
        }

        let workspace_hash = workspace_id_hash(self.workspace_id.as_str());
        let spooled_records = self.spool_reader_records(records, &workspace_hash)?;
        let indexed_records =
            index_spooled_records(&self.pack_id, &workspace_hash, &spooled_records.records)?;
        let mut header_and_directory = Vec::new();
        write_header_and_directory(
            &mut header_and_directory,
            &self.pack_id,
            &workspace_hash,
            &indexed_records,
        )?;

        let mut hasher = blake3::Hasher::new();
        let mut byte_len = 0_u64;
        write_and_hash(writer, &mut hasher, &mut byte_len, &header_and_directory)?;
        spooled_records
            .body
            .copy_to(writer, &mut hasher, &mut byte_len)?;

        let locators = indexed_records
            .iter()
            .map(|record| locator_from_record(&self.pack_id, record))
            .collect::<Vec<_>>();

        Ok(PackStreamWriteOutput {
            pack_id: self.pack_id.clone(),
            object_key: ObjectKey::from_pack_id(&self.pack_id)?,
            locators,
            byte_len,
            hash: format!("b3_{}", hasher.finalize().to_hex()),
        })
    }

    fn spool_reader_records(
        &self,
        records: &[PackRecordReader<'_>],
        workspace_hash: &str,
    ) -> Result<SpooledRecords, PackfileError> {
        let mut nonce_tracker = EnvelopeNonceTracker::new();
        let mut spooled_records = Vec::with_capacity(records.len());
        let mut spool = SpoolFileBuilder::create()?;
        let mut body_hasher = blake3::Hasher::new();
        let mut body_len = 0_u64;
        for record in records {
            let bytes = read_source_record(record.source)?;
            let sealed = self.seal_record(
                workspace_hash,
                record.content_id,
                &bytes,
                &mut nonce_tracker,
            )?;
            let length = sealed.sealed.as_bytes().len() as u64;
            spool.write_all(sealed.sealed.as_bytes())?;
            body_hasher.update(sealed.sealed.as_bytes());
            body_len = body_len
                .checked_add(length)
                .ok_or(PackfileError::PackTooLarge)?;
            spooled_records.push(SpooledRecord {
                content_id: sealed.content_id,
                raw_size: sealed.raw_size,
                length,
            });
        }
        let body = spool.finish(body_len, format!("b3_{}", body_hasher.finalize().to_hex()))?;
        Ok(SpooledRecords {
            records: spooled_records,
            body,
        })
    }

    #[cfg(test)]
    fn spool_records(
        &self,
        records: &[PackRecordRef<'_>],
        workspace_hash: &str,
    ) -> Result<SpooledRecords, PackfileError> {
        let sources = records
            .iter()
            .map(|record| SliceContentSource {
                bytes: record.bytes,
            })
            .collect::<Vec<_>>();
        let reader_records = records
            .iter()
            .zip(&sources)
            .map(|(record, source)| PackRecordReader {
                content_id: record.content_id,
                source,
            })
            .collect::<Vec<_>>();
        self.spool_reader_records(&reader_records, workspace_hash)
    }
}

impl PackRecordRef<'_> {
    fn raw_len(self) -> u64 {
        self.bytes.len() as u64
    }
}

impl PackRecordReader<'_> {
    fn raw_len(self) -> u64 {
        self.source.logical_len()
    }
}

pub fn write_source_pack_ref_batches_with(
    workspace_id: WorkspaceId,
    records: &[PackRecordRef<'_>],
    target_raw_pack_size: usize,
    key: StorageKey,
    key_epoch: u32,
    mut on_pack: impl FnMut(&PackWriter, &[PackRecordRef<'_>]) -> Result<(), PackfileError>,
) -> Result<(), PackfileError> {
    if records.is_empty() {
        return Ok(());
    }

    let target_raw_pack_size = target_raw_pack_size.max(1);
    let batch_seed =
        new_pack_ref_batch_seed(&workspace_id, records, target_raw_pack_size, key_epoch);
    let mut batch_start = 0_usize;
    let mut batch_raw_size = 0_u64;
    let mut sequence = 1_usize;

    for (index, record) in records.iter().enumerate() {
        if index > batch_start
            && batch_raw_size.saturating_add(record.raw_len()) > target_raw_pack_size as u64
        {
            let writer = PackWriter::new(
                workspace_id.clone(),
                opaque_pack_id(&batch_seed, sequence),
                key,
                key_epoch,
            );
            on_pack(&writer, &records[batch_start..index])?;
            sequence += 1;
            batch_start = index;
            batch_raw_size = 0;
        }
        batch_raw_size += record.raw_len();
    }

    if batch_start < records.len() {
        let writer = PackWriter::new(
            workspace_id,
            opaque_pack_id(&batch_seed, sequence),
            key,
            key_epoch,
        );
        on_pack(&writer, &records[batch_start..])?;
    }

    Ok(())
}

/// Forms pack batches from declared logical lengths without opening sources.
/// The callback decides when and where each pack is written.
pub fn write_source_pack_reader_batches_with(
    workspace_id: WorkspaceId,
    records: &[PackRecordReader<'_>],
    target_raw_pack_size: u64,
    key: StorageKey,
    key_epoch: u32,
    mut on_pack: impl FnMut(&PackWriter, &[PackRecordReader<'_>]) -> Result<(), PackfileError>,
) -> Result<(), PackfileError> {
    if records.is_empty() {
        return Ok(());
    }

    let target_raw_pack_size = target_raw_pack_size.max(1);
    let batch_seed =
        new_pack_reader_batch_seed(&workspace_id, records, target_raw_pack_size, key_epoch);
    let mut batch_start = 0_usize;
    let mut batch_raw_size = 0_u64;
    let mut sequence = 1_usize;

    for (index, record) in records.iter().enumerate() {
        if index > batch_start
            && batch_raw_size.saturating_add(record.raw_len()) > target_raw_pack_size
        {
            let writer = PackWriter::new(
                workspace_id.clone(),
                opaque_pack_id(&batch_seed, sequence),
                key,
                key_epoch,
            );
            on_pack(&writer, &records[batch_start..index])?;
            sequence += 1;
            batch_start = index;
            batch_raw_size = 0;
        }
        batch_raw_size = batch_raw_size.saturating_add(record.raw_len());
    }

    if batch_start < records.len() {
        let writer = PackWriter::new(
            workspace_id,
            opaque_pack_id(&batch_seed, sequence),
            key,
            key_epoch,
        );
        on_pack(&writer, &records[batch_start..])?;
    }

    Ok(())
}

fn index_spooled_records(
    pack_id: &PackId,
    workspace_hash: &str,
    records: &[SpooledRecord],
) -> Result<Vec<PackRecordIndexEntry>, PackfileError> {
    let mut cursor = u64::try_from(directory_len_for_spooled(pack_id, workspace_hash, records)?)
        .map_err(|_| PackfileError::PackTooLarge)?;
    records
        .iter()
        .map(|record| {
            let entry = PackRecordIndexEntry {
                content_id: record.content_id.clone(),
                raw_size: record.raw_size,
                offset: cursor,
                length: record.length,
            };
            cursor = cursor
                .checked_add(entry.length)
                .ok_or(PackfileError::PackTooLarge)?;
            Ok(entry)
        })
        .collect()
}

fn directory_len_for_spooled(
    pack_id: &PackId,
    workspace_hash: &str,
    records: &[SpooledRecord],
) -> Result<usize, PackfileError> {
    let mut len = HEADER_FIXED_LEN + pack_id.as_str().len() + workspace_hash.len();
    for record in records {
        len = len
            .checked_add(ENTRY_FIXED_LEN)
            .and_then(|value| value.checked_add(record.content_id.as_str().len()))
            .ok_or(PackfileError::PackTooLarge)?;
    }
    Ok(len)
}

#[derive(Debug)]
struct SpooledRecord {
    content_id: ContentId,
    raw_size: u64,
    length: u64,
}

#[derive(Debug)]
struct SpooledRecords {
    records: Vec<SpooledRecord>,
    body: SpoolFile,
}

#[derive(Debug)]
struct SpoolFile {
    path: PathBuf,
    len: u64,
    hash: String,
}

impl SpoolFile {
    fn copy_to(
        &self,
        writer: &mut impl Write,
        hasher: &mut blake3::Hasher,
        byte_len: &mut u64,
    ) -> Result<(), PackfileError> {
        let mut file = File::open(&self.path)?;
        let mut buffer = [0_u8; STREAM_COPY_BUFFER_LEN];
        let mut copied = 0_u64;
        let mut copied_hasher = blake3::Hasher::new();
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            copied = copied
                .checked_add(read as u64)
                .ok_or(PackfileError::PackTooLarge)?;
            copied_hasher.update(&buffer[..read]);
            write_and_hash(writer, hasher, byte_len, &buffer[..read])?;
        }
        let copied_hash = format!("b3_{}", copied_hasher.finalize().to_hex());
        if copied != self.len || copied_hash != self.hash {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "spooled pack body changed identity",
            )
            .into());
        }
        Ok(())
    }
}

impl Drop for SpoolFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

struct SpoolFileBuilder {
    path: PathBuf,
    file: Option<File>,
    keep_path: bool,
}

impl SpoolFileBuilder {
    fn create() -> Result<Self, PackfileError> {
        for _ in 0..32 {
            let path = spool_path();
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&path) {
                Ok(file) => {
                    #[cfg(test)]
                    record_test_spool_creation();
                    return Ok(Self {
                        path,
                        file: Some(file),
                        keep_path: false,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not create unique pack spool file",
        )
        .into())
    }

    fn write_all(&mut self, bytes: &[u8]) -> Result<(), PackfileError> {
        self.file
            .as_mut()
            .ok_or_else(|| io::Error::other("pack spool writer was already closed"))?
            .write_all(bytes)
            .map_err(Into::into)
    }

    fn finish(mut self, len: u64, hash: String) -> Result<SpoolFile, PackfileError> {
        let mut file = self
            .file
            .take()
            .ok_or_else(|| io::Error::other("pack spool writer was already closed"))?;
        file.flush()?;
        let actual_len = file.metadata()?.len();
        drop(file);
        if actual_len != len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "pack spool length changed while sealing",
            )
            .into());
        }
        self.keep_path = true;
        Ok(SpoolFile {
            path: self.path.clone(),
            len,
            hash,
        })
    }
}

impl Drop for SpoolFileBuilder {
    fn drop(&mut self) {
        drop(self.file.take());
        if !self.keep_path {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn spool_path() -> PathBuf {
    let sequence = NEXT_SPOOL_FILE_SEED.fetch_add(1, Ordering::Relaxed);
    let now_nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    std::env::temp_dir().join(format!(
        "bowline-pack-spool-{}-{sequence}-{now_nanos}.tmp",
        std::process::id()
    ))
}

fn write_and_hash(
    writer: &mut impl Write,
    hasher: &mut blake3::Hasher,
    byte_len: &mut u64,
    bytes: &[u8],
) -> Result<(), PackfileError> {
    writer.write_all(bytes)?;
    hasher.update(bytes);
    *byte_len = byte_len
        .checked_add(bytes.len() as u64)
        .ok_or(PackfileError::PackTooLarge)?;
    Ok(())
}

#[cfg(test)]
mod aggregate_spool_tests {
    use super::*;

    struct SpoolCreationObserver;

    impl SpoolCreationObserver {
        fn start() -> Self {
            TEST_SPOOL_CREATIONS.set(0);
            Self
        }

        fn count(&self) -> u64 {
            TEST_SPOOL_CREATIONS.get()
        }
    }

    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("injected downstream write failure"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn streaming_writer_uses_one_owner_only_body_spool_per_pack() {
        let observer = SpoolCreationObserver::start();
        let records = (0..32)
            .map(|index| {
                let content_id = ContentId::new(format!("cid_aggregate_{index}"));
                let bytes = vec![index as u8; 1024];
                (content_id, bytes)
            })
            .collect::<Vec<_>>();
        let refs = records
            .iter()
            .map(|(content_id, bytes)| PackRecordRef { content_id, bytes })
            .collect::<Vec<_>>();
        let output = PackWriter::new(
            WorkspaceId::new("ws_aggregate_spool"),
            PackId::new("pk_00112233445566aa"),
            StorageKey::deterministic(91),
            1,
        )
        .write_streaming_refs(&refs, &mut Vec::new())
        .expect("production streaming writer writes pack");

        assert_eq!(output.locators.len(), records.len());
        assert_eq!(observer.count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn aggregate_spool_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let builder = SpoolFileBuilder::create().expect("spool creates");
        let path = builder.path.clone();
        let mode = builder
            .file
            .as_ref()
            .expect("spool file")
            .metadata()
            .expect("spool metadata")
            .permissions()
            .mode()
            & 0o777;
        drop(builder);

        assert_eq!(mode, 0o600);
        assert!(!path.exists());
    }

    #[test]
    fn aggregate_spool_is_removed_after_downstream_write_error() {
        let content_id = ContentId::new("cid_spool_cleanup");
        let bytes = vec![7_u8; 256 * 1024];
        let writer = PackWriter::new(
            WorkspaceId::new("ws_spool_cleanup"),
            PackId::new("pk_spool_cleanup"),
            StorageKey::deterministic(92),
            1,
        );
        let spooled = writer
            .spool_records(
                &[PackRecordRef {
                    content_id: &content_id,
                    bytes: &bytes,
                }],
                &workspace_id_hash("ws_spool_cleanup"),
            )
            .expect("record spools");
        let path = spooled.body.path.clone();
        assert!(path.exists());
        let mut output_len = 0;
        let error = spooled.body.copy_to(
            &mut FailingWriter,
            &mut blake3::Hasher::new(),
            &mut output_len,
        );
        assert!(matches!(error, Err(PackfileError::Io(_))));

        drop(spooled);

        assert!(!path.exists());
    }

    #[test]
    fn aggregate_spool_rejects_same_length_mutation() {
        let content_id = ContentId::new("cid_spool_mutation");
        let bytes = vec![9_u8; 128 * 1024];
        let writer = PackWriter::new(
            WorkspaceId::new("ws_spool_mutation"),
            PackId::new("pk_spool_mutation"),
            StorageKey::deterministic(93),
            1,
        );
        let spooled = writer
            .spool_records(
                &[PackRecordRef {
                    content_id: &content_id,
                    bytes: &bytes,
                }],
                &workspace_id_hash("ws_spool_mutation"),
            )
            .expect("record spools");
        let mut replacement = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&spooled.body.path)
            .expect("open spool replacement");
        replacement
            .write_all(&vec![0xa5; spooled.body.len as usize])
            .expect("replace spool bytes");
        replacement.flush().expect("flush replacement");
        drop(replacement);

        let mut output_len = 0;
        let error = spooled
            .body
            .copy_to(&mut Vec::new(), &mut blake3::Hasher::new(), &mut output_len)
            .expect_err("same-length mutation must fail");

        assert!(matches!(error, PackfileError::Io(_)));
    }
}
