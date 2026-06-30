use std::{
    collections::BTreeMap,
    error::Error,
    fmt,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use bowline_core::{
    ids::{ContentId, ManifestId, PackId, SnapshotId, WorkspaceId},
    workspace_graph::{ContentLocator, ContentStorage},
};

use crate::{
    ObjectKey, ObjectKind,
    envelope::{
        EnvelopeContext, EnvelopeError, EnvelopeNonceTracker, SealedEnvelope, StorageKey, open,
        seal, seal_tracked, workspace_id_hash,
    },
    manifest::{
        IndexPackPointer, LocatorIndexBinding, LocatorIndexPointer, SealedIndexPack,
        SealedLocatorIndex,
    },
    store::stable_object_hash,
};

const PACK_MAGIC: &[u8; 8] = b"bowpk1\0\0";
const PACK_VERSION: u16 = 2;
const INDEX_PACK_FORMAT_VERSION: u16 = 1;
const LOCATOR_INDEX_FORMAT_VERSION: u16 = 1;
const DIRECTORY_DIGEST_LEN: usize = 32;
const HEADER_FIXED_LEN: usize = PACK_MAGIC.len() + 2 + 4 + 2 + 2 + DIRECTORY_DIGEST_LEN;
const ENTRY_FIXED_LEN: usize = 2 + 8 + 8 + 8;

static NEXT_PACK_BATCH_SEED: AtomicU64 = AtomicU64::new(1);
static NEXT_INDEX_PACK_SEED: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackRecordInput {
    pub content_id: ContentId,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackRecordIndexEntry {
    pub content_id: ContentId,
    pub raw_size: u64,
    pub offset: u64,
    pub length: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackWriteOutput {
    pub pack_id: PackId,
    pub object_key: ObjectKey,
    pub bytes: Vec<u8>,
    pub locators: Vec<ContentLocator>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackIndex {
    pub pack_id: PackId,
    pub workspace_id_hash: String,
    pub records: BTreeMap<ContentId, PackRecordIndexEntry>,
}

#[derive(Debug, Clone)]
pub(crate) struct PackWriter {
    workspace_id: WorkspaceId,
    pack_id: PackId,
    key: StorageKey,
    key_epoch: u32,
}

impl PackWriter {
    pub fn new(
        workspace_id: WorkspaceId,
        pack_id: PackId,
        key: StorageKey,
        key_epoch: u32,
    ) -> Self {
        Self {
            workspace_id,
            pack_id,
            key,
            key_epoch,
        }
    }

    pub fn write(&self, records: &[PackRecordInput]) -> Result<PackWriteOutput, PackfileError> {
        if records.is_empty() {
            return Err(PackfileError::EmptyPack);
        }

        let workspace_hash = workspace_id_hash(self.workspace_id.as_str());
        let mut nonce_tracker = EnvelopeNonceTracker::new();
        let mut sealed_records = Vec::with_capacity(records.len());
        for record in records {
            let context = record_context(
                &workspace_hash,
                &self.pack_id,
                &record.content_id,
                self.key_epoch,
                PACK_VERSION,
            );
            let sealed = seal_tracked(&record.bytes, self.key, &context, &mut nonce_tracker)?;
            sealed_records.push(PreparedRecord {
                content_id: record.content_id.clone(),
                raw_size: record.bytes.len() as u64,
                sealed,
            });
        }

        let directory_len = directory_len(&self.pack_id, &workspace_hash, &sealed_records)?;
        let mut cursor = directory_len as u64;
        let indexed_records = sealed_records
            .iter()
            .map(|record| {
                let entry = PackRecordIndexEntry {
                    content_id: record.content_id.clone(),
                    raw_size: record.raw_size,
                    offset: cursor,
                    length: record.sealed.as_bytes().len() as u64,
                };
                cursor += entry.length;
                entry
            })
            .collect::<Vec<_>>();

        let mut bytes = Vec::with_capacity(cursor as usize);
        write_header_and_directory(&mut bytes, &self.pack_id, &workspace_hash, &indexed_records)?;
        for record in &sealed_records {
            bytes.extend_from_slice(record.sealed.as_bytes());
        }

        let locators = indexed_records
            .iter()
            .map(|record| locator_from_record(&self.pack_id, record))
            .collect::<Vec<_>>();

        Ok(PackWriteOutput {
            pack_id: self.pack_id.clone(),
            object_key: ObjectKey::from_pack_id(&self.pack_id)?,
            bytes,
            locators,
        })
    }
}

pub fn write_source_packs(
    workspace_id: WorkspaceId,
    records: &[PackRecordInput],
    target_raw_pack_size: usize,
    key: StorageKey,
    key_epoch: u32,
) -> Result<Vec<PackWriteOutput>, PackfileError> {
    let mut packs = Vec::new();
    write_source_packs_with(
        workspace_id,
        records,
        target_raw_pack_size,
        key,
        key_epoch,
        |pack| {
            packs.push(pack);
            Ok(())
        },
    )?;
    Ok(packs)
}

pub fn write_source_packs_with(
    workspace_id: WorkspaceId,
    records: &[PackRecordInput],
    target_raw_pack_size: usize,
    key: StorageKey,
    key_epoch: u32,
    mut on_pack: impl FnMut(PackWriteOutput) -> Result<(), PackfileError>,
) -> Result<(), PackfileError> {
    if records.is_empty() {
        return Ok(());
    }

    let target_raw_pack_size = target_raw_pack_size.max(1);
    let batch_seed = new_pack_batch_seed(&workspace_id, records, target_raw_pack_size, key_epoch);
    let mut batch_start = 0_usize;
    let mut batch_raw_size = 0_usize;
    let mut sequence = 1_usize;

    for (index, record) in records.iter().enumerate() {
        if index > batch_start && batch_raw_size + record.bytes.len() > target_raw_pack_size {
            on_pack(write_numbered_pack(
                workspace_id.clone(),
                &batch_seed,
                sequence,
                &records[batch_start..index],
                key,
                key_epoch,
            )?)?;
            sequence += 1;
            batch_start = index;
            batch_raw_size = 0;
        }
        batch_raw_size += record.bytes.len();
    }

    if batch_start < records.len() {
        on_pack(write_numbered_pack(
            workspace_id,
            &batch_seed,
            sequence,
            &records[batch_start..],
            key,
            key_epoch,
        )?)?;
    }

    Ok(())
}

pub fn seal_index_pack(
    workspace_id: WorkspaceId,
    snapshot_id: SnapshotId,
    plaintext: &[u8],
    key: StorageKey,
    key_epoch: u32,
) -> Result<SealedIndexPack, PackfileError> {
    let index_pack_id = opaque_index_pack_id(&workspace_id, &snapshot_id, plaintext, key_epoch);
    let context = index_pack_context(&workspace_id, &snapshot_id, &index_pack_id, key_epoch);
    let bytes = seal(plaintext, key, &context)?.into_bytes();
    let pointer = IndexPackPointer {
        object_key: ObjectKey::from_index_pack_id(&index_pack_id)?,
        index_pack_id,
        snapshot_id,
        byte_len: bytes.len() as u64,
        hash: stable_object_hash(&bytes),
        key_epoch,
    };
    Ok(SealedIndexPack { pointer, bytes })
}

pub fn open_index_pack(
    sealed: &SealedIndexPack,
    key: StorageKey,
    workspace_id: &WorkspaceId,
) -> Result<Vec<u8>, PackfileError> {
    if sealed.pointer.byte_len != sealed.bytes.len() as u64 {
        return Err(PackfileError::PointerIntegrity("byte_len"));
    }
    if sealed.pointer.hash != stable_object_hash(&sealed.bytes) {
        return Err(PackfileError::PointerIntegrity("hash"));
    }

    let context = index_pack_context(
        workspace_id,
        &sealed.pointer.snapshot_id,
        &sealed.pointer.index_pack_id,
        sealed.pointer.key_epoch,
    );
    open(&sealed.bytes, key, &context).map_err(Into::into)
}

pub fn seal_locator_index(
    workspace_id: WorkspaceId,
    manifest_id: ManifestId,
    snapshot_id: SnapshotId,
    plaintext: &[u8],
    key: StorageKey,
    key_epoch: u32,
) -> Result<SealedLocatorIndex, PackfileError> {
    let locator_table_digest = stable_object_hash(plaintext);
    let locator_index_id = opaque_locator_index_id(
        &workspace_id,
        &manifest_id,
        &snapshot_id,
        &locator_table_digest,
        key_epoch,
    );
    let context = locator_index_context(
        &workspace_id,
        &manifest_id,
        &snapshot_id,
        &locator_index_id,
        &locator_table_digest,
        key_epoch,
    );
    let bytes = seal(plaintext, key, &context)?.into_bytes();
    let pointer = LocatorIndexPointer {
        object_key: ObjectKey::from_index_pack_id(&locator_index_id)?,
        locator_index_id,
        manifest_id,
        snapshot_id,
        byte_len: bytes.len() as u64,
        hash: stable_object_hash(&bytes),
        key_epoch,
        format_version: LOCATOR_INDEX_FORMAT_VERSION,
        locator_table_digest,
    };
    Ok(SealedLocatorIndex { pointer, bytes })
}

pub fn open_locator_index(
    sealed: &SealedLocatorIndex,
    key: StorageKey,
    workspace_id: &WorkspaceId,
    expected_binding: &LocatorIndexBinding,
) -> Result<Vec<u8>, PackfileError> {
    if sealed.pointer.byte_len != sealed.bytes.len() as u64 {
        return Err(PackfileError::PointerIntegrity("byte_len"));
    }
    if sealed.pointer.hash != stable_object_hash(&sealed.bytes) {
        return Err(PackfileError::PointerIntegrity("hash"));
    }
    if sealed.pointer.format_version != LOCATOR_INDEX_FORMAT_VERSION {
        return Err(PackfileError::UnsupportedVersion(
            sealed.pointer.format_version,
        ));
    }
    if sealed.pointer.binding() != *expected_binding {
        return Err(PackfileError::PointerIntegrity("locator_binding"));
    }

    let context = locator_index_context(
        workspace_id,
        &sealed.pointer.manifest_id,
        &sealed.pointer.snapshot_id,
        &sealed.pointer.locator_index_id,
        &sealed.pointer.locator_table_digest,
        sealed.pointer.key_epoch,
    );
    let plaintext = open(&sealed.bytes, key, &context)?;
    if stable_object_hash(&plaintext) != sealed.pointer.locator_table_digest {
        return Err(PackfileError::PointerIntegrity("locator_table_digest"));
    }
    Ok(plaintext)
}

pub fn parse_index(pack_bytes: &[u8]) -> Result<PackIndex, PackfileError> {
    if pack_bytes.len() < HEADER_FIXED_LEN {
        return Err(PackfileError::TruncatedPack);
    }
    if &pack_bytes[..PACK_MAGIC.len()] != PACK_MAGIC {
        return Err(PackfileError::UnknownFormat);
    }

    let version = read_u16(pack_bytes, PACK_MAGIC.len())?;
    if version != PACK_VERSION {
        return Err(PackfileError::UnsupportedVersion(version));
    }
    let record_count = read_u32(pack_bytes, PACK_MAGIC.len() + 2)? as usize;
    let pack_id_len = read_u16(pack_bytes, PACK_MAGIC.len() + 2 + 4)? as usize;
    let workspace_hash_len = read_u16(pack_bytes, PACK_MAGIC.len() + 2 + 4 + 2)? as usize;
    let expected_digest = read_directory_digest(pack_bytes)?;
    validate_directory_capacity(
        pack_bytes.len(),
        record_count,
        pack_id_len,
        workspace_hash_len,
    )?;

    let mut cursor = HEADER_FIXED_LEN;
    let pack_id = read_string(pack_bytes, &mut cursor, pack_id_len)?;
    let workspace_hash = read_string(pack_bytes, &mut cursor, workspace_hash_len)?;
    let mut entries = Vec::with_capacity(record_count);
    for _ in 0..record_count {
        let content_id_len = read_u16(pack_bytes, cursor)? as usize;
        cursor += 2;
        let raw_size = read_u64(pack_bytes, cursor)?;
        cursor += 8;
        let offset = read_u64(pack_bytes, cursor)?;
        cursor += 8;
        let length = read_u64(pack_bytes, cursor)?;
        cursor += 8;
        let content_id = ContentId::new(read_string(pack_bytes, &mut cursor, content_id_len)?);

        entries.push(PackRecordIndexEntry {
            content_id: content_id.clone(),
            raw_size,
            offset,
            length,
        });
    }

    let actual_digest = blake3::hash(&pack_bytes[HEADER_FIXED_LEN..cursor]);
    if expected_digest != *actual_digest.as_bytes() {
        return Err(PackfileError::DirectoryIntegrity);
    }

    validate_record_ranges(pack_bytes.len(), cursor as u64, &entries)?;

    let mut records = BTreeMap::new();
    for entry in entries {
        let content_id = entry.content_id.clone();
        if records.insert(content_id, entry).is_some() {
            return Err(PackfileError::DuplicateContentId);
        }
    }
    Ok(PackIndex {
        pack_id: PackId::new(pack_id),
        workspace_id_hash: workspace_hash,
        records,
    })
}

#[cfg(test)]
pub(crate) fn read_record_from_pack(
    pack_bytes: &[u8],
    content_id: &ContentId,
    key: StorageKey,
    key_epoch: u32,
) -> Result<Vec<u8>, PackfileError> {
    let index = parse_index(pack_bytes)?;
    let record = index
        .records
        .get(content_id)
        .ok_or(PackfileError::MissingRecord)?;
    let encrypted = slice_record(pack_bytes, record)?;
    read_record_range(
        encrypted,
        &index.workspace_id_hash,
        &index.pack_id,
        content_id,
        key,
        key_epoch,
    )
}

pub(crate) fn read_record_range(
    encrypted_record: &[u8],
    workspace_hash: &str,
    pack_id: &PackId,
    content_id: &ContentId,
    key: StorageKey,
    key_epoch: u32,
) -> Result<Vec<u8>, PackfileError> {
    let context = record_context(workspace_hash, pack_id, content_id, key_epoch, PACK_VERSION);
    open(encrypted_record, key, &context).map_err(Into::into)
}

fn write_numbered_pack(
    workspace_id: WorkspaceId,
    batch_seed: &str,
    sequence: usize,
    records: &[PackRecordInput],
    key: StorageKey,
    key_epoch: u32,
) -> Result<PackWriteOutput, PackfileError> {
    let writer = PackWriter::new(
        workspace_id,
        opaque_pack_id(batch_seed, sequence),
        key,
        key_epoch,
    );
    writer.write(records)
}

fn new_pack_batch_seed(
    workspace_id: &WorkspaceId,
    records: &[PackRecordInput],
    target_raw_pack_size: usize,
    key_epoch: u32,
) -> String {
    let sequence = NEXT_PACK_BATCH_SEED.fetch_add(1, Ordering::Relaxed);
    let now_nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let mut hasher = blake3::Hasher::new();
    hasher.update(workspace_id.as_str().as_bytes());
    hasher.update(&sequence.to_le_bytes());
    hasher.update(&now_nanos.to_le_bytes());
    hasher.update(&std::process::id().to_le_bytes());
    hasher.update(&target_raw_pack_size.to_le_bytes());
    hasher.update(&key_epoch.to_le_bytes());
    for record in records {
        hasher.update(record.content_id.as_str().as_bytes());
        hasher.update(&record.bytes.len().to_le_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

fn opaque_pack_id(batch_seed: &str, sequence: usize) -> PackId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(batch_seed.as_bytes());
    hasher.update(&sequence.to_le_bytes());
    PackId::new(format!("pk_{}", hasher.finalize().to_hex()))
}

fn opaque_index_pack_id(
    workspace_id: &WorkspaceId,
    snapshot_id: &SnapshotId,
    plaintext: &[u8],
    key_epoch: u32,
) -> String {
    let sequence = NEXT_INDEX_PACK_SEED.fetch_add(1, Ordering::Relaxed);
    let now_nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let mut hasher = blake3::Hasher::new();
    hasher.update(workspace_id.as_str().as_bytes());
    hasher.update(snapshot_id.as_str().as_bytes());
    hasher.update(&sequence.to_le_bytes());
    hasher.update(&now_nanos.to_le_bytes());
    hasher.update(&std::process::id().to_le_bytes());
    hasher.update(&key_epoch.to_le_bytes());
    hasher.update(&(plaintext.len() as u64).to_le_bytes());
    hasher.update(blake3::hash(plaintext).as_bytes());
    format!("ix_{}", hasher.finalize().to_hex())
}

fn opaque_locator_index_id(
    workspace_id: &WorkspaceId,
    manifest_id: &ManifestId,
    snapshot_id: &SnapshotId,
    locator_table_digest: &str,
    key_epoch: u32,
) -> String {
    let sequence = NEXT_INDEX_PACK_SEED.fetch_add(1, Ordering::Relaxed);
    let now_nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let mut hasher = blake3::Hasher::new();
    hasher.update(workspace_id.as_str().as_bytes());
    hasher.update(manifest_id.as_str().as_bytes());
    hasher.update(snapshot_id.as_str().as_bytes());
    hasher.update(locator_table_digest.as_bytes());
    hasher.update(&sequence.to_le_bytes());
    hasher.update(&now_nanos.to_le_bytes());
    hasher.update(&std::process::id().to_le_bytes());
    hasher.update(&key_epoch.to_le_bytes());
    format!("ix_{}", hasher.finalize().to_hex())
}

fn locator_from_record(pack_id: &PackId, record: &PackRecordIndexEntry) -> ContentLocator {
    ContentLocator {
        content_id: record.content_id.clone(),
        storage: ContentStorage::Packed,
        raw_size: record.raw_size,
        pack_id: Some(pack_id.clone()),
        offset: Some(record.offset),
        length: Some(record.length),
        chunk_ids: Vec::new(),
    }
}

fn validate_record_ranges(
    pack_len: usize,
    directory_end: u64,
    records: &[PackRecordIndexEntry],
) -> Result<(), PackfileError> {
    let mut ranges = records
        .iter()
        .map(|record| {
            let end = record
                .offset
                .checked_add(record.length)
                .ok_or(PackfileError::InvalidRecordRange)?;
            if record.length == 0 || record.offset < directory_end || end > pack_len as u64 {
                return Err(PackfileError::InvalidRecordRange);
            }
            Ok((record.offset, end))
        })
        .collect::<Result<Vec<_>, PackfileError>>()?;
    ranges.sort_unstable();

    let mut previous_end = directory_end;
    for (start, end) in ranges {
        if start < previous_end {
            return Err(PackfileError::InvalidRecordRange);
        }
        previous_end = end;
    }

    Ok(())
}

fn validate_directory_capacity(
    pack_len: usize,
    record_count: usize,
    pack_id_len: usize,
    workspace_hash_len: usize,
) -> Result<(), PackfileError> {
    let fixed_directory_len = HEADER_FIXED_LEN
        .checked_add(pack_id_len)
        .and_then(|len| len.checked_add(workspace_hash_len))
        .ok_or(PackfileError::TruncatedPack)?;
    if fixed_directory_len > pack_len {
        return Err(PackfileError::TruncatedPack);
    }
    let max_record_count = (pack_len - fixed_directory_len) / ENTRY_FIXED_LEN;
    if record_count > max_record_count {
        return Err(PackfileError::TruncatedPack);
    }
    Ok(())
}

fn record_context(
    workspace_hash: &str,
    pack_id: &PackId,
    content_id: &ContentId,
    key_epoch: u32,
    format_version: u16,
) -> EnvelopeContext {
    EnvelopeContext {
        workspace_id_hash: workspace_hash.to_string(),
        object_kind: ObjectKind::SourcePack,
        object_id: pack_id.as_str().to_string(),
        record_id: content_id.as_str().to_string(),
        key_epoch,
        format_version,
    }
}

fn index_pack_context(
    workspace_id: &WorkspaceId,
    snapshot_id: &SnapshotId,
    index_pack_id: &str,
    key_epoch: u32,
) -> EnvelopeContext {
    EnvelopeContext {
        workspace_id_hash: workspace_id_hash(workspace_id.as_str()),
        object_kind: ObjectKind::IndexPack,
        object_id: index_pack_id.to_string(),
        record_id: snapshot_id.as_str().to_string(),
        key_epoch,
        format_version: INDEX_PACK_FORMAT_VERSION,
    }
}

fn locator_index_context(
    workspace_id: &WorkspaceId,
    manifest_id: &ManifestId,
    snapshot_id: &SnapshotId,
    locator_index_id: &str,
    locator_table_digest: &str,
    key_epoch: u32,
) -> EnvelopeContext {
    EnvelopeContext {
        workspace_id_hash: workspace_id_hash(workspace_id.as_str()),
        object_kind: ObjectKind::LocatorIndex,
        object_id: locator_index_id.to_string(),
        record_id: format!(
            "{}:{}:{}",
            snapshot_id.as_str(),
            manifest_id.as_str(),
            locator_table_digest
        ),
        key_epoch,
        format_version: LOCATOR_INDEX_FORMAT_VERSION,
    }
}

fn directory_len(
    pack_id: &PackId,
    workspace_hash: &str,
    records: &[PreparedRecord],
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

fn write_header_and_directory(
    bytes: &mut Vec<u8>,
    pack_id: &PackId,
    workspace_hash: &str,
    records: &[PackRecordIndexEntry],
) -> Result<(), PackfileError> {
    let mut directory = Vec::new();
    directory.extend_from_slice(pack_id.as_str().as_bytes());
    directory.extend_from_slice(workspace_hash.as_bytes());
    for record in records {
        write_len(&mut directory, record.content_id.as_str().len())?;
        directory.extend_from_slice(&record.raw_size.to_le_bytes());
        directory.extend_from_slice(&record.offset.to_le_bytes());
        directory.extend_from_slice(&record.length.to_le_bytes());
        directory.extend_from_slice(record.content_id.as_str().as_bytes());
    }

    bytes.extend_from_slice(PACK_MAGIC);
    bytes.extend_from_slice(&PACK_VERSION.to_le_bytes());
    bytes.extend_from_slice(&(records.len() as u32).to_le_bytes());
    write_len(bytes, pack_id.as_str().len())?;
    write_len(bytes, workspace_hash.len())?;
    bytes.extend_from_slice(blake3::hash(&directory).as_bytes());
    bytes.extend_from_slice(&directory);
    Ok(())
}

fn write_len(bytes: &mut Vec<u8>, len: usize) -> Result<(), PackfileError> {
    let len = u16::try_from(len).map_err(|_| PackfileError::FieldTooLong)?;
    bytes.extend_from_slice(&len.to_le_bytes());
    Ok(())
}

#[cfg(test)]
fn slice_record<'a>(
    pack_bytes: &'a [u8],
    record: &PackRecordIndexEntry,
) -> Result<&'a [u8], PackfileError> {
    let start = usize::try_from(record.offset).map_err(|_| PackfileError::InvalidRecordRange)?;
    let length = usize::try_from(record.length).map_err(|_| PackfileError::InvalidRecordRange)?;
    let end = start
        .checked_add(length)
        .ok_or(PackfileError::InvalidRecordRange)?;
    pack_bytes
        .get(start..end)
        .ok_or(PackfileError::InvalidRecordRange)
}

fn read_string(bytes: &[u8], cursor: &mut usize, len: usize) -> Result<String, PackfileError> {
    let end = cursor
        .checked_add(len)
        .ok_or(PackfileError::TruncatedPack)?;
    let selected = bytes
        .get(*cursor..end)
        .ok_or(PackfileError::TruncatedPack)?;
    *cursor = end;
    String::from_utf8(selected.to_vec()).map_err(|_| PackfileError::InvalidUtf8)
}

fn read_directory_digest(pack_bytes: &[u8]) -> Result<[u8; DIRECTORY_DIGEST_LEN], PackfileError> {
    let start = PACK_MAGIC.len() + 2 + 4 + 2 + 2;
    let end = start + DIRECTORY_DIGEST_LEN;
    let digest = pack_bytes
        .get(start..end)
        .ok_or(PackfileError::TruncatedPack)?;
    Ok(digest
        .try_into()
        .expect("directory digest slice has fixed length"))
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, PackfileError> {
    let selected = bytes
        .get(offset..offset + 2)
        .ok_or(PackfileError::TruncatedPack)?;
    Ok(u16::from_le_bytes([selected[0], selected[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, PackfileError> {
    let selected = bytes
        .get(offset..offset + 4)
        .ok_or(PackfileError::TruncatedPack)?;
    Ok(u32::from_le_bytes([
        selected[0],
        selected[1],
        selected[2],
        selected[3],
    ]))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, PackfileError> {
    let selected = bytes
        .get(offset..offset + 8)
        .ok_or(PackfileError::TruncatedPack)?;
    Ok(u64::from_le_bytes([
        selected[0],
        selected[1],
        selected[2],
        selected[3],
        selected[4],
        selected[5],
        selected[6],
        selected[7],
    ]))
}

#[derive(Debug)]
struct PreparedRecord {
    content_id: ContentId,
    raw_size: u64,
    sealed: SealedEnvelope,
}

#[derive(Debug)]
pub enum PackfileError {
    EmptyPack,
    PackTooLarge,
    FieldTooLong,
    UnknownFormat,
    UnsupportedVersion(u16),
    TruncatedPack,
    InvalidUtf8,
    InvalidRecordRange,
    DirectoryIntegrity,
    DuplicateContentId,
    MissingRecord,
    PointerIntegrity(&'static str),
    Envelope(EnvelopeError),
    ObjectKey(crate::ByteStoreError),
}

impl fmt::Display for PackfileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyPack => formatter.write_str("pack must contain at least one record"),
            Self::PackTooLarge => formatter.write_str("pack is too large"),
            Self::FieldTooLong => formatter.write_str("pack field is too long"),
            Self::UnknownFormat => formatter.write_str("packfile has unknown format"),
            Self::UnsupportedVersion(version) => {
                write!(formatter, "packfile version {version} is unsupported")
            }
            Self::TruncatedPack => formatter.write_str("packfile is truncated"),
            Self::InvalidUtf8 => formatter.write_str("packfile contains invalid UTF-8 metadata"),
            Self::InvalidRecordRange => formatter.write_str("packfile record range is invalid"),
            Self::DirectoryIntegrity => {
                formatter.write_str("packfile directory digest did not match")
            }
            Self::DuplicateContentId => formatter.write_str("packfile has duplicate content ID"),
            Self::MissingRecord => formatter.write_str("packfile record is missing"),
            Self::PointerIntegrity(field) => {
                write!(
                    formatter,
                    "index-pack pointer {field} did not match sealed bytes"
                )
            }
            Self::Envelope(error) => write!(formatter, "packfile record envelope failed: {error}"),
            Self::ObjectKey(error) => write!(formatter, "packfile object key failed: {error}"),
        }
    }
}

impl Error for PackfileError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Envelope(error) => Some(error),
            Self::ObjectKey(error) => Some(error),
            _ => None,
        }
    }
}

impl From<EnvelopeError> for PackfileError {
    fn from(error: EnvelopeError) -> Self {
        Self::Envelope(error)
    }
}

impl From<crate::ByteStoreError> for PackfileError {
    fn from(error: crate::ByteStoreError) -> Self {
        Self::ObjectKey(error)
    }
}

#[cfg(test)]
mod tests {
    use bowline_core::workspace_graph::workspace_content_id;

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
                read_record_from_pack(&first.bytes, &record.content_id, key, 1)
                    .expect("first opens"),
                record.bytes
            );
            assert_eq!(
                read_record_from_pack(&retry.bytes, &record.content_id, key, 1)
                    .expect("retry opens"),
                record.bytes
            );
        }
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
        let second =
            write_source_packs(workspace_id, &records, 512, key, 1).expect("second import");

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
}
