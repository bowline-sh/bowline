use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictSnapshotRetention {
    pub conflict_id: String,
    pub base_snapshot_id: Option<SnapshotId>,
    pub remote_snapshot_id: Option<SnapshotId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LocalSnapshotMaintenanceReport {
    pub pins_acquired: u64,
    pub pins_updated: u64,
    pub pins_released: u64,
    pub active_pins: u64,
    pub snapshots_deleted: u64,
    pub gc_records_processed: u64,
    pub gc_records_marked: u64,
    pub cache_files_deleted: u64,
    pub cache_bytes_deleted: u64,
    pub metadata_records_deleted: u64,
    pub gc_complete: bool,
}

#[derive(Debug, Clone)]
struct PinSeed {
    snapshot_id: SnapshotId,
    reason: SnapshotPinReason,
    owner: SnapshotPinOwner,
    expires_at: Option<String>,
}

impl MetadataStore {
    pub fn maintain_snapshot_retention(
        &mut self,
        workspace_id: &WorkspaceId,
        conflicts: &[ConflictSnapshotRetention],
        policy: &LocalMetadataRetentionPolicy,
        now: &str,
    ) -> Result<LocalSnapshotMaintenanceReport, MetadataError> {
        let grace_before = super::sync::retention_cutoff(now, policy.snapshot_gc_grace_days)?;
        let (pin_report, snapshots_deleted) = self.with_committed(|store| {
            let desired = store.desired_snapshot_pins(workspace_id, conflicts, policy, now)?;
            let pin_report = store.reconcile_snapshot_pins(workspace_id, &desired)?;
            let snapshots_deleted = store.delete_unpinned_snapshots_batch(
                workspace_id,
                &grace_before,
                policy.snapshot_delete_batch,
                now,
            )?;
            Ok::<_, MetadataError>((pin_report, snapshots_deleted.len() as u64))
        })?;
        let checkpoint = self.metadata_gc_checkpoint(workspace_id)?;
        let should_start_generation = checkpoint.as_ref().is_none_or(|checkpoint| {
            checkpoint.phase == MetadataGcPhase::Complete
                && checkpoint.grace_before.as_str() < grace_before.as_str()
        });
        if should_start_generation {
            self.start_metadata_gc(
                workspace_id,
                &metadata_gc_generation(workspace_id, now),
                &grace_before,
                now,
            )?;
        }
        let batch = self.run_metadata_gc_batch(workspace_id, policy.metadata_gc_batch, now)?;
        let candidates =
            self.metadata_gc_delete_candidates(workspace_id, policy.metadata_cache_delete_batch)?;
        let mut cache_files_deleted = batch.cache_files_deleted;
        let mut cache_bytes_deleted = batch.cache_bytes_deleted;
        let mut metadata_records_deleted = batch.metadata_records_deleted;
        for candidate in candidates {
            let result = self.finalize_metadata_gc_candidate(workspace_id, &candidate)?;
            if result.metadata_record_deleted {
                metadata_records_deleted += 1;
            }
            if result.cache_file_deleted {
                cache_files_deleted += 1;
                cache_bytes_deleted = cache_bytes_deleted.saturating_add(result.cache_bytes);
            }
        }
        Ok(LocalSnapshotMaintenanceReport {
            pins_acquired: pin_report.acquired,
            pins_updated: pin_report.updated,
            pins_released: pin_report.released,
            active_pins: pin_report.active,
            snapshots_deleted,
            gc_records_processed: batch.records_processed,
            gc_records_marked: batch.records_marked,
            cache_files_deleted,
            cache_bytes_deleted,
            metadata_records_deleted,
            gc_complete: batch.complete,
        })
    }

    fn desired_snapshot_pins(
        &self,
        workspace_id: &WorkspaceId,
        conflicts: &[ConflictSnapshotRetention],
        policy: &LocalMetadataRetentionPolicy,
        now: &str,
    ) -> Result<Vec<SnapshotPinRecord>, MetadataError> {
        let mut seeds = Vec::new();
        self.collect_ref_pin_seeds(workspace_id, &mut seeds)?;
        self.collect_work_view_pin_seeds(workspace_id, now, &mut seeds)?;
        self.collect_operation_pin_seeds(workspace_id, &mut seeds)?;
        collect_conflict_pin_seeds(conflicts, &mut seeds);
        self.collect_history_pin_seeds(workspace_id, policy, now, &mut seeds)?;
        let mut pins = BTreeMap::new();
        for seed in seeds {
            let Some(snapshot) = self.snapshot(workspace_id, &seed.snapshot_id)? else {
                continue;
            };
            let pin = SnapshotPinRecord {
                id: deterministic_pin_id(workspace_id, &seed),
                workspace_id: workspace_id.clone(),
                snapshot_id: seed.snapshot_id,
                root_id: snapshot.root_id,
                reason: seed.reason,
                owner: seed.owner,
                expires_at: seed.expires_at,
                created_at: snapshot.created_at,
            };
            pins.insert(pin.id.clone(), pin);
        }
        Ok(pins.into_values().collect())
    }

    fn collect_ref_pin_seeds(
        &self,
        workspace_id: &WorkspaceId,
        seeds: &mut Vec<PinSeed>,
    ) -> Result<(), MetadataError> {
        if let Some(head) = self.workspace_sync_head(workspace_id)? {
            seeds.push(pin_seed(
                head.workspace_ref.snapshot_id,
                SnapshotPinReason::WorkspaceRef,
                SnapshotPinOwnerKind::WorkspaceRef,
                "workspace-head",
                None,
            ));
        }
        let mut statement = self.connection.prepare(
            "SELECT id, latest_snapshot_id FROM projects
             WHERE workspace_id = ?1 AND latest_snapshot_id IS NOT NULL
             ORDER BY id",
        )?;
        let rows = statement.query_map([workspace_id.as_str()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (project_id, snapshot_id) = row?;
            seeds.push(pin_seed(
                SnapshotId::new(snapshot_id),
                SnapshotPinReason::ProjectRef,
                SnapshotPinOwnerKind::ProjectRef,
                project_id,
                None,
            ));
        }
        Ok(())
    }

    fn collect_work_view_pin_seeds(
        &self,
        workspace_id: &WorkspaceId,
        now: &str,
        seeds: &mut Vec<PinSeed>,
    ) -> Result<(), MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT id, base_snapshot_id, exposed_snapshot_id, lifecycle, retain_until
             FROM work_views WHERE workspace_id = ?1 AND (
               lifecycle IN ('active', 'review-ready')
               OR (retention_state = 'retained' AND retain_until > ?2)
             ) ORDER BY id",
        )?;
        let rows = statement.query_map(params![workspace_id.as_str(), now], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })?;
        for row in rows {
            let (id, snapshot_id, exposed_snapshot_id, lifecycle, retain_until) = row?;
            let expires_at = (!matches!(lifecycle.as_str(), "active" | "review-ready"))
                .then_some(retain_until)
                .flatten();
            seeds.push(pin_seed(
                SnapshotId::new(snapshot_id),
                SnapshotPinReason::WorkView,
                SnapshotPinOwnerKind::WorkView,
                id.clone(),
                expires_at.clone(),
            ));
            if let Some(exposed_snapshot_id) = exposed_snapshot_id {
                seeds.push(pin_seed(
                    SnapshotId::new(exposed_snapshot_id),
                    SnapshotPinReason::WorkView,
                    SnapshotPinOwnerKind::WorkView,
                    format!("{}:exposed", id),
                    expires_at,
                ));
            }
        }
        Ok(())
    }

    fn collect_operation_pin_seeds(
        &self,
        workspace_id: &WorkspaceId,
        seeds: &mut Vec<PinSeed>,
    ) -> Result<(), MetadataError> {
        collect_operation_table_seeds(
            &self.connection,
            workspace_id,
            "sync_operations",
            "state NOT IN ('completed', 'cancelled')",
            &["base_snapshot_id", "target_snapshot_id"],
            "sync",
            seeds,
        )?;
        collect_operation_table_seeds(
            &self.connection,
            workspace_id,
            "work_view_accept_operations",
            "state IN ('queued', 'claimed', 'waiting-retry', 'review-required')",
            &[
                "observed_main_snapshot_id",
                "observed_ref_snapshot_id",
                "target_snapshot_id",
            ],
            "work-view-accept",
            seeds,
        )
    }

    fn collect_history_pin_seeds(
        &self,
        workspace_id: &WorkspaceId,
        policy: &LocalMetadataRetentionPolicy,
        now: &str,
        seeds: &mut Vec<PinSeed>,
    ) -> Result<(), MetadataError> {
        let cutoff = super::sync::retention_cutoff(now, policy.restore_point_retention_days)?;
        let mut snapshot_ids = BTreeSet::new();
        let mut recent = self.connection.prepare(
            "SELECT id FROM snapshots WHERE workspace_id = ?1 AND created_at >= ?2
             ORDER BY created_at DESC, id DESC",
        )?;
        for row in recent.query_map(params![workspace_id.as_str(), cutoff], |row| {
            row.get::<_, String>(0)
        })? {
            snapshot_ids.insert(row?);
        }
        let mut minimum = self.connection.prepare(
            "SELECT id FROM snapshots WHERE workspace_id = ?1
             ORDER BY created_at DESC, id DESC LIMIT ?2",
        )?;
        for row in minimum.query_map(
            params![
                workspace_id.as_str(),
                super::common::sql_limit(Some(policy.restore_point_min_keep)),
            ],
            |row| row.get::<_, String>(0),
        )? {
            snapshot_ids.insert(row?);
        }
        for snapshot_id in snapshot_ids {
            seeds.push(pin_seed(
                SnapshotId::new(&snapshot_id),
                SnapshotPinReason::ExplicitHistory,
                SnapshotPinOwnerKind::ExplicitHistory,
                snapshot_id,
                None,
            ));
        }
        Ok(())
    }
}

fn collect_operation_table_seeds(
    connection: &Connection,
    workspace_id: &WorkspaceId,
    table: &str,
    state_filter: &str,
    fields: &[&str],
    owner_prefix: &str,
    seeds: &mut Vec<PinSeed>,
) -> Result<(), MetadataError> {
    for field in fields {
        let sql = format!(
            "SELECT id, {field} FROM {table}
             WHERE workspace_id = ?1 AND {state_filter} AND {field} IS NOT NULL ORDER BY id"
        );
        let mut statement = connection.prepare(&sql)?;
        let rows = statement.query_map([workspace_id.as_str()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (operation_id, snapshot_id) = row?;
            seeds.push(pin_seed(
                SnapshotId::new(snapshot_id),
                SnapshotPinReason::DurableOperation,
                SnapshotPinOwnerKind::DurableOperation,
                format!("{owner_prefix}:{operation_id}:{field}"),
                None,
            ));
        }
    }
    Ok(())
}

fn collect_conflict_pin_seeds(conflicts: &[ConflictSnapshotRetention], seeds: &mut Vec<PinSeed>) {
    for conflict in conflicts {
        for (role, snapshot_id) in [
            ("base", conflict.base_snapshot_id.as_ref()),
            ("remote", conflict.remote_snapshot_id.as_ref()),
        ] {
            if let Some(snapshot_id) = snapshot_id {
                seeds.push(pin_seed(
                    snapshot_id.clone(),
                    SnapshotPinReason::Conflict,
                    SnapshotPinOwnerKind::Conflict,
                    format!("{}:{role}", conflict.conflict_id),
                    None,
                ));
            }
        }
    }
}

fn pin_seed(
    snapshot_id: SnapshotId,
    reason: SnapshotPinReason,
    owner_kind: SnapshotPinOwnerKind,
    owner_id: impl Into<String>,
    expires_at: Option<String>,
) -> PinSeed {
    PinSeed {
        snapshot_id,
        reason,
        owner: SnapshotPinOwner {
            kind: owner_kind,
            id: owner_id.into(),
        },
        expires_at,
    }
}

fn deterministic_pin_id(workspace_id: &WorkspaceId, seed: &PinSeed) -> SnapshotPinId {
    let mut hasher = blake3::Hasher::new();
    for component in [
        "bowline-snapshot-pin-v1",
        workspace_id.as_str(),
        seed.snapshot_id.as_str(),
        seed.reason.as_str(),
        seed.owner.kind.as_str(),
        seed.owner.id.as_str(),
    ] {
        hasher.update(&(component.len() as u64).to_be_bytes());
        hasher.update(component.as_bytes());
    }
    SnapshotPinId::new(format!("pin_{}", &hasher.finalize().to_hex()[..24]))
}

fn metadata_gc_generation(workspace_id: &WorkspaceId, now: &str) -> String {
    let digest = blake3::hash(format!("{}\0{now}", workspace_id.as_str()).as_bytes());
    format!("gc_{}", &digest.to_hex()[..24])
}
