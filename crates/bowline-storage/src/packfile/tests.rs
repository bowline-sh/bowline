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
