//! Scheduling and consuming signature-verified reactive ref observations.

use std::sync::Arc;

use std::collections::BTreeSet;

use super::pull_apply::{PullScope, pull_from_observation};
use super::push::WatcherEvidence;
use super::{
    Clock, CycleError, DEBOUNCE_MS, EngineIo, ManifestEngine, PullDeps, RefObservation,
    RemoteObjects, RemoteRef, WorkspacePath, pull, pull_cycle_error,
};

/// How much the driver knows about local divergence when it starts a pull.
///
/// A pull only re-observes paths that can produce merge work, and the dirty set
/// is what stands in for "everything the remote did not move". Whether that set
/// was just rebuilt from a full stat walk decides whether the narrowing can be
/// proved (debug builds) or merely trusted.
#[derive(Clone, Copy)]
pub(super) enum LocalObservation {
    /// Watcher events are the only evidence since the last cycle.
    Reactive,
    /// A full stat walk refreshed the dirty set inside this same cycle.
    FreshlyWalked,
}

impl LocalObservation {
    /// How much a matching stat fingerprint may be trusted for this cycle's
    /// push. A cycle that had to stat-walk did so precisely because the tree
    /// went unobserved for an unbounded interval.
    pub(super) fn watcher_evidence(self) -> WatcherEvidence {
        match self {
            Self::Reactive => WatcherEvidence::Continuous,
            Self::FreshlyWalked => WatcherEvidence::Gapped,
        }
    }

    fn scope<'a>(self, dirty: &'a BTreeSet<WorkspacePath>) -> PullScope<'a> {
        match self {
            Self::Reactive => PullScope::ChangedAndDirty(dirty),
            Self::FreshlyWalked => PullScope::ChangedAndWalked(dirty),
        }
    }
}

impl ManifestEngine {
    /// Retain a useful live observation and report whether it requires a pull.
    ///
    /// A subscription may deliver an authenticated value that was queued before
    /// this device completed its own newer CAS. That value is stale transport
    /// history, not a hosted rollback, so it must never reach the durable
    /// freshness ratchet as if it were the current authoritative ref.
    pub(super) fn coalesce_ref_hint(&mut self, observed: RefObservation) -> bool {
        if self.force_ref_read {
            return true;
        }
        if let Some(head) = self.head_ref.as_ref() {
            if observed.version < head.version
                || (observed.version == head.version && observed.manifest_key == head.manifest_key)
            {
                return false;
            }
            if observed.version == head.version {
                self.pending_ref_hint = None;
                self.force_ref_read = true;
                return true;
            }
        }
        match self.pending_ref_hint.as_ref() {
            None => {
                self.pending_ref_hint = Some(observed);
            }
            Some(current) if observed.version > current.version => {
                self.pending_ref_hint = Some(observed);
            }
            Some(current)
                if observed.version == current.version
                    && observed.manifest_key != current.manifest_key =>
            {
                // Conflicting same-version observations are not eligible for
                // the fast path. A synchronous read re-establishes authority;
                // the durable freshness ratchet still rejects a proven fork.
                self.pending_ref_hint = None;
                self.force_ref_read = true;
            }
            Some(_) => {}
        }
        true
    }

    pub(super) fn do_pull<O: RemoteObjects, R: RemoteRef, C: Clock>(
        &mut self,
        io: &EngineIo<'_, O, R, C>,
        observation: LocalObservation,
    ) -> Result<(), CycleError> {
        let deps = PullDeps {
            ctx: &self.ctx,
            objects: io.objects,
            refs: io.refs,
            scope: observation.scope(&self.dirty),
        };
        let observed = if self.force_ref_read {
            self.pending_ref_hint = None;
            None
        } else {
            self.pending_ref_hint.take()
        };
        self.force_ref_read = false;
        let result = match observed {
            Some(observed) => pull_from_observation(&mut self.store, &deps, observed),
            None => pull(&mut self.store, &deps),
        };
        if result.is_err() {
            // An unconsumed/failed hint is never replayed as authority. The
            // retry re-reads synchronously to resolve transport ambiguity.
            self.force_ref_read = true;
        }
        let outcome = result.map_err(pull_cycle_error)?;
        if let (Some(version), Some(key)) =
            (outcome.ref_version, outcome.applied_manifest_key.clone())
        {
            self.head_ref = Some(RefObservation {
                version,
                manifest_key: key,
            });
        }
        self.applied_manifest = outcome.applied_manifest_key;
        // Kept-local divergences and freshly materialized asides must push back.
        Arc::make_mut(&mut self.dirty).extend(outcome.push_again);
        Arc::make_mut(&mut self.dirty).extend(outcome.conflict_asides);
        // A deferred path (active Git lock) auto-rescans once the lock clears.
        if !outcome.deferred.is_empty() {
            self.pull_needed = true;
            self.debounce_deadline = Some(io.clock.now_millis() + DEBOUNCE_MS);
        }
        Ok(())
    }
}
