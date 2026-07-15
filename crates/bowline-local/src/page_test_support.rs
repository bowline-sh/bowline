use std::{fs, path::Path};

use crate::{
    metadata::{
        MetadataCacheRecord, MetadataCacheState, MetadataLogicalId, MetadataObjectBindingRecord,
        MetadataObjectKey, MetadataRecordKind, MetadataRecordRef, MetadataStore,
        MetadataVerificationState, SnapshotRecord,
    },
    sync::{SnapshotContent, namespace},
};

pub fn persist_cached_snapshot(
    store: &mut MetadataStore,
    snapshot: &SnapshotContent,
    cache_root: &Path,
    created_at: &str,
) {
    let workspace_id = &snapshot.manifest().workspace_id;
    store
        .register_metadata_identity_key(
            workspace_id,
            snapshot.namespace_store().identity_key().as_bytes(),
            created_at,
        )
        .expect("metadata identity key");
    let records = snapshot
        .namespace_store()
        .plaintext_records()
        .expect("metadata plaintext records");
    fs::create_dir_all(cache_root).expect("metadata cache root");
    for record in &records {
        let kind = local_metadata_kind(record.summary.kind);
        let logical_id = MetadataLogicalId::new(&record.summary.logical_id);
        let cache_path = cache_root.join(format!("{}.page", record.summary.logical_id));
        fs::write(&cache_path, &record.plaintext).expect("metadata cache write");
        store
            .insert_metadata_object_binding(&MetadataObjectBindingRecord {
                workspace_id: workspace_id.clone(),
                logical_id: logical_id.clone(),
                kind,
                object_key: MetadataObjectKey::new(format!(
                    "metadata_mp_{}",
                    blake3::hash(record.summary.logical_id.as_bytes()).to_hex()
                )),
                byte_len: record.plaintext.len() as u64,
                object_hash: blake3::hash(&record.plaintext).to_hex().to_string(),
                key_epoch: 1,
                verification_state: MetadataVerificationState::Verified,
                created_at: created_at.to_string(),
                verified_at: Some(created_at.to_string()),
            })
            .expect("metadata binding");
        store
            .put_metadata_cache_record(&MetadataCacheRecord {
                workspace_id: workspace_id.clone(),
                logical_id,
                kind,
                cache_path: Some(cache_path.display().to_string()),
                encoded_bytes: record.plaintext.len() as u64,
                state: MetadataCacheState::Present,
                last_accessed_at: created_at.to_string(),
            })
            .expect("metadata cache record");
    }
    for record in &records {
        let parent = MetadataRecordRef {
            kind: local_metadata_kind(record.summary.kind),
            logical_id: MetadataLogicalId::new(&record.summary.logical_id),
        };
        let children = record
            .summary
            .child_logical_ids
            .iter()
            .map(|child| {
                let child_record = records
                    .iter()
                    .find(|candidate| candidate.summary.logical_id == *child)
                    .expect("child metadata record");
                MetadataRecordRef {
                    kind: local_metadata_kind(child_record.summary.kind),
                    logical_id: MetadataLogicalId::new(child),
                }
            })
            .collect::<Vec<_>>();
        store
            .replace_metadata_record_edges(workspace_id, &parent, &children)
            .expect("metadata edges");
    }
    let manifest = snapshot.manifest();
    store
        .commit_snapshot_root(
            &SnapshotRecord {
                id: manifest.snapshot_id.clone(),
                workspace_id: manifest.workspace_id.clone(),
                project_id: manifest.project_id.clone(),
                kind: manifest.kind,
                base_snapshot_id: manifest.base_snapshot_id.clone(),
                root_id: manifest.namespace_root_id.clone(),
                semantic_manifest_digest: manifest.semantic_manifest_digest.clone(),
                entry_count: manifest.entry_count,
                refs: manifest.refs.clone(),
                created_at: created_at.to_string(),
            },
            &[],
            created_at,
        )
        .expect("snapshot root");
}

fn local_metadata_kind(kind: namespace::MetadataRecordKind) -> MetadataRecordKind {
    match kind {
        namespace::MetadataRecordKind::NamespacePage => MetadataRecordKind::NamespacePage,
        namespace::MetadataRecordKind::ContentLayout => MetadataRecordKind::ContentLayout,
        namespace::MetadataRecordKind::SegmentPage => MetadataRecordKind::SegmentPage,
    }
}
