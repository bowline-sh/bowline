use std::{collections::BTreeSet, sync::Arc};

use super::{Clock, Degradation, EngineIo, ManifestEngine, RemoteObjects, RemoteRef, StateSig};

impl ManifestEngine {
    pub(super) fn idle(&self) -> bool {
        self.dirty.is_empty()
            && self.dirty_subtrees.is_empty()
            && !self.scan_required
            && !self.pull_needed
    }

    pub(super) fn degradation_is_transient(&self) -> bool {
        matches!(
            self.degradation,
            Degradation::OfflineRetrying { .. } | Degradation::FullScanRequired(_)
        )
    }

    pub(super) fn refresh_and_bump<O: RemoteObjects, R: RemoteRef, C: Clock>(
        &mut self,
        _io: &EngineIo<'_, O, R, C>,
    ) {
        if let Ok(state) = self.store.engine_state() {
            self.applied_manifest = state.applied_manifest_key;
        }
        if let Ok(intents) = self.store.pending_intents() {
            self.pending_intents = intents.len();
            self.pending_intent_paths = Arc::new(
                intents
                    .into_iter()
                    .map(|intent| intent.path)
                    .collect::<BTreeSet<_>>(),
            );
        }
        self.bump_revision_if_changed();
    }

    pub(super) fn bump_revision_if_changed(&mut self) {
        let sig = StateSig {
            phase: self.phase,
            degradation: self.degradation,
            applied_manifest: self.applied_manifest.clone(),
            observed_version: self.head_ref.as_ref().map(|observed| observed.version),
            pending_intents: self.pending_intents,
            pending_intent_paths: self.pending_intent_paths.clone(),
            dirty_paths: self.dirty.clone(),
            dirty_subtree_paths: self.dirty_subtrees.clone(),
            scan_required: self.scan_required,
            unattributed_pull_pending: self.unattributed_pull_pending,
            cycle_active: self.cycle_active,
        };
        if self.last_sig.as_ref() != Some(&sig) {
            self.revision += 1;
            self.last_sig = Some(sig);
        }
    }
}
