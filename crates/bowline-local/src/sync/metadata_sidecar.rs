use crate::sync::namespace::MetadataRecordSummary;
use bowline_core::ids::PackId;
use bowline_storage::{ByteStoreError, ObjectKey};

pub(crate) fn metadata_direct_object_keys(
    summary: &MetadataRecordSummary,
) -> Result<Vec<String>, ByteStoreError> {
    summary
        .direct_pack_ids
        .iter()
        .map(|pack_id| ObjectKey::from_pack_id(&PackId::new(pack_id)))
        .collect::<Result<Vec<_>, _>>()
        .map(|keys| {
            keys.into_iter()
                .map(|key| key.as_str().to_string())
                .collect()
        })
}

pub(crate) fn metadata_sidecar_digest(
    summary: &MetadataRecordSummary,
    direct_object_keys: &[String],
) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"bowline metadata sidecar v2\0");
    hash_field(&mut hasher, summary.kind.as_str().as_bytes());
    hash_field(&mut hasher, summary.logical_id.as_bytes());
    hash_sequence(&mut hasher, &summary.child_logical_ids);
    hash_sequence(&mut hasher, direct_object_keys);
    format!("b3_{}", hasher.finalize().to_hex())
}

fn hash_sequence(hasher: &mut blake3::Hasher, values: &[String]) {
    hasher.update(&(values.len() as u64).to_be_bytes());
    for value in values {
        hash_field(hasher, value.as_bytes());
    }
}

fn hash_field(hasher: &mut blake3::Hasher, value: &[u8]) {
    hasher.update(&(value.len() as u64).to_be_bytes());
    hasher.update(value);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::namespace::MetadataRecordKind;

    fn summary(children: &[&str]) -> MetadataRecordSummary {
        MetadataRecordSummary {
            logical_id: "nsp_0000000000000000000000000000000000000000000000000000000000000000"
                .to_string(),
            kind: MetadataRecordKind::NamespacePage,
            encoded_bytes: 1,
            child_logical_ids: children.iter().map(|value| (*value).to_string()).collect(),
            direct_pack_ids: Vec::new(),
        }
    }

    #[test]
    fn digest_frames_sequence_fields_without_concatenation_ambiguity() {
        let first = metadata_sidecar_digest(&summary(&["ab", "c"]), &[]);
        let second = metadata_sidecar_digest(&summary(&["a", "bc"]), &[]);
        assert_ne!(first, second);
    }
}
