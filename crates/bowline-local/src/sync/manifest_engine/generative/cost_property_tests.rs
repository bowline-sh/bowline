//! The change-proportionality property of pull, stated as an observable.
//!
//! The cost model says a pull must classify only
//! `{p : remote_delta(p) ≠ Unchanged} ∪ dirty`, because every other path
//! provably lands on the `(Unchanged, Unchanged)` row. A pull that instead walks
//! the whole ancestor pays one `symlink_metadata` per workspace entry — the
//! `O(total workspace)` term the prior-art study measured.
//!
//! Stat syscalls are not metered, so the property is asserted through an
//! observable only a stat can produce: files removed behind the engine's back,
//! with no watcher event and no dirty entry. A pull that stats such a path
//! *sees* the removal and schedules the path for re-push; a change-proportional
//! pull cannot see it at all and leaves the discovery to the stat walk that owns
//! it. Stray re-pushes are therefore an exact, deterministic count of paths
//! statted outside the delta — a syscall assertion in all but name, with no
//! wall-clock timing in it.
//!
//! Both scopes run on identical workspaces in every case. The whole-ancestor
//! control must see every removal, which is what proves the narrowed
//! assertion is not passing merely because the trap stopped working.

use std::collections::BTreeSet;

use super::super::engine_test_support::TestEngine;
use super::super::manifest::{Manifest, WorkspacePath};
use super::rng::Rng;
use super::{base_seed, case_count, replay_hint};

/// Workspace size for the property. Large enough that a whole-ancestor pull is
/// unmistakable in the assertion message, small enough to stay a fast test.
const WORKSPACE_FILES: u32 = 64;
const CI_CASES: u32 = 4;

/// Which classification scope a case drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    /// The driver shape: only the remote delta plus the dirty set is re-observed.
    ChangedAndDirty,
    /// The audit shape, kept as the control: every ancestor path is re-observed.
    WholeAncestor,
}

#[test]
fn a_pull_classifies_only_the_remote_delta() {
    let base = base_seed();
    for index in 0..case_count(CI_CASES) {
        let seed = base.case(index);
        let narrowed = observe(seed.get(), index, Scope::ChangedAndDirty);
        let control = observe(seed.get(), index, Scope::WholeAncestor);

        assert_eq!(
            control.stray_push_again.len(),
            usize::try_from(WORKSPACE_FILES).unwrap_or(0) - 1,
            "the control must observe every off-delta removal, otherwise the \
             narrowed assertion below is vacuous (replay with {})",
            replay_hint(base, index)
        );
        assert!(
            narrowed.stray_push_again.is_empty(),
            "pull statted {} paths outside the remote delta (replay with {}); \
             a change-proportional pull classifies |delta| paths, not the whole \
             ancestor: {:?}",
            narrowed.stray_push_again.len(),
            replay_hint(base, index),
            narrowed
                .stray_push_again
                .iter()
                .take(5)
                .map(WorkspacePath::as_str)
                .collect::<Vec<_>>(),
        );
        assert!(
            narrowed.installed_changed_path,
            "the narrowed pull must still apply the one path that DID change \
             (replay with {})",
            replay_hint(base, index)
        );
    }
}

struct ScopeObservation {
    /// Paths the pull scheduled for re-push that lie outside the remote delta.
    /// Each one is proof of a stat outside the delta.
    stray_push_again: BTreeSet<WorkspacePath>,
    installed_changed_path: bool,
}

fn observe(seed: u64, case: u32, scope: Scope) -> ScopeObservation {
    let mut rng = Rng::from_seed(super::rng::Seed::new(seed));
    let mut engine = TestEngine::new(&format!("pull-cost-{case}-{scope:?}"));
    let paths: Vec<String> = (0..WORKSPACE_FILES)
        .map(|index| format!("proj/file-{index:04}.txt"))
        .collect();
    for (index, path) in paths.iter().enumerate() {
        engine.write(path, format!("body-{index}").as_bytes());
    }
    let borrowed: Vec<&str> = paths.iter().map(String::as_str).collect();
    engine.push(&borrowed);

    // Exactly one path changes remotely: that is the whole delta.
    let changed_index = rng.below(paths.len()).unwrap_or(0);
    let changed = WorkspacePath::new(paths[changed_index].clone());
    let replacement = engine.remote_file(b"remote body");
    let head = engine
        .remote
        .decoded_manifest(&engine.ctx.crypto)
        .expect("head manifest");
    let mut entries = head.entries.clone();
    entries.insert(changed.clone(), replacement);
    engine.remote.publish_manifest(
        &engine.ctx.crypto,
        &Manifest::new(engine.ctx.crypto.key_epoch(), entries),
    );

    // Every OTHER path disappears from disk with no watcher event and no dirty
    // entry, so only a stat can reveal it.
    for (index, path) in paths.iter().enumerate() {
        if index != changed_index {
            engine.remove(path);
        }
    }

    let outcome = match scope {
        Scope::ChangedAndDirty => engine.pull_dirty(&[]),
        Scope::WholeAncestor => engine.pull(),
    };
    let delta = BTreeSet::from([changed.clone()]);
    ScopeObservation {
        stray_push_again: outcome.push_again.difference(&delta).cloned().collect(),
        installed_changed_path: outcome.installed.contains(&changed),
    }
}
