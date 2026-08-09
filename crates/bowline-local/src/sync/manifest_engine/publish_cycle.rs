//! The publishing half of a cycle: the bounded push-with-one-retry over the
//! dirty set, and the rule for which push failures may still let the same cycle
//! pull.
//!
//! Split from `mod.rs` at the same seam as its siblings: `scan_cycle` owns what
//! a cycle observes, `ref_observation` owns what it receives, and this owns what
//! it publishes. That seam is load-bearing here, because "cannot publish" and
//! "must stop" are different answers and the engine has to keep them apart.

use std::collections::BTreeSet;
use std::sync::Arc;

use super::cycle_outcome::PushFailureScope;
use super::push::{self, DeletionAuthorization, PushDeps, PushOutcome};
use super::ref_observation::LocalObservation;
use super::{
    Clock, CycleError, EngineIo, MAX_PUSH_ATTEMPTS, ManifestEngine, RefObservation, RemoteObjects,
    RemoteRef, WorkspacePath, push_cycle_error,
};
use super::{DirtySeq, PUBLISH_BATCH_MAX};

impl ManifestEngine {
    /// The publishing half of a cycle: a bounded push-with-one-retry over the
    /// dirty set. `pulled` says whether this cycle has already applied the remote
    /// head, so a retry never spends a second round trip re-reading a ref it just
    /// read.
    ///
    /// Two things end an attempt short of publishing, and they are not the same
    /// thing. A lost CAS means another device got there first: pull the winner
    /// against the untouched ancestor and retry. A refused removal batch means
    /// this device is not allowed to publish until a human says so — but a device
    /// that may not publish may still RECEIVE, and receiving is usually how the
    /// refusal ends: when the remote has already removed those paths, the pull
    /// adopts the deletions as bookkeeping (no file is touched; they are already
    /// gone locally) and the retry's batch is empty, so the block clears with no
    /// confirmation at all. Every other push failure stops the cycle
    /// ([`CycleError::push_failure_scope`]).
    pub(super) fn publish_dirty<O: RemoteObjects, R: RemoteRef, C: Clock>(
        &mut self,
        io: &EngineIo<'_, O, R, C>,
        observation: LocalObservation,
        mut pulled: bool,
        deletions: DeletionAuthorization,
    ) -> Result<(), CycleError> {
        let mut attempts = 0u8;
        while !self.dirty.is_empty() && attempts < MAX_PUSH_ATTEMPTS {
            attempts += 1;
            // A second pass through this loop is a real retry after a lost CAS or
            // a refused batch this cycle then went and re-observed.
            if attempts > 1 {
                self.counters.record_retry();
            }
            let batch = self.publish_batch();
            let deps = PushDeps {
                ctx: &self.ctx,
                objects: io.objects,
                refs: io.refs,
            };
            // How the batch was discovered decides whether a matching stat
            // fingerprint settles a file. A watcher that has been attached since
            // the ancestor rows were written leaves only the racily-clean window
            // unobservable; a batch that came from a stat walk over an unbounded
            // daemon-down gap leaves everything unobservable, so its bytes are
            // read (see `endpoint`).
            let outcome = match push::push_dirty_paths_authorized(
                &mut self.store,
                &deps,
                &batch,
                deletions.clone(),
                observation.watcher_evidence(),
            ) {
                Ok(outcome) => outcome,
                Err(error) => {
                    let error = push_cycle_error(error);
                    if error.push_failure_scope() == PushFailureScope::StopsCycle || pulled {
                        return Err(error);
                    }
                    // The refusal published nothing and committed no ancestor
                    // write, so the pull classifies against exactly the base it
                    // would have used had the push never run.
                    self.do_pull(io, observation)?;
                    pulled = true;
                    continue;
                }
            };
            match outcome {
                PushOutcome::Advanced {
                    manifest_key,
                    ref_version,
                    skipped,
                } => {
                    self.head_ref = Some(RefObservation {
                        version: ref_version,
                        manifest_key: manifest_key.clone(),
                    });
                    self.applied_ref = super::EngineRef::Head(RefObservation {
                        version: ref_version,
                        manifest_key,
                    });
                    // Retain exactly the paths the scan could not settle (actively
                    // being written); everything published leaves the dirty set.
                    self.retain_skipped(skipped);
                    break;
                }
                PushOutcome::NoChange { skipped } => {
                    // No delta this cycle. Keep only the churning paths, if any.
                    self.retain_skipped(skipped);
                    break;
                }
                PushOutcome::RefLost { current } => {
                    // The ancestor and the local edit are untouched. Pull the
                    // winner against that unchanged base, then retry once.
                    self.head_ref = current;
                    // The dirty set is unchanged since the scan above, so this
                    // in-cycle re-pull inherits the same observation evidence.
                    self.do_pull(io, observation)?;
                    pulled = true;
                }
            }
        }
        Ok(())
    }

    /// Replace the dirty set with exactly the paths a push could not settle. On a
    /// successful/no-change push, every other dirty path is done, so retaining the
    /// skipped set both clears the completed work and re-arms the churning paths.
    /// The `break` at each call site is deliberate: a skip is NOT a lost CAS, so
    /// it must not consume a `MAX_PUSH_ATTEMPTS` retry by re-running push against a
    /// still-changing file in the same cycle — the later rescheduled cycle handles
    /// it instead.
    fn retain_skipped(&mut self, skipped: BTreeSet<WorkspacePath>) {
        if !skipped.is_empty() {
            self.counters.record_push_skip(skipped.len() as u64);
        }
        // Whatever this cycle did not carry is not a skip -- it was never
        // offered. It stays dirty so the rescheduled cycle takes it, and it is
        // not counted as churn.
        let leftover: BTreeSet<WorkspacePath> = self
            .dirty
            .iter()
            .filter(|path| !self.published_batch.contains(*path))
            .cloned()
            .collect();
        let retained: BTreeSet<WorkspacePath> = skipped.union(&leftover).cloned().collect();
        self.dirty_seen.retain(|path, _| retained.contains(path));
        self.dirty = Arc::new(retained);
        self.published_batch = BTreeSet::new();
    }

    /// The paths this cycle will publish.
    ///
    /// A publish that carries the whole backlog makes a fresh edit wait for it:
    /// the release proof measured two publishes in three and a half minutes, and
    /// a file written after a burst sat behind the burst's own backlog. Bound the
    /// batch and take the newest and oldest halves, so a new edit goes out in the
    /// next cycle while the backlog still drains in a bounded number of them.
    ///
    /// Removals are never split. The mass-deletion breaker judges one push
    /// against the whole workspace, so a batch carrying part of a removal set
    /// could pass under the threshold that the full set would trip. When the
    /// dirty set contains anything already gone from disk, this publishes all of
    /// it and lets the breaker see exactly what it sees today.
    fn publish_batch(&mut self) -> Arc<BTreeSet<WorkspacePath>> {
        if self.dirty.len() <= PUBLISH_BATCH_MAX {
            self.published_batch = self.dirty.as_ref().clone();
            return Arc::clone(&self.dirty);
        }
        let root = &self.ctx.workspace_root;
        if self
            .dirty
            .iter()
            .any(|path| !root.join(path.as_str()).symlink_metadata().is_ok())
        {
            self.published_batch = self.dirty.as_ref().clone();
            return Arc::clone(&self.dirty);
        }
        let mut ordered: Vec<(DirtySeq, WorkspacePath)> = self
            .dirty
            .iter()
            .map(|path| {
                (
                    self.dirty_seen
                        .get(path)
                        .copied()
                        .unwrap_or(DirtySeq::INITIAL),
                    path.clone(),
                )
            })
            .collect();
        ordered.sort();
        let half = PUBLISH_BATCH_MAX / 2;
        let mut batch: BTreeSet<WorkspacePath> = BTreeSet::new();
        for (_, path) in ordered.iter().take(half) {
            batch.insert(path.clone());
        }
        for (_, path) in ordered.iter().rev().take(PUBLISH_BATCH_MAX - half) {
            batch.insert(path.clone());
        }
        self.published_batch = batch.clone();
        Arc::new(batch)
    }
}
