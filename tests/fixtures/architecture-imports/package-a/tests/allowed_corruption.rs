fn test_only_flat_corruption_fixture(snapshot: &SnapshotContent) {
    let _ = snapshot.manifest().entries.len();
    let _ = FlatNamespaceReader::new();
    let manifest_json = "corrupt";
    let manifest_chunk_cache = Vec::new();
    let pack_objects = Vec::new();
    let _ = open_snapshot_manifest(manifest_json, manifest_chunk_cache, pack_objects);
}
