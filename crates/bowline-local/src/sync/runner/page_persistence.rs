use super::*;

const LOCAL_METADATA_CACHE_MAX_RECORDS_PER_SNAPSHOT: u64 = 1_000_000;
const LOCAL_METADATA_CACHE_MAX_BYTES_PER_SNAPSHOT: u64 = 4 * 1024 * 1024 * 1024;

struct LocalMetadataCacheAdmission {
    records: u64,
    encoded_bytes: u64,
    maximum_records: u64,
    maximum_bytes: u64,
}

impl Default for LocalMetadataCacheAdmission {
    fn default() -> Self {
        Self {
            records: 0,
            encoded_bytes: 0,
            maximum_records: LOCAL_METADATA_CACHE_MAX_RECORDS_PER_SNAPSHOT,
            maximum_bytes: LOCAL_METADATA_CACHE_MAX_BYTES_PER_SNAPSHOT,
        }
    }
}

impl LocalMetadataCacheAdmission {
    fn admit(&mut self, encoded_bytes: u64) -> Result<(), NamespaceReadError> {
        let next_records = self.records.saturating_add(1);
        if next_records > self.maximum_records {
            return Err(NamespaceReadError::BudgetExceeded {
                resource: bowline_core::namespace_snapshot::NamespaceResource::NamespacePagesLoaded,
                observed: next_records,
                limit: self.maximum_records,
            });
        }
        let next_bytes = self.encoded_bytes.saturating_add(encoded_bytes);
        if next_bytes > self.maximum_bytes {
            return Err(NamespaceReadError::BudgetExceeded {
                resource: bowline_core::namespace_snapshot::NamespaceResource::MetadataBytes,
                observed: next_bytes,
                limit: self.maximum_bytes,
            });
        }
        self.records = next_records;
        self.encoded_bytes = next_bytes;
        Ok(())
    }
}

impl<'a> SyncRunner<'a> {
    pub(super) fn persist_snapshot_page_authority(
        &self,
        store: &mut MetadataStore,
        snapshot: &SnapshotContent,
    ) -> Result<(), SyncRunnerError> {
        store.register_metadata_identity_key(
            &self.options.workspace_id,
            snapshot.namespace_store().identity_key().as_bytes(),
            &self.options.generated_at,
        )?;
        let cache_root = self.options.state_root.join("metadata-pages");
        fs::create_dir_all(&cache_root).map_err(SyncRunnerError::StateIo)?;
        self.check_claim_during_page_persistence(store)?;
        let mut cache_context = NamespaceOperationContext::uncancelled(
            crate::sync::namespace::operation_budget(0, 0, 0),
        );
        let mut cache_admission = LocalMetadataCacheAdmission::default();
        let mut records_since_claim_check = 0_usize;
        let root_id = snapshot.namespace_snapshot().namespace_root_id.as_str();
        let mut dependency_candidates = std::collections::VecDeque::from([root_id.to_string()]);
        snapshot
            .namespace_store()
            .visit_new_reachable_plaintext_records(
                &snapshot.namespace_snapshot().namespace_root_id,
                &mut cache_context,
                |record| {
                    dependency_candidates.extend(record.summary.child_logical_ids.iter().cloned());
                    if records_since_claim_check == 16 {
                        self.check_claim_during_page_persistence(store)?;
                        records_since_claim_check = 0;
                    }
                    cache_admission.admit(record.plaintext.len() as u64)?;
                    persist_metadata_cache_record(self, store, &cache_root, record)?;
                    records_since_claim_check += 1;
                    Ok::<(), SyncRunnerError>(())
                },
            )?;
        let mut checked_dependencies = BTreeSet::new();
        let mut recovered_dependencies = Vec::new();
        while let Some(logical_id) = dependency_candidates.pop_front() {
            if !checked_dependencies.insert(logical_id.clone()) {
                continue;
            }
            let kind = local_metadata_kind_for_id(&logical_id);
            let reference = MetadataRecordRef {
                kind,
                logical_id: MetadataLogicalId::new(&logical_id),
            };
            if local_metadata_record_is_verified(store, &self.options.workspace_id, &reference)? {
                continue;
            }
            if records_since_claim_check == 16 {
                self.check_claim_during_page_persistence(store)?;
                records_since_claim_check = 0;
            }
            let record = snapshot
                .namespace_store()
                .plaintext_record(&logical_id)?
                .ok_or(NamespaceReadError::MissingRecord {
                    record: "reused snapshot metadata dependency",
                })?;
            dependency_candidates.extend(record.summary.child_logical_ids.iter().cloned());
            cache_admission.admit(record.plaintext.len() as u64)?;
            persist_metadata_cache_record(self, store, &cache_root, record)?;
            recovered_dependencies.push(logical_id);
            records_since_claim_check += 1;
        }

        let mut binding_context = NamespaceOperationContext::uncancelled(
            crate::sync::namespace::operation_budget(0, 0, 0),
        );
        let mut pending = Vec::with_capacity(16);
        let mut resolve_bindings = true;
        for logical_id in recovered_dependencies {
            pending.push(
                snapshot
                    .namespace_store()
                    .plaintext_record(&logical_id)?
                    .ok_or(NamespaceReadError::MissingRecord {
                        record: "reused snapshot metadata dependency",
                    })?,
            );
            if pending.len() == 16 {
                persist_metadata_binding_batch(self, store, &mut pending, &mut resolve_bindings)?;
            }
        }
        snapshot
            .namespace_store()
            .visit_new_reachable_plaintext_records(
                &snapshot.namespace_snapshot().namespace_root_id,
                &mut binding_context,
                |record| {
                    pending.push(record);
                    if pending.len() == 16 {
                        persist_metadata_binding_batch(
                            self,
                            store,
                            &mut pending,
                            &mut resolve_bindings,
                        )?;
                    }
                    Ok::<(), SyncRunnerError>(())
                },
            )?;
        persist_metadata_binding_batch(self, store, &mut pending, &mut resolve_bindings)?;
        let manifest = snapshot.manifest();
        let snapshot_record = SnapshotRecord {
            id: manifest.snapshot_id.clone(),
            workspace_id: manifest.workspace_id.clone(),
            project_id: manifest.project_id.clone(),
            kind: manifest.kind,
            base_snapshot_id: manifest.base_snapshot_id.clone(),
            root_id: manifest.namespace_root_id.clone(),
            semantic_manifest_digest: manifest.semantic_manifest_digest.clone(),
            entry_count: manifest.entry_count,
            refs: manifest.refs.clone(),
            created_at: self.options.generated_at.clone(),
        };
        self.check_claim_during_page_persistence(store)?;
        store.commit_snapshot_root_uncommitted(
            &snapshot_record,
            &[],
            &self.options.generated_at,
        )?;
        Ok(())
    }

    pub(super) fn rebuild_current_namespace_for_scan(
        &self,
        store: &mut MetadataStore,
        snapshot: &SnapshotContent,
        candidate: &crate::sync::SnapshotCandidate,
    ) -> Result<(), SyncRunnerError> {
        let write_scope = ObservationWriteScope::for_scan_scope(&candidate.scan_scope);
        // The committed bound snapshot is the projection authority. The candidate
        // supplies only the observed write scope; it may intentionally omit
        // unchanged entries outside the scanner's changed frontier.
        let prefixes = projection_prefixes(&write_scope);
        let mut slices = prefixes
            .iter()
            .cloned()
            .map(ProjectionSlice::Component)
            .collect::<Vec<_>>();
        if projection_owns_root_level(&write_scope) {
            slices.insert(0, ProjectionSlice::RootLevel);
        }
        if slices.is_empty() {
            return Ok(());
        }
        store.replace_current_namespace_projection_stream_uncommitted(
            &self.options.workspace_id,
            &snapshot.manifest().snapshot_id,
            &slices,
            |slice, sink| match slice {
                ProjectionSlice::RootLevel => visit_snapshot_prefix_descriptors(
                    snapshot,
                    &WorkspaceRelativePath::new(""),
                    &mut |descriptor| {
                        let entry = &descriptor.entry_without_layout;
                        if entry.path.contains('/')
                            || prefixes.iter().any(|prefix| {
                                WorkspaceRelativePath::new(&entry.path).is_equal_to_or_below(prefix)
                            })
                        {
                            return Ok(true);
                        }
                        let path = WorkspaceRelativePath::new(&entry.path);
                        sink(current_namespace_record(
                            snapshot,
                            entry,
                            descriptor.content_layout_id.clone(),
                            path,
                            HydrationState::Local,
                            &self.options.generated_at,
                        )?)?;
                        Ok(true)
                    },
                ),
                ProjectionSlice::Component(prefix) => {
                    visit_snapshot_prefix_descriptors(snapshot, prefix, &mut |descriptor| {
                        let entry = &descriptor.entry_without_layout;
                        if write_scope.owns_path(&entry.path) {
                            sink(current_namespace_record(
                                snapshot,
                                entry,
                                descriptor.content_layout_id.clone(),
                                prefix.clone(),
                                HydrationState::Local,
                                &self.options.generated_at,
                            )?)?;
                        }
                        Ok(true)
                    })
                }
            },
        )?;
        Ok(())
    }

    pub(super) fn rebuild_current_namespace_projection_full(
        &self,
        store: &mut MetadataStore,
        snapshot: &SnapshotContent,
    ) -> Result<(), SyncRunnerError> {
        let prefix = WorkspaceRelativePath::new("");
        store.replace_current_namespace_projection_stream_uncommitted(
            &self.options.workspace_id,
            &snapshot.manifest().snapshot_id,
            &[ProjectionSlice::Component(prefix.clone())],
            |_, sink| {
                visit_snapshot_prefix_descriptors(snapshot, &prefix, &mut |descriptor| {
                    sink(current_namespace_record(
                        snapshot,
                        &descriptor.entry_without_layout,
                        descriptor.content_layout_id.clone(),
                        prefix.clone(),
                        descriptor.entry_without_layout.hydration_state,
                        &self.options.generated_at,
                    )?)?;
                    Ok(true)
                })
            },
        )?;
        Ok(())
    }
}

fn persist_metadata_cache_record(
    runner: &SyncRunner<'_>,
    store: &mut MetadataStore,
    cache_root: &Path,
    record: crate::sync::namespace::MetadataPlaintextRecord,
) -> Result<(), SyncRunnerError> {
    use bowline_core::fs_atomic::{AtomicWriteOptions, write_atomic};

    let kind = local_metadata_kind(record.summary.kind);
    let logical_id = MetadataLogicalId::new(&record.summary.logical_id);
    let reference = MetadataRecordRef {
        kind,
        logical_id: logical_id.clone(),
    };
    store.register_metadata_record(
        &runner.options.workspace_id,
        &reference,
        &runner.options.generated_at,
    )?;
    let cache_path = cache_root.join(format!("{}.page", logical_id.as_str()));
    write_atomic(
        &cache_path,
        &record.plaintext,
        AtomicWriteOptions {
            unix_mode: Some(0o600),
            reject_symlink: true,
            replace_existing: true,
        },
    )
    .map_err(SyncRunnerError::StateIo)?;
    store.put_metadata_cache_record(&MetadataCacheRecord {
        workspace_id: runner.options.workspace_id.clone(),
        logical_id,
        kind,
        cache_path: Some(cache_path.display().to_string()),
        encoded_bytes: record.plaintext.len() as u64,
        state: MetadataCacheState::Present,
        last_accessed_at: runner.options.generated_at.clone(),
    })?;
    Ok(())
}

fn persist_metadata_binding_batch(
    runner: &SyncRunner<'_>,
    store: &mut MetadataStore,
    pending: &mut Vec<crate::sync::namespace::MetadataPlaintextRecord>,
    resolve_bindings: &mut bool,
) -> Result<(), SyncRunnerError> {
    if pending.is_empty() {
        return Ok(());
    }
    runner.check_claim_during_page_persistence(store)?;
    let mut unresolved = Vec::new();
    for record in pending.iter() {
        let kind = local_metadata_kind(record.summary.kind);
        let logical_id = MetadataLogicalId::new(&record.summary.logical_id);
        if !store
            .metadata_object_binding(&runner.options.workspace_id, kind, &logical_id)?
            .is_some_and(|binding| {
                binding.verification_state == MetadataVerificationState::Verified
            })
        {
            unresolved.push(record.summary.logical_id.clone());
        }
    }
    let resolved = if *resolve_bindings && !unresolved.is_empty() {
        match runner
            .control_plane
            .resolve_metadata_bindings(&runner.options.workspace_id, &unresolved)
        {
            Ok(response) => response
                .bindings
                .into_iter()
                .map(|binding| (binding.logical_id.clone(), binding))
                .collect::<BTreeMap<_, _>>(),
            Err(_) => {
                // Local canonical pages remain independently verified. A later
                // online import/upload can fill optional physical bindings.
                *resolve_bindings = false;
                BTreeMap::new()
            }
        }
    } else {
        BTreeMap::new()
    };
    for record in pending.drain(..) {
        let kind = local_metadata_kind(record.summary.kind);
        let logical_id = MetadataLogicalId::new(&record.summary.logical_id);
        if let Some(binding) = resolved.get(&record.summary.logical_id) {
            store.insert_metadata_object_binding(&MetadataObjectBindingRecord {
                workspace_id: runner.options.workspace_id.clone(),
                logical_id: logical_id.clone(),
                kind,
                object_key: MetadataObjectKey::new(&binding.object.object_key),
                byte_len: binding.object.byte_len,
                object_hash: binding.object.hash.clone(),
                key_epoch: binding.object.key_epoch,
                verification_state: MetadataVerificationState::Verified,
                created_at: runner.options.generated_at.clone(),
                verified_at: Some(runner.options.generated_at.clone()),
            })?;
        }
        let parent = MetadataRecordRef { kind, logical_id };
        let children = record
            .summary
            .child_logical_ids
            .iter()
            .map(|id| MetadataRecordRef {
                kind: local_metadata_kind_for_id(id),
                logical_id: MetadataLogicalId::new(id),
            })
            .collect::<Vec<_>>();
        store.replace_metadata_record_edges(&runner.options.workspace_id, &parent, &children)?;
    }
    Ok(())
}

fn local_metadata_kind(
    kind: crate::sync::namespace::MetadataRecordKind,
) -> LocalMetadataRecordKind {
    match kind {
        crate::sync::namespace::MetadataRecordKind::NamespacePage => {
            LocalMetadataRecordKind::NamespacePage
        }
        crate::sync::namespace::MetadataRecordKind::ContentLayout => {
            LocalMetadataRecordKind::ContentLayout
        }
        crate::sync::namespace::MetadataRecordKind::SegmentPage => {
            LocalMetadataRecordKind::SegmentPage
        }
    }
}

fn local_metadata_kind_for_id(logical_id: &str) -> LocalMetadataRecordKind {
    if logical_id.starts_with("nsp_") {
        LocalMetadataRecordKind::NamespacePage
    } else if logical_id.starts_with("ctl_") {
        LocalMetadataRecordKind::ContentLayout
    } else {
        LocalMetadataRecordKind::SegmentPage
    }
}

fn local_metadata_record_is_verified(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    record: &MetadataRecordRef,
) -> Result<bool, MetadataError> {
    if store
        .metadata_object_binding(workspace_id, record.kind, &record.logical_id)?
        .is_some_and(|binding| binding.verification_state == MetadataVerificationState::Verified)
    {
        return Ok(true);
    }
    Ok(store
        .metadata_cache_record(workspace_id, record)?
        .is_some_and(|cache| cache.state == MetadataCacheState::Present))
}

fn projection_prefixes(scope: &ObservationWriteScope) -> Vec<WorkspaceRelativePath> {
    match scope {
        ObservationWriteScope::Full => vec![WorkspaceRelativePath::new("")],
        ObservationWriteScope::UnderRoots(roots) => {
            roots.iter().map(WorkspaceRelativePath::new).collect()
        }
        ObservationWriteScope::RootLevelOnly => Vec::new(),
        ObservationWriteScope::UnderRootsAndRootLevel(roots) => {
            roots.iter().map(WorkspaceRelativePath::new).collect()
        }
    }
}

fn projection_owns_root_level(scope: &ObservationWriteScope<'_>) -> bool {
    matches!(
        scope,
        ObservationWriteScope::RootLevelOnly | ObservationWriteScope::UnderRootsAndRootLevel(_)
    )
}

fn current_namespace_record(
    snapshot: &SnapshotContent,
    entry: &NamespaceEntry,
    content_layout_id: Option<bowline_core::ids::ContentLayoutId>,
    component_prefix: WorkspaceRelativePath,
    hydration_state: HydrationState,
    updated_at: &str,
) -> Result<CurrentNamespaceEntryRecord, SyncRunnerError> {
    Ok(CurrentNamespaceEntryRecord {
        workspace_id: snapshot.manifest().workspace_id.clone(),
        snapshot_id: snapshot.manifest().snapshot_id.clone(),
        project_id: snapshot.manifest().project_id.clone(),
        component_prefix,
        path: WorkspaceRelativePath::new(&entry.path),
        kind: entry.kind,
        classification: entry.classification,
        mode: entry.mode,
        access: entry.access.clone(),
        content_id: entry.content_id.clone(),
        content_layout_id,
        symlink_target: entry.symlink_target.clone(),
        byte_len: entry.byte_len,
        executability: entry.executability,
        hydration_state,
        updated_at: updated_at.to_string(),
    })
}

#[cfg(test)]
mod cache_admission_tests {
    use super::*;
    use bowline_core::namespace_snapshot::NamespaceResource;

    #[test]
    fn local_metadata_cache_admission_has_hard_count_and_byte_limits() {
        let mut count_limited = LocalMetadataCacheAdmission {
            records: 0,
            encoded_bytes: 0,
            maximum_records: 2,
            maximum_bytes: 100,
        };
        count_limited.admit(3).expect("first record");
        count_limited.admit(3).expect("second record");
        assert!(matches!(
            count_limited.admit(1),
            Err(NamespaceReadError::BudgetExceeded {
                resource: NamespaceResource::NamespacePagesLoaded,
                observed: 3,
                limit: 2,
            })
        ));

        let mut byte_limited = LocalMetadataCacheAdmission {
            records: 0,
            encoded_bytes: 0,
            maximum_records: 3,
            maximum_bytes: 5,
        };
        byte_limited.admit(3).expect("first byte admission");
        assert!(matches!(
            byte_limited.admit(3),
            Err(NamespaceReadError::BudgetExceeded {
                resource: NamespaceResource::MetadataBytes,
                observed: 6,
                limit: 5,
            })
        ));
    }
}
