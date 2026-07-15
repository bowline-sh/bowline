//! Checkpoint payload shapes emitted while uploading a snapshot. Field order
//! is the wire order: serde serializes struct fields as declared, and these
//! payloads are compared byte-for-byte in sync tests.

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SnapshotManifestPayload<'a> {
    pub(super) snapshot_id: &'a str,
    pub(super) manifest_id: &'a str,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SourcePacksWrittenPayload<'a> {
    pub(super) snapshot_id: &'a str,
    pub(super) preparation_lease_id: Option<&'a str>,
    pub(super) pack_count: usize,
    pub(super) record_count: usize,
    pub(super) reused_record_count: usize,
    pub(super) reused_pack_count: usize,
    pub(super) resident_content_bytes: u64,
    pub(super) prepared_content_bytes: u64,
    pub(super) staged_content_bytes: u64,
    pub(super) largest_content_bytes: u64,
    pub(super) packed_input_bytes: u64,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ObjectContentPayload<'a> {
    pub(super) object_key: &'a str,
    pub(super) content_id: &'a str,
    pub(super) byte_len: u64,
    pub(super) hash: &'a str,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SnapshotRootCommittedPayload<'a> {
    pub(super) snapshot_id: &'a str,
    pub(super) manifest_id: &'a str,
    pub(super) metadata_record_count: usize,
    pub(super) metadata_records_resolved: usize,
    pub(super) metadata_records_fetched: usize,
    pub(super) metadata_records_uploaded: usize,
    pub(super) metadata_plaintext_bytes_uploaded: u64,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct WorkspaceRefAdvancedPayload<'a> {
    pub(super) snapshot_id: &'a str,
    pub(super) version: u64,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct WorkspaceRefStalePayload<'a> {
    pub(super) attempted_snapshot_id: &'a str,
    pub(super) current_snapshot_id: &'a str,
    pub(super) current_version: u64,
}
