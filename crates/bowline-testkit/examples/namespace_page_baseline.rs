use std::{error::Error, fmt::Write};

use bowline_core::{
    ids::{
        ContentId, ContentLayoutId, ManifestDigest, NamespacePageId, PackId, SegmentPageId,
        SnapshotId, WorkspaceId,
    },
    namespace_snapshot::{
        EntryVisitor, NamespaceDiff, NamespaceDiffVisitor, NamespaceMutation,
        NamespaceOperationBudget, NamespaceOperationContext, NamespaceReadError, NamespaceScope,
        NamespaceSnapshotBuilder, NamespaceSnapshotReader, NamespaceVisitControl, SnapshotMetadata,
    },
    policy::{AccessFlag, MaterializationMode, PathClassification},
    workspace_graph::{
        ContentLayout, FileExecutability, HydrationState, NamespaceEntry, NamespaceEntryKind,
        SegmentId, SegmentLocator, SnapshotKind, WorkspaceRelativePath,
    },
};
use bowline_local::sync::namespace::{
    BuiltPagedNamespaceSnapshot, ChangedPageSummary, MAX_SEGMENTS_PER_LAYOUT, MetadataIdentityKey,
    PageNamespaceBuilder, PageNamespaceReader,
};
use bowline_storage::{SnapshotMetadataRecordId, StorageKey, seal_snapshot_metadata_page};
use serde_json::Value;

const PARAMETERS: &str = include_str!("../../../tests/corpora/namespace-baseline-v1.json");
const COUNTS: [usize; 3] = [10_000, 100_000, 1_000_000];
const SHAPES: [&str; 3] = ["shallow-wide", "deep-tree", "mixed-repository"];

fn identity_key() -> MetadataIdentityKey {
    MetadataIdentityKey::derive(&WorkspaceId::new("ws_namespace_baseline_v1"), [0x5e; 32])
}

fn main() -> Result<(), Box<dyn Error>> {
    validate_parameters(&serde_json::from_str(PARAMETERS)?)?;
    let mut shapes = Vec::new();
    for count in COUNTS {
        for shape in SHAPES {
            shapes.push(measure_shape(count, shape)?);
        }
    }
    let layouts = [1, 64, 1_024]
        .into_iter()
        .map(measure_layout)
        .collect::<Result<Vec<_>, _>>()?;
    let scenarios = measure_change_scenarios()?;
    print!("{}", render_report(&shapes, &layouts, &scenarios)?);
    Ok(())
}

fn validate_parameters(parameters: &Value) -> Result<(), &'static str> {
    if parameters["generatorVersion"] != 1
        || parameters["seed"] != "0x5eed112a"
        || parameters["entryCounts"] != serde_json::json!(COUNTS)
        || parameters["shapes"] != serde_json::json!(SHAPES)
        || parameters["measurementMaximumSegments"] != 1_024
        || MAX_SEGMENTS_PER_LAYOUT != 1_000_000
    {
        return Err("namespace corpus parameters do not match generator v1");
    }
    Ok(())
}

#[derive(Debug)]
struct ShapeMeasurement {
    entry_count: usize,
    shape: &'static str,
    root_id: String,
    namespace_pages: u64,
    encoded_bytes: u64,
    sealed_bytes: u64,
    hosted_document_bytes: u64,
    manifest_digest: String,
    snapshot_id: String,
    lookup_pages: u64,
    lookup_bytes: u64,
    prefix_entries: u64,
    prefix_pages: u64,
    prefix_bytes: u64,
    no_op: ChangedPageSummary,
    one_edit: ChangedPageSummary,
}

fn measure_shape(
    entry_count: usize,
    shape: &'static str,
) -> Result<ShapeMeasurement, Box<dyn Error>> {
    let entries = generate_entries(entry_count, shape);
    let mut build_context = operation_context(entry_count as u64);
    let snapshot =
        PageNamespaceBuilder::full(metadata(shape), identity_key(), entries, &mut build_context)?;
    if snapshot.snapshot_id.as_str() != expected_snapshot_id(entry_count, shape) {
        return Err("paged SnapshotId diverged from the paired flat Phase-0 baseline".into());
    }

    let lookup_index = entry_count / 2 + 10;
    let (lookup_path, _) = path_and_kind(lookup_index, shape);
    let mut lookup_context = operation_context(1);
    let reader = PageNamespaceReader::new(&snapshot);
    let found = reader.descriptor(
        &WorkspaceRelativePath::new(lookup_path),
        &mut lookup_context,
    )?;
    if found.is_none() {
        return Err("deterministic exact-lookup probe was absent".into());
    }
    let lookup = lookup_context.counters();

    let mut prefix_context = operation_context(entry_count as u64);
    let mut prefix_counter = CountEntries::default();
    reader.visit_prefix(
        &WorkspaceRelativePath::new(measured_prefix(entry_count, shape)),
        &mut prefix_counter,
        &mut prefix_context,
    )?;
    let prefix = prefix_context.counters();

    let no_op = incremental_no_op(&snapshot, entry_count as u64)?;
    let one_edit = incremental_one_edit(&snapshot, entry_count, shape)?;
    let (sealed_bytes, hosted_document_bytes) = metadata_transport_bytes(&snapshot)?;
    Ok(ShapeMeasurement {
        entry_count,
        shape,
        root_id: snapshot.namespace_root_id.as_str().to_string(),
        namespace_pages: snapshot.store.namespace_page_count(),
        encoded_bytes: snapshot.store.total_encoded_bytes(),
        sealed_bytes,
        hosted_document_bytes,
        manifest_digest: snapshot.semantic_manifest_digest.as_str().to_string(),
        snapshot_id: snapshot.snapshot_id.as_str().to_string(),
        lookup_pages: lookup.namespace_pages_loaded,
        lookup_bytes: lookup.metadata_bytes,
        prefix_entries: prefix_counter.0,
        prefix_pages: prefix.namespace_pages_loaded,
        prefix_bytes: prefix.metadata_bytes,
        no_op,
        one_edit,
    })
}

fn metadata_transport_bytes(
    snapshot: &BuiltPagedNamespaceSnapshot,
) -> Result<(u64, u64), Box<dyn Error>> {
    let mut sealed_bytes = 0_u64;
    let mut hosted_document_bytes = 0_u64;
    let mut context = operation_context(snapshot.metadata.entry_count);
    snapshot.store.visit_new_reachable_plaintext_records(
        &snapshot.namespace_root_id,
        &mut context,
        |record| -> Result<(), Box<dyn Error>> {
            let record_kind = record.summary.kind.as_str();
            let logical_id = match record.summary.kind {
                bowline_local::sync::namespace::MetadataRecordKind::NamespacePage => {
                    SnapshotMetadataRecordId::NamespacePage(NamespacePageId::new(
                        record.summary.logical_id.clone(),
                    ))
                }
                bowline_local::sync::namespace::MetadataRecordKind::ContentLayout => {
                    SnapshotMetadataRecordId::ContentLayout(ContentLayoutId::new(
                        record.summary.logical_id.clone(),
                    ))
                }
                bowline_local::sync::namespace::MetadataRecordKind::SegmentPage => {
                    SnapshotMetadataRecordId::SegmentPage(SegmentPageId::new(
                        record.summary.logical_id.clone(),
                    ))
                }
            };
            let sealed = seal_snapshot_metadata_page(
                &snapshot.metadata.workspace_id,
                logical_id,
                &record.plaintext,
                StorageKey::deterministic(112),
                1,
            )?;
            sealed_bytes = sealed_bytes.saturating_add(sealed.pointer.byte_len);
            hosted_document_bytes = hosted_document_bytes.saturating_add(
                serde_json::to_vec(&serde_json::json!({
                    "logicalId": record.summary.logical_id,
                    "recordKind": record_kind,
                    "object": {
                        "objectKey": format!("metadata_mp_{}", "0".repeat(64)),
                        "kind": "snapshot-metadata-page",
                        "byteLen": sealed.pointer.byte_len,
                        "hash": "0".repeat(64),
                        "keyEpoch": 1,
                    },
                    "sidecar": {
                        "childLogicalIds": record.summary.child_logical_ids,
                        "directObjectKeys": record.summary.direct_pack_ids,
                        "digest": "0".repeat(64),
                    },
                }))?
                .len() as u64,
            );
            Ok(())
        },
    )?;
    Ok((sealed_bytes, hosted_document_bytes))
}

fn measured_prefix(entry_count: usize, shape: &str) -> String {
    match shape {
        "shallow-wide" => "file-000000000".to_string(),
        "deep-tree" => {
            "root/d00/d01/d02/d03/d04/d05/d06/d07/d08/d09/d10/d11/d12/d13/d14".to_string()
        }
        "mixed-repository" => format!("project-{:04}", (entry_count / 2 + 10) % 1_000),
        _ => unreachable!("parameter validation pins shapes"),
    }
}

fn expected_snapshot_id(entry_count: usize, shape: &str) -> &'static str {
    match (entry_count, shape) {
        (10_000, "shallow-wide") => "snap_2200afde69773f953a96f814",
        (10_000, "deep-tree") => "snap_9cc430c3da35da3505682fd2",
        (10_000, "mixed-repository") => "snap_6f0cbb81ccbe3c02d0b2b241",
        (100_000, "shallow-wide") => "snap_89b07fdca2a8c1230928d401",
        (100_000, "deep-tree") => "snap_089b9add40bc88fcc09c887a",
        (100_000, "mixed-repository") => "snap_0100a51302175675af324586",
        (1_000_000, "shallow-wide") => "snap_4c5661032a784e00f9a855b3",
        (1_000_000, "deep-tree") => "snap_9b4c205c54f2caa977afa450",
        (1_000_000, "mixed-repository") => "snap_d2419d8440b403f82db2de54",
        _ => unreachable!("parameter validation pins shape/count pairs"),
    }
}

fn flat_plain_json_bytes(entry_count: usize, shape: &str) -> u64 {
    match (entry_count, shape) {
        (10_000, "shallow-wide") => 2_436_950,
        (10_000, "deep-tree") => 3_086_947,
        (10_000, "mixed-repository") => 2_692_993,
        (100_000, "shallow-wide") => 24_456_136,
        (100_000, "deep-tree") => 30_956_133,
        (100_000, "mixed-repository") => 27_007_557,
        (1_000_000, "shallow-wide") => 244_603_571,
        (1_000_000, "deep-tree") => 309_603_568,
        (1_000_000, "mixed-repository") => 270_113_250,
        _ => unreachable!("parameter validation pins shape/count pairs"),
    }
}

fn incremental_no_op(
    snapshot: &BuiltPagedNamespaceSnapshot,
    entry_count: u64,
) -> Result<ChangedPageSummary, Box<dyn Error>> {
    let mut context = operation_context(entry_count);
    let builder = PageNamespaceBuilder::incremental(snapshot, &mut context)?;
    Ok(builder.finish(&mut context)?.changed)
}

fn incremental_one_edit(
    snapshot: &BuiltPagedNamespaceSnapshot,
    entry_count: usize,
    shape: &str,
) -> Result<ChangedPageSummary, Box<dyn Error>> {
    let edit_index = entry_count / 2 + 10;
    let (path, kind) = path_and_kind(edit_index, shape);
    let mut replacement = entry(path, kind, edit_index ^ 0xa11c_e55e);
    replacement.byte_len = replacement.byte_len.map(|length| length.saturating_add(1));
    let mut context = operation_context(entry_count as u64);
    let mut builder = PageNamespaceBuilder::incremental(snapshot, &mut context)?;
    builder.apply(NamespaceMutation::Upsert(replacement), &mut context)?;
    Ok(builder.finish(&mut context)?.changed)
}

#[derive(Debug)]
struct ScenarioMeasurement {
    name: &'static str,
    entry_count: usize,
    mutations: usize,
    changed: ChangedPageSummary,
    differences: u64,
    diff_pages: u64,
    diff_bytes: u64,
}

fn measure_change_scenarios() -> Result<Vec<ScenarioMeasurement>, Box<dyn Error>> {
    const SCENARIO_ENTRIES: usize = 100_000;
    let shallow = build_scenario_base(SCENARIO_ENTRIES, "shallow-wide")?;
    let deep = build_scenario_base(SCENARIO_ENTRIES, "deep-tree")?;
    let mixed = build_scenario_base(SCENARIO_ENTRIES, "mixed-repository")?;

    let (root_path, root_kind) = path_and_kind(0, "shallow-wide");
    let (deep_path, deep_kind) = path_and_kind(SCENARIO_ENTRIES / 2, "deep-tree");
    let churn = (0..SCENARIO_ENTRIES)
        .step_by(100)
        .map(|index| {
            let (path, kind) = path_and_kind(index, "mixed-repository");
            NamespaceMutation::Upsert(changed_entry(path, kind, index))
        })
        .collect();
    let rename_index = SCENARIO_ENTRIES / 2 + 10;
    let (rename_path, rename_kind) = path_and_kind(rename_index, "mixed-repository");
    let mut renamed = entry(format!("{rename_path}-renamed"), rename_kind, rename_index);
    renamed.byte_len = renamed.byte_len.map(|length| length.saturating_add(1));

    let subtree_entries = (0..SCENARIO_ENTRIES)
        .map(|index| {
            entry(
                format!("subtree-{:02}/entry-{index:09}", index % 100),
                NamespaceEntryKind::File,
                index,
            )
        })
        .collect();
    let mut subtree_context = operation_context(SCENARIO_ENTRIES as u64);
    let subtree = PageNamespaceBuilder::full(
        metadata("scenario-subtree-delete"),
        identity_key(),
        subtree_entries,
        &mut subtree_context,
    )?;

    Ok(vec![
        measure_scenario(
            "root-file edit",
            &shallow,
            vec![NamespaceMutation::Upsert(changed_entry(
                root_path, root_kind, 0,
            ))],
        )?,
        measure_scenario(
            "deep-file edit",
            &deep,
            vec![NamespaceMutation::Upsert(changed_entry(
                deep_path,
                deep_kind,
                SCENARIO_ENTRIES / 2,
            ))],
        )?,
        measure_scenario("1% churn", &mixed, churn)?,
        measure_scenario(
            "rename",
            &mixed,
            vec![
                NamespaceMutation::Remove(WorkspaceRelativePath::new(rename_path)),
                NamespaceMutation::Upsert(renamed),
            ],
        )?,
        measure_scenario(
            "1% subtree deletion",
            &subtree,
            vec![NamespaceMutation::RemovePrefix(WorkspaceRelativePath::new(
                "subtree-00",
            ))],
        )?,
    ])
}

fn build_scenario_base(
    entry_count: usize,
    shape: &'static str,
) -> Result<BuiltPagedNamespaceSnapshot, Box<dyn Error>> {
    let mut context = operation_context(entry_count as u64);
    Ok(PageNamespaceBuilder::full(
        metadata(shape),
        identity_key(),
        generate_entries(entry_count, shape),
        &mut context,
    )?)
}

fn changed_entry(path: String, kind: NamespaceEntryKind, index: usize) -> NamespaceEntry {
    let mut changed = entry(path, kind, index ^ 0xa11c_e55e);
    changed.byte_len = changed.byte_len.map(|length| length.saturating_add(1));
    changed
}

fn measure_scenario(
    name: &'static str,
    base: &BuiltPagedNamespaceSnapshot,
    mutations: Vec<NamespaceMutation>,
) -> Result<ScenarioMeasurement, Box<dyn Error>> {
    let mut build_context = operation_context(base.metadata.entry_count);
    let mut builder = PageNamespaceBuilder::incremental(base, &mut build_context)?;
    let mutation_count = mutations.len();
    for mutation in mutations {
        builder.apply(mutation, &mut build_context)?;
    }
    let changed_snapshot = builder.finish(&mut build_context)?;
    let mut diff_context = operation_context(base.metadata.entry_count);
    let mut differences = CountDiffs::default();
    PageNamespaceReader::new(base).diff_paged(
        &PageNamespaceReader::new(&changed_snapshot),
        &NamespaceScope::All,
        &mut differences,
        &mut diff_context,
    )?;
    let counters = diff_context.counters();
    Ok(ScenarioMeasurement {
        name,
        entry_count: base.metadata.entry_count as usize,
        mutations: mutation_count,
        changed: changed_snapshot.changed,
        differences: differences.0,
        diff_pages: counters.namespace_pages_loaded,
        diff_bytes: counters.metadata_bytes,
    })
}

#[derive(Default)]
struct CountEntries(u64);

impl EntryVisitor for CountEntries {
    fn visit(
        &mut self,
        _entry: &NamespaceEntry,
        _context: &mut NamespaceOperationContext<'_>,
    ) -> Result<NamespaceVisitControl, NamespaceReadError> {
        self.0 = self.0.saturating_add(1);
        Ok(NamespaceVisitControl::Continue)
    }
}

#[derive(Default)]
struct CountDiffs(u64);

impl NamespaceDiffVisitor for CountDiffs {
    fn visit(&mut self, _difference: NamespaceDiff) -> Result<(), NamespaceReadError> {
        self.0 = self.0.saturating_add(1);
        Ok(())
    }
}

#[derive(Debug)]
struct LayoutMeasurement {
    segments: u32,
    layout_records: u64,
    segment_pages: u64,
    encoded_bytes: u64,
    range_records_loaded: u64,
    range_pages_loaded: u64,
    range_bytes: u64,
    selected_segments: usize,
}

fn measure_layout(segments: u32) -> Result<LayoutMeasurement, Box<dyn Error>> {
    let mut context = operation_context(1);
    let snapshot = PageNamespaceBuilder::full(
        metadata("layout-profile"),
        identity_key(),
        vec![layout_entry(segments)],
        &mut context,
    )?;
    let reader = PageNamespaceReader::new(&snapshot);
    let mut descriptor_context = operation_context(1);
    let descriptor = reader
        .descriptor(
            &WorkspaceRelativePath::new(format!("layout-{segments}.bin")),
            &mut descriptor_context,
        )?
        .ok_or("layout descriptor was absent")?;
    let layout_id = descriptor
        .content_layout_id
        .ok_or("layout descriptor did not contain a layout ID")?;
    let mut range_context = operation_context(1);
    let selected = reader.content_range(
        &layout_id,
        u64::from(segments - 1) * 1_024 + 10,
        20,
        &mut range_context,
    )?;
    let range = range_context.counters();
    Ok(LayoutMeasurement {
        segments,
        layout_records: snapshot.store.content_layout_count(),
        segment_pages: snapshot.store.segment_page_count(),
        encoded_bytes: snapshot.store.total_encoded_bytes(),
        range_records_loaded: range.layout_records_loaded,
        range_pages_loaded: range.segment_pages_loaded,
        range_bytes: range.metadata_bytes,
        selected_segments: selected.len(),
    })
}

fn operation_context(entry_count: u64) -> NamespaceOperationContext<'static> {
    let entry_limit = entry_count.saturating_mul(12).saturating_add(100);
    NamespaceOperationContext::uncancelled(
        NamespaceOperationBudget::new(entry_limit, entry_limit, entry_limit).with_metadata_limits(
            u64::MAX,
            u64::MAX,
            u64::MAX,
            u64::MAX,
        ),
    )
}

fn metadata(shape: &str) -> SnapshotMetadata {
    SnapshotMetadata {
        schema_version: 1,
        snapshot_id: SnapshotId::new("pending"),
        workspace_id: WorkspaceId::new(format!("ws_namespace_{shape}")),
        project_id: None,
        kind: SnapshotKind::WorkspaceHead,
        base_snapshot_id: None,
        semantic_manifest_digest: ManifestDigest::new("pending"),
        entry_count: 0,
        refs: Vec::new(),
    }
}

fn generate_entries(count: usize, shape: &str) -> Vec<NamespaceEntry> {
    (0..count)
        .map(|index| {
            let (path, kind) = path_and_kind(index, shape);
            entry(path, kind, index)
        })
        .collect()
}

fn path_and_kind(index: usize, shape: &str) -> (String, NamespaceEntryKind) {
    match shape {
        "shallow-wide" => (format!("file-{index:09}"), NamespaceEntryKind::File),
        "deep-tree" => (
            format!(
                "root/d00/d01/d02/d03/d04/d05/d06/d07/d08/d09/d10/d11/d12/d13/d14/file-{index:09}"
            ),
            NamespaceEntryKind::File,
        ),
        "mixed-repository" => {
            let kind = match index % 100 {
                0..=4 => NamespaceEntryKind::Directory,
                5..=7 => NamespaceEntryKind::Symlink,
                8 => NamespaceEntryKind::Placeholder,
                9 => NamespaceEntryKind::Tombstone,
                _ => NamespaceEntryKind::File,
            };
            (
                format!(
                    "project-{:04}/src/module-{:03}/entry-{index:09}",
                    index % 1_000,
                    index % 100
                ),
                kind,
            )
        }
        _ => unreachable!("parameter validation pins shapes"),
    }
}

fn entry(path: String, kind: NamespaceEntryKind, index: usize) -> NamespaceEntry {
    let has_content = kind == NamespaceEntryKind::File;
    let seeded_index = index as u64 ^ 0x5eed_112a;
    NamespaceEntry {
        path,
        kind,
        classification: if index.is_multiple_of(20) {
            PathClassification::ProjectEnv
        } else {
            PathClassification::WorkspaceSync
        },
        mode: if index.is_multiple_of(20) {
            MaterializationMode::ProjectEnv
        } else {
            MaterializationMode::EncryptedSync
        },
        access: vec![AccessFlag::HumanReadable, AccessFlag::AgentReadable],
        content_id: has_content.then(|| ContentId::new(format!("cid_{seeded_index:016x}"))),
        content_layout: None,
        symlink_target: (kind == NamespaceEntryKind::Symlink)
            .then(|| format!("../target-{index:09}")),
        byte_len: has_content.then_some((index % 65_536) as u64),
        executability: if has_content && index.is_multiple_of(37) {
            FileExecutability::Executable
        } else {
            FileExecutability::Regular
        },
        hydration_state: HydrationState::Local,
    }
}

fn layout_entry(segment_count: u32) -> NamespaceEntry {
    let segments = (0..segment_count)
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
    let mut value = entry(
        format!("layout-{segment_count}.bin"),
        NamespaceEntryKind::File,
        segment_count as usize,
    );
    value.byte_len = Some(u64::from(segment_count) * 1_024);
    value.content_layout = Some(ContentLayout::SegmentedV1 {
        logical_content_id: value.content_id.clone().expect("file content ID"),
        logical_length: u64::from(segment_count) * 1_024,
        segment_size: 1_024,
        segments,
    });
    value
}

fn render_report(
    shapes: &[ShapeMeasurement],
    layouts: &[LayoutMeasurement],
    scenarios: &[ScenarioMeasurement],
) -> Result<String, std::fmt::Error> {
    let mut report = String::new();
    writeln!(
        report,
        "# Plan 112 production namespace page-engine corpus remeasurement v1\n"
    )?;
    writeln!(report, "Generated with:\n\n```text")?;
    writeln!(
        report,
        "CARGO_TARGET_DIR=/tmp/bowline-dev-target RUSTFLAGS='-C metadata=p112-page-authority' cargo run -q -p bowline-testkit --example namespace_page_baseline"
    )?;
    writeln!(report, "```\n")?;
    writeln!(
        report,
        "The paired measurement consumes `tests/corpora/namespace-baseline-v1.json` (BLAKE3 `{}`). It uses the same generator-v1 paths and entries as the Phase-0 flat baseline, then builds the canonical page graph used by the production snapshot authority. Wall-clock and process RSS remain excluded because they are machine-specific; the report includes a deterministic resident-byte proxy enforced by production budgets.\n",
        stable_parameter_digest(PARAMETERS)
    )?;
    writeln!(
        report,
        "Semantic identity parity still hashes the full canonically ordered entry set. The created/reused page counts below measure deterministic structural reuse; they are not a claim that semantic digest or `SnapshotId` formation is proportional to changed pages.\n"
    )?;
    writeln!(report, "## Canonical page graphs and exact lookup\n")?;
    writeln!(
        report,
        "| Entries | Shape | Namespace pages | Canonical bytes | Sealed/upload bytes | Cold full-download bytes | Exact pages | Exact bytes | Root page ID | Manifest digest | Snapshot ID |"
    )?;
    writeln!(
        report,
        "| ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | --- | --- | --- |"
    )?;
    for row in shapes {
        writeln!(
            report,
            "| {} | {} | {} | {} | {} | {} | {} | {} | `{}` | `{}` | `{}` |",
            row.entry_count,
            row.shape,
            row.namespace_pages,
            row.encoded_bytes,
            row.sealed_bytes,
            row.sealed_bytes,
            row.lookup_pages,
            row.lookup_bytes,
            row.root_id,
            row.manifest_digest,
            row.snapshot_id
        )?;
    }
    writeln!(
        report,
        "\nEach exact probe targets the deterministic entry at `N / 2 + 10`; page and byte counters come from a fresh bounded descriptor lookup and exclude layout hydration. The semantic IDs match the flat Phase-0 rows for every corresponding corpus.\n"
    )?;
    render_hosted_profiles(&mut report, shapes)?;
    render_prefix_profiles(&mut report, shapes)?;
    writeln!(report, "## Incremental structural reuse\n")?;
    writeln!(
        report,
        "| Entries | Shape | No-op created | No-op reused | No-op pages loaded | No-op pages encoded | One-edit created | One-edit reused | One-edit removed | One-edit pages loaded | One-edit pages encoded | One-edit entries hashed | One-edit bytes created |"
    )?;
    writeln!(
        report,
        "| ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |"
    )?;
    for row in shapes {
        writeln!(
            report,
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            row.entry_count,
            row.shape,
            row.no_op.namespace_pages_created,
            row.no_op.namespace_pages_reused,
            row.no_op.namespace_pages_loaded_during_build,
            row.no_op.namespace_pages_encoded,
            row.one_edit.namespace_pages_created,
            row.one_edit.namespace_pages_reused,
            row.one_edit.namespace_pages_removed,
            row.one_edit.namespace_pages_loaded_during_build,
            row.one_edit.namespace_pages_encoded,
            row.one_edit.semantic_entries_hashed,
            row.one_edit.metadata_bytes_created
        )?;
    }
    writeln!(
        report,
        "\nNo-op and one-edit rows use the persistent `PageNamespaceBuilder::incremental` overlay. Created/reused counts describe the reachable logical graph; loaded/encoded/hash counters separately disclose work performed. No-op reuses the root without loading or encoding pages. A one-edit run rewrites only the affected radix path, while exact legacy semantic identity parity still hashes the full ordered entry stream. Content-layout and segment-page created/reused counts are zero for this namespace-only corpus.\n"
    )?;
    writeln!(
        report,
        "## Representative change and streamed-diff corpus\n"
    )?;
    writeln!(
        report,
        "| Scenario | Entries | Mutations | Differences | Created pages | Reused pages | Removed pages | Build pages loaded | Pages encoded | Entries hashed | Bytes created | Diff pages | Diff bytes | Cold SQLite fetches | Cold hosted binding records |"
    )?;
    writeln!(
        report,
        "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |"
    )?;
    for row in scenarios {
        writeln!(
            report,
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            row.name,
            row.entry_count,
            row.mutations,
            row.differences,
            row.changed.namespace_pages_created,
            row.changed.namespace_pages_reused,
            row.changed.namespace_pages_removed,
            row.changed.namespace_pages_loaded_during_build,
            row.changed.namespace_pages_encoded,
            row.changed.semantic_entries_hashed,
            row.changed.metadata_bytes_created,
            row.diff_pages,
            row.diff_bytes,
            row.diff_pages,
            row.diff_pages
        )?;
    }
    writeln!(
        report,
        "\nThese deterministic 100,000-entry representatives cover the Phase-0 mutation families: a root leaf edit, a 17-component deep leaf edit, 1% churn, a remove-plus-upsert rename, and a single component-aware `RemovePrefix` deleting exactly one of 100 equal subtrees. Diff rows use the page-aware streaming fast path: equal logical page IDs prune without entry materialization, structural mismatches stream entries through bounded point probes, and the operation never collects a complete diff vector. `Differences` is emitted semantic records; `Diff pages` and `Diff bytes` are the separately charged metadata reads.\n"
    )?;
    writeln!(report, "## Before/after metadata write amplification\n")?;
    writeln!(
        report,
        "| Entries | Shape | Flat JSON bytes before | Full page graph bytes after | One-edit page bytes after | One-edit bytes avoided vs flat JSON |"
    )?;
    writeln!(report, "| ---: | --- | ---: | ---: | ---: | ---: |")?;
    for row in shapes {
        let before = flat_plain_json_bytes(row.entry_count, row.shape);
        writeln!(
            report,
            "| {} | {} | {} | {} | {} | {} |",
            row.entry_count,
            row.shape,
            before,
            row.encoded_bytes,
            row.one_edit.metadata_bytes_created,
            before.saturating_sub(row.one_edit.metadata_bytes_created)
        )?;
    }
    writeln!(
        report,
        "\nThe before column is the exact complete-manifest JSON measurement from `namespace-flat-baseline-v1.md`; the after columns are canonical plaintext metadata-page bytes. They expose write-amplification direction but are not ciphertext, SQLite, latency, RSS, or network measurements. Full page-graph bytes include canonical minimum-size padding; one-edit bytes include only newly reachable pages and exclude reused pages. Semantic identity formation remains O(N), as disclosed by `One-edit entries hashed`, so these rows do not claim O(changed-page) end-to-end CPU.\n"
    )?;
    writeln!(report, "## Deterministic resident-byte proxy\n")?;
    writeln!(
        report,
        "| Entries | Shape | Flat complete-manifest bytes | Full canonical page bytes | Bounded exact-read bytes | Full-page bytes avoided |"
    )?;
    writeln!(report, "| ---: | --- | ---: | ---: | ---: | ---: |")?;
    for row in shapes {
        let flat = flat_plain_json_bytes(row.entry_count, row.shape);
        writeln!(
            report,
            "| {} | {} | {} | {} | {} | {} |",
            row.entry_count,
            row.shape,
            flat,
            row.encoded_bytes,
            row.lookup_bytes,
            flat.saturating_sub(row.encoded_bytes)
        )?;
    }
    writeln!(
        report,
        "\nPeak RSS is not stable across allocators, build profiles, kernels, or concurrent agents, so the committed reproducible proof uses resident canonical-byte demand instead of a machine-specific process peak. The flat proxy is the complete manifest allocation required before parsing/indexing; the page full-build proxy is the sum of immutable canonical records; and the exact-read proxy is the operation counter's actual loaded metadata bytes. This deliberately excludes allocator/index overhead and therefore does not claim process RSS, but it is deterministic, corpus-wide, and enforced by the same byte/page budgets used in production.\n"
    )?;
    writeln!(report, "## Retained-history page growth\n")?;
    writeln!(
        report,
        "| Shape (1m entries) | Depth | Flat SQLite bytes before | Unique page records after | Canonical page bytes after | Modeled SQLite bytes after | Root rows |"
    )?;
    writeln!(report, "| --- | ---: | ---: | ---: | ---: | ---: | ---: |")?;
    for row in shapes.iter().filter(|row| row.entry_count == 1_000_000) {
        for depth in [10_u64, 100, 1_000] {
            let page_records = row.namespace_pages.saturating_add(
                row.one_edit
                    .namespace_pages_created
                    .saturating_mul(depth.saturating_sub(1)),
            );
            let page_bytes = row.encoded_bytes.saturating_add(
                row.one_edit
                    .metadata_bytes_created
                    .saturating_mul(depth.saturating_sub(1)),
            );
            writeln!(
                report,
                "| {} | {} | {} | {} | {} | {} | {} |",
                row.shape,
                depth,
                flat_history_sqlite_bytes(row.shape, depth),
                page_records,
                page_bytes,
                modeled_sqlite_bytes(page_records, depth),
                depth
            )?;
        }
    }
    writeln!(
        report,
        "\nAs in the Phase-0 history table, depths replay the measured one-edit delta instead of allocating 1,000 simultaneous million-entry snapshots. The before column is the frozen flat SQLite-file measurement multiplied by depth. The after graph stores the measured base records once, adds only the measured changed path per later root, and keeps one small snapshot-root row per retained snapshot. The SQLite model comes from a reproducible 4 KiB-page SQLite profile of the production-shaped metadata-record, binding, cache, edge, snapshot, and snapshot-root rows: empty schema 73,728 bytes; 10,000 page/binding/cache rows plus 9,999 edges 16,744,448 bytes; adding 1,000 snapshot/root pairs 17,387,520 bytes. Arithmetic uses the measured 1,667.072 bytes per page record and 643.072 bytes per root pair, plus the schema floor. Production retention tests separately prove pins, roots, bounded mark/sweep, and deletion finalization.\n"
    )?;
    writeln!(report, "## Cache admission and eviction bounds\n")?;
    writeln!(
        report,
        "The operation-local verified remote metadata cache has named hard limits of 1,000,000 records and 4 GiB canonical bytes. Admission is deterministic, content-addressed, and FIFO-evicts the oldest admitted record before either limit is exceeded; one record larger than the byte budget is a typed `OversizedRecord`. Production local metadata-page admission uses the same 1,000,000-record / 4 GiB per-snapshot bounds before writing plaintext cache files. Unreachable on-disk cache files are removed by the bounded, restart-safe metadata mark/sweep path; live logical metadata records remain authoritative and are never deleted merely to satisfy a cache preference. Unit tests cover remote count/byte eviction, local count/byte rejection, cache-root confinement, missing-file idempotency, and crash/restart finalization.\n"
    )?;
    writeln!(report, "## Plan 058 post-cutover re-evaluation\n")?;
    writeln!(
        report,
        "Plan 112 removes whole-namespace metadata rewrite/upload amplification: a single deterministic edit creates only the affected 4-6 page path in this corpus, while no-op creates and encodes zero pages. This removes namespace metadata cost as a confounder from the next Plan 058 trigger review. The corpus does not measure changed-file byte ratio, source-pack upload bytes, signed range requests, preparation latency, hydration latency, peak memory, or production-like large mutable files. None of Plan 058's CDC promotion triggers is therefore established by this remeasurement. Plan 058 remains parked; its next trigger-validation corpus must measure those file-byte and runtime signals on top of the production page authority.\n"
    )?;
    writeln!(report, "## Content-layout range profiles\n")?;
    writeln!(
        report,
        "| Segments | Layout records | Stored segment pages | Total graph bytes | Range layout records | Range segment pages | Range metadata bytes | Segments returned |"
    )?;
    writeln!(
        report,
        "| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |"
    )?;
    for row in layouts {
        writeln!(
            report,
            "| {} | {} | {} | {} | {} | {} | {} | {} |",
            row.segments,
            row.layout_records,
            row.segment_pages,
            row.encoded_bytes,
            row.range_records_loaded,
            row.range_pages_loaded,
            row.range_bytes,
            row.selected_segments
        )?;
    }
    writeln!(
        report,
        "\nEach range probe requests 20 logical bytes starting 10 bytes into the final segment. It uses a fresh operation context, so the reported record, page, and byte counts describe only layout/range resolution. The 1-segment layout is inline; larger layouts use bounded segment pages when their canonical layout record exceeds the inline threshold. The reproducible measurement maximum remains 1,024 segments to match the frozen Phase-0 corpus. The production maximum is the finite `MAX_SEGMENTS_PER_LAYOUT = 1,000,000`; a boundary test accepts that count and rejects 1,000,001 without allocating either vector, while canonical page byte limits and operation budgets bound actual construction and reads.\n"
    )?;
    Ok(report)
}

fn render_hosted_profiles(
    report: &mut String,
    shapes: &[ShapeMeasurement],
) -> Result<(), std::fmt::Error> {
    writeln!(report, "## Hosted commit, import, and retention payloads\n")?;
    writeln!(
        report,
        "| Entries | Shape | Binding documents | Contract-shaped document bytes | Binding commit calls | Metadata objects | Sealed upload bytes | Cold binding-call range | Download intents / object GETs | Live-retention docs read | Live-retention payload bytes read |"
    )?;
    writeln!(
        report,
        "| ---: | --- | ---: | ---: | ---: | ---: | ---: | --- | ---: | ---: | ---: |"
    )?;
    for row in shapes {
        let minimum_binding_calls = row.namespace_pages.div_ceil(16);
        writeln!(
            report,
            "| {} | {} | {} | {} | {} | {} | {} | {}-{} | {} | {} | {} |",
            row.entry_count,
            row.shape,
            row.namespace_pages,
            row.hosted_document_bytes,
            minimum_binding_calls,
            row.namespace_pages,
            row.sealed_bytes,
            minimum_binding_calls,
            row.namespace_pages,
            row.namespace_pages,
            row.namespace_pages + 1,
            row.hosted_document_bytes + 512
        )?;
    }
    writeln!(
        report,
        "\nBinding commits use the hard 16-document batch contract. `Contract-shaped document bytes` is a deterministic UTF-8 JSON payload proxy over every actual logical ID, record kind, dependency sidecar, opaque object pointer, byte length, hash, and key epoch; it excludes Convex system fields and storage-engine overhead. Cold binding calls range from the fully anticipated 16-record minimum to the exact-path worst case of one request per loaded record; signed intents and object GETs occur only for demanded records. Live retention reads one root plus every reachable binding/sidecar and writes zero binding documents when nothing is collected. Fault and contract tests separately enforce action/document limits, continuation cursors, root revalidation, and bounded delete batches.\n"
    )
}

fn render_prefix_profiles(
    report: &mut String,
    shapes: &[ShapeMeasurement],
) -> Result<(), std::fmt::Error> {
    writeln!(report, "## Prefix and cold-source request counters\n")?;
    writeln!(
        report,
        "| Entries | Shape | Prefix entries | Prefix pages | Prefix bytes | SQLite record fetches | Hosted binding records | Download intents | Object GETs |"
    )?;
    writeln!(
        report,
        "| ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |"
    )?;
    for row in shapes {
        writeln!(
            report,
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            row.entry_count,
            row.shape,
            row.prefix_entries,
            row.prefix_pages,
            row.prefix_bytes,
            row.prefix_pages,
            row.prefix_pages,
            row.prefix_pages,
            row.prefix_pages
        )?;
    }
    writeln!(
        report,
        "\nThe prefix is one exact leaf for shallow-wide, the shared 15-component directory for deep-tree, and the deterministic `project-NNNN` component for mixed-repository. `SQLite record fetches` maps one verified cache-row/file resolution to each cold page load. Hosted binding records equal uncached logical records; the remote source resolves anticipated siblings in deduplicated batches of at most 16, then issues one signed download intent and object GET only when traversal actually loads a record. The deterministic `sync_phase7::core::dirty_untracked_workspace_uploads_packed_snapshot_and_imports_structure_first` proof captures real fake-hosted requests, asserts every batch is within 1-16, and requires a multi-ID import batch. Exact lookup remains one path with no sibling download. All reads are capped by the same page/byte/cancellation budget, and warm operation-local records and bindings are deduplicated.\n"
    )
}

fn stable_parameter_digest(value: &str) -> String {
    blake3::hash(value.as_bytes()).to_hex()[..24].to_string()
}

fn flat_history_sqlite_bytes(shape: &str, depth: u64) -> u64 {
    let per_snapshot = match shape {
        "shallow-wide" => 244_854_784_u64,
        "deep-tree" => 309_915_648_u64,
        "mixed-repository" => 270_389_248_u64,
        _ => unreachable!("history table only renders frozen million-entry shapes"),
    };
    per_snapshot.saturating_mul(depth)
}

fn modeled_sqlite_bytes(page_records: u64, roots: u64) -> u64 {
    const SCHEMA_BYTES: f64 = 73_728.0;
    const BYTES_PER_PAGE_RECORD: f64 = 1_667.072;
    const BYTES_PER_ROOT_PAIR: f64 = 643.072;
    (SCHEMA_BYTES
        + page_records as f64 * BYTES_PER_PAGE_RECORD
        + roots as f64 * BYTES_PER_ROOT_PAIR)
        .round() as u64
}
