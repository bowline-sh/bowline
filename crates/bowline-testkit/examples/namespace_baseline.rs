use std::{fs, path::PathBuf, process};

use bowline_core::{
    ids::{ContentId, ManifestId, PackId, SnapshotId, WorkspaceId},
    namespace_snapshot::{NamespaceOperationBudget, NamespaceOperationContext},
    policy::{AccessFlag, MaterializationMode, PathClassification},
    workspace_graph::{
        ContentLayout, FileExecutability, HydrationState, NamespaceEntry, NamespaceEntryKind,
        RefKind, SegmentId, SegmentLocator, SnapshotKind, WorkspaceRef,
    },
};
use bowline_local::sync::namespace::semantic_manifest_identity_with_context;
use bowline_storage::{EnvelopeContext, ObjectKind, StorageKey, seal, workspace_id_hash};
use rusqlite::{Connection, params};
use serde::Serialize;
use serde_json::{Value, json};

const PARAMETERS: &str = include_str!("../../../tests/corpora/namespace-baseline-v1.json");

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let parameters: Value = serde_json::from_str(PARAMETERS)?;
    validate_parameters(&parameters)?;
    let mut measurements = Vec::new();
    for count in [10_000_usize, 100_000, 1_000_000] {
        for shape in ["shallow-wide", "deep-tree", "mixed-repository"] {
            measurements.push(measure_shape(count, shape)?);
        }
    }
    let report = json!({
        "reportVersion": 1,
        "generator": "cargo run -p bowline-testkit --example namespace_baseline",
        "parametersBlake3": stable_parameter_digest(PARAMETERS),
        "parameters": parameters,
        "determinism": {
            "wallClockTimingsIncluded": false,
            "peakResidentMemoryIncluded": false,
            "reason": "The committed baseline is machine-independent and gates semantic work counters. Environment-specific latency and RSS belong in an attached run, not identity evidence."
        },
        "flatFormat": {
            "physicalRepresentation": "single-json-snapshot-manifest",
            "namespacePagesLoaded": 0,
            "namespacePagesReused": 0,
            "changedPageReuseRatio": null,
            "metadataObjectsPerSnapshot": 1,
            "hostedDocumentBytes": null,
            "hostedDocumentBytesReason": "The current hosted object manifest includes pack-key reachability. That Plan 005 physical/hosted baseline is outside p112-a because this corpus contains no uploaded packs."
        },
        "measurements": measurements,
        "layoutProfiles": layout_profiles(),
    });
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn validate_parameters(parameters: &Value) -> Result<(), &'static str> {
    if parameters["generatorVersion"] != 1
        || parameters["seed"] != "0x5eed112a"
        || parameters["measurementMaximumSegments"] != 1_024
        || !parameters["productionMaximumSegments"].is_null()
    {
        return Err("namespace corpus parameters do not match generator v1");
    }
    Ok(())
}

fn measure_shape(count: usize, shape: &str) -> Result<Value, Box<dyn std::error::Error>> {
    let mut entries = generate_entries(count, shape);
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    let composition = composition(&entries);
    let workspace_id = WorkspaceId::new(format!("ws_namespace_{shape}"));
    let entry_count = entries.len() as u64;
    let mut context = NamespaceOperationContext::uncancelled(NamespaceOperationBudget::new(
        entry_count * 3,
        0,
        0,
    ));
    let identity = semantic_manifest_identity_with_context(&workspace_id, &entries, &mut context)?;
    let manifest = manifest(workspace_id, identity.snapshot_id().clone(), entries);
    let plaintext = serde_json::to_vec(&manifest)?;
    let manifest_id = ManifestId::new(format!("mf_{:024x}", count as u128 + shape.len() as u128));
    let sealed = seal(
        &plaintext,
        StorageKey::deterministic(0x5e),
        &EnvelopeContext {
            workspace_id_hash: workspace_id_hash(manifest.workspace_id.as_str()),
            object_kind: ObjectKind::SnapshotManifest,
            object_id: manifest_id.as_str().to_string(),
            record_id: manifest.snapshot_id.as_str().to_string(),
            key_epoch: 1,
            format_version: 1,
        },
    )?
    .into_bytes();
    let sqlite_bytes = sqlite_manifest_row_bytes(&manifest, plaintext)?;
    let plaintext_bytes = serde_json::to_vec(&manifest)?.len() as u64;
    let sealed_bytes = sealed.len() as u64;
    Ok(json!({
        "entryCount": count,
        "shape": shape,
        "composition": composition,
        "semanticIdentity": {
            "manifestDigest": identity.digest().as_str(),
            "snapshotId": identity.snapshot_id().as_str(),
            "chunksProduced": identity.chunk_count(),
            "entriesHashed": identity.entries_hashed(),
            "entriesVisited": context.counters().entries_visited,
            "cancellationChecks": context.counters().cancellation_checks,
            "digestBytes": 32
        },
        "flatMetadata": {
            "plaintextBytes": plaintext_bytes,
            "sealedBytes": sealed_bytes,
            "uploadedBytesPerSnapshot": sealed_bytes,
            "downloadedBytesPerColdOpen": sealed_bytes,
            "sqliteBytesForIsolatedManifestRow": sqlite_bytes,
            "objectsWritten": 1,
            "requestsToUpload": 1,
            "requestsForColdOpen": 1
        },
        "readerOperations": reader_operations(count as u64),
        "mutationScenarios": mutation_scenarios(count as u64, sealed_bytes),
        "retainedHistory": retained_history(sealed_bytes, sqlite_bytes)
    }))
}

fn generate_entries(count: usize, shape: &str) -> Vec<NamespaceEntry> {
    (0..count)
        .map(|index| {
            let (path, kind) = match shape {
                "shallow-wide" => (
                    format!("file-{index:09}"),
                    NamespaceEntryKind::File,
                ),
                "deep-tree" => (
                    format!(
                        "root/d00/d01/d02/d03/d04/d05/d06/d07/d08/d09/d10/d11/d12/d13/d14/file-{index:09}"
                    ),
                    NamespaceEntryKind::File,
                ),
                "mixed-repository" => mixed_path_and_kind(index),
                _ => unreachable!("parameter validation pins shapes"),
            };
            entry(path, kind, index)
        })
        .collect()
}

fn mixed_path_and_kind(index: usize) -> (String, NamespaceEntryKind) {
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

#[derive(Serialize)]
struct HistoricalFlatSnapshotManifest {
    schema_version: u16,
    snapshot_id: SnapshotId,
    workspace_id: WorkspaceId,
    project_id: Option<bowline_core::ids::ProjectId>,
    kind: SnapshotKind,
    base_snapshot_id: Option<SnapshotId>,
    entries: Vec<NamespaceEntry>,
    refs: Vec<WorkspaceRef>,
}

fn manifest(
    workspace_id: WorkspaceId,
    snapshot_id: SnapshotId,
    entries: Vec<NamespaceEntry>,
) -> HistoricalFlatSnapshotManifest {
    HistoricalFlatSnapshotManifest {
        schema_version: 4,
        snapshot_id: snapshot_id.clone(),
        workspace_id,
        project_id: None,
        kind: SnapshotKind::WorkspaceHead,
        base_snapshot_id: None,
        entries,
        refs: vec![WorkspaceRef {
            name: "workspace-head".to_string(),
            target_snapshot_id: snapshot_id,
            kind: RefKind::Workspace,
        }],
    }
}

fn composition(entries: &[NamespaceEntry]) -> Value {
    let mut kinds = [0_u64; 5];
    let mut total_path_bytes = 0_u64;
    let mut max_depth = 0_u64;
    for entry in entries {
        kinds[kind_index(entry.kind)] += 1;
        total_path_bytes += entry.path.len() as u64;
        max_depth = max_depth.max(entry.path.split('/').count() as u64);
    }
    json!({
        "directories": kinds[0],
        "files": kinds[1],
        "symlinks": kinds[2],
        "placeholders": kinds[3],
        "tombstones": kinds[4],
        "totalPathBytes": total_path_bytes,
        "maximumDepth": max_depth,
        "layoutRecords": 0,
        "locatorRecords": 0
    })
}

fn kind_index(kind: NamespaceEntryKind) -> usize {
    match kind {
        NamespaceEntryKind::Directory => 0,
        NamespaceEntryKind::File => 1,
        NamespaceEntryKind::Symlink => 2,
        NamespaceEntryKind::Placeholder => 3,
        NamespaceEntryKind::Tombstone => 4,
    }
}

fn reader_operations(entry_count: u64) -> Value {
    json!({
        "exactLookupHit": {
            "entriesVisited": 1,
            "diffEntriesVisited": 0,
            "cancellationChecks": 2
        },
        "exactLookupMiss": {
            "entriesVisited": 0,
            "diffEntriesVisited": 0,
            "cancellationChecks": 1
        },
        "fullPrefixVisit": {
            "entriesVisited": entry_count,
            "entriesEmitted": entry_count,
            "cancellationChecks": entry_count + 1
        },
        "equalSnapshotDiff": {
            "leftEntriesCompared": entry_count,
            "rightEntriesCompared": entry_count,
            "diffEntriesVisited": entry_count * 2,
            "entryReads": entry_count * 2,
            "changesEmitted": 0
        }
    })
}

fn mutation_scenarios(entry_count: u64, sealed_bytes: u64) -> Value {
    let one_percent = entry_count / 100;
    json!([
        scenario("no-op", 0, 0, entry_count, entry_count, sealed_bytes),
        scenario(
            "root-file-edit",
            1,
            1,
            entry_count,
            entry_count,
            sealed_bytes
        ),
        scenario(
            "deep-file-edit",
            1,
            1,
            entry_count,
            entry_count,
            sealed_bytes
        ),
        scenario(
            "one-percent-churn",
            one_percent,
            one_percent,
            entry_count,
            entry_count,
            sealed_bytes
        ),
        scenario("rename", 2, 2, entry_count, entry_count, sealed_bytes),
        scenario(
            "subtree-deletion",
            1,
            one_percent,
            entry_count - one_percent,
            entry_count - one_percent,
            sealed_bytes
        )
    ])
}

fn scenario(
    name: &str,
    mutations: u64,
    changed_entries: u64,
    final_entries: u64,
    entries_hashed: u64,
    baseline_sealed_bytes: u64,
) -> Value {
    json!({
        "name": name,
        "mutationsApplied": mutations,
        "changedEntries": changed_entries,
        "finalEntryCount": final_entries,
        "entriesHashed": entries_hashed,
        "namespacePagesLoaded": 0,
        "namespacePagesReused": 0,
        "metadataObjectsWritten": 1,
        "baselineSealedBytesRewritten": baseline_sealed_bytes
    })
}

fn retained_history(sealed_bytes: u64, sqlite_bytes: u64) -> Value {
    Value::Array(
        [10_u64, 100, 1_000]
            .into_iter()
            .map(|snapshots| {
                json!({
                    "snapshots": snapshots,
                    "sealedManifestBytes": sealed_bytes * snapshots,
                    "isolatedSqliteManifestRowBytes": sqlite_bytes * snapshots,
                    "metadataObjects": snapshots,
                    "sharedNamespacePages": 0
                })
            })
            .collect(),
    )
}

fn layout_profiles() -> Value {
    let profiles = [1_u32, 64, 1_024]
        .into_iter()
        .map(|segment_count| {
            let layout = segmented_layout(segment_count);
            json!({
                "segments": segment_count,
                "serializedLayoutBytes": serde_json::to_vec(&layout)
                    .expect("layout serializes")
                    .len(),
                "locatorRecordsVisitedByFlatOpen": segment_count,
                "layoutPagesLoaded": 0
            })
        })
        .collect();
    Value::Array(profiles)
}

fn segmented_layout(segment_count: u32) -> ContentLayout {
    let segments = (0..segment_count)
        .map(|ordinal| SegmentLocator {
            ordinal,
            plaintext_length: 4_194_304,
            segment_id: SegmentId::new(format!("seg_{ordinal:08}")),
            pack_id: PackId::new(format!("pack_{ordinal:08}")),
            offset: u64::from(ordinal) * 4_194_304,
            length: 4_194_304,
            format_version: 1,
        })
        .collect();
    ContentLayout::SegmentedV1 {
        logical_content_id: ContentId::new(format!("cid_segments_{segment_count}")),
        logical_length: u64::from(segment_count) * 4_194_304,
        segment_size: 4_194_304,
        segments,
    }
}

fn sqlite_manifest_row_bytes(
    manifest: &HistoricalFlatSnapshotManifest,
    plaintext: Vec<u8>,
) -> Result<u64, Box<dyn std::error::Error>> {
    let path = sqlite_measurement_path(manifest);
    let connection = Connection::open(&path)?;
    connection.execute_batch(
        "PRAGMA journal_mode=OFF;
         PRAGMA synchronous=OFF;
         CREATE TABLE workspace_snapshots (
           snapshot_id TEXT PRIMARY KEY,
           workspace_id TEXT NOT NULL,
           manifest_json TEXT NOT NULL
         );",
    )?;
    let manifest_json = String::from_utf8(plaintext)?;
    connection.execute(
        "INSERT INTO workspace_snapshots (snapshot_id, workspace_id, manifest_json)
         VALUES (?1, ?2, ?3)",
        params![
            manifest.snapshot_id.as_str(),
            manifest.workspace_id.as_str(),
            manifest_json
        ],
    )?;
    let page_count: u64 = connection.query_row("PRAGMA page_count", [], |row| row.get(0))?;
    let page_size: u64 = connection.query_row("PRAGMA page_size", [], |row| row.get(0))?;
    drop(connection);
    fs::remove_file(path)?;
    Ok(page_count * page_size)
}

fn sqlite_measurement_path(manifest: &HistoricalFlatSnapshotManifest) -> PathBuf {
    std::env::temp_dir().join(format!(
        "bowline-namespace-baseline-{}-{}.sqlite",
        process::id(),
        stable_parameter_digest(manifest.snapshot_id.as_str())
    ))
}

fn stable_parameter_digest(value: &str) -> String {
    blake3::hash(value.as_bytes()).to_hex()[..24].to_string()
}
