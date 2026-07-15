//! Plan 06 U10 — consolidated end-to-end acceptance fixtures for the local
//! change frontier. These tests tie the U6/U7 units together at realistic scale
//! in one fixture per scenario, rather than re-proving a single unit's contract.
//! They live in-crate (not in `tests/`) because the non-recursion proof depends
//! on the `crate::fs_access` traversal probe, which is a `#[cfg(test)]`
//! thread-local reachable only from in-crate unit tests.
//!
//! Scenarios already fully covered by a unit test are NOT duplicated here; a
//! pointer comment in [`pointers`] names the owning test instead. New tests here
//! cover the genuine end-to-end gaps: the flagship root-edit tie-together
//! (traversal + indexed consultation + preservation + redaction in one fixture),
//! the vanished-subtree prune through a real store/session round-trip, and the
//! combined ownership merge tied to the write-back scope.

use std::collections::BTreeSet;

use bowline_core::ids::{ContentId, WorkspaceId};

use crate::metadata::MetadataStore;
use crate::scanner::{
    merge_scoped_and_shallow_reports, scan_workspace_root_shallow, scan_workspace_scoped,
};
use crate::sync::ScanScope;
use crate::sync::change_index::LocalChangeIndex;
use crate::sync::observation_scope::ObservationWriteScope;
use crate::sync::stat_cache::{
    ContentKeyFingerprint, FileTimestampNanos, STAT_CACHE_FORMAT_VERSION, StatCacheDeleteScope,
    StatCacheRow, StatCacheSession, StatFingerprint,
};
use crate::workspace::TempWorkspace;

const WORKSPACE: &str = "ws_acceptance";
const KEY: [u8; 32] = [7; 32];

// Redaction canary substrings from the Plan 06 Security/privacy contract. No
// aggregate/status/cost surface asserted by these tests may contain them.
const SECRET_NEEDLES: [&str; 3] = [".env", "secrets/prod.key", "client/acme-payroll/keys.json"];

struct Fixture {
    temp: TempWorkspace,
    store: MetadataStore,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let temp = TempWorkspace::new(label).expect("temp workspace");
        let db_path = temp.root().join(".state").join("local.sqlite3");
        let store = MetadataStore::open(&db_path).expect("metadata opens");
        store
            .insert_workspace(&workspace_id(), "Code", "2026-07-04T00:00:00Z")
            .expect("workspace row");
        Self { temp, store }
    }
}

fn workspace_id() -> WorkspaceId {
    WorkspaceId::new(WORKSPACE)
}

fn row(path: &str, content_id: &str) -> StatCacheRow {
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
        content_id: ContentId::new(content_id),
        byte_len: 10,
        format_version: STAT_CACHE_FORMAT_VERSION,
        hashed_at_ns: FileTimestampNanos::new(20),
        last_verified_at: "2026-07-04T00:00:00Z".to_string(),
    }
}

/// Scenario 1 (flagship): a root `README.md` edit over a fixture with 100k deep
/// cached rows, a nested policy tree, a root Git repo, and deep secret-named
/// files. One tick must be bounded on every axis U6/U7 introduced:
/// no deep `read_dir` (traversal), root-level-only policy, O(root entries)
/// indexed consultation, deep stat-cache rows preserved (no re-hash), fresh
/// root-owned metadata, and no secret substrings on the aggregate surfaces.
#[test]
fn root_edit_over_100k_deep_index_stays_bounded_end_to_end() {
    let fixture = Fixture::new("acceptance-root-edit-100k");
    let root = fixture.temp.root().to_path_buf();

    // --- On-disk workspace: root files, a root Git repo, a nested policy tree,
    // and deep secret-named files a recursive walk would touch. ---
    fixture
        .temp
        .write_file("package.json", b"{}")
        .expect("root identity");
    fixture
        .temp
        .write_file("README.md", b"edited top\n")
        .expect("edited root file");
    fixture
        .temp
        .write_file(".env", b"TOKEN=shh\n")
        .expect("root env");
    fixture
        .temp
        .write_file(".bowlineignore", b"notes.txt\n")
        .expect("root policy");
    fixture
        .temp
        .write_file("notes.txt", b"local\n")
        .expect("root-policy target");
    let git = root.join(".git");
    std::fs::create_dir_all(git.join("refs/heads")).expect("git refs dir");
    std::fs::write(git.join("HEAD"), b"ref: refs/heads/main\n").expect("git HEAD");
    std::fs::write(git.join("config"), b"[core]\n").expect("git config");
    // A nested policy tree a recursive policy load would walk into.
    fixture
        .temp
        .write_file("deep/nested/.bowlineignore", b"secret/**\n")
        .expect("nested policy");
    fixture
        .temp
        .write_file("deep/nested/more/module.rs", b"fn deep() {}\n")
        .expect("deep module");
    // Deep secret-named files: present on disk but never surfaced by a shallow
    // tick; the redaction canary proves the aggregate surfaces stay clean.
    fixture
        .temp
        .write_file("secrets/prod.key", b"KEY\n")
        .expect("deep secret key");
    fixture
        .temp
        .write_file("client/acme-payroll/keys.json", b"{}\n")
        .expect("deep client secret");

    // --- Seeded stat cache: 100k deep rows plus root-level rows (one of which,
    // `stale-root.txt`, is not on disk so the shallow write-back prunes it), and
    // deep secret-path rows to prove the indexed projection never consults them. ---
    let mut rows = Vec::with_capacity(100_006);
    rows.push(row("README.md", "cid_readme"));
    rows.push(row("package.json", "cid_pkg"));
    rows.push(row(".env", "cid_env"));
    rows.push(row("stale-root.txt", "cid_stale"));
    rows.push(row("secrets/prod.key", "cid_secret_key"));
    rows.push(row("client/acme-payroll/keys.json", "cid_client_secret"));
    for i in 0..100_000_u32 {
        rows.push(row(&format!("deep/dir{}/file{i}.rs", i % 512), "cid_deep"));
    }
    let mut store = fixture.store;
    store
        .apply_stat_cache_write_back(&workspace_id(), &rows, &BTreeSet::new())
        .expect("seed stat cache");

    // --- Traversal proof: the whole shallow tick performs exactly one root
    // `read_dir` and never descends — no nested-policy walk, no recursive Git
    // untracked walk, no deep-subtree descent. ---
    let direct_children = std::fs::read_dir(&root).expect("root read").count() as u64;
    crate::fs_access::install(&root);
    let report = scan_workspace_root_shallow(&root).expect("root-shallow scan");
    let counts = crate::fs_access::take();
    assert_eq!(counts.root_read_dir_count, 1, "exactly one root read_dir");
    assert_eq!(
        counts.subdir_read_dir_count, 0,
        "no subdirectory read_dir (policy, Git, or subtree) during a root-shallow tick"
    );
    assert_eq!(
        counts.metadata_count, direct_children,
        "metadata calls bounded by the root's direct children"
    );
    assert!(
        !report.paths.iter().any(|path| path.path.contains('/')),
        "no nested path is observed by a shallow tick"
    );

    // Root-level policy was applied without walking the nested policy tree.
    let notes = report
        .paths
        .iter()
        .find(|path| path.path == "notes.txt")
        .expect("root-policy target observed");
    assert_eq!(
        serde_json::to_value(notes.policy.classification).expect("classification json"),
        "local-only",
        "root-level .bowlineignore was applied"
    );
    // Fresh root-owned project metadata: cheap identity/classification ran and
    // flagged the expensive Git health for a later refresh (not walked now).
    let root_project = report
        .projects
        .iter()
        .find(|project| project.path.is_empty())
        .expect("root project observed");
    assert!(
        root_project.has_git_repo,
        "cheap Git identity still updates"
    );
    assert!(
        root_project.health_refresh_needed,
        "expensive Git health is deferred, not walked"
    );

    // --- Indexed consultation proof: the root-level projection loads/consults
    // O(root entries), never the 100k deep rows or the deep secret rows. ---
    let mut index = LocalChangeIndex::new(&store, workspace_id());
    let root_level = index.root_level_paths().expect("root-level projection");
    assert_eq!(
        root_level.iter().map(String::as_str).collect::<Vec<_>>(),
        vec![".env", "README.md", "package.json", "stale-root.txt"],
        "only root-level rows are returned"
    );
    let cost = *index.cost_summary();
    assert_eq!(
        cost.rows_loaded, 4,
        "loads O(root entries), not O(all cached)"
    );
    assert_eq!(
        cost.rows_consulted, 4,
        "the index confines consultation to root-level rows"
    );

    // --- Preservation proof: a RootLevelOnly write-back keyed to the observed
    // root files prunes the vanished `stale-root.txt` while leaving all 100k deep
    // rows (and the deep secret rows) byte-for-byte intact — no full re-hash. ---
    let observed_root_files: BTreeSet<String> = report
        .paths
        .iter()
        .filter(|path| !path.is_dir && !path.path.contains('/'))
        .map(|path| path.path.clone())
        .collect();
    let mut session =
        StatCacheSession::load_root_level(&store, &workspace_id(), 1, &KEY).expect("root session");
    let write_back =
        session.finish_with_delete_scope(&observed_root_files, StatCacheDeleteScope::RootLevelOnly);
    store
        .apply_stat_cache_write_back(&workspace_id(), &write_back.upserts, &write_back.deletes)
        .expect("apply shallow write-back");

    let after = store.stat_cache_rows(&workspace_id()).expect("rows after");
    assert!(
        !after.contains_key("stale-root.txt"),
        "vanished root row pruned"
    );
    assert!(
        after.contains_key("README.md"),
        "observed root row preserved"
    );
    let deep_preserved = after.get("deep/dir0/file0.rs").expect("deep row preserved");
    assert_eq!(
        deep_preserved.content_id.as_str(),
        "cid_deep",
        "deep row is untouched, so the next tick is a cache hit, not a re-hash"
    );
    let deep_count = after
        .keys()
        .filter(|path| path.starts_with("deep/"))
        .count();
    assert_eq!(deep_count, 100_000, "no deep row was rewritten or dropped");

    // --- Redaction canary: the aggregate/status/cost surfaces carry only counts,
    // never a secret path. ---
    let summary_json = serde_json::to_string(&report.summary).expect("summary json");
    let cost_debug = format!("{cost:?}");
    for needle in SECRET_NEEDLES {
        assert!(
            !summary_json.contains(needle),
            "aggregate summary must not leak {needle}"
        );
        assert!(
            !cost_debug.contains(needle),
            "cost counters must not leak {needle}"
        );
    }
}

/// Scenario 3 (local end-to-end): an atomic project delete / rename-away routes
/// (in the daemon, see [`pointers`]) to a scoped `DirtySubtrees` scan of the
/// vanished root. Here we prove the local effect through a real store/session
/// round-trip: the scoped scan observes the subtree empty, so its cache rows are
/// pruned (not preserved from head), while an unrelated deep subtree survives.
#[test]
fn vanished_subtree_scan_prunes_cache_rows_and_preserves_unrelated_deep() {
    let fixture = Fixture::new("acceptance-vanished-subtree");
    let mut store = fixture.store;
    let roots = BTreeSet::from(["oldrepo".to_string()]);
    store
        .apply_stat_cache_write_back(
            &workspace_id(),
            &[
                row("oldrepo/a.rs", "cid_a"),
                row("oldrepo/nested/b.rs", "cid_b"),
                row("keep/deep/c.rs", "cid_c"),
            ],
            &BTreeSet::new(),
        )
        .expect("seed rows");

    // The scoped session loads only rows under `oldrepo`; the unrelated `keep`
    // subtree is never loaded and cannot be pruned.
    let mut session =
        StatCacheSession::load_scoped(&store, &workspace_id(), &roots, 1, &KEY).expect("scoped");
    // The subtree vanished on disk, so the scan observed nothing under it.
    let observed = BTreeSet::new();
    let write_back =
        session.finish_with_delete_scope(&observed, StatCacheDeleteScope::UnderRoots(&roots));
    store
        .apply_stat_cache_write_back(&workspace_id(), &write_back.upserts, &write_back.deletes)
        .expect("apply prune");

    let after = store.stat_cache_rows(&workspace_id()).expect("rows after");
    assert!(
        !after.contains_key("oldrepo/a.rs"),
        "vanished subtree row pruned"
    );
    assert!(
        !after.contains_key("oldrepo/nested/b.rs"),
        "nested vanished subtree row pruned"
    );
    assert!(
        after.contains_key("keep/deep/c.rs"),
        "unrelated deep subtree preserved, not pruned"
    );
}

/// Scenario 5: a combined root+subtree tick. Ownership-aware report merge and
/// partial metadata write-back must both choose the scoped-owned view for
/// overlapping paths. The merge is proven end-to-end over real scans (the merged
/// report carries the scoped subtree's deep child and a single `src` entry); the
/// write-back scope is proven to own exactly the same slice.
#[test]
fn combined_tick_merge_and_write_back_agree_on_scoped_ownership() {
    let temp = TempWorkspace::new("acceptance-combined-ownership").expect("workspace");
    temp.write_file("README.md", b"top\n").expect("root file");
    temp.write_file("src/app.rs", b"fn app() {}\n")
        .expect("dirty subtree child");
    temp.write_file("other/deep.rs", b"fn other() {}\n")
        .expect("unrelated deep file");
    let roots = BTreeSet::from(["src".to_string()]);

    let scoped = scan_workspace_scoped(temp.root(), &roots).expect("scoped scan");
    let shallow = scan_workspace_root_shallow(temp.root()).expect("shallow scan");
    let merged = merge_scoped_and_shallow_reports(scoped, shallow, &roots);

    // The overlapping `src` directory entry, observed by both passes, appears
    // exactly once in the merged report — the scoped-owned view.
    let src_entries = merged.paths.iter().filter(|p| p.path == "src").count();
    assert_eq!(
        src_entries, 1,
        "overlapping root entry is deduped, not doubled"
    );
    // The scoped subtree's deep child is present (scoped ownership under `src`),
    // and the root-level file from the shallow pass is merged in.
    assert!(
        merged.paths.iter().any(|p| p.path == "src/app.rs"),
        "scoped subtree view of src/app.rs is kept"
    );
    assert!(
        merged.paths.iter().any(|p| p.path == "README.md"),
        "root-level shallow entry is merged in"
    );

    // The write-back scope for the same combined tick owns exactly that slice:
    // root-level and under-`src`, but nothing under an unrelated subtree.
    let scan_scope = ScanScope::DirtySubtrees {
        roots: roots.clone(),
        root_shallow: true,
    };
    let write_scope = ObservationWriteScope::for_scan_scope(&scan_scope);
    assert!(
        write_scope.owns_path("src"),
        "scoped owner claims the overlapping root"
    );
    assert!(
        write_scope.owns_path("src/app.rs"),
        "scoped owner claims its subtree"
    );
    assert!(
        write_scope.owns_path("README.md"),
        "root-level metadata is owned"
    );
    assert!(
        !write_scope.owns_path("other/deep.rs"),
        "unrelated deep metadata is preserved, not owned by this tick"
    );
}

/// Pointer comments for scenarios whose contract a single U6/U7/U8/U9 unit test
/// already proves in full. U10 does not duplicate them; it names the owner so a
/// reader of the acceptance suite can find the authoritative coverage.
///
/// - Scenario 2 — root `.bowlineignore` edit/delete/rename-away forces
///   `Full(CliRequested)` with a redacted reason code and no path-bearing status
///   payload: `bowline-daemon` `daemon::sync::dirty_scope` tests
///   `policy_marker_edit_forces_full_scan`, `policy_marker_deletion_forces_full_scan`,
///   and `policy_marker_dominates_other_root_files`. The predicate itself is
///   asserted against the policy loader in the same module (R5 mitigation).
/// - Scenario 3 (routing) — atomic move-in / delete / rename-away routes to a
///   scoped subtree via entry kind: `daemon::sync::dirty_scope`
///   `root_directory_create_routes_to_scoped_subtree` and
///   `root_directory_deletion_routes_to_scoped_subtree`. The coalescer-level
///   prune is `sync::coalescer`
///   `missing_scoped_root_prunes_manifest_and_stat_cache_without_masking_deletion`;
///   the local session/store prune is covered end-to-end above.
/// - Scenario 4 — dirty-burst breadth (small-ancestor coalescing, huge-ancestor
///   batching, top-level breadth batching, over-budget singleton, two-tick
///   fairness): `bowline-daemon` `daemon::sync::dirty_batch` tests
///   `coalesces_to_small_shared_ancestors`, `does_not_coalesce_into_huge_unrelated_ancestors`,
///   `breadth_across_top_level_batches_without_full_scan`,
///   `over_budget_single_root_runs_alone`, and `pending_root_promoted_after_two_deferrals`.
/// - Scenario 5 (merge internals) — the scoped observation winning on a shared
///   root directory and combining scoped-deep with shallow-root entries:
///   `scanner` `merge_prefers_scoped_observation_for_shared_root_directory` and
///   `merge_combines_scoped_deep_and_shallow_root_entries`.
mod pointers {}
