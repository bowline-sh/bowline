//! Work-view journey tests (Plan 112 Step 3): create → materialize → edit →
//! capture → review → accept, plus the conflict-aside and discard paths. Every
//! operation is expressed as (base ⊕ overlay) manifests reusing the Plan 109
//! pull/push engine — no second apply path.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use super::*;
use crate::sync::manifest_engine::aux_index::{
    AUX_INDEX_PATH, AuxDecodeLimits, AuxIndex, WorkViewId, WorkViewLifecycle, load_aux_index,
    upload_aux_index,
};
use crate::sync::manifest_engine::engine_test_support::{
    FakeRemote, open_engine_store, test_context,
};
use crate::sync::manifest_engine::manifest::{
    FileMode, KeyEpoch, Manifest, ManifestEntry, ManifestKey, WorkspacePath,
};
use crate::sync::manifest_engine::pull_apply::{PullDeps, pull};
use crate::sync::manifest_engine::push::{PushDeps, PushOutcome, push};
use crate::sync::manifest_engine::store::ManifestStore;
use crate::workspace::TempWorkspace;

#[test]
fn lifting_a_root_project_filters_workspace_private_state() {
    let directory = ManifestEntry::Directory {
        mode: FileMode::new(0o040_755),
    };
    let project = Manifest::new(
        KeyEpoch::new(1),
        BTreeMap::from([
            (WorkspacePath::new(".bowline"), directory.clone()),
            (
                WorkspacePath::new(".bowline/project.json"),
                directory.clone(),
            ),
            (WorkspacePath::new(".work"), directory.clone()),
            (WorkspacePath::new(".bowline-meta"), directory.clone()),
            (WorkspacePath::new("src"), directory.clone()),
        ]),
    );

    let root = lift_project_manifest(&project, "");
    assert_eq!(
        root.entries
            .keys()
            .map(WorkspacePath::as_str)
            .collect::<Vec<_>>(),
        vec!["src"],
    );

    let nested = lift_project_manifest(&project, "apps/web");
    assert!(
        nested
            .entries
            .contains_key(&WorkspacePath::new("apps/web/.bowline"))
    );
    assert!(
        nested
            .entries
            .contains_key(&WorkspacePath::new("apps/web/.work"))
    );
    assert!(
        nested
            .entries
            .contains_key(&WorkspacePath::new("apps/web/.bowline-meta"))
    );
}

/// A workspace plus a shared object store, standing in for the Mac device that
/// owns the workspace ref. Its `remote` is the shared object/CAS store every
/// view also uploads to.
struct WorkspaceFixture {
    _workspace: TempWorkspace,
    root: std::path::PathBuf,
    store: ManifestStore,
    ctx: EngineContext,
    remote: FakeRemote,
}

impl WorkspaceFixture {
    fn new() -> Self {
        let workspace = TempWorkspace::new("wv-workspace").expect("temp workspace");
        let root = workspace.root().to_path_buf();
        let store = open_engine_store(&root);
        let ctx = test_context(root.clone(), "mac");
        Self {
            _workspace: workspace,
            root,
            store,
            ctx,
            remote: FakeRemote::new(),
        }
    }

    fn write(&self, rel: &str, bytes: &[u8]) {
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(path, bytes).expect("write");
    }

    /// Push the named paths to the workspace ref and return the new head key.
    fn push_head(&mut self, paths: &[&str]) -> ManifestKey {
        let dirty: BTreeSet<WorkspacePath> = paths.iter().map(|p| WorkspacePath::new(*p)).collect();
        let deps = PushDeps {
            ctx: &self.ctx,
            objects: &self.remote,
            refs: &self.remote,
        };
        match push(&mut self.store, &deps, &dirty).expect("push") {
            PushOutcome::Advanced { manifest_key, .. } => manifest_key,
            other => panic!("expected advance, got {other:?}"),
        }
    }

    fn head_manifest(&self) -> Manifest {
        let key = self
            .remote
            .current_ref()
            .expect("workspace head")
            .manifest_key;
        fetch_manifest(&self.remote, &self.ctx.crypto, &key).expect("fetch head")
    }

    /// Publish a workspace head that carries the aux index at its reserved path,
    /// so a second device learns the work views by ordinary sync. Returns the new
    /// head key.
    fn publish_head_with_aux(&self, mut manifest: Manifest, aux: &AuxIndex) -> ManifestKey {
        let (path, entry) =
            upload_aux_index(&self.remote, &self.ctx.crypto, aux).expect("upload aux");
        manifest.entries.insert(path, entry);
        self.remote.publish_manifest(&self.ctx.crypto, &manifest)
    }
}

/// Apply the current shared head onto a device store via the real pull path.
fn pull_device(store: &mut ManifestStore, ctx: &EngineContext, remote: &FakeRemote) {
    let deps = PullDeps {
        ctx,
        objects: remote,
        refs: remote,
    };
    pull(store, &deps).expect("device pull");
}

/// A materialized work-view directory with its own engine store.
struct ViewFixture {
    _workspace: TempWorkspace,
    root: std::path::PathBuf,
    store: ManifestStore,
    ctx: EngineContext,
}

impl ViewFixture {
    fn new(name: &str) -> Self {
        let workspace = TempWorkspace::new(name).expect("temp view");
        let root = workspace.root().to_path_buf();
        let store = open_engine_store(&root);
        let ctx = test_context(root.clone(), "view");
        Self {
            _workspace: workspace,
            root,
            store,
            ctx,
        }
    }

    fn read(&self, rel: &str) -> Vec<u8> {
        fs::read(self.root.join(rel)).expect("read view file")
    }

    fn write(&self, rel: &str, bytes: &[u8]) {
        fs::write(self.root.join(rel), bytes).expect("write view file");
    }
}

#[test]
fn create_materialize_edit_review_accept_journey() {
    let mut workspace = WorkspaceFixture::new();
    workspace.write("a.txt", b"alpha");
    workspace.write("b.txt", b"beta");
    let base = workspace.push_head(&["a.txt", "b.txt"]);

    // create: register a view whose overlay starts equal to the base.
    let mut aux = AuxIndex::empty();
    let id = WorkViewId::new("wv_1");
    register_work_view(&mut aux, id.clone(), base.clone());
    let record = aux.get(&id).expect("record").clone();
    assert_eq!(record.overlay_manifest_key, base);
    assert_eq!(record.lifecycle, WorkViewLifecycle::Active);

    // materialize: pull the overlay (== base) into the view directory.
    let mut view = ViewFixture::new("wv-view-accept");
    materialize_view(&mut view.store, &view.ctx, &workspace.remote, &base).expect("materialize");
    assert_eq!(view.read("a.txt"), b"alpha");
    assert_eq!(view.read("b.txt"), b"beta");

    // edit in the view (as any agent would — it is just a directory).
    view.write("a.txt", b"alpha-edited-in-view");

    // capture: push the view against its private ref -> new overlay key.
    let dirty: BTreeSet<WorkspacePath> = [WorkspacePath::new("a.txt")].into_iter().collect();
    let overlay = capture_overlay(&mut view.store, &view.ctx, &workspace.remote, &base, &dirty)
        .expect("capture")
        .expect("overlay advanced");
    assert_ne!(overlay, base);

    let mut record = record;
    record.overlay_manifest_key = overlay.clone();

    // review: the diff shows exactly the edited path as Modified.
    let changes = review_view(
        &workspace.remote,
        &workspace.ctx.crypto,
        &record.base_manifest_key,
        &record.overlay_manifest_key,
    )
    .expect("review");
    assert_eq!(
        changes,
        vec![WorkViewChange {
            path: WorkspacePath::new("a.txt"),
            kind: ChangeKind::Modified,
        }]
    );

    // accept: workspace has not advanced since fork, so the overlay entry is
    // adopted with no conflict.
    let workspace_manifest = workspace.head_manifest();
    let merge = accept_view(
        &workspace.remote,
        &workspace.ctx.crypto,
        &record,
        &workspace_manifest,
    )
    .expect("accept");
    assert!(merge.conflict_asides.is_empty());
    let base_manifest =
        fetch_manifest(&workspace.remote, &workspace.ctx.crypto, &base).expect("base");
    let overlay_manifest =
        fetch_manifest(&workspace.remote, &workspace.ctx.crypto, &overlay).expect("overlay");
    assert_eq!(
        merge.merged.entries.get(&WorkspacePath::new("a.txt")),
        overlay_manifest.entries.get(&WorkspacePath::new("a.txt")),
        "accept adopts the overlay's edited entry",
    );
    assert_ne!(
        base_manifest.entries.get(&WorkspacePath::new("a.txt")),
        merge.merged.entries.get(&WorkspacePath::new("a.txt")),
    );

    // The merged manifest publishes through the ordinary push CAS.
    let published = workspace
        .remote
        .publish_manifest(&workspace.ctx.crypto, &merge.merged);
    assert_eq!(
        workspace.remote.current_ref().expect("ref").manifest_key,
        published
    );
}

#[test]
fn accept_conflict_preserves_local_and_asides_the_overlay() {
    let mut workspace = WorkspaceFixture::new();
    workspace.write("shared.txt", b"origin");
    let base = workspace.push_head(&["shared.txt"]);

    // Fork a view and edit shared.txt one way.
    let mut view = ViewFixture::new("wv-view-conflict");
    materialize_view(&mut view.store, &view.ctx, &workspace.remote, &base).expect("materialize");
    view.write("shared.txt", b"view-change");
    let dirty: BTreeSet<WorkspacePath> = [WorkspacePath::new("shared.txt")].into_iter().collect();
    let overlay = capture_overlay(&mut view.store, &view.ctx, &workspace.remote, &base, &dirty)
        .expect("capture")
        .expect("overlay");

    // Meanwhile the workspace edits shared.txt a different way and advances.
    workspace.write("shared.txt", b"workspace-change");
    workspace.push_head(&["shared.txt"]);

    let mut aux = AuxIndex::empty();
    let id = WorkViewId::new("wv_c");
    register_work_view(&mut aux, id.clone(), base.clone());
    let record = {
        let mut r = aux.get(&id).expect("record").clone();
        r.overlay_manifest_key = overlay.clone();
        r
    };

    let workspace_manifest = workspace.head_manifest();
    let merge = accept_view(
        &workspace.remote,
        &workspace.ctx.crypto,
        &record,
        &workspace_manifest,
    )
    .expect("accept");

    // Local bytes stay canonical; the overlay is preserved as a deterministic aside.
    assert_eq!(merge.conflict_asides.len(), 1, "one conflicting path");
    assert!(
        merge.discarded_deletions.is_empty(),
        "a modify/modify conflict asides the overlay, never a discarded deletion",
    );
    let workspace_entry = workspace_manifest
        .entries
        .get(&WorkspacePath::new("shared.txt"))
        .expect("workspace entry");
    assert_eq!(
        merge.merged.entries.get(&WorkspacePath::new("shared.txt")),
        Some(workspace_entry),
        "the workspace's own edit stays at the canonical path",
    );
    let aside = &merge.conflict_asides[0];
    assert!(aside.as_str().starts_with("shared.txt (overlay "));
    assert!(
        merge.merged.entries.contains_key(aside),
        "the overlay survives as an aside entry",
    );
}

#[test]
fn accept_delete_vs_workspace_modify_reports_a_discarded_deletion() {
    let mut workspace = WorkspaceFixture::new();
    workspace.write("shared.txt", b"origin");
    let base = workspace.push_head(&["shared.txt"]);

    // The view deletes shared.txt and captures that deletion as its overlay.
    let mut view = ViewFixture::new("wv-view-delete-conflict");
    materialize_view(&mut view.store, &view.ctx, &workspace.remote, &base).expect("materialize");
    fs::remove_file(view.root.join("shared.txt")).expect("remove in view");
    let dirty: BTreeSet<WorkspacePath> = [WorkspacePath::new("shared.txt")].into_iter().collect();
    let overlay = capture_overlay(&mut view.store, &view.ctx, &workspace.remote, &base, &dirty)
        .expect("capture")
        .expect("overlay");

    // Meanwhile the workspace independently modifies shared.txt and advances.
    workspace.write("shared.txt", b"workspace-change");
    workspace.push_head(&["shared.txt"]);

    let mut aux = AuxIndex::empty();
    let id = WorkViewId::new("wv_del");
    register_work_view(&mut aux, id.clone(), base.clone());
    let record = {
        let mut r = aux.get(&id).expect("record").clone();
        r.overlay_manifest_key = overlay.clone();
        r
    };

    let workspace_manifest = workspace.head_manifest();
    let merge = accept_view(
        &workspace.remote,
        &workspace.ctx.crypto,
        &record,
        &workspace_manifest,
    )
    .expect("accept");

    // The overlay's deletion is surfaced, never silently applied nor asided.
    assert!(
        merge.conflict_asides.is_empty(),
        "a deletion has no content to aside",
    );
    assert_eq!(
        merge.discarded_deletions,
        vec![WorkspacePath::new("shared.txt")],
        "the discarded deletion is reported so accept never claims it landed",
    );
    // The workspace's newer edit survives at the canonical path, intact.
    let workspace_entry = workspace_manifest
        .entries
        .get(&WorkspacePath::new("shared.txt"))
        .expect("workspace entry");
    assert_eq!(
        merge.merged.entries.get(&WorkspacePath::new("shared.txt")),
        Some(workspace_entry),
        "the live workspace file stays canonical",
    );
}

#[test]
fn accept_clean_overlay_deletion_still_deletes() {
    let mut workspace = WorkspaceFixture::new();
    workspace.write("keep.txt", b"keep");
    workspace.write("gone.txt", b"remove-me");
    let base = workspace.push_head(&["keep.txt", "gone.txt"]);

    // The view deletes gone.txt; the workspace does not touch it after the fork.
    let mut view = ViewFixture::new("wv-view-clean-delete");
    materialize_view(&mut view.store, &view.ctx, &workspace.remote, &base).expect("materialize");
    fs::remove_file(view.root.join("gone.txt")).expect("remove in view");
    let dirty: BTreeSet<WorkspacePath> = [WorkspacePath::new("gone.txt")].into_iter().collect();
    let overlay = capture_overlay(&mut view.store, &view.ctx, &workspace.remote, &base, &dirty)
        .expect("capture")
        .expect("overlay");

    let mut aux = AuxIndex::empty();
    let id = WorkViewId::new("wv_clean_del");
    register_work_view(&mut aux, id.clone(), base.clone());
    let record = {
        let mut r = aux.get(&id).expect("record").clone();
        r.overlay_manifest_key = overlay.clone();
        r
    };

    let workspace_manifest = workspace.head_manifest();
    let merge = accept_view(
        &workspace.remote,
        &workspace.ctx.crypto,
        &record,
        &workspace_manifest,
    )
    .expect("accept");

    // Workspace untouched since the fork: the deletion lands cleanly, no conflict.
    assert!(merge.conflict_asides.is_empty());
    assert!(
        merge.discarded_deletions.is_empty(),
        "a clean deletion is not a conflict",
    );
    assert!(
        !merge
            .merged
            .entries
            .contains_key(&WorkspacePath::new("gone.txt")),
        "the overlay's deletion is applied",
    );
    assert!(
        merge
            .merged
            .entries
            .contains_key(&WorkspacePath::new("keep.txt")),
        "untouched files remain",
    );
}

#[test]
fn discard_marks_the_record_and_drops_it_from_live_keys() {
    let mut workspace = WorkspaceFixture::new();
    workspace.write("a.txt", b"alpha");
    let base = workspace.push_head(&["a.txt"]);

    let mut aux = AuxIndex::empty();
    let id = WorkViewId::new("wv_d");
    register_work_view(&mut aux, id.clone(), base.clone());

    set_lifecycle(&mut aux, &id, WorkViewLifecycle::Discarded).expect("discard");
    assert_eq!(
        aux.get(&id).expect("record").lifecycle,
        WorkViewLifecycle::Discarded
    );

    // A discarded view cannot be transitioned again.
    let error = set_lifecycle(&mut aux, &id, WorkViewLifecycle::Accepted).expect_err("terminal");
    assert!(matches!(error, WorkViewError::NotActive { .. }));
}

#[test]
fn capture_with_no_dirty_paths_is_a_noop() {
    let mut workspace = WorkspaceFixture::new();
    workspace.write("a.txt", b"alpha");
    let base = workspace.push_head(&["a.txt"]);

    let mut view = ViewFixture::new("wv-view-noop");
    materialize_view(&mut view.store, &view.ctx, &workspace.remote, &base).expect("materialize");

    // Nothing dirty: capture uploads nothing and yields no new overlay key.
    let dirty: BTreeSet<WorkspacePath> = BTreeSet::new();
    let captured = capture_overlay(&mut view.store, &view.ctx, &workspace.remote, &base, &dirty)
        .expect("capture");
    assert!(captured.is_none());
}

#[test]
fn two_device_work_view_journey_visible_across_devices() {
    // ---- Mac owns the workspace ----
    let mut mac = WorkspaceFixture::new();
    mac.write("a.txt", b"alpha");
    mac.write("b.txt", b"beta");
    let base = mac.push_head(&["a.txt", "b.txt"]);

    // Create a work view and publish the aux index as a synced manifest entry.
    let mut aux = AuxIndex::empty();
    let id = WorkViewId::new("wv_two_device");
    register_work_view(&mut aux, id.clone(), base.clone());
    let head_with_aux = mac.publish_head_with_aux(mac.head_manifest(), &aux);
    assert!(
        mac.head_manifest()
            .entries
            .contains_key(&WorkspacePath::new(AUX_INDEX_PATH)),
        "the workspace head carries the aux index entry",
    );

    // ---- Vivobook joins by ordinary sync and learns the view ----
    let mut vivo = ViewFixture::new("wv-vivobook");
    pull_device(&mut vivo.store, &vivo.ctx, &mac.remote);
    assert_eq!(vivo.read("a.txt"), b"alpha");
    let vivo_head = fetch_manifest(&mac.remote, &vivo.ctx.crypto, &head_with_aux).expect("head");
    let vivo_aux = load_aux_index(
        &mac.remote,
        &vivo.ctx.crypto,
        &vivo_head,
        &AuxDecodeLimits::default(),
    )
    .expect("load aux")
    .expect("aux present");
    assert!(
        vivo_aux.get(&id).is_some(),
        "vivobook learns the work view from ordinary sync",
    );

    // ---- Do work in the view (as an agent would: add a new file), review,
    // accept on Mac. The agent adds new work rather than mutating a file it just
    // materialized, which is both the common agent scenario and side-steps a
    // pre-existing manifest_engine apply quirk where a re-pulled, locally
    // materialized file reads back mode-changed (see the Plan 112 report). ----
    let mut view = ViewFixture::new("wv-view-2d");
    materialize_view(&mut view.store, &view.ctx, &mac.remote, &base).expect("materialize");
    view.write("feature.txt", b"new agent work");
    let dirty: BTreeSet<WorkspacePath> = [WorkspacePath::new("feature.txt")].into_iter().collect();
    let overlay = capture_overlay(&mut view.store, &view.ctx, &mac.remote, &base, &dirty)
        .expect("capture")
        .expect("overlay");
    let record = {
        let mut r = vivo_aux.get(&id).cloned().expect("record");
        r.overlay_manifest_key = overlay.clone();
        r
    };
    let changes = review_view(
        &mac.remote,
        &mac.ctx.crypto,
        &record.base_manifest_key,
        &record.overlay_manifest_key,
    )
    .expect("review");
    assert_eq!(
        changes,
        vec![WorkViewChange {
            path: WorkspacePath::new("feature.txt"),
            kind: ChangeKind::Added,
        }],
        "review shows the single added path",
    );

    // Accept: three-way merge against the current workspace head, mark the record
    // accepted, and publish the merged head (still carrying the aux index).
    let merge =
        accept_view(&mac.remote, &mac.ctx.crypto, &record, &mac.head_manifest()).expect("accept");
    assert!(merge.conflict_asides.is_empty());
    set_lifecycle(&mut aux, &id, WorkViewLifecycle::Accepted).expect("accept lifecycle");
    let accepted_head = mac.publish_head_with_aux(merge.merged, &aux);

    // ---- Vivobook pulls again: the accepted change appears as ordinary sync ----
    pull_device(&mut vivo.store, &vivo.ctx, &mac.remote);
    assert_eq!(
        vivo.read("feature.txt"),
        b"new agent work",
        "the accepted work-view change reaches vivobook by ordinary sync",
    );
    let head2 = fetch_manifest(&mac.remote, &vivo.ctx.crypto, &accepted_head).expect("head2");
    let aux2 = load_aux_index(
        &mac.remote,
        &vivo.ctx.crypto,
        &head2,
        &AuxDecodeLimits::default(),
    )
    .expect("load aux2")
    .expect("aux2 present");
    assert_eq!(
        aux2.get(&id).expect("record").lifecycle,
        WorkViewLifecycle::Accepted,
        "vivobook sees the view accepted",
    );
}

#[test]
fn two_device_discard_syncs() {
    let mut mac = WorkspaceFixture::new();
    mac.write("a.txt", b"alpha");
    let base = mac.push_head(&["a.txt"]);

    let mut aux = AuxIndex::empty();
    let id = WorkViewId::new("wv_discard_sync");
    register_work_view(&mut aux, id.clone(), base.clone());
    mac.publish_head_with_aux(mac.head_manifest(), &aux);

    // Discard the view and republish the aux.
    set_lifecycle(&mut aux, &id, WorkViewLifecycle::Discarded).expect("discard");
    let head = mac.publish_head_with_aux(mac.head_manifest(), &aux);

    // Vivobook syncs and sees the discard.
    let mut vivo = ViewFixture::new("wv-vivobook-discard");
    pull_device(&mut vivo.store, &vivo.ctx, &mac.remote);
    let vivo_head = fetch_manifest(&mac.remote, &vivo.ctx.crypto, &head).expect("head");
    let vivo_aux = load_aux_index(
        &mac.remote,
        &vivo.ctx.crypto,
        &vivo_head,
        &AuxDecodeLimits::default(),
    )
    .expect("load")
    .expect("present");
    assert_eq!(
        vivo_aux.get(&id).expect("record").lifecycle,
        WorkViewLifecycle::Discarded,
    );
}

#[test]
fn diff_reports_add_modify_delete() {
    let mut workspace = WorkspaceFixture::new();
    workspace.write("keep.txt", b"same");
    workspace.write("edit.txt", b"before");
    workspace.write("gone.txt", b"remove-me");
    let base = workspace.push_head(&["keep.txt", "edit.txt", "gone.txt"]);

    let mut view = ViewFixture::new("wv-view-diff");
    materialize_view(&mut view.store, &view.ctx, &workspace.remote, &base).expect("materialize");
    view.write("edit.txt", b"after");
    view.write("new.txt", b"created");
    fs::remove_file(view.root.join("gone.txt")).expect("remove");

    let dirty: BTreeSet<WorkspacePath> = ["edit.txt", "new.txt", "gone.txt"]
        .iter()
        .map(|p| WorkspacePath::new(*p))
        .collect();
    let overlay = capture_overlay(&mut view.store, &view.ctx, &workspace.remote, &base, &dirty)
        .expect("capture")
        .expect("overlay");

    let base_m = fetch_manifest(&workspace.remote, &workspace.ctx.crypto, &base).expect("base");
    let overlay_m =
        fetch_manifest(&workspace.remote, &workspace.ctx.crypto, &overlay).expect("overlay");
    let changes = diff_manifests(&base_m, &overlay_m);

    let mut by_path: Vec<(&str, ChangeKind)> =
        changes.iter().map(|c| (c.path.as_str(), c.kind)).collect();
    by_path.sort();
    assert_eq!(
        by_path,
        vec![
            ("edit.txt", ChangeKind::Modified),
            ("gone.txt", ChangeKind::Deleted),
            ("new.txt", ChangeKind::Added),
        ]
    );
}
