use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::PathBuf,
};

use bowline_core::{
    ids::{ContentId, PackId, SnapshotId, WorkspaceId},
    workspace_graph::ContentLocator,
};
use serde::{Deserialize, Serialize};

use super::{ByteStoreError, LocalByteStore, ObjectKey};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourcePackUploadJournalKey {
    workspace_id: WorkspaceId,
    snapshot_id: SnapshotId,
    key_epoch: u32,
    content_set_digest: SourcePackUploadJournalDigest,
}

impl SourcePackUploadJournalKey {
    pub fn new(
        workspace_id: WorkspaceId,
        snapshot_id: SnapshotId,
        key_epoch: u32,
        content: impl IntoIterator<Item = (ContentId, u64)>,
    ) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(workspace_id.as_str().as_bytes());
        hasher.update(snapshot_id.as_str().as_bytes());
        hasher.update(&key_epoch.to_le_bytes());
        for (content_id, byte_len) in content {
            hasher.update(content_id.as_str().as_bytes());
            hasher.update(&byte_len.to_le_bytes());
        }
        Self {
            workspace_id,
            snapshot_id,
            key_epoch,
            content_set_digest: SourcePackUploadJournalDigest(format!(
                "b3_{}",
                hasher.finalize().to_hex()
            )),
        }
    }

    pub fn content_set_digest(&self) -> &str {
        self.content_set_digest.as_str()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SourcePackUploadJournalDigest(String);

impl SourcePackUploadJournalDigest {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SourcePackUploadJournalObjectHash(String);

impl SourcePackUploadJournalObjectHash {
    pub fn from_stable_hash(hash: String) -> Self {
        Self(hash)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourcePackUploadJournalEntry {
    pub pointer: SourcePackUploadJournalPointer,
    pub locators: Vec<ContentLocator>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourcePackUploadJournalPointer {
    pub object_key: ObjectKey,
    pub pack_id: PackId,
    pub byte_len: u64,
    pub hash: SourcePackUploadJournalObjectHash,
    pub key_epoch: u32,
    pub created_at_unix_ms: u64,
}

impl LocalByteStore {
    pub(super) fn source_pack_upload_journal_entries(
        &self,
        key: &SourcePackUploadJournalKey,
    ) -> Result<Vec<SourcePackUploadJournalEntry>, ByteStoreError> {
        Ok(self.read_upload_journal(key)?.unwrap_or_default())
    }

    pub(super) fn record_source_pack_upload_journal_entry(
        &self,
        key: &SourcePackUploadJournalKey,
        entry: &SourcePackUploadJournalEntry,
    ) -> Result<(), ByteStoreError> {
        self.append_upload_journal_entry(key, entry)
    }

    fn upload_journal_path(&self, key: &SourcePackUploadJournalKey) -> PathBuf {
        upload_journal_dir(&self.root).join(format!("{}.json", key.content_set_digest()))
    }

    fn read_upload_journal(
        &self,
        key: &SourcePackUploadJournalKey,
    ) -> Result<Option<Vec<SourcePackUploadJournalEntry>>, ByteStoreError> {
        let path = self.upload_journal_path(key);
        match fs::read_to_string(&path) {
            Ok(contents) => {
                let mut entries_by_key = BTreeMap::<ObjectKey, SourcePackUploadJournalEntry>::new();
                let lines = contents
                    .lines()
                    .filter(|line| !line.trim().is_empty())
                    .collect::<Vec<_>>();
                for (index, line) in lines.iter().enumerate() {
                    let entry = match serde_json::from_str::<SourcePackUploadJournalEntry>(line) {
                        Ok(entry) => entry,
                        Err(_) if index == lines.len() - 1 && !contents.ends_with('\n') => break,
                        Err(_) => {
                            return Err(ByteStoreError::CorruptJournal {
                                component: "source pack upload journal",
                                reason: "journal JSON line did not parse",
                            });
                        }
                    };
                    entries_by_key.insert(entry.pointer.object_key.clone(), entry);
                }
                Ok(Some(entries_by_key.into_values().collect()))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(ByteStoreError::Io(error)),
        }
    }

    fn append_upload_journal_entry(
        &self,
        key: &SourcePackUploadJournalKey,
        entry: &SourcePackUploadJournalEntry,
    ) -> Result<(), ByteStoreError> {
        let path = self.upload_journal_path(key);
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .truncate(false)
            .write(true)
            .open(path)
            .map_err(ByteStoreError::Io)?;
        repair_torn_journal_tail(&mut file)?;
        file.seek(SeekFrom::End(0)).map_err(ByteStoreError::Io)?;
        let mut bytes =
            serde_json::to_vec(entry).expect("source pack upload journal entry serializes");
        bytes.push(b'\n');
        file.write_all(&bytes).map_err(ByteStoreError::Io)?;
        file.sync_all().map_err(ByteStoreError::Io)?;
        Ok(())
    }
}

fn repair_torn_journal_tail(file: &mut fs::File) -> Result<(), ByteStoreError> {
    let byte_len = file.metadata().map_err(ByteStoreError::Io)?.len();
    if byte_len == 0 {
        return Ok(());
    }
    let mut bytes = Vec::with_capacity(usize::try_from(byte_len).unwrap_or_default());
    file.seek(SeekFrom::Start(0)).map_err(ByteStoreError::Io)?;
    file.read_to_end(&mut bytes).map_err(ByteStoreError::Io)?;
    if bytes.ends_with(b"\n") {
        return Ok(());
    }
    let repaired_len = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map(|index| index + 1)
        .unwrap_or_default();
    file.set_len(repaired_len as u64)
        .map_err(ByteStoreError::Io)?;
    file.sync_all().map_err(ByteStoreError::Io)?;
    Ok(())
}

pub(super) fn upload_journal_dir(root: &std::path::Path) -> PathBuf {
    root.join("upload-journal")
}
