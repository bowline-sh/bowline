fn forbidden_flat_authority(snapshot: &SnapshotContent) {
    let _ = snapshot.manifest().entries.len();
    let _ = FlatNamespaceBuilder::new();
    let manifest_json = String::new();
    let manifest_chunk_cache = Vec::new();
    let pack_objects = Vec::new();
    let entries = snapshot_entries(snapshot);
    let metadata_pages = snapshot.namespace_store().plaintext_records();
    let _ = seal_snapshot_manifest(manifest_json, manifest_chunk_cache, pack_objects);
}

#[cfg(test)]
mod tests {
    fn corruption_fixture(snapshot: &SnapshotContent) {
        let _ = snapshot.manifest().entries.len();
        let _ = FlatNamespaceReader::new();
        let manifest_json = "corrupt";
        let pack_objects = Vec::new();
        let _ = open_snapshot_manifest(manifest_json, pack_objects);
    }
}
