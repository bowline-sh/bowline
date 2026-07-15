use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::daemon) struct LocalWorkspaceKey {
    pub(in crate::daemon) bytes: [u8; 32],
    pub(in crate::daemon) key_epoch: u32,
}

pub(super) type HostedSyncPrerequisite =
    Arc<dyn Fn(&SyncOnceArgs) -> Result<LocalWorkspaceKey, SyncOnceError> + Send + Sync>;

pub(super) type HostedSyncOperation = Arc<
    dyn Fn(
            Arc<HostedContext>,
            SyncOnceArgs,
            Option<WorkspaceRef>,
            LocalWorkspaceKey,
        ) -> Result<SyncOnceSummary, SyncOnceError>
        + Send
        + Sync,
>;

pub(super) fn hosted_sync_executor_with_operations(
    resolver: HostedContextResolver,
    prerequisite: HostedSyncPrerequisite,
    operation: HostedSyncOperation,
) -> SyncExecutor {
    Box::new(move |args, observed_base_ref| {
        let workspace_key = prerequisite(&args)?;
        let hosted = resolver(&args).map_err(|error| {
            SyncOnceError::ControlPlane(ControlPlaneError::Storage(error.to_string()))
        })?;
        operation(hosted, args, observed_base_ref, workspace_key)
    })
}

pub(super) fn require_local_workspace_key(
    args: &SyncOnceArgs,
) -> Result<LocalWorkspaceKey, SyncOnceError> {
    let workspace_id = WorkspaceId::new(args.workspace_id.clone());
    let key_store = key_store()?;
    let workspace_key = key_store
        .load_workspace_key(&workspace_id)?
        .ok_or(SyncOnceError::WorkspaceKeyMissing)?;
    Ok(LocalWorkspaceKey {
        bytes: workspace_key_bytes(&workspace_key.key_bytes)
            .map_err(|_| SyncOnceError::WorkspaceKeyInvalid)?,
        key_epoch: workspace_key.key_epoch,
    })
}
