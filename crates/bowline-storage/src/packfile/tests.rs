use bowline_core::workspace_graph::workspace_content_id;
use std::{
    io::{self, Cursor, Read, Write},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use super::*;

#[test]
fn pack_writer_groups_records_and_reads_one_by_locator() {
    let key = StorageKey::deterministic(3);
    let workspace_id = WorkspaceId::new("ws_pack");
    let records = vec![
        record([1_u8; 32], b"first file"),
        record([1_u8; 32], b"second file"),
        record([1_u8; 32], b"third file"),
    ];
    let output = PackWriter::new(
        workspace_id.clone(),
        PackId::new("pk_0011223344556677"),
        key,
        1,
    )
    .write(&records)
    .expect("pack writes");

    assert_eq!(output.locators.len(), 3);
    assert!(
        output
            .bytes
            .windows("first file".len())
            .all(|window| window != b"first file")
    );
    let target = &records[1].content_id;
    let locator = output
        .locators
        .iter()
        .find(|locator| &locator.content_id == target)
        .expect("locator exists");
    let encrypted_range = &output.bytes[locator.offset.unwrap() as usize
        ..(locator.offset.unwrap() + locator.length.unwrap()) as usize];
    let hydrated = read_record_range(
        encrypted_range,
        &workspace_id_hash(workspace_id.as_str()),
        &output.pack_id,
        target,
        key,
        1,
    )
    .expect("range opens");

    assert_eq!(hydrated, b"second file");
    assert_eq!(
        read_record_from_pack(&output.bytes, target, key, 1).expect("pack opens"),
        b"second file"
    );
}

#[test]
fn pack_writer_uses_fresh_envelope_nonce_for_same_candidate() {
    let key = StorageKey::deterministic(3);
    let workspace_id = WorkspaceId::new("ws_pack");
    let pack_id = PackId::new("pk_0011223344556677");
    let records = vec![
        record([1_u8; 32], b"first file"),
        record([1_u8; 32], b"second file"),
    ];

    let first = PackWriter::new(workspace_id.clone(), pack_id.clone(), key, 1)
        .write(&records)
        .expect("first pack writes");
    let retry = PackWriter::new(workspace_id, pack_id, key, 1)
        .write(&records)
        .expect("retry pack writes");

    assert_ne!(first.bytes, retry.bytes);
    assert_eq!(first.locators, retry.locators);
    for record in &records {
        assert_eq!(
            read_record_from_pack(&first.bytes, &record.content_id, key, 1).expect("first opens"),
            record.bytes
        );
        assert_eq!(
            read_record_from_pack(&retry.bytes, &record.content_id, key, 1).expect("retry opens"),
            record.bytes
        );
    }
}

#[test]
fn streaming_pack_writer_round_trips_like_buffered_pack() {
    let key = StorageKey::deterministic(11);
    let workspace_id = WorkspaceId::new("ws_stream_pack");
    let pack_id = PackId::new("pk_0011223344556688");
    let records = vec![
        record([9_u8; 32], b"streamed first file"),
        record([9_u8; 32], b"streamed second file with more bytes"),
        record([9_u8; 32], b"streamed third file"),
    ];
    let buffered = PackWriter::new(workspace_id.clone(), pack_id.clone(), key, 3)
        .write(&records)
        .expect("buffered pack writes");
    let mut streamed_bytes = Vec::new();
    let streamed = PackWriter::new(workspace_id, pack_id, key, 3)
        .write_streaming(&records, &mut streamed_bytes)
        .expect("streamed pack writes");

    assert_eq!(streamed.pack_id, buffered.pack_id);
    assert_eq!(streamed.object_key, buffered.object_key);
    assert_eq!(streamed.locators, buffered.locators);
    assert_eq!(streamed.byte_len, streamed_bytes.len() as u64);
    assert_eq!(streamed.hash, stable_object_hash(&streamed_bytes));

    let buffered_index = parse_index(&buffered.bytes).expect("buffered index parses");
    let streamed_index = parse_index(&streamed_bytes).expect("streamed index parses");
    assert_eq!(streamed_index.pack_id, buffered_index.pack_id);
    assert_eq!(
        streamed_index.workspace_id_hash,
        buffered_index.workspace_id_hash
    );
    assert_eq!(streamed_index.records, buffered_index.records);

    for record in &records {
        assert_eq!(
            read_record_from_pack(&buffered.bytes, &record.content_id, key, 3)
                .expect("buffered opens"),
            record.bytes
        );
        assert_eq!(
            read_record_from_pack(&streamed_bytes, &record.content_id, key, 3)
                .expect("streamed opens"),
            record.bytes
        );
    }
}

#[test]
fn streaming_pack_writer_does_not_write_final_pack_as_one_sink_chunk() {
    let key = StorageKey::deterministic(13);
    let workspace_id = WorkspaceId::new("ws_stream_pack");
    let large_bytes = deterministic_bytes(512 * 1024);
    let records = vec![record([4_u8; 32], &large_bytes)];
    let mut sink = ChunkTrackingWriter::default();
    let output = PackWriter::new(
        workspace_id.clone(),
        PackId::new("pk_0011223344556699"),
        key,
        4,
    )
    .write_streaming(&records, &mut sink)
    .expect("streamed pack writes");

    assert_eq!(output.byte_len, sink.bytes.len() as u64);
    assert_eq!(output.hash, stable_object_hash(&sink.bytes));
    assert!(sink.max_write_len < sink.bytes.len() / 2);
    assert_eq!(
        read_record_from_pack(&sink.bytes, &records[0].content_id, key, 4).expect("record opens"),
        records[0].bytes
    );
}

#[test]
fn streaming_pack_writer_rejects_empty_records() {
    let mut sink = Vec::new();
    let result = PackWriter::new(
        WorkspaceId::new("ws_stream_empty"),
        PackId::new("pk_stream_empty"),
        StorageKey::deterministic(14),
        1,
    )
    .write_streaming(&[], &mut sink);

    assert!(matches!(result, Err(PackfileError::EmptyPack)));
    assert!(sink.is_empty());
}

#[test]
fn reader_pack_writer_preserves_pack_v2_index_and_locators() {
    let key = StorageKey::deterministic(17);
    let workspace_id = WorkspaceId::new("ws_reader_pack");
    let pack_id = PackId::new("pk_1021324354657687");
    let inputs = [
        record([3_u8; 32], b"reader first file"),
        record([3_u8; 32], b"reader second file"),
    ];
    let sources = inputs
        .iter()
        .map(|input| TestContentSource::new(input.bytes.clone()))
        .collect::<Vec<_>>();
    let reader_records = inputs
        .iter()
        .zip(&sources)
        .map(|(input, source)| PackRecordReader {
            content_id: &input.content_id,
            source,
        })
        .collect::<Vec<_>>();
    let buffered = PackWriter::new(workspace_id.clone(), pack_id.clone(), key, 2)
        .write(&inputs)
        .expect("slice-backed pack writes");
    let mut reader_bytes = Vec::new();
    let reader_output = PackWriter::new(workspace_id, pack_id, key, 2)
        .write_reader_streaming(&reader_records, &mut reader_bytes)
        .expect("reader-backed pack writes");

    assert_eq!(reader_output.locators, buffered.locators);
    assert_eq!(
        parse_index(&reader_bytes).expect("reader index parses"),
        parse_index(&buffered.bytes).expect("slice index parses")
    );
    for input in &inputs {
        assert_eq!(
            read_record_from_pack(&reader_bytes, &input.content_id, key, 2)
                .expect("reader record opens"),
            input.bytes
        );
    }
}

#[test]
fn reader_pack_writer_reopens_sources_for_retry_one_at_a_time() {
    let tracker = Arc::new(SourceOpenTracker::default());
    let inputs = [
        record([5_u8; 32], b"first reopenable source"),
        record([5_u8; 32], b"second reopenable source"),
    ];
    let sources = inputs
        .iter()
        .map(|input| TestContentSource::with_tracker(input.bytes.clone(), tracker.clone()))
        .collect::<Vec<_>>();
    let records = inputs
        .iter()
        .zip(&sources)
        .map(|(input, source)| PackRecordReader {
            content_id: &input.content_id,
            source,
        })
        .collect::<Vec<_>>();
    let writer = PackWriter::new(
        WorkspaceId::new("ws_reader_retry"),
        PackId::new("pk_2132435465768798"),
        StorageKey::deterministic(18),
        1,
    );
    writer
        .write_reader_streaming(&records, &mut Vec::new())
        .expect("first attempt writes");
    writer
        .write_reader_streaming(&records, &mut Vec::new())
        .expect("retry reopens sources");

    assert_eq!(tracker.open_count.load(Ordering::SeqCst), 4);
    assert_eq!(tracker.max_active.load(Ordering::SeqCst), 1);
    assert_eq!(tracker.active.load(Ordering::SeqCst), 0);
}

#[test]
fn reader_pack_writer_rejects_source_length_drift() {
    let source = TestContentSource::with_declared_len(b"too long".to_vec(), 3);
    let content_id = workspace_content_id([8_u8; 32], b"too long");
    let records = [PackRecordReader {
        content_id: &content_id,
        source: &source,
    }];
    let result = PackWriter::new(
        WorkspaceId::new("ws_reader_length"),
        PackId::new("pk_reader_length"),
        StorageKey::deterministic(20),
        1,
    )
    .write_reader_streaming(&records, &mut Vec::new());

    assert!(matches!(
        result,
        Err(PackfileError::ContentSourceLengthMismatch {
            expected: 3,
            actual: 4
        })
    ));
}

#[test]
fn reader_batching_uses_declared_lengths_without_opening_sources() {
    let inputs = [
        record([6_u8; 32], b"one"),
        record([6_u8; 32], b"two"),
        record([6_u8; 32], b"three"),
    ];
    let sources = inputs
        .iter()
        .map(|input| TestContentSource::new(input.bytes.clone()))
        .collect::<Vec<_>>();
    let records = inputs
        .iter()
        .zip(&sources)
        .map(|(input, source)| PackRecordReader {
            content_id: &input.content_id,
            source,
        })
        .collect::<Vec<_>>();
    let mut batch_sizes = Vec::new();

    write_source_pack_reader_batches_with(
        WorkspaceId::new("ws_reader_batches"),
        &records,
        5,
        StorageKey::deterministic(22),
        1,
        |_writer, batch| {
            batch_sizes.push(batch.len());
            Ok(())
        },
    )
    .expect("reader batches form");

    assert_eq!(batch_sizes, [1, 1, 1]);
    assert!(
        sources
            .iter()
            .all(|source| source.tracker.open_count.load(Ordering::SeqCst) == 0)
    );
}

#[test]
fn tiny_files_pack_into_fewer_objects_than_files() {
    let records = (0..200)
        .map(|index| record([2_u8; 32], format!("file {index}").as_bytes()))
        .collect::<Vec<_>>();
    let packs = write_source_packs(
        WorkspaceId::new("ws_tiny"),
        &records,
        512,
        StorageKey::deterministic(5),
        1,
    )
    .expect("packs write");

    assert!(packs.len() < records.len() / 10);
    assert_eq!(
        packs.iter().map(|pack| pack.locators.len()).sum::<usize>(),
        records.len()
    );
}

#[test]
fn source_pack_keys_are_opaque_and_unique_across_imports() {
    let records = (0..4)
        .map(|index| record([8_u8; 32], format!("file {index}").as_bytes()))
        .collect::<Vec<_>>();
    let key = StorageKey::deterministic(8);
    let workspace_id = WorkspaceId::new("ws_acme_web");

    let first =
        write_source_packs(workspace_id.clone(), &records, 512, key, 1).expect("first import");
    let second = write_source_packs(workspace_id, &records, 512, key, 1).expect("second import");

    assert_ne!(first[0].pack_id, second[0].pack_id);
    assert_ne!(first[0].object_key, second[0].object_key);
    for pack in first.iter().chain(second.iter()) {
        let key = pack.object_key.as_str();
        assert!(key.starts_with("packs_pk_"));
        for leaked in ["acme", "web", "main", "src", "package"] {
            assert!(!key.contains(leaked), "object key leaked {leaked}");
        }
    }
}

#[test]
fn pack_rejects_unknown_version_wrong_offset_and_corruption() {
    let key = StorageKey::deterministic(3);
    let records = [record([1_u8; 32], b"first file")];
    let output = PackWriter::new(
        WorkspaceId::new("ws_pack"),
        PackId::new("pk_8899aabbccddeeff"),
        key,
        1,
    )
    .write(&records)
    .expect("pack writes");

    let mut unknown_version = output.bytes.clone();
    unknown_version[PACK_MAGIC.len()] = 99;
    assert!(matches!(
        parse_index(&unknown_version),
        Err(PackfileError::UnsupportedVersion(_))
    ));

    let mut excessive_record_count = output.bytes.clone();
    excessive_record_count[PACK_MAGIC.len() + 2..PACK_MAGIC.len() + 6]
        .copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(matches!(
        parse_index(&excessive_record_count),
        Err(PackfileError::TruncatedPack)
    ));

    let mut wrong_offset = output.bytes.clone();
    let index = parse_index(&wrong_offset).expect("index parses");
    let first = index.records.values().next().expect("record");
    let offset_position =
        HEADER_FIXED_LEN + index.pack_id.as_str().len() + index.workspace_id_hash.len() + 2 + 8;
    wrong_offset[offset_position..offset_position + 8]
        .copy_from_slice(&(first.offset - 1).to_le_bytes());
    assert!(matches!(
        parse_index(&wrong_offset),
        Err(PackfileError::DirectoryIntegrity)
    ));

    let mut corrupted = output.bytes.clone();
    let last = corrupted.last_mut().expect("pack has bytes");
    *last ^= 1;
    let content_id = &records[0].content_id;
    assert!(matches!(
        read_record_from_pack(&corrupted, content_id, key, 1),
        Err(PackfileError::Envelope(EnvelopeError::VerificationFailed))
    ));

    assert!(matches!(
        parse_index(&output.bytes[..10]),
        Err(PackfileError::TruncatedPack)
    ));
}

#[test]
fn pack_reader_rejects_unsupported_format_versions() {
    let key = StorageKey::deterministic(3);
    let records = [record([1_u8; 32], b"current file")];
    let output = PackWriter::new(
        WorkspaceId::new("ws_pack"),
        PackId::new("pk_0011223344556677"),
        key,
        1,
    )
    .write(&records)
    .expect("pack writes");
    let mut unsupported = output.bytes;
    unsupported[PACK_MAGIC.len()..PACK_MAGIC.len() + 2].copy_from_slice(&1_u16.to_le_bytes());

    assert!(matches!(
        parse_index(&unsupported),
        Err(PackfileError::UnsupportedVersion(1))
    ));
}

#[test]
fn pack_rejects_record_ranges_inside_directory() {
    let key = StorageKey::deterministic(3);
    let records = [
        record([1_u8; 32], b"first file"),
        record([1_u8; 32], b"second file"),
    ];
    let output = PackWriter::new(
        WorkspaceId::new("ws_pack"),
        PackId::new("pk_0123456789abcdef"),
        key,
        1,
    )
    .write(&records)
    .expect("pack writes");
    let index = parse_index(&output.bytes).expect("index parses");
    let offset_position =
        HEADER_FIXED_LEN + index.pack_id.as_str().len() + index.workspace_id_hash.len() + 2 + 8;
    let directory_end = output
        .locators
        .iter()
        .filter_map(|locator| locator.offset)
        .min()
        .expect("pack has record offsets");
    let directory_middle = directory_end - 1;
    let mut corrupted = output.bytes.clone();
    corrupted[offset_position..offset_position + 8]
        .copy_from_slice(&directory_middle.to_le_bytes());
    rewrite_directory_digest(&mut corrupted, directory_end as usize);

    assert!(matches!(
        parse_index(&corrupted),
        Err(PackfileError::InvalidRecordRange)
    ));
}

fn rewrite_directory_digest(pack_bytes: &mut [u8], directory_end: usize) {
    let digest_start = PACK_MAGIC.len() + 2 + 4 + 2 + 2;
    let digest_end = digest_start + DIRECTORY_DIGEST_LEN;
    let digest = blake3::hash(&pack_bytes[HEADER_FIXED_LEN..directory_end]);
    pack_bytes[digest_start..digest_end].copy_from_slice(digest.as_bytes());
}

fn record(key: [u8; 32], bytes: &[u8]) -> PackRecordInput {
    PackRecordInput {
        content_id: workspace_content_id(key, bytes),
        bytes: bytes.to_vec(),
    }
}

fn deterministic_bytes(len: usize) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(len);
    let mut counter = 0_u64;
    while bytes.len() < len {
        let digest = blake3::hash(&counter.to_le_bytes());
        let remaining = len - bytes.len();
        bytes.extend_from_slice(&digest.as_bytes()[..remaining.min(digest.as_bytes().len())]);
        counter += 1;
    }
    bytes
}

#[derive(Default)]
struct ChunkTrackingWriter {
    bytes: Vec<u8>,
    max_write_len: usize,
}

#[derive(Default)]
struct SourceOpenTracker {
    active: AtomicUsize,
    max_active: AtomicUsize,
    open_count: AtomicUsize,
}

struct TestContentSource {
    bytes: Vec<u8>,
    declared_len: u64,
    tracker: Arc<SourceOpenTracker>,
}

impl TestContentSource {
    fn new(bytes: Vec<u8>) -> Self {
        Self::with_tracker(bytes, Arc::new(SourceOpenTracker::default()))
    }

    fn with_declared_len(bytes: Vec<u8>, declared_len: u64) -> Self {
        Self {
            bytes,
            declared_len,
            tracker: Arc::new(SourceOpenTracker::default()),
        }
    }

    fn with_tracker(bytes: Vec<u8>, tracker: Arc<SourceOpenTracker>) -> Self {
        Self {
            declared_len: bytes.len() as u64,
            bytes,
            tracker,
        }
    }
}

impl ContentSourceReader for TestContentSource {
    fn logical_len(&self) -> u64 {
        self.declared_len
    }

    fn open(&self) -> Result<Box<dyn Read + Send + '_>, PackfileError> {
        self.tracker.open_count.fetch_add(1, Ordering::SeqCst);
        let active = self.tracker.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.tracker.max_active.fetch_max(active, Ordering::SeqCst);
        Ok(Box::new(TrackedReader {
            cursor: Cursor::new(self.bytes.clone()),
            tracker: self.tracker.clone(),
        }))
    }

    fn open_range(
        &self,
        offset: u64,
        length: u64,
    ) -> Result<Box<dyn Read + Send + '_>, PackfileError> {
        self.tracker.open_count.fetch_add(1, Ordering::SeqCst);
        let active = self.tracker.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.tracker.max_active.fetch_max(active, Ordering::SeqCst);
        let start = usize::try_from(offset)
            .unwrap_or(self.bytes.len())
            .min(self.bytes.len());
        let requested_end = offset.saturating_add(length);
        let end = usize::try_from(requested_end)
            .unwrap_or(self.bytes.len())
            .min(self.bytes.len());
        Ok(Box::new(TrackedReader {
            cursor: Cursor::new(self.bytes[start..end].to_vec()),
            tracker: self.tracker.clone(),
        }))
    }
}

struct TrackedReader {
    cursor: Cursor<Vec<u8>>,
    tracker: Arc<SourceOpenTracker>,
}

impl Read for TrackedReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.cursor.read(buffer)
    }
}

impl Drop for TrackedReader {
    fn drop(&mut self) {
        self.tracker.active.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Write for ChunkTrackingWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.max_write_len = self.max_write_len.max(bytes.len());
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
