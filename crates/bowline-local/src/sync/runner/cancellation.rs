use super::work_view_accept::now_timestamp;
use super::*;

impl SyncRunner<'_> {
    pub(super) fn check_cancellation(
        &self,
        point: LongOperationCancellationPoint,
    ) -> Result<(), SyncRunnerError> {
        if let Some(claim) = &self.work_view_accept_claim {
            return self
                .authorize_work_view_accept_boundary(claim, point)?
                .then_some(())
                .ok_or(SyncRunnerError::SyncClaimOwnershipLost);
        }
        let Some(claim) = &self.options.sync_claim else {
            return Ok(());
        };
        let recovering_committed_operation =
            claim.claimed_from_state() == SyncOperationState::ReconciliationRequired;
        match self.with_store_sync(|store| {
            if recovering_committed_operation {
                store
                    .renew_sync_operation_reconciliation_boundary(claim)
                    .map_err(Into::into)
            } else {
                store
                    .authorize_sync_operation_boundary(claim)
                    .map_err(Into::into)
            }
        })? {
            SyncClaimCheck::Owned => Ok(()),
            SyncClaimCheck::CancellationRequested if recovering_committed_operation => {
                self.last_cancellation_point.set(Some(point));
                self.cancellation_requested_after_commit.set(true);
                Ok(())
            }
            SyncClaimCheck::CancellationRequested => {
                self.last_cancellation_point.set(Some(point));
                Err(SyncRunnerError::SyncOperationCancellationRequested)
            }
            SyncClaimCheck::OwnershipLost => Err(SyncRunnerError::SyncClaimOwnershipLost),
        }
    }

    pub(super) fn authorize_work_view_accept_boundary(
        &self,
        claim: &WorkViewAcceptClaimHandle,
        point: LongOperationCancellationPoint,
    ) -> Result<bool, SyncRunnerError> {
        if !self.renew_accept_claim(claim)? {
            return Ok(false);
        }
        let store = MetadataStore::open(self.options.state_root.join(DEFAULT_DATABASE_FILE))?;
        match store.check_work_view_accept_claim(claim, &now_timestamp()?)? {
            WorkViewAcceptClaimCheck::Owned => Ok(true),
            WorkViewAcceptClaimCheck::CancellationRequested
                if self.remote_domain_committed.get()
                    || self.local_materialization_committed.get() =>
            {
                self.last_cancellation_point.set(Some(point));
                self.cancellation_requested_after_commit.set(true);
                Ok(true)
            }
            WorkViewAcceptClaimCheck::CancellationRequested => {
                self.last_cancellation_point.set(Some(point));
                Err(SyncRunnerError::SyncOperationCancellationRequested)
            }
            WorkViewAcceptClaimCheck::OwnershipLost => Ok(false),
        }
    }

    pub(super) fn authorize_materialization(
        &self,
        expected_ref: &WorkspaceRef,
        boundary: MaterializationBoundary,
    ) -> Result<(), SyncRunnerError> {
        if boundary == MaterializationBoundary::AfterMutation {
            self.local_materialization_committed.set(true);
            return Ok(());
        }
        if self.local_materialization_committed.get() {
            self.authorize_materialization_reconciliation_boundary(
                LongOperationCancellationPoint::BeforeMaterializationMutation,
            )?;
        } else {
            self.check_cancellation(LongOperationCancellationPoint::BeforeMaterializationMutation)?;
        }
        if boundary == MaterializationBoundary::GuardAcquired {
            let current_ref = self
                .control_plane
                .get_workspace_ref(&self.options.workspace_id)?
                .ok_or_else(|| {
                    SyncRunnerError::SupersededMaterializationSnapshot(
                        expected_ref.snapshot_id.as_str().to_string(),
                    )
                })?;
            if current_ref.version != expected_ref.version
                || current_ref.snapshot_id != expected_ref.snapshot_id
            {
                return Err(SyncRunnerError::SupersededMaterializationSnapshot(
                    expected_ref.snapshot_id.as_str().to_string(),
                ));
            }
        }
        Ok(())
    }

    pub(super) fn check_reconciling_cancellation(
        &self,
        point: LongOperationCancellationPoint,
    ) -> Result<(), SyncRunnerError> {
        if self.remote_domain_committed.get() || self.local_materialization_committed.get() {
            self.authorize_materialization_reconciliation_boundary(point)
        } else {
            self.check_cancellation(point)
        }
    }

    fn authorize_materialization_reconciliation_boundary(
        &self,
        point: LongOperationCancellationPoint,
    ) -> Result<(), SyncRunnerError> {
        let Some(claim) = &self.options.sync_claim else {
            return Ok(());
        };
        match self.with_store_sync(|store| {
            store
                .renew_sync_operation_reconciliation_boundary(claim)
                .map_err(Into::into)
        })? {
            SyncClaimCheck::Owned => Ok(()),
            SyncClaimCheck::CancellationRequested => {
                self.last_cancellation_point.set(Some(point));
                self.cancellation_requested_after_commit.set(true);
                Ok(())
            }
            SyncClaimCheck::OwnershipLost => Err(SyncRunnerError::SyncClaimOwnershipLost),
        }
    }
}
