//! Cost-aware coalescing and bounded batching over a drained dirty frontier.
//!
//! `DirtyScope` (see `dirty_scope.rs`) is a pure watcher-event accumulator: it
//! records dirty roots, root-level file leaves, and forced-full state and knows
//! nothing about the store or change index. When dirt spans more top-level
//! projects than `MAX_DIRTY_SUBTREES`, the old accumulator forced a full
//! recursive workspace scan just to fit the cap. `DirtyBatchPlanner` replaces
//! that: it consumes the drained frontier plus a `ChangeIndexSnapshot` and
//! deterministically decides which roots to scan this tick, coalescing only
//! within a row budget and deferring the rest — so breadth is handled by bounded
//! batches, never by an O(workspace) scan.
//!
//! The planner is a pure function of (roots, root_shallow, snapshot, pending,
//! tick): tests feed fake snapshots directly. Determinism matters because the
//! chosen scope feeds scan-scope which feeds manifest ordering.

use std::collections::{BTreeMap, BTreeSet};

use bowline_local::sync::change_index::{ChangeIndexSnapshot, LocalChangeIndex};

use super::*;

/// Per-tick active-row budget. A coalescing parent whose estimated subtree rows
/// exceed this is not a valid broadening target; a batch keeps cumulative
/// estimated rows at or below it except for a single over-budget root that must
/// run alone.
pub(in crate::daemon) const PART_B_MAX_SCOPED_INDEX_ROWS: u64 = 10_000;

/// A root deferred across this many successful sync ticks is promoted into the
/// next batch (as a singleton if necessary) so smallest-cost-first scheduling
/// cannot starve a large root.
const MAX_DEFERRAL_TICKS: u64 = 2;

/// Fairness bookkeeping for a root that could not be scheduled this tick.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingRoot {
    first_seen_tick: u64,
    deferral_count: u64,
}

/// Roots carried between ticks because they did not fit the current batch. Owned
/// by the daemon reconcile loop, not by `DirtyScope` (KTD-16: store/index and
/// fairness state stay out of the raw accumulator).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(in crate::daemon) struct PendingDirtyRoots {
    roots: BTreeMap<String, PendingRoot>,
}

impl PendingDirtyRoots {
    pub(in crate::daemon) fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    pub(in crate::daemon) fn roots(&self) -> BTreeSet<String> {
        self.roots.keys().cloned().collect()
    }

    fn forced(&self) -> BTreeSet<String> {
        self.roots
            .iter()
            .filter(|(_, meta)| meta.deferral_count >= MAX_DEFERRAL_TICKS)
            .map(|(root, _)| root.clone())
            .collect()
    }

    /// Record `root` as deferred this tick, aging any pre-existing entry.
    fn defer(&mut self, root: String, tick: u64) {
        self.roots
            .entry(root)
            .and_modify(|meta| meta.deferral_count += 1)
            .or_insert(PendingRoot {
                first_seen_tick: tick,
                deferral_count: 1,
            });
    }

    fn clear_scheduled(&mut self, scheduled: &BTreeSet<String>) {
        // Drop a pending root when it is scheduled directly OR when a scheduled
        // ancestor subsumes it (coalescing can merge a pending child such as
        // `a/1` into a scheduled `a`; the ancestor's recursive scan covers it).
        // Without the ancestor check the child lingers in pending forever, so
        // `is_empty()` never returns true again and the pure root-shallow fast
        // path is permanently lost.
        self.roots.retain(|root, _| {
            !scheduled.contains(root)
                && !scheduled
                    .iter()
                    .any(|ancestor| root.starts_with(&format!("{ancestor}/")))
        });
    }
}

/// Cost/decision summary for observability and tests. Aggregate-only (no paths).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(in crate::daemon) struct DirtyBatchCost {
    pub(in crate::daemon) candidate_roots: usize,
    pub(in crate::daemon) scheduled_roots: usize,
    pub(in crate::daemon) deferred_roots: usize,
    pub(in crate::daemon) estimated_rows: u64,
    pub(in crate::daemon) coalesced: bool,
    pub(in crate::daemon) batched: bool,
    pub(in crate::daemon) over_budget_single_root: bool,
}

/// Planner output: the scope to scan this tick plus the cost summary. Pending
/// state is mutated in place on the `PendingDirtyRoots` passed to the planner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::daemon) struct DirtyBatchPlan {
    pub(in crate::daemon) scope: ScanScope,
    pub(in crate::daemon) cost: DirtyBatchCost,
}

fn path_depth(path: &str) -> usize {
    path.matches('/').count()
}

fn immediate_parent(path: &str) -> String {
    path.rsplit_once('/')
        .map(|(parent, _)| parent.to_string())
        .unwrap_or_default()
}

/// Estimated rows contributed by scanning `root`. A root absent from the
/// snapshot has no bounded estimate, so it is treated as over budget — the
/// conservative choice keeps an unmeasured subtree from silently joining a batch.
fn estimate(root: &str, snapshot: &ChangeIndexSnapshot) -> u64 {
    snapshot
        .estimated_subtree_entry_count(root)
        .unwrap_or(u64::MAX)
}

/// Remove `root` and every path strictly under it; they are subsumed by a scoped
/// scan of `root`.
fn absorb_into(current: &mut BTreeSet<String>, root: &str) {
    let prefix = format!("{root}/");
    current.retain(|path| path != root && !path.starts_with(&prefix));
    current.insert(root.to_string());
}

/// Deterministically coalesce `candidates` toward `cap` roots by merging the
/// deepest paths into their immediate parent, one depth level at a time
/// (shallowest reduction last). A parent is a valid merge target only when it
/// covers at least two current paths (so the merge reduces the count) and its
/// estimated subtree rows stay within budget. Parents at the same depth are
/// processed in sorted order and the pass stops as soon as the set fits, making
/// the result a pure function of the input set and estimates, independent of
/// insertion order.
fn coalesce(
    candidates: &BTreeSet<String>,
    snapshot: &ChangeIndexSnapshot,
    budget: u64,
    cap: usize,
) -> BTreeSet<String> {
    let mut current = candidates.clone();
    loop {
        if current.len() <= cap {
            return current;
        }
        let Some(max_depth) = current.iter().map(|p| path_depth(p)).max() else {
            return current;
        };
        if max_depth == 0 {
            return current; // all top-level: no shallower parent to merge into.
        }
        // Merge every qualifying parent at the current deepest level in one pass
        // (sorted order for determinism), then re-check fit. Finishing the whole
        // level before checking is what collapses e.g. {a/*, b/*} all the way to
        // {a, b} rather than stopping at {a, b/*} the instant the count first
        // dips under the cap.
        let parents: BTreeSet<String> = current
            .iter()
            .filter(|p| path_depth(p) == max_depth)
            .map(|p| immediate_parent(p))
            .collect();
        let mut changed = false;
        for parent in &parents {
            let prefix = format!("{parent}/");
            let covered = current
                .iter()
                .filter(|p| *p == parent || p.starts_with(&prefix))
                .count();
            if covered >= 2 && estimate(parent, snapshot) <= budget {
                absorb_into(&mut current, parent);
                changed = true;
            }
        }
        if !changed {
            return current; // nothing further reducible within budget.
        }
    }
}

/// Order roots for scheduling: roots that have aged past the deferral limit come
/// first (oldest first, then path) so they cannot starve; the rest follow
/// smallest-estimated-cost first, then path. Fully deterministic.
fn schedule_order(
    roots: &BTreeSet<String>,
    forced: &BTreeSet<String>,
    snapshot: &ChangeIndexSnapshot,
) -> Vec<String> {
    let mut forced_first: Vec<String> = roots
        .iter()
        .filter(|r| forced.contains(*r))
        .cloned()
        .collect();
    forced_first.sort();
    let mut rest: Vec<String> = roots
        .iter()
        .filter(|r| !forced.contains(*r))
        .cloned()
        .collect();
    rest.sort_by(|a, b| {
        estimate(a, snapshot)
            .cmp(&estimate(b, snapshot))
            .then_with(|| a.cmp(b))
    });
    forced_first.into_iter().chain(rest).collect()
}

/// Plan the scope for one tick from the drained frontier, the change-index
/// snapshot, and the carried pending roots. `root_shallow` reflects whether the
/// tick also has dirty root-level files (a shallow pass runs alongside the
/// scoped scan). `pending` is updated in place: scheduled roots are cleared,
/// deferred roots are aged.
pub(in crate::daemon) fn plan_dirty_batch(
    fresh_roots: BTreeSet<String>,
    root_shallow: bool,
    snapshot: &ChangeIndexSnapshot,
    pending: &mut PendingDirtyRoots,
    tick: u64,
) -> DirtyBatchPlan {
    let mut candidates = fresh_roots;
    candidates.extend(pending.roots());
    let candidate_count = candidates.len();

    if candidates.is_empty() {
        // Pure root-shallow tick (or nothing): no subtree scheduling to do.
        let scope = if root_shallow {
            ScanScope::RootShallow
        } else {
            ScanScope::Full(super::FullScanReason::ReconcileFallback)
        };
        return DirtyBatchPlan {
            scope,
            cost: DirtyBatchCost::default(),
        };
    }

    let forced = pending.forced();
    let coalesced = coalesce(
        &candidates,
        snapshot,
        PART_B_MAX_SCOPED_INDEX_ROWS,
        MAX_DIRTY_SUBTREES,
    );
    let did_coalesce = coalesced.len() < candidates.len();

    let ordered = schedule_order(&coalesced, &forced, snapshot);

    let mut batch: BTreeSet<String> = BTreeSet::new();
    let mut used_rows: u64 = 0;
    let mut over_budget_single_root = false;
    for root in &ordered {
        let est = estimate(root, snapshot);
        if batch.is_empty() {
            // The highest-priority root always runs; if it alone exceeds the
            // budget it runs as a singleton and no other root joins it.
            batch.insert(root.clone());
            used_rows = est;
            if est > PART_B_MAX_SCOPED_INDEX_ROWS {
                over_budget_single_root = true;
                break;
            }
            continue;
        }
        if batch.len() >= MAX_DIRTY_SUBTREES {
            break;
        }
        let next_rows = used_rows.saturating_add(est);
        if est > PART_B_MAX_SCOPED_INDEX_ROWS || next_rows > PART_B_MAX_SCOPED_INDEX_ROWS {
            continue; // does not fit this tick; leave for a later batch.
        }
        batch.insert(root.clone());
        used_rows = next_rows;
    }

    // Any coalesced root not scheduled this tick is carried and aged; scheduled
    // roots (and any pending entries they subsume) are cleared.
    pending.clear_scheduled(&batch);
    let mut deferred = 0usize;
    for root in &coalesced {
        if !batch.contains(root) {
            pending.defer(root.clone(), tick);
            deferred += 1;
        }
    }

    let cost = DirtyBatchCost {
        candidate_roots: candidate_count,
        scheduled_roots: batch.len(),
        deferred_roots: deferred,
        estimated_rows: used_rows,
        coalesced: did_coalesce,
        batched: deferred > 0,
        over_budget_single_root,
    };

    DirtyBatchPlan {
        scope: ScanScope::DirtySubtrees {
            roots: batch,
            root_shallow,
        },
        cost,
    }
}

impl ContinuousSyncRuntime {
    /// Refine a raw drained scope into a bounded, cost-aware batch. A forced full
    /// scan subsumes every deferred root; a scoped/shallow scope is planned by
    /// `DirtyBatchPlanner` against the change-index estimates so breadth never
    /// forces an O(workspace) scan. When estimates are unavailable, a scoped set
    /// wider than the cap degrades to the reserved `DirtyCapExceeded` full scan
    /// (the one unrecoverable case) rather than an unbounded scoped batch.
    pub(in crate::daemon) fn resolve_dirty_batch_scope(&mut self, raw: ScanScope) -> ScanScope {
        match raw {
            ScanScope::Full(reason) => {
                self.pending_dirty_roots = PendingDirtyRoots::default();
                ScanScope::Full(reason)
            }
            ScanScope::RootShallow if self.pending_dirty_roots.is_empty() => ScanScope::RootShallow,
            ScanScope::RootShallow => self.plan_dirty_batch_scope(BTreeSet::new(), true),
            ScanScope::DirtySubtrees {
                roots,
                root_shallow,
            } => self.plan_dirty_batch_scope(roots, root_shallow),
        }
    }

    fn plan_dirty_batch_scope(
        &mut self,
        fresh_roots: BTreeSet<String>,
        root_shallow: bool,
    ) -> ScanScope {
        let mut candidates = fresh_roots.clone();
        candidates.extend(self.pending_dirty_roots.roots());
        match self.change_index_snapshot(&candidates) {
            Some(snapshot) => {
                let tick = self.tick_count;
                plan_dirty_batch(
                    fresh_roots,
                    root_shallow,
                    &snapshot,
                    &mut self.pending_dirty_roots,
                    tick,
                )
                .scope
            }
            None if candidates.len() > MAX_DIRTY_SUBTREES => {
                ScanScope::Full(FullScanReason::DirtyCapExceeded)
            }
            None if fresh_roots.is_empty() && root_shallow => ScanScope::RootShallow,
            None if fresh_roots.is_empty() => ScanScope::Full(FullScanReason::ReconcileFallback),
            None => ScanScope::DirtySubtrees {
                roots: fresh_roots,
                root_shallow,
            },
        }
    }

    /// Build a change-index snapshot covering the candidate roots and every
    /// ancestor prefix (coalescing needs parent estimates), or `None` if the
    /// store read fails. Best-effort: a missing snapshot degrades safely.
    fn change_index_snapshot(
        &self,
        candidate_roots: &BTreeSet<String>,
    ) -> Option<ChangeIndexSnapshot> {
        let mut roots_with_ancestors = candidate_roots.clone();
        for root in candidate_roots {
            let mut path = root.as_str();
            while let Some((parent, _)) = path.rsplit_once('/') {
                roots_with_ancestors.insert(parent.to_string());
                path = parent;
            }
        }
        let workspace_id = self.options.args.workspace_id();
        self.with_store_clearing_swallowed_failures(|store| {
            let mut index = LocalChangeIndex::new(store, workspace_id.clone());
            index.snapshot_for_roots(&roots_with_ancestors)
        })
        .ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(estimates: &[(&str, u64)]) -> ChangeIndexSnapshot {
        let map: BTreeMap<String, u64> = estimates
            .iter()
            .map(|(root, rows)| ((*root).to_string(), *rows))
            .collect();
        ChangeIndexSnapshot::from_parts(BTreeSet::new(), map, Default::default())
    }

    fn roots(paths: &[&str]) -> BTreeSet<String> {
        paths.iter().map(|p| (*p).to_string()).collect()
    }

    fn scoped(plan: &DirtyBatchPlan) -> &BTreeSet<String> {
        match &plan.scope {
            ScanScope::DirtySubtrees { roots, .. } => roots,
            other => panic!("expected DirtySubtrees, got {other:?}"),
        }
    }

    // >64 roots under two small top-level ancestors coalesce to {a, b} when each
    // ancestor's estimate is within budget.
    #[test]
    fn coalesces_to_small_shared_ancestors() {
        let mut input = BTreeSet::new();
        for i in 0..40 {
            input.insert(format!("a/{i}"));
            input.insert(format!("b/{i}"));
        }
        let snap = snapshot(&[("a", 40), ("b", 40)]);
        let mut pending = PendingDirtyRoots::default();
        let plan = plan_dirty_batch(input, false, &snap, &mut pending, 1);
        assert_eq!(scoped(&plan), &roots(&["a", "b"]));
        assert!(plan.cost.coalesced);
        assert!(!plan.cost.batched);
        assert!(pending.is_empty());
    }

    // The same shape, but each ancestor holds a huge unrelated subtree: the
    // parent is not a valid coalescing target, so it does not collapse to {a, b}.
    #[test]
    fn does_not_coalesce_into_huge_unrelated_ancestors() {
        let mut input = BTreeSet::new();
        for i in 0..40 {
            input.insert(format!("a/{i}"));
            input.insert(format!("b/{i}"));
        }
        // Children are cheap; the ancestors are enormous.
        let mut est: Vec<(&str, u64)> = vec![("a", 500_000), ("b", 500_000)];
        let owned: Vec<String> = (0..40)
            .flat_map(|i| [format!("a/{i}"), format!("b/{i}")])
            .collect();
        for path in &owned {
            est.push((path.as_str(), 5));
        }
        let snap = snapshot(&est);
        let mut pending = PendingDirtyRoots::default();
        let plan = plan_dirty_batch(input, false, &snap, &mut pending, 1);
        // Cannot merge into a/b; breadth handled by a bounded batch instead.
        assert!(!scoped(&plan).contains("a") || scoped(&plan).len() > 2);
        assert!(scoped(&plan).len() <= MAX_DIRTY_SUBTREES);
        assert!(plan.cost.batched);
        assert!(!pending.is_empty());
        // No full scan was chosen.
        assert!(matches!(plan.scope, ScanScope::DirtySubtrees { .. }));
    }

    // >64 distinct top-level roots: deterministic bounded batch, remainder
    // pending, never a full scan.
    #[test]
    fn breadth_across_top_level_batches_without_full_scan() {
        let mut input = BTreeSet::new();
        let mut est = Vec::new();
        let names: Vec<String> = (0..200).map(|i| format!("proj{i:03}")).collect();
        for name in &names {
            input.insert(name.clone());
            est.push((name.as_str(), 10u64));
        }
        let snap = snapshot(&est);
        let mut pending = PendingDirtyRoots::default();
        let plan = plan_dirty_batch(input, false, &snap, &mut pending, 1);
        assert!(matches!(plan.scope, ScanScope::DirtySubtrees { .. }));
        assert_eq!(scoped(&plan).len(), MAX_DIRTY_SUBTREES);
        assert_eq!(pending.roots().len(), 200 - MAX_DIRTY_SUBTREES);
    }

    // A single dirty root larger than the budget runs alone and is flagged.
    #[test]
    fn over_budget_single_root_runs_alone() {
        let input = roots(&["huge", "small_a", "small_b"]);
        let snap = snapshot(&[("huge", 50_000), ("small_a", 10), ("small_b", 10)]);
        let mut pending = PendingDirtyRoots::default();
        // Force `huge` to the front by aging it.
        pending.roots.insert(
            "huge".to_string(),
            PendingRoot {
                first_seen_tick: 0,
                deferral_count: MAX_DEFERRAL_TICKS,
            },
        );
        let plan = plan_dirty_batch(input, false, &snap, &mut pending, 3);
        assert_eq!(scoped(&plan), &roots(&["huge"]));
        assert!(plan.cost.over_budget_single_root);
        assert!(pending.roots().contains("small_a"));
    }

    // Coalescing is a pure function of the input set: insertion order does not
    // change the result.
    #[test]
    fn coalescing_is_order_independent() {
        let snap = snapshot(&[("a", 40), ("b", 40)]);
        let forward: BTreeSet<String> = (0..40)
            .flat_map(|i| [format!("a/{i}"), format!("b/{i}")])
            .collect();
        let reverse: BTreeSet<String> = (0..40)
            .rev()
            .flat_map(|i| [format!("b/{i}"), format!("a/{i}")])
            .collect();
        let a = coalesce(
            &forward,
            &snap,
            PART_B_MAX_SCOPED_INDEX_ROWS,
            MAX_DIRTY_SUBTREES,
        );
        let b = coalesce(
            &reverse,
            &snap,
            PART_B_MAX_SCOPED_INDEX_ROWS,
            MAX_DIRTY_SUBTREES,
        );
        assert_eq!(a, b);
        assert_eq!(a, roots(&["a", "b"]));
    }

    // Exactly at the cap: no coalescing.
    #[test]
    fn cap_boundary_does_not_coalesce() {
        let names: Vec<String> = (0..MAX_DIRTY_SUBTREES)
            .map(|i| format!("r{i:03}"))
            .collect();
        let input: BTreeSet<String> = names.iter().cloned().collect();
        let est: Vec<(&str, u64)> = names.iter().map(|n| (n.as_str(), 5u64)).collect();
        let snap = snapshot(&est);
        let mut pending = PendingDirtyRoots::default();
        let plan = plan_dirty_batch(input.clone(), false, &snap, &mut pending, 1);
        assert_eq!(scoped(&plan), &input);
        assert!(!plan.cost.coalesced);
        assert!(!plan.cost.batched);
    }

    // Starvation guard: a large root left pending across two ticks is promoted
    // into the batch on the third even as small roots keep arriving.
    #[test]
    fn pending_root_promoted_after_two_deferrals() {
        let snap = snapshot(&[("big", 9_000), ("s0", 10), ("s1", 10)]);
        let mut pending = PendingDirtyRoots::default();
        // Tick 1: big deferred behind a full batch of cheap roots.
        let mut fresh1 = roots(&["big"]);
        for i in 0..MAX_DIRTY_SUBTREES {
            fresh1.insert(format!("c{i:03}"));
        }
        let est1: Vec<(&str, u64)> = std::iter::once(("big", 9_000u64))
            .chain((0..MAX_DIRTY_SUBTREES).map(|_| ("c", 1u64)))
            .collect();
        // Give every cheap root a small estimate; unknown ones default over-budget.
        let mut map: BTreeMap<String, u64> = BTreeMap::new();
        map.insert("big".to_string(), 9_000);
        for i in 0..MAX_DIRTY_SUBTREES {
            map.insert(format!("c{i:03}"), 1);
        }
        let snap1 =
            ChangeIndexSnapshot::from_parts(BTreeSet::new(), map.clone(), Default::default());
        let _ = est1;
        let plan1 = plan_dirty_batch(fresh1, false, &snap1, &mut pending, 1);
        assert!(!scoped(&plan1).contains("big"));
        assert!(pending.roots().contains("big"));

        // Tick 2: still deferred (aged to 2).
        let mut fresh2 = BTreeSet::new();
        for i in 0..MAX_DIRTY_SUBTREES {
            fresh2.insert(format!("d{i:03}"));
            map.insert(format!("d{i:03}"), 1);
        }
        let snap2 =
            ChangeIndexSnapshot::from_parts(BTreeSet::new(), map.clone(), Default::default());
        let plan2 = plan_dirty_batch(fresh2, false, &snap2, &mut pending, 2);
        assert!(!scoped(&plan2).contains("big"));

        // Tick 3: forced to the front, scheduled despite new cheap arrivals.
        let mut fresh3 = BTreeSet::new();
        for i in 0..MAX_DIRTY_SUBTREES {
            fresh3.insert(format!("e{i:03}"));
            map.insert(format!("e{i:03}"), 1);
        }
        let snap3 = ChangeIndexSnapshot::from_parts(BTreeSet::new(), map, Default::default());
        let plan3 = plan_dirty_batch(fresh3, false, &snap3, &mut pending, 3);
        assert!(scoped(&plan3).contains("big"));
        let _ = snap;
    }

    // Root-shallow flag rides through to the emitted combined scope.
    #[test]
    fn root_shallow_flag_preserved_on_batch() {
        let input = roots(&["src"]);
        let snap = snapshot(&[("src", 10)]);
        let mut pending = PendingDirtyRoots::default();
        let plan = plan_dirty_batch(input, true, &snap, &mut pending, 1);
        match plan.scope {
            ScanScope::DirtySubtrees { root_shallow, .. } => assert!(root_shallow),
            other => panic!("expected combined scope, got {other:?}"),
        }
    }

    // A pending child coalesced into a scheduled ancestor is cleared, not
    // orphaned forever (regression guard for the clear_scheduled ancestor case).
    #[test]
    fn pending_child_cleared_when_ancestor_scheduled() {
        let mut pending = PendingDirtyRoots::default();
        // `a/1` was deferred on an earlier tick.
        pending.defer("a/1".to_string(), 1);
        // This tick, >cap `a/*` roots arrive and coalesce to `a`, which schedules.
        let mut fresh = BTreeSet::new();
        let mut est = BTreeMap::new();
        est.insert("a".to_string(), 40u64);
        for i in 0..(MAX_DIRTY_SUBTREES + 40) {
            fresh.insert(format!("a/{i}"));
            est.insert(format!("a/{i}"), 1u64);
        }
        est.insert("a/1".to_string(), 1u64);
        let snap = ChangeIndexSnapshot::from_parts(BTreeSet::new(), est, Default::default());
        let plan = plan_dirty_batch(fresh, false, &snap, &mut pending, 2);
        assert_eq!(scoped(&plan), &roots(&["a"]));
        // `a/1` is subsumed by the scheduled `a`; it must not linger in pending.
        assert!(
            pending.is_empty(),
            "pending leaked a coalesced child: {pending:?}"
        );
    }

    // Pure root-shallow tick (no subtree roots, no pending) stays RootShallow.
    #[test]
    fn pure_root_shallow_tick_stays_shallow() {
        let snap = snapshot(&[]);
        let mut pending = PendingDirtyRoots::default();
        let plan = plan_dirty_batch(BTreeSet::new(), true, &snap, &mut pending, 1);
        assert_eq!(plan.scope, ScanScope::RootShallow);
    }
}
