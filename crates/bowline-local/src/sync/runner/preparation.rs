use super::*;
use crate::metadata::{
    PreparationLeaseId, PreparationLeaseRecord, PreparationLeaseState, PreparedStagedContentRecord,
    SourceFingerprint,
};
use crate::sync::prepared_content::staged_content_matches;
use crate::sync::{
    PreparedContentSource, PreparedSnapshotLease, PreparedSourceFingerprint, short_hash,
};

impl<'a> SyncRunner<'a> {
    pub(super) fn persist_candidate_preparation(
        &self,
        candidate: &mut crate::sync::SnapshotCandidate,
    ) -> Result<(), SyncRunnerError> {
        if candidate.snapshot.preparation_lease().is_some() {
            return Ok(());
        }
        let staged = candidate
            .snapshot
            .prepared_content()
            .values()
            .filter_map(|content| match &content.source {
                PreparedContentSource::StagedFile { path, owner_marker } => {
                    Some((content, path, owner_marker))
                }
                PreparedContentSource::Memory(_) => None,
            })
            .collect::<Vec<_>>();
        let Some((_, _, owner_marker)) = staged.first().copied() else {
            return Ok(());
        };
        if staged
            .iter()
            .any(|(_, _, candidate_owner)| *candidate_owner != owner_marker)
        {
            return Err(MetadataError::InvalidStorageMetadata(
                "prepared snapshot contains more than one staging owner".to_string(),
            )
            .into());
        }
        let lease_id = PreparationLeaseId::new(format!(
            "lease_{}",
            short_hash([
                self.options.workspace_id.as_str().as_bytes(),
                candidate
                    .snapshot
                    .manifest()
                    .snapshot_id
                    .as_str()
                    .as_bytes(),
                owner_marker.as_str().as_bytes(),
            ])
        ));
        let reservation_bytes = staged
            .iter()
            .map(|(content, _, _)| content.logical_len)
            .try_fold(0_u64, u64::checked_add)
            .ok_or_else(|| {
                MetadataError::InvalidStorageMetadata(
                    "prepared snapshot reservation overflowed".to_string(),
                )
            })?;
        let now = self.options.generated_at.clone();
        self.with_store(|store| {
            store.insert_workspace(&self.options.workspace_id, "Code", &now)?;
            store.create_preparation_lease(&PreparationLeaseRecord {
                id: lease_id.clone(),
                workspace_id: self.options.workspace_id.clone(),
                project_id: None,
                snapshot_candidate_id: candidate.snapshot.manifest().snapshot_id.clone(),
                owner_marker: owner_marker.clone(),
                state: PreparationLeaseState::Preparing,
                reservation_bytes,
                prepared_at: None,
                referenced_at: None,
                finished_at: None,
                created_at: now.clone(),
                updated_at: now.clone(),
            })?;
            for (content, staged_path, _) in &staged {
                store.upsert_prepared_staged_content(&PreparedStagedContentRecord {
                    lease_id: lease_id.clone(),
                    content_id: content.content_id.clone(),
                    staged_path: (*staged_path).clone(),
                    logical_size: content.logical_len,
                    source_fingerprint: source_fingerprint(content.source_fingerprint),
                    owner_marker: owner_marker.clone(),
                    created_at: now.clone(),
                    updated_at: now.clone(),
                })?;
            }
            let transitioned = store.transition_preparation_lease(
                &lease_id,
                owner_marker,
                PreparationLeaseState::Preparing,
                PreparationLeaseState::Prepared,
                &now,
            )?;
            if !transitioned {
                return Err(MetadataError::InvalidStorageMetadata(
                    "preparation lease was not in preparing state".to_string(),
                ));
            }
            Ok(())
        })?;
        candidate
            .snapshot
            .attach_preparation_lease(PreparedSnapshotLease {
                id: lease_id,
                owner_marker: owner_marker.clone(),
            });
        Ok(())
    }

    pub(super) fn reference_candidate_preparation(
        &self,
        candidate: &crate::sync::SnapshotCandidate,
    ) -> Result<(), SyncRunnerError> {
        if self.transition_candidate_preparation_if_current(
            candidate,
            PreparationLeaseState::Prepared,
            PreparationLeaseState::ReferencedByUpload,
        )? {
            return Ok(());
        }
        let Some(lease) = candidate.snapshot.preparation_lease() else {
            return Ok(());
        };
        let current = self.with_store(|store| store.preparation_lease(&lease.id))?;
        if current.is_some_and(|record| record.state == PreparationLeaseState::ReferencedByUpload) {
            return Ok(());
        }
        Err(MetadataError::InvalidStorageMetadata(
            "preparation lease could not enter referenced-by-upload".to_string(),
        )
        .into())
    }

    pub(super) fn adopt_existing_preparation(
        &self,
        candidate: &mut crate::sync::SnapshotCandidate,
    ) -> Result<(), SyncRunnerError> {
        let leases =
            self.with_store(|store| store.preparation_leases(&self.options.workspace_id))?;
        for lease in leases.into_iter().rev().filter(|lease| {
            lease.snapshot_candidate_id == candidate.snapshot.manifest().snapshot_id
                && matches!(
                    lease.state,
                    PreparationLeaseState::Prepared | PreparationLeaseState::ReferencedByUpload
                )
        }) {
            let staged = self.with_store(|store| {
                store.prepared_staged_content(&lease.id, &lease.owner_marker)
            })?;
            let mut staged_is_valid = !staged.is_empty();
            for record in &staged {
                if !candidate
                    .snapshot
                    .prepared_content()
                    .contains_key(&record.content_id)
                    || !staged_content_matches(
                        &record.staged_path,
                        &record.content_id,
                        record.logical_size,
                        self.options.workspace_content_key,
                    )
                    .unwrap_or(false)
                {
                    staged_is_valid = false;
                    break;
                }
            }
            if !staged_is_valid {
                continue;
            }
            candidate
                .snapshot
                .remove_lease_owned_files()
                .map_err(SyncRunnerError::StateIo)?;
            for record in staged {
                candidate.snapshot.prepared_content_mut().insert(
                    record.content_id.clone(),
                    crate::sync::PreparedContent {
                        content_id: record.content_id,
                        logical_len: record.logical_size,
                        source: PreparedContentSource::StagedFile {
                            path: record.staged_path,
                            owner_marker: record.owner_marker,
                        },
                        source_fingerprint: None,
                        cleanup_policy: crate::sync::PreparedContentCleanup::LeaseOwned,
                    },
                );
            }
            candidate
                .snapshot
                .attach_preparation_lease(PreparedSnapshotLease {
                    id: lease.id,
                    owner_marker: lease.owner_marker,
                });
            return Ok(());
        }
        Ok(())
    }

    pub(super) fn finish_candidate_preparation(
        &self,
        candidate: &crate::sync::SnapshotCandidate,
        state: PreparationLeaseState,
    ) -> Result<(), SyncRunnerError> {
        if state == PreparationLeaseState::Committed {
            self.transition_candidate_preparation(
                candidate,
                PreparationLeaseState::ReferencedByUpload,
                state,
            )?;
            return self.release_terminal_preparation(candidate);
        }
        if state != PreparationLeaseState::Abandoned {
            return Err(MetadataError::InvalidStorageMetadata(
                "preparation can finish only as committed or abandoned".to_string(),
            )
            .into());
        }
        if candidate.snapshot.preparation_lease().is_none() {
            return Ok(());
        }
        if self.transition_candidate_preparation_if_current(
            candidate,
            PreparationLeaseState::ReferencedByUpload,
            state,
        )? {
            return self.release_terminal_preparation(candidate);
        }
        self.transition_candidate_preparation(candidate, PreparationLeaseState::Prepared, state)?;
        self.release_terminal_preparation(candidate)
    }

    fn transition_candidate_preparation(
        &self,
        candidate: &crate::sync::SnapshotCandidate,
        expected: PreparationLeaseState,
        next: PreparationLeaseState,
    ) -> Result<(), SyncRunnerError> {
        let transitioned =
            self.transition_candidate_preparation_if_current(candidate, expected, next)?;
        if !transitioned {
            return Err(MetadataError::InvalidStorageMetadata(format!(
                "preparation lease did not transition from {} to {}",
                expected.as_str(),
                next.as_str()
            ))
            .into());
        }
        Ok(())
    }

    fn transition_candidate_preparation_if_current(
        &self,
        candidate: &crate::sync::SnapshotCandidate,
        expected: PreparationLeaseState,
        next: PreparationLeaseState,
    ) -> Result<bool, SyncRunnerError> {
        let Some(lease) = candidate.snapshot.preparation_lease() else {
            return Ok(true);
        };
        self.with_store(|store| {
            store.transition_preparation_lease(
                &lease.id,
                &lease.owner_marker,
                expected,
                next,
                &self.options.generated_at,
            )
        })
    }

    fn release_terminal_preparation(
        &self,
        candidate: &crate::sync::SnapshotCandidate,
    ) -> Result<(), SyncRunnerError> {
        let Some(lease) = candidate.snapshot.preparation_lease() else {
            return Ok(());
        };
        candidate
            .snapshot
            .remove_lease_owned_files()
            .map_err(SyncRunnerError::StateIo)?;
        self.with_store(|store| {
            for content in candidate.snapshot.prepared_content().values() {
                if matches!(&content.source, PreparedContentSource::StagedFile { .. }) {
                    store.forget_reconciled_preparation_orphan(
                        &lease.id,
                        &content.content_id,
                        &lease.owner_marker,
                    )?;
                }
            }
            Ok(())
        })?;
        Ok(())
    }
}

fn source_fingerprint(fingerprint: Option<PreparedSourceFingerprint>) -> SourceFingerprint {
    fingerprint.map_or_else(
        || SourceFingerprint::new("synthetic-override"),
        |fingerprint| {
            SourceFingerprint::new(format!(
                "v1:{}:{}:{}:{}:{}:{}",
                fingerprint.size,
                fingerprint.mtime_ns,
                fingerprint.ctime_ns,
                fingerprint.inode,
                fingerprint.device,
                fingerprint.file_mode
            ))
        },
    )
}
