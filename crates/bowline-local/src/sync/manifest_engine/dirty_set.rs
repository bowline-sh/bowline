//! Ordering and bounds for the engine's dirty set.
//!
//! Publication is bounded, so the dirty set is not merely a set: it carries the
//! order paths were last observed in. That is what lets a bounded publish carry
//! a fresh edit instead of whatever happens to sort first -- without it, a file
//! written after a burst waits for the burst's whole backlog to drain.

/// How many paths one publish may carry when the backlog is larger than that.
///
/// Chosen to sit at or under the transport's object batch size, so a bounded
/// publish is one reserve/commit pair rather than several.
pub(super) const PUBLISH_BATCH_MAX: usize = 64;

/// Monotonic observation order for dirty paths, so a publish that must be
/// bounded can prefer the newest edits without guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct DirtySeq(u64);

impl DirtySeq {
    pub(super) const INITIAL: Self = Self(0);

    pub(super) fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

use std::collections::BTreeSet;
use std::sync::Arc;

use super::ManifestEngine;
use super::manifest::WorkspacePath;

impl ManifestEngine {
    pub(super) fn absorb_dirty(&mut self, paths: BTreeSet<WorkspacePath>) {
        let folded = self.canonical_paths(paths);
        for path in &folded {
            // Re-absorbing re-stamps: recency means last observed, not first.
            self.next_dirty_seq = self.next_dirty_seq.next();
            self.dirty_seen.insert(path.clone(), self.next_dirty_seq);
        }
        Arc::make_mut(&mut self.dirty).extend(folded);
    }

    pub(super) fn canonical_paths(
        &self,
        paths: BTreeSet<WorkspacePath>,
    ) -> BTreeSet<WorkspacePath> {
        paths
            .into_iter()
            .map(|path| self.ctx.names.canonical_spelling(&path))
            .collect()
    }
}
