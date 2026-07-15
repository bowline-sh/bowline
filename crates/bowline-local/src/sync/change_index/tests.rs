use std::collections::{BTreeMap, BTreeSet};

use bowline_core::ids::{ContentId, WorkspaceId};

use super::*;
use crate::metadata::MetadataStore;
use crate::sync::stat_cache::{
    ContentKeyFingerprint, FileTimestampNanos, STAT_CACHE_FORMAT_VERSION, StatCacheDeleteScope,
    StatCacheRow, StatFingerprint,
};
use crate::sync::{FullScanReason, ScanScope};
use crate::workspace::TempWorkspace;

const WORKSPACE: &str = "ws_index";

fn workspace_id() -> WorkspaceId {
    WorkspaceId::new(WORKSPACE)
}

struct Fixture {
    _temp: TempWorkspace,
    store: MetadataStore,
}

impl Fixture {
    fn new(label: &str, paths: &[&str]) -> Self {
        let temp = TempWorkspace::new(label).expect("temp workspace");
        let db_path = temp.root().join(".state").join("local.sqlite3");
        let mut store = MetadataStore::open(&db_path).expect("metadata opens");
        store
            .insert_workspace(&workspace_id(), "Code", "2026-07-04T00:00:00Z")
            .expect("workspace");
        let rows: Vec<StatCacheRow> = paths.iter().map(|path| row(path)).collect();
        store
            .apply_stat_cache_write_back(&workspace_id(), &rows, &BTreeSet::new())
            .expect("write back");
        Self { _temp: temp, store }
    }

    fn index(&self) -> LocalChangeIndex<'_> {
        LocalChangeIndex::new(&self.store, workspace_id())
    }
}

fn row(path: &str) -> StatCacheRow {
    StatCacheRow {
        path: path.to_string(),
        fingerprint: StatFingerprint {
            size: 10,
            mtime_ns: FileTimestampNanos::new(11),
            ctime_ns: FileTimestampNanos::new(12),
            inode: 13,
            dev: 14,
            file_mode: 0o100644,
        },
        key_epoch: 1,
        content_key_fingerprint: ContentKeyFingerprint::new("0123456789abcdef".to_string()),
        content_id: ContentId::new("cid"),
        byte_len: 10,
        format_version: STAT_CACHE_FORMAT_VERSION,
        hashed_at_ns: FileTimestampNanos::new(20),
        last_verified_at: "2026-07-04T00:00:00Z".to_string(),
    }
}

fn as_vec(paths: &BTreeSet<String>) -> Vec<&str> {
    paths.iter().map(String::as_str).collect()
}

#[test]
fn root_level_and_under_roots_are_deterministic_from_the_same_fixture() {
    let fixture = Fixture::new(
        "change-index-contract",
        &[
            "README.md",
            "Cargo.toml",
            "app/src/main.rs",
            "app/src/lib.rs",
            "docs/guide.md",
        ],
    );

    let mut first = fixture.index();
    let mut second = fixture.index();
    let roots = BTreeSet::from(["app".to_string()]);

    assert_eq!(
        as_vec(&first.root_level_paths().expect("root level")),
        vec!["Cargo.toml", "README.md"]
    );
    assert_eq!(
        as_vec(&second.root_level_paths().expect("root level again")),
        vec!["Cargo.toml", "README.md"]
    );
    assert_eq!(
        as_vec(&first.paths_under_roots(&roots).expect("under roots")),
        vec!["app/src/lib.rs", "app/src/main.rs"]
    );
    assert_eq!(
        first.paths_under_roots(&roots).expect("under roots repeat"),
        second.paths_under_roots(&roots).expect("under roots again"),
    );
}

#[test]
fn cost_summary_records_loaded_consulted_and_pruned() {
    let fixture = Fixture::new(
        "change-index-cost",
        &["README.md", "stale.txt", "app/src/main.rs"],
    );
    let mut index = fixture.index();

    let root_level = index.root_level_paths().expect("root level");
    // A shallow pass observed only README.md; the stale root row would be pruned.
    let observed = BTreeSet::from(["README.md".to_string()]);
    let pruned =
        index.record_prune_preview(&root_level, &observed, StatCacheDeleteScope::RootLevelOnly);

    let cost = index.cost_summary();
    assert_eq!(cost.rows_returned, 2);
    assert_eq!(cost.rows_loaded, 2);
    assert_eq!(cost.rows_consulted, 2);
    assert_eq!(pruned, 1);
    assert_eq!(cost.rows_pruned, 1);
}

#[test]
fn delete_scope_for_maps_every_scan_scope() {
    let roots = BTreeSet::from(["src".to_string()]);
    assert_eq!(
        LocalChangeIndex::delete_scope_for(&ScanScope::Full(FullScanReason::CliRequested)),
        StatCacheDeleteScope::All
    );
    assert_eq!(
        LocalChangeIndex::delete_scope_for(&ScanScope::RootShallow),
        StatCacheDeleteScope::RootLevelOnly
    );
    assert_eq!(
        LocalChangeIndex::delete_scope_for(&ScanScope::DirtySubtrees {
            roots: roots.clone(),
            root_shallow: false,
        }),
        StatCacheDeleteScope::UnderRoots(&roots)
    );
    assert_eq!(
        LocalChangeIndex::delete_scope_for(&ScanScope::DirtySubtrees {
            roots: roots.clone(),
            root_shallow: true,
        }),
        StatCacheDeleteScope::UnderRootsAndRootLevel(&roots)
    );
}

#[test]
fn root_level_projection_stays_bounded_with_a_large_deep_index() {
    let mut paths: Vec<String> = vec![
        "README.md".to_string(),
        "Cargo.toml".to_string(),
        "LICENSE".to_string(),
    ];
    for index in 0..100_000_u32 {
        paths.push(format!("deep/dir{}/file{index}.rs", index % 256));
    }
    let path_refs: Vec<&str> = paths.iter().map(String::as_str).collect();
    let fixture = Fixture::new("change-index-budget", &path_refs);
    let mut index = fixture.index();

    let root_level = index.root_level_paths().expect("root level");

    // 100k deep rows do not change the cost of a root-level lookup.
    assert_eq!(
        as_vec(&root_level),
        vec!["Cargo.toml", "LICENSE", "README.md"]
    );
    let cost = index.cost_summary();
    assert_eq!(cost.rows_loaded, 3);
    assert_eq!(cost.rows_consulted, 3);
    assert!(!root_level.iter().any(|path| path.contains('/')));
}

#[test]
fn combined_projection_loads_only_root_and_under_roots() {
    let fixture = Fixture::new(
        "change-index-combined",
        &[
            "README.md",
            "src/app.rs",
            "src/lib.rs",
            "unrelated/deep/thing.rs",
        ],
    );
    let mut index = fixture.index();
    let roots = BTreeSet::from(["src".to_string()]);

    let root_level = index.root_level_paths().expect("root level");
    let under_roots = index.paths_under_roots(&roots).expect("under roots");

    assert_eq!(as_vec(&root_level), vec!["README.md"]);
    assert_eq!(as_vec(&under_roots), vec!["src/app.rs", "src/lib.rs"]);
    // The unrelated deep row is never consulted by either projection.
    assert!(
        !under_roots
            .iter()
            .any(|path| path.starts_with("unrelated/"))
    );
    assert!(!root_level.contains("unrelated/deep/thing.rs"));
}

#[test]
fn indexed_projection_proof_root_level_uses_index_not_full_scan() {
    let fixture = Fixture::new("change-index-proof", &["README.md", "app/src/main.rs"]);
    let plan = fixture
        .store
        .stat_cache_root_level_query_plan(&workspace_id())
        .expect("query plan");

    assert!(
        plan.iter()
            .any(|detail| detail.contains("idx_scan_stat_cache_root_level")),
        "root-level lookup must use the U7a index, plan was {plan:?}"
    );
    assert!(
        !plan.iter().any(|detail| detail.starts_with("SCAN")),
        "an instr(path,'/')-style full scan must fail this test, plan was {plan:?}"
    );
}

#[test]
fn directory_heavy_estimate_is_not_cheap_on_file_rows_alone() {
    // One file buried under six directories: few file rows, many directories.
    let fixture = Fixture::new(
        "change-index-dir-heavy",
        &[
            "deep/a/b/c/d/e/only.rs",
            "flat/f0.rs",
            "flat/f1.rs",
            "flat/f2.rs",
        ],
    );
    let mut index = fixture.index();

    let deep = index
        .estimated_subtree_entry_count("deep")
        .expect("deep estimate");
    let flat = index
        .estimated_subtree_entry_count("flat")
        .expect("flat estimate");

    // The directory-heavy subtree is not deemed cheap on its single file row.
    assert_eq!(deep.stat_cache_rows, 1);
    assert_eq!(deep.inferred_directory_entries, 5); // a, a/b, a/b/c, a/b/c/d, a/b/c/d/e
    assert!(deep.estimated_entries() > deep.stat_cache_rows);
    assert!(deep.estimated_entries() > flat.estimated_entries());

    // v1 limitation: empty directories are invisible, so counts are inexact and
    // the cost summary records that directory counts are unavailable.
    assert!(!deep.directory_counts_exact);
    assert!(!index.cost_summary().directory_counts_available);
}

#[test]
fn seeded_manifest_makes_directory_counts_exact() {
    let fixture = Fixture::new("change-index-manifest", &["pkg/src/main.rs"]);
    let manifest_paths = BTreeSet::from([
        "pkg".to_string(),
        "pkg/src".to_string(),
        "pkg/src/main.rs".to_string(),
    ]);
    let mut index =
        LocalChangeIndex::new(&fixture.store, workspace_id()).with_manifest_paths(manifest_paths);

    let estimate = index
        .estimated_subtree_entry_count("pkg")
        .expect("estimate");

    assert!(estimate.directory_counts_exact);
    assert_eq!(estimate.manifest_entries, 3);
    assert!(index.cost_summary().directory_counts_available);
}

#[test]
fn snapshot_captures_root_level_and_estimates_for_u9() {
    let fixture = Fixture::new(
        "change-index-snapshot",
        &["README.md", "src/app.rs", "src/lib.rs"],
    );
    let mut index = fixture.index();
    let roots = BTreeSet::from(["src".to_string()]);

    let snapshot = index.snapshot_for_roots(&roots).expect("snapshot");

    assert_eq!(as_vec(snapshot.root_level_paths()), vec!["README.md"]);
    assert_eq!(snapshot.estimated_subtree_entry_count("src"), Some(2));
    assert_eq!(snapshot.estimated_subtree_entry_count("missing"), None);
    assert!(snapshot.cost().rows_consulted >= 3);
}

#[test]
fn snapshot_from_parts_supports_fake_estimates() {
    let snapshot = ChangeIndexSnapshot::from_parts(
        BTreeSet::from(["README.md".to_string()]),
        BTreeMap::from([("huge".to_string(), 42_000_u64)]),
        ChangeIndexCost::default(),
    );

    assert_eq!(snapshot.estimated_subtree_entry_count("huge"), Some(42_000));
    assert_eq!(as_vec(snapshot.root_level_paths()), vec!["README.md"]);
}
