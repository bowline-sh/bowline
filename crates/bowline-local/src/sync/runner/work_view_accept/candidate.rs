use bowline_core::workspace_graph::{RefKind, SnapshotKind, WorkspaceRef as SnapshotRef};

use super::*;
use crate::sync::{CandidateBase, manifest_id_for_snapshot};

impl SyncRunner<'_> {
    pub(super) fn work_accept_candidate(
        &self,
        operation_id: &str,
        work_view: &bowline_core::work_views::WorkView,
        base_ref: &WorkspaceRef,
        current: &SnapshotManifest,
        prepared: &PreparedSnapshotAccept,
    ) -> Result<SnapshotCandidate, SyncRunnerError> {
        let prefix = normalize_workspace_path(&work_view.project_path);
        let entries = splice_current_manifest_entries(
            current,
            &prefix,
            prepared.branch_paths(),
            prepared.merged_entries(),
        );
        let identity = super::super::super::manifest_tree::rebuild_manifest_identity(
            &self.options.workspace_id,
            &entries,
            &self.options.generated_at,
        );
        let snapshot_id = identity.snapshot_id.clone();
        let manifest = SnapshotManifest {
            schema_version: current.schema_version,
            snapshot_id: snapshot_id.clone(),
            workspace_id: self.options.workspace_id.clone(),
            project_id: None,
            kind: SnapshotKind::WorkspaceHead,
            base_snapshot_id: Some(SnapshotId::new(base_ref.snapshot_id.clone())),
            entries,
            refs: vec![SnapshotRef {
                name: "workspace".to_string(),
                target_snapshot_id: snapshot_id.clone(),
                kind: RefKind::Workspace,
            }],
        };
        let scan_report = scan_work_accept_workspace(&self.options.root, || {
            self.check_cancellation(LongOperationCancellationPoint::BetweenChunks)
                .map_err(|error| {
                    crate::scanner::ScanError::Io(std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        error.to_string(),
                    ))
                })
        });
        let scan_report = match scan_report {
            Ok(report) => report,
            Err(error) => {
                self.check_cancellation(LongOperationCancellationPoint::BetweenChunks)?;
                return Err(work_accept_error(&format!(
                    "workspace scan failed: {error}"
                )));
            }
        };
        Ok(SnapshotCandidate {
            base: CandidateBase {
                workspace_id: self.options.workspace_id.clone(),
                version: base_ref.version,
                snapshot_id: SnapshotId::new(base_ref.snapshot_id.clone()),
            },
            device_id: self.options.device_id.clone(),
            manifest_id: manifest_id_for_snapshot(&snapshot_id),
            snapshot: SnapshotContent::from_prepared(manifest, prepared.prepared_content().clone()),
            scan_report,
            scan_scope: ScanScope::Full(FullScanReason::ReconcileFallback),
            stat_cache_hit_paths: BTreeSet::new(),
            stat_cache_divergences: Vec::new(),
            scan_stats: super::super::super::ScanStats::default(),
            manifest_identity: identity,
            stat_cache_write_back: None,
            causation_ids: vec![operation_id.to_string()],
            skipped_unsafe_symlinks: BTreeSet::new(),
            created_at: self.options.generated_at.clone(),
        })
    }
}

fn scan_work_accept_workspace(
    root: &std::path::Path,
    checkpoint: impl FnMut() -> Result<(), crate::scanner::ScanError>,
) -> Result<crate::scanner::ScanReport, crate::scanner::ScanError> {
    crate::scanner::scan_workspace_with_checkpoint(root, checkpoint)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_candidate_scan_stops_at_the_next_scanner_checkpoint() {
        let root = std::env::temp_dir().join(format!(
            "bowline-accept-scan-cancel-{}-{}",
            std::process::id(),
            OffsetDateTime::now_utc().unix_timestamp_nanos()
        ));
        std::fs::create_dir_all(root.join("nested")).expect("nested workspace creates");
        std::fs::write(root.join("nested/one.txt"), b"one").expect("first file writes");
        std::fs::write(root.join("nested/two.txt"), b"two").expect("second file writes");
        let mut checkpoints = 0_u32;

        let result = scan_work_accept_workspace(&root, || {
            checkpoints = checkpoints.saturating_add(1);
            Err(crate::scanner::ScanError::Io(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "cancelled accept scan",
            )))
        });

        assert!(matches!(result, Err(crate::scanner::ScanError::Io(_))));
        assert_eq!(checkpoints, 1);
        let _ = std::fs::remove_dir_all(root);
    }
}
