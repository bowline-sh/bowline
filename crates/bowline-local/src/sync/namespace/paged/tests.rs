use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use bowline_core::{
    ids::{ContentId, ManifestDigest, NamespacePageId, PackId, SnapshotId, WorkspaceId},
    namespace_snapshot::{
        NamespaceBuildError, NamespaceCancellation, NamespaceDiff, NamespaceDiffVisitor,
        NamespaceMutation, NamespaceOperationBudget, NamespaceOperationContext, NamespaceReadError,
        NamespaceScope, NamespaceSnapshotBuilder, NamespaceVisitControl, SnapshotMetadata,
    },
    policy::{AccessFlag, MaterializationMode, PathClassification},
    workspace_graph::{
        ContentLayout, FileExecutability, HydrationState, NamespaceEntry, NamespaceEntryKind,
        SegmentId, SegmentLocator, SnapshotKind, WorkspaceRelativePath,
    },
};
use proptest::{
    collection::{btree_set, vec as prop_vec},
    prelude::*,
};

use super::{
    PackLengthResolver, PageNamespaceBuilder, PageNamespaceReader,
    codec::{Encoder, NamespaceEntryValue, NamespacePage, encode_namespace_page, logical_id},
    layout::{
        ContentLayoutRecord, SegmentChildSummary, SegmentPage, SegmentSequence,
        encode_content_layout, encode_segment_page, read_layout_range,
        validate_supported_segment_count,
    },
    tree::verify_graph_shape_without_identity,
    types::{
        MAX_SEGMENTS_PER_LAYOUT, MetadataIdentityKey, NAMESPACE_PAGE_MIN_BYTES, PageStore,
        PagedRecordSource,
    },
    validate_namespace_page_encoding,
};
use crate::sync::namespace::semantic_manifest_identity;

const BUDGET: NamespaceOperationBudget = NamespaceOperationBudget::new(
    10_000_000, 10_000_000, 10_000_000,
)
.with_metadata_limits(10_000_000, 10_000_000, 10_000_000, u64::MAX);

#[test]
fn canonical_empty_page_bytes_and_logical_id_are_golden() {
    let built = build(Vec::new());
    let bytes = built
        .store
        .namespace_page_bytes(&built.namespace_root_id)
        .expect("root page bytes");
    let mut expected = b"BWNP".to_vec();
    expected.extend_from_slice(&2_u16.to_be_bytes());
    expected.push(0);
    expected.extend_from_slice(&0_u32.to_be_bytes());
    expected.extend_from_slice(&0_u32.to_be_bytes());
    expected.extend_from_slice(&493_u32.to_be_bytes());
    expected.resize(NAMESPACE_PAGE_MIN_BYTES, 0);
    assert_eq!(bytes, expected);
    assert_eq!(
        built.namespace_root_id.as_str(),
        "nsp_30284888b4249170cd7a8680cf4da70199dd3d49578e14243d27d1de09c97534"
    );
    assert_eq!(bytes.len(), NAMESPACE_PAGE_MIN_BYTES);
}

#[test]
fn canonical_layout_and_segment_bytes_and_logical_ids_are_golden() {
    let locator = SegmentLocator {
        ordinal: 0,
        plaintext_length: 10,
        segment_id: SegmentId::new("seg_golden"),
        pack_id: PackId::new("pack_golden"),
        offset: 4,
        length: 12,
        format_version: 1,
    };
    let layout_bytes = encode_content_layout(&ContentLayoutRecord {
        format_version: super::CONTENT_LAYOUT_FORMAT_VERSION,
        logical_content_id: ContentId::new("cid_golden"),
        logical_length: 10,
        segment_size: 10,
        segments: SegmentSequence::Inline(vec![locator.clone()]),
    })
    .expect("golden layout encoding");
    let segment_bytes = encode_segment_page(&SegmentPage::Leaf {
        first_ordinal: 0,
        logical_start: 0,
        segments: vec![locator],
    })
    .expect("golden segment encoding");

    assert_eq!(
        hex_bytes(&layout_bytes),
        "4257434c00010000000a6369645f676f6c64656e000000000000000a000000000000000a000000000100000000000000000000000a0000000a7365675f676f6c64656e0000000b7061636b5f676f6c64656e0000000000000004000000000000000c0001"
    );
    assert_eq!(
        logical_id("ctl", identity_key(), &layout_bytes),
        "ctl_17a27720c3ae664ae9205d61ce84a42e29064dec5d1f9966966b79f707dfdf92"
    );
    assert_eq!(
        hex_bytes(&segment_bytes),
        "425753500001000000000000000000000000000000000100000000000000000000000a0000000a7365675f676f6c64656e0000000b7061636b5f676f6c64656e0000000000000004000000000000000c0001"
    );
    assert_eq!(
        logical_id("sgp", identity_key(), &segment_bytes),
        concat!(
            "sgp_",
            "58aed5724603e61ab84d30d023b20650e42a1cac400153e6c4c99f26225af1bd"
        )
    );
}

#[test]
fn namespace_decoder_rejects_hostile_entry_counts_before_allocation() {
    // The first count is the compact shape of the retired libFuzzer OOM reproducer.
    for hostile_count in [0x4545_4545_u32, u32::MAX] {
        let mut bytes = Vec::from(b"BWNP\0\x02\0\0\0\0\0".as_slice());
        bytes.extend_from_slice(&hostile_count.to_be_bytes());

        assert!(matches!(
            super::validate_namespace_page_encoding(&bytes),
            Err(NamespaceReadError::CorruptGraph {
                reason: "canonical collection length exceeds remaining record bytes"
            })
        ));
    }
}

#[test]
fn decoder_rejects_semantically_aliased_empty_leaf_prefix() {
    let bytes = encode_namespace_page(&NamespacePage::Leaf {
        common_prefix: b"guessed/private/path".to_vec(),
        entries: Vec::new(),
    })
    .expect("binary encoding is structurally valid");

    assert!(matches!(
        validate_namespace_page_encoding(&bytes),
        Err(NamespaceReadError::CorruptGraph {
            reason: "non-canonical empty namespace leaf prefix"
        })
    ));
}

#[test]
fn content_layout_decoder_rejects_paged_count_before_allocation() {
    let mut encoder = Encoder::new(b"BWCL", super::CONTENT_LAYOUT_FORMAT_VERSION);
    encoder.string("cid_fixture").expect("content ID");
    encoder.u64(1);
    encoder.u64(1);
    encoder.u8(1);
    encoder
        .logical_id(&format!("sgp_{}", "11".repeat(32)), "sgp")
        .expect("segment root ID");
    encoder.u64(u64::MAX);
    let bytes = encoder
        .finish("content layout", super::SEGMENT_PAGE_MAX_BYTES)
        .expect("small hostile layout");

    assert!(matches!(
        super::validate_content_layout_encoding(&bytes),
        Err(NamespaceReadError::CorruptGraph {
            reason: "content layout exceeds the supported segment count"
        })
    ));
}

#[test]
fn content_layout_decoder_rejects_a_count_that_cannot_cover_its_logical_shape() {
    let bytes = encode_content_layout(&ContentLayoutRecord {
        format_version: super::CONTENT_LAYOUT_FORMAT_VERSION,
        logical_content_id: ContentId::new("cid_bad_shape"),
        logical_length: 21,
        segment_size: 10,
        segments: SegmentSequence::Paged {
            root: bowline_core::ids::SegmentPageId::new(format!("sgp_{}", "11".repeat(32))),
            count: 2,
        },
    })
    .expect("invalid logical shape is structurally encodable");

    assert!(matches!(
        super::validate_content_layout_encoding(&bytes),
        Err(NamespaceReadError::CorruptGraph {
            reason: "paged content layout count does not match its logical shape"
        })
    ));
}

#[test]
fn segment_index_decoder_rejects_noncontiguous_summary_ranges() {
    let bytes = encode_segment_page(&SegmentPage::Index {
        first_ordinal: 0,
        segment_count: 2,
        logical_start: 0,
        logical_length: 20,
        children: vec![SegmentChildSummary {
            first_ordinal: 1,
            segment_count: 1,
            logical_start: 10,
            logical_length: 10,
            page_id: bowline_core::ids::SegmentPageId::new(format!("sgp_{}", "22".repeat(32))),
        }],
    })
    .expect("binary encoding is structurally valid");

    assert!(matches!(
        super::validate_segment_page_encoding(&bytes),
        Err(NamespaceReadError::CorruptGraph {
            reason: "segment index summaries are not contiguous"
        })
    ));
}

#[test]
fn paged_range_reader_rejects_leaf_values_that_violate_the_layout() {
    let identity_key = identity_key();
    let mut store = PageStore::with_identity_key(identity_key);
    let segment = SegmentLocator {
        ordinal: 0,
        plaintext_length: 10,
        segment_id: SegmentId::new("seg_forged"),
        pack_id: PackId::new("pack_forged"),
        offset: u64::MAX,
        length: 1,
        format_version: 1,
    };
    let leaf_bytes = encode_segment_page(&SegmentPage::Leaf {
        first_ordinal: 0,
        logical_start: 0,
        segments: vec![segment],
    })
    .expect("forged leaf is structurally encodable");
    let root = bowline_core::ids::SegmentPageId::new(logical_id("sgp", identity_key, &leaf_bytes));
    store
        .insert_segment_page(root.clone(), leaf_bytes)
        .expect("forged leaf fixture");
    let layout_bytes = encode_content_layout(&ContentLayoutRecord {
        format_version: super::CONTENT_LAYOUT_FORMAT_VERSION,
        logical_content_id: ContentId::new("cid_forged_range"),
        logical_length: 10,
        segment_size: 10,
        segments: SegmentSequence::Paged { root, count: 1 },
    })
    .expect("forged layout is structurally encodable");
    let layout_id =
        bowline_core::ids::ContentLayoutId::new(logical_id("ctl", identity_key, &layout_bytes));
    store
        .insert_content_layout(layout_id.clone(), layout_bytes)
        .expect("forged layout fixture");

    assert!(matches!(
        read_layout_range("ws_page_fixture", &layout_id, &store, 0, 1, &mut context(),),
        Err(NamespaceReadError::CorruptGraph {
            reason: "segment pack range overflows"
        })
    ));
}

#[test]
fn builder_rejects_noncanonical_current_directory_path_segments() {
    let mut builder = PageNamespaceBuilder::new(metadata(), identity_key());
    let mut operation = context();
    let error = builder
        .apply(
            NamespaceMutation::Upsert(file_entry("apps/web/./.work/private", 1)),
            &mut operation,
        )
        .expect_err("current-directory path segments cannot enter canonical pages");
    assert!(matches!(
        error,
        NamespaceBuildError::Read(NamespaceReadError::InvalidPath { .. })
    ));
}

#[test]
fn metadata_dependencies_deduplicate_shared_content_layouts() {
    let first = layout_entry("copy-a.bin", 2);
    let mut second = first.clone();
    second.path = "copy-b.bin".to_string();
    let built = build(vec![first, second]);
    let root = built
        .store
        .metadata_record(built.namespace_root_id.as_str())
        .expect("root summary")
        .expect("root metadata record");
    assert_eq!(root.child_logical_ids.len(), 1);
    assert!(root.child_logical_ids[0].starts_with("ctl_"));
}

#[test]
fn full_builder_rejects_noncanonical_current_directory_path_segments() {
    let error = PageNamespaceBuilder::full(
        metadata(),
        identity_key(),
        vec![file_entry("apps/web/./.work/private", 1)],
        &mut context(),
    )
    .expect_err("draft normalization cannot hide private namespace segments");
    assert!(matches!(
        error,
        NamespaceBuildError::Read(NamespaceReadError::InvalidPath { .. })
    ));
}

#[test]
fn insertion_and_mutation_order_produce_identical_page_graphs() {
    let entries = (0..500)
        .map(|index| file_entry(&format!("src/module-{index:04}.rs"), index))
        .collect::<Vec<_>>();
    let mut reversed = entries.clone();
    reversed.reverse();
    let forward = build(entries.clone());
    let reverse = build(reversed);
    assert_eq!(forward.namespace_root_id, reverse.namespace_root_id);
    assert_eq!(forward.store, reverse.store);

    let mut left = PageNamespaceBuilder::new(metadata(), identity_key());
    let mut right = PageNamespaceBuilder::new(metadata(), identity_key());
    let mut left_context = context();
    let mut right_context = context();
    for entry in &entries {
        left.apply(NamespaceMutation::Upsert(entry.clone()), &mut left_context)
            .expect("left upsert");
    }
    for entry in entries.iter().rev() {
        right
            .apply(NamespaceMutation::Upsert(entry.clone()), &mut right_context)
            .expect("right upsert");
    }
    let left = left.finish(&mut left_context).expect("left finish");
    let right = right.finish(&mut right_context).expect("right finish");
    assert_eq!(left.namespace_root_id, right.namespace_root_id);
    assert_eq!(left.store, right.store);
}

#[test]
fn every_namespace_page_obeys_the_hard_encoded_byte_bounds() {
    let built = build(
        (0..2_000)
            .map(|index| file_entry(&format!("bounded/{index:06}/entry"), index))
            .collect(),
    );
    for record in built.store.metadata_records().expect("metadata summaries") {
        if record.kind == super::MetadataRecordKind::NamespacePage {
            assert!(record.encoded_bytes >= NAMESPACE_PAGE_MIN_BYTES as u64);
            assert!(record.encoded_bytes <= super::types::NAMESPACE_PAGE_MAX_BYTES as u64);
        }
    }
}

#[test]
fn incremental_and_full_builds_have_exact_graph_and_semantic_identity_parity() {
    let base_entries = (0..800)
        .map(|index| file_entry(&format!("tree/{index:04}/file"), index))
        .collect::<Vec<_>>();
    let base = build(base_entries.clone());
    let mut no_op_context = context();
    let no_op = PageNamespaceBuilder::incremental(&base, &mut no_op_context)
        .expect("no-op incremental")
        .finish(&mut no_op_context)
        .expect("no-op finish");
    assert_eq!(no_op.changed.namespace_pages_created, 0);
    assert_eq!(no_op.changed.content_layouts_created, 0);
    assert_eq!(no_op.changed.segment_pages_created, 0);
    assert_eq!(no_op.changed.semantic_entries_hashed, 0);
    assert_eq!(no_op.changed.namespace_pages_encoded, 0);
    assert_eq!(no_op.changed.namespace_pages_loaded_during_build, 0);
    let mut incremental_context = context();
    let mut incremental =
        PageNamespaceBuilder::incremental(&base, &mut incremental_context).expect("incremental");
    incremental
        .apply(
            NamespaceMutation::Upsert(file_entry("tree/0400/file", 99_999)),
            &mut incremental_context,
        )
        .expect("edit");
    incremental
        .apply(
            NamespaceMutation::Remove(WorkspaceRelativePath::new("tree/0600/file")),
            &mut incremental_context,
        )
        .expect("remove");
    incremental
        .apply(
            NamespaceMutation::Upsert(file_entry("tree/new/file", 100_000)),
            &mut incremental_context,
        )
        .expect("add");
    let incremental = incremental
        .finish(&mut incremental_context)
        .expect("incremental finish");

    let mut expected = base_entries
        .into_iter()
        .filter(|entry| entry.path != "tree/0600/file" && entry.path != "tree/0400/file")
        .collect::<Vec<_>>();
    expected.push(file_entry("tree/0400/file", 99_999));
    expected.push(file_entry("tree/new/file", 100_000));
    let full = build(expected.clone());
    assert_eq!(incremental.namespace_root_id, full.namespace_root_id);
    assert_eq!(
        incremental
            .store
            .reachable_plaintext_records(&incremental.namespace_root_id)
            .expect("incremental reachable graph"),
        full.store
            .reachable_plaintext_records(&full.namespace_root_id)
            .expect("full reachable graph")
    );
    assert_eq!(incremental.snapshot_id, full.snapshot_id);
    assert_eq!(
        incremental.semantic_manifest_digest,
        full.semantic_manifest_digest
    );
    assert!(incremental.changed.namespace_pages_created < incremental.store.namespace_page_count());
    assert!(incremental.changed.namespace_pages_reused > 0);
    assert!(incremental.changed.namespace_pages_encoded < incremental.store.namespace_page_count());

    expected.sort_by(|left, right| left.path.cmp(&right.path));
    let flat_identity = semantic_manifest_identity(&WorkspaceId::new("ws_page_fixture"), &expected);
    assert_eq!(&incremental.snapshot_id, flat_identity.snapshot_id());
    assert_eq!(
        &incremental.semantic_manifest_digest,
        flat_identity.digest()
    );
}

#[test]
fn descriptor_lookup_prefix_and_paged_diff_are_lazy_and_component_correct() {
    let left = build(vec![
        file_entry("src/lib.rs", 1),
        file_entry("src/main.rs", 2),
        file_entry("src-old/lib.rs", 3),
        file_entry("unchanged/deep/file", 4),
    ]);
    let right = build(vec![
        file_entry("src/lib.rs", 9),
        file_entry("src/new.rs", 5),
        file_entry("src-old/lib.rs", 3),
        file_entry("unchanged/deep/file", 4),
    ]);
    let reader = PageNamespaceReader::new(&left);
    let other = PageNamespaceReader::new(&right);
    let mut operation = context();
    assert_eq!(
        reader
            .descriptor(&WorkspaceRelativePath::new("src/lib.rs"), &mut operation)
            .expect("descriptor")
            .expect("present")
            .entry_without_layout
            .path,
        "src/lib.rs"
    );
    let mut paths = Vec::new();
    reader
        .visit_prefix_descriptors(
            &WorkspaceRelativePath::new("src"),
            &mut operation,
            &mut |descriptor| {
                paths.push(descriptor.entry_without_layout.path);
                Ok(NamespaceVisitControl::Continue)
            },
        )
        .expect("prefix");
    assert_eq!(paths, vec!["src/lib.rs", "src/main.rs"]);

    let mut differences = Differences(Vec::new());
    let before_pages = operation.counters().namespace_pages_loaded;
    reader
        .diff_paged(
            &other,
            &NamespaceScope::All,
            &mut differences,
            &mut operation,
        )
        .expect("paged diff");
    assert_eq!(differences.0.len(), 3);
    let diff_pages = operation.counters().namespace_pages_loaded - before_pages;
    assert!(
        diff_pages <= 16,
        "diff exceeded its small-graph bound: {:?}",
        operation.counters()
    );
    assert!(
        matches!(&differences.0[0], NamespaceDiff::Modified { before, .. } if before.path == "src/lib.rs")
    );
    assert!(
        matches!(&differences.0[1], NamespaceDiff::Removed(entry) if entry.path == "src/main.rs")
    );
    assert!(matches!(&differences.0[2], NamespaceDiff::Added(entry) if entry.path == "src/new.rs"));
}

#[test]
fn paged_diff_streams_under_entry_budgets_and_honors_cancellation() {
    let left = build(
        (0..1_000)
            .map(|index| file_entry(&format!("src/{index:06}.rs"), index))
            .collect(),
    );
    let right = build(
        (0..1_000)
            .map(|index| file_entry(&format!("src/{index:06}.rs"), index + 1))
            .collect(),
    );
    let reader = PageNamespaceReader::new(&left);
    let other = PageNamespaceReader::new(&right);
    let mut differences = Differences(Vec::new());
    let budget = NamespaceOperationBudget::new(10_000, 2, 10).with_metadata_limits(
        10_000,
        10_000,
        10_000,
        u64::MAX,
    );
    let mut operation = NamespaceOperationContext::uncancelled(budget);
    let error = reader
        .diff_paged(
            &other,
            &NamespaceScope::All,
            &mut differences,
            &mut operation,
        )
        .expect_err("diff entry budget must stop the streamed traversal");
    assert!(
        matches!(error, NamespaceReadError::BudgetExceeded { .. }),
        "unexpected diff error: {error:?}"
    );
    assert_eq!(differences.0.len(), 2);

    let cancelled = Cancelled;
    let mut cancelled_context = NamespaceOperationContext::new(BUDGET, &cancelled);
    let mut cancelled_differences = Differences(Vec::new());
    assert!(matches!(
        reader.diff_paged(
            &other,
            &NamespaceScope::All,
            &mut cancelled_differences,
            &mut cancelled_context,
        ),
        Err(NamespaceReadError::Cancelled)
    ));
    assert!(cancelled_differences.0.is_empty());
}

#[test]
fn on_demand_source_exact_lookup_loads_only_the_radix_path_and_honors_cancellation() {
    let built = build(
        (0..10_000)
            .map(|index| file_entry(&format!("src/{index:06}/file"), index))
            .collect(),
    );
    let source = CountingSource::from_store(&built.store);
    let reader =
        PageNamespaceReader::from_source(&built.metadata, &built.namespace_root_id, &source);
    let mut operation = context();
    let found = reader
        .descriptor(
            &WorkspaceRelativePath::new("src/005000/file"),
            &mut operation,
        )
        .expect("lazy exact lookup");
    assert!(found.is_some());
    assert!(source.loads() <= 8);
    assert_eq!(source.loads(), operation.counters().namespace_pages_loaded);

    let cancelled = Cancelled;
    let mut cancelled_context = NamespaceOperationContext::new(BUDGET, &cancelled);
    assert!(matches!(
        reader.descriptor(
            &WorkspaceRelativePath::new("src/009999/file"),
            &mut cancelled_context,
        ),
        Err(NamespaceReadError::Cancelled)
    ));

    let prefix_source = CountingSource::from_store(&built.store);
    let prefix_reader =
        PageNamespaceReader::from_source(&built.metadata, &built.namespace_root_id, &prefix_source);
    let mut prefix_context = context();
    prefix_reader
        .visit_prefix_descriptors(
            &WorkspaceRelativePath::new("src"),
            &mut prefix_context,
            &mut |_| Ok(NamespaceVisitControl::Stop),
        )
        .expect("lazy prefix lookup");
    assert!(
        prefix_source
            .prefetch_batches()
            .iter()
            .any(|batch| batch.len() > 1),
        "prefix traversal must expose anticipated siblings for binding batching"
    );
}

#[test]
fn large_layout_is_descriptor_lazy_and_range_reads_only_intersecting_pages() {
    let entry = layout_entry("large.bin", 2_048);
    let built = build(vec![entry]);
    assert!(built.store.segment_page_count() > 2);
    let reader = PageNamespaceReader::new(&built);
    let mut operation = context();
    let descriptor = reader
        .descriptor(&WorkspaceRelativePath::new("large.bin"), &mut operation)
        .expect("descriptor")
        .expect("entry");
    assert_eq!(operation.counters().layout_records_loaded, 0);
    assert_eq!(operation.counters().segment_pages_loaded, 0);
    let layout_id = descriptor.content_layout_id.expect("layout ID");
    let segments = reader
        .content_range(&layout_id, 2_047 * 1_024 + 10, 20, &mut operation)
        .expect("range");
    assert_eq!(segments.len(), 1);
    assert_eq!(segments[0].ordinal, 2_047);
    assert_eq!(operation.counters().layout_records_loaded, 1);
    assert!(operation.counters().segment_pages_loaded <= 3);
}

#[test]
fn production_segment_count_limit_is_finite_and_checked_without_allocation() {
    assert_eq!(MAX_SEGMENTS_PER_LAYOUT, 1_000_000);
    validate_supported_segment_count(MAX_SEGMENTS_PER_LAYOUT).expect("maximum is supported");
    assert!(matches!(
        validate_supported_segment_count(MAX_SEGMENTS_PER_LAYOUT + 1),
        Err(NamespaceReadError::CorruptGraph { .. })
    ));
}

#[test]
fn metadata_budgets_cancel_before_unbounded_page_or_range_reads() {
    let built = build(vec![layout_entry("large.bin", 512)]);
    let reader = PageNamespaceReader::new(&built);
    let mut lookup = NamespaceOperationContext::uncancelled(
        NamespaceOperationBudget::new(10, 10, 10).with_metadata_limits(0, 0, 0, 0),
    );
    assert!(matches!(
        reader.descriptor(&WorkspaceRelativePath::new("large.bin"), &mut lookup),
        Err(NamespaceReadError::BudgetExceeded { .. })
    ));
}

#[test]
fn oversized_single_entry_is_typed_and_does_not_echo_the_path() {
    let canary = "private/canary-name";
    let mut entry = file_entry(canary, 1);
    entry.kind = NamespaceEntryKind::Symlink;
    entry.symlink_target = Some("x".repeat(32 * 1_024));
    let mut operation = context();
    let error = PageNamespaceBuilder::full(metadata(), identity_key(), vec![entry], &mut operation)
        .expect_err("oversized entry must fail");
    assert!(matches!(
        error,
        bowline_core::namespace_snapshot::NamespaceBuildError::Read(
            NamespaceReadError::OversizedRecord { .. }
        )
    ));
    assert!(!error.to_string().contains(canary));
}

#[test]
fn corruption_version_order_missing_child_identity_and_cycle_fail_closed() {
    let built = build(
        (0..500)
            .map(|index| file_entry(&format!("d/{index:04}"), index))
            .collect(),
    );
    let root_bytes = built
        .store
        .namespace_page_bytes(&built.namespace_root_id)
        .expect("root bytes");
    let mut unsupported = root_bytes.to_vec();
    unsupported[4..6].copy_from_slice(&3_u16.to_be_bytes());
    assert!(matches!(
        validate_namespace_page_encoding(&unsupported),
        Err(NamespaceReadError::UnsupportedFormat { version: 3, .. })
    ));
    let mut nonzero_padding = root_bytes.to_vec();
    *nonzero_padding.last_mut().expect("padded root") = 1;
    assert!(matches!(
        validate_namespace_page_encoding(&nonzero_padding),
        Err(NamespaceReadError::CorruptGraph { .. })
    ));

    let value = NamespaceEntryValue::from_entry(&file_entry("a", 1), None);
    let unordered = NamespacePage::Leaf {
        common_prefix: Vec::new(),
        entries: vec![
            (b"z".to_vec(), value.clone()),
            (b"a".to_vec(), value.clone()),
        ],
    };
    assert!(matches!(
        encode_namespace_page(&unordered),
        Err(NamespaceReadError::NonCanonicalOrder { .. })
    ));

    let mut corrupt_store = built.store.clone();
    let corrupt_bytes = {
        let mut bytes = root_bytes.to_vec();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        bytes
    };
    corrupt_store.insert_namespace_page_bytes(built.namespace_root_id.clone(), corrupt_bytes);
    let corrupt_snapshot = super::BuiltPagedNamespaceSnapshot {
        store: corrupt_store,
        ..built.clone()
    };
    let mut operation = context();
    assert!(matches!(
        PageNamespaceReader::new(&corrupt_snapshot)
            .descriptor(&WorkspaceRelativePath::new("d/1"), &mut operation),
        Err(NamespaceReadError::CorruptGraph { .. })
    ));

    let cycle_id = NamespacePageId::new(format!("nsp_{}", "00".repeat(32)));
    let cycle = NamespacePage::Branch {
        common_prefix: Vec::new(),
        children: vec![(b'x', cycle_id.clone())],
        value: Some(value),
    };
    let mut cycle_store = PageStore::with_identity_key(identity_key());
    cycle_store.insert_namespace_page_bytes(
        cycle_id.clone(),
        encode_namespace_page(&cycle).expect("cycle fixture encodes"),
    );
    assert!(matches!(
        verify_graph_shape_without_identity(&cycle_id, &cycle_store),
        Err(NamespaceReadError::CorruptGraph { .. })
    ));

    let mut missing_store = built.store.clone();
    let child = missing_store
        .metadata_records()
        .expect("metadata summaries")
        .into_iter()
        .find(|record| !record.child_logical_ids.is_empty())
        .and_then(|record| {
            record
                .child_logical_ids
                .into_iter()
                .find(|id| id.starts_with("nsp_"))
        })
        .expect("branch child");
    missing_store.remove_namespace_page(&NamespacePageId::new(child));
    let missing_snapshot = super::BuiltPagedNamespaceSnapshot {
        store: missing_store,
        ..built
    };
    let mut operation = context();
    assert!(matches!(
        PageNamespaceReader::new(&missing_snapshot).verify(&mut operation),
        Err(NamespaceReadError::MissingRecord { .. })
    ));
}

#[test]
fn logical_ids_and_object_metadata_never_contain_plaintext_paths() {
    let canary = "private/customer-secret-name.txt";
    let built = build(vec![file_entry(canary, 1)]);
    assert!(!built.namespace_root_id.as_str().contains(canary));
    for record in built.store.metadata_records().expect("metadata summaries") {
        assert!(!record.logical_id.contains(canary));
        assert!(
            record
                .child_logical_ids
                .iter()
                .all(|id| !id.contains(canary))
        );
    }
}

#[test]
fn logical_ids_are_keyed_by_the_secret_workspace_identity_context() {
    let entries = vec![file_entry("private/customer-secret-name.txt", 1)];
    let first = PageNamespaceBuilder::full(
        metadata(),
        MetadataIdentityKey::derive(&WorkspaceId::new("ws_page_fixture"), [41; 32]),
        entries.clone(),
        &mut context(),
    )
    .expect("first secret-keyed graph");
    let second = PageNamespaceBuilder::full(
        metadata(),
        MetadataIdentityKey::derive(&WorkspaceId::new("ws_page_fixture"), [42; 32]),
        entries,
        &mut context(),
    )
    .expect("second secret-keyed graph");

    assert_eq!(first.snapshot_id, second.snapshot_id);
    assert_eq!(
        first.semantic_manifest_digest,
        second.semantic_manifest_digest
    );
    assert_ne!(first.namespace_root_id, second.namespace_root_id);
    assert_eq!(
        first
            .store
            .namespace_page_bytes(&first.namespace_root_id)
            .expect("first canonical bytes"),
        second
            .store
            .namespace_page_bytes(&second.namespace_root_id)
            .expect("second canonical bytes"),
    );
}

#[test]
fn pack_length_resolver_rejects_out_of_bounds_locator() {
    let mut builder = PageNamespaceBuilder::new(metadata(), identity_key())
        .with_pack_length_resolver(Arc::new(FixedPackLength(100)));
    let mut operation = context();
    builder
        .apply(
            NamespaceMutation::Upsert(layout_entry("bad.bin", 2)),
            &mut operation,
        )
        .expect("upsert");
    assert!(matches!(
        builder.finish(&mut operation),
        Err(bowline_core::namespace_snapshot::NamespaceBuildError::Read(
            NamespaceReadError::CorruptGraph { .. }
        ))
    ));
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn randomized_valid_path_sets_are_build_and_mutation_order_independent(
        indexes in btree_set(0_u16..4_000, 1..250)
    ) {
        let entries = indexes
            .iter()
            .map(|index| file_entry(&format!("root/{index:04}/file"), u64::from(*index)))
            .collect::<Vec<_>>();
        let mut reversed = entries.clone();
        reversed.reverse();
        let full = build(entries.clone());
        let reverse = build(reversed.clone());
        prop_assert_eq!(&full.namespace_root_id, &reverse.namespace_root_id);
        prop_assert_eq!(&full.store, &reverse.store);

        let mut first = PageNamespaceBuilder::new(metadata(), identity_key());
        let mut second = PageNamespaceBuilder::new(metadata(), identity_key());
        let mut first_context = context();
        let mut second_context = context();
        for entry in entries {
            first.apply(NamespaceMutation::Upsert(entry), &mut first_context).expect("first mutation");
        }
        for entry in reversed {
            second.apply(NamespaceMutation::Upsert(entry), &mut second_context).expect("second mutation");
        }
        let first = first.finish(&mut first_context).expect("first graph");
        let second = second.finish(&mut second_context).expect("second graph");
        prop_assert_eq!(first.namespace_root_id, second.namespace_root_id);
        prop_assert_eq!(first.store, second.store);
    }

    #[test]
    fn randomized_incremental_mutations_match_full_canonical_graph(
        indexes in btree_set(0_u16..2_000, 1..150),
        operations in prop_vec((0_u8..3, 0_u16..2_000), 1..80),
    ) {
        let base_entries = indexes
            .iter()
            .map(|index| file_entry(&format!("root/{index:04}/file"), u64::from(*index)))
            .collect::<Vec<_>>();
        let base = build(base_entries.clone());
        let mut expected = base_entries
            .into_iter()
            .map(|entry| (entry.path.clone(), entry))
            .collect::<std::collections::BTreeMap<_, _>>();
        let mut operation = context();
        let mut incremental = PageNamespaceBuilder::incremental(&base, &mut operation)
            .expect("persistent incremental builder");

        for (kind, index) in operations {
            let path = format!("root/{index:04}/file");
            match kind {
                0 => {
                    let entry = file_entry(&path, u64::from(index) + 100_000);
                    expected.insert(path, entry.clone());
                    incremental
                        .apply(NamespaceMutation::Upsert(entry), &mut operation)
                        .expect("incremental upsert");
                }
                1 => {
                    expected.remove(&path);
                    incremental
                        .apply(
                            NamespaceMutation::Remove(WorkspaceRelativePath::new(path)),
                            &mut operation,
                        )
                        .expect("incremental remove");
                }
                _ => {
                    let prefix = format!("root/{index:04}");
                    expected.retain(|path, _| {
                        path != &prefix && !path.starts_with(&format!("{prefix}/"))
                    });
                    incremental
                        .apply(
                            NamespaceMutation::RemovePrefix(WorkspaceRelativePath::new(prefix)),
                            &mut operation,
                        )
                        .expect("incremental remove prefix");
                }
            }
        }

        let incremental = incremental.finish(&mut operation).expect("incremental graph");
        let full = build(expected.into_values().collect());
        prop_assert_eq!(&incremental.namespace_root_id, &full.namespace_root_id);
        prop_assert_eq!(&incremental.snapshot_id, &full.snapshot_id);
        prop_assert_eq!(
            incremental
                .store
                .reachable_plaintext_records(&incremental.namespace_root_id)
                .expect("incremental reachable graph"),
            full.store
                .reachable_plaintext_records(&full.namespace_root_id)
                .expect("full reachable graph"),
        );
    }
}

fn build(entries: Vec<NamespaceEntry>) -> super::BuiltPagedNamespaceSnapshot {
    PageNamespaceBuilder::full(metadata(), identity_key(), entries, &mut context())
        .expect("page build")
}

fn identity_key() -> MetadataIdentityKey {
    MetadataIdentityKey::derive(&WorkspaceId::new("ws_page_fixture"), [42; 32])
}

fn hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn metadata() -> SnapshotMetadata {
    SnapshotMetadata {
        schema_version: 1,
        snapshot_id: SnapshotId::new("pending"),
        workspace_id: WorkspaceId::new("ws_page_fixture"),
        project_id: None,
        kind: SnapshotKind::WorkspaceHead,
        base_snapshot_id: None,
        semantic_manifest_digest: ManifestDigest::new("pending"),
        entry_count: 0,
        refs: Vec::new(),
    }
}

fn context() -> NamespaceOperationContext<'static> {
    NamespaceOperationContext::uncancelled(BUDGET)
}

fn file_entry(path: &str, seed: u64) -> NamespaceEntry {
    NamespaceEntry {
        path: path.to_string(),
        kind: NamespaceEntryKind::File,
        classification: PathClassification::WorkspaceSync,
        mode: MaterializationMode::EncryptedSync,
        access: vec![AccessFlag::HumanReadable, AccessFlag::AgentReadable],
        content_id: Some(ContentId::new(format!("cid_{seed:016x}"))),
        content_layout: None,
        symlink_target: None,
        byte_len: Some(seed),
        executability: FileExecutability::Regular,
        hydration_state: HydrationState::Local,
    }
}

fn layout_entry(path: &str, count: u32) -> NamespaceEntry {
    let segments = (0..count)
        .map(|ordinal| SegmentLocator {
            ordinal,
            plaintext_length: 1_024,
            segment_id: SegmentId::new(format!("seg_{ordinal:08}")),
            pack_id: PackId::new(format!("pack_{:04}", ordinal / 64)),
            offset: u64::from(ordinal % 64) * 2_048,
            length: 2_048,
            format_version: 1,
        })
        .collect();
    let mut entry = file_entry(path, u64::from(count) * 1_024);
    entry.byte_len = Some(u64::from(count) * 1_024);
    entry.content_layout = Some(ContentLayout::SegmentedV1 {
        logical_content_id: entry.content_id.clone().expect("content ID"),
        logical_length: u64::from(count) * 1_024,
        segment_size: 1_024,
        segments,
    });
    entry
}

struct Differences(Vec<NamespaceDiff>);

impl NamespaceDiffVisitor for Differences {
    fn visit(&mut self, difference: NamespaceDiff) -> Result<(), NamespaceReadError> {
        self.0.push(difference);
        Ok(())
    }
}

struct FixedPackLength(u64);

impl PackLengthResolver for FixedPackLength {
    fn pack_length(&self, _pack_id: &PackId) -> Result<Option<u64>, NamespaceReadError> {
        Ok(Some(self.0))
    }
}

struct CountingSource {
    identity_key: MetadataIdentityKey,
    records: BTreeMap<(super::MetadataRecordKind, String), Arc<[u8]>>,
    loads: Mutex<u64>,
    prefetch_batches: Mutex<Vec<Vec<String>>>,
}

impl CountingSource {
    fn from_store(store: &PageStore) -> Self {
        let records = store
            .plaintext_records()
            .expect("source fixture records")
            .into_iter()
            .map(|record| {
                (
                    (record.summary.kind, record.summary.logical_id),
                    Arc::<[u8]>::from(record.plaintext),
                )
            })
            .collect();
        Self {
            identity_key: store.identity_key(),
            records,
            loads: Mutex::new(0),
            prefetch_batches: Mutex::new(Vec::new()),
        }
    }

    fn loads(&self) -> u64 {
        *self.loads.lock().expect("counting source lock")
    }

    fn prefetch_batches(&self) -> Vec<Vec<String>> {
        self.prefetch_batches
            .lock()
            .expect("counting source prefetch lock")
            .clone()
    }
}

impl PagedRecordSource for CountingSource {
    fn metadata_identity_key(&self) -> MetadataIdentityKey {
        self.identity_key
    }

    fn load_record(
        &self,
        kind: super::MetadataRecordKind,
        logical_id: &str,
        context: &mut NamespaceOperationContext<'_>,
    ) -> Result<Option<Arc<[u8]>>, NamespaceReadError> {
        context.ensure_active()?;
        *self.loads.lock().expect("counting source lock") += 1;
        Ok(self.records.get(&(kind, logical_id.to_string())).cloned())
    }

    fn prefetch_records(
        &self,
        _kind: super::MetadataRecordKind,
        logical_ids: &[String],
        context: &mut NamespaceOperationContext<'_>,
    ) -> Result<(), NamespaceReadError> {
        context.ensure_active()?;
        if !logical_ids.is_empty() {
            self.prefetch_batches
                .lock()
                .expect("counting source prefetch lock")
                .push(logical_ids.to_vec());
        }
        Ok(())
    }
}

struct Cancelled;

impl NamespaceCancellation for Cancelled {
    fn is_cancelled(&self) -> bool {
        true
    }
}
