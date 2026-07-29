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
                &self.dirty,
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
                    self.applied_manifest = Some(manifest_key);
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
        self.dirty = Arc::new(skipped);
    }
}
