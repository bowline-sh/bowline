use super::*;

#[derive(Debug, Clone, Copy)]
pub(super) enum FillBytesScope {
    AllHits,
    UploadShardSampled { verify_shard: u64 },
}

impl FillBytesScope {
    fn reads_bound_hit(self, path: &str) -> bool {
        match self {
            Self::AllHits => true,
            Self::UploadShardSampled { verify_shard } => {
                crate::sync::stat_cache::verify_shard_for_path(path) == verify_shard
            }
        }
    }
}

impl<'a> SyncRunner<'a> {
    pub(super) fn load_stat_cache_session(
        &self,
        scan_scope: &ScanScope,
    ) -> Result<StatCacheSession, SyncRunnerError> {
        self.with_store(|store| match scan_scope {
            // Combined tick: bounded roots + root-level rows through the indexed
            // projections; unrelated deep rows are never loaded.
            ScanScope::DirtySubtrees {
                roots: dirty_roots,
                root_shallow: true,
            } => StatCacheSession::load_roots_and_root_level(
                store,
                &self.options.workspace_id,
                dirty_roots,
                self.options.key_epoch,
                &self.options.workspace_content_key,
            ),
            ScanScope::DirtySubtrees {
                roots: dirty_roots,
                root_shallow: false,
            } => StatCacheSession::load_scoped(
                store,
                &self.options.workspace_id,
                dirty_roots,
                self.options.key_epoch,
                &self.options.workspace_content_key,
            ),
            // Root-shallow tick: only root-level rows through the indexed
            // root-level projection. Deep entries are preserved from the head
            // manifest, not by loading their cache rows.
            ScanScope::RootShallow => StatCacheSession::load_root_level(
                store,
                &self.options.workspace_id,
                self.options.key_epoch,
                &self.options.workspace_content_key,
            ),
            ScanScope::Full(_) => StatCacheSession::load(
                store,
                &self.options.workspace_id,
                self.options.key_epoch,
                &self.options.workspace_content_key,
            ),
        })
    }

    pub(super) fn preserved_exception_entries(
        &self,
        candidate_base_ref: &WorkspaceRef,
        excluded_paths: &BTreeSet<String>,
    ) -> Result<Vec<NamespaceEntry>, SyncRunnerError> {
        self.preserved_base_entries(candidate_base_ref, excluded_paths)
    }

    pub(super) fn effective_scan_scope(
        &self,
        local_head: Option<&WorkspaceRef>,
        local_head_snapshot: Option<&SnapshotContent>,
    ) -> Result<ScanScope, SyncRunnerError> {
        let requested = &self.options.scan_scope;
        // A full scan observes everything and preserves nothing from the head, so
        // it never degrades.
        if matches!(requested, ScanScope::Full(_)) {
            return Ok(requested.clone());
        }
        // Every partial pass (RootShallow, DirtySubtrees) preserves the head
        // entries it does not re-observe, so all of them require a usable head
        // manifest before the scope can stand.
        let Some(head_snapshot) = local_head_snapshot.filter(|_| local_head.is_some()) else {
            return self.degrade_scoped_scan(CheckpointReasonCode::HeadManifestUnavailable);
        };
        // A dirty-subtree tick with no roots cannot scope anything; treat it as a
        // full scan (a root-shallow tick has no roots to check here).
        if let ScanScope::DirtySubtrees { roots, .. } = requested
            && roots.is_empty()
        {
            return Ok(ScanScope::Full(FullScanReason::ReconcileFallback));
        }
        // Any preserved (not live-observed) File entry that lacks a packed locator
        // cannot be rehydrated after a partial pass drops it from the working set,
        // so degrade to a full scan. Root-level entries a shallow pass re-observes
        // are excluded because that pass rebinds them this tick.
        if has_unbound_preserved_file(head_snapshot, requested)? {
            return self.degrade_scoped_scan(CheckpointReasonCode::UnboundDeepFileEntry);
        }
        Ok(requested.clone())
    }

    /// Emit the `scoped-scan-degraded` checkpoint with a redacted reason code and
    /// fall back to a full scan. Shared by every partial-scope degrade path so the
    /// checkpoint step string and payload shape cannot drift.
    fn degrade_scoped_scan(
        &self,
        reason: CheckpointReasonCode,
    ) -> Result<ScanScope, SyncRunnerError> {
        self.record_sync_checkpoint(
            "scoped-scan-degraded",
            "limited",
            &checkpoint_payload(&ReasonPayload {
                reason: reason.as_code(),
            })?,
        )?;
        Ok(ScanScope::Full(FullScanReason::HeadManifestUnavailable))
    }

    pub(super) fn apply_stat_cache_write_back_to_store(
        &self,
        store: &mut MetadataStore,
        candidate: &crate::sync::SnapshotCandidate,
    ) -> Result<(), SyncRunnerError> {
        let Some(write_back) = &candidate.stat_cache_write_back else {
            return Ok(());
        };
        store.apply_stat_cache_write_back_uncommitted(
            &self.options.workspace_id,
            &write_back.upserts,
            &write_back.deletes,
        )?;
        #[cfg(feature = "fault-injection")]
        crate::sync::fault::trip(crate::sync::fault::FaultPoint::AfterStatCacheWriteBack)?;
        Ok(())
    }

    pub(super) fn with_store<T>(
        &self,
        operation: impl FnOnce(&mut MetadataStore) -> Result<T, MetadataError>,
    ) -> Result<T, SyncRunnerError> {
        self.with_store_sync(|store| operation(store).map_err(Into::into))
    }

    pub(super) fn with_store_sync<T>(
        &self,
        operation: impl FnOnce(&mut MetadataStore) -> Result<T, SyncRunnerError>,
    ) -> Result<T, SyncRunnerError> {
        if self.store.borrow().is_none() {
            *self.store.borrow_mut() = Some(MetadataStore::open(self.metadata_db_path())?);
        }
        let result = {
            let mut store = self.store.borrow_mut();
            let store = store.as_mut().expect("store initialized");
            operation(store)
        };
        if result.is_err() {
            *self.store.borrow_mut() = None;
        }
        result
    }

    pub(super) fn fill_candidate_bytes(
        &self,
        candidate: &mut crate::sync::SnapshotCandidate,
        scope: FillBytesScope,
    ) -> Result<(), SyncRunnerError> {
        let mut additions = BTreeMap::new();
        visit_snapshot_entries(&candidate.snapshot, &mut |entry| {
            if entry.kind != NamespaceEntryKind::File
                || entry.hydration_state != HydrationState::Local
            {
                return Ok(true);
            }
            let is_hit = candidate.stat_cache_hit_paths.contains(&entry.path);
            if entry.content_layout.is_some() && !is_hit {
                return Ok(true);
            }
            // A bound hit's bytes only feed reused-pack verification on Upload,
            // which samples one verify shard per tick. `AllHits` keeps every
            // pre-merge path byte-backed for merge's three-way tail.
            if entry.content_layout.is_some() && is_hit && !scope.reads_bound_hit(&entry.path) {
                return Ok(true);
            }
            let Some(content_id) = entry.content_id.as_ref() else {
                return Ok(true);
            };
            if candidate
                .snapshot
                .prepared_content()
                .contains_key(content_id)
            {
                return Ok(true);
            }
            let preparation_root = self.options.state_root.join("preparations");
            let prepared = crate::sync::coalescer::prepare_snapshot_path(
                crate::sync::coalescer::PrepareSnapshotPathRequest {
                    workspace_id: &self.options.workspace_id,
                    workspace_content_key: self.options.workspace_content_key,
                    workspace_root: &self.options.root,
                    preparation_root: &preparation_root,
                    relative_path: &entry.path,
                    created_at: &self.options.generated_at,
                    portable_git_worktree_link: worktree_link_file(&entry.path, entry.kind)
                        .is_some(),
                    owner_marker: candidate.snapshot.preparation_owner_marker(),
                },
            )?;
            let observed_content_id = prepared.content_id.clone();
            if &observed_content_id != content_id {
                self.record_stat_cache_divergence(&entry.path, content_id, &observed_content_id)?;
                return Err(SyncRunnerError::StatCacheDivergence {
                    path: entry.path.clone(),
                    cached_content_id: content_id.clone(),
                    observed_content_id,
                });
            }
            additions.insert(content_id.clone(), prepared);
            Ok(true)
        })?;
        candidate.snapshot.prepared_content_mut().extend(additions);
        Ok(())
    }

    pub(super) fn fail_if_candidate_has_stat_cache_divergence(
        &self,
        candidate: &crate::sync::SnapshotCandidate,
    ) -> Result<(), SyncRunnerError> {
        let Some(divergence) = candidate.stat_cache_divergences.first() else {
            return Ok(());
        };
        self.record_stat_cache_divergence(
            &divergence.path,
            &divergence.cached_content_id,
            &divergence.observed_content_id,
        )?;
        Err(SyncRunnerError::StatCacheDivergence {
            path: divergence.path.clone(),
            cached_content_id: divergence.cached_content_id.clone(),
            observed_content_id: divergence.observed_content_id.clone(),
        })
    }

    fn record_stat_cache_divergence(
        &self,
        path: &str,
        cached_content_id: &ContentId,
        observed_content_id: &ContentId,
    ) -> Result<(), SyncRunnerError> {
        self.with_store(|store| {
            store.clear_stat_cache(&self.options.workspace_id)?;
            let mut event = WorkspaceEvent::new(
                EventId::new(format!(
                    "evt_stat_cache_divergence_{}_{}",
                    event_id_component(path),
                    event_id_component(&self.options.generated_at)
                )),
                EventName::StatCacheDivergence,
                self.options.generated_at.clone(),
                EventSeverity::Attention,
                "Stat cache divergence detected; cache cleared for full recovery.".to_string(),
                self.options.workspace_id.clone(),
            );
            event.path = Some(path.to_string());
            event.device_id = Some(self.options.device_id.clone());
            event.subject = Some(EventSubject {
                kind: EventSubjectKind::Path,
                id: path.to_string(),
                path: Some(path.to_string()),
            });
            event.payload.insert(
                "cachedContentId".to_string(),
                serde_json::Value::String(cached_content_id.as_str().to_string()),
            );
            event.payload.insert(
                "observedContentId".to_string(),
                serde_json::Value::String(observed_content_id.as_str().to_string()),
            );
            store
                .append_event(event)
                .map_err(|error| MetadataError::InvalidStorageMetadata(error.to_string()))?;
            Ok(())
        })?;
        *self.last_scan_stats.borrow_mut() = {
            let mut stats = self.last_scan_stats();
            stats.divergence_count = stats.divergence_count.saturating_add(1);
            stats
        };
        Ok(())
    }
}

fn has_unbound_preserved_file(
    snapshot: &SnapshotContent,
    scan_scope: &ScanScope,
) -> Result<bool, SyncRunnerError> {
    let (namespace_page_limit, metadata_byte_limit) =
        crate::sync::namespace::lazy_namespace_read_limits(snapshot.manifest().entry_count);
    let mut operation =
        NamespaceOperationContext::uncancelled(
            NamespaceOperationBudget::new(snapshot.manifest().entry_count, 0, 0)
                .with_metadata_limits(namespace_page_limit, 0, 0, metadata_byte_limit),
        );
    let mut found = false;
    snapshot.namespace_reader().visit_prefix_descriptors(
        &WorkspaceRelativePath::new(""),
        &mut operation,
        &mut |descriptor| {
            let entry = descriptor.entry_without_layout;
            if entry.kind == NamespaceEntryKind::File
                && !crate::sync::stat_cache::path_is_live_observed(&entry.path, scan_scope)
                && descriptor.content_layout_id.is_none()
            {
                found = true;
                return Ok(NamespaceVisitControl::Stop);
            }
            Ok(NamespaceVisitControl::Continue)
        },
    )?;
    Ok(found)
}

#[cfg(test)]
mod tests;
